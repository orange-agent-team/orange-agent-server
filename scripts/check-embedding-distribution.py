#!/usr/bin/env python3
"""Qualify the current committed macOS E5 distribution with isolated builds and an offline install."""
from pathlib import Path
import json
import subprocess
import sys

root=Path(__file__).resolve().parents[1]
python=Path.home()/'.local/bin/python3.12'
if not python.exists():
    import shutil
    found=shutil.which('python3.12')
    if not found:
        raise SystemExit('Python 3.12 required for the macOS distribution')
    python=Path(found)
commit=subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip()
wheelhouse=root/'.local/embedding-wheelhouse'
if not wheelhouse.is_dir():
    raise SystemExit('Prepare the locked wheelhouse following docs/local-embedding-distribution.md')
subprocess.run([sys.executable,root/'scripts/build-embedding-release.py','--commit',commit],cwd=root,check=True)
subprocess.run([sys.executable,root/'scripts/package-embedding-backend.py'],cwd=root,check=True)
wheel_dir=root/'.local/python-dist/reproducible'
build=json.loads((wheel_dir/'build.json').read_text())
subprocess.run([python,root/'scripts/package-local-embedding-release.py','--native',root/'.local/embedding-release'/commit,
                '--wheelhouse',wheelhouse,'--backend-wheel',wheel_dir/build['wheel'],'--uv-cache',root/'.local/uv-cache',
                '--model-cache',root/'.local/embedding-model','--output-dir',root/'.local/embedding-distributions'],cwd=root,check=True)
