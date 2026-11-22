#!/usr/bin/env python3
"""Check the three real API entry points and their separate local scrape sockets."""
import argparse
import http.client
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import subprocess
import tempfile
import time

from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]
ROLES = (("development","orange-api","ORANGE_BIND","Orange development API"),
         ("user","orange-user-api","ORANGE_USER_BIND","Development User API"),
         ("admin","orange-admin-api","ORANGE_ADMIN_BIND","Development Admin API"))


def request(port, path, marker):
    client = http.client.HTTPConnection("127.0.0.1",port,timeout=5)
    try:
        client.request("GET",path,headers={"Authorization":"Bearer "+marker})
        response=client.getresponse()
        data=response.read(1024*1024+1)
        if len(data)>1024*1024:
            raise ValueError("Metrics response exceeded expected bounds")
        return response.status,data.decode(errors="replace")
    finally:
        client.close()


def exercise(executable, role, label, env, log):
    with log.open("wb") as stream:
        child=subprocess.Popen([executable],env=env,cwd=ROOT,stdout=subprocess.DEVNULL,stderr=stream)
        try:
            deadline=time.monotonic()+45
            while time.monotonic()<deadline:
                if child.poll() is not None:
                    raise RuntimeError(role+" stopped before startup")
                lines=log.read_text()
                api=re.search(re.escape(label)+r" listening on 127\.0\.0\.1:(\d+)",lines)
                scrape=re.search(r"Local "+role+r" HTTP metrics listening on 127\.0\.0\.1:(\d+)",lines)
                if api and scrape:
                    break
                time.sleep(.05)
            else:
                raise TimeoutError(role+" did not bind both owned sockets")
            marker="private-"+secrets.token_hex(16)
            api_port,metrics_port=int(api[1]),int(scrape[1])
            if api_port==metrics_port or not api_port:
                raise ValueError("Metrics listener was not separate")
            for path in ("/metrics","/unknown/"+marker+"?q="+marker):
                status,body=request(api_port,path,marker)
                if status not in (401,403,404) or "orange_http_responses_total" in body:
                    raise ValueError("API exposed unexpected content")
            status,text=request(metrics_port,"/metrics",marker)
            expected=f'orange_http_responses_total{{api_role="{role}",method="GET",status_class="4xx"}} 2'
            if status!=200 or expected not in text.splitlines() or marker in text:
                raise ValueError("Private scrape failed role, count or data-minimization check")
            if request(metrics_port,"/api/v1",marker)[0]!=404:
                raise ValueError("Observer exposed application routes")
            print("PASS actual",role,"process: separate API and observer, two client rejections, fixed public labels",flush=True)
        finally:
            if child.poll() is None:
                child.send_signal(signal.SIGINT)
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
                raise RuntimeError(role+" did not drain on shutdown")


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--private-dir",type=Path,default=ROOT/".local")
    args=cli.parse_args()
    private=args.private_dir.resolve()
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    env=dict(os.environ,CARGO_NET_OFFLINE="true",PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
    targets=[option for _,name,_,_ in ROLES for option in ("--bin",name)]
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-control-plane",*targets],cwd=ROOT,env=env,check=True)
    meta=json.loads(subprocess.check_output([cargo,"metadata","--locked","--offline","--no-deps","--format-version","1"],cwd=ROOT,env=env))
    output=Path(meta["target_directory"])/"debug"
    with database(private/"database-url","orange_observer_") as (_,url), tempfile.TemporaryDirectory() as work:
        temp=Path(work)
        identities=temp/"development.json"
        identities.write_text(json.dumps([{"owner_id":"observer-fixture","token":secrets.token_hex(32)}]))
        identities.chmod(0o600)
        for role,name,address,label in ROLES:
            app_env={k:v for k,v in env.items() if not k.startswith("ORANGE_")}
            app_env.update(ORANGE_ENV="development",DATABASE_URL=url,ORANGE_METRICS_BIND="127.0.0.1:0",
                           ORANGE_DEV_IDENTITIES_FILE=str(identities),ORANGE_USER_CONFIG_FILE=str(private/"identity/user-config.json"),
                           ORANGE_ADMIN_CONFIG_FILE=str(private/"identity/admin-config.json"))
            app_env[address]="127.0.0.1:0"
            exercise(output/name,role,label,app_env,temp/(role+".log"))


if __name__=="__main__":
    main()
