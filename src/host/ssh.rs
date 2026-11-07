use super::{common::Host, network, service};
use anyhow::{bail, ensure, Result};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    os::fd::AsRawFd,
    path::Path,
    process::{Child, Command, Output, Stdio},
    time::Instant,
};

pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn command(host: &Host, vm: &str, identity: &Path) -> Result<Command> {
    let (slot, alias) = network::connection(host, vm)?;
    let mut c = Command::new("/usr/bin/ssh");
    c.args(["-F", "/dev/null", "-T", "-i"])
        .arg(identity)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ConnectTimeout=2",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=3",
        ])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", host.services().join("known_hosts").display()))
        .arg("-o")
        .arg(format!("HostKeyAlias={alias}"))
        .arg(format!("agent@{}", slot.guest_ip));
    Ok(c)
}

pub fn spawn(host: &Host, vm: &str, identity: &Path, remote: &str) -> Result<Child> {
    let mut command = command(host, vm, identity)?;
    super::common::parent_death(&mut command);
    Ok(command
        .arg(remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?)
}

pub fn nonblocking(fd: std::os::fd::RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "PIPE_CONFIGURATION_FAILED"
    );
    Ok(())
}

pub fn upload(child: &mut Child, bytes: &[u8], deadline: Instant) -> Result<()> {
    let input = child.stdin.as_mut().ok_or_else(|| anyhow::anyhow!("SSH_STDIN_MISSING"))?;
    nonblocking(input.as_raw_fd())?;
    let mut sent = 0;
    while sent < bytes.len() {
        ensure!(Instant::now() < deadline, "GUEST_UPLOAD_TIMEOUT");
        service::check_stop()?;
        match input.write(&bytes[sent..bytes.len().min(sent + 65536)]) {
            Ok(0) => bail!("SSH_UPLOAD_CLOSED"),
            Ok(n) => sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(super::common::duration())
            }
            Err(_) => bail!("SSH_UPLOAD_FAILED"),
        }
    }
    Ok(())
}

pub fn finish(child: &mut Child) {
    child.stdin.take();
    let deadline = Instant::now() + std::time::Duration::from_secs(15);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(super::common::duration());
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn capture(
    host: &Host,
    vm: &str,
    identity: &Path,
    remote: &str,
    input: &[u8],
    deadline: Instant,
) -> Result<Output> {
    let mut child = spawn(host, vm, identity, remote)?;
    let result = (|| {
        upload(&mut child, input, deadline)?;
        child.stdin.take();
        while child.try_wait()?.is_none() {
            ensure!(Instant::now() < deadline, "SSH_COMMAND_TIMEOUT");
            service::check_stop()?;
            std::thread::sleep(super::common::duration());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result?;
    Ok(child.wait_with_output()?)
}

pub fn wait_ready(host: &Host, vm: &str, identity: &Path, deadline: Instant) -> Result<()> {
    let mut category = "SSH_CONNECTION_FAILED";
    while Instant::now() < deadline {
        let result = capture(host, vm, identity, "true", &[], deadline)?;
        if result.status.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&result.stderr);
        category = if text.contains("Permission denied") {
            "SSH_AUTHENTICATION_FAILED"
        } else if text.contains("Host key verification failed") {
            "SSH_HOST_KEY_REJECTED"
        } else if text.contains("Connection refused") {
            "SSH_CONNECTION_REFUSED"
        } else {
            "SSH_CONNECTION_FAILED"
        };
        if matches!(category, "SSH_AUTHENTICATION_FAILED" | "SSH_HOST_KEY_REJECTED") {
            break;
        }
        std::thread::sleep(super::common::duration());
    }
    bail!("{category}")
}

pub fn prepare_agent(host: &Host, vm: &str, identity: &Path, deadline: Instant) -> Result<()> {
    let path = Path::new("/usr/local/libexec/mindos-firecracker/firecracker-guest");
    crate::config::trusted(path)?;
    let bytes = std::fs::read(path)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let target = "/home/agent/.local/libexec/firecracker-guest";
    let result = capture(
        host,
        vm,
        identity,
        &format!("test -x {target} && sha256sum {target}"),
        &[],
        deadline,
    )?;
    if result.status.success()
        && String::from_utf8_lossy(&result.stdout).split_whitespace().next() == Some(hash.as_str())
    {
        return Ok(());
    }
    let temporary = format!("/home/agent/.local/libexec/.guest-{}", uuid::Uuid::new_v4());
    let remote=format!("set -eu; umask 077; d=/home/agent/.local/libexec; test \"$(readlink -m \"$d\")\" = \"$d\"; mkdir -p \"$d\"; trap 'rm -f {}' EXIT; cat > {}; chmod 700 {}; mv {} {target}",temporary,temporary,temporary,temporary);
    let result = capture(host, vm, identity, &remote, &bytes, deadline)?;
    ensure!(result.status.success(), "GUEST_AGENT_UPLOAD_FAILED");
    Ok(())
}
