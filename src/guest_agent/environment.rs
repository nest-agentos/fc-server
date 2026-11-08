//! Initialize user configuration once, or once per explicit host apply token.
use super::files;
use crate::host::environment::UserEnvironment;
use anyhow::{ensure, Context, Result};
use std::{path::Path, process::Command};

fn git(home: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(home)
        .output()
        .context("ENVIRONMENT_GIT_UNAVAILABLE")?;
    ensure!(output.status.success(), "ENVIRONMENT_GIT_CONFIG_FAILED");
    Ok(())
}
fn configure(home: &Path, environment: &UserEnvironment, ssh: &Path) -> Result<()> {
    let author = &environment.git.user;
    git(home, &["config", "--global", "user.name", &author.name])?;
    git(home, &["config", "--global", "user.email", &author.email])?;
    let command = format!("/usr/bin/ssh -F {}", crate::host::ssh::quote(&ssh.to_string_lossy()));
    git(home, &["config", "--global", "core.sshCommand", &command])?;
    rewrite(home, environment.git.github.https_to_ssh)?;
    if environment.git.lfs {
        git(home, &["lfs", "install", "--skip-repo"])?;
    }
    Ok(())
}
fn rewrite(home: &Path, enabled: bool) -> Result<()> {
    let key = "url.git@github.com:.insteadOf";
    if enabled {
        return git(
            home,
            &[
                "config",
                "--global",
                "--fixed-value",
                "--replace-all",
                key,
                "https://github.com/",
                "https://github.com/",
            ],
        );
    }
    let output = Command::new("git")
        .args(["config", "--global", "--fixed-value", "--unset-all", key, "https://github.com/"])
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(home)
        .output()?;
    ensure!(
        output.status.success() || output.status.code() == Some(5),
        "ENVIRONMENT_GIT_CONFIG_FAILED"
    );
    Ok(())
}
fn write_files(root: &Path, environment: &UserEnvironment) -> Result<()> {
    let github = &environment.git.github;
    files::atomic(&root.join("private-key"), github.private_key.as_bytes(), 0o600)?;
    files::atomic(&root.join("known_hosts"), github.known_hosts.as_bytes(), 0o600)?;
    let text = format!("Host github.com\n    HostName github.com\n    User git\n    IdentityFile \"{}/private-key\"\n    UserKnownHostsFile \"{}/known_hosts\"\n    IdentitiesOnly yes\n    StrictHostKeyChecking yes\n    BatchMode yes\n    ConnectTimeout 10\n", root.display(), root.display());
    files::atomic(&root.join("ssh_config"), text.as_bytes(), 0o600)
}
fn tools(home: &Path, lfs: bool) -> Result<()> {
    let output = Command::new("/usr/bin/ssh")
        .arg("-V")
        .output()
        .context("ENVIRONMENT_SSH_CLIENT_MISSING_REBUILD_IMAGE")?;
    ensure!(output.status.success(), "ENVIRONMENT_SSH_CLIENT_INVALID");
    git(home, &["--version"])?;
    if lfs {
        git(home, &["lfs", "version"])?;
    }
    Ok(())
}
pub fn prepare(home: &Path, environment: &UserEnvironment) -> Result<&'static str> {
    let root = home.join(".config/firecracker/git");
    files::directory(&root)?;
    let _lock = files::lock(&root)?;
    let marker = root.join("initialized.json");
    let token = serde_json::to_vec(&environment.apply_token)?;
    if files::regular_exists(&marker)? {
        let previous = std::fs::read(&marker)?;
        if environment.apply_token.is_none() || previous == token {
            return Ok("preserved");
        }
    }
    tools(home, environment.git.lfs)?;
    files::regular_exists(&home.join(".gitconfig"))?;
    for name in ["private-key", "known_hosts", "ssh_config"] {
        files::regular_exists(&root.join(name))?;
    }
    write_files(&root, environment)?;
    configure(home, environment, &root.join("ssh_config"))?;
    files::atomic(&marker, &token, 0o600)?;
    Ok("configured")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::environment::{Author, Git, Github};
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn fixture() -> UserEnvironment {
        UserEnvironment {
            git: Git {
                user: Author { name: "Example Author".into(), email: "author@example.test".into() },
                lfs: false,
                github: Github {
                    private_key: "fixture-key".into(),
                    known_hosts: "fixture-host".into(),
                    https_to_ssh: true,
                },
            },
            apply_token: None,
        }
    }
    fn read(home: &Path, key: &str) -> String {
        let output = Command::new("git")
            .args(["config", "--file"])
            .arg(home.join(".gitconfig"))
            .arg(key)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    #[test]
    fn first_boot_preserves_edits_and_explicit_apply_runs_only_once() {
        let home = tempfile::tempdir().unwrap();
        let mut value = fixture();
        assert_eq!(prepare(home.path(), &value).unwrap(), "configured");
        git(home.path(), &["config", "--global", "user.name", "User edit"]).unwrap();
        value.git.user.name = "Updated host configuration".into();
        assert_eq!(prepare(home.path(), &value).unwrap(), "preserved");
        assert_eq!(read(home.path(), "user.name"), "User edit");
        value.apply_token = Some("explicit-apply-1".into());
        assert_eq!(prepare(home.path(), &value).unwrap(), "configured");
        assert_eq!(read(home.path(), "user.name"), "Updated host configuration");
        git(home.path(), &["config", "--global", "user.name", "Second edit"]).unwrap();
        assert_eq!(prepare(home.path(), &value).unwrap(), "preserved");
        assert_eq!(read(home.path(), "user.name"), "Second edit");
        let root = home.path().join(".config/firecracker/git");
        assert_eq!(
            root.join("private-key").metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(root.metadata().unwrap().permissions().mode() & 0o777, 0o700);
    }
    #[test]
    fn symlink_configuration_is_rejected_without_touching_target() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "do not modify").unwrap();
        symlink(outside.path(), home.path().join(".gitconfig")).unwrap();
        assert!(prepare(home.path(), &fixture()).is_err());
        assert_eq!(std::fs::read_to_string(outside.path()).unwrap(), "do not modify");
        assert!(!home.path().join(".config/firecracker/git/initialized.json").exists());
    }
}
