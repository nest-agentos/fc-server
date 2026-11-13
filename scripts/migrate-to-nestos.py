#!/usr/bin/env python3
"""Migrate a MindOS-era Firecracker installation to NestOS paths.

Run as root. Defaults to a dry run; pass --apply to change the host.
Stops the server and legacy network units, renames installed paths,
rewrites textual path references, and clears legacy nftables/namespaces.
Guest images built before the rename must be rebuilt: they still contain
the old init script and systemd unit names.
"""
import argparse
import glob
import os
import pwd
import shutil
import subprocess
import sys
from pathlib import Path

DIRECTORY_PAIRS = [
    ('/etc/mindos-firecracker', '/etc/nestos-firecracker'),
    ('/var/lib/mindos-firecracker', '/var/lib/nestos-firecracker'),
    ('/var/lib/mindos-images', '/var/lib/nestos-images'),
    ('/usr/local/libexec/mindos-firecracker', '/usr/local/libexec/nestos-firecracker'),
    ('/usr/share/mindos-image', '/usr/share/nestos-image'),
    ('/var/backups/mindos-firecracker', '/var/backups/nestos-firecracker'),
    ('/run/mindos-firecracker-publish', '/run/nestos-firecracker-publish'),
    ('/run/mindos-firecracker-proxy', '/run/nestos-firecracker-proxy'),
    ('/run/mindos-firecracker-network', '/run/nestos-firecracker-network'),
]
FILE_PAIRS = [
    ('/run/mindos-firecracker.lock', '/run/nestos-firecracker.lock'),
    ('/var/tmp/mindos-firecracker-debian13-clean.ext4', '/var/tmp/nestos-firecracker-debian13-clean.ext4'),
    ('/etc/systemd/system/mindos-firecracker-network.service', '/etc/systemd/system/nestos-firecracker-network.service'),
    ('/etc/systemd/system/mindos-firecracker-dns@.service', '/etc/systemd/system/nestos-firecracker-dns@.service'),
    ('/etc/sysctl.d/90-mindos-firecracker.conf', '/etc/sysctl.d/90-nestos-firecracker.conf'),
    ('/etc/dpkg/dpkg.cfg.d/01-mindos-small', '/etc/dpkg/dpkg.cfg.d/01-nestos-small'),
]
SSH_SUFFIXES = ('', '.pub', '-known-hosts')
TEXT_SUFFIXES = {'.toml', '.json', '.yaml', '.yml', '.conf', '.env', '.service'}
REPLACEMENTS = ((b'MINDOS', b'NESTOS'), (b'MindOS', b'NestOS'), (b'mindos', b'nestos'))


def note(message):
    print(f"[migrate] {message}", file=sys.stderr)


def run(command, apply):
    if apply:
        subprocess.run(command, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        note('would run: ' + ' '.join(command))


def plan_pair(source, target):
    if source.is_symlink() or target.is_symlink():
        raise RuntimeError(f'refusing symlink in migration: {source} / {target}')
    if source.exists() and target.exists():
        raise RuntimeError(f'both legacy and current paths exist; resolve manually: {source} and {target}')
    return source.exists() and not target.exists()


def stop_services(apply):
    run(['systemctl', 'stop', 'firecracker-server'], apply)
    for unit in glob.glob('/etc/systemd/system/mindos-firecracker-dns@*.service'):
        run(['systemctl', 'stop', Path(unit).name], apply)


def clear_network_state(apply):
    for family, table in (('inet', 'mindos_firecracker'), ('ip', 'mindos_firecracker_nat')):
        run(['nft', 'delete', 'table', family, table], apply)
    for namespace in subprocess.run(['ip', 'netns', 'list'], capture_output=True, text=True).stdout.split():
        if namespace.startswith('mindos-fc-'):
            run(['ip', 'netns', 'delete', namespace], apply)
    for root in ('/sys/fs/cgroup/mindos-firecracker', '/sys/fs/cgroup/mindos-firecracker-fs'):
        if apply and os.path.isdir(root):
            shutil.rmtree(root, ignore_errors=True)


def rewrite(path):
    if not path.is_file() or path.is_symlink():
        return False
    if path.suffix not in TEXT_SUFFIXES and path.name != 'environment':
        return False
    original = path.read_bytes()
    updated = original
    for old, new in REPLACEMENTS:
        updated = updated.replace(old, new)
    if updated == original:
        return False
    mode = path.stat().st_mode & 0o777
    temporary = path.with_name(f'.{path.name}.migrate')
    temporary.write_bytes(updated)
    os.chmod(temporary, mode)
    os.replace(temporary, path)
    return True


def migrate(apply):
    if os.geteuid() != 0:
        raise RuntimeError('migration requires root')
    stop_services(apply)
    clear_network_state(apply)

    moved = []
    for source_name, target_name in [*DIRECTORY_PAIRS, *FILE_PAIRS]:
        source, target = Path(source_name), Path(target_name)
        if plan_pair(source, target):
            note(f'rename {source} -> {target}')
            moved.append(target)
            if apply:
                target.parent.mkdir(parents=True, exist_ok=True)
                os.rename(source, target)

    for home in dict.fromkeys((pwd.getpwuid(os.geteuid()).pw_dir,
                               pwd.getpwnam(os.environ['SUDO_USER']).pw_dir
                               if os.environ.get('SUDO_USER') and os.environ['SUDO_USER'] != 'root' else None)):
        if not home:
            continue
        for suffix in SSH_SUFFIXES:
            source = Path(home, '.ssh', 'mindos-firecracker' + suffix)
            target = Path(home, '.ssh', 'nestos-firecracker' + suffix)
            if plan_pair(source, target):
                note(f'rename {source} -> {target}')
                moved.append(target)
                if apply:
                    os.rename(source, target)

    for path in moved:
        if path.is_dir():
            for child in path.rglob('*'):
                if child.is_file() and rewrite(child):
                    note(f'rewrote {child}')
        elif rewrite(path):
            note(f'rewrote {path}')

    run(['systemctl', 'daemon-reload'], apply)
    run(['sysctl', '--system'], apply)
    note('migration applied' if apply else 'dry run only; rerun with --apply')
    note('rebuild guest images (./vmm.sh build --os) and reinstall; pre-rename '
         'images still contain the old init script and unit names')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apply', action='store_true', help='change the host instead of a dry run')
    arguments = parser.parse_args()
    try:
        migrate(arguments.apply)
    except (OSError, RuntimeError, KeyError) as error:
        note(f'result=failed reason={error}')
        sys.exit(1)
