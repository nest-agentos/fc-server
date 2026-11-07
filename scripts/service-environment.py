#!/usr/bin/env python3
"""Host lifecycle for generic SSH-managed services; guest images remain unchanged."""
import argparse
from contextlib import contextmanager, ExitStack
import fcntl
from importlib.util import module_from_spec, spec_from_file_location
import json
import os
from pathlib import Path
import selectors
import shlex
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import uuid


def sibling(name):
    spec = spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


network = sibling('network-environment')
definition = sibling('service-definition')
installer = network.installer
ROOT = Path('/var/lib/mindos-firecracker/services')
OPERATION = uuid.uuid4().hex
STARTED = time.monotonic()


def audit(stage, result='success', **metadata):
    print(json.dumps({'operation': 'managed_service', 'operation_id': OPERATION,
                     'stage': stage, 'result': result,
                     'elapsed_ms': round((time.monotonic() - STARTED) * 1000), **metadata}),
          file=sys.stderr, flush=True)


def initialize():
    definition.require(os.geteuid() == 0, 'SERVICE_MANAGEMENT_REQUIRES_ROOT')
    for name in ('', 'packages', 'vms', 'runtime', 'locks'):
        installer.directory(ROOT / name, 0o700)


def save(path, value):
    installer.atomic_file(path, 0o600, data=json.dumps(value, sort_keys=True) + '\n')


def read(path):
    installer.trusted(path)
    return json.loads(path.read_text())


def unit(vm):
    return 'firecracker-workload@' + definition.identifier(vm) + '.service'


def systemctl(*args, check=True):
    result = subprocess.run(['/usr/bin/systemctl', *args], capture_output=True, timeout=420)
    if check and result.returncode:
        targets = [a for a in args if a.endswith('.service')]
        audit('systemd_operation', 'failure', exit_code=result.returncode, units=targets)
    definition.require(not check or result.returncode == 0, 'SYSTEMD_OPERATION_FAILED')
    return result


@contextmanager
def lock(vm):
    path = ROOT / 'locks' / (definition.identifier(vm) + '.lock')
    installer.trusted(path)
    with path.open('a') as handle:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield


def inspect(vm):
    try:
        return network.api_request('GET', '/v1/vms/' + vm)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def check_resources(spec):
    config = network.tomllib.loads(network.CONFIG.read_text())
    profile = config['profiles'].get(spec['profile'])
    definition.require(profile is not None, 'UNKNOWN_PROFILE')
    definition.require(bool(profile.get('network_slots')), 'PROFILE_REQUIRES_SSH_NETWORK')
    allowed = set(profile.get('bindings', []))
    definition.require(set(spec['directories']) <= allowed, 'DIRECTORY_NOT_AUTHORIZED')
    for name in spec['directories']:
        definition.require(config['bindings'][name].get('backend') == 'virtiofs',
                           'DIRECTORY_REQUIRES_VIRTIOFS')


def install(path, identity):
    spec = definition.definition(Path(path).resolve())
    check_resources(spec)
    identity = Path(identity)
    definition.require(identity.is_file() and identity.stat().st_size <= 65536, 'INVALID_SSH_IDENTITY')
    key = identity.read_bytes()
    package = {'definition': spec, 'identity': key.decode()}
    path = ROOT / 'packages' / (spec['id'] + '.json')
    install_package(path, package)
    audit('package_installed', service=spec['id'], file_count=len(spec['files']),
          digest=definition.digest(spec))


def install_package(path, value):
    with (ROOT / 'locks' / ('package.' + value['definition']['id'])).open('a') as guard:
        fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if path.exists() and read(path) == value:
            return
        with ExitStack() as stack:
            for binding in sorted((ROOT / 'vms').glob('*.json')):
                if read(binding)['service'] == value['definition']['id']:
                    stack.enter_context(lock(binding.stem))
                    active = systemctl('is-active', unit(binding.stem), check=False).stdout.decode().strip()
                    definition.require(active in ('inactive', 'failed'), 'SERVICE_UPDATE_REQUIRES_STOPPED_SUPERVISORS')
            save(path, value)


