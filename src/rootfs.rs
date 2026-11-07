use crate::{
    config::{Config, Profile, RootfsMode},
    error::Error,
    launcher,
    manager::{conflict, CreateVm},
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

pub fn directory(config: &Config, id: &str) -> PathBuf {
    config.state_dir.join("roots").join(id)
}

fn manifest(config: &Config, spec: &CreateVm, profile: &Profile) -> Value {
    // Network policy changes must not invalidate an existing writable root.
    let mut storage_spec = spec.clone();
    storage_spec.proxy = None;
    let bindings: Vec<_> = profile.bindings.iter().map(|name| {
        let b = &config.bindings[name];
        json!({"id":name,"source":b.source,"target":b.target,"readOnly":b.read_only,"backend":b.backend})
    }).collect();
    json!({"version":1,"spec":storage_spec,"bindings":bindings,"source":profile.rootfs,
        "mode":if profile.rootfs_mode == RootfsMode::Shared {"shared"} else {"private"},
        "upperSizeMiB":profile.upper_size_mib})
}

pub async fn prepare(
    config: &Config,
    spec: &CreateVm,
    profile: &Profile,
    root: &Path,
    uid: u32,
    deadline: Instant,
) -> Result<(), Error> {
    if !profile.rootfs_persistent {
        if directory(config, &spec.id).exists() {
            return Err(conflict("PERSISTENT_ROOT_REQUIRES_PERSISTENT_PROFILE"));
        }
        launcher::copy(&profile.kernel, &root.join("vmlinux"), deadline).await?;
        launcher::readonly(&root.join("vmlinux"))?;
        return launcher::prepare_root(profile, root, uid, deadline).await;
    }
    let started = Instant::now();
    let disk = directory(config, &spec.id);
    let expected = manifest(config, spec, profile);
    if !disk.exists() {
        initialize(config, spec, profile, uid, deadline, &expected).await?;
    }
    validate(&disk, &expected).await?;
    attach(&disk, root, profile, uid).await?;
    tracing::info!(stage="persistent_root_attached", vm_id=%spec.id, elapsed_ms=started.elapsed().as_millis() as u64, result="success");
    Ok(())
}

async fn initialize(
    config: &Config,
    spec: &CreateVm,
    profile: &Profile,
    uid: u32,
    deadline: Instant,
    manifest: &Value,
) -> Result<(), Error> {
    let parent = config.state_dir.join("roots");
    tokio::fs::create_dir_all(&parent).await.map_err(|e| Error::io("root_store_create", e))?;
    let temporary = parent.join(format!("{}.initializing", spec.id));
    tokio::fs::create_dir(&temporary).await.map_err(|e| Error::io("root_store_reserve", e))?;
    let result = populate(&temporary, profile, uid, deadline, manifest).await;
    if let Err(error) = result {
        tokio::fs::remove_dir_all(&temporary)
            .await
            .map_err(|e| Error::io("root_store_rollback", e))?;
        return Err(error);
    }
    tokio::fs::rename(&temporary, directory(config, &spec.id))
        .await
        .map_err(|e| Error::io("root_store_commit", e))?;
    sync(&parent)
}

async fn populate(
    path: &Path,
    profile: &Profile,
    uid: u32,
    deadline: Instant,
    manifest: &Value,
) -> Result<(), Error> {
    launcher::copy(&profile.kernel, &path.join("vmlinux"), deadline).await?;
    launcher::readonly(&path.join("vmlinux"))?;
    launcher::prepare_root(profile, path, uid, deadline).await?;
    tokio::fs::write(path.join("manifest.json"), serde_json::to_vec(manifest).unwrap())
        .await
        .map_err(|e| Error::io("root_manifest_write", e))?;
    for name in assets(profile).into_iter().chain(["manifest.json"]) {
        sync(&path.join(name))?;
    }
    sync(path)
}

fn sync(path: &Path) -> Result<(), Error> {
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| Error::io("root_store_sync", e))
}

async fn validate(path: &Path, expected: &Value) -> Result<(), Error> {
    let bytes = tokio::fs::read(path.join("manifest.json"))
        .await
        .map_err(|e| Error::io("root_manifest_read", e))?;
    let actual: Value =
        serde_json::from_slice(&bytes).map_err(|_| conflict("PERSISTENT_ROOT_MANIFEST_INVALID"))?;
    if actual != *expected {
        return Err(conflict("PERSISTENT_ROOT_CONFIGURATION_CONFLICT"));
    }
    Ok(())
}

fn assets(profile: &Profile) -> Vec<&'static str> {
    if profile.rootfs_mode == RootfsMode::Shared {
        vec!["vmlinux", "rootfs.ext4", "base.ext4", "initrd"]
    } else {
        vec!["vmlinux", "rootfs.ext4"]
    }
}

async fn attach(disk: &Path, root: &Path, profile: &Profile, uid: u32) -> Result<(), Error> {
    for name in assets(profile) {
        let source = disk.join(name);
        let metadata = tokio::fs::symlink_metadata(&source)
            .await
            .map_err(|e| Error::io("root_asset_metadata", e))?;
        if !metadata.is_file() {
            return Err(conflict("PERSISTENT_ROOT_ASSET_INVALID"));
        }
        tokio::fs::hard_link(source, root.join(name))
            .await
            .map_err(|e| Error::io("root_asset_link", e))?;
    }
    launcher::chown(&root.join("rootfs.ext4"), uid)
}

pub async fn delete(config: &Config, id: &str) -> Result<(), Error> {
    let path = directory(config, id);
    if path.exists() {
        tokio::fs::remove_dir_all(path).await.map_err(|e| Error::io("root_store_delete", e))?;
        sync(&config.state_dir.join("roots"))?;
    }
    Ok(())
}
