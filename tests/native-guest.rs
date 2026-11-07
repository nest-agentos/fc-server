use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Guest(Child);
impl Drop for Guest {
    fn drop(&mut self) {
        self.0.stdin.take();
        let until = Instant::now() + Duration::from_secs(5);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn definition(home: &Path) -> Value {
    json!({"id":"generic","profile":"test","command":["/bin/false"],"working_directory":home,
        "ready_port":port(),"startup_timeout_secs":1,"environment":{},"files":[]})
}

fn launch(home: &Path, spec: Value, action: &str) -> Guest {
    let mut child = Command::new(env!("CARGO_BIN_EXE_firecracker-guest"))
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut data = serde_json::to_vec(&json!({"action":action,"definition":spec})).unwrap();
    data.push(b'\n');
    child.stdin.as_mut().unwrap().write_all(&data).unwrap();
    Guest(child)
}

fn event(guest: &mut Guest) -> Value {
    let mut bytes = Vec::new();
    let output = guest.0.stdout.as_mut().unwrap();
    loop {
        let mut fd = libc::pollfd {
            fd: output.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut fd, 1, 8000) } > 0, "guest event timeout");
        let mut byte = [0];
        assert_eq!(output.read(&mut byte).unwrap(), 1, "guest closed without event");
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        assert!(bytes.len() < 4096);
    }
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn preparation_is_private_and_does_not_start_application() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = definition(home.path());
    spec["files"] =
        json!([{"target":"config","mode":384,"content":STANDARD.encode("private-test-value")}]);
    let mut guest = launch(home.path(), spec, "prepare");
    assert_eq!(event(&mut guest)["event"], "prepared");
    assert!(guest.0.wait().unwrap().success());
    let path = home.path().join(".local/share/firecracker-services/generic/config");
    assert_eq!(std::fs::read(path.clone()).unwrap(), b"private-test-value");
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
}

fn prepare(home: &Path, spec: Value) -> Value {
    let mut guest = launch(home, spec, "prepare");
    let result = event(&mut guest);
    assert!(guest.0.wait().unwrap().success());
    result
}

#[test]
fn guest_edits_survive_restart_and_package_updates_until_explicit_overwrite() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = definition(home.path());
    spec["files"] = json!([{"target":"config","mode":384,"content":STANDARD.encode("host-v1")}]);
    assert_eq!(prepare(home.path(), spec.clone())["files_written"], 1);
    let path = home.path().join(".local/share/firecracker-services/generic/config");
    std::fs::write(&path, "guest-edited").unwrap();
    spec["files"][0]["content"] = json!(STANDARD.encode("host-v2"));
    let result = prepare(home.path(), spec.clone());
    assert_eq!(result["files_preserved"], 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"guest-edited");
    spec["files"][0]["overwrite"] = json!(true);
    assert_eq!(prepare(home.path(), spec)["files_written"], 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"host-v2");
}

#[test]
fn migration_preserves_existing_files_without_a_deployment_receipt() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(".local/share/firecracker-services/generic/config");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "existing-user-file").unwrap();
    let mut spec = definition(home.path());
    spec["files"] = json!([{"target":"config","mode":384,"content":STANDARD.encode("host")}]);
    assert_eq!(prepare(home.path(), spec)["files_preserved"], 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"existing-user-file");
}

#[test]
fn existing_symlink_is_rejected_without_touching_its_destination() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(".local/share/firecracker-services/generic/config");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let outside = home.path().join("outside");
    std::fs::write(&outside, "untouched").unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    let mut spec = definition(home.path());
    spec["files"] = json!([{"target":"config","mode":384,"content":STANDARD.encode("host")}]);
    let mut guest = launch(home.path(), spec, "prepare");
    assert_eq!(event(&mut guest)["category"], "UNTRUSTED_GUEST_FILE");
    assert!(!guest.0.wait().unwrap().success());
    assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
}

#[test]
fn invalid_paths_never_escape_guest_service_directory() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = definition(home.path());
    spec["files"] =
        json!([{"target":"../escape","mode":384,"content":STANDARD.encode("must-not-write")}]);
    let mut guest = launch(home.path(), spec, "prepare");
    assert_eq!(event(&mut guest)["category"], "INVALID_PAYLOAD");
    assert!(!home.path().join(".local/share/firecracker-services/escape").exists());
}