def package(service):
    return read(ROOT / 'packages' / (definition.identifier(service) + '.json'))


def bind(vm, service):
    path = ROOT / 'vms' / (definition.identifier(vm) + '.json')
    desired = {'service': definition.identifier(service)}
    package(service)
    with lock(vm):
        if path.exists():
            definition.require(read(path) == desired, 'VM_SERVICE_BINDING_CONFLICT')
        else:
            definition.require(inspect(vm) is None, 'VM_ALREADY_EXISTS_NOT_MANAGED')
            save(path, desired)
    audit('vm_bound', vm=vm, service=service)


def load(vm):
    return package(read(ROOT / 'vms' / (definition.identifier(vm) + '.json'))['service'])


def runtime_path(vm):
    return ROOT / 'runtime' / (definition.identifier(vm) + '.json')


def generation(vm):
    return network.connection(vm)[1]


def ensure_vm(vm, spec):
    existing = inspect(vm)
    record = read(runtime_path(vm)) if runtime_path(vm).exists() else None
    if existing:
        definition.require(record is not None and record.get('generation') == generation(vm),
                           'VM_INSTANCE_OWNERSHIP_CONFLICT')
    network.api_request('POST', '/v1/vms', {'id': vm, 'profile': spec['profile'],
        'directories': spec['directories'], 'volumes': spec['volumes']})
    save(runtime_path(vm), {'generation': generation(vm), 'state': 'preparing'})
    audit('vm_created_or_reused', vm=vm)


def ssh_command(vm, identity):
    address, alias = network.connection(vm)
    return ['/usr/bin/ssh', '-F', '/dev/null', '-T', '-i', str(identity),
        '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'ForwardAgent=no',
        '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=2',
        '-o', 'ServerAliveInterval=5', '-o', 'ServerAliveCountMax=3',
        '-o', 'UserKnownHostsFile=' + str(ROOT / 'known_hosts'),
        '-o', 'HostKeyAlias=' + alias, 'agent@' + address]


def wait_ssh(vm, identity, timeout):
    deadline = time.monotonic() + timeout
    category, attempts = 'SSH_CONNECTION_FAILED', 0
    while time.monotonic() < deadline:
        attempts += 1
        result = subprocess.run(ssh_command(vm, identity) + ['true'],
                                capture_output=True, timeout=5)
        if result.returncode == 0:
            audit('ssh_ready', vm=vm, attempts=attempts)
            return
        category = ssh_category(result.stderr)
        if category in ('SSH_AUTHENTICATION_FAILED', 'SSH_HOST_KEY_REJECTED'):
            break
        time.sleep(0.3)
    raise RuntimeError(category)


def ssh_category(output):
    for token, category in ((b'Permission denied', 'SSH_AUTHENTICATION_FAILED'),
                            (b'Host key verification failed', 'SSH_HOST_KEY_REJECTED'),
                            (b'Connection refused', 'SSH_CONNECTION_REFUSED'),
                            (b'timed out', 'SSH_CONNECT_TIMEOUT')):
        if token in output:
            return category
    return 'SSH_CONNECTION_FAILED'


def launch(vm, value, action, identity):
    source = Path(__file__).with_name('service-guest.py').read_text()
    remote = 'exec /usr/bin/python3 -u -c ' + shlex.quote(source)
    child = subprocess.Popen(ssh_command(vm, identity) + [remote],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=0)
    try:
        payload = json.dumps({'action': action, 'definition': value}).encode() + b'\n'
        send_payload(child, payload, value['startup_timeout_secs'])
        return child
    except BaseException:
        finish_ssh(child)
        raise


