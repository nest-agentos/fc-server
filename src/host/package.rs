use super::{
    common::{self, Host},
    service,
};
use crate::config::{valid_id, Backend};
use crate::publication::Specification;
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Component, Path},
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub target: String,
    pub mode: u32,
    pub content: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub overwrite: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub id: String,
    pub profile: String,
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    pub command: Vec<String>,
    pub working_directory: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<Asset>,
    #[serde(default)]
    pub publish: Vec<Specification>,
    pub ready_port: u16,
    #[serde(default = "timeout")]
    pub startup_timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_proxy_environment")]
    pub proxy_environment: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_environment: Option<super::environment::UserEnvironment>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub definition: Service,
    pub identity: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSource {
    source: String,
    target: String,
    #[serde(default)]
    executable: bool,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    id: String,
    profile: String,
    #[serde(default)]
    directories: Vec<String>,
    #[serde(default)]
    volumes: Vec<String>,
    proxy: Option<String>,
    command: Vec<String>,
    working_directory: String,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    files: Vec<FileSource>,
    #[serde(default)]
    publish: Vec<Specification>,
    ready_port: u16,
    #[serde(default = "timeout")]
    startup_timeout_secs: u64,
}

fn timeout() -> u64 {
    60
}

pub fn relative(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(
        !value.is_empty()
            && !path.is_absolute()
            && path.components().all(|v| matches!(v, Component::Normal(_))),
        "INVALID_RELATIVE_PATH"
    );
    ensure!(!["run.lock", "service.log", "service.log.1"].contains(&value), "RESERVED_FILE");
    ensure!(
        path.to_string_lossy() == value && !value.contains("//") && !value.ends_with('/'),
        "INVALID_RELATIVE_PATH"
    );
    Ok(())
}

