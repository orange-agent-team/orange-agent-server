#!/usr/bin/env python3
"""Export signed object deletion facts and replay them into a freshly restored, offline development database."""
import argparse
import csv
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from urllib.parse import unquote, urlsplit, urlunsplit
import uuid

HERE = Path(__file__).resolve().parent
FORMAT = "orange-private-artifact-redactions.v1"
HEX = re.compile(r"[0-9a-f]{64}")
ID = re.compile(r"[a-zA-Z0-9_.:-]{1,128}")
STAMP = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z")


def binary(name):
    if name=="openssl" and Path("/opt/homebrew/opt/openssl@3/bin/openssl").is_file():
        return "/opt/homebrew/opt/openssl@3/bin/openssl"
    found = shutil.which(name)
    if found:
        return found
    prefix = "/opt/homebrew/opt/openssl@3/bin" if name == "openssl" else "/opt/homebrew/opt/postgresql@17/bin"
    return str((Path(prefix)/name).resolve(strict=True))


def check(value, message):
    if not value:
        raise ValueError(message)


def url_file(path):
    if path.is_symlink() or not stat.S_ISREG(path.stat().st_mode) or path.stat().st_mode & 0o077:
        raise ValueError("Private regular database URL file required")
    url = urlsplit(path.read_text().strip())
    check(url.scheme in ("postgres","postgresql") and url.hostname in ("127.0.0.1","localhost","::1")
          and url.username and ID.fullmatch(url.path[1:]) and not url.query and not url.fragment,
          "Dedicated loopback PostgreSQL URL required")
    return url


def connection(url, name=None):
    env = {k:v for k,v in os.environ.items() if not k.startswith("PG")}
    env.update(PGHOST=url.hostname,PGPORT=str(url.port or 5432),PGDATABASE=name or url.path[1:],
               PGUSER=unquote(url.username),PGPASSWORD=unquote(url.password or ""),PGCONNECT_TIMEOUT="5",
               PGOPTIONS="-c lock_timeout=10s -c search_path=pg_catalog,public",PGCLIENTENCODING="UTF8")
    return env


def sql(env, command, *, stdout=subprocess.PIPE):
    run=subprocess.run([binary("psql"),"-X","-w","-qAt","-v","ON_ERROR_STOP=1"],env=env,
                       input=command.encode(),stdout=stdout,stderr=subprocess.PIPE)
    check(run.returncode==0,"Recovery database operation failed")
    return run.stdout


def canonical_uuid(value):
    check(str(uuid.UUID(value))==value,"Canonical object root UUID required")
    return value


