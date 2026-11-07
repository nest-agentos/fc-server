//! Unprivileged, static guest supervisor uploaded over SSH; no interpreter required.
mod files;
mod supervisor;

use crate::host::package::Service;
use anyhow::{ensure, Result};
use serde::Deserialize;
use std::{
    io::{BufRead, Read, Write},
    os::fd::AsRawFd,
    sync::atomic::{AtomicBool, Ordering},
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}
fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}

#[derive(Deserialize)]
struct Payload {
    action: String,
    definition: Service,
}

pub fn emit(value: serde_json::Value) -> Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

pub fn run() -> Result<()> {
    unsafe {
        libc::umask(0o077);
        for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            libc::signal(signal, stop as *const () as libc::sighandler_t);
        }
    }
    let mut input = std::io::stdin().lock();
    let mut data = Vec::new();
    (&mut input).take(48 * 1024 * 1024 + 1).read_until(b'\n', &mut data)?;
    ensure!(data.len() <= 48 * 1024 * 1024 && data.last() == Some(&b'\n'), "INVALID_PAYLOAD");
    let payload: Payload =
        serde_json::from_slice(&data).map_err(|_| anyhow::anyhow!("INVALID_PAYLOAD"))?;
    ensure!(crate::config::valid_id(&payload.definition.id), "INVALID_PAYLOAD");
    ensure!(
        payload.definition.ready_port > 0
            && (1..=300).contains(&payload.definition.startup_timeout_secs),
        "INVALID_PAYLOAD"
    );
    ensure!(["prepare", "run"].contains(&payload.action.as_str()), "INVALID_PAYLOAD");
    let root = files::root(&payload.definition)?;
    let _guard = files::lock(&root)?;
    let started = std::time::Instant::now();
    let (written, preserved) = files::prepare(&payload.definition, &root)?;
    emit(serde_json::json!({"event":"prepared","files_written":written,
        "files_preserved":preserved,"elapsed_ms":started.elapsed().as_millis()}))?;
    if payload.action == "run" {
        supervisor::execute(&payload.definition, &root, input.as_raw_fd())?;
    }
    Ok(())
}

pub fn category(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::NotFound => "EXECUTABLE_NOT_FOUND",
                std::io::ErrorKind::PermissionDenied => "PERMISSION_DENIED",
                _ => "GUEST_IO",
            };
        }
    }
    match error.to_string().as_str() {
        "PROXY_UNREACHABLE" => "PROXY_UNREACHABLE",
        "UNTRUSTED_GUEST_FILE" => "UNTRUSTED_GUEST_FILE",
        "SERVICE_BUSY" => "SERVICE_BUSY",
        "READY_PORT_ALREADY_IN_USE" => "READY_PORT_ALREADY_IN_USE",
        _ => "INVALID_PAYLOAD",
    }
}