#[test]
fn executable_failure_and_service_exit_are_distinct() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = definition(home.path());
    spec["command"] = json!(["/not-an-installed-command"]);
    let mut guest = launch(home.path(), spec, "run");
    assert_eq!(event(&mut guest)["event"], "prepared");
    assert_eq!(event(&mut guest)["category"], "EXECUTABLE_NOT_FOUND");
    drop(guest);
    let mut guest = launch(home.path(), definition(home.path()), "run");
    assert_eq!(event(&mut guest)["event"], "prepared");
    let value = event(&mut guest);
    assert_eq!(value["category"], "SERVICE_EXITED");
    assert_eq!(value["exit_code"], 1);
}

#[test]
fn existing_listener_is_not_mistaken_for_application_readiness() {
    let home = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut spec = definition(home.path());
    spec["ready_port"] = json!(listener.local_addr().unwrap().port());
    let mut guest = launch(home.path(), spec, "run");
    assert_eq!(event(&mut guest)["event"], "prepared");
    assert_eq!(event(&mut guest)["category"], "READY_PORT_ALREADY_IN_USE");
}

#[test]
fn ssh_eof_reaps_application_process_group() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = definition(home.path());
    let port = spec["ready_port"].as_u64().unwrap();
    spec["command"] =
        json!([std::env::current_exe().unwrap(), "--exact", "application_fixture", "--nocapture"]);
    spec["environment"] = json!({"FC_NATIVE_FIXTURE":"listen","PORT":port.to_string()});
    let mut guest = launch(home.path(), spec, "run");
    assert_eq!(event(&mut guest)["event"], "prepared");
    assert_eq!(event(&mut guest)["event"], "ready");
    let pid = std::fs::read_to_string(home.path().join("pid")).unwrap().parse::<i32>().unwrap();
    guest.0.stdin.take();
    assert_eq!(event(&mut guest)["category"], "CONTROL_DISCONNECTED");
    assert!(guest.0.wait().unwrap().success());
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[test]
fn readiness_timeout_is_bounded() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = definition(home.path());
    spec["command"] = json!(["/bin/sleep", "60"]);
    let mut guest = launch(home.path(), spec, "run");
    assert_eq!(event(&mut guest)["event"], "prepared");
    assert_eq!(event(&mut guest)["category"], "READINESS_TIMEOUT");
    assert!(guest.0.wait().unwrap().success());
}

#[test]
fn shell_proxy_configuration_preserves_profiles_and_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".profile"), "export ORIGINAL=yes\n").unwrap();
    let mut spec = definition(home.path());
    spec["_proxy_environment"] =
        json!({"http_proxy":"http://10.204.0.1:7890","HTTP_PROXY":"http://10.204.0.1:7890"});
    for _ in 0..2 {
        let mut guest = launch(home.path(), spec.clone(), "prepare");
        assert_eq!(event(&mut guest)["event"], "prepared");
        assert!(guest.0.wait().unwrap().success());
    }
    let profile = std::fs::read_to_string(home.path().join(".profile")).unwrap();
    assert!(profile.starts_with("export ORIGINAL=yes\n"));
    assert_eq!(profile.matches("# firecracker-managed-proxy").count(), 1);
    let path = home.path().join(".config/firecracker/proxy.env");
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            ". \"$HOME/.config/firecracker/proxy.env\"; test \"$http_proxy\" = \"$HTTP_PROXY\"",
        ])
        .env("HOME", home.path())
        .status()
        .unwrap();
    assert!(output.success());
}

#[test]
fn application_fixture() {
    let Ok(mode) = std::env::var("FC_NATIVE_FIXTURE") else {
        return;
    };
    assert_eq!(mode, "listen");
    std::fs::write("pid", std::process::id().to_string()).unwrap();
    let _listener =
        TcpListener::bind(("127.0.0.1", std::env::var("PORT").unwrap().parse::<u16>().unwrap()))
            .unwrap();
    std::thread::sleep(Duration::from_secs(60));
}
