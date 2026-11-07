#!/usr/bin/env python3
"""Deployment regression tests; require neither root nor a running VM."""
from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
from collections import namedtuple
import ipaddress
import os
import stat
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch


SPEC = spec_from_file_location("installer", Path(__file__).with_name("install-environment.py"))
installer = module_from_spec(SPEC)
SPEC.loader.exec_module(installer)
REPO = Path(__file__).resolve().parents[1]
INFO_SPEC = spec_from_file_location('image_info', REPO / 'scripts/image-info.py')
image_info = module_from_spec(INFO_SPEC)
INFO_SPEC.loader.exec_module(image_info)
NET_SPEC = spec_from_file_location('network_info', REPO / 'scripts/install-network.py')
network_info = module_from_spec(NET_SPEC)
NET_SPEC.loader.exec_module(network_info)
SHARED_SPEC = spec_from_file_location('shared_root', REPO / 'scripts/shared-root-environment.py')
shared_root = module_from_spec(SHARED_SPEC)
SHARED_SPEC.loader.exec_module(shared_root)


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)

    def test_config_and_key_survive_reinstall(self):
        with patch.object(installer, "trusted"):
            installer.initial_config(self.directory, REPO, Path("/usr/local/libexec/fc-hp"))
            environment = self.directory / "environment"
            original_key = environment.read_bytes()
            config = self.directory / "config.toml"
            self.assertIn('virtiofsd = "/usr/local/libexec/fc-hp/virtiofsd"', config.read_text())
            config.write_text("# administrator config\n")
            installer.initial_config(self.directory, REPO, Path("/another/daemon"))
            self.assertEqual(config.read_text(), "# administrator config\n")
            self.assertEqual(environment.read_bytes(), original_key)
            self.assertEqual(stat.S_IMODE(environment.stat().st_mode), 0o600)

    def test_server_only_install_never_deploys_images(self):
        (self.directory / 'bin').mkdir()
        for name in ('firecracker-server', 'firecracker-guest'):
            (self.directory / 'bin' / name).write_bytes(b'fixture')
        with patch.object(installer, 'directory'), patch.object(installer, 'trusted'), \
                patch.object(installer, 'atomic_file') as write, patch.object(installer, 'migrate_runtime_paths'), \
                patch.object(installer.subprocess, 'run'):
            installer.install_server(self.directory, REPO)
        targets = [str(call.args[0]) for call in write.call_args_list]
        self.assertIn('/usr/local/bin/firecracker-server', targets)
        self.assertIn('/etc/systemd/system/firecracker-workload@.service', targets)
        self.assertFalse(any(path.startswith(('/var/lib/mindos-images/', '/etc/mindos-firecracker/'))
                             for path in targets))

    def test_runtime_migration_preserves_business_config_and_removes_known_python_helpers(self):
        helpers = self.directory / 'helpers'
        helpers.mkdir()
        (helpers / '__pycache__').mkdir()
        (helpers / 'service-guest.py').write_text('legacy')
        (helpers / '__pycache__/service-guest.cpython-314.pyc').write_bytes(b'cache')
        (helpers / 'custom.py').write_text('administrator tool')
        config = self.directory / 'config.toml'
        original = f'port_publisher = "{helpers}/publish-ports.py"\n[profiles.custom]\nvalue="private-fixture"\n'
        config.write_text(original)
        unit = self.directory / 'network.service'
        unit.write_text(f'ExecStart={helpers}/network-environment.py up\n')
        with patch.object(installer, 'trusted'):
            installer.migrate_runtime_paths(helpers, config, unit)
            installer.migrate_runtime_paths(helpers, config, unit)
        self.assertEqual(config.with_name('config.before-rust-runtime.toml').read_text(), original)
        self.assertIn('value="private-fixture"', config.read_text())
        self.assertNotIn('publish-ports.py', config.read_text())
        self.assertIn('/usr/local/bin/firecracker-server network up', unit.read_text())
        self.assertFalse((helpers / 'service-guest.py').exists())
        self.assertEqual(list((helpers / '__pycache__').iterdir()), [])
        self.assertTrue((helpers / 'custom.py').exists())

    def test_missing_guest_artifact_fails_before_any_system_mutation(self):
        with patch.object(installer, 'atomic_file') as write:
            with self.assertRaisesRegex(RuntimeError, 'missing runtime artifact'):
                installer.install_server(self.directory, REPO)
            write.assert_not_called()

    def test_failed_copy_preserves_existing_artifact(self):
        target = self.directory / "rootfs.ext4"
        target.write_bytes(b"previous image")
        with patch.object(installer, "trusted"), self.assertRaises(FileNotFoundError):
            installer.atomic_file(target, 0o600, source=self.directory / "missing")
        self.assertEqual(target.read_bytes(), b"previous image")
        self.assertEqual(list(self.directory.iterdir()), [target])

    def test_atomic_replacement_is_private(self):
        target = self.directory / "environment"
        with patch.object(installer, "trusted"):
            installer.atomic_file(target, 0o600, data="private test fixture")
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o600)
        self.assertEqual(target.read_text(), "private test fixture")

    def test_sparse_image_survives_deployment(self):
        source, target = self.directory / 'source.ext4', self.directory / 'deployed.ext4'
        size = 64 * 1024 * 1024
        with source.open('wb') as output:
            output.write(b'head')
            output.seek(size - 1)
            output.write(b'Z')
        with patch.object(installer, 'trusted'):
            installer.atomic_file(target, 0o600, source=source)
        self.assertEqual(target.stat().st_size, size)
        self.assertLess(target.stat().st_blocks * 512, size // 2)
        with target.open('rb') as content:
            self.assertEqual(content.read(4), b'head')
            content.seek(size - 1)
            self.assertEqual(content.read(), b'Z')

    def test_trust_rejects_symlink(self):
        link = self.directory / "alias"
        link.symlink_to(self.directory)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            installer.trusted(link)

    def test_existing_binding_ownership_is_preserved(self):
        with patch.object(installer.os, "chown") as chown:
            installer.new_binding(self.directory, 0o700)
            chown.assert_not_called()

    def test_binding_symlink_is_rejected(self):
        link = self.directory / "project"
        link.symlink_to(self.directory)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            installer.new_binding(link, 0o700)

    def test_custom_uid_range_conflict_is_rejected(self):
        account = namedtuple("Account", "pw_name pw_uid")("occupied", 300003)
        with patch.object(installer.pwd, "getpwall", return_value=[account]), \
                patch.object(installer.grp, "getgrall", return_value=[]), \
                self.assertRaisesRegex(RuntimeError, "occupied"):
            installer.reserve_uids(300000, 4)


class BuildSelectionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.build = self.directory / 'artifacts'
        self.trace = self.directory / 'trace'
        commands = self.directory / 'commands'
        commands.mkdir()
        cargo = commands / 'cargo'
        cargo.write_text('#!/bin/sh\nset -eu\nprintf "cargo:%s\\n" "$*" >> "$TRACE"\n'
                         'mkdir -p "$CARGO_TARGET_DIR/release"\n'
                         'printf server > "$CARGO_TARGET_DIR/release/firecracker-server"\n')
        cargo.chmod(0o755)
        self.environment = {**os.environ, 'PATH': str(commands) + ':' + os.environ['PATH'],
                            'BUILD_DIR': str(self.build), 'CARGO_TARGET_DIR': str(self.directory / 'target'),
                            'TRACE': str(self.trace), 'SSH_IDENTITY': str(self.directory / 'identity')}
        (self.directory / 'identity').write_text('fixture')

    def images(self):
        directory = self.build / 'images'
        directory.mkdir(parents=True, exist_ok=True)
        for name in ('vmlinux', 'debian13.ext4', 'debian13.ext4.initrd', 'debian13.ext4.zst'):
            (directory / name).write_bytes(b'existing OS fixture')
        return {path.name: (path.read_bytes(), path.stat().st_mtime_ns) for path in directory.iterdir()}

    def run_build(self, *arguments):
        program = r'''
source "$1"
shift
record() { printf '%s\n' "$*" >> "$TRACE"; }
build_preflight() { record "preflight:$*"; }
build_fork() { record fork; printf fork > "$build_dir/bin/firecracker"; }
build_daemon() { record daemon; printf daemon > "$build_dir/bin/virtiofsd"; }
compile_guest() { record guest-agent; printf guest > "$1/bin/firecracker-guest"; }
build_kernel() { record kernel; printf kernel > "$build_dir/images/vmlinux"; }
build_image() { record image; printf image > "$build_dir/images/debian13.ext4"; printf initrd > "$build_dir/images/debian13.ext4.initrd"; }
package_images() { record package; write_manifest; }
dependencies() { record deps; }
as_root() { record "privileged:$*"; }
host_cli() { record "native:$*"; }
deploy() { record deploy; }
check_host() { record check; }
need() { :; }
main "$@"
'''
        return subprocess.run(['bash', '-c', program, 'build-test', str(REPO / 'vmm.sh'), *arguments],
                              env=self.environment, capture_output=True, text=True, timeout=10)

    def steps(self):
        return self.trace.read_text().splitlines() if self.trace.exists() else []

    def test_default_build_preserves_existing_os_and_does_not_package(self):
        before = self.images()
        result = self.run_build('build')
        self.assertEqual(result.returncode, 0, result.stderr)
        after = {path.name: (path.read_bytes(), path.stat().st_mtime_ns)
                 for path in (self.build / 'images').iterdir()}
        self.assertEqual(before, after)
        self.assertFalse({'kernel', 'image', 'package'} & set(self.steps()))
        self.assertIn('cargo:build --locked --release --bin firecracker-server', self.steps())
        subprocess.run(['sha256sum', '--check', 'SHA256SUMS'], cwd=self.build,
                       check=True, capture_output=True)

    def test_default_build_succeeds_without_os_artifacts(self):
        result = self.run_build('build')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(list((self.build / 'images').iterdir()))
        self.assertNotIn('images/', (self.build / 'SHA256SUMS').read_text())

    def test_os_flag_runs_kernel_image_and_packaging(self):
        result = self.run_build('build', '--os')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue({'kernel', 'image', 'package'} <= set(self.steps()))
        self.assertIn('images/vmlinux', (self.build / 'SHA256SUMS').read_text())

    def test_unknown_options_fail_before_build_or_directory_creation(self):
        for command in ('build', 'setup', 'network', 'shared-root'):
            result = self.run_build(command, '--unknown')
            self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.build.exists())
        self.assertEqual(self.steps(), [])

    def test_initialization_requires_explicit_os_flag_when_assets_missing(self):
        for command in ('setup', 'network', 'shared-root'):
            result = self.run_build(command)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('add --os', result.stderr)
        self.assertEqual(self.steps(), [])

    def test_initialization_propagates_only_explicit_os_selection(self):
        self.images()
        for command in ('setup', 'network', 'shared-root'):
            for options in ([], ['--os']):
                self.trace.unlink(missing_ok=True)
                result = self.run_build(command, *options)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual('kernel' in self.steps(), bool(options))


