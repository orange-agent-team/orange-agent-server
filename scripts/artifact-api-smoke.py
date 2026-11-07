#!/usr/bin/env python3
"""Start the actual dev binary on a private ephemeral root/port; probe auth and stop it."""
import json
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
ROOT=Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='orange-artifact-api-') as directory:
    private=Path(directory);private.chmod(0o700)
    env=os.environ.copy()
    env.update(ORANGE_ENV='development',DATABASE_URL=(ROOT/'.local/database-url').read_text().strip(),ORANGE_USER_CONFIG_FILE=str(ROOT/'.local/identity/user-config.json'),ORANGE_USER_BIND='127.0.0.1:0',ORANGE_ARTIFACT_ROOT=str(private),ORANGE_ARTIFACT_BYTE_LIMIT='1048576',ORANGE_ARTIFACT_RECORD_LIMIT='100')
    process=subprocess.Popen([str(ROOT/'target/debug/orange-user-api')],cwd=ROOT,env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,text=True)
    try:
        selector=selectors.DefaultSelector();selector.register(process.stderr,selectors.EVENT_READ)
        deadline=time.monotonic()+30;address=None
        while time.monotonic()<deadline:
            if process.poll() is not None:raise RuntimeError('Artifact API exited before listening')
            for key,_ in selector.select(timeout=1):
                line=key.fileobj.readline()
                match=re.search(r'listening on (127\.0\.0\.1:\d+)',line)
                if match:address=match.group(1);break
            if address:break
        selector.close()
        if not address:raise RuntimeError('Artifact API listen deadline')
        url='http://'+address+'/api/v1/owners/probe/uploads/probe'
        try:urllib.request.urlopen(url,timeout=5)
        except urllib.error.HTTPError as e:
            assert e.code==401 and json.loads(e.read())['code']=='UNAUTHENTICATED'
        else:raise AssertionError('No cookie must never access an object')
        assert (private/'.store-id').is_file()
        process.send_signal(signal.SIGINT)
        assert process.wait(timeout=15)==0,'graceful shutdown must succeed'
        print('PASS actual dev API optional artifact root, bound loopback endpoint, denied anonymous request and graceful maintenance/I/O shutdown')
    finally:
        if process.poll() is None:
            process.terminate()
            try:process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill();process.wait(timeout=10)
        process.stderr.close()
