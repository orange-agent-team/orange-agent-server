#!/usr/bin/env python3
"""Actual CPU closure/redispatch, plus explicitly separate protocol fixtures."""
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
env=os.environ.copy()
for name in ['ORANGE_REAL_WORKER_PROFILE','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_JOB_QUERY_TESTS']:
    env.pop(name,None)
env['ORANGE_CLEANUP_PROTOCOL_TESTS']='1'
subprocess.run([os.sys.executable,'scripts/check-worker-session.py'],cwd=ROOT,env=env,check=True)
env.pop('ORANGE_CLEANUP_PROTOCOL_TESTS',None)
env['ORANGE_CLEANUP_REAL_TESTS']='1';env['ORANGE_PRICED_WORKER_TESTS']='1'
subprocess.run([os.sys.executable,'scripts/check-real-worker.py'],cwd=ROOT,env=env,check=True)
subprocess.run([os.sys.executable,'scripts/check-broker.py'],cwd=ROOT,env=env,check=True)
print('PASS actual CPU cleanup/second priced Job, unknown delivery release, receipt recovery and reference-safe derivative reclaim; protocol/price/enrollment admission fixtures, no GPU/TEE/production certification')
