#!/usr/bin/env python3
"""Private stdio bridge to a single development OCI workspace. No arbitrary commands."""
import argparse
import base64
import io
import json
from pathlib import Path
import shutil
import os
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import uuid
from cloud_tools import build_payload,digest

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('operation',choices=('reserve','open','recover'))
parser.add_argument('allocation')
parser.add_argument('--archive',type=Path)
args=parser.parse_args()
if str(uuid.UUID(args.allocation))!=args.allocation:raise SystemExit('Canonical allocation ID required')
guest=(ROOT/'scripts/cloud/qualify_guest.py').read_text()
command=['colima','ssh','--profile','orange-agent','--','sudo','-n','python3','-c',guest,args.operation,args.allocation]
if args.operation=='recover':
    raise SystemExit(subprocess.run(command,stdin=subprocess.DEVNULL).returncode)
# Keep the same buffer for the initial packet and renewals that may arrive in one write.
control_stream=os.fdopen(os.dup(sys.stdin.fileno()),'rb')
packet=json.loads(control_stream.readline(360000))
if args.operation=='reserve':
    metadata=json.dumps(packet,separators=(',',':')).encode()+b'\n'
    if len(metadata)>4096 or 'content_base64' in packet:raise ValueError('Bounded identity-only reservation required')
    raise SystemExit(subprocess.run(command,input=metadata,timeout=30).returncode)
data=base64.b64decode(packet.pop('content_base64'),validate=True)
import hashlib
if not 0<len(data)<=262144 or hashlib.sha256(data).hexdigest()!=packet['input_sha256'] or packet['allocation_id']!=args.allocation:
    raise SystemExit('Input differs from the fetched allocation')
data.decode('utf-8')
meta=json.dumps(packet,separators=(',',':')).encode()
if len(meta)>4096:raise SystemExit('Oversized binding')
policy_file=ROOT/'config/cloud-isolation.lock.json'
policy=json.loads(policy_file.read_text())
archive=(args.archive or ROOT/'.local/gvisor-download'/policy['archive']).resolve()
if archive.stat().st_size!=policy['size_bytes'] or digest(archive)!=policy['sha256']:
    raise SystemExit('Pinned runtime archive required')
with tempfile.TemporaryDirectory(prefix='orange-host-driver-') as directory:
    probe=Path(directory)/'payload'
    build_payload(policy,ROOT/'scripts/cloud/workspace_payload.rs',probe)
    with tempfile.TemporaryFile(dir=directory) as frame:
        with tarfile.open(fileobj=frame,mode='w') as bundle:
            for file,name in [(archive,'archive'),(probe,'probe'),(policy_file,'profile')]:
                bundle.add(file,arcname=name,recursive=False)
            for name,value in [('allocation',meta),('input',data)]:
                info=tarfile.TarInfo(name);info.mode=0o600;info.size=len(value);bundle.addfile(info,io.BytesIO(value))
        length=frame.tell();frame.seek(0)
        proc=subprocess.Popen(command,stdin=subprocess.PIPE)
        try:
            proc.stdin.write(meta+b'\n'+struct.pack('>Q',length))
            shutil.copyfileobj(frame,proc.stdin,1024*1024);proc.stdin.flush()
            finished=threading.Event()
            def relay():
                try:
                    while not finished.is_set():
                        line=control_stream.readline(2049)
                        if len(line)>2048:break
                        if not line:break
                        if line!=b'stop\n' and not (line.startswith(b'renew ') and line[6:-1].isdigit() and line.endswith(b'\n')) and not (line.startswith(b'tool {') and line.endswith(b'}\n')):
                            break
                        proc.stdin.write(line);proc.stdin.flush()
                        if line==b'stop\n':break
                except (BrokenPipeError,OSError):
                    pass
                finally:
                    try:proc.stdin.close()
                    except (BrokenPipeError,OSError):pass
            thread=threading.Thread(target=relay,daemon=True);thread.start()
            result=proc.wait(timeout=3700)
            finished.set();thread.join(timeout=2)
        except BaseException:
            if not proc.stdin.closed:proc.stdin.close()
            proc.wait(timeout=60)
            raise
        if result:raise SystemExit('Managed allocation driver failed; use original allocation recovery')
