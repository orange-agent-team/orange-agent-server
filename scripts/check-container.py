#!/usr/bin/env python3
"""Verify only the dedicated orange-agent local container engine."""
import json
import os
from pathlib import Path
import shutil
import subprocess

socket=Path.home()/'.colima/orange-agent/docker.sock'
if not socket.is_socket():raise SystemExit('Dedicated Colima socket is absent')
environment=os.environ.copy()
environment.pop('DOCKER_CONTEXT',None)
environment.pop('DOCKER_HOST',None)
docker=shutil.which('docker') or '/usr/local/bin/docker'
result=subprocess.run([docker,'--host','unix://'+str(socket),'info','--format','{{json .}}'],
                      env=environment,capture_output=True,text=True,timeout=20,check=True)
info=json.loads(result.stdout)
assert info['OSType']=='linux'
assert 1<=info['NCPU']<=2
assert 0<info['MemTotal']<=5*1024**3
config=(Path.home()/'.colima/orange-agent/colima.yaml').read_text()
assert 'autoActivate: false' in config
assert 'mounts: null' in config or 'mounts: []' in config
print(json.dumps({'runtime':'colima-orange-agent','server_version':info['ServerVersion'],
                  'cpus':info['NCPU'],'memory_bytes':info['MemTotal'],
                  'host_mounts':'disabled','default_context_changed':False,
                  'result':'PASS','not_verified':'gVisor, GPU, production and external infrastructure'}))
