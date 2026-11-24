"""Measure the fixed local CPU engine and one constrained completion for the model-governance acceptance."""
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import platform
import secrets
import socket
import subprocess
import tempfile
import time


def sha(raw):return hashlib.sha256(raw).hexdigest()


def file_sha(path):
    with path.open("rb") as f:return hashlib.file_digest(f,"sha256").hexdigest()


def request_hash(purpose,value):
    payload=json.dumps(value,sort_keys=True,separators=(",",":"),ensure_ascii=False).encode()
    return sha(b"orange-request-v1\0"+purpose.encode()+b"\0"+payload)


def measure(root,runtime,manifest,fingerprint,tool):
    build=json.loads((runtime/"build.json").read_bytes())
    engine=runtime/"build/bin/llama-server"
    asset,=manifest["artifacts"]
    model=runtime/asset["cache_path"]
    if build["gpu_enabled"] or build["backend_commit"]!=manifest["runtime"]["source_revision"] or file_sha(engine)!=build["executable_sha256"]:
        raise ValueError("Not the pinned local CPU release")
    measured=json.loads(subprocess.check_output([tool,"verify-proposed",runtime],input=json.dumps(manifest).encode(),timeout=120))
    if measured["manifest_sha256"]!=fingerprint:raise ValueError("Manifest bytes differ")
    with tempfile.TemporaryDirectory(prefix="oa-cert-",dir="/private/tmp" if platform.system()=="Darwin" else None) as tmp:
        directory=Path(tmp);uds=directory/"a.sock";keyfile=directory/"token";token=secrets.token_hex(32)
        with os.fdopen(os.open(keyfile,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600),"w") as out:out.write(token)
        class PrivateHTTP(HTTPConnection):
            def connect(self):
                self.sock=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
                self.sock.settimeout(self.timeout);self.sock.connect(str(uds))
        def req(method,path,data=None):
            http=PrivateHTTP("localhost",timeout=45)
            try:
                http.request(method,path,body=json.dumps(data) if data else None,headers={"Authorization":"Bearer "+token,"Content-Type":"application/json"})
                status=http.getresponse();body=json.loads(status.read(1024*1024))
                if status.status!=200:raise ValueError("CPU model rejected sample")
                return body
            finally:http.close()
        cmd=[engine,"--model",model,"--host",uds,"--api-key-file",keyfile,"--alias",manifest["model_version"],"--threads","2","--ctx-size","4096",
             "--parallel","1","--n-gpu-layers","0","--no-warmup","--no-webui","--offline"]
        with (directory/"engine.log").open("wb") as log:
            process=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT)
            try:
                until=time.monotonic()+60
                while time.monotonic()<until:
                    if process.poll() is not None:raise RuntimeError("Pinned CPU process exited")
                    try:
                        if req("GET","/health").get("status")=="ok":break
                    except (OSError,ValueError):pass
                    time.sleep(.15)
                else:raise TimeoutError("Pinned CPU startup")
                tag="OK"+secrets.token_hex(3)
                sample={"prompt":"Produce the required short result.","n_predict":32,"temperature":0,"seed":37,"grammar":'root ::= "'+tag+'"'}
                actual=req("POST","/completion",sample)
                if actual.get("content")!=tag or actual.get("tokens_predicted",0)<=0:raise ValueError("Pinned CPU result differed from constrained reference")
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:process.wait(timeout=8)
                    except subprocess.TimeoutExpired:process.kill();process.wait()
    mem=(int(subprocess.check_output(["sysctl","-n","hw.memsize"])) if platform.system()=="Darwin" else os.sysconf("SC_PHYS_PAGES")*os.sysconf("SC_PAGE_SIZE"))
    execution=request_hash("orange-owned-cpu-v1",{"backend_sha256":build["executable_sha256"],"model_sha256":asset["sha256"],"model_bytes":str(asset["bytes"]),
            "model_version":manifest["model_version"],"context_tokens":4096,"startup_timeout_ms":"120000","gpu_layers":0,"parallel":1,"threads":2})
    return {"runtime_sha256":request_hash("model-runtime-pin.v1",manifest["runtime"]),"verified_asset_count":measured["verified_files"],"verified_asset_bytes":measured["verified_bytes"],
            "probe_id":"llama-cpu-constrained-completion.v1","input_sha256":sha(json.dumps(sample,sort_keys=True,separators=(",",":")).encode()),
            "expected_result_sha256":sha(tag.encode()),"observed_result_sha256":sha(actual["content"].encode())}, {
            "device":"cpu","operating_system":platform.system().lower(),"architecture":platform.machine(),"memory_bytes":mem,"logical_processors":os.cpu_count()},execution
