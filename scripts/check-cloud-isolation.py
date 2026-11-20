#!/usr/bin/env python3
"""Qualify the pinned OCI profile in the dedicated, mount-free Colima guest."""
import argparse
from cloud_tools import build_payload,digest
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
LOCK = ROOT / 'config/cloud-isolation.lock.json'
policy = json.loads(LOCK.read_text())
parser = argparse.ArgumentParser()
parser.add_argument('--archive', type=Path, default=ROOT/'.local/gvisor-download'/policy['archive'])
parser.add_argument('--download',action='store_true',help='Fetch a missing fixed archive from the official release URL')
args = parser.parse_args()

archive=args.archive.resolve()
if args.download and not archive.exists():
    archive.parent.mkdir(parents=True,mode=0o700,exist_ok=True)
    with tempfile.TemporaryDirectory(dir=archive.parent) as stage:
        staged=Path(stage)/policy['archive']
        for attempt in range(3):
            outcome=subprocess.run(['curl','--fail','--silent','--show-error','--location','--proto','=https',
                '--connect-timeout','15','--max-time','1200','--continue-at','-','--output',staged,policy['url']])
            if outcome.returncode==0:
                break
        if not staged.is_file() or staged.stat().st_size!=policy['size_bytes'] or digest(staged)!=policy['sha256']:
            raise SystemExit('Official archive download incomplete or inconsistent with locked digest')
        try:
            os.link(staged,archive)
        except FileExistsError:
            pass  # concurrent preparer won; the same full verification follows
if not archive.is_file() or archive.stat().st_size!=policy['size_bytes'] or digest(archive)!=policy['sha256']:
    raise SystemExit('Supply the exact official archive listed in config/cloud-isolation.lock.json')
subprocess.run(['python3',ROOT/'scripts/check-container.py'],check=True)
result_dir=ROOT/'.local/cloud-isolation'
result_dir.mkdir(mode=0o700,parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(dir=result_dir) as work:
    work=Path(work)
    probe=work/'probe'
    version=build_payload(policy,ROOT/'scripts/cloud/isolation_probe.rs',probe)
    # Only these three fixed files cross the VM boundary. No host mounts.
    with tempfile.TemporaryFile(dir=work) as payload:
        with tarfile.open(fileobj=payload,mode='w') as tar:
            tar.add(archive,arcname='archive',recursive=False)
            tar.add(probe,arcname='probe',recursive=False)
            tar.add(LOCK,arcname='profile',recursive=False)
        payload.seek(0)
        script=(ROOT/'scripts/cloud/qualify_guest.py').read_text()
        output=subprocess.run(['colima','ssh','--profile','orange-agent','--','sudo','-n','python3','-c',script],
                              stdin=payload,stdout=subprocess.PIPE,timeout=240)
        if output.returncode:
            raise SystemExit(f'Guest qualification failed (exit {output.returncode})')
    report=json.loads(output.stdout)
    if not report.get('qualified') or report.get('archive_sha256')!=policy['sha256'] or report.get('probe_sha256')!=digest(probe):
        raise SystemExit('Guest receipt differs from the pinned inputs')
    report['compiler']=version
    dest=result_dir/'result.json'
    tmp=work/'result.json';tmp.write_text(json.dumps(report,indent=2)+'\n');os.replace(tmp,dest)
    print(json.dumps(report),flush=True)
