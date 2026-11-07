#!/usr/bin/env python3
"""Actual built development Worker, OS identity/CPU admission and owned shutdown."""
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
env=os.environ.copy()
env['ORANGE_WORKER_HOST_TESTS']='1'
subprocess.run([os.sys.executable,'scripts/check-real-worker.py'],cwd=ROOT,env=env,check=True)
subprocess.run([os.sys.executable,'scripts/check-broker.py'],cwd=ROOT,env=env,check=True)
print('PASS actual development Worker binary, current-user identity, private admission, model execution and graceful exit; catalog/price/enrollment fixtures, no output delivery or production installation')
