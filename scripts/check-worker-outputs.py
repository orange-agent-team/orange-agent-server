#!/usr/bin/env python3
"""Actual output byte publication and current-user/source authorization tests."""
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
env=os.environ.copy()
env.pop('ORANGE_REAL_WORKER_PROFILE',None)
env['ORANGE_WORKER_OUTPUT_TESTS']='1'
subprocess.run([os.sys.executable,'scripts/check-artifact-contract.py'],cwd=ROOT,env=env,check=True)
subprocess.run([os.sys.executable,'scripts/check-worker-session.py'],cwd=ROOT,env=env,check=True)
print('PASS actual output bytes/commit, replay, source permissions, corruption, expiry and user result retrieval; catalog/price/candidate fixtures; no settlement or cloud resource release')
