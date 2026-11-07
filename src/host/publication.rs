use super::{
    common,
    network::{self, Plan, Slot},
};
use crate::publication::{Protocol, Specification};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, net::Ipv4Addr, path::Path};

const ROOT: &str = "/run/mindos-firecracker-publish";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    listen: Ipv4Addr,
    host_port: u16,
    guest_port: u16,
    protocol: Protocol,
    id: String,
    instance: String,
    slot: usize,
    token: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    records: Vec<Record>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<Vec<Record>>,
}

fn protocol(p: Protocol) -> &'static str {
    match p {
        Protocol::Tcp => "tcp",
        Protocol::Udp => "udp",
    }
}

fn guard() -> Result<()> {
    ensure!(
        common::command(
            "/usr/sbin/nft",
            &["list", "set", "inet", "mindos_firecracker", "published_marks"],
            false
        )?
        .status
        .success(),
        "PUBLICATION_NETWORK_NOT_READY"
    );
    Ok(())
}

fn root_rules(records: &[Record], plan: &Plan) -> String {
    let mut s = "flush set inet mindos_firecracker published_marks\n".to_owned();
    for chain in ["published_output", "published_prerouting", "published_snat"] {
        s += &format!("flush chain ip mindos_firecracker_nat {chain}\n");
    }
    if !records.is_empty() {
        s += &format!(
            "add element inet mindos_firecracker published_marks {{ {} }}\n",
            records.iter().map(|r| r.token.to_string()).collect::<Vec<_>>().join(", ")
        );
    }
    for r in records {
        let i = &plan.slots[r.slot];
        let p = protocol(r.protocol);
        let target = if r.listen.is_unspecified() {
            "fib daddr type local".to_owned()
        } else {
            format!("ip daddr {}", r.listen)
        };
        let action = format!(
            "{target} {p} dport {} ct mark set {} dnat to {}:{}\n",
            r.host_port, r.token, i.guest_ip, r.guest_port
        );
        s += &format!("add rule ip mindos_firecracker_nat published_output {action}");
        if !r.listen.is_loopback() {
            s+=&format!("add rule ip mindos_firecracker_nat published_prerouting ip daddr != 127.0.0.0/8 {action}");
        }
        s+=&format!("add rule ip mindos_firecracker_nat published_snat ct mark {} oifname \"{}\" ip daddr {} {p} dport {} snat to {}\n",r.token,i.host_interface,i.guest_ip,r.guest_port,i.host_ip);
    }
    s
}

fn namespace_rules(records: &[Record], i: &Slot) -> String {
    let mut s = "flush chain inet mindos_firecracker published_forward\n".to_owned();
    let entries: BTreeSet<_> = records
        .iter()
        .filter(|r| r.slot == i.index)
        .map(|r| (protocol(r.protocol), r.guest_port))
        .collect();
    for (p, port) in entries {
        s+=&format!("add rule inet mindos_firecracker published_forward iifname \"{}\" ip saddr {} ip daddr {} {p} dport {port} accept\n",i.peer_interface,i.host_ip,i.guest_ip);
    }
    s
}

fn apply(records: &[Record], plan: &Plan, slots: &BTreeSet<usize>) -> Result<()> {
    for &index in slots {
        let i = &plan.slots[index];
        common::nft(&namespace_rules(records, i), Some(&i.namespace))?;
    }
    common::nft(&root_rules(records, plan), None)
}

fn conntrack(args: &[String], ns: Option<&str>) -> Result<()> {
    let output = common::run("/usr/sbin/conntrack", args, ns, None, false)?;
    ensure!(
        output.status.success()
            || String::from_utf8_lossy(&output.stderr).contains("0 flow entries"),
        "PUBLICATION_CONNTRACK_FAILED"
    );
    Ok(())
}

fn purge(token: u32) -> Result<()> {
    conntrack(&["-D", "-f", "ipv4", "--mark", &token.to_string()].map(String::from), None)
}