def tool_crypto(key, message, signature, verify):
    action=["-verify",str(key),"-signature",str(signature)] if verify else ["-sign",str(key),"-out",str(signature)]
    run=subprocess.run([binary("openssl"),"dgst","-sha256",*action,"-sigopt","rsa_padding_mode:pss","-sigopt","rsa_pss_saltlen:digest"],
                       input=message,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    check(run.returncode==0,"Recovery authority signature verification or signing failed")


def scan(stream, copy=None):
    digest=hashlib.sha256()
    count=0
    previous=""
    writer=csv.writer(copy,lineterminator="\n") if copy is not None else None
    while True:
        line=stream.readline(4097)
        if not line:
            break
        check(len(line)<=4096 and line.endswith(b"\n"),"Invalid deletion fact size")
        row=json.loads(line)
        check(isinstance(row,list) and len(row)==5 and all(isinstance(v,str) for v in row),"Invalid deletion fact shape")
        aid,version,owner,content,at=row
        check(all(ID.fullmatch(v) for v in (aid,version,owner)) and HEX.fullmatch(content)
              and STAMP.fullmatch(at) and previous<aid,"Invalid or repeated deletion fact")
        previous=aid
        count+=1
        digest.update(line)
        if writer:
            writer.writerow(row)
    return count,digest.hexdigest()


def export(args):
    root=canonical_uuid(args.root_id)
    url=url_file(args.database_url_file)
    env=connection(url)
    env["PGOPTIONS"]+=" -c default_transaction_read_only=on"
    check(not args.signing_key.is_symlink() and stat.S_ISREG(args.signing_key.stat().st_mode) and args.signing_key.stat().st_mode & 0o077==0,"Private recovery signing key required")
    target=args.destination
    with tempfile.TemporaryDirectory(prefix=".orange-deletion-export-",dir=target.parent) as temporary:
        scratch=Path(temporary)
        stage=scratch/"sealed";stage.mkdir(mode=0o700)
        manifest,message=write_snapshot(stage,root,env,args.signing_key)
        try:
            stage.rename(target)
        except OSError:
            if not target.exists(): raise
            check(target.is_dir() and not target.is_symlink() and target.stat().st_mode&0o077==0,
                  "Existing destination is not a private journal")
            check((target/"manifest.json").read_bytes()==message,"Source changed or destination belongs to a different checkpoint")
            public=scratch/"verification.pem"
            result=subprocess.run([binary("openssl"),"pkey","-in",args.signing_key,"-pubout","-out",public],capture_output=True)
            check(result.returncode==0,"Could not verify the original signing authority")
            tool_crypto(public,message,target/"manifest.sig",True)
            with (target/"deletions.jsonl").open("rb") as records:
                check(scan(records)==(manifest["count"],manifest["sha256"]),"Original deletion archive differs")
        directory=os.open(target.parent,os.O_RDONLY)
        try: os.fsync(directory)
        finally: os.close(directory)
        print(json.dumps({"manifest_sha256":hashlib.sha256(message).hexdigest(),"facts":manifest["count"],"root_id":root}))


def write_snapshot(target,root,env,key):
    data=target/"deletions.jsonl"
    # Check existence even when the signed snapshot contains zero deleted versions.
    check(sql(env,f"SELECT count(*) FROM artifact_capacity WHERE root_id='{root}'").strip()==b"1","Unknown source object root")
    nested=sql(env,"SELECT to_regclass('public.artifact_restore_suppressions') IS NOT NULL").strip()==b"t"
    history = f"SELECT artifact_id,version_id,owner_id,sha256,deleted_at FROM artifacts a WHERE root_id='{root}' AND state='deleted'"
    if nested:
        history+=f""" AND NOT EXISTS(SELECT FROM artifact_restore_suppressions f WHERE f.artifact_id=a.artifact_id)
            UNION ALL SELECT artifact_id,version_id,original_owner,content_digest,source_deleted_at FROM artifact_restore_suppressions WHERE root_id='{root}'"""
    with data.open("xb") as stream:
        sql(env,f"""SELECT jsonb_build_array(artifact_id,version_id,owner_id,sha256,
               to_char(deleted_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'))::text
               FROM ({history}) facts ORDER BY artifact_id COLLATE "C";""",stdout=stream)
        stream.flush();os.fsync(stream.fileno())
    with data.open("rb") as stream:
        count,digest=scan(stream)
    manifest={"format":FORMAT,"root_id":root,"count":count,"sha256":digest}
    message=(json.dumps(manifest,sort_keys=True,separators=(",",":"))+"\n").encode()
    (target/"manifest.json").write_bytes(message)
    tool_crypto(key,message,target/"manifest.sig",False)
    for path in ("manifest.json","manifest.sig"):
        with (target/path).open("rb") as f:os.fsync(f.fileno())
    fd=os.open(target,os.O_RDONLY)
    try: os.fsync(fd)
    finally: os.close(fd)
    return manifest,message


def restore(args):
    root=canonical_uuid(args.root_id)
    admin=url_file(args.database_url_file)
    check(HEX.fullmatch(args.dump_sha256) and HEX.fullmatch(args.manifest_sha256),"Independent trusted digests required")
    manifest_path=args.journal/"manifest.json"
    check(manifest_path.stat().st_size<=4096 and (args.journal/"manifest.sig").stat().st_size<=16384,"Oversized recovery envelope")
    message=manifest_path.read_bytes()
    check(hashlib.sha256(message).hexdigest()==args.manifest_sha256,"Snapshot differs from the trusted recovery head")
    tool_crypto(args.verification_key,message,args.journal/"manifest.sig",True)
    manifest=json.loads(message)
    check(isinstance(manifest,dict) and set(manifest)=={"format","root_id","count","sha256"}
          and manifest["format"]==FORMAT and manifest["root_id"]==root and type(manifest["count"]) is int,"Wrong journal identity")
    with tempfile.TemporaryDirectory(prefix="orange-private-restore-") as work:
        work=Path(work);dump=work/"database.dump";command_file=work/"replay.sql"
        digest=hashlib.sha256()
        with args.dump.open("rb") as source,dump.open("xb") as dest:
            for chunk in iter(lambda:source.read(1024*1024),b""):
                digest.update(chunk);dest.write(chunk)
        check(digest.hexdigest()==args.dump_sha256,"Backup differs from trusted dump digest")
        with command_file.open("x",encoding="utf-8",newline="") as command,(args.journal/"deletions.jsonl").open("rb") as source:
            command.write(f"BEGIN; SET LOCAL orange.restore_root='{root}'; SET LOCAL orange.restore_hash='{args.manifest_sha256}'; SET LOCAL orange.restore_dump='{args.dump_sha256}';\n")
            command.write("CREATE TEMP TABLE restore_facts(artifact_id text PRIMARY KEY,version_id text NOT NULL UNIQUE,owner_id text NOT NULL,content_digest text NOT NULL,deleted_at timestamptz NOT NULL) ON COMMIT DROP;\nCOPY restore_facts FROM STDIN WITH(FORMAT csv);\n")
            count,hash_value=scan(source,command)
            check(count==manifest["count"] and hash_value==manifest["sha256"],"Incomplete or altered deletion facts")
            command.write("\\.\n");command.write((HERE/"sql/replay-artifact-redactions.sql").read_text())
        migrator=args.user_api.resolve(strict=True) if args.user_api else (HERE/"bin/orange-user-api").resolve(strict=True)
        if (HERE/"manifest.json").is_file():
            packaged=(HERE/"bin/orange-user-api").resolve(strict=True)
            check(migrator==packaged,"A packaged restore must use its bundled migrator")
            with packaged.open("rb") as exe:
                check(hashlib.file_digest(exe,"sha256").hexdigest()==json.loads((HERE/"manifest.json").read_bytes())["sha256"]["bin/orange-user-api"],
                      "Packaged migrator differs from its release manifest")
        try:
            args.destination.mkdir(mode=0o700)
        except FileExistsError:
            pass
        folder=args.destination
        check(not folder.is_symlink() and folder.is_dir() and folder.stat().st_mode&0o077==0 and folder.stat().st_uid==os.getuid(),"Private restore work directory required")
        fd=os.open(folder/"restore.lock",os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW,0o600)
        with os.fdopen(fd,"rb") as lock:
            fcntl.flock(lock,fcntl.LOCK_EX)
            proof=restore_locked(args,admin,root,dump,command_file,migrator)
        print(json.dumps(proof))


def atomic_file(path,data):
    with tempfile.NamedTemporaryFile(dir=path.parent,delete=False) as f:
        staged=Path(f.name)
        try:
            f.write(data);f.flush();os.fsync(f.fileno())
            os.replace(staged,path)
            directory=os.open(path.parent,os.O_RDONLY)
            try: os.fsync(directory)
            finally: os.close(directory)
        finally:
            staged.unlink(missing_ok=True)


def restore_locked(args,admin,root,dump,command_file,migrator):
    folder=args.destination
    request={"root_id":root,"manifest_sha256":args.manifest_sha256,"dump_sha256":args.dump_sha256}
    intent_file=folder/"intent.json"
    if intent_file.exists():
        intent=json.loads(intent_file.read_bytes())
        check(all(intent.get(k)==v for k,v in request.items()),"Restore work directory belongs to another request")
    else:
        check(set(p.name for p in folder.iterdir())=={"restore.lock"},"Recovery directory is not empty")
        intent={**request,"database":"orange_recovered_"+uuid.uuid4().hex}
        atomic_file(intent_file,(json.dumps(intent)+"\n").encode())
    name=intent["database"]
    check(re.fullmatch(r"orange_recovered_[0-9a-f]{32}",name),"Invalid recorded restore identity")
    admin_env=connection(admin);env=connection(admin,name)
    marker="orange-private-recovery-"+args.dump_sha256+"-"+args.manifest_sha256
    def information():
        raw=sql(admin_env,f"SELECT json_build_object('oid',oid,'owner',pg_get_userbyid(datdba)=current_user,'open',datallowconn,'marker',shobj_description(oid,'pg_database')) FROM pg_database WHERE datname='{name}'").strip()
        return json.loads(raw) if raw else None
    info=information()
    receipt_file=folder/"receipt.json"
    if receipt_file.exists():
        completed=json.loads(receipt_file.read_bytes())
        check(info and not info["open"] and info["owner"] and info["oid"]==completed["database_oid"] and info["marker"]==marker and
              completed["database"]==name and all(completed.get(k)==v for k,v in request.items()),"Completed restore identity or quarantine changed")
        check(url_file(folder/"database-url")==admin._replace(path="/"+name),"Published restore locator differs from the completed candidate")
        return completed
    if info and info["owner"] and not info["open"] and info["marker"] is None and "database_oid" not in intent:
        # CREATE DATABASE cannot share a transaction with COMMENT. After an
        # interruption in that gap, preserve the unmarked database closed and
        # allocate a fresh identity instead of adopting an unverifiable object.
        intent.setdefault("unmarked_quarantines",[]).append({"database":name,"oid":info["oid"]})
        name="orange_recovered_"+uuid.uuid4().hex
        intent["database"]=name
        atomic_file(intent_file,(json.dumps(intent)+"\n").encode())
        env=connection(admin,name);info=information()
        check(info is None,"New restore database name collision")
    owned=False
    try:
        if info is None:
            sql(admin_env,f'CREATE DATABASE "{name}" WITH TEMPLATE template0 ALLOW_CONNECTIONS false;')
            owned=True
            sql(admin_env,f"""COMMENT ON DATABASE "{name}" IS '{marker}'; REVOKE CONNECT ON DATABASE "{name}" FROM PUBLIC;""")
            info=information()
        check(info and info["owner"] and info["marker"]==marker,"Recorded restore database has no matching ownership marker")
        if "database_oid" in intent:
            check(info["oid"]==intent["database_oid"],"Recorded database was replaced")
        else:
            intent["database_oid"]=info["oid"]
            atomic_file(intent_file,(json.dumps(intent)+"\n").encode())
        owned=True
        # A pg_restore child can outlive an interrupted controller. Fence new clients
        # and wait for the earlier transaction's real connection to go away before resuming.
        sql(admin_env,f'ALTER DATABASE "{name}" ALLOW_CONNECTIONS false;')
        deadline=time.monotonic()+120
        while sql(admin_env,f"SELECT count(*) FROM pg_stat_activity WHERE datname='{name}'").strip()!=b"0":
            if time.monotonic()>=deadline:raise ValueError("Earlier recovery transaction still owns its connection")
            time.sleep(.25)
        sql(admin_env,f'REVOKE CONNECT ON DATABASE "{name}" FROM PUBLIC; ALTER DATABASE "{name}" ALLOW_CONNECTIONS true;')
        loaded=sql(env,"SELECT to_regclass('public._sqlx_migrations') IS NOT NULL").strip()==b"t"
        if not loaded:
            run=subprocess.run([binary("pg_restore"),"--exit-on-error","--single-transaction","--no-owner","--dbname",name,str(dump)],
                               env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
            check(run.returncode==0,"Could not restore the trusted database dump")
        restored_url=urlunsplit(admin._replace(path="/"+name))
        run=subprocess.run([migrator,"--migrate-private-restore"],env={"PATH":os.environ.get("PATH",""),"DATABASE_URL":restored_url},capture_output=True)
        check(run.returncode==0 and b"Migrated private restore database offline" in run.stdout,"Offline migration refused")
        result=subprocess.run([binary("psql"),"-X","-w","-qAt","-v","ON_ERROR_STOP=1","-f",command_file],
                              env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        check(result.returncode==0,"Recovery redactions did not match restored object identities; transaction aborted")
        proof=json.loads(result.stdout)
        sql(admin_env,f'ALTER DATABASE "{name}" ALLOW_CONNECTIONS false;')
        proof.update(**request,database=name,database_oid=info["oid"],scope="private object deletion replay only",
                     traffic_enabled=False,connections_enabled=False)
        atomic_file(folder/"database-url",(restored_url+"\n").encode())
        atomic_file(receipt_file,(json.dumps(proof,indent=2)+"\n").encode())
        return proof
    except BaseException:
        if owned:
            sql(admin_env,f'ALTER DATABASE "{name}" ALLOW_CONNECTIONS false;')
            atomic_file(folder/"quarantined-database",(name+"\n").encode())
        raise


def main():
    p=argparse.ArgumentParser(description=__doc__)
    sub=p.add_subparsers(dest="action",required=True)
    for name in ("export","restore"):
        c=sub.add_parser(name);c.add_argument("--database-url-file",type=Path,required=True)
        c.add_argument("--root-id",required=True);c.add_argument("--destination",type=Path,required=True)
        if name=="export":
            c.add_argument("--signing-key",type=Path,required=True)
        else:
            c.add_argument("--journal",required=True,type=Path);c.add_argument("--verification-key",type=Path,required=True)
            c.add_argument("--dump",required=True,type=Path);c.add_argument("--dump-sha256",required=True)
            c.add_argument("--manifest-sha256",required=True);c.add_argument("--user-api",type=Path)
    args=p.parse_args()
    (export if args.action=="export" else restore)(args)


if __name__=="__main__":
    try:
        main()
    except (ValueError, OSError, RuntimeError, subprocess.SubprocessError) as exc:
        # Runtime errors never include SQL rows, process output, connection URLs or keys.
        sys.exit("Private artifact recovery failed: "+(str(exc) if type(exc) is ValueError and not isinstance(exc,json.JSONDecodeError) else type(exc).__name__))
