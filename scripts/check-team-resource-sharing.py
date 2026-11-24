#!/usr/bin/env python3.12
"""Check session-bound resource ACL mutations and budget-independent team image reads on real local services."""
import argparse
import concurrent.futures
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import runpy
import secrets
import shutil
import socket
import subprocess
import tempfile
from urllib.parse import unquote,urlsplit,urlencode
from uuid import uuid4

from owned_postgres import database
from platform_package_smoke import listening

ROOT=Path(__file__).resolve().parents[1]


def check(ok,msg):
    if not ok:raise ValueError(msg)


def main():
    p=argparse.ArgumentParser()
    p.add_argument("--api",type=Path,required=True)
    p.add_argument("--environment",type=Path,default=ROOT/".local")
    p.add_argument("--node",type=Path)
    p.add_argument("--driver",type=Path)
    p.add_argument("--site",type=Path)
    p.add_argument("--tls",type=Path)
    args=p.parse_args()
    check(all((args.node,args.driver,args.site,args.tls)) or not any((args.node,args.driver,args.site,args.tls)),"Specify all browser fixture paths")
    png,rgb=runpy.run_path(str(ROOT/"scripts/check-authorized-image-inspection.py"))["sample"]()
    with tempfile.TemporaryDirectory() as tmp,database(args.environment.resolve()/"database-url","orange_team_image_") as (_,url):
        temp=Path(tmp)
        config=json.loads((args.environment.resolve()/"identity/user-config.json").read_bytes())
        with socket.socket() as choose:
            choose.bind(("127.0.0.1",0))
            port=choose.getsockname()[1]
        origin=f"https://127.0.0.1:{port}"
        config["user_origin"]=origin
        cfg=temp/"user-config.json"
        with os.fdopen(os.open(cfg,os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600),"w") as private:json.dump(config,private)
        del config
        artifact_dir=temp/"objects";artifact_dir.mkdir(mode=0o700)
        db=urlsplit(url)
        pg=dict(os.environ,PGHOST=db.hostname,PGPORT=str(db.port or 5432),PGDATABASE=db.path[1:],PGUSER=unquote(db.username),PGPASSWORD=unquote(db.password))
        def sql(statement):
            return subprocess.check_output([shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql","-X","-w","-Atq","-v","ON_ERROR_STOP=1"],env=pg,input=statement,text=True).strip()
        settings=dict(PATH=os.environ["PATH"],ORANGE_ENV="development",DATABASE_URL=url,ORANGE_USER_CONFIG_FILE=str(cfg),ORANGE_USER_BIND="127.0.0.1:0",ORANGE_ARTIFACT_ROOT=str(artifact_dir))
        with listening(args.api.resolve(strict=True),settings,"User API") as address:
            org=str(uuid4())
            sql(f"INSERT INTO owners VALUES('{org}'); INSERT INTO organizations(organization_id,name) VALUES('{org}','Image inspection team');")
            def member(role):
                user=str(uuid4());token=secrets.token_hex(32);csrf=secrets.token_hex(32)
                sql(f"""INSERT INTO owners(owner_id) VALUES('{user}');
                        INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{user}','image-team-dev','{user}','{user}');
                        INSERT INTO memberships(organization_id,user_id,role) VALUES('{org}','{user}','{role}');
                        INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                        SELECT '{hashlib.sha256(token.encode()).hexdigest()}','{uuid4()}','{user}','{csrf}',t,t+600,t+3600 FROM (SELECT floor(extract(epoch FROM now()))::bigint t) clock""")
                return {"user":user,"token":token,"csrf":csrf}
            author,viewer,owner,billing=[member(role) for role in ["member","viewer","owner","billing"]]
            def request(method,path,actor,body=None,key=None):
                h={"Cookie":"__Host-orange-user="+actor["token"]}
                if key:h.update({"Origin":origin,"X-CSRF-Token":actor["csrf"],"Idempotency-Key":key})
                if isinstance(body,dict):h["Content-Type"]="application/json";body=json.dumps(body).encode()
                elif body is not None:h["Content-Type"]="application/octet-stream"
                conn=HTTPConnection("127.0.0.1",urlsplit(address).port,timeout=18)
                try:
                    conn.request(method,path,body=body,headers=h);res=conn.getresponse();raw=res.read()
                    check(res.getheader("Cache-Control")=="no-store","Missing private cache boundary")
                    return res.status,json.loads(raw) if raw else None
                finally:conn.close()
            upload=str(uuid4());sha=hashlib.sha256(png).hexdigest();collection=f"/api/v1/owners/{org}/uploads"
            status,obj=request("POST",collection,author,{"upload_id":upload,"sha256":sha,"size_bytes":str(len(png)),"media_type":"image/png"},upload)
            check(status==200,"Team upload denied")
            status,obj=request("PUT",collection+"/"+upload+"/content",author,png,upload)
            check(status==200 and obj["state"]=="available","Team bytes unavailable")
            resource=obj["artifact_id"];version=obj["version_id"]
            inspect=collection+"/"+upload+"/image-output-inspection?"+urlencode({"version_id":version})
            sharing=f"/api/v1/organizations/{org}/resources/{resource}/sharing"
            def list_grants():
                reply=request("GET",sharing,author)
                check(reply[0]==200 and reply[1]["resource_id"]==resource and reply[1]["creator_id"]==author["user"],"Wrong creator resource")
                return reply[1]
            def patch(actor,target,access,expected,key):
                return request("POST",sharing,actor,{"user_id":target["user"],"access":access,"expected_acl_version":expected},key)
            initial=list_grants();v=initial["acl_version"]
            check(initial["data"]==[{"user_id":author["user"],"access":"write"}],"Unexpected initial recipients")
            for principal in [viewer,owner,billing]:
                check(request("GET",inspect,principal)[0] in (403,404),"Membership exposed private pixels")
                check(request("GET",sharing,principal)[0] in (403,404),"Membership exposed grant list")
                check(patch(principal,viewer,"read",v,str(uuid4()))[0] in (403,404),"Non-creator changed grants")
            check(request("POST",sharing,author,{"user_id":viewer["user"],"expected_acl_version":v},str(uuid4()))[0]==422,"Omitted grant action accepted")
            key=str(uuid4())
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                left,right=pool.map(lambda _:patch(author,viewer,"read",v,key),range(2))
            check(left==right and left[0]==200 and left[1]["acl_version"]==str(int(v)+1),"Concurrent grant changed more than once")
            check(patch(author,viewer,"write",v,key)[0]==409 and patch(author,viewer,"write",v,str(uuid4()))[0]==409,"Key or stale ACL accepted")
            check(list_grants()["acl_version"]==left[1]["acl_version"],"Failed grant changed resource version")
            status,value=request("GET",inspect,viewer)
            check(status==200 and value["content"]["rgb_sha256"]==rgb,"Explicit read failed")
            check(request("GET",sharing,viewer)[0]==403,"Read grant authorized managing grants")
            check(request("GET",inspect,billing)[0]==403,"Billing-only identity saw content")
            if args.driver:
                fixture={"api":address,"origin":origin,"site":str(args.site.resolve(strict=True)),"tls":str(args.tls.resolve(strict=True)),"team":org,"resource":resource,
                         "upload":upload,"version":version,"rgb":rgb,"sha256":sha,"author":author,"viewer":viewer,"billing":billing}
                subprocess.run([args.node.resolve(strict=True),args.driver.resolve(strict=True)],input=json.dumps(fixture),text=True,check=True,timeout=100)
                check(request("GET",inspect,viewer)[0]==403,"UI revocation did not remove content access")
            else:
                check(patch(author,viewer,None,list_grants()["acl_version"],str(uuid4()))[0]==200,"Revoke failed")
            check(request("GET",inspect,viewer)[0]==403 and request("GET",inspect,owner)[0]==403,"Revoked or implicit content access")
            effects=sql("SELECT (SELECT count(*) FROM jobs),(SELECT count(*) FROM ledger_transactions),(SELECT count(*) FROM budget_roots)")
            check(effects=="0|0|0","Resource sharing created financial or job state")
            print(json.dumps({"real_team_http":True,"concurrent_grant_effects":1,"viewer_read_without_budget":True,
                              "owner_role_only_read":False,"revoked_reader_denied":True,"jobs":0,"ledger_transactions":0,"budget_roots":0}),flush=True)


if __name__=="__main__":main()
