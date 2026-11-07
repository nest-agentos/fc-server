#!/usr/bin/env python3
"""Validate and snapshot administrator-owned, application-independent service packages."""
import base64
import hashlib
import ipaddress
import json
from pathlib import Path, PurePosixPath
import re
import stat
import tomllib


def require(condition, category):
    if not condition:
        raise ValueError(category)


def identifier(value):
    require(isinstance(value, str) and re.fullmatch(r'[A-Za-z0-9-]{1,48}', value),
            'INVALID_IDENTIFIER')
    return value


def fields(value, allowed, required=()):
    require(isinstance(value, dict), 'EXPECTED_TABLE')
    require(not value.keys() - set(allowed), 'UNKNOWN_FIELD')
    require(set(required) <= value.keys(), 'MISSING_FIELD')


def relative(value):
    require(isinstance(value, str) and bool(value), 'INVALID_RELATIVE_PATH')
    path = PurePosixPath(value)
    require(value != '.' and not path.is_absolute() and '..' not in path.parts and str(path) == value,
            'INVALID_RELATIVE_PATH')
    require(value not in ('service.log', 'service.log.1', 'run.lock'), 'RESERVED_FILE')
    return value


def strings(values):
    require(isinstance(values, list) and all(isinstance(v, str) and '\0' not in v for v in values),
            'INVALID_STRING_ARRAY')
    return values


def number(value, low, high):
    require(type(value) is int and low <= value <= high, 'NUMBER_OUT_OF_RANGE')
    return value


def publications(values):
    require(isinstance(values, list) and len(values) <= 16, 'INVALID_PUBLICATIONS')
    result = []
    for value in values:
        fields(value, ('listen', 'host_port', 'guest_port', 'protocol'), ('host_port', 'guest_port'))
        result.append({'listen': str(ipaddress.IPv4Address(value.get('listen', '127.0.0.1'))),
                       'host_port': number(value['host_port'], 1, 65535),
                       'guest_port': number(value['guest_port'], 1, 65535),
                       'protocol': value.get('protocol', 'tcp')})
        require(result[-1]['protocol'] in ('tcp', 'udp'), 'INVALID_PROTOCOL')
    require(len({(v['listen'], v['host_port'], v['protocol']) for v in result}) == len(result),
            'DUPLICATE_PUBLICATION')
    return result


def assets(values, parent):
    require(isinstance(values, list) and len(values) <= 64, 'INVALID_FILES')
    result, total = [], 0
    for value in values:
        fields(value, ('source', 'target', 'executable'), ('source', 'target'))
        require(type(value.get('executable', False)) is bool, 'INVALID_EXECUTABLE_FLAG')
        source = parent / relative(value['source'])
        require(source.resolve().is_relative_to(parent.resolve()) and not source.is_symlink(),
                'FILE_OUTSIDE_PACKAGE')
        info = source.stat()
        require(stat.S_ISREG(info.st_mode), 'FILE_NOT_REGULAR')
        total += info.st_size
        require(total <= 32 * 1024 * 1024, 'PACKAGE_TOO_LARGE')
        result.append({'target': relative(value['target']),
                       'mode': 0o700 if value.get('executable', False) else 0o600,
                       'content': base64.b64encode(source.read_bytes()).decode()})
    targets = [PurePosixPath(v['target']) for v in result]
    require(all(not a.is_relative_to(b) and not b.is_relative_to(a)
                for i, a in enumerate(targets) for b in targets[i + 1:]), 'OVERLAPPING_FILES')
    return result


def definition(path):
    value = tomllib.loads(path.read_text())
    fields(value, ('id', 'profile', 'directories', 'volumes', 'command', 'working_directory',
                   'environment', 'files', 'publish', 'ready_port', 'startup_timeout_secs'),
           ('id', 'profile', 'command', 'working_directory', 'ready_port'))
    result = {k: identifier(value[k]) for k in ('id', 'profile')}
    result.update(runtime(value))
    for key in ('directories', 'volumes'):
        result[key] = [identifier(v) for v in strings(value.get(key, []))]
        require(len(set(result[key])) == len(result[key]), 'DUPLICATE_RESOURCE')
    result['files'] = assets(value.get('files', []), path.parent)
    result['publish'] = publications(value.get('publish', []))
    return result


def runtime(value):
    command = strings(value['command'])
    require(bool(command) and bool(command[0]), 'EMPTY_COMMAND')
    working = value['working_directory']
    require(isinstance(working, str) and working.startswith('/') and '\0' not in working,
            'INVALID_WORKING_DIRECTORY')
    environment = value.get('environment', {})
    require(isinstance(environment, dict), 'INVALID_ENVIRONMENT')
    require(all(re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', k) and isinstance(v, str)
                and '\0' not in v for k, v in environment.items()), 'INVALID_ENVIRONMENT')
    return {'command': command, 'working_directory': working, 'environment': environment,
            'ready_port': number(value['ready_port'], 1, 65535),
            'startup_timeout_secs': number(value.get('startup_timeout_secs', 60), 1, 300)}


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()
