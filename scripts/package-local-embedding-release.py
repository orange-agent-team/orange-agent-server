#!/usr/bin/env python3
"""Assemble a deterministic local runtime archive and verify a fresh offline installation."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from urllib.parse import unquote,urlsplit
from zipfile import ZipFile

root=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser()
p.add_argument('--native',type=Path,required=True)
p.add_argument('--wheelhouse',type=Path,required=True)
p.add_argument('--backend-wheel',type=Path,required=True)
p.add_argument('--uv-cache',type=Path,required=True,help='Offline cache containing the pinned Python build backend')
p.add_argument('--model-cache',type=Path,required=True)
p.add_argument('--output-dir',type=Path,required=True)
p.add_argument('--profile', choices=('embedding','transcription'), default='embedding')
args=p.parse_args()
profile=args.profile
if sys.version_info[:2]!=(3,12) or platform.machine()!='arm64' or sys.platform!='darwin':
    raise SystemExit('Run with Python 3.12 on the qualified macOS arm64 host')

def sha(data):
    return hashlib.sha256(data).hexdigest()

def tracked(commit,name):
    return subprocess.check_output(['git','show',f'{commit}:{name}'],cwd=root)

native=json.loads((args.native/'native-build.json').read_text())
commit=native['source_commit']
if len(commit)!=40 or any(c not in '0123456789abcdef' for c in commit):
    raise ValueError('Invalid immutable source reference')
if native.get('profile','embedding') != profile:
    raise ValueError('Binary profile differs from requested distribution')
builds=native['builds']
if len(builds)!=2 or builds[0]!=builds[1] or not native['reproducible'] or native['target']!='aarch64-apple-darwin':
    raise ValueError('Two identical native builds required')
files={}
programs = ['local-embeddings','orange-embedding-host'] if profile=='embedding' else ['orange-transcribe']
for name in programs:
    data=(args.native/name).read_bytes()
    if sha(data)!=builds[0][name]:
        raise ValueError('Native release digest mismatch')
    files['bin/'+name]=data
packaging=subprocess.check_output(['git','rev-parse','HEAD'],text=True,cwd=root).strip()
files['install.py']=tracked(packaging,'distribution/embedding/install.py')
for name in ['README.md','smoke.py']:
    files[name]=tracked(packaging,'distribution/'+profile+'/'+name)
for name in [f'config/local-{profile}.lock.json','backends/uv.lock','backends/pyproject.toml','Cargo.lock']:
    files[name]=tracked(commit,name)

lock=tomllib.loads(files['backends/uv.lock'].decode())
wheel_index={unquote(urlsplit(w['url']).path.rsplit('/',1)[-1]):(pkg,w)
             for pkg in lock['package'] for w in pkg.get('wheels',[])}
requirements={}
for path in sorted(args.wheelhouse.glob('*.whl')):
    pkg,w=wheel_index[path.name]
    data=path.read_bytes()
    if len(data)!=w['size'] or 'sha256:'+sha(data)!=w['hash'] or pkg['name'] in requirements:
        raise ValueError('Unexpected or duplicate locked wheel: '+path.name)
    requirements[pkg['name']]=pkg['name']+'=='+pkg['version']+' --hash='+w['hash']
    files['wheels/'+path.name]=data
project=tomllib.loads(files['backends/pyproject.toml'].decode())['project']
wheel=args.backend_wheel
if wheel.name!=f"orange_inference_backends-{project['version']}-py3-none-any.whl":
    raise ValueError('Wrong E09 wheel version')
backend=wheel.read_bytes()
# Reproduce the entire first-party wheel, including installer metadata and entry points.
with tempfile.TemporaryDirectory(prefix='oe-backend-') as temp:
    tree=Path(temp)/'backends'
    tree.mkdir()
    sources=subprocess.check_output(['git','ls-tree','-r','--name-only',commit,'--','backends'],cwd=root,text=True).splitlines()
    for path in sources:
        dst=Path(temp)/path
        dst.parent.mkdir(parents=True,exist_ok=True)
        dst.write_bytes(tracked(commit,path))
    out=Path(temp)/'dist'
    build_env=dict(os.environ,SOURCE_DATE_EPOCH='946684800',UV_CACHE_DIR=str(args.uv_cache.resolve()))
    subprocess.run(['uv','build','--offline','--wheel','--python',sys.executable,'--out-dir',str(out)],cwd=tree,env=build_env,check=True)
    if (out/wheel.name).read_bytes()!=backend:
        raise ValueError('Supplied backend wheel differs from the complete pinned-source build')
with ZipFile(io.BytesIO(backend)) as z:
    for file in (('embedding.py','__init__.py') if profile=='embedding' else ('embedding.py','transcription.py','__init__.py')):
        name='orange_inference_backends/'+file
        if z.read(name)!=tracked(commit,'backends/'+name):
            raise ValueError('E09 wheel disagrees with pinned source')
files['wheels/'+wheel.name]=backend
requirements['orange-inference-backends']=f"orange-inference-backends[{profile}]=={project['version']} --hash=sha256:"+sha(backend)
files['requirements.txt']=('\n'.join(sorted(requirements.values()))+'\n').encode()
manifest={'runtime_source':commit,'packaging_source':packaging,'target':native['target'],'model_assets':'external verified local cache',
          'sha256':{name:sha(data) for name,data in sorted(files.items())}}
files['manifest.json']=(json.dumps(manifest,indent=2)+'\n').encode()

def archive(path):
    with path.open('xb') as out,gzip.GzipFile(fileobj=out,mode='wb',filename='',mtime=0) as compressed:
        with tarfile.open(fileobj=compressed,mode='w') as tar:
            for name,data in sorted(files.items()):
                entry=tarfile.TarInfo('orange-'+profile+'/'+name)
                entry.mode=0o755 if name.startswith('bin/') else 0o644
                entry.size=len(data)
                tar.addfile(entry,io.BytesIO(data))

dest=args.output_dir.resolve()
dest.mkdir(parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(dir=dest,prefix='.verify-') as temp:
    scratch=Path(temp)
    first,second=scratch/'1.tgz',scratch/'2.tgz'
    archive(first)
    archive(second)
    digest=sha(first.read_bytes())
    if digest!=sha(second.read_bytes()):
        raise ValueError('Archive bytes differ')
    with tarfile.open(first) as tar:
        tar.extractall(scratch,filter='data')
    unpacked=scratch/('orange-'+profile)
    env={'PATH':'/usr/bin:/bin','PIP_CONFIG_FILE':os.devnull,'PYTHONNOUSERSITE':'1'}
    venv=scratch/'new-venv'
    subprocess.run([sys.executable,'-I',unpacked/'install.py',venv],cwd=scratch,env=env,check=True)
    answer=json.loads(subprocess.check_output([sys.executable,'-I',unpacked/'smoke.py',venv,args.model_cache.resolve()],cwd=scratch,env=env))
    published=dest/('orange-'+profile+'-macos-arm64-'+digest[:16]+'.tar.gz')
    if published.exists():
        if sha(published.read_bytes())!=digest:
            raise ValueError('Destination conflicts with verified archive')
    else:
        os.link(first,published)
    report=dict(profile=profile,archive=published.name,sha256=digest,bytes=published.stat().st_size,source=commit,packaging_source=packaging,
                identical_archives=2,verified_native_builds=2,backend_rebuilt_from_commit=True,offline_wheels=len(requirements),smoke=answer)
    receipt=published.with_suffix('.json')
    receipt.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report))
