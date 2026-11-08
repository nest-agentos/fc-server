use super::{
    common::{self, Host},
    network,
    package::{self, Package},
    ssh,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::Read,
    os::fd::AsRawFd,
    path::Path,
    process::Child,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

static STOP: AtomicBool = AtomicBool::new(false);
#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub struct Binding {
    pub service: String,
}
#[derive(Serialize, Deserialize)]
pub struct Runtime {
    pub generation: String,
    pub state: String,
}

extern "C" fn stopped(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}
pub fn check_stop() -> Result<()> {
    ensure!(!STOP.load(Ordering::Relaxed), "HOST_STOP_REQUESTED");
    Ok(())
}
pub fn unit(vm: &str) -> Result<String> {
    common::id_check(vm)?;
    Ok(format!("firecracker-workload@{vm}.service"))
}
pub fn active(vm: &str) -> Result<String> {
    Ok(String::from_utf8(common::systemctl(&["is-active", &unit(vm)?], false)?.stdout)?
        .trim()
        .to_owned())
}
pub fn vm_lock(host: &Host, vm: &str) -> Result<File> {
    common::id_check(vm)?;
    common::lock(&host.services().join("locks").join(format!("{vm}.lock")), false)
}
fn runtime(host: &Host, vm: &str) -> std::path::PathBuf {
    host.services().join("runtime").join(format!("{vm}.json"))
}
fn binding(host: &Host, vm: &str) -> std::path::PathBuf {
    host.services().join("vms").join(format!("{vm}.json"))
}

pub fn initialize(host: &Host) -> Result<()> {
    common::private_dir(&host.services())?;
    for name in ["packages", "vms", "runtime", "locks"] {
        common::private_dir(&host.services().join(name))?;
    }
    Ok(())
}

pub fn load(host: &Host, vm: &str) -> Result<Package> {
    common::id_check(vm)?;
    let b: Binding = common::read(&binding(host, vm))?;
    package::read(host, &b.service)
}

fn bind(host: &Host, vm: &str, id: &str) -> Result<()> {
    common::id_check(vm)?;
    common::id_check(id)?;
    package::read(host, id)?;
    let _guard = vm_lock(host, vm)?;
    let target = binding(host, vm);
    let desired = Binding { service: id.into() };
    if target.exists() {
        ensure!(common::read::<Binding>(&target)? == desired, "VM_SERVICE_BINDING_CONFLICT");
    } else {
        ensure!(host.inspect(vm)?.is_none(), "VM_ALREADY_EXISTS_NOT_MANAGED");
        common::save(&target, &desired)?;
    }
    Ok(())
}

fn generation(host: &Host, vm: &str) -> Result<String> {
    Ok(network::connection(host, vm)?.1)
}

fn ensure_vm(host: &Host, vm: &str, package: &Package) -> Result<()> {
    if host.inspect(vm)?.is_some() {
        ensure!(runtime(host, vm).exists(), "VM_INSTANCE_OWNERSHIP_CONFLICT");
        ensure!(
            common::read::<Runtime>(&runtime(host, vm))?.generation == generation(host, vm)?,
            "VM_INSTANCE_OWNERSHIP_CONFLICT"
        );
    }
    let s = &package.definition;
    let spec = crate::manager::CreateVm {
        id: vm.into(),
        profile: s.profile.clone(),
        directories: s.directories.clone(),
        volumes: s.volumes.clone(),
        proxy: s.proxy.clone(),
    };
    host.api("POST", "/v1/vms", Some(&serde_json::to_value(spec)?))?;
    common::save(
        &runtime(host, vm),
        &Runtime { generation: generation(host, vm)?, state: "preparing".into() },
    )
}

fn cleanup(host: &Host, vm: &str) -> Result<()> {
    if !runtime(host, vm).exists() {
        return Ok(());
    }
    let started = Instant::now();
    let mut mode = "absent".to_owned();
    if host.inspect(vm)?.is_some() {
        ensure!(
            common::read::<Runtime>(&runtime(host, vm))?.generation == generation(host, vm)?,
            "CLEANUP_INSTANCE_OWNERSHIP_CONFLICT"
        );
        let result = host.api("DELETE", &format!("/v1/vms/{vm}"), None)?;
        ensure!(result["stopped"] == true, "VM_CLEANUP_UNCONFIRMED");
        mode = result["mode"].as_str().unwrap_or("unknown").into();
    }
    std::fs::remove_file(runtime(host, vm))?;
    common::audit("managed_service", "vm_cleaned", started, json!({"vm":vm,"stop_mode":mode}));
    Ok(())
}

fn configure_proxy(host: &Host, vm: &str, value: &mut Package) -> Result<()> {
    let values = if let Some(id) = &value.definition.proxy {
        let (slot, _) = network::connection(host, vm)?;
        super::proxy::environment(&host.config.proxies[id], slot.host_ip.parse()?)?
    } else {
        Default::default()
    };
    value.definition.environment.extend(values.clone());
    value.definition.proxy_environment = Some(values);
    Ok(())
}

