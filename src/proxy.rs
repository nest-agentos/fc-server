use crate::{
    config::{Config, Proxy},
    error::Error,
    manager::Vm,
};
use anyhow::{ensure, Result};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::{
    net::Ipv4Addr,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};

pub fn validate(proxy: &Proxy) -> Result<()> {
    ensure!(proxy.http.is_some() || proxy.socks.is_some(), "proxy endpoint required");
    for (endpoint, schemes) in
        [(&proxy.http, &["http", "https"][..]), (&proxy.socks, &["socks5", "socks5h"][..])]
    {
        if let Some(endpoint) = endpoint {
            let parsed = reqwest::Url::parse(&endpoint.replace("{host_ip}", "192.0.2.1"))?;
            ensure!(
                schemes.contains(&parsed.scheme())
                    && parsed.port_or_known_default().is_some_and(|p| p > 0),
                "invalid proxy protocol or port"
            );
            ensure!(
                parsed.username().is_empty() && parsed.password().is_none(),
                "proxy URL credentials are not supported"
            );
            ensure!(
                ["", "/"].contains(&parsed.path())
                    && parsed.query().is_none()
                    && parsed.fragment().is_none(),
                "invalid proxy URL suffix"
            );
            let ip: Ipv4Addr =
                parsed.host_str().ok_or_else(|| anyhow::anyhow!("proxy host required"))?.parse()?;
            ensure!(
                !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_link_local()
                    && !ip.is_multicast()
                    && !ip.is_broadcast(),
                "proxy requires reachable unicast IPv4"
            );
        }
    }
    ensure!(
        proxy.no_proxy.len() <= 64
            && proxy.no_proxy.iter().all(|v| !v.is_empty()
                && v.len() <= 256
                && v.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:/[]*".contains(&b))),
        "invalid no_proxy list"
    );
    Ok(())
}

fn context(
    config: &Config,
    spec: &crate::manager::CreateVm,
    uid: u32,
    instance: &str,
    action: &str,
) -> Value {
    let profile = &config.profiles[&spec.profile];
    let index = (uid - config.uid_base) as usize;
    let slot = &profile.network_slots[index];
    json!({"action":action,"id":spec.id,"instance":instance,"slot":index,
        "netns":slot.netns,"tap":slot.tap,"guest_mac":slot.guest_mac,
        "definition":spec.proxy.as_ref().map(|id| &config.proxies[id])})
}

pub async fn prepare(
    config: &Config,
    spec: &crate::manager::CreateVm,
    uid: u32,
    instance: &str,
) -> Result<(), Error> {
    if config.proxy_egress.is_none() || config.profiles[&spec.profile].network_slots.is_empty() {
        return Ok(());
    }
    invoke(
        config,
        context(config, spec, uid, instance, if spec.proxy.is_some() { "apply" } else { "guard" }),
    )
    .await
}

pub async fn release(
    config: &Config,
    spec: &crate::manager::CreateVm,
    uid: u32,
    instance: &str,
) -> Result<(), Error> {
    if spec.proxy.is_none() {
        return Ok(());
    }
    invoke(config, context(config, spec, uid, instance, "clear")).await
}

pub async fn clear(config: &Config, vm: &Vm) -> Result<(), Error> {
    if vm.spec.proxy.is_none() {
        return Ok(());
    }
    release(config, &vm.spec, vm.uid, &vm.instance).await
}

pub async fn reset(config: &Config) -> Result<(), Error> {
    if config.proxy_egress.is_none() {
        return Ok(());
    }
    invoke(config, json!({"action":"reset"})).await
}

async fn invoke(config: &Config, value: Value) -> Result<(), Error> {
    let helper =
        config.proxy_egress.as_ref().ok_or_else(|| Error::unavailable("proxy_helper_missing"))?;
    let started = Instant::now();
    let mut child = Command::new(helper)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::io("proxy_helper_spawn", e))?;
    let work = async {
        child.stdin.take().unwrap().write_all(&serde_json::to_vec(&value).unwrap()).await?;
        child.wait_with_output().await
    };
    let output = tokio::time::timeout(Duration::from_secs(15), work)
        .await
        .map_err(|_| Error::unavailable("proxy_helper_timeout"))?
        .map_err(|e| Error::io("proxy_helper_wait", e))?;
    let data: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    crate::error::audit(
        &uuid::Uuid::new_v4().to_string(),
        "proxy_egress",
        "firewall_apply",
        started,
        output.status.success(),
    );
    if output.status.success() {
        return Ok(());
    }
    let code = match data["code"].as_str() {
        Some("PROXY_NETWORK_NOT_READY") => "PROXY_NETWORK_NOT_READY",
        Some("PROXY_NETWORK_MISMATCH") => "PROXY_NETWORK_MISMATCH",
        Some("PROXY_INSTANCE_CONFLICT") => "PROXY_INSTANCE_CONFLICT",
        Some("PROXY_CONNTRACK_FAILED") => "PROXY_CONNTRACK_FAILED",
        _ => "PROXY_EGRESS_FAILED",
    };
    Err(Error::new(StatusCode::CONFLICT, code, "proxy_egress"))
}
