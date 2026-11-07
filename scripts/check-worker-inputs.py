#!/usr/bin/env python3
"""Real private objects and current-lease provider delivery, without model execution."""
from pathlib import Path
import subprocess
import sys
ROOT=Path(__file__).resolve().parents[1]
for script in ['check-artifacts.py','check-worker-session.py']:
    subprocess.run([sys.executable,str(ROOT/'scripts'/script)],cwd=ROOT,check=True)
print('PASS actual user/Job input binding, explicit budget authority, immutable pin and mTLS provider read; price/catalog/session/OS/device fixtures remain, no model execution or settlement certification')
