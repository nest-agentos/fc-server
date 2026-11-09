#!/usr/bin/env bash
# Manage VMs and build as the invoking user; privileged operations use sudo.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
stage=init
trap 'printf "[build] stage=%s result=failed line=%s exit=%s\n" "$stage" "$LINENO" "$?" >&2' ERR

usage() {
    cat <<'EOF'
Usage: ./vmm.sh [COMMAND] [ARGS...]
No command: show this help.

  build [--os]  Build release server/fork and virtiofsd; --os also builds/packages kernel/rootfs.
  deps     Install Debian/Ubuntu host dependencies and rustup if missing.
  install  Deploy built artifacts, initial config, key and systemd unit as root.
  install-server  Build/deploy only server and host helpers; preserve all VM images/config.
  setup [--os]  Run deps, build, install, check. First installation needs --os.
  network [--os]  Prepare SSH/NAT/DHCP/DNS, deploy, restart an idle server; --os updates guest OS.
  shared-root [--os]  Deploy shared-root mode; --os builds guest OS, otherwise reuse existing assets.
  ssh ID   Log into a running VM as agent, with the dedicated local SSH key.
  ip ID    Print the private IPv4 address of a running VM.
  list [--all] [--json]  List running VMs; --all includes stopped services/retained roots.
  publish ID HOST:GUEST[/tcp|udp] [--listen IPv4]  Publish a port via nftables (default: 127.0.0.1).
  unpublish ID HOST[/tcp|udp]  Remove a published port and its tracked NAT flows.
  ports ID  List a VM's published TCP/UDP ports.
  start ID [DIRECTORY...]  Create a debian13 VM, optionally with authorized directory bindings.
  stop ID  Stop a VM; persistent roots/volumes are retained, temporary root disks deleted.
  reboot ID  Restart a managed service VM, or recreate an ordinary VM and restore its ports.
  update ID  Update a managed service using its saved source and SSH identity.
  service <update|install|setup|start|up|stop|destroy|status> ...  Manage generic guest services.
  service update ID [FILE --identity KEY] [--enable]  Reuse saved source/key; replace declared files and retain sessions/root.
  environment <check|up|apply> FILE.yaml  Provision a user environment from one private YAML.
  check    Check installed paths, config, KVM, cgroup v2 and namespace support.
  smoke    Run real KVM block-volume and virtio-fs acceptance with disposable fixtures.
  package  Create zstd kernel/rootfs archives and refresh SHA256SUMS.

Profile ports authorize vsock connections; they do not publish TCP/UDP ports.

Environment:
  FIRECRACKER_SOURCE  Fork checkout (default: ../fc-hp).
  BUILD_DIR          Artifact/cache directory (default: ./build).
  JOBS               Compiler parallelism (default: 8).
  KERNEL_IMAGE       With --os, use an existing compatible vmlinux instead of compiling.
  KERNEL_MIRROR      Kernel archive directory URL (default: kernel.org v6.x).
  VIRTIOFSD          Use an existing Rust virtiofsd advertising statx in --print-capabilities.
  IMAGE_SIZE_MIB     New Debian root disk size (default: 2048).
  GUEST_FLAVOR       harness (pinned Codex/Claude Code/DSH) or base (no Node/tools).
  NPM_REGISTRY       Registry used in the build stage; npm is absent at runtime.
  NODE_IMAGE         Pinned Node build image (default: node:26.13.0-trixie-slim).
  SSH_PUBLIC_KEY     Guest authorized public key (auto: ~/.ssh/mindos-firecracker.pub).
  FC_EGRESS_INTERFACE, FC_DNS_SERVERS  Optional NAT interface/public DNS overrides.
  REBUILD_IMAGE=1    With --os, force rebuilding the Debian image.
  DEBIAN_MIRROR, DEBIAN_SECURITY_MIRROR  Optional guest APT mirrors.
EOF
}

log() { printf '[build] stage=%s %s\n' "$stage" "$*" >&2; }
die() { log "result=failed reason=$*"; exit 1; }
need() { command -v "$1" >/dev/null || die "missing $1; run ./vmm.sh deps"; }
as_root() {
    if (( EUID == 0 )); then "$@"; else sudo -- "$@"; fi
}

host_cli() {
    stage="runtime-${1:-command}"
    local server=${FIRECRACKER_SERVER:-/usr/local/bin/firecracker-server}
    [[ -x $server ]] || die 'install the Rust runtime first with ./vmm.sh setup --os'
    as_root "$server" "$@"
}

update_service() {
    stage=service-update
    as_root python3 "$root/scripts/update-service.py" "${FIRECRACKER_SERVER:-/usr/local/bin/firecracker-server}" "$@"
}