fn purge_listener(record: &Record) -> Result<()> {
    let mut addresses = BTreeSet::from([record.listen.to_string()]);
    if record.listen.is_unspecified() {
        addresses.clear();
        addresses.insert("127.0.0.0/8".into());
        let interfaces: Value =
            serde_json::from_slice(&network::run(&["-j", "-4", "addr"], None, true)?.stdout)?;
        for interface in interfaces.as_array().context("INVALID_INTERFACE_RESPONSE")? {
            for info in interface["addr_info"].as_array().context("INVALID_ADDRESS_RESPONSE")? {
                if info["family"] == "inet" {
                    let a: Ipv4Addr =
                        info["local"].as_str().context("INVALID_LOCAL_ADDRESS")?.parse()?;
                    addresses.insert(a.to_string());
                }
            }
        }
    }
    for address in addresses {
        conntrack(
            &[
                "-D",
                "-f",
                "ipv4",
                "-p",
                protocol(record.protocol),
                "--orig-dst",
                &address,
                "--dport",
                &record.host_port.to_string(),
            ]
            .map(String::from),
            None,
        )?;
    }
    Ok(())
}

fn flush(i: &Slot) -> Result<()> {
    common::run(
        "/usr/sbin/conntrack",
        &["-F", "-f", "ipv4"].map(String::from),
        Some(&i.namespace),
        None,
        true,
    )?;
    Ok(())
}

fn changed_slots(old: &[Record], new: &[Record]) -> BTreeSet<usize> {
    old.iter()
        .chain(new)
        .map(|r| r.slot)
        .filter(|i| {
            old.iter().filter(|r| r.slot == *i).collect::<Vec<_>>()
                != new.iter().filter(|r| r.slot == *i).collect::<Vec<_>>()
        })
        .collect()
}

fn changes(old: &[Record], new: &[Record]) -> Result<()> {
    for r in old {
        if !new.iter().any(|n| n.token == r.token) {
            purge(r.token)?;
        }
    }
    for r in new {
        if !old.iter().any(|n| n.token == r.token) {
            purge_listener(r)?;
        }
    }
    Ok(())
}

fn transaction(
    old: &[Record],
    new: &[Record],
    plan: &Plan,
    flush_slots: &[Slot],
    path: &Path,
) -> Result<()> {
    if old == new {
        for i in flush_slots {
            flush(i)?;
        }
        return Ok(());
    }
    let changed = changed_slots(old, new);
    common::save(path, &Ledger { records: old.to_vec(), pending: Some(new.to_vec()) })?;
    let result = (|| {
        apply(new, plan, &changed)?;
        changes(old, new)?;
        for i in flush_slots {
            flush(i)?;
        }
        common::save(path, &Ledger { records: new.to_vec(), pending: None })
    })();
    if result.is_err() {
        let rollback = (|| {
            apply(old, plan, &changed)?;
            for r in new {
                if !old.iter().any(|o| o.token == r.token) {
                    purge(r.token)?;
                }
            }
            common::save(path, &Ledger { records: old.to_vec(), pending: None })
        })();
        if rollback.is_err() {
            common::nft("flush set inet mindos_firecracker published_marks\n", None)?;
        }
    }
    result
}

fn add(records: &[Record], value: &Value) -> Result<Vec<Record>> {
    let spec: Specification = serde_json::from_value(value["spec"].clone())?;
    ensure!(
        spec.host_port > 0
            && spec.guest_port > 0
            && !spec.listen.is_multicast()
            && !spec.listen.is_broadcast(),
        "INVALID_PUBLICATION"
    );
    let id = value["id"].as_str().context("PUBLICATION_VM_REQUIRED")?;
    common::id_check(id)?;
    let instance = value["instance"].as_str().context("PUBLICATION_INSTANCE_REQUIRED")?;
    uuid::Uuid::parse_str(instance)?;
    let slot = value["slot"].as_u64().context("PUBLICATION_SLOT_REQUIRED")? as usize;
    for r in records {
        if r.protocol == spec.protocol
            && r.host_port == spec.host_port
            && (r.listen == spec.listen
                || r.listen.is_unspecified()
                || spec.listen.is_unspecified())
        {
            ensure!(
                r.id == id
                    && r.instance == instance
                    && r.slot == slot
                    && r.listen == spec.listen
                    && r.guest_port == spec.guest_port,
                "HOST_PORT_IN_USE"
            );
            return Ok(records.to_vec());
        }
    }
    let mut token;
    loop {
        token = 0xfc000000 | (uuid::Uuid::new_v4().as_u128() as u32 & 0x00ffffff);
        if !records.iter().any(|r| r.token == token) {
            break;
        }
    }
    let mut result = records.to_vec();
    result.push(Record {
        listen: spec.listen,
        host_port: spec.host_port,
        guest_port: spec.guest_port,
        protocol: spec.protocol,
        id: id.into(),
        instance: instance.into(),
        slot,
        token,
    });
    Ok(result)
}

