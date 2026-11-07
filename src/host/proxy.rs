use super::{
    common,
    network::{self, Plan},
};
use crate::config::Proxy;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
    path::Path,
};

const ROOT: &str = "/run/mindos-firecracker-proxy";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Record {
    id: String,
    instance: String,
    slot: usize,
    targets: Vec<(Ipv4Addr, u16)>,
}

#[derive(Default, Deserialize, Serialize)]
struct Ledger {
    records: Vec<Record>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollback: Option<Vec<Record>>,
}

pub fn is_proxy_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "http_proxy" | "https_proxy" | "all_proxy" | "no_proxy"
    )
}

pub fn environment(proxy: &Proxy, host: Ipv4Addr) -> Result<BTreeMap<String, String>> {
    crate::proxy::validate(proxy)?;
    let mut values = BTreeMap::new();
    if let Some(http) = &proxy.http {
        let url = http.replace("{host_ip}", &host.to_string());
        values.insert("http_proxy".to_owned(), url.clone());
        values.insert("https_proxy".into(), url);
    }
    if let Some(socks) = &proxy.socks {
        values.insert("all_proxy".into(), socks.replace("{host_ip}", &host.to_string()));
    }
    values.insert("no_proxy".into(), proxy.no_proxy.join(","));
    for (key, value) in values.clone() {
        values.insert(key.to_uppercase(), value);
    }
    Ok(values)
}

fn record(value: &Value, plan: &Plan) -> Result<Record> {
    let slot = network::slot_for(value, plan, "PROXY_NETWORK_MISMATCH")?;
    let id = value["id"].as_str().context("PROXY_VM_REQUIRED")?.to_owned();
    common::id_check(&id)?;
    let instance = value["instance"].as_str().context("PROXY_INSTANCE_REQUIRED")?.to_owned();
    uuid::Uuid::parse_str(&instance)?;
    let definition: Proxy = serde_json::from_value(value["definition"].clone())?;
    crate::proxy::validate(&definition)?;
    let mut targets = BTreeSet::new();
    for url in [&definition.http, &definition.socks].into_iter().flatten() {
        let url = reqwest::Url::parse(&url.replace("{host_ip}", &slot.host_ip))?;
        let address: Ipv4Addr = url.host_str().context("PROXY_HOST_REQUIRED")?.parse()?;
        ensure!(!network::contains(&plan.guest_network, address)?, "PROXY_GUEST_NETWORK_FORBIDDEN");
        targets.insert((address, url.port_or_known_default().context("PROXY_PORT_REQUIRED")?));
    }
    Ok(Record { id, instance, slot: slot.index, targets: targets.into_iter().collect() })
}

