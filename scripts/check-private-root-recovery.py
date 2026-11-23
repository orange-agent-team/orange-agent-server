#!/usr/bin/env python3
"""Restore an actual older pg_dump into a new quarantined DB and exercise the recovered object root."""
import argparse
from contextlib import ExitStack
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
from urllib.parse import unquote,urlsplit
import uuid

from owned_postgres import database
from platform_package_smoke import listening
from verified_platform_archive import binaries,unpack_archive

ROOT=Path(__file__).resolve().parents[1]


def require(value,message):
    if not value: raise RuntimeError(message)


def api(origin,method,path,session,csrf,site,key=None,payload=None):
    db=http.client.HTTPConnection("127.0.0.1",urlsplit(origin).port,timeout=5)
    try:
        headers={"Cookie":"__Host-orange-user="+session}
        if method!="GET":
            headers.update({"Origin":site,"X-CSRF-Token":csrf,"Idempotency-Key":key,"Content-Type":"application/json"})
        db.request(method,path,payload,headers)
        response=db.getresponse()
        return response.status,response.read(65536)
    finally:db.close()


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--environment",type=Path,default=ROOT/".local")
    candidate=parser.add_mutually_exclusive_group(required=True)
    candidate.add_argument("--current-bin",type=Path)
    candidate.add_argument("--installed",action="store_true")
    previous=parser.add_mutually_exclusive_group()
    previous.add_argument("--baseline-bin",type=Path)
    previous.add_argument("--legacy-archive",type=Path)
    parser.add_argument("--legacy-sha256")
    args=parser.parse_args()
    with ExitStack() as stack:
        if args.installed:
            native=stack.enter_context(binaries(None))
            current=native/"orange-user-api"
            tool=native.parent/"recover-private-artifacts.py"
        else:
            current=args.current_bin.resolve(strict=True)
            tool=ROOT/"distribution/platform/recover-private-artifacts.py"
        if args.legacy_archive:
            require(args.legacy_sha256 and re.fullmatch(r"[0-9a-f]{64}",args.legacy_sha256),"trusted historical digest required")
            legacy=stack.enter_context(unpack_archive(args.legacy_archive,args.legacy_sha256))
            old=legacy/"bin/orange-user-api"
        else:
            old=args.baseline_bin.resolve(strict=True) if args.baseline_bin else current
        verify(args.environment.resolve(),current,old,[sys.executable,str(tool)])


