use super::{
    common::{self, Host},
    network, service,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, time::Instant};

fn entries(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut result = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|v| v == "json") {
            result.push(entry.path());
        }
    }
    Ok(result)
}

fn bindings(host: &Host) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for path in entries(&host.services().join("vms"))? {
        let value: service::Binding = common::read(&path)?;
        let id = path.file_stem().context("BINDING_NAME_REQUIRED")?.to_string_lossy().into_owned();
        common::id_check(&id)?;
        result.insert(id, value.service);
    }
    Ok(result)
}

fn stopped(host: &Host, bindings: &BTreeMap<String, String>) -> Result<BTreeMap<String, Value>> {
    let mut result: BTreeMap<_, _> = bindings
        .keys()
        .map(|id| (id.clone(), json!({"id":id,"profile":"-","state":"stopped"})))
        .collect();
    let root = host.config.state_dir.join("roots");
    if root.exists() {
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|s| s.to_string_lossy().ends_with(".initializing")) {
                continue;
            }
            let value: Value = common::read(&path.join("manifest.json"))?;
            let id = value["spec"]["id"].as_str().context("INVALID_ROOT_MANIFEST")?;
            common::id_check(id)?;
            let mut row = value["spec"].clone();
            row["state"] = json!("stopped");
            row["rootfsPersistent"] = json!(true);
            result.insert(id.into(), row);
        }
    }
    Ok(result)
}

fn states(names: impl Iterator<Item = String>) -> Result<BTreeMap<String, String>> {
    let units = names.map(|id| service::unit(&id)).collect::<Result<Vec<_>>>()?;
    if units.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut args = vec!["show".to_owned()];
    args.extend(units);
    args.extend(["-p".into(), "Id,ActiveState".into()]);
    let output = common::run("/usr/bin/systemctl", &args, None, None, true)?;
    let text = String::from_utf8(output.stdout)?;
    let mut result = BTreeMap::new();
    for block in text.trim().split("\n\n") {
        let values: BTreeMap<_, _> = block.lines().filter_map(|s| s.split_once('=')).collect();
        if let Some(id) = values.get("Id") {
            result.insert(
                (*id).to_owned(),
                values.get("ActiveState").unwrap_or(&"unknown").to_string(),
            );
        }
    }
    Ok(result)
}

fn row(
    host: &Host,
    machine: &Value,
    service: Option<&String>,
    states: &BTreeMap<String, String>,
) -> Result<Value> {
    let id = machine["id"].as_str().context("VM_ID_REQUIRED")?;
    let profile = machine["profile"].as_str().context("PROFILE_REQUIRED")?;
    let mut address = None;
    let mut generation = None;
    if machine["state"] == "vmm-running"
        && host.config.profiles.get(profile).is_some_and(|p| !p.network_slots.is_empty())
    {
        match network::connection(host, id) {
            Ok((slot, alias)) => {
                address = Some(slot.guest_ip);
                generation = Some(alias);
            }
            Err(error) => {
                if !error.chain().any(|e| {
                    e.downcast_ref::<std::io::Error>()
                        .is_some_and(|v| v.kind() == std::io::ErrorKind::NotFound)
                }) {
                    return Err(error);
                }
            }
        }
    }
    let mut state = service
        .map(|_| states.get(&service::unit(id).unwrap()).cloned().unwrap_or("unknown".into()));
    let path = host.services().join("runtime").join(format!("{id}.json"));
    if state.as_deref() == Some("active") && path.is_file() {
        let record: service::Runtime = common::read(&path)?;
        if generation.as_deref() == Some(&record.generation) {
            state = Some(record.state);
        }
    }
    Ok(
        json!({"id":id,"profile":profile,"service":service,"proxy":machine["proxy"],"state":machine["state"],"serviceState":state,
        "ip":address,"rootfsPersistent":machine.get("rootfsPersistent").cloned().unwrap_or(json!(host.config.profiles.get(profile).is_some_and(|p|p.rootfs_persistent))),
        "resources":machine["resources"],"ageSecs":machine["ageSecs"],"ports":machine.get("ports").cloned().unwrap_or(json!([]))}),
    )
}

pub fn collect(host: &Host, all: bool) -> Result<Vec<Value>> {
    let started = Instant::now();
    let values = host.api("GET", "/v1/vms", None)?;
    let bindings = bindings(host)?;
    let mut machines = if all { stopped(host, &bindings)? } else { BTreeMap::new() };
    for value in values["vms"].as_array().context("INVALID_VM_LIST")? {
        machines.insert(value["id"].as_str().context("VM_ID_REQUIRED")?.to_owned(), value.clone());
    }
    let states = states(machines.keys().filter(|id| bindings.contains_key(*id)).cloned())?;
    let result = machines
        .iter()
        .map(|(id, machine)| row(host, machine, bindings.get(id), &states))
        .collect::<Result<Vec<_>>>()?;
    common::audit("vm_list", "snapshot", started, json!({"count":result.len(),"api_calls":1}));
    Ok(result)
}

fn ports(value: &Value) -> String {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .map(|v| {
                    format!(
                        "{}:{}->{}/{}",
                        v["listen"].as_str().unwrap_or("?"),
                        v["host_port"],
                        v["guest_port"],
                        v["protocol"].as_str().unwrap_or("?")
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or("-".into())
}

pub fn table(values: &[Value], all: bool) -> String {
    if values.is_empty() {
        return if all { "未找到虚拟机。\n" } else { "当前没有运行的虚拟机。\n" }.into();
    }
    let mut rows = vec![[
        "VM",
        "PROFILE",
        "SERVICE",
        "PROXY",
        "VM_STATE",
        "SERVICE_STATE",
        "IP",
        "VCPUS",
        "MEM_MIB",
        "ROOT",
        "PORTS",
    ]
    .map(String::from)
    .to_vec()];
    for v in values {
        let text = |key: &str| v[key].as_str().unwrap_or("-").to_owned();
        rows.push(vec![
            text("id"),
            text("profile"),
            text("service"),
            text("proxy"),
            text("state"),
            text("serviceState"),
            text("ip"),
            v["resources"]["vcpus"].as_u64().map(|v| v.to_string()).unwrap_or("-".into()),
            v["resources"]["memory_mib"].as_u64().map(|v| v.to_string()).unwrap_or("-".into()),
            if v["rootfsPersistent"] == true { "persistent" } else { "ephemeral" }.into(),
            ports(&v["ports"]),
        ]);
    }
    let widths: Vec<_> =
        (0..rows[0].len()).map(|i| rows.iter().map(|r| r[i].len()).max().unwrap()).collect();
    rows.iter()
        .map(|r| {
            r.iter()
                .zip(&widths)
                .map(|(v, w)| format!("{v:w$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
                + "\n"
        })
        .collect()
}

pub fn run(host: &Host, args: &[String]) -> Result<()> {
    ensure!(args.iter().all(|s| s == "--all" || s == "--json"), "usage: list [--all] [--json]");
    let all = args.iter().any(|s| s == "--all");
    let values = collect(host, all)?;
    if args.iter().any(|s| s == "--json") {
        println!("{}", json!({"vms":values}));
    } else {
        print!("{}", table(&values, all));
    }
    Ok(())
}