fn root_rules(records: &[Record], plan: &Plan) -> String {
    let mut entries = BTreeSet::new();
    for r in records {
        let i = &plan.slots[r.slot];
        for (ip, port) in &r.targets {
            entries
                .insert(format!("\"{}\" . {} . {} . {}", i.host_interface, i.guest_ip, ip, port));
        }
    }
    let mut result = "flush set inet mindos_firecracker proxy_peers\n".to_owned();
    if !entries.is_empty() {
        result += &format!(
            "add element inet mindos_firecracker proxy_peers {{ {} }}\n",
            entries.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    result
}

fn namespace_rules(records: &[Record], index: usize) -> String {
    let entries: BTreeSet<_> = records
        .iter()
        .filter(|r| r.slot == index)
        .flat_map(|r| r.targets.iter().map(|(a, p)| format!("{a} . {p}")))
        .collect();
    let mut result = "flush set inet mindos_firecracker proxy_targets\n".to_owned();
    if !entries.is_empty() {
        result += &format!(
            "add element inet mindos_firecracker proxy_targets {{ {} }}\n",
            entries.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    result
}

fn apply(records: &[Record], plan: &Plan, indices: &BTreeSet<usize>) -> Result<()> {
    common::nft(&root_rules(records, plan), None)?;
    for &index in indices {
        common::nft(&namespace_rules(records, index), Some(&plan.slots[index].namespace))?;
    }
    Ok(())
}

fn purge(records: &[Record], plan: &Plan) -> Result<()> {
    for r in records {
        let i = &plan.slots[r.slot];
        for (address, port) in &r.targets {
            let args = [
                "-D",
                "-p",
                "tcp",
                "-s",
                &i.guest_ip,
                "-d",
                &address.to_string(),
                "--dport",
                &port.to_string(),
            ]
            .map(String::from);
            for namespace in [None, Some(i.namespace.as_str())] {
                let output = common::run("/usr/sbin/conntrack", &args, namespace, None, false)?;
                ensure!(
                    output.status.success()
                        || String::from_utf8_lossy(&output.stderr).contains("0 flow entries"),
                    "PROXY_CONNTRACK_FAILED"
                );
            }
        }
    }
    Ok(())
}

fn transaction(old: &[Record], desired: &[Record], plan: &Plan, path: &Path) -> Result<()> {
    let indices = old.iter().chain(desired).map(|r| r.slot).collect();
    common::save(path, &Ledger { records: desired.to_vec(), rollback: Some(old.to_vec()) })?;
    let result = (|| {
        apply(desired, plan, &indices)?;
        purge(&old.iter().filter(|r| !desired.contains(r)).cloned().collect::<Vec<_>>(), plan)
    })();
    if result.is_err() {
        if apply(old, plan, &indices).is_err() {
            common::nft("flush set inet mindos_firecracker proxy_peers\n", None)?;
            anyhow::bail!("PROXY_ROLLBACK_FAILED");
        }
        common::save(path, &Ledger { records: old.to_vec(), rollback: None })?;
        return result;
    }
    common::save(path, &Ledger { records: desired.to_vec(), rollback: None })
}

pub fn execute(value: &Value) -> Result<()> {
    ensure!(unsafe { libc::geteuid() } == 0, "PROXY_REQUIRES_ROOT");
    common::private_dir(Path::new(ROOT))?;
    let _lock = common::lock(&Path::new(ROOT).join("lock"), true)?;
    let path = Path::new(ROOT).join("rules.json");
    let action = value["action"].as_str().context("PROXY_ACTION_REQUIRED")?;
    if action == "reset" && !path.exists() {
        return Ok(());
    }
    let plan = network::plan()?;
    ensure!(
        common::command(
            "/usr/sbin/nft",
            &["list", "set", "inet", "mindos_firecracker", "proxy_peers"],
            false
        )?
        .status
        .success(),
        "PROXY_NETWORK_NOT_READY"
    );
    let mut ledger: Ledger = if path.exists() { common::read(&path)? } else { Ledger::default() };
    if let Some(old) = ledger.rollback.take() {
        ensure!(action == "reset", "PROXY_RECOVERY_REQUIRED");
        for r in old {
            if !ledger.records.contains(&r) {
                ledger.records.push(r);
            }
        }
    }
    if action == "reset" {
        return transaction(&ledger.records, &[], &plan, &path);
    }
    if action == "guard" {
        let slot = network::slot_for(value, &plan, "PROXY_NETWORK_MISMATCH")?;
        ensure!(!ledger.records.iter().any(|r| r.slot == slot.index), "PROXY_INSTANCE_CONFLICT");
        return Ok(());
    }
    let selected = record(value, &plan)?;
    ensure!(
        ledger
            .records
            .iter()
            .filter(|r| r.id == selected.id || r.slot == selected.slot)
            .all(|r| r == &selected),
        "PROXY_INSTANCE_CONFLICT"
    );
    let mut desired: Vec<_> =
        ledger.records.iter().filter(|r| r.id != selected.id).cloned().collect();
    if action == "apply" {
        desired.push(selected);
    } else {
        ensure!(action == "clear", "INVALID_PROXY_ACTION");
    }
    transaction(&ledger.records, &desired, &plan, &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_environment_uses_slot_host_and_both_cases() {
        let proxy: Proxy =
            toml::from_str("http='http://{host_ip}:7890'\nsocks='socks5h://{host_ip}:7890'")
                .unwrap();
        let env = environment(&proxy, "10.204.3.1".parse().unwrap()).unwrap();
        assert_eq!(env["HTTP_PROXY"], "http://10.204.3.1:7890");
        assert_eq!(env["http_proxy"], env["HTTPS_PROXY"]);
        assert_eq!(env["all_proxy"], "socks5h://10.204.3.1:7890");
        assert_eq!(env.len(), 8);
    }
}

#[cfg(test)]
mod rule_tests {
    use super::*;
    #[test]
    fn exact_proxy_exceptions_remain_isolated_by_guest_slot_and_port() {
        let plan: Plan =
            serde_json::from_str(include_str!("../../tests/fixtures/network-plan.json")).unwrap();
        let record = Record {
            id: "first".into(),
            instance: uuid::Uuid::new_v4().to_string(),
            slot: 0,
            targets: vec![("10.204.0.1".parse().unwrap(), 7890)],
        };
        let rules = root_rules(std::slice::from_ref(&record), &plan);
        assert!(rules.contains("\"mfc-h0\" . 10.203.0.2 . 10.204.0.1 . 7890"));
        assert!(!rules.contains("mfc-h1"));
        assert_eq!(
            namespace_rules(std::slice::from_ref(&record), 1),
            "flush set inet mindos_firecracker proxy_targets\n"
        );
        assert!(namespace_rules(&[record], 0).contains("10.204.0.1 . 7890"));
    }
}
