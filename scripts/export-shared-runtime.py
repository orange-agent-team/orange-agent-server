#!/usr/bin/env python3
"""Export immutable E01-E04 source from a Git commit; never export working-tree edits."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser();parser.add_argument('--commit',required=True);parser.add_argument('--output',required=True);parser.add_argument('--verify',action='store_true');args=parser.parse_args()
if not re.fullmatch('[0-9a-f]{40}',args.commit):raise SystemExit('A complete immutable Git commit is required')
def git(*argv):return subprocess.check_output(['git',*argv],cwd=ROOT)
if git('cat-file','-t',args.commit).strip()!=b'commit':raise SystemExit('Origin must be a commit')
members=['crates/domain','crates/protocol','crates/agent-runtime','crates/model-runtime']
roots=['Cargo.toml','Cargo.lock',*members,'contracts/proto','contracts/fixtures/metering-v1.json','contracts/fixtures/builtin-tool-pins.v1.json','config/local-model.lock.json','config/local-tokenizer.lock.json']
entries=git('ls-tree','-r','-z',args.commit,'--',*roots).split(b'\0')
files={};modes={}
for entry in filter(None,entries):
    attributes,path=entry.split(b'\t',1);mode,kind,blob=attributes.split();name=path.decode('utf-8')
    if mode not in [b'100644',b'100755'] or kind!=b'blob':raise SystemExit('Only regular committed files may be exported')
    files[name]=git('cat-file','blob',blob.decode());modes[name]=0o755 if mode==b'100755' else 0o644
original={name:hashlib.sha256(content).hexdigest() for name,content in files.items()}
source_lock=files['Cargo.lock'].decode()
def package_ids(text):
    result=set()
    for block in text.split('[[package]]')[1:]:
        fields={}
        for key in ['name','version','source','checksum']:
            match=re.search(r'^'+key+r' = ("[^"\n]*")$',block,re.M)
            if match:fields[key]=json.loads(match.group(1))
        if not {'name','version'}<=fields.keys():raise SystemExit('Unrecognized Cargo package record')
        result.add(tuple(fields.get(key) for key in ['name','version','source','checksum']))
    if not result:raise SystemExit('Empty Cargo lock package set')
    return result
root_manifest=files['Cargo.toml'].decode()
root_manifest,count=re.subn(r'^members = .*$', 'members = '+json.dumps(members),root_manifest,count=1,flags=re.M)
if count!=1:raise SystemExit('Unrecognized workspace member declaration')
files['Cargo.toml']=root_manifest.encode()
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','');environment['CARGO_TARGET_DIR']=str(ROOT/'target')
version=subprocess.check_output([str(cargo.parent/'rustc'),'--version'],env=environment,text=True)
if not version.startswith('rustc 1.97.1 '):raise SystemExit('Pinned Rust 1.97.1 required')
stage_root=ROOT/'.local/shared-runtime';stage_root.mkdir(parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(prefix='export-',dir=stage_root) as directory:
    stage=Path(directory)
    for name,content in files.items():
        target=stage/name;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(content);target.chmod(modes[name])
    subprocess.run([str(cargo),'metadata','--offline','--format-version','1'],cwd=stage,env=environment,stdout=subprocess.DEVNULL,check=True)
    files['Cargo.lock']=(stage/'Cargo.lock').read_bytes()
    if not package_ids(files['Cargo.lock'].decode())<=package_ids(source_lock):
        raise SystemExit('Export resolved a dependency absent from the committed source lock')
    if args.verify:
        subprocess.run([str(cargo),'test','--workspace','--locked','--offline'],cwd=stage,env=environment,check=True)
manifest={'schema':'orange-shared-runtime-source-v1','origin_repository':'orange-agent-server','origin_commit':args.commit,'version':'0.1.0','members':members,'source_sha256':original,'content_sha256':{name:hashlib.sha256(content).hexdigest() for name,content in files.items()},'transformations':['Cargo workspace members restricted to E01-E04','Cargo lock pruned offline; every remaining package/version/source/checksum must exist in original lock'],'verification':'actual shared-workspace Rust tests' if args.verify else 'source integrity and locked dependency subset only'}
files['release-manifest.json']=(json.dumps(manifest,ensure_ascii=False,sort_keys=True,indent=2)+'\n').encode();modes['release-manifest.json']=0o644
buffer=io.BytesIO()
with gzip.GzipFile(fileobj=buffer,mode='wb',mtime=0,filename='') as zipped:
    with tarfile.open(fileobj=zipped,mode='w') as archive:
        for name in sorted(files):
            info=tarfile.TarInfo(name);info.size=len(files[name]);info.mode=modes[name];info.mtime=0;info.uid=info.gid=0;info.uname=info.gname=''
            archive.addfile(info,io.BytesIO(files[name]))
payload=buffer.getvalue();output=Path(args.output).resolve();output.parent.mkdir(parents=True,exist_ok=True)
if output.exists() and output.read_bytes()!=payload:raise SystemExit('Refuse to replace an existing different release artifact')
output.write_bytes(payload)
sha=hashlib.sha256(payload).hexdigest();output.with_suffix(output.suffix+'.sha256').write_text(sha+'\n')
print(json.dumps({'artifact':str(output),'sha256':sha,'bytes':len(payload),'origin_commit':args.commit,'files':len(files),'verification':manifest['verification']},indent=2))
