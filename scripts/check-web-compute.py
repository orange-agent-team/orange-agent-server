#!/usr/bin/env python3
"""Actual model/browser path using an explicitly selected, independently built Web checkout."""
import argparse
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--web-repo',required=True,type=Path)
parser.add_argument('--node',default=shutil.which('node'))
args=parser.parse_args()
web=args.web_repo.resolve()
driver=web/'apps/web/scripts/real-compute-driver.mjs'
if not driver.is_file() or not (web/'apps/web/dist/index.html').is_file():raise SystemExit('Explicit built Web compute checkout required')
if not args.node or subprocess.check_output([args.node,'--version'],text=True).strip()!='v22.23.2':raise SystemExit('Pinned Node22.23.2 required')
with socket.socket() as probe:
    if probe.connect_ex(('127.0.0.1',43127))==0:raise SystemExit('Isolated browser port43127 occupied; do not replace an unknown process')
build_env={**os.environ,'PATH':str(Path(args.node).parent)+os.pathsep+os.environ.get('PATH','')}
subprocess.run([args.node,web/'scripts/check.mjs'],cwd=web,env=build_env,check=True)
subprocess.run([sys.executable,web/'scripts/local-tls.py'],cwd=web,check=True)
env=os.environ.copy()
env.pop('ORANGE_WITHDRAW_TESTS',None)
env.pop('ORANGE_QUEUE_EXPIRY_TESTS',None)
for flag in ['ORANGE_REAL_WORKER_PROFILE','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_JOB_QUERY_TESTS','ORANGE_WEB_COMPUTE_TESTS']:
    env.pop(flag,None)
evidence=Path(tempfile.mkdtemp(prefix='web-compute-',dir=ROOT/'.local'));evidence.chmod(0o700)
env['ORANGE_WEB_COMPUTE_EVIDENCE_DIR']=str(evidence)
print('Private generated-test evidence: '+str(evidence.relative_to(ROOT)),flush=True)
env.update(ORANGE_WEB_COMPUTE_TESTS='1',ORANGE_PRICED_WORKER_TESTS='1',ORANGE_WEB_NODE_BINARY=str(Path(args.node).resolve()),ORANGE_WEB_COMPUTE_DRIVER=str(driver))
subprocess.run([sys.executable,ROOT/'scripts/check-real-worker.py'],cwd=ROOT,env=env,check=True)
print('PASS explicit Web checkout with actual owned API/Worker/Counter and Chrome; ephemeral session/price/enrollment fixtures, no payment or production certification')
