use crate::host::package::{self, Service};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME_REQUIRED")?))
}

pub fn directory(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "UNTRUSTED_GUEST_DIRECTORY");
    for ancestor in path.ancestors() {
        if let Ok(meta) = std::fs::symlink_metadata(ancestor) {
            ensure!(!meta.file_type().is_symlink(), "UNTRUSTED_GUEST_DIRECTORY");
        }
    }
    if !path.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    }
    ensure!(
        path.canonicalize()? == path && path.metadata()?.uid() == unsafe { libc::geteuid() },
        "UNTRUSTED_GUEST_DIRECTORY"
    );
    Ok(())
}

pub fn root(service: &Service) -> Result<PathBuf> {
    let path = home()?.join(".local/share/firecracker-services").join(&service.id);
    directory(&path)?;
    Ok(path)
}

pub fn lock(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("run.lock"))?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "SERVICE_BUSY"
    );
    Ok(file)
}

pub fn atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let temporary = path.with_file_name(format!(".guest-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(data)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

pub fn regular_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && meta.uid() == unsafe { libc::geteuid() },
                "UNTRUSTED_GUEST_FILE"
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn deploy(path: &Path, bytes: &[u8], mode: u32, overwrite: bool) -> Result<bool> {
    if regular_exists(path)? && !overwrite {
        return Ok(false);
    }
    if overwrite {
        atomic(path, bytes, mode)?;
        return Ok(true);
    }
    let temporary = path.with_file_name(format!(".asset-{}", uuid::Uuid::new_v4()));
    atomic(&temporary, bytes, mode)?;
    let result = std::fs::hard_link(&temporary, path);
    let _ = std::fs::remove_file(temporary);
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            regular_exists(path)?;
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

pub fn prepare(service: &Service, root: &Path) -> Result<(usize, usize)> {
    ensure!(service.files.len() <= 64, "INVALID_PAYLOAD");
    let mut size = 0;
    let (mut written, mut preserved) = (0, 0);
    for file in &service.files {
        package::relative(&file.target)?;
        ensure!([0o600, 0o700].contains(&file.mode), "INVALID_PAYLOAD");
        let target = root.join(&file.target);
        directory(target.parent().unwrap())?;
        let bytes = STANDARD.decode(&file.content)?;
        size += bytes.len();
        ensure!(size <= 32 * 1024 * 1024, "INVALID_PAYLOAD");
        if deploy(&target, &bytes, file.mode, file.overwrite)? {
            written += 1;
        } else {
            preserved += 1;
        }
    }
    shell_proxy(service)?;
    Ok((written, preserved))
}

fn shell_proxy(service: &Service) -> Result<()> {
    let Some(values) = &service.proxy_environment else {
        return Ok(());
    };
    ensure!(
        values.keys().all(|key| crate::host::proxy::is_proxy_key(key)
            && key.bytes().all(|v| v.is_ascii_alphanumeric() || v == b'_')),
        "INVALID_PROXY_ENVIRONMENT"
    );
    let directory_path = home()?.join(".config/firecracker");
    directory(&directory_path)?;
    let data = values
        .iter()
        .map(|(key, value)| format!("export {key}={}\n", crate::host::ssh::quote(value)))
        .collect::<String>();
    atomic(&directory_path.join("proxy.env"), data.as_bytes(), 0o600)?;
    const BLOCK:&str="\n# firecracker-managed-proxy\n[ ! -r \"$HOME/.config/firecracker/proxy.env\" ] || . \"$HOME/.config/firecracker/proxy.env\"\n";
    for name in [".profile", ".bashrc"] {
        let path = home()?.join(name);
        let mut content = String::new();
        let mut mode = 0o644;
        match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path) {
            Ok(mut file) => {
                use std::io::Read;
                mode = file.metadata()?.mode() & 0o777;
                file.read_to_string(&mut content)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if !content.contains("# firecracker-managed-proxy\n") {
            content.push_str(BLOCK);
            atomic(&path, content.as_bytes(), mode)?;
        }
    }
    Ok(())
}

pub fn log(root: &Path, data: &[u8]) -> Result<()> {
    let path = root.join("service.log");
    if path.metadata().is_ok_and(|m| m.len() + data.len() as u64 > 5 * 1024 * 1024) {
        std::fs::rename(&path, root.join("service.log.1"))?;
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .write_all(data)?;
    Ok(())
}
