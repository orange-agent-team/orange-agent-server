#!/usr/bin/env python3.12
"""Exercise model registration and the dispatch namespace interlock on an isolated PostgreSQL database."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import copy
from contextlib import nullcontext
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import uuid
from urllib.parse import unquote, urlsplit

from owned_postgres import database
from platform_package_smoke import listening
from admin_contract_consistency import check_contract

ROOT=Path(__file__).resolve().parents[1]


def require(ok, message):
    if not ok:
        raise ValueError(message)


def native():
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],cwd=ROOT,text=True).strip())
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],CARGO_NET_OFFLINE="true")
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-control-plane","--bin","orange-admin-api"],cwd=ROOT,env=env,check=True)
    meta=json.loads(subprocess.check_output([cargo,"metadata","--no-deps","--offline","--locked","--format-version","1"],cwd=ROOT,env=env))
    return Path(meta["target_directory"])/"debug/orange-admin-api",cargo,env


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    cli.add_argument("--admin-repo",type=Path,help="Explicit Admin checkout for Chrome acceptance")
    cli.add_argument("--assets",type=Path,help="Previously verified static files, optionally extracted from a package")
    cli.add_argument("--installed",action="store_true",help="Use the extracted native release matching this checkout")
    args=cli.parse_args()
    if args.assets and not args.admin_repo:cli.error("--assets requires --admin-repo")
    config=args.environment.resolve()/"identity/admin-config.json"
    origin=json.loads(config.read_bytes())["admin_origin"]
    check_contract(json.loads((ROOT/"contracts/openapi/admin.v1.json").read_bytes()),json.loads((ROOT/"contracts/json-schema/admin.schema.json").read_bytes()))
    program,cargo,build_env=native()
    with database(args.environment.resolve()/"database-url","orange_model_prev_") as (_,previous):
        case="legacy_model_profiles_keep_their_identity_and_review_state_at_registry_upgrade"
        test=subprocess.run([cargo,"test","--locked","--offline","-p","orange-control-plane","--features","model-registry-upgrade-tests",
                             "--test","admin_actions",case,"--","--exact"],cwd=ROOT,env=dict(build_env,ORANGE_MODEL_DRAFT_UPGRADE_URL=previous),
                            stdout=subprocess.PIPE,text=True,check=True)
        require("test "+case+" ... ok" in test.stdout,"Legacy DB was not upgraded")
        print(test.stdout,end="",flush=True)
    template=json.loads((ROOT/"crates/model-runtime/model-manifests/qwen-local-test-v1.json").read_bytes())
    scope={"environment":"dev","kind":"platform","scope_id":"platform"}
    query="?environment=dev&scope_kind=platform&scope_id=platform"
    from verified_platform_archive import binaries
    if args.admin_repo:
        from acceptance_origins import reserve_origins
        reserve_origins(43112)
        admin=args.admin_repo.resolve(strict=True)
        subprocess.run([sys.executable,admin/"scripts/local-tls.py"],cwd=admin,check=True)
    with (binaries(None) if args.installed else nullcontext(program.parent)) as programs, database(args.environment.resolve()/"database-url","orange_model_draft_") as (_,url):
        program=programs/"orange-admin-api"
        parsed=urlsplit(url)
        pg=dict(os.environ,PGHOST=parsed.hostname,PGPORT=str(parsed.port or 5432),PGUSER=unquote(parsed.username),PGPASSWORD=unquote(parsed.password),PGDATABASE=parsed.path[1:])
        psql=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
        def sql(statement,*,must_pass=True):
            result=subprocess.run([psql,"-X","-w","-Atq","-v","ON_ERROR_STOP=1"],env=pg,input=statement,text=True,capture_output=True,timeout=20)
            if must_pass:
                require(result.returncode==0,"Test SQL failed: "+result.stderr[:250])
                return result.stdout.strip()
            return result
        def actor(role,kind="platform",sid="platform"):
            who=str(uuid.uuid4());token=secrets.token_hex(32);csrf=secrets.token_hex(32)
            hashed=hashlib.sha256(token.encode()).hexdigest()
            sql(f"""INSERT INTO admin_principals(principal_id,issuer,subject) VALUES('{who}','draft-fixture','{who}');
                INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at)
                VALUES('{who}','{role}','dev','{kind}','{sid}',extract(epoch FROM now())::bigint+3600);
                INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                SELECT '{hashed}','{who}','{csrf}',t,t+600,t+3600 FROM (SELECT extract(epoch FROM now())::bigint AS t) clock;""")
            return who,token,csrf
        env=dict(PATH=os.environ["PATH"],ORANGE_ENV="development",DATABASE_URL=url,ORANGE_ADMIN_BIND="127.0.0.1:0",ORANGE_ADMIN_CONFIG_FILE=str(config))
        with listening(program,env,"Admin API") as api:
            writer=actor("compute_operator");reader=actor("finance_operator");foreign=actor("compute_operator","region","local-test")
            def http(method,path,actor=None,body=None,key=None,overrides=None):
                headers={"content-type":"application/json","origin":origin}
                if actor:headers.update({"cookie":"__Host-orange-admin="+actor[1],"x-csrf-token":actor[2]})
                if key:headers["idempotency-key"]=key
                headers.update(overrides or {})
                client=HTTPConnection("127.0.0.1",urlsplit(api).port,timeout=10)
                try:
                    client.request(method,"/admin/v1/"+path,body if isinstance(body,bytes) else json.dumps(body) if body is not None else None,headers)
                    r=client.getresponse();return r.status,json.loads(r.read())
                finally:client.close()
            def post(doc,who=writer,key="original-key",overrides=None):
                return http("POST","model-drafts",who,{"scope":scope,"manifest":doc},key,overrides)
            def profile(name):
                return sql(f"INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES('{uuid.uuid4()}','{name}',repeat('a',64),repeat('b',64))",must_pass=False)
            counts="SELECT (SELECT count(*) FROM reviewed_execution_profiles),(SELECT count(*) FROM quotes),(SELECT count(*) FROM jobs)"
            require(http("GET","models"+query)[0]==401,"Anonymous directory opened")
            require(post(template,reader)[0]==403 and post(template,foreign)[0]==403,"Unauthorized draft registration")
            require(post(template,overrides={"origin":"https://wrong.invalid"})[0]==403 and post(template,overrides={"x-csrf-token":"invalid"})[0]==403,"Missing write protection")
            require(post(template,overrides={"cookie":"__Host-orange-user="+writer[1]})[0]==401,"User cookie replaced Admin identity")
            bad=copy.deepcopy(template);bad["runtime"]["command"]="launch"
            require(post(bad)[0]==422,"Accepted unknown runtime field")
            duplicate=json.dumps({"scope":scope,"manifest":template}).replace('"schema_version": 1','"schema_version": 0,"schema_version": 1').encode()
            require(http("POST","model-drafts",writer,duplicate,"duplicate")[0]==422,"Duplicate field accepted")
            require(sql("SELECT count(*) FROM admin_model_drafts")=="0","Rejected operation created a draft")
            before=sql(counts)
            with ThreadPoolExecutor(max_workers=2) as pool:
                a,b=list(pool.map(lambda _:post(template),range(2)))
            require(sorted([a[0],b[0]])==[200,201] and a[1]==b[1],"Concurrent idempotent POST did not resolve to one receipt")
            receipt=a[1];name=template["model_version"]
            digest=hashlib.sha256(json.dumps(template,sort_keys=True,separators=(",",":")).encode()).hexdigest()
            registration=hashlib.sha256(b"orange-request-v1\0admin-model-draft-registration.v1\0"+json.dumps([writer[0],"original-key",digest],separators=(",",":")).encode()).hexdigest()
            require(receipt=={"registered_by":writer[0],"registration_sha256":registration,"model":{"model_version_id":name,"modality":"text","status":"draft","version":digest},"manifest_sha256":digest,"manifest":template},"Unexpected canonical draft projection")
            require(sql("SELECT count(*) FROM admin_audit WHERE event_type='model.draft_registered'")=="1","Duplicate write audit")
            require(http("GET","model-drafts/"+name+query,reader)==(200,receipt),"Cannot reconcile registered manifest")
            require(http("GET","models/"+name+query,reader)==(200,receipt["model"]),"Summary mismatch")
            require(http("GET","model-drafts/"+name+query,foreign)[0]==403,"Region granted platform read")
            changed=copy.deepcopy(template);changed["source"]["license"]="changed"
            require(post(changed)[0]==409 and post(template,key="different")[0]==409,"Mutable draft or key binding")
            require(profile(name).returncode!=0 and sql(counts)==before,"Draft could act as a reviewed execution profile")
            require(sql("UPDATE admin_model_drafts SET manifest_sha256=repeat('a',64)",must_pass=False).returncode!=0,"Mutable historical manifest")
            require(sql("DELETE FROM admin_model_drafts",must_pass=False).returncode!=0,"Deleted draft")
            require(profile("already-reviewed").returncode==0,"Existing trusted provisioner was blocked")
            conflict=dict(template,model_version="already-reviewed")
            require(post(conflict,key="legacy-conflict")[0]==409,"Draft appropriated a reviewed identity")
            second=dict(template,model_version="z-second")
            require(post(second,key="second")[0]==201,"Second model rejected")
            page=http("GET","models"+query+"&limit=1",writer)[1]
            token=page["next_cursor"]
            require(len(page["data"])==1 and http("GET","models"+query+"&limit=1&cursor="+token,writer)[1]["data"][0]["model_version_id"]=="z-second","Missing next page")
            require(http("GET","models"+query+"&limit=1&cursor="+token,reader)[0]==422,"Cursor crossed principals")
            # The manifest can exceed the general Admin JSON limit while remaining inside its own bound.
            large=dict(template,model_version="large-fixture")
            large["artifacts"]=[{"source_path":"weights/"+("a"*86)+f"{i:03d}","cache_path":"assets/"+("b"*86)+f"{i:03d}",
                                 "bytes":1,"sha256":"a"*64} for i in range(250)]
            large["resources"]=dict(template["resources"],artifact_bytes=250)
            require(len(json.dumps(large).encode())>65536 and post(large,key="large")[0]==201,
                    "A valid large proposal did not traverse the normal endpoint")
            sql("""CREATE FUNCTION test_draft_audit_failure() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'test-owned audit failure'; END $$;
                 CREATE TRIGGER test_draft_audit_failure BEFORE INSERT ON admin_audit FOR EACH ROW
                 WHEN (NEW.entity_id='audit-failure') EXECUTE FUNCTION test_draft_audit_failure();""")
            failed=dict(template,model_version="audit-failure")
            require(post(failed,key="audit-abort")[0]==503,"A failed audit write was acknowledged")
            require(sql("SELECT count(*) FROM model_version_names WHERE model_version_id='audit-failure'")=="0",
                    "Rolled-back registration retained its name claim")
            sql("DROP TRIGGER test_draft_audit_failure ON admin_audit; DROP FUNCTION test_draft_audit_failure()")
            require(post(failed,key="audit-abort")[0]==201,"An atomic registration failure consumed the original key")
            # The two provisioners do not share an application lock. PostgreSQL must choose only one.
            for number in range(4):
                race="race-"+str(number)
                with ThreadPoolExecutor(max_workers=2) as pool:
                    old=pool.submit(profile,race);new=pool.submit(post,dict(template,model_version=race),writer,race)
                    permitted=old.result().returncode==0;status=new.result()[0]
                    require((permitted and status==409) or (not permitted and status==201),"Racing provisioners disagreed about the model namespace")
            require(sql("SELECT count(*) FROM admin_model_drafts d JOIN reviewed_execution_profiles p USING(model_version_id)")=="0","Draft visible to the legacy dispatch planner")
            if args.admin_repo:
                browser_doc=dict(template,model_version="browser-draft")
                fingerprint=hashlib.sha256(json.dumps(browser_doc,sort_keys=True,separators=(",",":")).encode()).hexdigest()
                web=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
                values={"api":api,"origin":origin,"writer":writer[1],"reader":reader[1],"assets":str((args.assets or admin/"apps/admin/dist").resolve(strict=True)),
                        "manifest":browser_doc,"sha":fingerprint}
                proc=subprocess.run([web,admin/"apps/admin/scripts/real-model-registry.mjs"],cwd=admin,input=json.dumps(values),text=True,stdout=subprocess.PIPE,timeout=100)
                print(proc.stdout,end="",flush=True);proc.check_returncode()
                require("PASS actual Admin browser registered a draft once" in proc.stdout,"Missing real browser")
                require(sql("SELECT count(*) FROM admin_audit WHERE event_type='model.draft_registered' AND entity_id='browser-draft'")=="1","Browser performed more than one mutation")
            sql(f"UPDATE admin_sessions SET auth_time=extract(epoch FROM now())::bigint-310 WHERE principal_id='{writer[0]}'")
            status,reason=post(dict(template,model_version="requires-mfa"),key="stale")
            require(status==403 and reason.get("code")=="STEP_UP_REQUIRED" and sql("SELECT count(*) FROM admin_model_drafts WHERE model_version_id='requires-mfa'")=="0","Stale authentication had a write effect")
            sql(f"UPDATE admin_grants SET revoked=true WHERE principal_id='{writer[0]}'")
            require(post(template)[0] in (401,403) and http("GET","model-drafts/"+name+query,writer)[0] in (401,403),"Revocation exposed an old idempotent receipt")
        print(json.dumps({"duplicate_posts":2,"original_write_audits":1,"racing_namespace_claims":4,"legacy_draft_overlaps":0,
                         "authorization_effects_from_drafts":0,"fixture_admin_sessions":True}),flush=True)
    print("PASS actual Admin draft registration, immutable readback, concurrent namespace fencing and revoked retries",flush=True)


if __name__=="__main__":main()