fn publish(host: &Host, vm: &str, package: &Package) -> Result<()> {
    for spec in &package.definition.publish {
        host.api("POST", &format!("/v1/vms/{vm}/ports"), Some(&serde_json::to_value(spec)?))?;
    }
    common::save(
        &runtime(host, vm),
        &Runtime { generation: generation(host, vm)?, state: "ready".into() },
    )?;
    crate::systemd::ready()
}

fn guest_error(value: &Value) -> &str {
    if let Some(code) = value["code"].as_str() {
        if code.starts_with("ENVIRONMENT_")
            && code.len() <= 100
            && code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
        {
            return code;
        }
    }
    match value["category"].as_str().unwrap_or("") {
        "PROXY_UNREACHABLE" => "PROXY_UNREACHABLE",
        "UNTRUSTED_GUEST_FILE" => "UNTRUSTED_GUEST_FILE",
        "SERVICE_EXITED" => "SERVICE_EXITED",
        "READINESS_TIMEOUT" => "READINESS_TIMEOUT",
        "READY_PORT_ALREADY_IN_USE" => "READY_PORT_ALREADY_IN_USE",
        "CONTROL_DISCONNECTED" => "CONTROL_DISCONNECTED",
        "EXECUTABLE_NOT_FOUND" => "EXECUTABLE_NOT_FOUND",
        "PERMISSION_DENIED" => "PERMISSION_DENIED",
        "SERVICE_BUSY" => "SERVICE_BUSY",
        "INVALID_PAYLOAD" => "INVALID_PAYLOAD",
        "GUEST_IO" => "GUEST_IO",
        "USER_ENVIRONMENT_FAILED" => "USER_ENVIRONMENT_FAILED",
        _ => "INVALID_GUEST_ERROR",
    }
}

fn event(
    host: &Host,
    vm: &str,
    value: &Package,
    action: &str,
    message: &Value,
    ready: &mut bool,
) -> Result<bool> {
    match message["event"].as_str() {
        Some("prepared") if action == "prepare" => {
            common::save(
                &runtime(host, vm),
                &Runtime { generation: generation(host, vm)?, state: "prepared".into() },
            )?;
            Ok(true)
        }
        Some("prepared") => Ok(false),
        Some("ready") => {
            ensure!(!*ready, "DUPLICATE_GUEST_READY");
            publish(host, vm, value)?;
            *ready = true;
            Ok(false)
        }
        Some("stopped" | "failed") => bail!("GUEST_{}", guest_error(message)),
        _ => bail!("INVALID_GUEST_EVENT"),
    }
}

fn watch(
    host: &Host,
    vm: &str,
    value: &Package,
    action: &str,
    child: &mut Child,
    deadline: Instant,
) -> Result<()> {
    let output = child.stdout.as_mut().context("SSH_STDOUT_MISSING")?;
    ssh::nonblocking(output.as_raw_fd())?;
    let mut pending = Vec::new();
    let mut ready = false;
    let started = Instant::now();
    loop {
        check_stop()?;
        ensure!(ready || Instant::now() < deadline, "GUEST_START_TIMEOUT");
        let mut buffer = [0u8; 4096];
        match output.read(&mut buffer) {
            Ok(0) => bail!("SSH_CONNECTION_ENDED"),
            Ok(n) => {
                pending.extend_from_slice(&buffer[..n]);
                while let Some(index) = pending.iter().position(|b| *b == b'\n') {
                    ensure!(index <= 4096, "GUEST_EVENT_TOO_LARGE");
                    let line = pending.drain(..=index).collect::<Vec<_>>();
                    let message: Value =
                        serde_json::from_slice(&line).context("INVALID_GUEST_EVENT")?;
                    if message["event"] == "prepared" {
                        common::audit(
                            "managed_service",
                            "files_prepared",
                            started,
                            json!({"vm":vm,"written":message["files_written"],
                            "preserved":message["files_preserved"],"environment":message["environment"],"guest_elapsed_ms":message["elapsed_ms"]}),
                        );
                    }
                    if event(host, vm, value, action, &message, &mut ready)? {
                        return Ok(());
                    }
                    if message["event"] == "ready" {
                        common::audit(
                            "managed_service",
                            "service_ready",
                            started,
                            json!({"vm":vm,"proxy":value.definition.proxy}),
                        );
                    }
                }
                ensure!(pending.len() <= 4096, "GUEST_EVENT_TOO_LARGE");
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(common::duration())
            }
            Err(_) => bail!("SSH_EVENT_READ_FAILED"),
        }
    }
}

fn run_guest(
    host: &Host,
    vm: &str,
    package: &Package,
    identity: &Path,
    action: &str,
    deadline: Instant,
) -> Result<()> {
    ssh::prepare_agent(host, vm, identity, deadline)?;
    let mut child =
        ssh::spawn(host, vm, identity, "exec /home/agent/.local/libexec/firecracker-guest")?;
    let result = (|| {
        let mut data =
            serde_json::to_vec(&json!({"action":action,"definition":package.definition}))?;
        data.push(b'\n');
        ssh::upload(&mut child, &data, deadline)?;
        watch(host, vm, package, action, &mut child, deadline)
    })();
    ssh::finish(&mut child);
    result
}

