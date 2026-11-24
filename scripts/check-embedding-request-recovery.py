#!/usr/bin/env python3.12
"""Real private HTTP disconnect and worker reap, injected allocator failure, then actual locked CPU inference."""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import subprocess
import tempfile
import time

ROOT=Path(__file__).resolve().parents[1]
PROFILE="multilingual-e5-small-onnx-cpu-v1"


def require(ok,reason):
    if not ok:raise ValueError(reason)


def until(predicate,reason,seconds):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        if predicate():return
        time.sleep(.04)
    raise TimeoutError(reason)


def reaped(pid):
    try:os.kill(pid,0)
    except ProcessLookupError:return True
    return False


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--asset-root",type=Path,default=ROOT/".local/embedding-model")
    cli.add_argument("--python",type=Path,default=ROOT/"backends/.venv/bin/python")
    cli.add_argument("--host",type=Path,help="Verify this installed release program")
    cli.add_argument("--host-sha256",help="Required digest of the installed program")
    args=cli.parse_args()
    if bool(args.host)!=bool(args.host_sha256):cli.error("--host and --host-sha256 are required together")
    python=args.python.absolute();assets=args.asset_root.resolve(strict=True)
    require(python.is_file(),"Locked Python executable required")
    if args.host:
        program=args.host.absolute()
        with program.open("rb") as source:
            require(not program.is_symlink() and hashlib.file_digest(source,"sha256").hexdigest()==args.host_sha256,"Unexpected native program")
    else:
        rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
        cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
        env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
        subprocess.run([cargo,"build","--locked","--offline","-p","orange-model-runtime","--features","embedding-host","--bin","orange-embedding-host"],cwd=ROOT,env=env,check=True)
        target=Path(json.loads(subprocess.check_output([cargo,"metadata","--format-version","1","--locked","--offline","--no-deps"],cwd=ROOT,env=env))["target_directory"])
        program=target/"debug/orange-embedding-host"
    with tempfile.TemporaryDirectory(prefix="oa-embed-reap-") as temporary:
        root=Path(temporary);fixture=root/"controlled-python";mode=root/"mode";pidfile=root/"pid";out_of_memory=root/"memory-error"
        # The fixture changes only process scheduling or the allocator fault. The released E09
        # script handles the exception itself. In normal mode exec enters its original locked venv.
        fixture.write_text("#!"+str(python)+"\n"+f"""import builtins,os,sys,time
from pathlib import Path
state=Path({str(mode)!r}).read_text()
if state=='block':
    Path({str(pidfile)!r}).write_text(str(os.getpid()))
    while True:time.sleep(5)
if state=='memory':
    original=builtins.__import__
    def load(name,*args,**kwargs):
        if name=='tokenizers':
            Path({str(out_of_memory)!r}).write_text(str(os.getpid()))
            raise MemoryError('isolated allocator fault')
        return original(name,*args,**kwargs)
    builtins.__import__=load
    position=sys.argv.index('-c');source=sys.argv[position+1];sys.argv=['-c',*sys.argv[position+2:]]
    exec(compile(source,'<released E09 embedding runner>','exec'),{{'__name__':'__main__'}})
else:
    os.execv({str(python)!r},[{str(python)!r},*sys.argv[1:]])
""");fixture.chmod(0o700);mode.write_text("block")
        with subprocess.Popen([program,fixture,assets],cwd=ROOT,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True) as host:
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(host.stdout,selectors.EVENT_READ)
                    require(selector.select(15),"Owned host startup")
                session=json.loads(host.stdout.readline())
                def call(wait=True):
                    con=http.client.HTTPConnection("localhost",timeout=60)
                    try:
                        con.sock=socket.socket(socket.AF_UNIX);con.sock.settimeout(60);con.sock.connect(session["socket"])
                        con.request("POST","/v1/embeddings",json.dumps({"model":PROFILE,"input":["a bounded local request"]}),
                                    {"Authorization":"Bearer "+session["token"],"Content-Type":"application/json"})
                        if not wait:return con
                        response=con.getresponse();return response.status,json.loads(response.read(1024*1024))
                    finally:
                        if wait:con.close()
                original=call(False)
                try:
                    until(lambda:pidfile.exists() and pidfile.stat().st_size>0,"No owned runner started",10)
                    pid=int(pidfile.read_text());require(not reaped(pid) and call()[0]==429,"A busy runner was not excluded")
                finally:original.close()
                until(lambda:reaped(pid),"Disconnected request kept the original CPU runner",10)
                mode.write_text("memory")
                code,data=call()
                require(code==503 and "data" not in data and data["error"]["code"]=="embedding_backend_unavailable" and out_of_memory.is_file(),"Invalid allocator-failure response")
                require(reaped(int(out_of_memory.read_text())),"Failure arrived before child exit was confirmed")
                mode.write_text("inference")
                status,value=call()
                require(status==200 and value["model"]==PROFILE and len(value["data"])==1 and len(value["data"][0]["embedding"])==384 and
                        value["usage"]["prompt_tokens"]>0,"Normal inference after confirmed recovery failed")
                print(json.dumps({"busy_status":429,"disconnected_runner_reaped":True,"injected_memory_error_status":code,
                                  "memory_error_runner_reaped":True,"recovered_real_cpu_dimensions":384,"received_vectors_on_failure":0}),flush=True)
            finally:
                host.stdin.close()
                try:host.wait(timeout=15)
                except subprocess.TimeoutExpired:host.kill();host.wait();raise
            require(host.returncode==0,"Owned embedding host failed to drain")
    print("PASS real disconnect, confirmed CPU runner exit, allocator-fault refusal and subsequent real ONNX inference",flush=True)


if __name__=="__main__":main()
