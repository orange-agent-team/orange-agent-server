#!/usr/bin/env python3
"""Actual SQLite/Broker plus local mTLS/PostgreSQL receiver; no model/OS-probe certification."""
from pathlib import Path
import subprocess
import sys
ROOT=Path(__file__).resolve().parents[1]
for script in ['check-broker.py','check-worker-session.py']:
    subprocess.run([sys.executable,str(ROOT/'scripts'/script)],cwd=ROOT,check=True)
print('PASS E08 durable receive/reserve/ACK/start-confirm and conservative clocks; actual SQLite/Broker/mTLS/PG, explicit identity/device/profile fixtures, no engine launch/cleanup/settlement')
