#!/usr/bin/env python3
"""Qualify the current committed macOS local inference distribution with isolated builds and an offline install."""
import argparse
from local_distribution_profiles import PROFILES
from pathlib import Path
import json
import subprocess
import sys

parser=argparse.ArgumentParser()
parser.add_argument('--profile', choices=tuple(PROFILES), default='embedding')
profile=parser.parse_args().profile
root=Path(__file__).resolve().parents[1]
python=Path.home()/'.local/bin/python3.12'
if not python.exists():
    import shutil
    found=shutil.which('python3.12')
    if not found:
        raise SystemExit('Python 3.12 required for the macOS distribution')
    python=Path(found)
commit=subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip()
wheelhouse=root/'.local'/(profile+'-wheelhouse')
if not wheelhouse.is_dir():
    raise SystemExit('Prepare the locked wheelhouse for '+profile+' first')
subprocess.run([sys.executable,root/'scripts/build-embedding-release.py','--commit',commit,'--profile',profile],cwd=root,check=True)
subprocess.run([sys.executable,root/'scripts/package-embedding-backend.py'],cwd=root,check=True)
wheel_dir=root/'.local/python-dist/reproducible'
build=json.loads((wheel_dir/'build.json').read_text())
subprocess.run([python,root/'scripts/package-local-embedding-release.py','--profile',profile,'--native',root/'.local'/(profile+'-release')/commit,
                '--wheelhouse',wheelhouse,'--backend-wheel',wheel_dir/build['wheel'],'--uv-cache',root/'.local/uv-cache',
                '--model-cache',root/'.local'/(profile+'-model'),'--output-dir',root/'.local'/(profile+'-distributions')],cwd=root,check=True)
