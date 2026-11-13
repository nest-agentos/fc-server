use super::common::{self, Host};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub const RUNTIME: &str = "/run/nestos-firecracker-network";
pub const PLAN: &str = "/etc/nestos-firecracker/network.json";
pub const BLOCKED: &str = "0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.0.0.0/24, 192.0.2.0/24, 192.168.0.0/16, 198.18.0.0/15, 198.51.100.0/24, 203.0.113.0/24, 224.0.0.0/4, 240.0.0.0/4";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slot {
    pub index: usize,
    pub namespace: String,
    pub tap: String,
    pub host_interface: String,
    pub peer_interface: String,
    pub guest_subnet: String,
    pub uplink_subnet: String,
    pub guest_ip: String,
    pub gateway: String,
    pub host_ip: String,
    pub peer_ip: String,
    pub guest_mac: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Plan {
    pub schema: u32,
    pub egress_interface: String,
    pub dns_servers: Vec<String>,
    pub guest_network: String,
    pub uplink_network: String,
    pub slots: Vec<Slot>,
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value.bytes().all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c))
}

pub fn contains(subnet: &str, address: Ipv4Addr) -> Result<bool> {
    let (network, bits) = subnet.split_once('/').context("INVALID_SUBNET")?;
    let bits: u32 = bits.parse()?;
    ensure!(bits <= 32, "INVALID_SUBNET");
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    Ok(u32::from(network.parse::<Ipv4Addr>()?) & mask == u32::from(address) & mask)
}

pub fn plan() -> Result<Plan> {
    let plan: Plan = common::read(Path::new(PLAN))?;
    ensure!(name(&plan.egress_interface) && !plan.slots.is_empty(), "INVALID_NETWORK_PLAN");
    for (index, item) in plan.slots.iter().enumerate() {
        ensure!(
            item.index == index
                && [&item.namespace, &item.tap, &item.host_interface, &item.peer_interface]
                    .iter()
                    .all(|v| name(v)),
            "INVALID_NETWORK_SLOT"
        );
        for address in [&item.guest_ip, &item.gateway, &item.host_ip, &item.peer_ip] {
            address.parse::<Ipv4Addr>()?;
        }
        ensure!(
            item.guest_mac.len() == 17
                && item.guest_mac.bytes().all(|c| c.is_ascii_hexdigit() || c == b':'),
            "INVALID_GUEST_MAC"
        );
        for subnet in
            [&item.guest_subnet, &item.uplink_subnet, &plan.guest_network, &plan.uplink_network]
        {
            let (address, bits) = subnet.split_once('/').context("INVALID_SUBNET")?;
            address.parse::<Ipv4Addr>()?;
            ensure!(bits.parse::<u8>()? <= 32, "INVALID_SUBNET");
        }
    }
    Ok(plan)
}

pub fn state_path() -> PathBuf {
    Path::new(RUNTIME).join("state.json")
}

pub fn run(args: &[&str], ns: Option<&str>, check: bool) -> Result<std::process::Output> {
    common::run(
        "/usr/sbin/ip",
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        ns,
        None,
        check,
    )
}

pub fn connection(host: &Host, id: &str) -> Result<(Slot, String)> {
    common::id_check(id)?;
    use std::os::unix::fs::MetadataExt;
    let root = crate::launcher::jail(&host.config, id).join("root");
    let uid = std::fs::metadata(root.join("rootfs.ext4"))
        .with_context(|| format!("VM_NOT_RUNNING: {id}; start the VM before connecting"))?
        .uid();
    let index = uid.checked_sub(host.config.uid_base).context("VM_NETWORK_UID_MISMATCH")? as usize;
    let item = plan()?.slots.get(index).context("VM_NETWORK_SLOT_MISMATCH")?.clone();
    let generation = std::fs::metadata(root.join("firecracker.pid"))?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_nanos();
    Ok((item, format!("nestos-{id}-{generation}")))
}

pub fn empty(host: &Host) -> Result<()> {
    match host.api("GET", "/v1/vms", None) {
        Ok(value) => {
            ensure!(value["vms"].as_array().is_some_and(Vec::is_empty), "ACTIVE_VMS_EXIST")
        }
        Err(error) => {
            ensure!(
                common::systemctl(&["is-active", "--quiet", "firecracker-server"], false)?
                    .status
                    .code()
                    != Some(0),
                "CANNOT_VERIFY_ACTIVE_VMS: {error}"
            );
            let dir = host
                .config
                .state_dir
                .join(host.config.firecracker.file_name().context("INVALID_VMM_PATH")?);
            ensure!(
                !dir.exists() || std::fs::read_dir(dir)?.next().is_none(),
                "STALE_VM_RESOURCES_EXIST"
            );
        }
    }
    Ok(())
}

