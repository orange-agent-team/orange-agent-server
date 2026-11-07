#!/usr/bin/env python3
"""Current submitter metadata recovery; real router/PG, explicit session fixtures."""
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
env=os.environ.copy()
for flag in ['ORANGE_REAL_WORKER_PROFILE','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_JOB_QUERY_TESTS']:
    env.pop(flag,None)
env['ORANGE_JOB_QUERY_TESTS']='1'
subprocess.run([os.sys.executable,'scripts/check-worker-session.py'],cwd=ROOT,env=env,check=True)
print('PASS own Job metadata, multi-page cursor, foreign submitter and deletion/reauthorization boundaries; no content-read or physical execution certification')