def verify(private,current,old,tool):
    cfg=private/"identity/user-config.json";site=json.loads(cfg.read_bytes())["user_origin"]
    require(site=="https://127.0.0.1:43122","dedicated development config required")
    def executable(name):
        return shutil.which(name) or "/opt/homebrew/opt/postgresql@17/bin/"+name
    def run(args,expected=True):
        reply=subprocess.run(args,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        if (reply.returncode==0)!=expected:
            raise RuntimeError("Unexpected "+str(Path(args[1]).name)+" result: "+reply.stderr.decode(errors="replace")[:420])
        return reply.stdout
    with database(private/"database-url","orange_restore_test_") as (_,url),tempfile.TemporaryDirectory() as ws:
        work=Path(ws).resolve()
        conn=urlsplit(url)
        denv=dict(os.environ,PGHOST=conn.hostname,PGPORT=str(conn.port),PGUSER=unquote(conn.username),PGPASSWORD=unquote(conn.password or ""),PGDATABASE=conn.path[1:])
        def sql(stmt,db=None):
            return subprocess.check_output([executable("psql"),"-X","-w","-qAt","-v","ON_ERROR_STOP=1"],input=stmt,text=True,
                                           env=dict(denv,PGDATABASE=db or conn.path[1:])).strip()
        source=work/"source-url";source.touch(mode=0o600);source.write_text(url+"\n")
        objects=work/"objects";objects.mkdir(mode=0o700)
        runtime=dict(PATH="/usr/bin:/bin",DATABASE_URL=url,ORANGE_ENV="development",ORANGE_USER_BIND="127.0.0.1:0",
                     ORANGE_USER_CONFIG_FILE=str(cfg),ORANGE_ARTIFACT_ROOT=str(objects))
        owner,session_id=str(uuid.uuid4()),str(uuid.uuid4());token,csrf=secrets.token_hex(32),secrets.token_hex(32)
        key=work/"authority.pem";pub=work/"authority.pub";pub2=work/"wrong.pub"
        openssl="/opt/homebrew/opt/openssl@3/bin/openssl" if Path("/opt/homebrew/opt/openssl@3/bin/openssl").is_file() else "openssl"
        for dest,public in [(key,pub),(work/"other.pem",pub2)]:
            subprocess.run([openssl,"genpkey","-algorithm","RSA","-pkeyopt","rsa_keygen_bits:3072","-out",dest],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=True)
            dest.chmod(0o600)
            subprocess.run([openssl,"pkey","-in",dest,"-pubout","-out",public],check=True)
        copy=work/"restored-objects";backup=work/"older.dump";out=work/"quarantine"
        refused=subprocess.run([current,"--migrate-private-restore"],env=runtime,capture_output=True,timeout=10)
        require(refused.returncode!=0,"offline migration accepted a normal application database")
        with listening(old,runtime,"User API") as origin:
            sql(f"""INSERT INTO owners(owner_id) VALUES('{owner}'); INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{owner}','test','{owner}','{owner}');
                INSERT INTO wallets(owner_id,gift) VALUES('{owner}',1000); INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                VALUES('{hashlib.sha256(token.encode()).hexdigest()}','{session_id}','{owner}','{csrf}',0,floor(extract(epoch from clock_timestamp()))::bigint+1800,floor(extract(epoch from clock_timestamp()))::bigint+1800);""")
            base=f"/api/v1/owners/{owner}/uploads"
            def create(body):
                id=str(uuid.uuid4());payload=body.encode()
                status,response=api(origin,"POST",base,token,csrf,site,id,json.dumps(dict(upload_id=id,sha256=hashlib.sha256(payload).hexdigest(),size_bytes=str(len(payload)),media_type="text/plain")))
                require(status==200,"historical create failed")
                status,response=api(origin,"PUT",base+"/"+id+"/content",token,csrf,site,id,payload)
                require(status==200,"historical publish failed")
                return json.loads(response)
            deleted=create("private removed fixture");retained=create("private retained fixture")
            root_id=(objects/".store-id").read_text().strip()
            root_args=["--root-id",root_id]
            def export(urlpath,dest):
                return json.loads(run(tool+["export",*root_args,"--database-url-file",urlpath,"--destination",dest,"--signing-key",key]))
            empty=export(source,work/"old-head")
            require(empty["facts"]==0 and export(source,work/"old-head")==empty,"export did not replay the same original snapshot")
            subprocess.run([executable("pg_dump"),"-w","--format=custom","--file",backup],env=denv,check=True)
            shutil.copytree(objects,copy)
            absent=create("newer than backup")
            for artifact in (deleted,absent):
                uid=artifact["upload_id"]
                require(api(origin,"DELETE",base+"/"+uid,token,csrf,site,uid)[0]==200,"original user delete")
            journal=work/"current-head";head=export(source,journal)
            require(head["facts"]==2,"missing independent deletion facts")
            run(tool+["export",*root_args,"--database-url-file",source,"--destination",work/"old-head","--signing-key",key],False)
        with backup.open("rb") as stream:dump_hash=hashlib.file_digest(stream,"sha256").hexdigest()
        restore=tool+["restore",*root_args,"--database-url-file",source,"--destination",out,"--dump",backup,"--dump-sha256",dump_hash,
                     "--manifest-sha256",head["manifest_sha256"],"--user-api",current]
        run(restore+["--journal",work/"old-head","--verification-key",pub],False)
        run(restore+["--journal",journal,"--verification-key",pub2],False)
        require(not out.exists(),"untrusted evidence created a recovery database")
        corrupt=work/"altered";shutil.copytree(journal,corrupt)
        (corrupt/"deletions.jsonl").write_bytes(b"")
        run(restore+["--journal",corrupt,"--verification-key",pub],False)
        call=restore+["--journal",journal,"--verification-key",pub]
        try:
            receipt=json.loads(run(call));name=receipt["database"]
            require(re.fullmatch(r"orange_recovered_[0-9a-f]{32}",name),"unexpected recovery identity")
            require(receipt["hidden_from_backup"]==1 and receipt["absent_from_backup"]==1 and not receipt["connections_enabled"],"incomplete replay")
            require(json.loads(run(call))==receipt,"retry did not return original quarantine")
            # Discard only the local response to simulate a crash after PostgreSQL's
            # replay transaction committed but before the final receipt was published.
            (out/"receipt.json").unlink()
            require(json.loads(run(call))==receipt,"resume after lost receipt changed the original result")
            require(sql(f"SELECT datallowconn FROM pg_database WHERE datname='{name}'")=="f","restored DB admitted traffic")
            # Only this test-owned clone is temporarily opened to exercise the normal API.
            sql(f'ALTER DATABASE "{name}" ALLOW_CONNECTIONS true;')
            restored=(out/"database-url").read_text().strip()
            require(sql("SELECT count(*) FROM artifact_restore_suppressions",name)=="2","absent identity lost")
            require(sql(f"SELECT gift FROM wallets WHERE owner_id='{owner}'",name)=="1000","finance changed")
            require(sql("SELECT count(*) FROM artifact_restore_manifests",name)=="1","replay created duplicate audit identity")
            aid=str(uuid.UUID(absent["artifact_id"]));version=str(uuid.UUID(absent["version_id"]))
            sql(f"""DO $$ BEGIN BEGIN
                    INSERT INTO artifacts(artifact_id,version_id,root_id,owner_id,creator_id,sha256,size_bytes,media_type)
                    VALUES('{aid}','{version}','{root_id}','{owner}','{owner}',repeat('a',64),1,'text/plain');
                    RAISE EXCEPTION 'retired version revived';
                  EXCEPTION WHEN check_violation THEN
                    IF SQLERRM<>'retired private object identity' THEN RAISE; END IF;
                  END; END $$;""",name)
            require(export(out/"database-url",work/"reexport")["manifest_sha256"]==head["manifest_sha256"],"recovery chain lost a suppression")
            with listening(current,dict(runtime,DATABASE_URL=restored,ORANGE_ARTIFACT_ROOT=str(copy)),"User API") as recovered:
                def read(d):
                    path=base+"/"+d["upload_id"]+"/content?version_id="+d["version_id"]
                    return api(recovered,"GET",path,token,csrf,site)
                require(read(deleted)[0]==410,"deleted bytes revived")
                require(read(retained)==(200,b"private retained fixture"),"unrelated content changed")
            print("PASS signed later deletion snapshot, stale/altered/wrong-key rejection, closed restore, original-file denial, independent content and ledger retained, same request and re-export")
        finally:
            for file in [out/"receipt.json",out/"quarantined-database"]:
                if file.is_file():
                    name=json.loads(file.read_bytes())["database"] if file.suffix==".json" else file.read_text().strip()
                    require(re.fullmatch(r"orange_recovered_[0-9a-f]{32}",name),"refuse unexpected database cleanup")
                    sql(f'ALTER DATABASE "{name}" ALLOW_CONNECTIONS false;')
                    sql(f'DROP DATABASE "{name}";')
                    break


if __name__=="__main__":
    main()