fn variable(key: &str) -> bool {
    !key.is_empty()
        && !key.as_bytes()[0].is_ascii_digit()
        && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn validate(service: &Service) -> Result<()> {
    ensure!(valid_id(&service.id) && valid_id(&service.profile), "INVALID_IDENTIFIER");
    ensure!(
        !service.command.is_empty()
            && !service.command[0].is_empty()
            && service.command.iter().all(|s| !s.contains('\0')),
        "INVALID_COMMAND"
    );
    ensure!(
        Path::new(&service.working_directory).is_absolute()
            && !service.working_directory.contains('\0'),
        "INVALID_WORKING_DIRECTORY"
    );
    ensure!(
        service.ready_port > 0 && (1..=300).contains(&service.startup_timeout_secs),
        "INVALID_READINESS"
    );
    ensure!(
        service.environment.iter().all(|(k, v)| variable(k) && !v.contains('\0')),
        "INVALID_ENVIRONMENT"
    );
    for values in [&service.directories, &service.volumes] {
        ensure!(
            values.iter().all(|id| valid_id(id))
                && values.iter().collect::<std::collections::BTreeSet<_>>().len() == values.len(),
            "INVALID_RESOURCE_SET"
        );
    }
    if let Some(proxy) = &service.proxy {
        ensure!(valid_id(proxy), "INVALID_PROXY_ID");
        ensure!(
            !service.environment.keys().any(|k| super::proxy::is_proxy_key(k)),
            "PROXY_ENVIRONMENT_CONFLICT"
        );
    }
    ensure!(service.files.len() <= 64 && service.publish.len() <= 16, "SERVICE_LIMIT_EXCEEDED");
    for (index, file) in service.files.iter().enumerate() {
        relative(&file.target)?;
        ensure!([0o600, 0o700].contains(&file.mode), "INVALID_FILE_MODE");
        ensure!(
            service.files[index + 1..]
                .iter()
                .all(|other| !Path::new(&other.target).starts_with(&file.target)
                    && !Path::new(&file.target).starts_with(&other.target)),
            "OVERLAPPING_FILES"
        );
    }
    for (index, spec) in service.publish.iter().enumerate() {
        ensure!(
            !service.publish[index + 1..].iter().any(|other| other.listen == spec.listen
                && other.host_port == spec.host_port
                && other.protocol == spec.protocol),
            "DUPLICATE_PUBLICATION"
        );
        ensure!(
            spec.host_port > 0
                && spec.guest_port > 0
                && !spec.listen.is_multicast()
                && !spec.listen.is_broadcast(),
            "INVALID_PUBLICATION"
        );
    }
    Ok(())
}

pub fn definition(path: &Path) -> Result<Service> {
    let text = std::fs::read_to_string(path)?;
    let m: Manifest = toml::from_str(&text).map_err(|error: toml::de::Error| {
        let before = &text[..error.span().map(|r| r.start).unwrap_or(0)];
        anyhow::anyhow!(
            "SERVICE_TOML_INVALID line={} column={}",
            before.lines().count().max(1),
            before.rsplit('\n').next().unwrap_or("").len() + 1
        )
    })?;
    let parent = path.parent().context("PACKAGE_DIRECTORY_REQUIRED")?.canonicalize()?;
    let mut size = 0;
    let mut files = Vec::new();
    for file in m.files {
        relative(&file.source)?;
        relative(&file.target)?;
        let source = parent.join(file.source);
        ensure!(
            !source.symlink_metadata()?.is_symlink() && source.canonicalize()?.starts_with(&parent),
            "FILE_OUTSIDE_PACKAGE"
        );
        ensure!(source.metadata()?.is_file(), "FILE_NOT_REGULAR");
        let mut bytes = Vec::new();
        std::fs::File::open(source)?.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        size += bytes.len();
        ensure!(size <= 32 * 1024 * 1024, "PACKAGE_TOO_LARGE");
        files.push(Asset {
            target: file.target,
            mode: if file.executable { 0o700 } else { 0o600 },
            content: STANDARD.encode(bytes),
            overwrite: file.overwrite,
        });
    }
    let service = Service {
        id: m.id,
        profile: m.profile,
        directories: m.directories,
        volumes: m.volumes,
        proxy: m.proxy,
        command: m.command,
        working_directory: m.working_directory,
        environment: m.environment,
        files,
        publish: m.publish,
        ready_port: m.ready_port,
        startup_timeout_secs: m.startup_timeout_secs,
        proxy_environment: None,
        user_environment: None,
    };
    validate(&service)?;
    Ok(service)
}

pub fn resources(host: &Host, service: &Service) -> Result<()> {
    let profile = host.config.profiles.get(&service.profile).context("UNKNOWN_PROFILE")?;
    ensure!(!profile.network_slots.is_empty(), "PROFILE_REQUIRES_SSH_NETWORK");
    for id in &service.directories {
        ensure!(
            profile.bindings.contains(id)
                && host.config.bindings.get(id).is_some_and(|b| b.backend == Backend::Virtiofs),
            "DIRECTORY_NOT_AUTHORIZED"
        );
    }
    if let Some(proxy) = &service.proxy {
        ensure!(profile.allowed_proxies.contains(proxy), "PROXY_NOT_AUTHORIZED");
    }
    Ok(())
}

pub fn read(host: &Host, id: &str) -> Result<Package> {
    common::id_check(id)?;
    common::read(&host.services().join("packages").join(format!("{id}.json")))
}

pub fn install(host: &Host, path: &Path, identity: &Path) -> Result<()> {
    let definition = definition(&path.canonicalize()?)?;
    resources(host, &definition)?;
    ensure!(
        identity.metadata()?.is_file() && identity.metadata()?.len() <= 65536,
        "INVALID_SSH_IDENTITY"
    );
    let value = Package { definition, identity: std::fs::read_to_string(identity)? };
    install_value(host, value)
}

pub fn install_value(host: &Host, value: Package) -> Result<()> {
    let started = std::time::Instant::now();
    validate(&value.definition)?;
    resources(host, &value.definition)?;
    let root = host.services();
    let _guard =
        common::lock(&root.join("locks").join(format!("package.{}", value.definition.id)), false)?;
    let path = root.join("packages").join(format!("{}.json", value.definition.id));
    if path.exists() && common::read::<Package>(&path)? == value {
        return Ok(());
    }
    let mut guards = Vec::new();
    for entry in std::fs::read_dir(root.join("vms"))? {
        let entry = entry?;
        let binding: service::Binding = common::read(&entry.path())?;
        if binding.service == value.definition.id {
            let name = entry.path().file_stem().unwrap().to_string_lossy().into_owned();
            guards.push(service::vm_lock(host, &name)?);
            ensure!(
                ["inactive", "failed"].contains(&service::active(&name)?.as_str()),
                "SERVICE_UPDATE_REQUIRES_STOPPED_SUPERVISORS"
            );
        }
    }
    common::save(&path, &value)?;
    common::audit(
        "managed_service",
        "package_installed",
        started,
        serde_json::json!({"service":value.definition.id,"file_count":value.definition.files.len()}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest(root: &Path, extra: &str) -> std::path::PathBuf {
        let path = root.join("service.toml");
        std::fs::write(&path,format!("id='generic'\nprofile='test'\ncommand=['/bin/true']\nworking_directory='/home/agent'\nready_port=8080\n{extra}")).unwrap();
        path
    }
    #[test]
    fn relative_paths_and_reserved_targets_are_rejected() {
        for name in ["../outside", "/etc/config", ".", "a/../b", "run.lock", "service.log"] {
            assert!(relative(name).is_err(), "{name}");
        }
    }
    #[test]
    fn package_snapshots_assets_without_retaining_source_paths() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config"), "private-fixture").unwrap();
        let path = manifest(root.path(), "[[files]]\nsource='config'\ntarget='config'");
        let service = definition(&path).unwrap();
        std::fs::write(root.path().join("config"), "changed").unwrap();
        assert_eq!(STANDARD.decode(&service.files[0].content).unwrap(), b"private-fixture");
        assert!(!serde_json::to_string(&service)
            .unwrap()
            .contains(&root.path().to_string_lossy().to_string()));
    }
    #[test]
    fn overwrite_is_an_explicit_per_file_policy() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config"), "fixture").unwrap();
        let path =
            manifest(root.path(), "[[files]]\nsource='config'\ntarget='config'\noverwrite=true");
        let service = definition(&path).unwrap();
        assert!(service.files[0].overwrite);
        assert_eq!(serde_json::to_value(&service).unwrap()["files"][0]["overwrite"], true);
        let path = manifest(root.path(), "[[files]]\nsource='config'\ntarget='config'");
        let service = definition(&path).unwrap();
        assert!(!service.files[0].overwrite);
        assert!(serde_json::to_value(&service).unwrap()["files"][0].get("overwrite").is_none());
    }
    #[test]
    fn symlink_and_overlapping_assets_fail() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config"), "fixture").unwrap();
        std::os::unix::fs::symlink("config", root.path().join("alias")).unwrap();
        let path = manifest(root.path(), "[[files]]\nsource='alias'\ntarget='config'");
        assert!(definition(&path).unwrap_err().to_string().contains("FILE_OUTSIDE_PACKAGE"));
        let path=manifest(root.path(),"[[files]]\nsource='config'\ntarget='config'\n[[files]]\nsource='config'\ntarget='config/child'");
        assert!(definition(&path).unwrap_err().to_string().contains("OVERLAPPING_FILES"));
    }
    #[test]
    fn proxy_reference_and_manual_proxy_environment_cannot_conflict() {
        let root = tempfile::tempdir().unwrap();
        let path = manifest(
            root.path(),
            "proxy='route'\n[environment]\nhttp_proxy='http://10.204.0.1:7890'",
        );
        assert!(definition(&path).unwrap_err().to_string().contains("PROXY_ENVIRONMENT_CONFLICT"));
    }
    #[test]
    fn malformed_manifest_errors_do_not_echo_secret_values() {
        let root = tempfile::tempdir().unwrap();
        let path = manifest(root.path(), "unknown_setting='private-fixture-secret'");
        let error = definition(&path).unwrap_err().to_string();
        assert!(error.contains("SERVICE_TOML_INVALID"));
        assert!(!error.contains("private-fixture-secret"));
    }
}
