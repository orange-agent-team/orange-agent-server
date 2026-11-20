"""Serialize test servers which must use an exact configured OIDC/CSRF origin."""
import atexit
from contextlib import ExitStack
import fcntl
import os
from pathlib import Path
import socket
import stat
import tempfile
import time

def reserve_origins(*ports):
    # The same user may run a candidate checkout and main on the same machine.
    # Kernel advisory locks disappear automatically after a verifier process exits.
    handles=ExitStack()
    lock_deadline=time.monotonic()+1200
    try:
        for port in sorted(set(ports)):
            if port not in (43112,43122,43127,43128):
                raise ValueError('Unknown development HTTPS origin')
            name=Path(tempfile.gettempdir())/f'orange-origin-{os.getuid()}-{port}.lock'
            fd=os.open(name,os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW,0o600)
            handle=handles.enter_context(os.fdopen(fd,'r+b'))
            metadata=os.fstat(fd)
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid!=os.getuid() or metadata.st_mode&0o077:
                raise SystemExit('Invalid private test origin lock')
            while True:
                if time.monotonic()>=lock_deadline:raise SystemExit(f'Development origin {port} remains occupied')
                try:
                    fcntl.flock(handle,fcntl.LOCK_EX|fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    time.sleep(0.25)
            # Older checkout or an operator-run process may not participate in
            # the lock. Wait a bounded time; never replace an unknown listener.
            deadline=time.monotonic()+120
            while True:
                with socket.socket() as probe:
                    probe.settimeout(0.5)
                    if probe.connect_ex(('127.0.0.1',port))!=0:break
                if time.monotonic()>=deadline:raise SystemExit(f'Existing listener still owns origin {port}')
                time.sleep(0.5)
    except BaseException:
        handles.close()
        raise
    atexit.register(handles.close)
