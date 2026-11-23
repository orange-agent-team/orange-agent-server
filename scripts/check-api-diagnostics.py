#!/usr/bin/env python3
"""Exercise all three actual native configuration entry points and correlate an authorized Job with its leased Worker attempt."""
import argparse
import http.client
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import signal
import time
import sys
import tempfile
import uuid
from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
ROLES={"development":("orange-api","ORANGE_BIND","ORANGE_DEV_IDENTITIES_FILE"),
       "user":("orange-user-api","ORANGE_USER_BIND","ORANGE_USER_CONFIG_FILE"),
       "admin":("orange-admin-api","ORANGE_ADMIN_BIND","ORANGE_ADMIN_CONFIG_FILE")}


def require(ok,reason):
    if not ok:
        raise ValueError(reason)


def settings(area,output,private,section,env):
    marker="confidential-"+secrets.token_hex(12)
    password=area/"database.secret"
    password.write_text("postgresql://orange_local:"+marker+"@127.0.0.1:1/development")
    password.chmod(0o600)
    identity=area/("identity-"+marker+".json")
    identity.write_text(json.dumps([{"owner_id":"dev-owner","token":marker+"x"*32}]))
    for role,(name,bind,ident) in ROLES.items():
        file=area/(role+".json")
        source=identity if role=="development" else private/"identity"/(role+"-config.json")
        inner={"environment":"development","bind":"127.0.0.1:51333","identity_file":str(source),"database_url_file":password.name,
               "region":"US","diagnostic_log":"off"}
        file.write_text(json.dumps(inner))
        env_base={k:v for k,v in env.items() if not k.startswith("ORANGE_") and k!="DATABASE_URL"}
        def invoke(args,env_updates,expected=None):
            result=subprocess.run([output/name,*args],cwd=ROOT,env={**env_base,**env_updates},capture_output=True,text=True,timeout=10)
            require(marker not in result.stdout+result.stderr,"Sensitive setting printed by "+name)
            if expected is None:
                require(result.returncode==0,"Expected configuration success: "+result.stderr[:200])
                return json.loads(result.stdout)
            require(result.returncode!=0,"Expected startup refusal")
            payload=json.loads(result.stderr.strip())
            require((payload["code"],payload["field"],payload["source"])==expected and not payload["retryable"],"Wrong refusal: "+str(payload))
        if section=="settings":
            args=["--config",str(file),"--describe-config"]
            v=invoke(args,{})
            require(v["region"]=="US" and v["sources"]["database"]=="file" and v["sources"]["bind"]=="file","File layer lost")
            v=invoke(args,{bind:"127.0.0.1:51334","ORANGE_REGION":"JP"})
            require(v["bind"]=="127.0.0.1:51334" and v["sources"]["region"]=="environment","Environment layer lost")
            v=invoke([*args,"--bind","127.0.0.1:0","--region","CN-HK","--diagnostic-log","on"],
                     {bind:"127.0.0.1:51334","ORANGE_REGION":"JP","ORANGE_API_SETTINGS_FILE":str(area/"does-not-exist")})
            require(v["bind"]=="127.0.0.1:0" and v["region"]=="CN-HK" and v["sources"]["bind"]=="cli" and v["diagnostic_log"],"CLI layer lost")
            v=invoke(["--describe-config"],{"ORANGE_ENV":"development",ident:str(source),"DATABASE_URL":password.read_text()})
            require(v["region"]=="LOCAL" and v["sources"]["bind"]=="default" and v["metrics_bind"] is None,"Legacy/default path differs")
        else:
            args=["--config",str(file)]
            invoke(args+["--region",marker],{},("CONFIG_VALUE_INVALID","region","cli"))
            invoke(args+["--bind","0.0.0.0:8080"],{},("CONFIG_ADDRESS_FORBIDDEN","bind","cli"))
            invoke(args+["--environment","production"],{},("CONFIG_ENVIRONMENT_FORBIDDEN","environment","cli"))
            invoke(args+["--bind","127.0.0.1:7","--bind","127.0.0.1:8"],{},("CONFIG_CONFLICT","bind","cli"))
            invoke(args,{"DATABASE_URL":password.read_text(),"ORANGE_DATABASE_URL_FILE":str(password)},("CONFIG_CONFLICT","database","environment"))
            invoke(args,{"DATABASE_URL":marker},("CONFIG_VALUE_INVALID","database","environment"))
            password.chmod(0o644)
            try:
                invoke(args,{},("CONFIG_SECRET_PERMISSIONS","database","file"))
            finally:
                password.chmod(0o600)
            missing=area/("invalid-"+marker+".json")
            missing.write_text(json.dumps({"unknown":marker}))
            invoke(args+["--identity-file",str(missing)],{},("CONFIG_IDENTITY_INVALID","identity_file","cli"))
            broken=area/"unknown.json"
            broken.write_text(json.dumps({**inner,marker:marker}))
            invoke(["--config",str(broken)],{},("CONFIG_FILE_INVALID","config","cli"))
            invoke(["--environment","development","--identity-file",str(source)],{},("CONFIG_VALUE_MISSING","database","default"))
        print("PASS actual",role,section,flush=True)


