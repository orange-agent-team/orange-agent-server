#!/usr/bin/env python3
"""Real original-submission withdrawal: no synthetic Job/refund/cleanup completion."""
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
env=os.environ.copy()
for flag in ['ORANGE_REAL_WORKER_PROFILE','ORANGE_WEB_COMPUTE_TESTS','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_JOB_QUERY_TESTS','ORANGE_WITHDRAW_TESTS']:
    env.pop(flag,None)
env['ORANGE_WITHDRAW_TESTS']='1'
subprocess.run([os.sys.executable,ROOT/'scripts/check-artifact-contract.py'],cwd=ROOT,env=env,check=True)
subprocess.run([os.sys.executable,ROOT/'scripts/check-worker-session.py'],cwd=ROOT,env=env,check=True)
print('PASS immutable pre-admission barrier, current authority, original receipt and serializable races; existing cancellation retains separate resource/financial completion')