def send_payload(child, payload, timeout):
    deadline = time.monotonic() + timeout
    os.set_blocking(child.stdin.fileno(), False)
    with selectors.DefaultSelector() as selector:
        selector.register(child.stdin, selectors.EVENT_WRITE)
        view = memoryview(payload)
        while view:
            definition.require(time.monotonic() < deadline, 'GUEST_UPLOAD_TIMEOUT')
            if selector.select(timeout=0.1):
                try:
                    count = os.write(child.stdin.fileno(), view[:65536])
                    view = view[count:]
                except BlockingIOError:
                    continue


def guest_events(child):
    pending = b''
    with selectors.DefaultSelector() as selector:
        selector.register(child.stdout, selectors.EVENT_READ)
        while True:
            if not selector.select(timeout=0.2):
                yield None
                continue
            chunk = os.read(child.stdout.fileno(), 4096)
            definition.require(bool(chunk), 'SSH_CONNECTION_ENDED')
            pending += chunk
            while b'\n' in pending:
                line, pending = pending.split(b'\n', 1)
                definition.require(len(line) <= 4096, 'GUEST_EVENT_TOO_LARGE')
                event = json.loads(line)
                definition.require(isinstance(event, dict), 'INVALID_GUEST_EVENT')
                yield event
            definition.require(len(pending) <= 4096, 'GUEST_EVENT_TOO_LARGE')


def finish_ssh(child):
    try:
        child.stdin.close()
    except BrokenPipeError:
        pass
    try:
        child.wait(timeout=15)
    except subprocess.TimeoutExpired:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)


def notify():
    address = os.environ.get('NOTIFY_SOCKET')
    if address:
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as channel:
            channel.sendto(b'READY=1', '\0' + address[1:] if address.startswith('@') else address)


def publish(vm, spec):
    for entry in spec['publish']:
        network.api_request('POST', '/v1/vms/' + vm + '/ports', entry)
    save(runtime_path(vm), {'generation': generation(vm), 'state': 'ready'})
    notify()
    audit('service_ready', vm=vm, service=spec['id'], port_count=len(spec['publish']))


def watch(child, vm, spec, action):
    deadline = time.monotonic() + spec['startup_timeout_secs'] + 10
    ready = False
    for event in guest_events(child):
        if not ready and time.monotonic() >= deadline:
            raise RuntimeError('GUEST_START_TIMEOUT')
        if event is None:
            continue
        if event['event'] == 'prepared' and action == 'prepare':
            save(runtime_path(vm), {'generation': generation(vm), 'state': 'prepared'})
            audit('files_prepared', vm=vm, file_count=len(spec['files']))
            return
        if event['event'] == 'ready':
            definition.require(not ready, 'DUPLICATE_GUEST_READY')
            publish(vm, spec)
            ready = True
        if event['event'] in ('failed', 'stopped'):
            category = guest_category(event.get('category'))
            code = event.get('exit_code')
            audit('guest_execution', 'failure', vm=vm, category=category,
                  exit_code=code if type(code) is int else None)
            raise RuntimeError('GUEST_' + category)


def guest_category(value):
    allowed = ('SERVICE_EXITED', 'READINESS_TIMEOUT', 'READY_PORT_ALREADY_IN_USE',
               'CONTROL_DISCONNECTED', 'UNTRUSTED_GUEST_DIRECTORY', 'FileNotFoundError',
               'PermissionError', 'BlockingIOError', 'NotADirectoryError', 'OSError',
               'InterruptedError')
    return value if value in allowed else 'INVALID_GUEST_ERROR'


def cleanup_vm(vm):
    if not runtime_path(vm).exists():
        return
    mode = 'absent'
    if inspect(vm) is not None:
        definition.require(read(runtime_path(vm))['generation'] == generation(vm),
                           'CLEANUP_INSTANCE_OWNERSHIP_CONFLICT')
        result = network.api_request('DELETE', '/v1/vms/' + vm)
        definition.require(result.get('stopped'), 'VM_CLEANUP_UNCONFIRMED')
        mode = result.get('mode')
    runtime_path(vm).unlink(missing_ok=True)
    audit('vm_cleaned', vm=vm, stop_mode=mode)


