#!/usr/bin/env python3
"""Ephemeral unprivileged guest preparation and SSH-lifetime process supervision."""
import base64
import fcntl
import json
import os
from pathlib import Path
import selectors
import signal
import socket
import subprocess
import sys
import tempfile
import time


def emit(event, **values):
    print(json.dumps({'event': event, **values}), flush=True)


def interrupted(signum, frame):
    raise InterruptedError('SSH_SESSION_INTERRUPTED')


def directory(path):
    if path.resolve() != path:
        raise RuntimeError('UNTRUSTED_GUEST_DIRECTORY')
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    if path.resolve() != path or path.stat().st_uid != os.getuid():
        raise RuntimeError('UNTRUSTED_GUEST_DIRECTORY')


def prepare(spec):
    root = Path.home() / '.local/share/firecracker-services' / spec['id']
    directory(root)
    for item in spec['files']:
        path = root / item['target']
        directory(path.parent)
        fd, temporary = tempfile.mkstemp(dir=path.parent)
        try:
            with os.fdopen(fd, 'wb') as output:
                output.write(base64.b64decode(item['content'], validate=True))
                os.fchmod(output.fileno(), item['mode'])
            os.replace(temporary, path)
        finally:
            Path(temporary).unlink(missing_ok=True)
    return root


def port_open(port):
    try:
        with socket.create_connection(('127.0.0.1', port), timeout=0.1):
            return True
    except OSError:
        return False


def terminate(child):
    try:
        os.killpg(child.pid, signal.SIGTERM)
        child.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait(timeout=5)
    except ProcessLookupError:
        pass


def log_chunk(root, chunk):
    path = root / 'service.log'
    if path.exists() and path.stat().st_size + len(chunk) > 5 * 1024 * 1024:
        os.replace(path, root / 'service.log.1')
    # Never follow a log symlink into a mounted application directory.
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'ab') as output:
        output.write(chunk)


def supervise(child, spec, root):
    selector = selectors.DefaultSelector()
    selector.register(sys.stdin, selectors.EVENT_READ, 'control')
    selector.register(child.stdout, selectors.EVENT_READ, 'output')
    deadline, ready = time.monotonic() + spec['startup_timeout_secs'], False
    while child.poll() is None:
        for key, _ in selector.select(timeout=0.1):
            chunk = os.read(key.fd, 65536)
            if key.data == 'control' and not chunk:
                return 'CONTROL_DISCONNECTED'
            if key.data == 'output':
                if chunk:
                    log_chunk(root, chunk)
                else:
                    selector.unregister(key.fileobj)
        if not ready and port_open(spec['ready_port']):
            ready = True
            emit('ready')
        if not ready and time.monotonic() >= deadline:
            return 'READINESS_TIMEOUT'
    return 'SERVICE_EXITED'


def execute(spec, root):
    expand = lambda value: value.replace('{service_dir}', str(root))
    if port_open(spec['ready_port']):
        raise RuntimeError('READY_PORT_ALREADY_IN_USE')
    environment = dict(os.environ)
    environment.update({k: expand(v) for k, v in spec['environment'].items()})
    child = subprocess.Popen([expand(v) for v in spec['command']],
        cwd=expand(spec['working_directory']), env=environment, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        category = supervise(child, spec, root)
        emit('stopped', category=category, exit_code=child.poll())
    finally:
        terminate(child)


def main():
    os.umask(0o077)
    for number in (signal.SIGHUP, signal.SIGTERM, signal.SIGINT):
        signal.signal(number, interrupted)
    payload = json.loads(sys.stdin.buffer.readline(48 * 1024 * 1024))
    spec = payload['definition']
    root = Path.home() / '.local/share/firecracker-services' / spec['id']
    directory(root)
    fd = os.open(root / 'run.lock', os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        root = prepare(spec)
        emit('prepared')
        if payload['action'] == 'run':
            execute(spec, root)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # Application stderr and payloads stay in private guest files.
        emit('failed', category=str(error) if isinstance(error, RuntimeError) else type(error).__name__)
        sys.exit(1)