class SharedRootTests(unittest.TestCase):
    def test_migration_preserves_network_and_is_idempotent(self):
        text = (REPO / 'config.virtiofs.example.toml').read_text()
        text += '\n[[profiles.debian13.network_slots]]\ntap="fc-tap"\nnetns="/run/netns/a"\nguest_mac="02:fc:00:00:00:01"\n'
        before = tomllib.loads(text)
        updated = shared_root.updated(text, 'debian13')
        self.assertEqual(shared_root.updated(updated, 'debian13'), updated)
        after = tomllib.loads(updated)
        self.assertEqual(before['bindings'], after['bindings'])
        self.assertEqual(before['profiles']['debian13']['network_slots'],
                         after['profiles']['debian13']['network_slots'])
        self.assertEqual(after['profiles']['debian13']['rootfs_mode'], 'shared')

    def test_migration_rejects_custom_images(self):
        text = (REPO / 'config.virtiofs.example.toml').read_text()
        text = text.replace('/var/lib/mindos-images/debian13.ext4', '/other.ext4')
        with self.assertRaisesRegex(RuntimeError, 'built-in Debian image'):
            shared_root.updated(text, 'debian13')


class ImageRecipeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        (self.repo / 'guest').mkdir()
        (self.repo / 'scripts').mkdir()
        (self.repo / 'guest/Dockerfile').write_text('FROM debian:13-slim\n')
        (self.repo / 'scripts/build-debian13.sh').write_text('# image export recipe\n')
        (self.repo / 'scripts/build-initramfs.py').write_text('# bootstrap archive recipe\n')

    def test_guest_change_invalidates_cached_image(self):
        before = image_info.inputs(self.repo, 2048, 'base')
        (self.repo / 'guest/service').write_text('new startup service')
        self.assertNotEqual(before, image_info.inputs(self.repo, 2048, 'base'))

    def test_disk_size_and_flavor_invalidate_cached_image(self):
        before = image_info.inputs(self.repo, 2048, 'base')
        self.assertNotEqual(before, image_info.inputs(self.repo, 4096, 'base'))
        self.assertNotEqual(before, image_info.inputs(self.repo, 2048, 'harness'))

    def test_local_dependencies_do_not_change_recipe(self):
        before = image_info.inputs(self.repo, 2048, 'base')
        dependencies = self.repo / 'guest/node_modules'
        dependencies.mkdir()
        (dependencies / 'local-package').write_text('not part of Docker context')
        self.assertEqual(before, image_info.inputs(self.repo, 2048, 'base'))

    def test_private_ssh_key_cannot_be_baked_into_image(self):
        key = self.repo / 'private-key'
        key.write_text('-----BEGIN OPENSSH PRIVATE KEY-----\nprivate-fixture\n')
        with patch.dict(os.environ, {'SSH_PUBLIC_KEY': str(key)}), self.assertRaises(ValueError):
            image_info.ssh_key()


