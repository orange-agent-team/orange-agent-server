#!/usr/bin/env python3.12
"""Check all four locked descriptors through the Rust validator, local assets and authenticated User API."""
import argparse
import copy
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
from urllib.parse import unquote,urlsplit
import uuid

from owned_postgres import database
from platform_package_smoke import listening

ROOT=Path(__file__).resolve().parents[1]
CACHES={"text":"model-runtime","embedding":"embedding-model","image":"image-model","transcription":"transcription-model"}


def require(ok,message):
    if not ok:raise ValueError(message)


def build():
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],cwd=ROOT,text=True).strip())
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],CARGO_NET_OFFLINE="true")
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-model-runtime","--bin","orange-model-manifest","-p","orange-control-plane","--bin","orange-user-api"],cwd=ROOT,env=env,check=True)
    subprocess.run([cargo,"test","--locked","--offline","-p","orange-model-runtime","--lib","manifest::tests"],cwd=ROOT,env=env,check=True)
    target=json.loads(subprocess.check_output([cargo,"metadata","--no-deps","--locked","--offline","--format-version","1"],cwd=ROOT,env=env))["target_directory"]
    return Path(target)/"debug"


def mutants(doc):
    def alter(path,value):
        result=copy.deepcopy(doc);node=result
        for key in path[:-1]:node=node[key]
        node[path[-1]]=value;return result
    yield alter(["source","revision"],"latest")
    yield alter(["source","repository"],"https://example.invalid/unreviewed")
    yield alter(["source","license"],"")
    yield alter(["source","asset_lock_sha256"],"0"*64)
    yield alter(["runtime","command"],["sh","-c","version"])
    yield alter(["runtime","source_revision"],"latest")
    yield alter(["runtime"],{"engine":"vllm","image_digest":"image:latest"})
    yield alter(["artifacts",0,"sha256"],"0"*64)
    yield alter(["artifacts",0,"cache_path"],"../other/weights")
    yield alter(["artifacts",0,"cache_path"],"NUL.gguf")
    yield alter(["artifacts",0,"cache_path"],"model.")
    yield alter(["resources","artifact_bytes"],1)
    yield alter(["resources","secret"],"invalid")
    yield alter(["pricing"],{"policy":"price_version","price_version":"latest"})
    yield alter(["interface","context_tokens"],-1)
    yield alter(["interface","output"],"shell")
    yield alter(["model_version"],"unsafe\n")
    for path in ("weights","weights/a","WEIGHTS/A"):
        value=copy.deepcopy(doc)
        value["artifacts"][0]["cache_path"]="weights/a"
        other=dict(value["artifacts"][0],source_path="z.bin",cache_path=path)
        value["artifacts"].append(other);value["resources"]["artifact_bytes"]*=2
        yield value


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--environment",type=Path,default=ROOT/".local",help="Existing local DB and development identity settings")
    cli.add_argument("--asset-root",type=Path,default=ROOT/".local",help="Existing version-locked local model caches")
    args=cli.parse_args()
    subprocess.run([sys.executable,ROOT/"scripts/generate-model-manifests.py"],cwd=ROOT,check=True)
    programs=build()
    tool=programs/"orange-model-manifest"
    def output(*argv,data=None):
        return json.loads(subprocess.check_output([tool,*argv],input=data,timeout=360))
    catalogue=output("list");require(len(catalogue)==4 and len(dict(catalogue))==4,"Invalid bundled catalogue")
    receipts={};measured={}
    for model,sha in catalogue:
        r=output("show",model);doc=r["manifest"];kind=doc["interface"]["modality"]
        # The four bundled fixtures contain printable ASCII and integers only, for which sorted compact JSON is canonical.
        canonical=json.dumps(doc,ensure_ascii=False,sort_keys=True,separators=(",",":")).encode()
        require(hashlib.sha256(canonical).hexdigest()==sha==r["manifest_sha256"],"Independent canonical digest differs")
        require(output("validate",data=canonical)==r,"CLI validation changed the descriptor")
        seen=output("verify",model,args.asset_root.resolve()/CACHES[kind])
        require(seen=={"model_version":model,"manifest_sha256":sha,"verified_bytes":doc["resources"]["artifact_bytes"],"verified_files":len(doc["artifacts"])},"Incorrect asset measurements")
        changed=copy.deepcopy(doc)
        original=changed["artifacts"][0]["sha256"]
        changed["artifacts"][0]["sha256"]=("a" if original[0]!="a" else "b")+original[1:]
        mismatch=subprocess.run([tool,"verify-proposed",str(args.asset_root.resolve()/CACHES[kind])],input=json.dumps(changed).encode(),
                                capture_output=True,timeout=360)
        require(mismatch.returncode!=0 and not mismatch.stdout and b"Integrity" in mismatch.stderr,"Tampered manifest measured successfully: "+kind)
        receipts[model]=r;measured[kind]=seen
    require(set(measured)==set(CACHES),"Missing model modality")
    base=receipts["qwen-local-test-v1"]["manifest"]
    invalid=[json.dumps(v).encode() for v in mutants(base)]
    invalid.append(json.dumps(base).encode().replace(b'"schema_version": 1',b'"schema_version": 4,"schema_version": 1',1))
    invalid.append(b' '*((256<<10)+1))
    for data in invalid:
        status=subprocess.run([tool,"validate"],input=data,capture_output=True,timeout=10)
        require(status.returncode!=0 and not status.stdout,"Invalid descriptor published a receipt")
    external=copy.deepcopy(base);external["runtime"]={"engine":"vllm","image_digest":"sha256:"+"a"*64}
    external["model_version"]="proposed-gpu-fixture";external["resources"]["device"]="nvidia_gpu"
    external["pricing"]={"policy":"price_version","price_version":"fixed-fixture-v1"}
    permitted=output("validate",data=json.dumps(external).encode())
    require(permitted["manifest"]==external,"Immutable OCI descriptor rejected")
    private=args.environment.resolve()
    with database(private/"database-url","orange_model_spec_") as (_,url):
        parsed=urlsplit(url)
        pg=dict(os.environ,PGHOST=parsed.hostname,PGPORT=str(parsed.port or 5432),PGUSER=unquote(parsed.username),PGPASSWORD=unquote(parsed.password),PGDATABASE=parsed.path[1:])
        psql=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
        def sql(statement):return subprocess.check_output([psql,"-X","-w","-Atq","-v","ON_ERROR_STOP=1"],input=statement,text=True,env=pg).strip()
        env=dict(PATH=os.environ["PATH"],DATABASE_URL=url,ORANGE_ENV="development",ORANGE_USER_CONFIG_FILE=str(private/"identity/user-config.json"),ORANGE_USER_BIND="127.0.0.1:0")
        with listening(programs/"orange-user-api",env,"User API") as origin:
            user=str(uuid.uuid4());sid=str(uuid.uuid4());token=secrets.token_hex(32);hashed=hashlib.sha256(token.encode()).hexdigest()
            sql(f"""INSERT INTO owners(owner_id) VALUES('{user}');
                 INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{user}','model-manifest-test','{user}','{user}');
                 INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                 SELECT '{hashed}','{sid}','{user}','{secrets.token_hex(32)}',t,t+600,t+1200 FROM (SELECT extract(epoch FROM now())::bigint AS t) clock;""")
            def http(path,status,auth=True,method="GET"):
                conn=HTTPConnection("127.0.0.1",urlsplit(origin).port,timeout=8)
                try:
                    conn.request(method,"/api/v1/model-manifests"+path,headers={"Cookie":"__Host-orange-user="+token} if auth else {})
                    reply=conn.getresponse();raw=reply.read(512*1024)
                    require(reply.status==status,f"Expected {status} for {method} {path}, got {reply.status}")
                    require(reply.getheader("Cache-Control")=="no-store","Reply differs from the shared User API no-store policy")
                    return json.loads(raw) if status==200 else None
                finally:conn.close()
            counts="SELECT (SELECT count(*) FROM reviewed_execution_profiles),(SELECT count(*) FROM quotes),(SELECT count(*) FROM jobs)"
            before=sql(counts)
            http("",401,False)
            require(http("",200)=={"items":[{"model_version":k,"manifest_sha256":v} for k,v in catalogue]},"HTTP list differs from CLI")
            for model,r in receipts.items():require(http("/"+model,200)==r,"HTTP descriptor differs from CLI")
            http("/"+external["model_version"],404);http("?command=run",422);http("/qwen-local-test-v1?url=other",422);http("",405,method="POST")
            sql(f"UPDATE user_sessions SET revoked=true WHERE session_id='{sid}'")
            http("/qwen-local-test-v1",401)
            require(sql(counts)==before,"Manifest inspection changed admission, quote or Job state")
    print(json.dumps({"measured":measured,"invalid_descriptors_refused":len(invalid),"tampered_modality_digests_refused":4,"authenticated_http_records":4,
                      "session_revocation_enforced":True,"market_state_mutations":0}),flush=True)
    print("PASS original ML-001.01 real pinned model assets, immutable reference contract and authenticated catalogue",flush=True)


if __name__=="__main__":main()
