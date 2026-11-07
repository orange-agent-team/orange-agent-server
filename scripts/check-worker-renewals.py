#!/usr/bin/env python3
"""Real local renewal authority tests; no model substitutes represented as hardware."""
import os
from pathlib import Path
import subprocess
ROOT = Path(__file__).resolve().parents[1]
env = os.environ.copy()
env.pop('ORANGE_WEB_COMPUTE_TESTS',None)
env.pop('ORANGE_WITHDRAW_TESTS',None)
env.pop('ORANGE_REAL_WORKER_PROFILE', None)
env['ORANGE_WORKER_RENEWAL_TESTS'] = '1'
subprocess.run([os.sys.executable, 'scripts/check-worker-session.py'], cwd=ROOT, env=env, check=True)
subprocess.run([os.sys.executable, 'scripts/check-broker.py'], cwd=ROOT, env=env, check=True)
print('PASS request-bound cloud renewal, replay/expiry/deletion/session fencing and durable local uncertainty; initial catalog/price/device fixtures, no output or settlement')