fn recover(ledger: Ledger, plan: &Plan, path: &Path) -> Result<Vec<Record>> {
    if let Some(pending) = ledger.pending {
        apply(&ledger.records, plan, &plan.slots.iter().map(|i| i.index).collect())?;
        for r in pending {
            if !ledger.records.iter().any(|o| o.token == r.token) {
                purge(r.token)?;
            }
        }
        common::save(path, &Ledger { records: ledger.records.clone(), pending: None })?;
    }
    Ok(ledger.records)
}

pub fn execute(value: &Value) -> Result<()> {
    ensure!(unsafe { libc::geteuid() } == 0, "PUBLICATION_REQUIRES_ROOT");
    common::private_dir(Path::new(ROOT))?;
    let _lock = common::lock(&Path::new(ROOT).join("lock"), true)?;
    let path = Path::new(ROOT).join("ports.json");
    let ledger: Ledger = if path.exists() { common::read(&path)? } else { Ledger::default() };
    let action = value["action"].as_str().context("PUBLICATION_ACTION_REQUIRED")?;
    if action == "reset" && !Path::new(network::PLAN).exists() && ledger.records.is_empty() {
        return Ok(());
    }
    guard()?;
    let plan = network::plan()?;
    if action == "reset" {
        let mut records = ledger.records;
        for r in ledger.pending.unwrap_or_default() {
            if !records.iter().any(|o| o.token == r.token) {
                records.push(r);
            }
        }
        return transaction(&records, &[], &plan, &plan.slots, &path);
    }
    let slot = network::slot_for(value, &plan, "PUBLICATION_NETWORK_MISMATCH")?;
    let records = recover(ledger, &plan, &path)?;
    let own = |r: &&Record| value["id"] == r.id && value["instance"] == r.instance;
    let new = match action {
        "add" => add(&records, value)?,
        "remove" => records
            .iter()
            .filter(|r| {
                !(own(r)
                    && value["spec"]["host_port"] == r.host_port
                    && value["spec"]["protocol"] == protocol(r.protocol))
            })
            .cloned()
            .collect(),
        "clear" => records.iter().filter(|r| !own(r)).cloned().collect(),
        _ => anyhow::bail!("INVALID_ACTION"),
    };
    transaction(
        &records,
        &new,
        &plan,
        if action == "clear" { std::slice::from_ref(&slot) } else { &[] },
        &path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(listen: &str, slot: usize) -> Value {
        serde_json::json!({"id":format!("vm-{slot}"),"instance":"12345678-1234-4234-8234-123456789abc","slot":slot,
            "spec":{"listen":listen,"host_port":18787,"guest_port":8787,"protocol":"tcp"}})
    }
    #[test]
    fn publication_is_idempotent_and_wildcard_conflicts_with_specific_listeners() {
        let records = add(&[], &request("127.0.0.1", 0)).unwrap();
        assert_eq!(add(&records, &request("127.0.0.1", 0)).unwrap(), records);
        assert!(add(&records, &request("0.0.0.0", 1))
            .unwrap_err()
            .to_string()
            .contains("HOST_PORT_IN_USE"));
        assert_eq!(add(&records, &request("192.168.31.1", 1)).unwrap().len(), 2);
    }
    #[test]
    fn loopback_publication_never_enters_external_prerouting() {
        let plan: Plan =
            serde_json::from_str(include_str!("../../tests/fixtures/network-plan.json")).unwrap();
        let records = add(&[], &request("127.0.0.1", 0)).unwrap();
        let rules = root_rules(&records, &plan);
        assert!(!rules.contains("add rule ip mindos_firecracker_nat published_prerouting"));
        let mut other = plan.slots[0].clone();
        other.index = 1;
        assert!(namespace_rules(&records, &other).lines().count() == 1);
        assert_eq!(changed_slots(&records, &records), BTreeSet::new());
        assert_eq!(changed_slots(&records, &[]), BTreeSet::from([0]));
    }
    #[test]
    fn unique_instance_token_survives_serialization_and_changes_after_recreation() {
        let first = add(&[], &request("127.0.0.1", 0)).unwrap();
        let second = add(&[], &request("127.0.0.1", 0)).unwrap();
        assert_ne!(first[0].token, second[0].token);
        assert_eq!(first[0].token & 0xff000000, 0xfc000000);
        let ledger: Ledger = serde_json::from_str(
            &serde_json::to_string(&Ledger {
                records: first.clone(),
                pending: Some(second.clone()),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(ledger.records, first);
        assert_eq!(ledger.pending.unwrap(), second);
    }
}