fn blocked_set() -> String {
    format!("set blocked {{ type ipv4_addr; flags interval; elements = {{ {BLOCKED} }}; }}\n")
}

pub fn host_rules(plan: &Plan) -> String {
    let mut s = format!("table inet nestos_firecracker {{\n{}set proxy_peers {{ type ifname . ipv4_addr . ipv4_addr . inet_service; }}\nset published_marks {{ type mark; }}\n", blocked_set());
    s += "chain published_guard { ct mark @published_marks ct status dnat accept\nct mark & 0xff000000 == 0xfc000000 drop\n}\nchain published_forward { }\n";
    s += "chain output { type filter hook output priority 0; policy accept; oifname \"mfc-h*\" jump published_guard\n}\n";
    s += "chain input { type filter hook input priority -20; policy accept;\niifname \"mfc-h*\" jump published_guard\niifname . ip saddr . ip daddr . tcp dport @proxy_peers accept\niifname \"mfc-h*\" ct state established,related accept\niifname \"mfc-h*\" drop\n}\n";
    s += "chain forward { type filter hook forward priority -20; policy accept;\n";
    for item in &plan.slots {
        s += &format!(
            "iifname \"{}\" ip saddr != {{ {}, {} }} drop\n",
            item.host_interface, item.guest_ip, item.peer_ip
        );
    }
    s += "iifname \"mfc-h*\" jump published_guard\noifname \"mfc-h*\" jump published_guard\niifname . ip saddr . ip daddr . tcp dport @proxy_peers accept\niifname \"mfc-h*\" ip daddr @blocked drop\n";
    s += &format!("iifname \"mfc-h*\" oifname \"{}\" meta nfproto ipv4 accept\niifname \"{}\" oifname \"mfc-h*\" ct state established,related accept\n",plan.egress_interface,plan.egress_interface);
    s += "iifname \"mfc-h*\" drop\noifname \"mfc-h*\" drop\n}\n}\ntable ip nestos_firecracker_nat {\nchain published_prerouting { }\nchain published_output { }\nchain published_snat { }\n";
    s += "chain prerouting { type nat hook prerouting priority dstnat; policy accept; iifname != \"mfc-h*\" jump published_prerouting\n}\nchain output { type nat hook output priority dstnat; policy accept; jump published_output\n}\nchain postrouting { type nat hook postrouting priority srcnat; policy accept; jump published_snat\n";
    s += &format!("iifname \"mfc-h*\" oifname \"{}\" masquerade\n}}\n}}\n", plan.egress_interface);
    s
}

pub fn namespace_rules(i: &Slot) -> String {
    format!("table inet nestos_firecracker {{\n{}set proxy_targets {{ type ipv4_addr . inet_service; }}\nchain input {{ type filter hook input priority 0; policy drop;\niifname \"lo\" accept\nct state established,related accept\niifname \"{}\" udp dport 67 accept\niifname \"{}\" ip saddr {} ip daddr {} udp dport 53 accept\niifname \"{}\" ip saddr {} ip daddr {} tcp dport 53 accept\niifname \"{}\" ip daddr {} icmp type echo-request accept\n}}\nchain published_forward {{ }}\nchain forward {{ type filter hook forward priority 0; policy drop;\niifname \"{}\" ip saddr != {} drop\nct state established,related accept\njump published_forward\niifname \"{}\" ip daddr . tcp dport @proxy_targets accept\niifname \"{}\" ip saddr {} ip daddr {} tcp dport 22 accept\niifname \"{}\" ip daddr @blocked drop\niifname \"{}\" oifname \"{}\" ip saddr {} accept\n}}\n}}\n",blocked_set(),i.tap,i.tap,i.guest_ip,i.gateway,i.tap,i.guest_ip,i.gateway,i.tap,i.gateway,i.tap,i.guest_ip,i.tap,i.peer_interface,i.host_ip,i.guest_ip,i.tap,i.tap,i.peer_interface,i.guest_ip)
}