init() {
    [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || die 'only Linux x86_64 is validated'
    build_dir=$(realpath -m -- "${BUILD_DIR:-$root/build}")
    fc_source=$(realpath -m -- "${FIRECRACKER_SOURCE:-$root/../fc-hp}")
    server_target=${CARGO_TARGET_DIR:-$root/target}
    [[ $server_target == /* ]] || server_target=$root/$server_target
    server_target=$(realpath -m -- "$server_target")
    jobs=${JOBS:-8}
    [[ $jobs =~ ^[1-9][0-9]*$ ]] || die 'JOBS must be a positive integer'
    local image_size=${IMAGE_SIZE_MIB:-2048}
    [[ $image_size =~ ^[0-9]+$ && $image_size -ge 1024 && $image_size -le 65536 ]] || \
        die 'IMAGE_SIZE_MIB must be 1024..65536'
    [[ ${REBUILD_IMAGE:-0} == 0 || ${REBUILD_IMAGE:-0} == 1 ]] || die 'REBUILD_IMAGE must be 0 or 1'
    [[ ${GUEST_FLAVOR:-harness} == harness || ${GUEST_FLAVOR:-harness} == base ]] || die 'GUEST_FLAVOR must be harness or base'
    export CARGO_BUILD_JOBS=$jobs
}

dependencies() {
    stage=dependencies
    need apt-get
    as_root apt-get update
    local packages=(build-essential git patch curl ca-certificates e2fsprogs bubblewrap
        musl-tools pkg-config libssl-dev libelf-dev libseccomp-dev libcap-ng-dev
        clang llvm flex bison bc cpio xz-utils zstd python3 sudo openssl openssh-client util-linux systemd)
    command -v docker >/dev/null || packages+=(docker.io)
    as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${packages[@]}"
    if ! command -v rustup >/dev/null; then
        local installer
        installer=$(mktemp)
        curl --fail --location --retry 3 https://sh.rustup.rs -o "$installer"
        sh "$installer" -y --profile minimal --default-toolchain stable
        rm -f -- "$installer"
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    if ! docker info >/dev/null 2>&1; then
        as_root systemctl start docker
    fi
    log 'result=success; Docker uses sudo when the invoking user cannot access its socket'
}

build_fork() {
    stage=firecracker
    [[ -f $fc_source/Cargo.toml ]] || die "fork not found: $fc_source"
    [[ -f $fc_source/src/vmm/src/devices/virtio/fs.rs ]] || die 'checkout lacks the virtio-fs device'
    local toolchain
    toolchain=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$fc_source/rust-toolchain.toml")
    [[ -n $toolchain ]] || die 'fork rust-toolchain.toml has no channel'
    rustup toolchain install "$toolchain" --profile minimal
    rustup target add --toolchain "$toolchain" x86_64-unknown-linux-musl
    (
        cd -- "$fc_source"
        export CFLAGS_x86_64_unknown_linux_musl="${CFLAGS_x86_64_unknown_linux_musl:--idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu}"
        CARGO_TARGET_DIR="$fc_source/build/cargo_target" \
            cargo build --locked --release --target x86_64-unknown-linux-musl -p firecracker -p jailer
    )
    local binary
    for binary in firecracker jailer; do
        install -m 0755 "$fc_source/build/cargo_target/x86_64-unknown-linux-musl/release/$binary" "$build_dir/bin/$binary"
    done
}

build_daemon() {
    stage=virtiofsd
    local daemon=${VIRTIOFSD:-} help capabilities
    if [[ -z $daemon ]]; then
        need patch
        "$root/scripts/build-virtiofsd.sh" "$build_dir" "$jobs"
        daemon=$build_dir/virtiofsd/bin/virtiofsd
    fi
    [[ -x $daemon ]] || die "daemon is not executable: $daemon"
    help=$("$daemon" --help)
    [[ $help == *--readonly* && $help == *--sandbox* ]] || die 'Rust virtiofsd with --readonly and --sandbox required'
    capabilities=$("$daemon" --print-capabilities)
    [[ $capabilities == *'"statx"'* ]] || die 'virtiofsd lacks FUSE_STATX; unset VIRTIOFSD to build the patched daemon'
    install -m 0755 "$daemon" "$build_dir/bin/virtiofsd"
    "$build_dir/bin/virtiofsd" --version
}

kernel_source() {
    # Fixed to the kernel used in VALIDATION.md; digest from kernel.org sha256sums.asc.
    local version=6.18.54
    local digest=9df30b02dd8102bbd0be52556288ef6889ddbe7f1ddb96fbf847d0becf3eacac
    linux_source=$build_dir/cache/linux-$version
    local archive=$build_dir/cache/linux-$version.tar.xz
    mkdir -p "$build_dir/cache"
    if [[ ! -f $archive ]]; then
        curl --fail --location --retry 3 --connect-timeout 30 \
            "${KERNEL_MIRROR:-https://www.kernel.org/pub/linux/kernel/v6.x}/linux-$version.tar.xz" -o "$archive.part"
        mv -- "$archive.part" "$archive"
    fi
    printf '%s  %s\n' "$digest" "$archive" | sha256sum --check --status || \
        die "kernel checksum mismatch; remove $archive and retry"
    if [[ ! -d $linux_source ]]; then
        local unpack
        unpack=$(mktemp -d "$build_dir/cache/unpack.XXXXXXXX")
        tar -xJf "$archive" -C "$unpack"
        mv -- "$unpack/linux-$version" "$linux_source"
        rmdir -- "$unpack"
    fi
}

build_kernel() {
    stage=kernel
    if [[ -n ${KERNEL_IMAGE:-} ]]; then
        install -m 0644 "$KERNEL_IMAGE" "$build_dir/images/vmlinux"
        log "using supplied kernel: $KERNEL_IMAGE (must have built-in virtio-fs/vsock/ext4/i8042)"
        return
    fi
    kernel_source
    local output=$build_dir/kernel
    mkdir -p "$output"
    cp -- "$fc_source/resources/guest_configs/microvm-kernel-ci-x86_64-6.18.config" "$output/.config"
    "$linux_source/scripts/config" --file "$output/.config" \
        -e FUSE_FS -e VIRTIO_FS -e VIRTIO_MMIO -e VIRTIO_MMIO_CMDLINE_DEVICES \
        -e VIRTIO_BLK -e VSOCKETS -e VIRTIO_VSOCKETS -e EXT4_FS \
        -e DEVTMPFS -e DEVTMPFS_MOUNT -e CGROUPS -e MEMCG -e CGROUP_PIDS \
        -e NAMESPACES -e INPUT -e INPUT_KEYBOARD -e KEYBOARD_ATKBD \
        -e SERIO -e SERIO_I8042 -e SERIO_LIBPS2 -d FUSE_DAX \
        -e BLK_DEV_INITRD -e RD_GZIP -e OVERLAY_FS -e VIRTIO_PMEM -e FS_DAX
    make -C "$linux_source" O="$output" olddefconfig
    make -C "$linux_source" O="$output" -j"$jobs" vmlinux
    install -m 0644 "$output/vmlinux" "$build_dir/images/vmlinux"
    install -m 0644 "$output/.config" "$build_dir/images/kernel.config"
}

build_image() {
    stage=debian-image
    local image=$build_dir/images/debian13.ext4
    local inputs
    inputs=$(python3 "$root/scripts/image-info.py" inputs "$root" "${IMAGE_SIZE_MIB:-2048}" "${GUEST_FLAVOR:-harness}")
    if [[ -f $image && -f $image.initrd && -f $image.inputs.json && ${REBUILD_IMAGE:-0} != 1 ]] && \
        cmp -s <(printf '%s\n' "$inputs") "$image.inputs.json"; then
        log "reuse $image; recipe/options match"
        return
    fi
    local pending
    pending=$(mktemp -d "$build_dir/images/pending.XXXXXXXX")
    bash "$root/scripts/build-debian13.sh" "$pending/debian13.ext4" "${IMAGE_SIZE_MIB:-2048}"
    local suffix
    for suffix in '' .json .inputs.json .initrd; do
        mv -f -- "$pending/debian13.ext4$suffix" "$image$suffix"
    done
    rmdir -- "$pending"
}

validate_build_options() {
    [[ $# == 0 || ( $# == 1 && $1 == --os ) ]] || die 'expected no build options or --os'
}

require_os_assets() {
    [[ ${1:-} != --os ]] || return 0
    local name
    for name in vmlinux debian13.ext4 debian13.ext4.initrd; do
        [[ -f $build_dir/images/$name ]] || \
            die "missing OS artifact $name; run ./vmm.sh build --os or add --os to this command"
    done
}

build_preflight() {
    stage=preflight
    (( EUID != 0 )) || die 'run build/setup as your regular user; sudo is used for privileged steps'
    export PATH="$HOME/.cargo/bin:$PATH"
    local tool
    for tool in cargo rustup git musl-gcc python3; do need "$tool"; done
    if [[ ${1:-} == --os ]]; then
        for tool in curl make zstd; do need "$tool"; done
    fi
    python3 -c 'import tomllib' || die 'Python 3.11+ is required'
}

compile_server() {
    local output=${1:-$build_dir}
    stage=server
    (cd -- "$root"; CARGO_TARGET_DIR="$server_target" cargo build --locked --release --bin firecracker-server)
    install -m 0755 "$server_target/release/firecracker-server" "$output/bin/firecracker-server"
    compile_guest "$output"
}

compile_guest() {
    stage=guest-agent
    rustup target add x86_64-unknown-linux-musl
    (cd -- "$root"; CARGO_TARGET_DIR="$server_target" cargo build --locked --release --target x86_64-unknown-linux-musl --bin firecracker-guest)
    install -m 0755 "$server_target/x86_64-unknown-linux-musl/release/firecracker-guest" "$1/bin/firecracker-guest"
}

build_all() {
    validate_build_options "$@"
    build_preflight "$@"
    mkdir -p "$build_dir/bin" "$build_dir/images"
    # Serialize build and install so a deployment cannot read half-written artifacts.
    exec 9>"$build_dir/.lock"
    flock -n 9 || die 'another build/install is using BUILD_DIR'
    build_fork
    build_daemon
    if [[ ${1:-} == --os ]]; then
        build_kernel
        build_image
    else
        stage=os
        log 'result=skipped reason=--os-not-requested; existing kernel/rootfs left unchanged'
    fi
    compile_server
    if [[ ${1:-} == --os ]]; then package_images; else write_manifest; fi
    log "result=success artifacts=$build_dir; deploy with ./vmm.sh install"
    exec 9>&-
}

package_images() {
    stage=package
    need zstd
    local name
    for name in vmlinux debian13.ext4; do
        [[ -f $build_dir/images/$name ]] || die "missing image: $name"
        zstd -T"$jobs" -3 -f "$build_dir/images/$name" -o "$build_dir/images/$name.zst.part"
        zstd -t "$build_dir/images/$name.zst.part"
        mv -f -- "$build_dir/images/$name.zst.part" "$build_dir/images/$name.zst"
    done
    write_manifest
}

write_manifest() {
    stage=manifest
    (
        cd -- "$build_dir"
        shopt -s nullglob
        sha256sum bin/* > SHA256SUMS.part
        for file in images/vmlinux images/debian13.ext4 images/*.initrd images/*.zst images/*.json; do
            [[ ! -f $file ]] || sha256sum "$file" >> SHA256SUMS.part
        done
        mv -f SHA256SUMS.part SHA256SUMS
    )
}

package_only() {
    [[ -d $build_dir/images ]] || die 'build images first'
    exec 9>"$build_dir/.lock"
    flock -n 9 || die 'another build/install is using BUILD_DIR'
    package_images
    exec 9>&-
}

deploy() {
    stage=install
    require_os_assets
    [[ -f $build_dir/SHA256SUMS ]] || die 'build artifacts first with ./vmm.sh build'
    exec 9>"$build_dir/.lock"
    flock -n 9 || die 'another build/install is using BUILD_DIR'
    (cd -- "$build_dir"; sha256sum --check SHA256SUMS)
    as_root python3 "$root/scripts/install-environment.py" "$build_dir" "$root"
    exec 9>&-
    log 'result=success; start with sudo systemctl enable --now firecracker-server'
}

deploy_server() {
    stage=server
    (( EUID != 0 )) || die 'build/install-server as your regular user; sudo is used for deployment'
    need cargo
    local server_build="$build_dir/server-only"
    mkdir -p "$server_build/bin"
    exec 9>"$build_dir/.lock"
    flock -n 9 || die 'another build/install is using BUILD_DIR'
    compile_server "$server_build"
    (cd -- "$server_build"; sha256sum bin/* > SHA256SUMS)
    as_root python3 "$root/scripts/install-environment.py" "$server_build" "$root" --server-only
    exec 9>&-
    log 'result=success; images/config preserved; start firecracker-server explicitly'
}

check_host() {
    stage=check
    as_root python3 "$root/scripts/check-environment.py"
}

network_setup() {
    stage=network-preflight
    host_cli network idle
    need ssh-keygen
    local identity=${SSH_IDENTITY:-$HOME/.ssh/mindos-firecracker}
    if [[ ! -f $identity ]]; then
        (umask 077; mkdir -p -- "$(dirname -- "$identity")")
        ssh-keygen -q -t ed25519 -N '' -f "$identity"
    fi
    export SSH_PUBLIC_KEY=${SSH_PUBLIC_KEY:-$identity.pub}
    build_all "$@"
    stage=network-install
    as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends dnsmasq-base nftables iproute2 conntrack
    host_cli network idle
    as_root systemctl stop firecracker-server
    deploy
    as_root python3 "$root/scripts/install-network.py" install
    as_root systemctl enable --now mindos-firecracker-network.service
    as_root systemctl restart mindos-firecracker-network.service
    as_root systemctl enable --now firecracker-server
    log 'result=success; create a VM, then ./vmm.sh ssh VM_ID'
}

shared_root_setup() {
    stage=shared-root-preflight
    host_cli network idle
    build_all "$@"
    host_cli network idle
    stage=shared-root-install
    as_root systemctl stop firecracker-server
    if ! (deploy && as_root python3 "$root/scripts/shared-root-environment.py" debian13); then
        as_root systemctl start firecracker-server
        return 1
    fi
    as_root systemctl enable --now firecracker-server
    log 'result=success profile=debian13 rootfs_mode=shared'
}

ssh_vm() {
    local vm=${1:?usage: ./vmm.sh ssh VM_ID [COMMAND...]}
    shift
    local address alias
    read -r address alias < <(host_cli network address "$vm")
    ssh -F /dev/null -i "${SSH_IDENTITY:-$HOME/.ssh/mindos-firecracker}" \
        -o "UserKnownHostsFile=$HOME/.ssh/mindos-firecracker-known-hosts" \
        -o "HostKeyAlias=$alias" -o StrictHostKeyChecking=accept-new \
        -o ConnectTimeout=3 -o ConnectionAttempts=20 -o IdentitiesOnly=yes -o ForwardAgent=no \
        "agent@$address" "$@"
}

smoke() {
    stage=smoke
    need python3
    need sudo
    require_os_assets
    sudo -n true || die 'smoke tests need noninteractive sudo (run sudo -v first)'
    if systemctl is-active --quiet firecracker-server 2>/dev/null; then
        die 'isolated smoke needs the deployed server stopped; use scripts/smoke-network.py for online network checks'
    fi
    [[ -f $build_dir/SHA256SUMS ]] || die 'build artifacts first with ./vmm.sh build'
    exec 9>"$build_dir/.lock"
    flock -sn 9 || die 'another build/install is using BUILD_DIR'
    PATH="$build_dir/bin:$PATH" python3 "$root/scripts/smoke.py" "$build_dir/bin/firecracker-server" \
        "$build_dir/images/vmlinux" "$build_dir/images/debian13.ext4"
    python3 "$root/scripts/smoke-virtiofs.py" "$build_dir/bin/firecracker-server" \
        "$build_dir/images/vmlinux" "$build_dir/images/debian13.ext4" \
        "$build_dir/bin/firecracker" "$build_dir/bin/jailer" "$build_dir/bin/virtiofsd"
    python3 "$root/scripts/smoke-shared-root.py" "$build_dir/bin/firecracker-server" \
        "$build_dir/images/vmlinux" "$build_dir/images/debian13.ext4" "$build_dir/images/debian13.ext4.initrd" \
        "$build_dir/bin/firecracker" "$build_dir/bin/jailer" "$build_dir/bin/virtiofsd"
    exec 9>&-
}

main() {
case ${1:-help} in
    help|-h|--help) usage ;;
    build) validate_build_options "${@:2}"; init; build_all "${@:2}" ;;
    deps) init; dependencies ;;
    install) init; deploy ;;
    install-server) init; deploy_server ;;
    setup) validate_build_options "${@:2}"; init; require_os_assets "${@:2}"; dependencies; build_all "${@:2}"; deploy; check_host ;;
    network) validate_build_options "${@:2}"; init; require_os_assets "${@:2}"; network_setup "${@:2}" ;;
    shared-root) validate_build_options "${@:2}"; init; require_os_assets "${@:2}"; shared_root_setup "${@:2}" ;;
    ssh) init; ssh_vm "${@:2}" ;;
    update) init; update_service "${@:2}" ;;
    service)
        init
        if [[ ${2:-} == update ]]; then
            update_service "${@:3}"
        else host_cli service "${@:2}"; fi ;;
    environment|list) init; host_cli "$1" "${@:2}" ;;
    ip) init; host_cli network address "${2:?VM_ID required}" | cut -d ' ' -f 1 ;;
    start|stop|reboot) init; host_cli "$1" "${2:?VM_ID required}" "${@:3}" ;;
    publish|unpublish|ports) init; host_cli "$1" "${@:2}" ;;
    check) init; check_host ;;
    smoke) init; smoke ;;
    package) init; package_only ;;
    *) usage >&2; exit 2 ;;
esac
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then main "$@"; fi
