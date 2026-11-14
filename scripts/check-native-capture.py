#!/usr/bin/env python3
"""Owned actual Apple sandbox capture plus fixed-old-format compatibility.

Model protocol is an explicit substitute. This does not certify manual pickers,
other filesystems/OSes or production signing. Native labels are never removed.
"""
import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--legacy-runtime', type=Path, required=True)
args = parser.parse_args()
for command in [
    ['scripts/check-publication-metadata.py'],
    ['scripts/check-publication-metadata.py', '--bundled-source'],
    ['scripts/check-file-editing.py', '--sandboxed'],
    ['scripts/check-file-editing.py', '--legacy-runtime', str(args.legacy_runtime.resolve())],
]:
    subprocess.run([sys.executable, *command], cwd=ROOT, check=True, timeout=600)
print('PASS actual signed sandbox metadata/capture/continuous-edit/reopen/unknown preservation and real schema16 approved-edit migration plus nonmutating old-version rejection; scope excludes real inference/manual picker and broader release certification')
