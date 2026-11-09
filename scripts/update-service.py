#!/usr/bin/env python3
"""Apply a source package to one managed VM while retaining its persistent root."""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
import tomllib
import uuid


class UpdateError(Exception):
    pass


def toml_value(value):
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, bool):
        return str(value).lower()
    if isinstance(value, int):
        return str(value)
    if isinstance(value, list):
        return '[' + ','.join(toml_value(item) for item in value) + ']'
    if isinstance(value, dict):
        return '{' + ','.join(toml_value(key) + '=' + toml_value(item) for key, item in value.items()) + '}'
    raise UpdateError('SERVICE_UPDATE_UNSUPPORTED_MANIFEST_VALUE')


def relative(value):
    if not isinstance(value, str) or not value or value.startswith('/') or any(p in ('', '.', '..') for p in value.split('/')):
        raise UpdateError('SERVICE_UPDATE_INVALID_FILE_PATH')
    if value in ('run.lock', 'service.log', 'service.log.1'):
        raise UpdateError('SERVICE_UPDATE_RESERVED_FILE')
    return value


def snapshot(manifest, identity, directory):
    try:
        definition = tomllib.loads(manifest.read_text())
    except tomllib.TOMLDecodeError as error:
        raise UpdateError('SERVICE_UPDATE_MANIFEST_INVALID') from error
    environment = definition.get('environment', {})
    if not isinstance(environment, dict) or any(not isinstance(value, str) for value in environment.values()):
        raise UpdateError('SERVICE_UPDATE_ENVIRONMENT_VALUE_MUST_BE_STRING')
    assets = definition.get('files', [])
    if not isinstance(assets, list) or len(assets) > 64:
        raise UpdateError('SERVICE_UPDATE_INVALID_FILES')
    size, targets = 0, set()
    for asset in assets:
        size = copy_asset(asset, manifest.parent, directory, targets, size)
    return finish_snapshot(definition, identity, directory)


def copy_asset(asset, parent, directory, targets, size):
    source, target = relative(asset['source']), relative(asset['target'])
    path = parent / source
    if path.is_symlink() or not path.resolve().is_relative_to(parent.resolve()) or target in targets:
        raise UpdateError('SERVICE_UPDATE_INVALID_FILE_SOURCE')
    if not path.is_file():
        raise UpdateError('SERVICE_UPDATE_FILE_NOT_REGULAR')
    size += path.stat().st_size
    if size > 32 * 1024 * 1024:
        raise UpdateError('SERVICE_UPDATE_PACKAGE_TOO_LARGE')
    destination = directory / 'files' / source
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(path, destination); destination.chmod(0o600)
    asset['source'] = 'files/' + source
    asset['overwrite'] = True; targets.add(target)
    return size


def finish_snapshot(definition, identity, directory):
    if not identity.is_file() or identity.stat().st_size > 65536:
        raise UpdateError('SERVICE_UPDATE_INVALID_IDENTITY')
    key = directory / '.update-identity'
    key.write_bytes(identity.read_bytes()); key.chmod(0o600)
    manifest = directory / '.update-manifest.toml'
    manifest.write_text('\n'.join(toml_value(k) + '=' + toml_value(v) for k, v in definition.items()) + '\n')
    manifest.chmod(0o600)
    return definition, manifest, key


def invoke(server, *args):
    result = subprocess.run([server, *args], capture_output=True, text=True)
    if result.returncode:
        match = re.search(r'\b(?:API_|SERVICE_|INVALID_|PROFILE_|VM_|ROOT_|MANAGED_)[A-Z0-9_]+\b|\bUNKNOWN_PROFILE\b|\bDIRECTORY_NOT_AUTHORIZED\b|\bPROXY_NOT_AUTHORIZED\b', result.stderr)
        raise UpdateError(match.group() if match else 'SERVICE_UPDATE_CLI_EXIT_' + str(result.returncode))
    try:
        return json.loads(result.stdout) if result.stdout.strip() else {}
    except json.JSONDecodeError as error:
        raise UpdateError('SERVICE_UPDATE_INVALID_CLI_RESPONSE') from error


def enabled(vm):
    result = subprocess.run(['/usr/bin/systemctl', 'is-enabled', 'firecracker-workload@' + vm + '.service'], capture_output=True, text=True)
    return result.stdout.strip() in ('enabled', 'enabled-runtime')


def preflight(vm, definition, status):
    if status.get('service') != definition.get('id'):
        raise UpdateError('SERVICE_UPDATE_BINDING_MISMATCH')
    if not status.get('rootDisk', {}).get('retained'):
        raise UpdateError('SERVICE_UPDATE_REQUIRES_PERSISTENT_ROOT')
    machine = status.get('machine')
    if machine and any(machine.get(key) != definition.get(key, []) for key in ('directories', 'volumes')):
        raise UpdateError('SERVICE_UPDATE_STORAGE_CHANGED_USE_ENVIRONMENT_APPLY')
    if machine and machine.get('profile') != definition.get('profile'):
        raise UpdateError('SERVICE_UPDATE_PROFILE_CHANGED_USE_ENVIRONMENT_APPLY')


def update(server, vm, manifest, identity, enable=False):
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9-]{0,47}', vm):
        raise UpdateError('INVALID_VM_ID')
    operation, started, stage = str(uuid.uuid4()), time.monotonic(), 'prepare'
    def report(result, code=None):
        print(json.dumps({'operation': 'service.update', 'operationId': operation, 'vm': vm, 'stage': stage, 'result': result, 'code': code, 'elapsedMs': round((time.monotonic()-started)*1000)}), flush=True)
    try:
        with tempfile.TemporaryDirectory(prefix='firecracker-service-update-') as temporary:
            report('started')
            definition, prepared, key = snapshot(manifest.resolve(), identity, Path(temporary))
            status = invoke(server, 'service', 'status', vm); preflight(vm, definition, status)
            restore_enabled = enable or enabled(vm); report('success')
            stage = 'stop'; report('started'); invoke(server, 'service', 'stop', vm); report('success')
            stage = 'install'; report('started'); invoke(server, 'service', 'install', str(prepared), '--identity', str(key)); report('success')
            stage = 'start'; report('started')
            invoke(server, 'service', 'up', vm, definition['id'], *(['--enable'] if restore_enabled else []))
            status = invoke(server, 'service', 'status', vm)
            if status.get('state') != 'ready' or not status.get('rootDisk', {}).get('retained'):
                raise UpdateError('SERVICE_UPDATE_NOT_READY')
            report('success'); print(json.dumps(status)); return status
    except Exception as error:
        report('failure', str(error) if isinstance(error, UpdateError) else 'SERVICE_UPDATE_' + type(error).__name__.upper())
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('server'); parser.add_argument('vm'); parser.add_argument('manifest', type=Path)
    parser.add_argument('--identity', required=True, type=Path); parser.add_argument('--enable', action='store_true')
    args = parser.parse_args()
    try:
        update(args.server, args.vm, args.manifest, args.identity, args.enable)
    except Exception:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