class NetworkPlanTests(unittest.TestCase):
    def test_slots_have_disjoint_addresses_and_mac_addresses(self):
        guest, uplink = map(ipaddress.ip_network, ('10.203.0.0/16', '10.204.0.0/16'))
        slots = [network_info.slot(i, guest, uplink) for i in range(100)]
        self.assertEqual(len({item['guest_mac'] for item in slots}), 100)
        self.assertEqual(len({item['guest_ip'] for item in slots}), 100)
        networks = [ipaddress.ip_network(item['guest_subnet']) for item in slots]
        for index, first in enumerate(networks):
            self.assertFalse(any(first.overlaps(second) for second in networks[index + 1:]))

    def test_loopback_dns_is_rejected(self):
        with patch.dict(os.environ, {'FC_DNS_SERVERS': '127.0.0.53'}), self.assertRaises(RuntimeError):
            network_info.dns_servers()

    def test_existing_host_routes_cannot_be_overridden(self):
        plan = {'guest_network': '10.203.0.0/16', 'uplink_network': '10.204.0.0/16'}
        response = namedtuple('Response', 'stdout')('[{"dst":"10.0.0.0/8","dev":"corp"}]')
        with patch.object(network_info, 'run', return_value=response), self.assertRaises(RuntimeError):
            network_info.check_routes(plan)


if __name__ == "__main__":
    unittest.main()