fn sysctl(values: &[&str], ns: Option<&str>) -> Result<()> {
    let args: Vec<String> =
        ["-q", "-w"].into_iter().chain(values.iter().copied()).map(String::from).collect();
    common::run("/usr/sbin/sysctl", &args, ns, None, true)?;
    Ok(())
}

fn prepare(i: &Slot) -> Result<()> {
    run(&["netns", "add", &i.namespace], None, true)?;
    run(
        &["link", "add", &i.host_interface, "type", "veth", "peer", "name", &i.peer_interface],
        None,
        true,
    )?;
    run(&["link", "set", &i.host_interface, "alias", "nestos-firecracker-network"], None, true)?;
    run(&["link", "set", &i.peer_interface, "netns", &i.namespace], None, true)?;
    run(&["address", "add", &format!("{}/30", i.host_ip), "dev", &i.host_interface], None, true)?;
    run(&["link", "set", &i.host_interface, "up"], None, true)?;
    sysctl(&[&format!("net.ipv4.conf.{}.route_localnet=1", i.host_interface)], None)?;
    run(
        &["address", "add", &format!("{}/30", i.peer_ip), "dev", &i.peer_interface],
        Some(&i.namespace),
        true,
    )?;
    run(&["link", "set", &i.peer_interface, "up"], Some(&i.namespace), true)?;
    run(&["link", "set", "lo", "up"], Some(&i.namespace), true)?;
    run(&["tuntap", "add", &i.tap, "mode", "tap"], Some(&i.namespace), true)?;
    run(
        &["address", "add", &format!("{}/30", i.gateway), "dev", &i.tap],
        Some(&i.namespace),
        true,
    )?;
    run(&["link", "set", &i.tap, "up"], Some(&i.namespace), true)?;
    run(&["route", "add", "default", "via", &i.host_ip], Some(&i.namespace), true)?;
    run(
        &["route", "add", &i.guest_subnet, "via", &i.peer_ip, "dev", &i.host_interface],
        None,
        true,
    )?;
    sysctl(&["net.ipv4.ip_forward=1", "net.ipv6.conf.all.disable_ipv6=1"], Some(&i.namespace))?;
    common::nft(&namespace_rules(i), Some(&i.namespace))
}

pub fn down(host: &Host) -> Result<()> {
    empty(host)?;
    if !state_path().exists() {
        return Ok(());
    }
    let value: Value = common::read(&state_path())?;
    let slots: Vec<Slot> = serde_json::from_value(value["slots"].clone())?;
    for i in slots.iter().rev() {
        run(&["link", "delete", &i.host_interface], None, false)?;
        run(&["netns", "delete", &i.namespace], None, false)?;
    }
    for (family, name) in [("inet", "nestos_firecracker"), ("ip", "nestos_firecracker_nat")] {
        common::command("/usr/sbin/nft", &["delete", "table", family, name], false)?;
    }
    std::fs::remove_file(state_path())?;
    Ok(())
}

fn unused(plan: &Plan) -> Result<()> {
    for (family, name) in [("inet", "nestos_firecracker"), ("ip", "nestos_firecracker_nat")] {
        ensure!(
            !common::command("/usr/sbin/nft", &["list", "table", family, name], false)?
                .status
                .success(),
            "UNOWNED_NETWORK_TABLE"
        );
    }
    for item in &plan.slots {
        ensure!(
            !Path::new("/run/netns").join(&item.namespace).exists(),
            "UNOWNED_NETWORK_NAMESPACE"
        );
        ensure!(
            !run(&["link", "show", &item.host_interface], None, false)?.status.success(),
            "UNOWNED_NETWORK_INTERFACE"
        );
    }
    Ok(())
}

pub fn up(host: &Host) -> Result<()> {
    empty(host)?;
    let p = plan()?;
    common::private_dir(Path::new(RUNTIME))?;
    if state_path().exists() {
        down(host)?;
    }
    unused(&p)?;
    check_routes(&p)?;
    let mut prepared = Vec::new();
    common::save(&state_path(), &json!({"slots":prepared}))?;
    let result = (|| {
        sysctl(&["net.ipv4.ip_forward=1"], None)?;
        common::nft(&host_rules(&p), None)?;
        for item in &p.slots {
            prepared.push(item);
            common::save(&state_path(), &json!({"slots":prepared}))?;
            prepare(item)?;
        }
        Ok(())
    })();
    if result.is_err() {
        down(host)?;
    }
    result
}

