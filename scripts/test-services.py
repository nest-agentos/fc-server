#!/usr/bin/env python3
"""Service policy, ownership, recovery and unprivileged guest execution regressions."""
from importlib.util import module_from_spec, spec_from_file_location
import base64
import copy
from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import selectors
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch


def sibling(name):
    spec = spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


host = sibling('service-environment')
policy = host.definition
listing = sibling('service-list')


class Services(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for name in ('packages', 'vms', 'runtime', 'locks'):
            (self.root / name).mkdir()
        self.patch(host, 'ROOT', self.root)
        self.patch(host.installer, 'trusted')

    def patch(self, target, name, *args, **options):
        context = patch.object(target, name, *args, **options)
        self.addCleanup(context.stop)
        return context.start()

    def manifest(self, extra=''):
        path = self.root / 'service.toml'
        path.write_text('id="generic"\nprofile="debian13"\ncommand=["/bin/true"]\n'
                        'working_directory="/home/agent"\nready_port=8080\n' + extra)
        return path

    def test_rejects_unknown_fields_and_traversal(self):
        with self.assertRaisesRegex(ValueError, 'UNKNOWN_FIELD'):
            policy.definition(self.manifest('application_special_case=true\n'))
        for path in ('../config', '/etc/config', '.', 'a/../b', 'run.lock'):
            with self.assertRaises(ValueError):
                policy.relative(path)

    def test_snapshot_copies_assets_and_keeps_secret_out_of_digest(self):
        (self.root / 'config').write_text('private-test-token')
        value = policy.definition(self.manifest('[[files]]\nsource="config"\ntarget="config"\n'))
        (self.root / 'config').write_text('changed')
        self.assertEqual(base64.b64decode(value['files'][0]['content']), b'private-test-token')
        self.assertNotIn('private-test-token', policy.digest(value))

    def test_rejects_symlinks_and_overlapping_assets(self):
        (self.root / 'file').write_text('config')
        (self.root / 'link').symlink_to(self.root / 'file')
        with self.assertRaisesRegex(ValueError, 'FILE_OUTSIDE_PACKAGE'):
            policy.assets([{'source': 'link', 'target': 'config'}], self.root)
        with self.assertRaisesRegex(ValueError, 'OVERLAPPING_FILES'):
            policy.assets([{'source': 'file', 'target': name} for name in ('config', 'config/a')], self.root)

    def test_port_validation(self):
        for value in ({'host_port': 0, 'guest_port': 80},
                      {'host_port': 8080, 'guest_port': 80, 'protocol': 'sctp'}):
            with self.assertRaises(ValueError):
                policy.publications([value])

    def test_binding_does_not_adopt_unmanaged_vm(self):
        self.patch(host, 'package', return_value={})
        self.patch(host, 'inspect', return_value={'id': 'unmanaged'})
        with self.assertRaisesRegex(ValueError, 'VM_ALREADY_EXISTS_NOT_MANAGED'):
            host.bind('unmanaged', 'generic')
        self.assertFalse((self.root / 'vms/unmanaged.json').exists())

    def test_binding_is_stable_and_conflicts_fail(self):
        self.patch(host, 'package', return_value={})
        self.patch(host, 'inspect', return_value=None)
        host.bind('test', 'generic')
        host.bind('test', 'generic')
        with self.assertRaisesRegex(ValueError, 'VM_SERVICE_BINDING_CONFLICT'):
            host.bind('test', 'another')

    def test_cleanup_never_deletes_replacement_vm(self):
        host.save(host.runtime_path('test'), {'generation': 'old'})
        self.patch(host, 'inspect', return_value={'id': 'test'})
        self.patch(host, 'generation', return_value='replacement')
        api = self.patch(host.network, 'api_request')
        with self.assertRaisesRegex(ValueError, 'CLEANUP_INSTANCE_OWNERSHIP_CONFLICT'):
            host.cleanup_vm('test')
        api.assert_not_called()
        self.assertTrue(host.runtime_path('test').exists())

    def test_failed_publication_never_notifies_ready(self):
        self.patch(host.network, 'api_request', side_effect=RuntimeError('HOST_PORT_IN_USE'))
        notify = self.patch(host, 'notify')
        with self.assertRaisesRegex(RuntimeError, 'HOST_PORT_IN_USE'):
            host.publish('test', {'id': 'generic', 'publish': [{}]})
        notify.assert_not_called()

    def test_package_update_requires_stopped_supervisor(self):
        path = self.root / 'packages/generic.json'
        host.save(path, {'definition': {'id': 'generic'}, 'identity': 'old'})
        host.save(self.root / 'vms/test.json', {'service': 'generic'})
        value = {'definition': {'id': 'generic'}, 'identity': 'new'}
        self.patch(host, 'systemctl', return_value=subprocess.CompletedProcess([], 0, b'active\n'))
        with self.assertRaisesRegex(ValueError, 'SERVICE_UPDATE_REQUIRES_STOPPED_SUPERVISORS'):
            host.install_package(path, value)
        self.assertEqual(host.read(path)['identity'], 'old')

    def test_guest_errors_cannot_inject_payload_into_host_logs(self):
        self.assertEqual(host.guest_category('private-test-token'), 'INVALID_GUEST_ERROR')
        self.assertEqual(host.guest_category('READINESS_TIMEOUT'), 'READINESS_TIMEOUT')

    def test_prepare_failure_cleans_vm(self):
        self.patch(host, 'load', return_value={'identity': 'key', 'definition': {'startup_timeout_secs': 1}})
        self.patch(host, 'ensure_vm')
        self.patch(host, 'wait_ssh', side_effect=RuntimeError('SSH_READY_TIMEOUT'))
        cleanup = self.patch(host, 'cleanup_vm')
        with self.assertRaisesRegex(RuntimeError, 'SSH_READY_TIMEOUT'):
            host.execute('test', 'prepare')
        cleanup.assert_called_once_with('test')
        self.assertFalse((self.root / 'runtime/test.identity').exists())


class Inventory(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.config = self.root / 'config.toml'
        self.config.write_text('state_dir=' + json.dumps(str(self.root)) + '\n[profiles.test]\n')
        for target, name, value in ((listing, 'SERVICES', self.root),
                                    (listing.network, 'CONFIG', self.config)):
            patcher = patch.object(target, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        patcher = patch.object(listing.network.installer, 'trusted')
        patcher.start()
        self.addCleanup(patcher.stop)

    def live(self, name='running'):
        return {'id': name, 'profile': 'test', 'state': 'vmm-running',
                'rootfsPersistent': True, 'ports': [{'listen': '127.0.0.1',
                'host_port': 18080, 'guest_port': 8080, 'protocol': 'tcp'}]}

    def write(self, name, value):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))

    def inventory(self, live, all=False):
        with patch.object(listing.network, 'api_request', return_value={'vms': live}) as api, \
                patch.object(listing, 'supervisor_states', return_value={}):
            result = listing.inventory(all)
        api.assert_called_once_with('GET', '/v1/vms')
        return result

    def test_default_lists_managed_and_unmanaged_running_vms_only(self):
        self.write('vms/running.json', {'service': 'web'})
        self.write('vms/stopped.json', {'service': 'other'})
        values = self.inventory([self.live(), self.live('manual')])
        self.assertEqual([row['id'] for row in values], ['manual', 'running'])
        self.assertEqual(values[1]['service'], 'web')
        self.assertIsNone(values[0]['service'])
        self.assertEqual(values[1]['ports'][0]['host_port'], 18080)

    def test_all_includes_stopped_bindings_and_detached_persistent_roots(self):
        self.write('vms/stopped.json', {'service': 'web'})
        self.write('roots/retained/manifest.json', {'spec': {'id': 'retained', 'profile': 'test'}})
        self.write('roots/pending.initializing/manifest.json', {'spec': {'id': 'pending', 'profile': 'test'}})
        values = self.inventory([self.live()], all=True)
        self.assertEqual([row['id'] for row in values], ['retained', 'running', 'stopped'])
        self.assertEqual(values[0]['state'], 'stopped')
        self.assertTrue(values[0]['rootfsPersistent'])
        self.assertEqual(values[2]['service'], 'web')

    def test_many_vms_still_use_one_api_snapshot(self):
        self.assertEqual(len(self.inventory([self.live('vm-' + str(i)) for i in range(128)])), 128)

    def test_service_ready_requires_matching_generation(self):
        self.write('runtime/running.json', {'generation': 'old', 'state': 'ready'})
        states = {'firecracker-workload@running.service': 'active'}
        self.assertEqual(listing.service_state('running', 'replacement', states), 'active')
        self.assertEqual(listing.service_state('running', 'old', states), 'ready')

    def test_empty_and_table_output_do_not_read_package_secrets(self):
        self.write('packages/private.json', {'identity': 'private-test-token'})
        output = io.StringIO()
        with redirect_stdout(output):
            listing.table(self.inventory([]), False)
            listing.table(self.inventory([self.live()]), False)
        self.assertIn('当前没有运行的虚拟机', output.getvalue())
        self.assertIn('127.0.0.1:18080->8080/tcp', output.getvalue())
        self.assertNotIn('private-test-token', output.getvalue())

    def test_management_failure_is_not_reported_as_empty_list(self):
        with patch.object(listing.network, 'api_request', side_effect=RuntimeError('UNAVAILABLE')):
            with self.assertRaisesRegex(RuntimeError, 'UNAVAILABLE'):
                listing.inventory()


class Guest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.home = Path(self.temporary.name)

    def spec(self):
        return {'id': 'generic', 'files': [], 'command': ['/bin/false'],
                'environment': {}, 'working_directory': str(self.home),
                'startup_timeout_secs': 2, 'ready_port': self.free_port()}

    def free_port(self):
        with socket.socket() as channel:
            channel.bind(('127.0.0.1', 0))
            return channel.getsockname()[1]

    def launch(self, spec, action='run'):
        child = subprocess.Popen([sys.executable, '-u', str(Path(__file__).with_name('service-guest.py'))],
            env={**os.environ, 'HOME': str(self.home)}, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self.addCleanup(self.stop, child)
        child.stdin.write(json.dumps({'definition': spec, 'action': action}).encode() + b'\n')
        child.stdin.flush()
        return child

    def stop(self, child):
        if not child.stdin.closed:
            child.stdin.close()
        try:
            child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)
        child.stdout.close()
        child.stderr.close()

    def event(self, child):
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(timeout=10), 'guest event timed out')
        return json.loads(child.stdout.readline())

    def test_prepare_private_files_without_running_command(self):
        spec = self.spec()
        spec['files'] = [{'target': 'config', 'content': base64.b64encode(b'private').decode(), 'mode': 0o600}]
        child = self.launch(spec, 'prepare')
        self.assertEqual(self.event(child)['event'], 'prepared')
        self.assertEqual(child.wait(timeout=5), 0)
        path = self.home / '.local/share/firecracker-services/generic/config'
        self.assertEqual(path.read_bytes(), b'private')
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_command_failure_reports_category_without_payload(self):
        child = self.launch(self.spec())
        self.assertEqual(self.event(child)['event'], 'prepared')
        result = self.event(child)
        self.assertEqual(result['category'], 'SERVICE_EXITED')

    def test_ssh_eof_reaps_running_service(self):
        spec = self.spec()
        program = ('import socket,time,os;'
                   'open("pid","w").write(str(os.getpid()));'
                   's=socket.socket();s.bind(("127.0.0.1",int(os.environ["PORT"])));'
                   's.listen();time.sleep(60)')
        spec.update(command=[sys.executable, '-c', program], environment={'PORT': str(spec['ready_port'])})
        child = self.launch(spec)
        self.assertEqual(self.event(child)['event'], 'prepared')
        self.assertEqual(self.event(child)['event'], 'ready')
        pid = int((self.home / 'pid').read_text())
        child.stdin.close()
        self.assertEqual(self.event(child)['category'], 'CONTROL_DISCONNECTED')
        self.assertEqual(child.wait(timeout=15), 0)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_existing_listener_is_not_mistaken_for_ready(self):
        spec = self.spec()
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', spec['ready_port']))
            listener.listen()
            child = self.launch(spec)
            self.assertEqual(self.event(child)['event'], 'prepared')
            self.assertEqual(self.event(child)['category'], 'READY_PORT_ALREADY_IN_USE')

    def test_partial_guest_event_does_not_block_timeout(self):
        child = subprocess.Popen([sys.executable, '-u', '-c',
            'import sys,time;sys.stdout.write("partial");sys.stdout.flush();time.sleep(30)'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self.addCleanup(self.stop, child)
        events = host.guest_events(child)
        self.addCleanup(events.close)
        self.assertIsNone(next(events))
        child.terminate()

    def test_upload_to_unresponsive_guest_is_bounded(self):
        child = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(30)'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self.addCleanup(self.stop, child)
        with self.assertRaisesRegex(ValueError, 'GUEST_UPLOAD_TIMEOUT'):
            host.send_payload(child, b'x' * 1024 * 1024, 0.2)
        child.terminate()


if __name__ == '__main__':
    unittest.main()
