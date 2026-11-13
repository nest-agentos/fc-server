#!/usr/bin/env python3
"""Deploy artifacts to the service's fixed, trusted system paths."""
import fcntl
import grp
import os
from pathlib import Path
import pwd
import secrets
import stat
import subprocess
import sys
import tempfile
import tomllib


def trusted(path):
    if path.resolve() != path:
        raise RuntimeError(f"symlink/noncanonical deployment path: {path}")
    for parent in (path, *path.parents):
        if parent.exists():
            info = parent.stat()
            if info.st_uid != 0 or info.st_mode & 0o022:
                raise RuntimeError(f"expected root ownership and no group/other writes: {parent}")


def directory(path, mode=0o755):
    trusted(path)
    path.mkdir(parents=True, exist_ok=True, mode=mode)
    trusted(path)


def atomic_file(target, mode, source=None, data=None):
    trusted(target)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{target.name}.", dir=target.parent)
    try:
        with os.fdopen(descriptor, "wb") as output:
            if source is not None:
                source.stat()
                subprocess.run(['/usr/bin/cp', '--reflink=auto', '--sparse=always', '--',
                                str(source), temporary], check=True)
            else:
                output.write(data.encode())
            os.fchmod(output.fileno(), mode)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, target)
    finally:
        Path(temporary).unlink(missing_ok=True)


def new_binding(path, mode):
    # Never change ownership of an existing project/config directory.
    if path.is_symlink():
        raise RuntimeError(f"binding cannot be a symlink: {path}")
    if not path.exists():
        path.mkdir(mode=mode)
        os.chown(path, 1000, 1000)
    if not path.is_dir():
        raise RuntimeError(f"binding is not a directory: {path}")


def reserve_uids(base=200000, count=4):
    end = base + count
    for entry in [*pwd.getpwall(), *grp.getgrall()]:
        identity = entry.pw_uid if hasattr(entry, "pw_uid") else entry.gr_gid
        if base <= identity < end:
            raise RuntimeError(f"dedicated VM UID/GID conflicts with host account: {entry[0]}")
    for file in (Path("/etc/subuid"), Path("/etc/subgid")):
        if not file.exists():
            continue
        for line in file.read_text().splitlines():
            if not line or line.startswith("#"):
                continue
            account, start, length = line.split(":")
            if int(start) < end and int(start) + int(length) > base:
                raise RuntimeError(f"VM IDs overlap {file} allocation for {account}")


