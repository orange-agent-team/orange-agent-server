#!/usr/bin/env python3
"""TK-03: actual capability filesystem, limits, approvals and SQLite recovery."""
import os
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
env = os.environ.copy()
env['PATH'] = str(cargo.parent) + os.pathsep + env.get('PATH', '')
result = subprocess.run([str(cargo), 'test', '-p', 'orange-agent-runtime', 'file_search', '--locked'], cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
print(result.stdout, end='')
result.check_returncode()
if not re.search(r'test result: ok\. [1-9][0-9]* passed; 0 failed;', result.stdout):
    raise SystemExit('No actual file search acceptance cases ran')
print('PASS actual filename search with restricted globs, links/protected paths, entry/depth/output limits, current grant and durable approval/recovery; scripted model only, no new desktop release or other-OS certification')