fn wait_management(host: &Host, deadline: Instant) -> Result<()> {
    loop {
        check_stop()?;
        match host.api("GET", "/v1/vms", None) {
            Ok(_) => return Ok(()),
            Err(error) => {
                let retry = error.chain().any(|cause| {
                    cause.downcast_ref::<reqwest::Error>().is_some_and(|e| e.is_connect())
                });
                ensure!(retry && Instant::now() < deadline, "MANAGEMENT_START_FAILED: {error}");
                std::thread::sleep(common::duration());
            }
        }
    }
}

fn execute(host: &Host, vm: &str, action: &str) -> Result<()> {
    let _guard = vm_lock(host, vm)?;
    let mut value = load(host, vm)?;
    let path = host.services().join("runtime").join(format!("{vm}.identity"));
    common::atomic(&path, value.identity.as_bytes(), 0o600)?;
    let deadline = Instant::now() + Duration::from_secs(value.definition.startup_timeout_secs);
    let result = (|| {
        wait_management(host, deadline)?;
        ensure_vm(host, vm, &value)?;
        ssh::wait_ready(host, vm, &path, deadline)?;
        configure_proxy(host, vm, &mut value)?;
        run_guest(host, vm, &value, &path, action, deadline)
    })();
    let _ = std::fs::remove_file(path);
    if result.is_err() {
        cleanup(host, vm)?;
    }
    result
}

fn run(host: &Host, vm: &str) -> Result<()> {
    STOP.store(false, Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGTERM, stopped as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stopped as *const () as libc::sighandler_t);
    }
    let result = execute(host, vm, "run");
    if STOP.load(Ordering::Relaxed) {
        return Ok(());
    }
    result
}

pub fn status(host: &Host, vm: &str) -> Result<Value> {
    let value = load(host, vm)?;
    let machine = host.inspect(vm)?;
    let record = if runtime(host, vm).exists() {
        Some(common::read::<Runtime>(&runtime(host, vm))?)
    } else {
        None
    };
    let ports = if machine.is_some() {
        host.api("GET", &format!("/v1/vms/{vm}/ports"), None)?["ports"].clone()
    } else {
        json!([])
    };
    let disk = crate::rootfs::directory(&host.config, vm).join("rootfs.ext4");
    Ok(
        json!({"vm":vm,"service":value.definition.id,"proxy":value.definition.proxy,"supervisor":active(vm)?,
        "state":if machine.is_some(){record.map(|r|r.state).unwrap_or("unknown".into())}else{"stopped".into()},
        "machine":machine,"ports":ports,"rootDisk":{"retained":disk.is_file(),"sizeBytes":disk.metadata().map(|v|v.len()).unwrap_or(0)}}),
    )
}

fn start(host: &Host, vm: &str, enable: bool) -> Result<()> {
    load(host, vm)?;
    let name = unit(vm)?;
    if enable {
        common::systemctl(&["enable", &name], true)?;
    }
    common::systemctl(&["start", &name], true)?;
    println!("{}", status(host, vm)?);
    Ok(())
}

pub fn restart(host: &Host, vm: &str) -> Result<Value> {
    load(host, vm)?;
    ensure!(
        ["active", "activating"].contains(&active(vm)?.as_str()),
        "MANAGED_SERVICE_NOT_RUNNING"
    );
    common::systemctl(&["restart", &unit(vm)?], true)?;
    let value = status(host, vm)?;
    ensure!(value["state"] == "ready", "SERVICE_RESTART_NOT_READY");
    Ok(value)
}

fn stop(host: &Host, vm: &str, destroy: bool) -> Result<()> {
    load(host, vm)?;
    common::systemctl(&["disable", "--now", &unit(vm)?], true)?;
    let _guard = vm_lock(host, vm)?;
    cleanup(host, vm)?;
    if destroy {
        host.api("DELETE", &format!("/v1/roots/{vm}"), None)?;
        std::fs::remove_file(binding(host, vm))?;
    }
    Ok(())
}

pub fn dispatch(host: &Host, args: &[String]) -> Result<()> {
    initialize(host)?;
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["install",path,"--identity",identity]=>package::install(host,Path::new(path),Path::new(identity)),
        ["setup",vm,id]=>{bind(host,vm,id)?;execute(host,vm,"prepare")},
        ["up",vm,id]=>{bind(host,vm,id)?;start(host,vm,false)},
        ["up",vm,id,"--enable"]=>{bind(host,vm,id)?;start(host,vm,true)},
        ["start",vm]=>start(host,vm,false),["run",vm]=>run(host,vm),
        ["status",vm]=>{println!("{}",status(host,vm)?);Ok(())},
        ["stop",vm]=>stop(host,vm,false),["destroy",vm]=>stop(host,vm,true),
        _=>bail!("usage: service install FILE --identity KEY | setup VM SERVICE | up VM SERVICE [--enable] | start|stop|destroy|status VM"),
    }
}
