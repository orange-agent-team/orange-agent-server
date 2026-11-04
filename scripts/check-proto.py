#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import subprocess

root=Path(__file__).resolve().parents[1]
protoc=os.environ.get('PROTOC') or shutil.which('protoc') or '/opt/homebrew/bin/protoc'
out=root/'.local/contracts'
out.mkdir(parents=True,exist_ok=True)
subprocess.run([protoc,'--proto_path='+str(root/'contracts/proto'),
    '--descriptor_set_out='+str(out/'orange.pb'),'--include_imports','worker.proto','host.proto'],check=True,cwd=root)
print('Protobuf Worker/RunStore descriptors compiled; services are not implemented by this check')
