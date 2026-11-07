use super::{emit, files, stopping};
use crate::host::package::Service;
use anyhow::{ensure, Result};
use serde_json::json;
use std::{
    io::Read,
    net::{Ipv4Addr, SocketAddr, TcpStream},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Group(Child);
impl Drop for Group {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(100),
    )
    .is_ok()
}

pub fn execute(service: &Service, root: &Path, control: i32) -> Result<()> {
    proxy_ready(service)?;
    ensure!(!listening(service.ready_port), "READY_PORT_ALREADY_IN_USE");
    let expand = |s: &str| s.replace("{service_dir}", &root.to_string_lossy());
    ensure!(!service.command.is_empty(), "INVALID_PAYLOAD");
    let mut command = Command::new(expand(&service.command[0]));
    crate::host::common::parent_death(&mut command);
    command
        .args(service.command[1..].iter().map(|v| expand(v)))
        .current_dir(expand(&service.working_directory));
    for (key, value) in &service.environment {
        command.env(key, expand(value));
    }
    let mut group = Group(
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let category = supervise(&mut group.0, service, root, control)?;
    emit(
        json!({"event":"stopped","category":category,"exit_code":group.0.try_wait()?.and_then(|s|s.code())}),
    )
}

fn proxy_ready(service: &Service) -> Result<()> {
    let mut targets = std::collections::BTreeSet::new();
    for key in ["http_proxy", "all_proxy"] {
        if let Some(value) = service.proxy_environment.as_ref().and_then(|v| v.get(key)) {
            let endpoint = reqwest::Url::parse(value)?;
            let host = endpoint
                .host_str()
                .ok_or_else(|| anyhow::anyhow!("INVALID_PROXY_ENVIRONMENT"))?
                .parse::<Ipv4Addr>()?;
            let port = endpoint
                .port_or_known_default()
                .ok_or_else(|| anyhow::anyhow!("INVALID_PROXY_ENVIRONMENT"))?;
            targets.insert(SocketAddr::from((host, port)));
        }
    }
    for target in targets {
        ensure!(
            TcpStream::connect_timeout(&target, Duration::from_secs(2)).is_ok(),
            "PROXY_UNREACHABLE"
        );
    }
    Ok(())
}

fn drain(reader: &mut impl Read, fd: &mut libc::pollfd, root: &Path) -> Result<()> {
    let mut bytes = [0u8; 65536];
    for _ in 0..16 {
        match reader.read(&mut bytes) {
            Ok(0) => {
                fd.fd = -1;
                break;
            }
            Ok(n) => files::log(root, &bytes[..n])?,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn supervise(
    child: &mut Child,
    service: &Service,
    root: &Path,
    control: i32,
) -> Result<&'static str> {
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    crate::host::ssh::nonblocking(out.as_raw_fd())?;
    crate::host::ssh::nonblocking(err.as_raw_fd())?;
    let descriptor = |fd| libc::pollfd { fd, events: libc::POLLIN | libc::POLLHUP, revents: 0 };
    let mut fds = [descriptor(control), descriptor(out.as_raw_fd()), descriptor(err.as_raw_fd())];
    let deadline = Instant::now() + Duration::from_secs(service.startup_timeout_secs);
    let mut ready = false;
    loop {
        if stopping() {
            return Ok("CONTROL_DISCONNECTED");
        }
        let count = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 100) };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if fds[0].revents != 0 {
            let mut byte = 0u8;
            let read = unsafe { libc::read(control, (&mut byte as *mut u8).cast(), 1) };
            if read == 0 {
                return Ok("CONTROL_DISCONNECTED");
            }
        }
        if fds[1].revents != 0 {
            drain(&mut out, &mut fds[1], root)?;
        }
        if fds[2].revents != 0 {
            drain(&mut err, &mut fds[2], root)?;
        }
        if child.try_wait()?.is_some() {
            drain(&mut out, &mut fds[1], root)?;
            drain(&mut err, &mut fds[2], root)?;
            return Ok("SERVICE_EXITED");
        }
        if !ready && listening(service.ready_port) {
            ready = true;
            emit(json!({"event":"ready"}))?;
        }
        if !ready && Instant::now() >= deadline {
            return Ok("READINESS_TIMEOUT");
        }
    }
}
