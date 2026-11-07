//! Restart managed workloads or recreate ordinary VMs with their original policy.
use super::{
    common::{self, Host},
    service,
};
use crate::{manager::CreateVm, publication::Specification};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::time::Instant;

pub fn run(host: &Host, args: &[String]) -> Result<()> {
    ensure!(args.len() == 1, "usage: reboot VM_ID");
    let id = &args[0];
    common::id_check(id)?;
    let operation = uuid::Uuid::new_v4().to_string();
    let started = Instant::now();
    let result = execute(host, id, &operation);
    crate::error::audit(&operation, "vm_reboot", "complete", started, result.is_ok());
    if let Err(error) = &result {
        tracing::error!(operation_id=%operation, operation="vm_reboot", vm_id=%id,
            stage="complete", category=%error, result="failed");
    }
    println!("{}", result?);
    Ok(())
}

fn execute(host: &Host, id: &str, operation: &str) -> Result<Value> {
    let binding = host.services().join("vms").join(format!("{id}.json"));
    if binding.exists() {
        let started = Instant::now();
        let result = service::restart(host, id);
        crate::error::audit(operation, "vm_reboot", "systemd_restart", started, result.is_ok());
        return result.context("MANAGED_REBOOT_FAILED");
    }
    let lock_dir = host.config.state_dir.join("reboot-locks");
    common::private_dir(&lock_dir)?;
    let _guard = common::lock(&lock_dir.join(format!("{id}.lock")), false)?;
    let machine = host.inspect(id)?.context("VM_NOT_RUNNING")?;
    recreate(&machine, |method, path, data| {
        let started = Instant::now();
        let stage = match method {
            "DELETE" => "stop",
            _ if path.ends_with("/ports") => "restore_ports",
            _ => "create",
        };
        let result = host.api(method, path, data);
        crate::error::audit(operation, "vm_reboot", stage, started, result.is_ok());
        result
    })
}

fn recreate(
    machine: &Value,
    mut request: impl FnMut(&str, &str, Option<&Value>) -> Result<Value>,
) -> Result<Value> {
    ensure!(machine["state"] == "vmm-running", "VM_NOT_RUNNING");
    let spec = json!({"id":machine["id"],"profile":machine["profile"],
        "directories":machine["directories"],"volumes":machine["volumes"],"proxy":machine["proxy"]});
    let spec: CreateVm = serde_json::from_value(spec).context("INVALID_VM_SNAPSHOT")?;
    common::id_check(&spec.id)?;
    let ports: Vec<Specification> =
        serde_json::from_value(machine["ports"].clone()).context("INVALID_PORT_SNAPSHOT")?;
    let base = format!("/v1/vms/{}", spec.id);
    let stopped = request("DELETE", &base, None).context("REBOOT_STOP_FAILED")?;
    ensure!(stopped["stopped"] == true, "REBOOT_STOP_UNCONFIRMED");
    let created = request("POST", "/v1/vms", Some(&serde_json::to_value(&spec)?))
        .context("REBOOT_CREATE_FAILED")?;
    for port in &ports {
        request("POST", &format!("{base}/ports"), Some(&serde_json::to_value(port)?))
            .context("REBOOT_PORT_RESTORE_FAILED: recreated VM remains running")?;
    }
    Ok(json!({"vm":spec.id,"mode":"recreated","rootfsPersistent":machine["rootfsPersistent"],
        "machine":created,"ports":ports}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn machine() -> Value {
        json!({"id":"demo","state":"vmm-running","profile":"custom","directories":["project"],
            "volumes":["home"],"proxy":"clash","rootfsPersistent":true,
            "ports":[{"listen":"127.0.0.1","host_port":18787,"guest_port":8787,"protocol":"tcp"}]})
    }
    #[test]
    fn ordinary_reboot_preserves_policy_and_restores_ports_after_creation() {
        let mut calls = vec![];
        let value = recreate(&machine(), |method, path, data| {
            calls.push((method.to_owned(), path.to_owned(), data.cloned()));
            Ok(if method == "DELETE" { json!({"stopped":true}) } else { json!({"id":"demo"}) })
        })
        .unwrap();
        assert_eq!(
            calls.iter().map(|(m, p, _)| (m.as_str(), p.as_str())).collect::<Vec<_>>(),
            [("DELETE", "/v1/vms/demo"), ("POST", "/v1/vms"), ("POST", "/v1/vms/demo/ports")]
        );
        let spec = calls[1].2.as_ref().unwrap();
        for key in ["profile", "directories", "volumes", "proxy"] {
            assert_eq!(spec[key], machine()[key]);
        }
        assert_eq!(value["rootfsPersistent"], true);
    }
    #[test]
    fn unconfirmed_stop_never_creates_a_replacement() {
        let mut calls = 0;
        let error = recreate(&machine(), |_, _, _| {
            calls += 1;
            Ok(json!({"stopped":false}))
        })
        .unwrap_err();
        assert!(error.to_string().contains("STOP_UNCONFIRMED"));
        assert_eq!(calls, 1);
    }
    #[test]
    fn failed_creation_never_restores_ports() {
        let mut calls = 0;
        let error = recreate(&machine(), |_, _, _| {
            calls += 1;
            if calls == 1 {
                Ok(json!({"stopped":true}))
            } else {
                anyhow::bail!("fixture failure")
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("CREATE_FAILED"));
        assert_eq!(calls, 2);
    }
    #[test]
    fn incomplete_snapshot_never_stops_the_vm() {
        let mut value = machine();
        value.as_object_mut().unwrap().remove("ports");
        assert!(recreate(&value, |_, _, _| panic!("must not mutate")).is_err());
    }
}
