//! Native host administration. Build-time image tooling is deliberately separate.
pub mod common;
pub mod inventory;
pub mod network;
pub mod package;
pub mod proxy;
pub mod publication;
pub mod reboot;
pub mod service;
pub mod ssh;

use anyhow::{bail, ensure, Result};
use std::path::Path;

pub fn dispatch(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|s| s == "list") && unsafe { libc::geteuid() } != 0 {
        use std::os::unix::process::CommandExt;
        let installed = Path::new("/usr/local/bin/firecracker-server");
        crate::config::trusted(installed)?;
        return Err(std::process::Command::new("/usr/bin/sudo")
            .arg("--")
            .arg(installed)
            .args(args)
            .exec()
            .into());
    }
    ensure!(unsafe { libc::geteuid() } == 0, "HOST_COMMAND_REQUIRES_ROOT");
    let host = common::Host::load(Path::new("/etc/mindos-firecracker/config.toml"))?;
    match args.first().map(String::as_str) {
        Some("list") => inventory::run(&host, &args[1..]),
        Some("service") => service::dispatch(&host, &args[1..]),
        Some("network") => network::dispatch(&host, &args[1..]),
        Some("reboot") => reboot::run(&host, &args[1..]),
        Some("start" | "stop" | "publish" | "unpublish" | "ports") => {
            network::vm_command(&host, args)
        }
        _ => bail!("UNKNOWN_HOST_COMMAND"),
    }
}

pub fn helper(kind: &str, reset: bool) -> Result<()> {
    let value = if reset {
        serde_json::json!({"action":"reset"})
    } else {
        serde_json::from_reader(std::io::stdin().lock().take(1024 * 1024))?
    };
    let started = std::time::Instant::now();
    let result = match kind {
        "publish-ports" => publication::execute(&value),
        "proxy-egress" => proxy::execute(&value),
        _ => unreachable!(),
    };
    let code = result.as_ref().err().map(|e| e.to_string());
    eprintln!(
        "{}",
        serde_json::json!({"operation":kind,"operation_id":uuid::Uuid::new_v4().to_string(),
        "stage":value["action"],"result":if result.is_ok(){"success"}else{"failure"},
        "category":code,"elapsed_ms":started.elapsed().as_millis()})
    );
    println!("{}", serde_json::json!({"code":code}));
    result
}

use std::io::Read;
