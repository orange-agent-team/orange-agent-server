"""Select a non-editable, versioned ASR environment built from the repository's locked offline inputs."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

def interpreter(root,cache=None):
    root=Path(root).resolve()
    project=root/'backends'
    version=json.loads((root/'config/transcription-probe.v1.json').read_bytes())['packages']['orange-inference-backends']
    sources={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((project/'orange_inference_backends').glob('*.py'))}
    digest=hashlib.sha256(json.dumps(dict(sources=sources,lock=hashlib.sha256((project/'uv.lock').read_bytes()).hexdigest(),project=hashlib.sha256((project/'pyproject.toml').read_bytes()).hexdigest()),sort_keys=True).encode()).hexdigest()
    parent=root/'.local/transcription-environments';parent.mkdir(mode=0o700,parents=True,exist_ok=True)
    target=parent/(version+'-'+digest[:20]);python=target/'bin/python'
    inspect="import hashlib,importlib.metadata as m,json,pathlib;d=m.distribution('orange-inference-backends');p=pathlib.Path(d.locate_file('orange_inference_backends'));print(json.dumps([d.version,{f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in p.glob('*.py')}]))"
    def valid():
        if not python.is_file():return False
        result=subprocess.run([python,'-I','-B','-c',inspect],capture_output=True,text=True,check=False)
        return result.returncode==0 and json.loads(result.stdout)==[version,sources]
    lock=parent/(digest+'.lock')
    with lock.open('a') as guard:
        fcntl.flock(guard,fcntl.LOCK_EX)
        if not valid():
            uv=shutil.which('uv') or str(Path.home()/'.local/bin/uv')
            env=dict(os.environ,UV_CACHE_DIR=str(Path(cache or root/'.local/uv-cache').resolve()),UV_PROJECT_ENVIRONMENT=str(target),UV_LINK_MODE='clone')
            subprocess.run([uv,'sync','--locked','--offline','--no-editable','--reinstall-package','orange-inference-backends','--extra','transcription','--python','3.12','--project',project],cwd=root,env=env,check=True)
            if not valid():raise RuntimeError('Installed ASR wheel source or version differs from the locked producer')
    return python

if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--uv-cache',type=Path);args=parser.parse_args()
    print(interpreter(Path(__file__).resolve().parents[1],args.uv_cache))