fn check_routes(plan: &Plan) -> Result<()> {
    let routes: Value = serde_json::from_slice(
        &run(&["-j", "-4", "route", "show", "table", "all"], None, true)?.stdout,
    )?;
    for route in routes.as_array().context("INVALID_ROUTE_RESPONSE")? {
        let destination = route["dst"].as_str().unwrap_or("default");
        if destination == "default" || route["dev"].as_str().is_some_and(|s| s.starts_with("mfc-h"))
        {
            continue;
        }
        let address = destination.split('/').next().context("INVALID_ROUTE")?.parse()?;
        let canonical = if destination.contains('/') {
            destination.to_owned()
        } else {
            format!("{destination}/32")
        };
        for subnet in [&plan.guest_network, &plan.uplink_network] {
            let base = subnet.split('/').next().unwrap().parse()?;
            ensure!(
                !contains(subnet, address)? && !contains(&canonical, base)?,
                "NETWORK_OVERLAPS_EXISTING_ROUTE"
            );
        }
    }
    Ok(())
}

pub fn slot_for(value: &Value, plan: &Plan, category: &str) -> Result<Slot> {
    let index = value["slot"].as_u64().context("INVALID_SLOT")? as usize;
    let slot = plan.slots.get(index).with_context(|| category.to_owned())?;
    ensure!(
        value["netns"] == format!("/run/netns/{}", slot.namespace)
            && value["tap"] == slot.tap
            && value["guest_mac"] == slot.guest_mac,
        "{category}"
    );
    let state: Value = common::read(&state_path())?;
    let active: Vec<Slot> = serde_json::from_value(state["slots"].clone())?;
    ensure!(active.contains(slot), "NETWORK_NOT_READY");
    Ok(slot.clone())
}

pub fn dispatch(host: &Host, args: &[String]) -> Result<()> {
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["up"] => up(host),
        ["down"] => down(host),
        ["idle"] => empty(host),
        ["address", id] => {
            let (slot, alias) = connection(host, id)?;
            println!("{} {}", slot.guest_ip, alias);
            Ok(())
        }
        _ => bail!("usage: network up|down|idle|address VM_ID"),
    }
}

pub fn vm_command(host: &Host, args: &[String]) -> Result<()> {
    ensure!(args.len() >= 2, "VM_ID_REQUIRED");
    common::id_check(&args[1])?;
    let base = format!("/v1/vms/{}", args[1]);
    let result = match args[0].as_str() {
        "start" => host.api(
            "POST",
            "/v1/vms",
            Some(&json!({"id":args[1],"profile":"debian13","directories":args[2..].to_vec()})),
        )?,
        "stop" => host.api("DELETE", &base, None)?,
        "ports" => host.api("GET", &format!("{base}/ports"), None)?,
        "publish" | "unpublish" => publish_command(host, &base, args)?,
        _ => bail!("INVALID_VM_COMMAND"),
    };
    println!("{result}");
    Ok(())
}

fn publish_command(host: &Host, base: &str, args: &[String]) -> Result<Value> {
    ensure!(args.len() >= 3, "PORT_MAPPING_REQUIRED");
    let (mapping, protocol) = args[2].split_once('/').unwrap_or((&args[2], "tcp"));
    ensure!(["tcp", "udp"].contains(&protocol), "INVALID_PROTOCOL");
    if args[0] == "unpublish" {
        let port: u16 = mapping.parse()?;
        ensure!(port > 0, "INVALID_PORT");
        return host.api("DELETE", &format!("{base}/ports/{protocol}/{port}"), None);
    }
    let (source, target) = mapping.split_once(':').context("EXPECTED_HOST_GUEST_PORTS")?;
    let host_port: u16 = source.parse()?;
    let guest_port: u16 = target.parse()?;
    let listen = if args.len() == 5 && args[3] == "--listen" {
        args[4].parse::<Ipv4Addr>()?
    } else {
        ensure!(args.len() == 3, "INVALID_PUBLISH_ARGUMENTS");
        Ipv4Addr::LOCALHOST
    };
    host.api("POST",&format!("{base}/ports"),Some(&json!({"listen":listen,"host_port":host_port,"guest_port":guest_port,"protocol":protocol})))
}
