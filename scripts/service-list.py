#!/usr/bin/env python3
"""Read-only VM inventory from one management API snapshot and local service metadata."""
import argparse
from importlib.util import module_from_spec, spec_from_file_location
import json
import os
from pathlib import Path
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
SERVICES = Path('/var/lib/mindos-firecracker/services')
OPERATION = uuid.uuid4().hex
STARTED = time.monotonic()


def read(path):
    network.installer.trusted(path)
    return json.loads(path.read_text())


def bindings():
    return {path.stem: read(path)['service'] for path in (SERVICES / 'vms').glob('*.json')}


def stopped_machines(config, names):
    result = {name: {'id': name, 'profile': '-', 'state': 'stopped'} for name in names}
    for path in (Path(config['state_dir']) / 'roots').glob('*/manifest.json'):
        if path.parent.name.endswith('.initializing'):
            continue
        manifest = read(path)
        spec = manifest['spec']
        result[spec['id']] = {**spec, 'state': 'stopped', 'rootfsPersistent': True}
    return result


def supervisor_states(names):
    if not names:
        return {}
    units = ['firecracker-workload@' + name + '.service' for name in sorted(names)]
    result = subprocess.run(['/usr/bin/systemctl', 'show', *units, '-p', 'Id,ActiveState'],
                            capture_output=True, text=True, timeout=5)
    if result.returncode:
        raise RuntimeError('SYSTEMD_LIST_FAILED')
    states = {}
    for block in result.stdout.strip().split('\n\n'):
        fields = dict(line.split('=', 1) for line in block.splitlines() if '=' in line)
        if 'Id' in fields:
            states[fields['Id']] = fields.get('ActiveState', 'unknown')
    return states


def connection(machine, config):
    profile = config['profiles'].get(machine['profile'], {})
    if machine['state'] != 'vmm-running' or not profile.get('network_slots'):
        return None, None
    try:
        return network.connection(machine['id'])
    except FileNotFoundError:
        # A VM may stop after the API snapshot; do not manufacture a guest address.
        return None, None


def service_state(name, generation, states):
    if name is None:
        return None
    unit = 'firecracker-workload@' + name + '.service'
    active = states.get(unit, 'unknown')
    record = SERVICES / 'runtime' / (name + '.json')
    if active == 'active' and generation and record.is_file():
        state = read(record)
        if state.get('generation') == generation:
            return state.get('state', 'unknown')
    return active


def row(machine, service, states, config):
    address, generation = connection(machine, config)
    return {'id': machine['id'], 'profile': machine['profile'], 'service': service,
            'state': machine['state'], 'serviceState': service_state(
                machine['id'] if service else None, generation, states),
            'ip': address, 'rootfsPersistent': machine.get('rootfsPersistent',
                config['profiles'].get(machine['profile'], {}).get('rootfs_persistent', False)),
            'ageSecs': machine.get('ageSecs'), 'ports': machine.get('ports', [])}


def inventory(include_stopped=False):
    started = time.monotonic()
    live = network.api_request('GET', '/v1/vms')['vms']
    config = network.tomllib.loads(network.CONFIG.read_text())
    services = bindings()
    machines = stopped_machines(config, services) if include_stopped else {}
    machines.update({value['id']: value for value in live})
    states = supervisor_states(set(machines) & services.keys())
    values = [row(machines[name], services.get(name), states, config) for name in sorted(machines)]
    print(json.dumps({'operation': 'vm_list', 'operation_id': OPERATION,
        'stage': 'snapshot', 'result': 'success', 'count': len(values), 'api_calls': 1,
        'elapsed_ms': round((time.monotonic() - started) * 1000)}), file=sys.stderr)
    return values


def port_text(ports):
    return ','.join(f'{p["listen"]}:{p["host_port"]}->{p["guest_port"]}/{p["protocol"]}'
                    for p in ports) or '-'


def table(values, include_stopped):
    if not values:
        print('未找到虚拟机。' if include_stopped else '当前没有运行的虚拟机。')
        return
    rows = [['VM', 'PROFILE', 'SERVICE', 'VM_STATE', 'SERVICE_STATE', 'IP', 'ROOT', 'PORTS']]
    for value in values:
        rows.append([value['id'], value['profile'], value['service'] or '-', value['state'],
                     value['serviceState'] or '-', value['ip'] or '-',
                     'persistent' if value['rootfsPersistent'] else 'ephemeral', port_text(value['ports'])])
    widths = [max(len(row[index]) for row in rows) for index in range(len(rows[0]))]
    for row in rows:
        print('  '.join(value.ljust(width) for value, width in zip(row, widths)).rstrip())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--all', action='store_true', help='Include stopped services and retained roots')
    parser.add_argument('--json', action='store_true', help='Print machine-readable JSON')
    options = parser.parse_args()
    if os.geteuid() != 0:
        raise RuntimeError('LIST_REQUIRES_ROOT_FOR_PRIVATE_MANAGEMENT_CONFIG')
    values = inventory(options.all)
    if options.json:
        print(json.dumps({'vms': values}))
    else:
        table(values, options.all)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        category = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        if isinstance(error, urllib.error.HTTPError):
            category = 'MANAGEMENT_HTTP_' + str(error.code)
        print(json.dumps({'operation': 'vm_list', 'operation_id': OPERATION,
                          'stage': 'read', 'result': 'failure', 'category': category,
                          'elapsed_ms': round((time.monotonic() - STARTED) * 1000)}), file=sys.stderr)
        sys.exit(1)
