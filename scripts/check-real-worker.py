#!/usr/bin/env python3
"""Actual pinned CPU model on a real mTLS/PG/object Worker path. No cloud completion or billing."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
ROOT=Path(__file__).resolve().parents[1]
lock=json.loads((ROOT/'config/local-model.lock.json').read_text())
build=json.loads((ROOT/'.local/model-runtime/build.json').read_text())
backend=ROOT/'.local/model-runtime/build/bin/llama-server'
model=ROOT/'.local/model-runtime'/ (lock['model']['sha256']+'.gguf')
def digest(path):
    h=hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda:f.read(1024*1024),b''):h.update(block)
    return h.hexdigest()
if build['gpu_enabled'] or build['backend_commit']!=lock['backend']['commit'] or digest(backend)!=build['executable_sha256']:raise SystemExit('Verified fixed CPU backend required')
if model.stat().st_size!=lock['model']['bytes'] or digest(model)!=lock['model']['sha256']:raise SystemExit('Actual full pinned model required; no substitute')
with tempfile.TemporaryDirectory(prefix='oa-worker-profile-') as directory:
    private=Path(directory);private.chmod(0o700);profile=private/'profile.json'
    profile.write_text(json.dumps({'executable':str(backend),'executable_sha256':build['executable_sha256'],'model':str(model),'model_sha256':lock['model']['sha256'],'model_bytes':lock['model']['bytes']}));profile.chmod(0o600)
    env=os.environ.copy();env['ORANGE_REAL_WORKER_PROFILE']=str(profile)
    subprocess.run([os.sys.executable,str(ROOT/'scripts/check-worker-session.py')],cwd=ROOT,env=env,check=True)
if os.environ.get('ORANGE_PRICED_WORKER_TESTS'):
    print('PASS actual server-owned fixed model counting, quote snapshots and development-credit settlement; candidate/admission fixtures except explicit full-process case, no external payments or cloud resource cleanup')
elif os.environ.get('ORANGE_WORKER_HOST_TESTS'):
    print('PASS actual Worker/User API/Gateway processes and pinned model output delivery/recovery; identity/enrollment/catalog/price fixtures; no settlement, cloud resource release or production certification')
else:
    print('PASS real pinned CPU Worker inference/cancellation and owned process exit via actual objects/mTLS/PG; catalog/price/initial OS/device admission fixtures, no cloud output commit, GPU or settlement certification')