def main():
    if os.geteuid() != 0:
        raise RuntimeError("installation requires root")
    if Path("/etc/mindos-firecracker").exists() and not Path("/etc/nestos-firecracker").exists():
        raise RuntimeError("legacy MindOS installation detected; run ./vmm.sh migrate --apply first")
    build, repo = [Path(argument).resolve() for argument in sys.argv[1:3]]
    if sys.argv[3:] not in ([], ['--server-only']):
        raise RuntimeError('usage: install-environment.py BUILD REPO [--server-only]')
    # Holding the same lock as the server prevents replacement while it is running.
    with open("/run/nestos-firecracker.lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if subprocess.run(["systemctl", "is-active", "--quiet", "firecracker-server"]).returncode == 0:
            raise RuntimeError("stop firecracker-server before installing")
        config = Path("/etc/nestos-firecracker/config.toml")
        if sys.argv[3:] and not config.exists():
            raise RuntimeError('install-server requires an existing installation; run setup first')
        if config.exists():
            trusted(config)
            data = tomllib.loads(config.read_text())
            reserve_uids(data["uid_base"], data["max_vms"])
        else:
            reserve_uids()
        if sys.argv[3:]:
            install_server(build, repo)
        else:
            install(build, repo)
    print("Installed; existing config and API key preserved. Service has not been started.")


def install(build, repo):
    binaries = Path("/usr/local/libexec/fc-hp")
    images = Path("/var/lib/nestos-images")
    config = Path("/etc/nestos-firecracker")
    for path in (Path("/usr/local/bin"), binaries, Path("/srv/harness")):
        directory(path)
    for path in (images, config, Path("/var/lib/nestos-firecracker")):
        directory(path, 0o700)
    install_server(build, repo)
    for name in ("firecracker", "jailer", "virtiofsd"):
        atomic_file(binaries / name, 0o755, source=build / "bin" / name)
    for name in ("vmlinux", "debian13.ext4", "debian13.ext4.initrd"):
        mode = 0o444 if name == 'debian13.ext4' else 0o600
        atomic_file(images / name, mode, source=build / "images" / name)
    receipt = build / 'images/debian13.ext4.json'
    if receipt.exists():
        atomic_file(images / receipt.name, 0o600, source=receipt)
    new_binding(Path("/srv/harness/project"), 0o750)
    for name in ('codex', 'claude', 'dsh'):
        new_binding(Path(f'/srv/harness/{name}-home'), 0o700)
    initial_config(config, repo, binaries)


def install_server(build, repo):
    for name in ("firecracker-server", "firecracker-guest"):
        if not (build / "bin" / name).is_file():
            raise RuntimeError(f"missing runtime artifact: {name}")
    helpers = Path('/usr/local/libexec/nestos-firecracker')
    directory(helpers)
    atomic_file(Path('/usr/local/bin/firecracker-server'), 0o755,
                source=build / 'bin/firecracker-server')
    for name in ('publish-ports', 'proxy-egress'):
        atomic_file(helpers / name, 0o755, source=build / 'bin/firecracker-server')
    atomic_file(helpers / 'firecracker-guest', 0o755, source=build / 'bin/firecracker-guest')
    unit = Path("/etc/systemd/system/firecracker-server.service")
    trusted(unit.parent)
    atomic_file(unit, 0o644, source=repo / "firecracker-server.service")
    atomic_file(unit.with_name('firecracker-workload@.service'), 0o644,
                source=repo / 'firecracker-workload@.service')
    migrate_runtime_paths(helpers)
    subprocess.run(["systemctl", "daemon-reload"], check=True)


def migrate_runtime_paths(helpers, config=Path('/etc/nestos-firecracker/config.toml'),
                          unit=Path('/etc/systemd/system/nestos-firecracker-network.service')):
    if config.exists():
        text = config.read_text()
        for name in ('publish-ports', 'proxy-egress'):
            text = text.replace(str(helpers / (name + '.py')), str(helpers / name))
        if text != config.read_text():
            backup = config.with_name('config.before-rust-runtime.toml')
            if not backup.exists():
                atomic_file(backup, 0o600, source=config)
            atomic_file(config, 0o600, data=text)
    if unit.exists():
        text = unit.read_text().replace(str(helpers / 'network-environment.py'),
                                       '/usr/local/bin/firecracker-server network')
        atomic_file(unit, 0o644, data=text)
    for name in ('network-environment', 'install-environment', 'publish-ports', 'service-environment',
                 'service-definition', 'service-guest', 'service-list', 'proxy-egress', 'proxy-policy'):
        path = helpers / (name + '.py')
        if path.exists():
            trusted(path)
            path.unlink()
        cache = helpers / "__pycache__"
        if cache.is_dir():
            for bytecode in cache.glob(name + ".*.pyc"):
                trusted(bytecode)
                bytecode.unlink()


def initial_config(config, repo, binaries):
    target = config / "config.toml"
    if not target.exists():
        text = (repo / "config.virtiofs.example.toml").read_text()
        text = text.replace('"/usr/libexec/virtiofsd"', f'"{binaries}/virtiofsd"')
        atomic_file(target, 0o600, data=text)
    environment = config / "environment"
    if not environment.exists():
        atomic_file(environment, 0o600, data=f"FIRECRACKER_API_KEY={secrets.token_hex(32)}\n")
    for path in (target, environment):
        trusted(path)
        if stat.S_IMODE(path.stat().st_mode) != 0o600:
            raise RuntimeError(f"private config must have mode 0600: {path}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"[install] result=failed reason={error}", file=sys.stderr)
        sys.exit(1)
