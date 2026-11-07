use crate::config::{valid_id, Config};
use anyhow::{bail, ensure, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

pub struct Host {
    pub config: Config,
    pub directory: PathBuf,
    key: String,
    client: reqwest::blocking::Client,
}

impl Host {
    pub fn load(path: &Path) -> Result<Self> {
        crate::config::trusted(path)?;
        let config: Config = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        let directory = path.parent().context("CONFIG_DIRECTORY_REQUIRED")?.to_path_buf();
        let environment = std::fs::read_to_string(directory.join("environment"))?;
        let values: BTreeMap<_, _> = environment
            .lines()
            .filter(|s| !s.starts_with('#'))
            .filter_map(|s| s.split_once('='))
            .collect();
        let key = values
            .get(config.api_key_env.as_str())
            .context("MANAGEMENT_KEY_MISSING")?
            .trim()
            .trim_matches(['\'', '"'])
            .to_owned();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(45))
            .build()?;
        Ok(Self { config, directory, key, client })
    }

    pub fn api(&self, method: &str, path: &str, data: Option<&Value>) -> Result<Value> {
        let url = format!("http://{}{}", self.config.listen, path);
        let mut request = self.client.request(method.parse()?, url).bearer_auth(&self.key);
        if let Some(value) = data {
            request = request.json(value);
        }
        let response = request.send().context("MANAGEMENT_CONNECTION_FAILED")?;
        let status = response.status();
        let value: Value = response.json().context("INVALID_MANAGEMENT_RESPONSE")?;
        if !status.is_success() {
            bail!(
                "API_{} stage={} operation={}",
                value["error"].as_str().unwrap_or("REJECTED"),
                value["stage"].as_str().unwrap_or("request"),
                value["operationId"].as_str().unwrap_or("unknown")
            );
        }
        Ok(value)
    }

    pub fn inspect(&self, id: &str) -> Result<Option<Value>> {
        id_check(id)?;
        let values = self.api("GET", "/v1/vms", None)?;
        Ok(values["vms"]
            .as_array()
            .context("INVALID_VM_LIST")?
            .iter()
            .find(|v| v["id"] == id)
            .cloned())
    }

    pub fn services(&self) -> PathBuf {
        self.config.state_dir.join("services")
    }
}

pub fn id_check(id: &str) -> Result<()> {
    ensure!(valid_id(id), "INVALID_IDENTIFIER");
    Ok(())
}

pub fn private_dir(path: &Path) -> Result<()> {
    crate::config::trusted(path.parent().context("DIRECTORY_PARENT_REQUIRED")?)?;
    if !path.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    crate::config::trusted(path)
}

pub fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    crate::config::trusted(path)?;
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub fn atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    trusted_destination(path)?;
    let temporary = path.with_file_name(format!(
        ".{}.{}",
        path.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

pub fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic(path, &serde_json::to_vec(value)?, 0o600)
}

pub fn lock(path: &Path, blocking: bool) -> Result<File> {
    trusted_destination(path)?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    use std::os::fd::AsRawFd;
    ensure!(
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_EX | if blocking { 0 } else { libc::LOCK_NB })
        } == 0,
        "RESOURCE_BUSY"
    );
    Ok(file)
}

pub fn trusted_destination(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return crate::config::trusted(path);
    }
    crate::config::trusted(path.parent().context("PARENT_REQUIRED")?)
}

pub fn run(
    program: &str,
    args: &[String],
    namespace: Option<&str>,
    data: Option<&str>,
    check: bool,
) -> Result<Output> {
    let mut command = if let Some(ns) = namespace {
        let mut c = Command::new("/usr/sbin/ip");
        c.args(["netns", "exec", ns, program]);
        c
    } else {
        Command::new(program)
    };
    parent_death(&mut command);
    let mut child = command
        .args(args)
        .stdin(if data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(value) = data {
        child.stdin.take().unwrap().write_all(value.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    ensure!(
        !check || output.status.success(),
        "HOST_COMMAND_FAILED program={} status={:?}",
        program,
        output.status.code()
    );
    Ok(output)
}

pub fn command(program: &str, args: &[&str], check: bool) -> Result<Output> {
    run(program, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(), None, None, check)
}

pub fn parent_death(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let owner = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != owner {
                return Err(std::io::Error::other("PARENT_EXITED"));
            }
            Ok(())
        });
    }
}

pub fn nft(rules: &str, namespace: Option<&str>) -> Result<()> {
    run(
        "/usr/sbin/nft",
        &["--check".into(), "-f".into(), "-".into()],
        namespace,
        Some(rules),
        true,
    )
    .context("NFT_VALIDATION_FAILED")?;
    run("/usr/sbin/nft", &["-f".into(), "-".into()], namespace, Some(rules), true)
        .context("NFT_APPLY_FAILED")?;
    Ok(())
}

pub fn audit(operation: &str, stage: &str, started: Instant, values: Value) {
    eprintln!(
        "{}",
        serde_json::json!({"operation":operation,"operation_id":uuid::Uuid::new_v4().to_string(),"stage":stage,"result":"success","elapsed_ms":started.elapsed().as_millis(),"metadata":values})
    );
}

pub fn systemctl(args: &[&str], check: bool) -> Result<Output> {
    command("/usr/bin/systemctl", args, check)
}

pub fn duration() -> Duration {
    Duration::from_millis(100)
}

use std::os::unix::fs::DirBuilderExt;
