#!/usr/bin/env python3
"""Bounded macOS process-ownership experiment, not an installed product service.

Only a fresh UUID-labelled current-user job is registered. Its own fixture parent
kills itself; this controller never sends a signal to a PID from a file. Finally
boot out exactly the job created here. No login/startup directory is modified.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import signal
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def private_write(path, data):
    fd = os.open(str(path), os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def fixture(mode, directory, nonce):
    if mode == 'child':
        # This stdin is the parent's locked lease description, intentionally held
        # until process exit. The child cannot release it before its 25s watchdog.
        stat = os.fstat(0)
        fcntl.flock(0, fcntl.LOCK_EX | fcntl.LOCK_NB)
        private_write(directory/'child.json', json.dumps({'pid': os.getpid(), 'pgrp': os.getpgrp(),
            'lease_inode': stat.st_ino, 'lease_device': stat.st_dev,
            'nonce_digest': hashlib.sha256(nonce.encode()).hexdigest()}).encode())
        time.sleep(25)
        return
    fd = os.open(str(directory/'lease'), os.O_RDWR | os.O_NOFOLLOW)
    with os.fdopen(fd, 'r+b') as lease:
        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), 'child', str(directory), nonce], stdin=lease)
        private_write(directory/'parent.json', json.dumps({'pid': os.getpid(), 'pgrp': os.getpgrp(), 'child_pid': child.pid}).encode())
        limit = time.monotonic() + 18
        while time.monotonic() < limit:
            marker = directory/'crash-request'
            if marker.is_file() and marker.read_text() == nonce:
                os.kill(os.getpid(), signal.SIGKILL)  # Only this fixture process itself.
            time.sleep(.02)
        # Bounded fallback if the controller is interrupted before requesting crash.
        child.terminate()
        child.wait(timeout=5)


def probe():
    if sys.platform != 'darwin':
        raise SystemExit('This experiment requires actual macOS launchd')
    directory = ROOT/'.local/launchd-probes'/uuid.uuid4().hex
    directory.mkdir(parents=True, mode=0o700)
    directory.chmod(0o700)
    label = 'com.orange-agent.test.cleanup.' + uuid.uuid4().hex
    domain = 'gui/' + str(os.getuid())
    target = domain + '/' + label
    nonce = uuid.uuid4().hex + uuid.uuid4().hex
    for name in ['lease', 'stdout.log', 'stderr.log']:
        private_write(directory/name, b'')
    spec = {'Label': label, 'ProgramArguments': [sys.executable, str(Path(__file__).resolve()), 'parent', str(directory), nonce],
            'RunAtLoad': True, 'KeepAlive': False, 'AbandonProcessGroup': False, 'ExitTimeOut': 3,
            'ProcessType': 'Background', 'Umask': 0o077, 'WorkingDirectory': str(directory),
            'StandardOutPath': str(directory/'stdout.log'), 'StandardErrorPath': str(directory/'stderr.log'),
            'EnvironmentVariables': {'PYTHONDONTWRITEBYTECODE': '1'}}
    private_write(directory/'job.plist', plistlib.dumps(spec))
    created = False
    report = {'kind': 'actual-macos-launchd-owned-process-fixture', 'directory': str(directory),
              'label': label, 'registered': False, 'removed': False, 'cleanup_verified': False,
              'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    def launchctl(*arguments):
        return subprocess.run(['/bin/launchctl', *arguments], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
    try:
        if launchctl('print', target).returncode == 0:
            raise RuntimeError('Refuse to adopt any preexisting job')
        started = launchctl('bootstrap', domain, str(directory/'job.plist'))
        report['bootstrap_returncode'] = started.returncode
        if started.returncode:
            report['bootstrap_error'] = started.stderr.decode(errors='replace')[:1000]
            raise RuntimeError('Current-user temporary job registration unavailable; no fallback to a privileged domain')
        created = True
        report['registered'] = True
        deadline = time.monotonic() + 8
        while not (directory/'child.json').is_file() or not (directory/'parent.json').is_file():
            if time.monotonic() >= deadline:
                raise RuntimeError('Owned fixture did not become ready within8seconds')
            time.sleep(.04)
        parent = json.loads((directory/'parent.json').read_text())
        child = json.loads((directory/'child.json').read_text())
        if parent['pgrp'] != child['pgrp'] or child['pid'] != parent['child_pid']:
            raise RuntimeError('Fixture process group or child relationship mismatch')
        if child['nonce_digest'] != hashlib.sha256(nonce.encode()).hexdigest():
            raise RuntimeError('Fixture nonce mismatch')
        with (directory/'lease').open('r+b') as lease:
            lease_stat = os.fstat(lease.fileno())
            if (child['lease_inode'], child['lease_device']) != (lease_stat.st_ino, lease_stat.st_dev):
                raise RuntimeError('Child did not inherit the exact test lease')
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise RuntimeError('Fixture did not hold the lease before its crash')
            private_write(directory/'crash-request', nonce.encode())
            began = time.monotonic()
            deadline = began + 5
            while True:
                try:
                    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    if time.monotonic() >= deadline:
                        raise RuntimeError('Lease still held after helper crash; cleanup not verified')
                    time.sleep(.04)
            report['cleanup_latency_seconds'] = round(time.monotonic() - began, 3)
            report['cleanup_verified'] = True
    finally:
        if created:
            try:
                removed = launchctl('bootout', target)
                report['bootout_returncode'] = removed.returncode
                after = launchctl('print', target)
                report['post_bootout_returncode'] = after.returncode
                report['removed'] = removed.returncode == 0 and after.returncode != 0
            except (OSError, subprocess.TimeoutExpired) as error:
                report['removal_error_kind'] = type(error).__name__
        private_write(directory/'report.json', (json.dumps(report, indent=2)+'\n').encode())
        print(json.dumps({k: v for k, v in report.items() if k != 'bootstrap_error'}))
    if not report['cleanup_verified'] or not report['removed']:
        raise RuntimeError('Owned process cleanup or temporary job removal was not verified')


if __name__ == '__main__':
    if len(sys.argv) == 4 and sys.argv[1] in ['parent', 'child']:
        fixture(sys.argv[1], Path(sys.argv[2]), sys.argv[3])
    elif len(sys.argv) == 1:
        probe()
    else:
        raise SystemExit('Invalid fixture invocation')