def malformed_http(output,private,env):
    with database(private/"database-url","orange_redaction_") as (_,url), tempfile.TemporaryDirectory() as folder:
        area=Path(folder)
        marker="sensitive-"+secrets.token_hex(16)
        identity=area/"identities.json"
        identity.write_text(json.dumps([{"owner_id":"diagnostic-owner","token":marker}]))
        database_file=area/"url"
        database_file.write_text(url)
        database_file.chmod(0o600)
        settings=area/"settings.json"
        settings.write_text(json.dumps({"environment":"development","identity_file":identity.name,"database_url_file":database_file.name,
                                        "bind":"127.0.0.1:0","diagnostic_log":"on"}))
        safe={k:v for k,v in env.items() if not k.startswith("ORANGE_") and k!="DATABASE_URL"}
        log=area/"stderr"
        with log.open("wb") as stream:
            child=subprocess.Popen([output/"orange-api","--config",settings],env=safe,cwd=area,stdout=subprocess.DEVNULL,stderr=stream)
            try:
                end=time.monotonic()+30
                while time.monotonic()<end and child.poll() is None:
                    match=re.search(r"Orange development API listening on 127\.0\.0\.1:(\d+)",log.read_text())
                    if match:
                        break
                    time.sleep(.03)
                else:
                    raise RuntimeError("Configured test API did not become ready")
                port=int(match[1])
                observed=set()
                for supplied,expected,classification in [(marker+"-denied",401,"permission"),(marker,400,"input")]:
                    conn=http.client.HTTPConnection("127.0.0.1",port,timeout=4)
                    try:
                        headers={"Authorization":"Bearer "+supplied,"Content-Type":"application/json","x-orange-diagnostic-id":marker,
                                 "Idempotency-Key":"local-redaction-example"}
                        conn.request("POST","/api/v1/budgets?secret="+marker,json.dumps({"unexpected":{"secret":marker}}),headers)
                        response=conn.getresponse()
                        require(response.status==expected and response.getheader("x-orange-error-class")==classification,"Wrong HTTP rejection")
                        body=json.loads(response.read())
                        require(response.getheader("x-orange-retry-class")==body["retry_class"],"HTTP retry disagrees with body")
                        observed.add(str(uuid.UUID(response.getheader("x-orange-diagnostic-id"))))
                    finally:
                        conn.close()
                require(len(observed)==2,"Diagnostic ID reused")
                end=time.monotonic()+3
                while time.monotonic()<end:
                    diagnostics={json.loads(l)["request_id"] for l in log.read_text().splitlines() if l.startswith('{"event":"http_completed"')}
                    if observed<=diagnostics:
                        break
                    time.sleep(.02)
                else:
                    raise ValueError("Completed rejections did not produce correlated diagnostics")
            finally:
                if child.poll() is None:
                    child.send_signal(signal.SIGINT)
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
                    raise RuntimeError("Configured development API failed to drain")
        require(marker not in log.read_text() and url not in log.read_text(),"HTTP diagnostics exposed a private value")
        print("PASS native file-configured API: structured authentication and input errors, distinct correlation IDs, redacted nested data")


def correlation(private,env):
    with database(private/"database-url","orange_correlation_") as (_,url):
        child={k:v for k,v in env.items() if not k.startswith("ORANGE_")}
        child.update(ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),
                     ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"),ORANGE_DIAGNOSTIC_LOG="on",
                     ORANGE_DIAGNOSTIC_CORRELATION_TEST="1")
        result=subprocess.run([sys.executable,ROOT/"scripts/check-worker-session.py"],env=child,cwd=ROOT,
                              stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
        if result.returncode:
            print(result.stdout[-4000:])
            raise RuntimeError("Current Worker correlation suite failed")
        http=re.search(r"^expected_job_event ([0-9a-f-]{36}) ([0-9a-f]{64})$",result.stdout,re.M)
        worker=re.search(r"^expected_assignment ([0-9a-f]{64}) ([0-9a-f]{64}) ([0-9a-f]{64})$",result.stdout,re.M)
        require(http and worker and http[2]==worker[1],"Expected Job and Worker references differ")
        uuid.UUID(http[1])
        events=[json.loads(line) for line in result.stdout.splitlines() if line.startswith('{"event":')]
        require(any(e["event"]=="http_completed" and e["request_id"]==http[1] and e["job"]==http[2] and e["status"]==200 for e in events),"Missing authorized HTTP event")
        require(any(e["event"]=="assignment_committed" and (e["job"],e["attempt"],e["node"])==worker.groups() for e in events),"Missing committed Worker assignment")
        require(all(set(e)<={"event","role","request_id","status","job","attempt","node","dropped_before"} for e in events),"Unexpected diagnostic data")
        require(all(x not in json.dumps(events) for x in ("must-not-enter-log","private-input-fixture","authorization")),"Unexpected raw field")
        print(json.dumps({"real_job_submission":True,"real_mtls_assignment":True,"joined_job_reference":http[2],"typed_events":events}))


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--section",choices=["settings","errors","correlation","invalid"],required=True)
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    args=cli.parse_args()
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    env=dict(os.environ,CARGO_NET_OFFLINE="true",PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
    private=args.environment.resolve()
    if args.section=="errors":
        result=subprocess.check_output([cargo,"test","--offline","--locked","-p","orange-control-plane","--lib","diagnostics::tests::"],cwd=ROOT,env=env,text=True)
        require("2 passed; 0 failed" in result,"Missing diagnostic classification cases")
        print(result)
    elif args.section=="correlation":
        correlation(private,env)
    else:
        bins=[arg for name,_,_ in ROLES.values() for arg in ("--bin",name)]
        subprocess.run([cargo,"build","--offline","--locked","-p","orange-control-plane",*bins],cwd=ROOT,env=env,check=True)
        target=json.loads(subprocess.check_output([cargo,"metadata","--format-version","1","--no-deps","--locked","--offline"],cwd=ROOT,env=env))["target_directory"]
        with tempfile.TemporaryDirectory() as tmp:
            settings(Path(tmp),Path(target)/"debug",private,args.section,env)
        if args.section=="invalid":
            malformed_http(Path(target)/"debug",private,env)
    print("PASS API diagnostics",args.section,flush=True)


if __name__=="__main__":
    main()
