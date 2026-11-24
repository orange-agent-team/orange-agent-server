#!/usr/bin/env python3
"""Export immutable E01-E04 source from a Git commit; never export working-tree edits."""
import argparse
import gzip
import hashlib
import sys
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
# Release recipes evolve with their source inventory. Reproducing a consumer pin must run
# that revision's exporter, including its own supporting files, without changing a live checkout.
if git("rev-parse","HEAD").decode().strip()!=args.commit or Path(__file__).read_bytes()!=git("show",args.commit+":scripts/export-shared-runtime.py"):
    objects=Path(git("rev-parse","--path-format=absolute","--git-path","objects").decode().strip()).resolve(strict=True)
    if "\n" in str(objects):raise ValueError("Unsupported Git object directory")
    destination=Path(args.output).resolve()
    private=ROOT/".local/shared-runtime"
    private.mkdir(parents=True,exist_ok=True)
    clean_env={k:v for k,v in os.environ.items() if k not in {"GIT_DIR","GIT_WORK_TREE","GIT_INDEX_FILE","GIT_OBJECT_DIRECTORY","GIT_ALTERNATE_OBJECT_DIRECTORIES"}}
    with tempfile.TemporaryDirectory(prefix="pinned-recipe-",dir=private) as tmp:
        checkout=Path(tmp)
        subprocess.run(["git","-c","core.hooksPath=/dev/null","init","-q",checkout],env=clean_env,check=True)
        (checkout/".git/objects/info/alternates").write_text(str(objects)+"\n")
        subprocess.run(["git","-c","core.hooksPath=/dev/null","checkout","--quiet","--detach",args.commit],cwd=checkout,env=clean_env,check=True)
        exporter=checkout/"scripts/export-shared-runtime.py"
        if exporter.is_symlink() or exporter.read_bytes()!=git("show",args.commit+":scripts/export-shared-runtime.py"):
            raise ValueError("Unexpected historical release recipe")
        print("Using exact shared-runtime export recipe",args.commit,hashlib.sha256(exporter.read_bytes()).hexdigest(),flush=True)
        subprocess.run([sys.executable,exporter,"--commit",args.commit,"--output",destination,*(["--verify"] if args.verify else [])],
                       cwd=checkout,env=clean_env,check=True)
    raise SystemExit(0)

members=['crates/domain','crates/protocol','crates/agent-runtime','crates/model-runtime']
# E03's native metadata boundary is an internal package, not a fifth core
# project. Historical commits must still export their original four members.
if subprocess.run(['git','cat-file','-e',args.commit+':crates/platform-files/Cargo.toml'],cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode==0:
    members.append('crates/platform-files')
roots=['Cargo.toml','Cargo.lock',*members,'contracts/proto','contracts/json-schema/knowledge-tools.v1.json','contracts/json-schema/model-version.v1.json','contracts/fixtures/metering-v1.json','contracts/fixtures/marketplace-relationships.v1.json','contracts/fixtures/job-input-interop.v1.json','contracts/fixtures/worker-v1-baseline','contracts/fixtures/transcription-observation.v1.json','contracts/fixtures/builtin-tool-pins.v1.json','contracts/fixtures/editing-tool-pins.v1.json','contracts/fixtures/memory-tool-pins.v1.json','contracts/fixtures/memory-tool-pins.v2.json','config/local-model.lock.json','config/local-tokenizer.lock.json','config/journal-schema.json']
roots.extend(['contracts/json-schema/skill-template.v1.json','config/local-code-review-model.lock.json'])
# Optional local embedding host embeds the exact E09 runner and approved profile.
roots.extend(['config/local-embedding.lock.json','config/local-transcription.lock.json','config/transcription-probe.v1.json','config/local-image.lock.json','backends/orange_inference_backends/image.py','backends/environments/sdxl-macos/pyproject.toml','backends/environments/sdxl-macos/uv.lock','backends/orange_inference_backends/embedding.py','backends/orange_inference_backends/transcription.py','backends/pyproject.toml','backends/uv.lock'])
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
environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
version=subprocess.check_output([str(cargo.parent/'rustc'),'--version'],env=environment,text=True)
if not version.startswith('rustc 1.97.1 '):raise SystemExit('Pinned Rust 1.97.1 required')
stage_root=ROOT/'.local/shared-runtime';stage_root.mkdir(parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(prefix='export-',dir=stage_root) as directory:
    stage=Path(directory)
    # A release archive can contain an older crate with the same package name and version.
    # Keep its compiler fingerprints and binaries out of the consuming checkout.
    environment['CARGO_TARGET_DIR']=str(stage/'target')
    for name,content in files.items():
        target=stage/name;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(content);target.chmod(modes[name])
    subprocess.run([str(cargo),'metadata','--offline','--format-version','1'],cwd=stage,env=environment,stdout=subprocess.DEVNULL,check=True)
    files['Cargo.lock']=(stage/'Cargo.lock').read_bytes()
    if not package_ids(files['Cargo.lock'].decode())<=package_ids(source_lock):
        raise SystemExit('Export resolved a dependency absent from the committed source lock')
    if args.verify:
        subprocess.run([str(cargo),'test','--workspace','--locked','--offline'],cwd=stage,env=environment,check=True)
manifest={'schema':'orange-shared-runtime-source-v1','origin_repository':'orange-agent-server','origin_commit':args.commit,'version':'0.1.0','members':members,'source_sha256':original,'content_sha256':{name:hashlib.sha256(content).hexdigest() for name,content in files.items()},'transformations':['Cargo workspace members restricted to E01-E04 and their explicitly listed internal support packages','Cargo lock pruned offline; every remaining package/version/source/checksum must exist in original lock'],'verification':'actual shared-workspace Rust tests' if args.verify else 'source integrity and locked dependency subset only'}
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
