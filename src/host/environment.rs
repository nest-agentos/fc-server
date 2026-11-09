//! Compile a private YAML environment into the existing managed-service snapshot.
use super::{
    common::{self, Host},
    package::{self, Package},
    service,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UserEnvironment {
    pub git: Git,
    #[serde(default)]
    pub apply_token: Option<String>,
}
impl std::fmt::Debug for UserEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UserEnvironment([redacted])")
    }
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Git {
    pub user: Author,
    pub lfs: bool,
    pub github: Github,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub name: String,
    pub email: String,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Github {
    pub private_key: String,
    pub known_hosts: String,
    pub https_to_ssh: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u8,
    vm: Vm,
    rootfs: Rootfs,
    service: ServiceSource,
    git: GitSource,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vm {
    id: String,
    profile: String,
    #[serde(default)]
    resources: Option<crate::resources::Resources>,
    proxy: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rootfs {
    persistent: bool,
    user: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceSource {
    manifest: PathBuf,
    identity_file: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitSource {
    user: Author,
    lfs: bool,
    github: GithubSource,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubSource {
    private_key_file: PathBuf,
    known_hosts_file: PathBuf,
    https_to_ssh: bool,
}

fn read(path: &Path) -> Result<String> {
    ensure!(
        path.metadata().context("ENVIRONMENT_FILE_UNAVAILABLE")?.len() <= 65536,
        "ENVIRONMENT_FILE_TOO_LARGE"
    );
    std::fs::read_to_string(path).context("ENVIRONMENT_FILE_READ_FAILED")
}
fn resolve(root: &Path, path: &Path) -> PathBuf {
    root.join(path)
}
fn author(user: &Author) -> Result<()> {
    for value in [&user.name, &user.email] {
        ensure!(
            !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
            "ENVIRONMENT_INVALID_GIT_AUTHOR"
        );
    }
    Ok(())
}
fn credentials(root: &Path, source: &GithubSource) -> Result<Github> {
    let path = resolve(root, &source.private_key_file);
    use std::os::unix::fs::PermissionsExt;
    ensure!(path.metadata()?.permissions().mode() & 0o077 == 0, "ENVIRONMENT_KEY_PERMISSIONS");
    let private_key = read(&path)?;
    ensure!(private_key.contains("PRIVATE KEY-----"), "ENVIRONMENT_INVALID_PRIVATE_KEY");
    let path = resolve(root, &source.known_hosts_file);
    read(&path)?;
    let output = std::process::Command::new("ssh-keygen")
        .args(["-F", "github.com", "-f"])
        .arg(path)
        .output()
        .context("ENVIRONMENT_HOST_KEY_LOOKUP_FAILED")?;
    ensure!(output.status.success(), "ENVIRONMENT_GITHUB_HOST_KEY_MISSING");
    let selected = String::from_utf8(output.stdout).context("ENVIRONMENT_HOST_KEY_INVALID")?;
    let known_hosts =
        selected.lines().filter(|line| !line.starts_with('#')).collect::<Vec<_>>().join("\n")
            + "\n";
    Ok(Github { private_key, known_hosts, https_to_ssh: source.https_to_ssh })
}
fn compile(path: &Path) -> Result<(Document, Package)> {
    let path = path.canonicalize().context("ENVIRONMENT_FILE_UNAVAILABLE")?;
    let root = path.parent().context("ENVIRONMENT_DIRECTORY_REQUIRED")?;
    let document: Document = serde_yaml::from_str(&read(&path)?).map_err(|error| {
        anyhow::anyhow!(
            "ENVIRONMENT_YAML_INVALID line={}",
            error.location().map(|p| p.line()).unwrap_or(0)
        )
    })?;
    ensure!(document.version == 1, "ENVIRONMENT_VERSION_UNSUPPORTED");
    common::id_check(&document.vm.id)?;
    ensure!(document.rootfs.user == "agent", "ENVIRONMENT_USER_UNSUPPORTED");
    author(&document.git.user)?;
    let mut definition = package::definition(&resolve(root, &document.service.manifest))?;
    definition.profile = document.vm.profile.clone();
    definition.proxy = document.vm.proxy.clone();
    if let Some(resources) = &document.vm.resources {
        definition.resources = resources.clone();
    }
    definition.user_environment = Some(UserEnvironment {
        git: Git {
            user: document.git.user.clone(),
            lfs: document.git.lfs,
            github: credentials(root, &document.git.github)?,
        },
        apply_token: None,
    });
    let identity = read(&resolve(root, &document.service.identity_file))?;
    Ok((document, Package { definition, identity }))
}
fn validate(host: &Host, document: &Document, value: &Package) -> Result<()> {
    let profile = host.config.profiles.get(&document.vm.profile).context("UNKNOWN_PROFILE")?;
    ensure!(
        profile.rootfs_persistent == document.rootfs.persistent,
        "ENVIRONMENT_ROOTFS_PROFILE_MISMATCH"
    );
    package::validate(&value.definition)?;
    package::resources(host, &value.definition)
}
fn exclusive(host: &Host, vm: &str, id: &str, applying: bool) -> Result<()> {
    for entry in std::fs::read_dir(host.services().join("vms"))? {
        let entry = entry?;
        let binding: service::Binding = common::read(&entry.path())?;
        ensure!(
            !applying
                || binding.service != id
                || entry.path().file_stem().is_some_and(|name| name == vm),
            "ENVIRONMENT_SERVICE_SHARED_USE_SEPARATE_PACKAGE"
        );
    }
    if host.services().join("vms").join(format!("{vm}.json")).exists() {
        ensure!(service::load(host, vm)?.definition.id == id, "VM_SERVICE_BINDING_CONFLICT");
    }
    Ok(())
}
fn execute(host: &Host, action: &str, path: &Path) -> Result<()> {
    let (document, mut value) = compile(path)?;
    validate(host, &document, &value)?;
    if action == "check" {
        return Ok(());
    }
    exclusive(host, &document.vm.id, &value.definition.id, action == "apply")?;
    if action == "apply" {
        value.definition.user_environment.as_mut().unwrap().apply_token =
            Some(uuid::Uuid::new_v4().to_string());
        if host.services().join("vms").join(format!("{}.json", document.vm.id)).exists() {
            service::dispatch(host, &["stop".into(), document.vm.id.clone()])?;
        }
    } else if let Ok(previous) = package::read(host, &value.definition.id) {
        if let (Some(before), Some(next)) =
            (&previous.definition.user_environment, &mut value.definition.user_environment)
        {
            if before.git == next.git {
                next.apply_token = before.apply_token.clone();
            }
        }
    }
    package::install_value(host, value.clone())?;
    activate(host, &document.vm.id, &value.definition.id)
}
fn activate(host: &Host, vm: &str, id: &str) -> Result<()> {
    if host.services().join("vms").join(format!("{vm}.json")).exists() {
        common::systemctl(&["enable", &service::unit(vm)?], true)?;
        return service::dispatch(host, &["start".into(), vm.into()]);
    }
    service::dispatch(host, &["up".into(), vm.into(), id.into(), "--enable".into()])
}
pub fn dispatch(host: &Host, args: &[String]) -> Result<()> {
    let [action, path] = args else { anyhow::bail!("usage: environment check|up|apply FILE.yaml") };
    ensure!(["check", "up", "apply"].contains(&action.as_str()), "ENVIRONMENT_UNKNOWN_ACTION");
    service::initialize(host)?;
    let started = Instant::now();
    let result = execute(host, action, Path::new(path));
    eprintln!(
        "{}",
        serde_json::json!({"operation":"user_environment",
        "operation_id":uuid::Uuid::new_v4().to_string(),"stage":action,
        "result":if result.is_ok(){"success"}else{"failure"},
        "elapsed_ms":started.elapsed().as_millis(),
        "category":result.as_ref().err().map(|error| error.to_string())})
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn fixture(root: &Path) -> PathBuf {
        std::fs::write(root.join("service.toml"), "id='example'\nprofile='services'\ncommand=['/bin/true']\nworking_directory='/home/agent'\nready_port=8080\n").unwrap();
        std::fs::write(root.join("key"), "-----BEGIN OPENSSH PRIVATE KEY-----\nfixture-secret\n")
            .unwrap();
        std::fs::set_permissions(root.join("key"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(
            root.join("known_hosts"),
            "other.test ssh-ed25519 ignored\ngithub.com ssh-ed25519 fixture\n",
        )
        .unwrap();
        let path = root.join("environment.yaml");
        std::fs::write(&path, "version: 1\nvm: {id: example, profile: services}\nrootfs: {persistent: true, user: agent}\nservice: {manifest: service.toml, identity_file: key}\ngit:\n  user: {name: Example, email: example@test.invalid}\n  lfs: true\n  github: {private_key_file: key, known_hosts_file: known_hosts, https_to_ssh: true}\n").unwrap();
        path
    }
    #[test]
    fn yaml_resolves_relative_paths_and_snapshots_only_selected_host() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path());
        let (_, value) = compile(&path).unwrap();
        let environment = value.definition.user_environment.unwrap();
        assert!(environment.git.github.private_key.contains("fixture-secret"));
        assert!(!environment.git.github.known_hosts.contains("other.test"));
        assert!(!format!("{environment:?}").contains("fixture-secret"));
    }
    #[test]
    fn yaml_resource_requests_override_manifest_and_survive_snapshot_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path());
        let manifest = root.path().join("service.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(&manifest, text + "[resources]\nvcpus=1\nmemory_mib=2048\n").unwrap();
        let content = std::fs::read_to_string(&path)
            .unwrap()
            .replace("profile: services}", "profile: services, resources: {memory_mib: 4096}}");
        std::fs::write(&path, content).unwrap();
        let (_, package) = compile(&path).unwrap();
        let restored: Package =
            serde_json::from_slice(&serde_json::to_vec(&package).unwrap()).unwrap();
        assert_eq!(restored.definition.resources.memory_mib, Some(4096));
        assert_eq!(restored.definition.resources.vcpus, None);
        fixture(root.path());
        let (_, package) = compile(&path).unwrap();
        assert!(package.definition.resources.is_empty());
    }
    #[test]
    fn unknown_fields_and_malformed_yaml_never_echo_input() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path());
        std::fs::write(&path, "version: 1\nsecret: [DO_NOT_ECHO_SECRET\n").unwrap();
        let error = compile(&path).err().unwrap().to_string();
        assert!(error.starts_with("ENVIRONMENT_YAML_INVALID"));
        assert!(!error.contains("DO_NOT_ECHO_SECRET"));
        fixture(root.path());
        let content = std::fs::read_to_string(&path).unwrap() + "unknown: DO_NOT_ECHO_SECRET\n";
        std::fs::write(&path, content).unwrap();
        assert!(!compile(&path).err().unwrap().to_string().contains("DO_NOT_ECHO_SECRET"));
    }
    #[test]
    fn permissive_private_key_and_unsupported_user_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path());
        std::fs::set_permissions(root.path().join("key"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert_eq!(compile(&path).err().unwrap().to_string(), "ENVIRONMENT_KEY_PERMISSIONS");
        fixture(root.path());
        let content = std::fs::read_to_string(&path).unwrap().replace("user: agent", "user: root");
        std::fs::write(&path, content).unwrap();
        assert_eq!(compile(&path).err().unwrap().to_string(), "ENVIRONMENT_USER_UNSUPPORTED");
    }
}
