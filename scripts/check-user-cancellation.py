#!/usr/bin/env python3
"""Authenticated cancellation: real DB protocol cases, then actual owned CPU."""
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
env=os.environ.copy()
env.pop('ORANGE_WEB_COMPUTE_TESTS',None)
for name in ['ORANGE_REAL_WORKER_PROFILE','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_JOB_QUERY_TESTS']:
    env.pop(name,None)
env['ORANGE_CANCEL_PROTOCOL_TESTS']='1'
subprocess.run([os.sys.executable,'scripts/check-worker-session.py'],cwd=ROOT,env=env,check=True)
env.pop('ORANGE_CANCEL_PROTOCOL_TESTS')
env['ORANGE_CANCEL_REAL_TESTS']='1';env['ORANGE_PRICED_WORKER_TESTS']='1'
subprocess.run([os.sys.executable,'scripts/check-real-worker.py'],cwd=ROOT,env=env,check=True)
print('PASS current-user cancellation, zero duplicate release, actual CPU exit and late-cancel delivery retention; identity/admission fixtures, no GPU or payment certification')
