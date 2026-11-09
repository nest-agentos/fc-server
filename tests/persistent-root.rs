use firecracker_server::{
    config::{Config, RootfsMode},
    manager::CreateVm,
    rootfs,
};
use std::{
    os::unix::fs::MetadataExt,
    path::Path,
    time::{Duration, Instant},
};

fn configuration(path: &Path) -> Config {
    let text = format!(
        r#"
listen = "127.0.0.1:8790"
api_key_env = "TEST_KEY"
jailer = "/jailer"
firecracker = "/firecracker"
state_dir = "{root}"
uid_base = 200000
max_vms = 1
[profiles.test]
kernel = "{root}/kernel"
rootfs = "{root}/base"
rootfs_persistent = true
upper_size_mib = 64
vcpus = 1
memory_mib = 128
max_lifetime_secs = 0
startup_timeout_secs = 30
max_connections = 1
ports = [7000]
"#,
        root = path.display()
    );
    std::fs::write(path.join("kernel"), b"original-kernel").unwrap();
    std::fs::write(path.join("base"), b"original-base").unwrap();
    let config: Config = toml::from_str(&text).unwrap();
    config.validate().unwrap();
    config
}

fn spec() -> CreateVm {
    CreateVm {
        id: "retained".into(),
        profile: "test".into(),
        resources: Default::default(),
        directories: vec![],
        volumes: vec![],
        proxy: None,
    }
}

async fn prepare(config: &Config, target: &Path) -> Result<(), firecracker_server::error::Error> {
    std::fs::create_dir(target).unwrap();
    rootfs::prepare(
        config,
        &spec(),
        &config.profiles["test"],
        target,
        unsafe { libc::geteuid() },
        Instant::now() + Duration::from_secs(30),
    )
    .await
}

#[tokio::test]
async fn private_root_survives_recreation_and_rejects_policy_changes() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = configuration(temp.path());
    let first = temp.path().join("first");
    prepare(&config, &first).await.unwrap();
    std::fs::write(first.join("rootfs.ext4"), b"user-home-and-system-state").unwrap();
    std::fs::remove_dir_all(&first).unwrap();
    std::fs::write(temp.path().join("base"), b"new-base").unwrap();
    let second = temp.path().join("second");
    prepare(&config, &second).await.unwrap();
    assert_eq!(std::fs::read(second.join("rootfs.ext4")).unwrap(), b"user-home-and-system-state");
    assert_eq!(std::fs::metadata(second.join("rootfs.ext4")).unwrap().nlink(), 2);
    config.profiles.get_mut("test").unwrap().upper_size_mib = 128;
    let error = prepare(&config, &temp.path().join("conflict")).await.unwrap_err();
    assert_eq!(error.code, "PERSISTENT_ROOT_CONFIGURATION_CONFLICT");
    config.profiles.get_mut("test").unwrap().rootfs_persistent = false;
    let error = prepare(&config, &temp.path().join("ephemeral")).await.unwrap_err();
    assert_eq!(error.code, "PERSISTENT_ROOT_REQUIRES_PERSISTENT_PROFILE");
}

#[tokio::test]
async fn shared_root_pins_lower_and_boot_assets_until_explicit_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = configuration(temp.path());
    std::fs::write(temp.path().join("initrd"), b"original-initrd").unwrap();
    let profile = config.profiles.get_mut("test").unwrap();
    profile.rootfs_mode = RootfsMode::Shared;
    profile.initrd = Some(temp.path().join("initrd"));
    let first = temp.path().join("first");
    prepare(&config, &first).await.unwrap();
    let lower_inode = std::fs::metadata(first.join("base.ext4")).unwrap().ino();
    let upper_inode = std::fs::metadata(first.join("rootfs.ext4")).unwrap().ino();
    std::fs::remove_dir_all(&first).unwrap();
    std::fs::write(temp.path().join("new-base"), b"new-base").unwrap();
    std::fs::rename(temp.path().join("new-base"), temp.path().join("base")).unwrap();
    std::fs::write(temp.path().join("kernel"), b"new-kernel").unwrap();
    std::fs::write(temp.path().join("initrd"), b"new-initrd").unwrap();
    let second = temp.path().join("second");
    prepare(&config, &second).await.unwrap();
    assert_eq!(std::fs::metadata(second.join("base.ext4")).unwrap().ino(), lower_inode);
    assert_eq!(std::fs::metadata(second.join("rootfs.ext4")).unwrap().ino(), upper_inode);
    assert_eq!(std::fs::read(second.join("vmlinux")).unwrap(), b"original-kernel");
    assert_eq!(std::fs::read(second.join("initrd")).unwrap(), b"original-initrd");
    std::fs::remove_dir_all(second).unwrap();
    rootfs::delete(&config, "retained").await.unwrap();
    assert!(!rootfs::directory(&config, "retained").exists());
}

#[tokio::test]
async fn compute_resize_reuses_existing_root_and_legacy_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let config = configuration(temp.path());
    let first = temp.path().join("first");
    prepare(&config, &first).await.unwrap();
    let inode = std::fs::metadata(first.join("rootfs.ext4")).unwrap().ino();
    let mut resized = spec();
    resized.resources.memory_mib = Some(4096);
    let mut profile = config.profiles["test"].clone();
    profile.max_memory_mib = Some(4096);
    let profile = resized.resources.resolve(&profile).unwrap();
    let second = temp.path().join("resized");
    std::fs::create_dir(&second).unwrap();
    rootfs::prepare(
        &config,
        &resized,
        &profile,
        &second,
        unsafe { libc::geteuid() },
        Instant::now() + Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::metadata(second.join("rootfs.ext4")).unwrap().ino(), inode);
}
