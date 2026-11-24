#!/usr/bin/env python3.12
"""Run the actual local User API with owned PostgreSQL and OpenDAL, without a hardware or billing claim."""
import argparse
from hashlib import sha256
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import secrets
import shutil
import struct
import subprocess
import tempfile
from urllib.parse import unquote, urlencode, urlsplit
from uuid import uuid4
import zlib

from owned_postgres import database
from platform_package_smoke import listening

ROOT=Path(__file__).resolve().parents[1]


def check(ok, message):
    if not ok:raise ValueError(message)


def sample():
    def chunk(tag,data):
        return struct.pack(">I",len(data))+tag+data+struct.pack(">I",zlib.crc32(tag+data))
    row=bytes(i%251 for i in range(3072))
    return (b"\x89PNG\r\n\x1a\n"+chunk(b"IHDR",struct.pack(">IIBBBBB",1024,1024,8,2,0,0,0))+
            chunk(b"IDAT",zlib.compress((b"\0"+row)*1024))+chunk(b"IEND",b"")),sha256(row*1024).hexdigest()


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--environment",type=Path,default=ROOT/".local")
    parser.add_argument("--api",type=Path,default=ROOT/"target/debug/orange-user-api")
    parser.add_argument("--png",type=Path)
    parser.add_argument("--rgb-sha256")
    parser.add_argument("--browser-driver",type=Path,help="Optional Orange Web browser acceptance script")
    parser.add_argument("--node",type=Path)
    parser.add_argument("--static-root",type=Path)
    parser.add_argument("--browser-tls",type=Path)
    args=parser.parse_args()
    check(bool(args.node)==bool(args.browser_driver),"Browser driver requires its pinned Node executable")
    check(bool(args.png)==bool(args.rgb_sha256),"Real PNG and independently measured pixel digest must be supplied together")
    payload,pixel_hash=(args.png.read_bytes(),args.rgb_sha256) if args.png else sample()
    config=args.environment.resolve()/"identity/user-config.json"
    origin=json.loads(config.read_bytes())["user_origin"]
    with tempfile.TemporaryDirectory() as temp,database(args.environment.resolve()/"database-url","orange_image_read_") as (_,url):
        db=urlsplit(url)
        pg=dict(os.environ,PGHOST=db.hostname,PGPORT=str(db.port or 5432),PGDATABASE=db.path[1:],
                PGUSER=unquote(db.username),PGPASSWORD=unquote(db.password))
        def sql(statement):
            return subprocess.check_output([shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql","-X","-w","-Atq","-v","ON_ERROR_STOP=1"],
                                           input=statement,env=pg,text=True).strip()
        settings=dict(PATH=os.environ["PATH"],ORANGE_ENV="development",DATABASE_URL=url,ORANGE_USER_CONFIG_FILE=str(config),
                      ORANGE_USER_BIND="127.0.0.1:0",ORANGE_ARTIFACT_ROOT=temp)
        with listening(args.api.resolve(),settings,"User API") as address:
            def identity():
                who=str(uuid4());token=secrets.token_hex(32);csrf=secrets.token_hex(32)
                sql(f"""INSERT INTO owners(owner_id) VALUES('{who}');
                    INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{who}','image-inspection-dev','{who}','{who}');
                    INSERT INTO wallets(owner_id,gift) VALUES('{who}',1000);
                    INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                    SELECT '{sha256(token.encode()).hexdigest()}','{uuid4()}','{who}','{csrf}',t,t+600,t+3600 FROM (SELECT floor(extract(epoch FROM now()))::bigint t) clock;""")
                return who,token,csrf
            owner,token,csrf=identity()
            _,other,_=identity()
            def wire(method,path,body=None,fields=None,cookie=token):
                conn=HTTPConnection("127.0.0.1",urlsplit(address).port,timeout=25)
                try:
                    headers=dict(fields or {})
                    if cookie:headers["Cookie"]="__Host-orange-user="+cookie
                    conn.request(method,path,body=body,headers=headers)
                    reply=conn.getresponse()
                    raw=reply.read()
                    return reply.status,reply.getheader("Cache-Control"),json.loads(raw) if raw else None
                finally:conn.close()
            def funds():
                return sql(f"""SELECT (SELECT gift FROM wallets WHERE owner_id='{owner}'),(SELECT reserved_gift FROM wallets WHERE owner_id='{owner}'),
                     (SELECT count(*) FROM ledger_transactions),(SELECT count(*) FROM jobs),(SELECT count(*) FROM job_response_commits),
                     (SELECT count(*) FROM job_settlements)""")
            uploads=f"/api/v1/owners/{owner}/uploads"
            def create(data,media):
                upload=str(uuid4())
                fields={"Origin":origin,"X-CSRF-Token":csrf,"Idempotency-Key":upload,"Content-Type":"application/json"}
                status,_,view=wire("POST",uploads,json.dumps({"upload_id":upload,"size_bytes":str(len(data)),"sha256":sha256(data).hexdigest(),"media_type":media}),fields)
                check(status==200,"Upload intent failed: "+str(status))
                path=uploads+"/"+upload
                fields["Content-Type"]="application/octet-stream"
                status,_,view=wire("PUT",path+"/content",data,fields)
                check(status==200 and view["state"]=="available","Immutable upload missing")
                return path,view,fields
            saved,view,headers=create(payload,"image/png")
            def endpoint(path,version):
                return path+"/image-output-inspection?"+urlencode({"version_id":version})
            read=endpoint(saved,view["version_id"])
            for _ in range(2):
                status,cache,measured=wire("GET",read)
                check(status==200 and cache=="no-store" and measured=={"artifact_id":view["artifact_id"],"version_id":view["version_id"],
                    "content":{"width":1024,"height":1024,"bytes":len(payload),"media_type":"image/png","sha256":sha256(payload).hexdigest(),"rgb_sha256":pixel_hash}},
                    "Different image measurement or cache behavior")
            for label,path,cookie,accepted in [
                ("foreign",read,other,{403,404}),("signed-out",read,None,{401}),("unknown-version",endpoint(saved,"unknown"),token,{409}),
                ("duplicate-version",read+"&version_id="+view["version_id"],token,{422}),("unknown-query",read+"&amount=100",token,{422})]:
                status,cache,error=wire("GET",path,cookie=cookie)
                check(status in accepted and cache=="no-store" and "content" not in error,"Data disclosed for "+label)
            for label,data,media in [("corrupt PNG",payload+b"x","image/png"),("incorrect MIME",payload,"application/octet-stream"),("progress only",b'{"phase":"denoising"}',"image/png")]:
                target,version,_=create(data,media)
                status,cache,error=wire("GET",endpoint(target,version["version_id"]))
                check(status==422 and cache=="no-store" and "content" not in error,label+" incorrectly measured")
            if args.browser_driver:
                browser_token=secrets.token_hex(32)
                browser_csrf=secrets.token_hex(32)
                sql(f"""INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                    SELECT '{sha256(browser_token.encode()).hexdigest()}','{uuid4()}','{owner}','{browser_csrf}',t,t+600,t+3600
                    FROM (SELECT floor(extract(epoch FROM now()))::bigint t) clock""")
                data={"api":address,"authorityOrigin":origin,"owner":owner,"token":browser_token,"csrf":browser_csrf,"upload":view["upload_id"],
                      "version":view["version_id"],"rgb":pixel_hash,"sha256":sha256(payload).hexdigest()}
                if args.static_root:data["staticRoot"]=str(args.static_root.resolve(strict=True))
                if args.browser_tls:data["tls"]=str(args.browser_tls.resolve(strict=True))
                subprocess.run([args.node.resolve(strict=True),args.browser_driver.resolve(strict=True)],input=json.dumps(data),text=True,timeout=100,check=True)
            status,_,_=wire("DELETE",saved,fields=headers)
            check(status==200,"Delete must succeed")
            status,_,error=wire("GET",read)
            check(status in {403,404,409,410} and "content" not in error,"Retired output remained measurable")
            check(funds()=="1000|0|0|0|0|0","Content inspection changed payment or job state")
            print(json.dumps({"source":"actual PNG" if args.png else "fixed sample","rgb_sha256":pixel_hash,"png_bytes":len(payload),
                              "authorized_repeat_reads":2,"rejected_reads":9,"ledger_transactions":0,"jobs":0,"status":"PASS"}),flush=True)


if __name__=="__main__":
    main()
