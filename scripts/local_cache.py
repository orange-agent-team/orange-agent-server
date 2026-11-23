"""Ephemeral, authenticated Redis on the dedicated development engine. No persistent data volume."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time

from linux_platform_inputs import local_image_id

ROOT=Path(__file__).resolve().parents[1]
DOCKER=[shutil.which("docker") or "/usr/local/bin/docker","--context","colima-orange-agent"]
LABEL="org.orange-agent.development-cache"
PORT="6379/tcp"


class CacheError(Exception):
    pass


def require(value,reason):
    if not value:
        raise CacheError(reason)


def docker(args):
    result=subprocess.run([*DOCKER,*args],capture_output=True,text=True,timeout=45)
    if result.returncode:
        raise CacheError("Local Docker command failed: "+args[0])
    return result.stdout.strip()


def container(state):
    name="orange-cache-"+state["id"]
    # Only an empty, successful listing proves absence; connection errors retain the ownership record.
    match=docker(["ps","-aq","--no-trunc","--filter","name=^/"+name+"$"])
    if not match:
        return None
    require(re.fullmatch(r"[0-9a-f]{64}",match),"Ambiguous cache container")
    info=json.loads(docker(["inspect",match]))[0]
    require(info["Name"]=="/"+name and info["Config"]["Labels"].get(LABEL)==state["owner"],"Cache container ownership mismatch")
    return info


def record(path,state):
    with tempfile.NamedTemporaryFile(mode="w",dir=path.parent,delete=False) as tmp:
        try:
            json.dump(state,tmp)
            tmp.flush()
            os.fsync(tmp.fileno())
            os.replace(tmp.name,path)
        finally:
            Path(tmp.name).unlink(missing_ok=True)


def load(path):
    if not path.exists() and not path.is_symlink():
        return None
    with os.fdopen(os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK),"rb") as stream:
        meta=os.fstat(stream.fileno())
        require(stat.S_ISREG(meta.st_mode) and meta.st_uid==os.getuid() and meta.st_mode&0o077==0 and meta.st_size<4096,"Invalid private cache state")
        state=json.load(stream)
    for key,length in (("id",24),("owner",32),("password",64)):
        require(re.fullmatch("[0-9a-f]{"+str(length)+"}",state[key]),"Invalid private cache identity")
    return state


def frame(*fields):
    values=[value.encode() for value in fields]
    return b"*"+str(len(values)).encode()+b"\r\n"+b"".join(b"$"+str(len(v)).encode()+b"\r\n"+v+b"\r\n" for v in values)


def response(reader):
    line=reader.readline(4096)
    require(line.endswith(b"\r\n"),"Invalid development Redis response")
    tag=line[:1]
    raw=line[1:-2]
    if tag==b"-":
        return ("error",raw.split(b" ",1)[0].decode("ascii"))
    if tag==b"+":
        return raw.decode("ascii")
    if tag==b":":
        return int(raw)
    require(tag==b"$","Unexpected Redis response type")
    length=int(raw)
    if length==-1:
        return None
    require(0<=length<=65536,"Oversized Redis response")
    data=reader.read(length)
    require(len(data)==length and reader.read(2)==b"\r\n","Incomplete Redis response")
    return data


def exchange(port,commands):
    with socket.create_connection(("127.0.0.1",port),timeout=2) as conn, conn.makefile("rb") as reader:
        conn.sendall(b"".join(frame(*cmd) for cmd in commands))
        return [response(reader) for _ in commands]


def probe(state,port):
    require(exchange(port,[("AUTH","orange_cache",state["password"]),("PING",)])==["OK","PONG"],"Cache readiness denied")


def start(path):
    old=load(path)
    if old:
        info=container(old)
        if info and info["State"]["Running"]:
            port=host_port(info)
            probe(old,port)
            old.update(port=port)
            record(path,old)
            return old
        if info:
            docker(["rm",info["Id"]])
    spec=json.loads((ROOT/"config/development-cache.lock.json").read_bytes())
    image=local_image_id(DOCKER,spec)
    require(image,"Provision the pinned development_cache image first")
    state={"id":secrets.token_hex(12),"owner":secrets.token_hex(16),"password":secrets.token_hex(32)}
    state["prefix"]="orange:"+state["id"]+":"
    record(path,state)
    hashed="#"+hashlib.sha256(state["password"].encode()).hexdigest()
    docker(["run","-d","--rm","--name","orange-cache-"+state["id"],"--label",LABEL+"="+state["owner"],
            "--pull","never","--user","redis","--read-only","--cap-drop","ALL","--security-opt","no-new-privileges",
            "--pids-limit","64","--memory","96m","--tmpfs","/data:rw,noexec,nosuid,nodev,size=16m,mode=1777",
            "-p","127.0.0.1::6379/tcp","--entrypoint","/bin/busybox",image,"timeout","1800","redis-server",
            "--bind","0.0.0.0","--port","6379","--save","","--appendonly","no","--maxmemory","32mb",
            "--maxmemory-policy","allkeys-lru","--user","default","off","--user","orange_cache","on",hashed,
            "~"+state["prefix"]+"*","-@all","+ping","+get","+set","+del","+ttl","+expire"])
    info=container(state)
    require(info and info["State"]["Running"],"Cache process failed to start")
    port=host_port(info)
    deadline=time.monotonic()+15
    while True:
        try:
            probe(state,port)
            break
        except (OSError,CacheError):
            if time.monotonic()>=deadline:
                raise CacheError("Cache did not become ready")
            time.sleep(.15)
    state.update(port=port)
    record(path,state)
    return state


def host_port(info):
    (binding,)=info["NetworkSettings"]["Ports"][PORT]
    require(binding["HostIp"]=="127.0.0.1","Cache must bind only to loopback")
    return int(binding["HostPort"])


def stop(path):
    state=load(path)
    if state:
        info=container(state)
        if info:
            docker(["rm","-f",info["Id"]])
        require(container(state) is None,"Cache removal unconfirmed")
        path.unlink()


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("action",choices=["start","status","stop"])
    parser.add_argument("--instance",default="default")
    args=parser.parse_args()
    require(re.fullmatch(r"[a-z][a-z0-9-]{0,40}",args.instance),"Invalid local instance name")
    root=ROOT/".local"/"cache-fixtures"/args.instance
    root.mkdir(mode=0o700,parents=True,exist_ok=True)
    require(root.resolve()==root and root.stat().st_uid==os.getuid() and root.stat().st_mode&0o077==0,"Private fixture directory required")
    path=root/"connection.json"
    with os.fdopen(os.open(root/"lifecycle.lock",os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600),"rb+") as guard:
        fcntl.flock(guard,fcntl.LOCK_EX)
        if args.action=="stop":
            stop(path)
            print(json.dumps({"removed":True}))
            return
        if args.action=="start":
            state=start(path)
        else:
            state=load(path)
            info=container(state) if state else None
            require(info and info["State"]["Running"],"No running owned cache")
            port=host_port(info)
            probe(state,port)
            if state.get("port")!=port:
                state["port"]=port
                record(path,state)
        print(json.dumps({"ready":True,"host":"127.0.0.1","port":state["port"],"user":"orange_cache","prefix":state["prefix"],"credential_file":str(path)}))


if __name__=="__main__":
    try:
        main()
    except (OSError,CacheError,KeyError,ValueError,subprocess.TimeoutExpired) as exc:
        sys.exit(str(exc) if isinstance(exc,CacheError) else "Local cache failure: "+type(exc).__name__)
