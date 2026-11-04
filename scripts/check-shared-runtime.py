#!/usr/bin/env python3
"""Reproduce fixed source artifact and test independent consumer integrity rejection."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser();parser.add_argument('--desktop-repo',required=True);args=parser.parse_args()
desktop=Path(args.desktop_repo).resolve();lock=json.loads((desktop/'config/runtime.lock.json').read_text())
output=ROOT/'.local/shared-runtime'/('verify-'+lock['origin_commit']+'.tar.gz')
subprocess.run([sys.executable,str(ROOT/'scripts/export-shared-runtime.py'),'--commit',lock['origin_commit'],'--output',str(output),'--verify'],cwd=ROOT,check=True)
if output.stat().st_size!=lock['archive_bytes'] or hashlib.sha256(output.read_bytes()).hexdigest()!=lock['archive_sha256']:
    raise SystemExit('Reproduced source release differs from fixed consumer artifact')
with tempfile.TemporaryDirectory(prefix='consumer-check-',dir=ROOT/'.local/shared-runtime') as directory:
    isolated=Path(directory);(isolated/'scripts').mkdir();(isolated/'config').mkdir()
    shutil.copyfile(desktop/'scripts/import-runtime.py',isolated/'scripts/import-runtime.py')
    shutil.copyfile(desktop/'config/runtime.lock.json',isolated/'config/runtime.lock.json')
    script=isolated/'scripts/import-runtime.py'
    subprocess.run([sys.executable,str(script),'--archive',str(output)],cwd=isolated,check=True)
    subprocess.run([sys.executable,str(script),'--verify'],cwd=isolated,check=True)
    def rejected(*arguments):
        result=subprocess.run([sys.executable,str(script),*arguments],cwd=isolated,capture_output=True)
        if result.returncode==0:raise SystemExit('Corruption was accepted')
    corrupt=isolated/'corrupt.tar.gz';payload=bytearray(output.read_bytes());payload[len(payload)//2]^=1;corrupt.write_bytes(payload)
    rejected('--archive',str(corrupt))
    source=isolated/lock['vendor_directory']/'crates/domain/src/lib.rs';original=source.read_bytes();source.write_bytes(original+b'\n// altered source\n');rejected('--verify');source.write_bytes(original)
    manifest=isolated/lock['vendor_directory']/'release-manifest.json';original=manifest.read_bytes();manifest.write_bytes(original+b'\n');rejected('--verify');manifest.write_bytes(original)
    subprocess.run([sys.executable,str(script),'--verify'],cwd=isolated,check=True)
subprocess.run([sys.executable,str(desktop/'scripts/import-runtime.py'),'--verify'],cwd=desktop,check=True)
print('PASS reproducible committed-source artifact and isolated import/tamper rejection; consumer verification does not read producer checkout')
