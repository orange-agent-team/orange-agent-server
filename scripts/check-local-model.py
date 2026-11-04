#!/usr/bin/env python3
"""Actual pinned CPU inference through the private supervised process, no paid calls."""
import fcntl
import hashlib
import json
import os
import platform
import stat
from pathlib import Path
import selectors
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
PRIVATE=ROOT/'.local/model-runtime'
def sha256(path):
    digest=hashlib.sha256()
    with path.open('rb') as file:
        for block in iter(lambda:file.read(1024*1024),b''):digest.update(block)
    return digest.hexdigest()
def write_private(path,value):
    descriptor=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_NOFOLLOW,0o600)
    with os.fdopen(descriptor,'w') as file:
        metadata=os.fstat(file.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink!=1 or metadata.st_mode&0o077:raise RuntimeError('Private regular configuration file required')
        file.truncate(0);json.dump(value,file);file.flush();os.fsync(file.fileno())
def main():
    if platform.system() not in ['Darwin','Linux']:raise SystemExit('Unix supervised inference validation only; other targets need independent backend evidence')
    manifest=json.loads((ROOT/'config/local-model.lock.json').read_text())
    report=json.loads((PRIVATE/'build.json').read_text())
    binary=PRIVATE/'build/bin/llama-server';model=PRIVATE/(manifest['model']['sha256']+'.gguf')
    if not model.is_file():raise SystemExit('Pinned official GGUF is missing; no backend launched and no substitute inference used')
    if report['backend_commit']!=manifest['backend']['commit'] or report['gpu_enabled'] is not False:raise SystemExit('Pinned CPU-only build required')
    if sha256(binary)!=report['executable_sha256'] or model.stat().st_size!=manifest['model']['bytes'] or sha256(model)!=manifest['model']['sha256']:raise SystemExit('Artifact verification failed')
    rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
    environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
    subprocess.run([str(cargo),'build','--bin','model-smoke','--bin','orange-model-host','--locked'],cwd=ROOT,env=environment,check=True)
    live=PRIVATE/'live';live.mkdir(mode=0o700,exist_ok=True)
    profile=live/'profile.json';client_config=live/'client.json'
    write_private(profile,{'protocol':1,'runtime_directory':str(live),'executable':str(binary),'executable_sha256':report['executable_sha256'],'model':str(model),'model_sha256':manifest['model']['sha256'],'model_bytes':manifest['model']['bytes'],'model_version':'qwen-local-test-v1','context_tokens':2048,'startup_timeout_seconds':120})
    with open(live/'host.log','w',opener=lambda p,f:os.open(p,f,0o600)) as log:
        host=subprocess.Popen([str(ROOT/'target/debug/orange-model-host'),str(profile)],cwd=ROOT,env={'PATH':'/usr/bin:/bin'},stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True)
        try:
            selector=selectors.DefaultSelector();selector.register(host.stdout,selectors.EVENT_READ)
            for _ in range(30):
                if host.poll() is not None:raise RuntimeError('Owned supervisor exited before ready')
                host.stdin.write('ping\n');host.stdin.flush()
                if selector.select(5):break
            else:raise RuntimeError('Supervisor ready deadline exceeded')
            line=host.stdout.readline();ready=json.loads(line)
            if ready['protocol']!=1 or ready['record']['phase']!='ready' or ready['record']['model_sha256']!=manifest['model']['sha256']:raise RuntimeError('Invalid supervised ready record')
            selector.close()
            write_private(client_config,{'socket_path':str(live/'engine.sock'),'api_key':(live/'api.key').read_text()})
            environment['ORANGE_LOCAL_MODEL_CONFIG']=str(client_config)
            client=subprocess.Popen([str(ROOT/'target/debug/model-smoke')],cwd=ROOT,env=environment)
            try:
                for _ in range(30):
                    try:
                        result=client.wait(timeout=5);break
                    except subprocess.TimeoutExpired:
                        host.stdin.write('ping\n');host.stdin.flush()
                else:raise RuntimeError('Actual inference smoke exceeded deadline')
                if result:raise RuntimeError('Actual inference checks failed')
            finally:
                if client.poll() is None:client.kill();client.wait()
        finally:
            if client_config.exists():client_config.unlink()
            if host.stdin:host.stdin.close()
            host.wait(timeout=15)
            record=json.loads((live/'process.json').read_text())
            if record['phase']!='stopped':raise RuntimeError('Owned model cleanup is not confirmed')
    print('PASS actual pinned CPU model through supervised private IPC; not full Agent, billing, GPU or other-platform certification')
if __name__=='__main__':
    PRIVATE.mkdir(parents=True,exist_ok=True,mode=0o700)
    with (PRIVATE/'smoke.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB);main()