def execute(vm, action):
    with lock(vm):
        value = load(vm)
        spec = value['definition']
        identity = ROOT / 'runtime' / (vm + '.identity')
        installer.atomic_file(identity, 0o600, data=value['identity'])
        try:
            ensure_vm(vm, spec)
            wait_ssh(vm, identity, spec['startup_timeout_secs'])
            run_guest(vm, value, action, identity)
        except BaseException:
            cleanup_vm(vm)
            raise
        finally:
            identity.unlink(missing_ok=True)


def run_guest(vm, value, action, identity):
    child = launch(vm, value['definition'], action, identity)
    try:
        watch(child, vm, value['definition'], action)
    finally:
        finish_ssh(child)


def stopped(signum, frame):
    raise InterruptedError('HOST_STOP_REQUESTED')


def run(vm):
    for number in (signal.SIGTERM, signal.SIGINT):
        signal.signal(number, stopped)
    try:
        execute(vm, 'run')
    except InterruptedError:
        audit('service_stopped', vm=vm)


def status(vm):
    value = load(vm)['definition']
    active = systemctl('is-active', unit(vm), check=False).stdout.decode().strip()
    record = read(runtime_path(vm)) if runtime_path(vm).exists() else {}
    machine = inspect(vm)
    config = network.tomllib.loads(network.CONFIG.read_text())
    disk = Path(config['state_dir']) / 'roots' / vm / 'rootfs.ext4'
    print(json.dumps({'vm': vm, 'service': value['id'], 'supervisor': active,
        'state': record.get('state', 'stopped') if machine else 'stopped', 'machine': machine,
        'rootDisk': {'retained': disk.is_file(), 'sizeBytes': disk.stat().st_size if disk.is_file() else 0},
        'ports': network.api_request('GET', '/v1/vms/' + vm + '/ports')['ports'] if machine else []}))


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest='action', required=True)
    install_parser = commands.add_parser('install')
    install_parser.add_argument('definition')
    install_parser.add_argument('--identity', required=True)
    for action in ('setup', 'up'):
        command = commands.add_parser(action)
        command.add_argument('vm', type=definition.identifier)
        command.add_argument('service', type=definition.identifier)
        if action == 'up':
            command.add_argument('--enable', action='store_true')
    for action in ('start', 'stop', 'destroy', 'status', 'run'):
        commands.add_parser(action).add_argument('vm', type=definition.identifier)
    return result


def main():
    options = parser().parse_args()
    initialize()
    if options.action == 'install':
        install(options.definition, options.identity)
    elif options.action in ('setup', 'up'):
        bind(options.vm, options.service)
        if options.action == 'setup':
            execute(options.vm, 'prepare')
        else:
            start(options.vm, options.enable)
    elif options.action == 'start':
        start(options.vm)
    elif options.action in ('stop', 'destroy'):
        load(options.vm)
        systemctl('disable', '--now', unit(options.vm))
        with lock(options.vm):
            cleanup_vm(options.vm)
            if options.action == 'destroy':
                network.api_request('DELETE', '/v1/roots/' + options.vm)
                (ROOT / 'vms' / (options.vm + '.json')).unlink(missing_ok=True)
                audit('vm_destroyed', vm=options.vm)
    elif options.action == 'status':
        status(options.vm)
    elif options.action == 'run':
        run(options.vm)


def start(vm, enable=False):
    load(vm)
    if enable:
        systemctl('enable', unit(vm))
    systemctl('start', unit(vm))
    status(vm)


def failure(error):
    category = str(error) if isinstance(error, (RuntimeError, ValueError)) else type(error).__name__
    if isinstance(error, urllib.error.HTTPError):
        try:
            response = json.load(error)
            category = 'API_' + str(response.get('error', 'REJECTED'))
        except ValueError:
            category = 'API_HTTP_' + str(error.code)
    audit('failed', 'failure', category=category)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        failure(error)
        sys.exit(1)
