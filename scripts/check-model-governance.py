#!/usr/bin/env python3.12
"""Qualify signed evidence, independent approval, live model admission and historical billing in a private local DB."""
import argparse
from contextlib import ExitStack
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
import tempfile
import time
from urllib.parse import unquote,urlsplit
import uuid

from model_certification_probe import measure,request_hash,sha
from owned_postgres import database
from acceptance_origins import reserve_origins
from platform_package_smoke import listening

ROOT=Path(__file__).resolve().parents[1]


def require(predicate,message):
    if not predicate:raise ValueError(message)


def new_id():return str(uuid.uuid4())


def build():
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],cwd=ROOT,text=True).strip())
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],CARGO_NET_OFFLINE="true")
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-control-plane","--bin","orange-user-api","--bin","orange-admin-api",
                    "-p","orange-model-runtime","--bin","orange-model-manifest"],cwd=ROOT,env=env,check=True)
    target=json.loads(subprocess.check_output([cargo,"metadata","--locked","--offline","--no-deps","--format-version","1"],env=env,cwd=ROOT))["target_directory"]
    return Path(target)/"debug"


def http(origin,method,path,headers=None,body=None):
    conn=HTTPConnection("127.0.0.1",urlsplit(origin).port,timeout=12)
    try:
        conn.request(method,path,None if body is None else json.dumps(body),
                     {"Content-Type":"application/json",**(headers or {})})
        response=conn.getresponse();raw=response.read(512*1024)
        return response.status,json.loads(raw) if raw else {}
    finally:conn.close()


def main():
    cli=argparse.ArgumentParser();cli.add_argument("--environment",type=Path,default=ROOT/".local")
    cli.add_argument("--runtime",type=Path,default=ROOT/".local/model-runtime")
    cli.add_argument("--installed",action="store_true",help="Start the user/Admin APIs and use the signer from the verified native archive")
    cli.add_argument("--admin-repo",type=Path)
    cli.add_argument("--assets",type=Path,help="Optional extracted Admin static assets")
    args=cli.parse_args()
    if args.assets and not args.admin_repo:cli.error("--assets requires --admin-repo")
    if args.admin_repo:
        admin=args.admin_repo.resolve(strict=True)
        reserve_origins(43112)
        subprocess.run([sys.executable,admin/"scripts/local-tls.py"],cwd=admin,check=True)
    envroot=args.environment.resolve();runtime=args.runtime.resolve()
    aconf=envroot/"identity/admin-config.json";uconf=envroot/"identity/user-config.json"
    admin_site=json.loads(aconf.read_bytes())["admin_origin"];user_site=json.loads(uconf.read_bytes())["user_origin"]
    programs=build();tool=programs/"orange-model-manifest"
    scope={"environment":"dev","kind":"platform","scope_id":"platform"};query="?environment=dev&scope_kind=platform&scope_id=platform"
    with tempfile.TemporaryDirectory() as tmp,database(envroot/"database-url","orange_model_review_") as (_,url),ExitStack() as running:
        signing_tool=ROOT/"scripts/sign-model-certification.py"
        if args.installed:
            from verified_platform_archive import binaries
            programs=running.enter_context(binaries(None))
            signing_tool=programs.parent/"scripts/sign-model-certification.py"
            require(signing_tool.is_file() and (programs.parent/"docs/model-registry-drafts.md").is_file(),"Installed governance tools missing")
        work=Path(tmp);objects=work/"objects";objects.mkdir(mode=0o700)
        parsed=urlsplit(url);pg=dict(os.environ,PGHOST=parsed.hostname,PGPORT=str(parsed.port or 5432),PGUSER=unquote(parsed.username),PGPASSWORD=unquote(parsed.password),PGDATABASE=parsed.path[1:])
        psql=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
        def sql(statement,ok=True):
            result=subprocess.run([psql,"-X","-w","-Atq","-v","ON_ERROR_STOP=1"],input=statement,text=True,env=pg,capture_output=True,timeout=20)
            if ok and result.returncode:raise ValueError(result.stderr[:380])
            return result.stdout.strip() if ok else result
        ssl=Path("/opt/homebrew/opt/openssl@3/bin/openssl")
        if not ssl.is_file():ssl=shutil.which("openssl")
        key=work/"review.key";subprocess.run([ssl,"genpkey","-algorithm","Ed25519","-out",key],check=True,capture_output=True);key.chmod(0o600)
        public=subprocess.check_output([ssl,"pkey","-in",key,"-pubout","-outform","DER"])
        require(len(public)==44 and public.startswith(bytes.fromhex("302a300506032b6570032100")),"Wrong signing key type")
        pub=work/"review.pub";pub.write_text(public[12:].hex()+"\n");pub.chmod(0o600)
        common=dict(PATH=os.environ["PATH"],ORANGE_ENV="development",DATABASE_URL=url)
        api=running.enter_context(listening(programs/"orange-admin-api",dict(common,ORANGE_ADMIN_BIND="127.0.0.1:0",ORANGE_ADMIN_CONFIG_FILE=str(aconf),ORANGE_MODEL_REVIEW_PUBLIC_KEY_FILE=str(pub)),"Admin API"))
        userapi=running.enter_context(listening(programs/"orange-user-api",dict(common,ORANGE_USER_BIND="127.0.0.1:0",ORANGE_USER_CONFIG_FILE=str(uconf),ORANGE_ARTIFACT_ROOT=str(objects)),"User API"))
        def actor(role="compute_operator"):
            who=new_id();token=secrets.token_hex(32);csrf=secrets.token_hex(32)
            sql(f"""INSERT INTO admin_principals(principal_id,issuer,subject) VALUES('{who}','certification-fixture','{who}');
                INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES('{who}','{role}','dev','platform','platform',extract(epoch FROM now())::bigint+3600);
                INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                SELECT '{sha(token.encode())}','{who}','{csrf}',t,t+600,t+3600 FROM (SELECT extract(epoch FROM now())::bigint t) clock;""")
            return who,{"Origin":admin_site,"Cookie":"__Host-orange-admin="+token,"X-CSRF-Token":csrf}
        maker,mh=actor();checker,ch=actor()
        def post(path,headers,data,key=None,version=None):
            h={**headers,"Idempotency-Key":key or new_id()}
            if version:h["If-Match"]='"'+version+'"'
            return http(api,"POST","/admin/v1/"+path,h,data)
        def get(path,h=mh):return http(api,"GET","/admin/v1/"+path,h)
        def expect_code(result,status):
            code,body=result
            require(code==status,f"Expected HTTP {status}, got {code}: {str(body)[:180]}")
            return body
        def create_action(kind,version,payload):
            return post("actions",mh,{"scope":scope,"action_type":kind,"target_version":version,
                        "ticket_id":"development-model-test","reason":"Verify the fixed local certification and current platform authority","payload":payload})
        def preview(action):return {"policy_version":action["policy_version"],"payload_digest":action["payload_digest"]}
        def execute(action,who=mh,key=None):
            return post("actions/"+action["action_id"]+"/execute",who,preview(action),key,action["version"])
        doc=json.loads(subprocess.check_output([tool,"show","qwen-local-test-v1"]))["manifest"]
        model="validated-"+new_id();doc["model_version"]=model
        price="price-"+new_id();doc["pricing"]={"policy":"price_version","price_version":price}
        original=expect_code(post("model-drafts",mh,{"scope":scope,"manifest":doc}),201)
        fingerprint=original["manifest_sha256"];endpoint="models/"+model+query
        measurement,hardware,execution=measure(ROOT,runtime,doc,fingerprint,tool)
        # This development signer attests a fixture license review; the primary source link is retained for a real reviewer.
        notice="Development review fixture. The pinned publisher release declares Apache-2.0. Source: https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/blob/9217f5db79a29953eb74d5343926648285ec7e67/LICENSE\n"
        notice_file=work/"notice.txt";notice_file.write_text(notice)
        now=int(time.time());profile="profile-"+new_id();weight=doc["artifacts"][0]["sha256"]
        report={"version":1,"purpose":"model-qualification.v1","environment":"dev","model_version_id":model,"manifest_sha256":fingerprint,
                "model_sha256":weight,"execution_profile_id":profile,"execution_profile_digest":execution,"issued_unix":str(now-5),"expires_unix":str(now+900),
                "license":{"identifier":doc["source"]["license"],"source_revision":doc["source"]["revision"],
                           "permitted_use":"development-inference","notice_sha256":sha(notice.encode())},
                "measurement":measurement,"hardware":hardware}
        def envelope(value,signer=key):
            file=work/"report.json";file.write_text(json.dumps(value))
            return json.loads(subprocess.check_output([sys.executable,signing_tool,"--report",file,
                          "--license-notice",notice_file,"--signing-key",signer],cwd=ROOT,timeout=20))
        bundle=envelope(report);rid=request_hash("model-qualification-report.v1",report)
        upload="model-drafts/"+model+"/certifications"
        def attach(s,key=None):return post(upload,mh,{"scope":scope,"certification":s},key)
        def provision(digest=execution):return sql(f"INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES('{profile}','{model}','{weight}','{digest}')",False)
        require(provision().returncode!=0,"Draft accepted an execution profile")
        payload={"model_version_id":model,"certification_report_id":rid}
        expect_code(create_action("model.activate",fingerprint,payload),404)
        modified=copy.deepcopy(bundle);modified["signature"]="A"*86;expect_code(attach(modified),403)
        modified=copy.deepcopy(bundle);modified["license_notice"]+="modified";expect_code(attach(modified),422)
        for path,value in [(("manifest_sha256",),"a"*64),(("model_sha256",),"b"*64),(("measurement","observed_result_sha256"),"e"*64),
                           (("hardware","device"),"other"),(("expires_unix",),str(now-1))]:
            changed=copy.deepcopy(report);leaf=changed
            for segment in path[:-1]:leaf=leaf[segment]
            leaf[path[-1]]=value
            expect_code(attach(envelope(changed)),422)
        alien=work/"other.key";subprocess.run([ssl,"genpkey","-algorithm","Ed25519","-out",alien],capture_output=True,check=True);alien.chmod(0o600)
        expect_code(attach(envelope(report,alien)),403)
        require(sql("SELECT count(*) FROM model_certifications")=="0","Invalid evidence persisted")
        ckey=new_id();proof=expect_code(attach(bundle,ckey),201)
        require(proof["report_id"]==rid and expect_code(attach(bundle,ckey),200)==proof,"Evidence registration changed on replay")
        evidence=expect_code(get(upload+"/"+rid+query,ch),200)
        require(evidence["report"]==report and evidence["license_notice"]==notice,"Checker did not read the signed facts")
        created=expect_code(create_action("model.activate",fingerprint,payload),201)
        require(created["state"]=="pending_approval" and provision().returncode!=0,"Proposal activated model")
        decide="actions/"+created["action_id"]+"/approvals"
        approval=dict(preview(created),decision="approve",reason="Independent development review of signed evidence")
        expect_code(post(decide,mh,approval,version=created["version"]),403)
        expect_code(execute(created),409)
        approved=expect_code(post(decide,ch,approval,version=created["version"]),200)
        require(approved["state"]=="ready" and provision().returncode!=0,"Approval alone enabled execution")
        sql(f"UPDATE admin_principals SET active=false WHERE principal_id='{checker}'")
        expect_code(execute(approved),403)
        require(sql("SELECT count(*) FROM model_registry_activations")=="0","A withdrawn checker still authorized model activation")
        sql(f"UPDATE admin_principals SET active=true WHERE principal_id='{checker}'")
        exkey=new_id();done=expect_code(execute(approved,key=exkey),202)
        require(done["state"]=="succeeded" and expect_code(execute(approved,key=exkey),202)==done,"Activation was not idempotent")
        current=expect_code(get(endpoint),200)
        require(current["status"]=="active" and current["certification_report_id"]==rid and sql("SELECT count(*) FROM model_registry_activations")=="1","Activation not bound to report")
        require(provision("a"*64).returncode!=0 and provision().returncode==0,"Wrong certification binding at provider admission")
        buyer=new_id();budget=new_id();node=new_id();user_token=secrets.token_hex(32);csrf=secrets.token_hex(32)
        sql(f"""INSERT INTO owners(owner_id) VALUES('{buyer}'); INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{buyer}','model-test','{buyer}','{buyer}');
            INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
            SELECT '{sha(user_token.encode())}','{new_id()}','{buyer}','{csrf}',t,t+600,t+3600 FROM (SELECT extract(epoch FROM now())::bigint t) t;
            INSERT INTO wallets(owner_id,gift) VALUES('{buyer}',1000); INSERT INTO budget_roots(root_budget_id,owner_id,scope_id,limit_amount) VALUES('{budget}','{buyer}','model-test',1000);
            INSERT INTO nodes(node_id,owner_id,environment,region_id,state) VALUES('{node}','{buyer}','dev','local-test','online');""")
        # Use the existing pricing draft entry point instead of manufacturing a circular model/price reference.
        _,finance=actor("finance_operator")
        execution_rules={"execution_digest":execution,"input_rule":"llama-chat-template-input-v1",
                         "output_rule":"orange-visible-components-v1","tool_schema_rule":"jsonschema-0.56-draft202012-offline-bounded-v1"}
        meter=request_hash("orange.trusted-meter.v1",execution_rules)
        exe_sha=json.loads((runtime/"build.json").read_bytes())["executable_sha256"]
        descriptor={**execution_rules,"profile_digest":meter,"model_version":model,"model_sha256":weight,"executable_sha256":exe_sha,"context_tokens":4096}
        sql(f"""INSERT INTO reviewed_cpu_profiles(execution_profile_id,executable_sha256,model_bytes,context_tokens,startup_timeout_ms)
                  VALUES('{profile}','{exe_sha}',{doc["resources"]["artifact_bytes"]},4096,120000);
            INSERT INTO trusted_meter_profiles(profile_digest,descriptor) VALUES('{meter}','{json.dumps(descriptor)}');""")
        pricing={"scope":scope,"node_id":node,"execution_profile_id":profile,"meter_profile_digest":meter,
                 "input_price":"5","output_price":"10","output_limit":64,"fee_bps":100}
        draft_price=expect_code(post("price-drafts",finance,pricing),201)
        require(draft_price["price_version_id"]==price,"Pricing entry point ignored the immutable model price reference")
        def quote(price_version,should_pass=True):
            quote_id=new_id();invocation=new_id();projection="b"*64
            inserted=sql(f"""BEGIN; INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at)
                VALUES('{quote_id}','{buyer}','{budget}','{invocation}','{projection}','{price_version}',41,clock_timestamp()+interval '10 minutes');
                INSERT INTO quote_execution_plans(quote_id,target_node_id,execution_profile_id,request_projection_digest,job_deadline)
                VALUES('{quote_id}','{node}','{profile}','{projection}',clock_timestamp()+interval '30 minutes'); COMMIT;""",False)
            require((inserted.returncode==0)==should_pass,"Unexpected quote admission: "+inserted.stderr[:200])
            return {"quote_id":quote_id,"root_budget_id":budget,"invocation_id":invocation,"purpose":"primary","request_projection_digest":projection}
        quote("wrong-price",False);live=quote(price);outstanding=quote(price)
        require(int(sql("SELECT extract(epoch FROM job_deadline)::bigint FROM quote_execution_plans WHERE quote_id='"+live["quote_id"]+"'"))<=int(report["expires_unix"]),"Job deadline exceeds signed evidence")

        def submit(value,k):return http(userapi,"POST","/api/v1/owners/"+buyer+"/jobs",{"Cookie":"__Host-orange-user="+user_token,"Origin":user_site,
                             "X-CSRF-Token":csrf,"Idempotency-Key":k},value)
        jobkey=new_id();status,job=submit(live,jobkey);require(status in (200,201),"Real user Job rejected: "+str((status,job))[:180])
        def financial():return sql(f"SELECT gift,reserved_gift,(SELECT reserved FROM budget_roots WHERE root_budget_id='{budget}'),(SELECT count(*) FROM jobs WHERE owner_id='{buyer}'),(SELECT count(*) FROM ledger_transactions) FROM wallets WHERE owner_id='{buyer}'")
        before=financial();require(before.startswith("959|41|41|1|"),"Unexpected development hold")
        retire=expect_code(create_action("model.disable",current["version"],{"model_version_id":model}),201)
        require(retire["state"]=="ready","Wrong withdrawal policy")
        old_reader=running.enter_context(subprocess.Popen([psql,"-X","-w","-Atq","-v","ON_ERROR_STOP=1","-v","VERBOSITY=verbose"],
                         env=pg,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True))
        running.callback(lambda:old_reader.kill() if old_reader.poll() is None else None)
        old_reader.stdin.write(f"BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT registry_profile_current('{profile}');\n")
        old_reader.stdin.flush()
        require(old_reader.stdout.readline().strip()=="t","Old transaction did not read the active profile")
        expect_code(execute(retire),202)
        stdout,stderr=old_reader.communicate(f"SELECT require_registry_price('{profile}','{price}'); COMMIT;\n",timeout=10)
        require(old_reader.returncode!=0 and "40001" in stderr,"Old snapshot admitted a withdrawn model: "+stdout[:100])
        require(expect_code(get(endpoint),200)["status"]=="disabled","Withdrawal not visible")
        require(sql(f"SELECT enabled FROM reviewed_execution_profiles WHERE execution_profile_id='{profile}'")=="f","Old provider profile still enabled")
        require(sql(f"UPDATE reviewed_execution_profiles SET enabled=true WHERE execution_profile_id='{profile}'",False).returncode!=0,"Withdrawn profile reenabled")
        denied,_=submit(outstanding,new_id());require(denied==409 and before==financial(),"New Job changed balances after withdrawal")
        again,res=submit(live,jobkey);require(again in (200,201) and res['job_id']==job['job_id'] and financial()==before,"Old Job or original hold lost")
        quote(price,False)
        expect_code(create_action("model.activate",fingerprint,payload),409)
        def additional_model():
            catalog=dict(doc,model_version="local-only-"+new_id(),pricing={"policy":"local_unpriced"})
            registration=expect_code(post("model-drafts",mh,{"scope":scope,"manifest":catalog}),201)
            digest=registration["manifest_sha256"]
            probe,target,exact=measure(ROOT,runtime,catalog,digest,tool)
            return dict(report,model_version_id=catalog["model_version"],manifest_sha256=digest,execution_profile_id="profile-"+new_id(),
                        execution_profile_digest=exact,measurement=probe,hardware=target)
        # A real clock cutoff must stop work even without an explicit disable row or background sweep.
        expiring=additional_model();expiry=int(time.time())+12
        expiring["expires_unix"]=str(expiry)
        em=expiring["model_version_id"];ep=expiring["execution_profile_id"];ed=expiring["execution_profile_digest"]
        cert_path="model-drafts/"+em+"/certifications"
        exp_id=expect_code(post(cert_path,mh,{"scope":scope,"certification":envelope(expiring)}),201)["report_id"]
        # With two reports, pagination must bind the model, principal and scope.
        alternate=dict(expiring,expires_unix=str(expiry+60))
        expect_code(post(cert_path,mh,{"scope":scope,"certification":envelope(alternate)}),201)
        first=expect_code(get(cert_path+query+"&limit=1"),200);cursor=first["next_cursor"]
        second=expect_code(get(cert_path+query+"&limit=1&cursor="+cursor),200)
        require(len(first["data"])==len(second["data"])==1 and first["data"][0]["report_id"]<second["data"][0]["report_id"] and "next_cursor" not in second,"Evidence pagination lost a report")
        expect_code(get(cert_path+query+"&limit=1&cursor="+cursor,ch),422)
        ar=expect_code(create_action("model.activate",expiring["manifest_sha256"],{"model_version_id":em,"certification_report_id":exp_id}),201)
        accepted=expect_code(post("actions/"+ar["action_id"]+"/approvals",ch,dict(preview(ar),decision="approve",reason="Approve expiring local CPU evidence"),version=ar["version"]),200)
        expect_code(execute(accepted),202)
        sql(f"INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES('{ep}','{em}','{weight}','{ed}')")
        require(sql(f"SELECT registry_profile_current('{ep}')")=="t","Newly activated local-only certification unavailable")
        no_bill=sql(f"""BEGIN; INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at)
            VALUES('unpriced-quote','{buyer}','{budget}','{new_id()}','{'b'*64}','{price}',41,now()+interval '10 minutes');
            INSERT INTO quote_execution_plans(quote_id,target_node_id,execution_profile_id,request_projection_digest,job_deadline)
            VALUES('unpriced-quote','{node}','{ep}','{'b'*64}',now()+interval '10 minutes'); COMMIT;""",False)
        require(no_bill.returncode!=0 and "model" in no_bill.stderr.lower() and before==financial(),"Local-only model received a price")
        until=time.monotonic()+18
        while get("models/"+em+query)[1].get("status")!="disabled":
            if time.monotonic()>=until:raise ValueError("Certificate did not expire")
            time.sleep(.15)
        require(sql(f"SELECT registry_profile_current('{ep}')")=="f" and before==financial(),"Expired proof still permits work")
        if args.admin_repo:
            browser_report=additional_model()
            program=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
            def cookie(headers):return headers["Cookie"].split("=",1)[1]
            fixture={"api":api,"origin":admin_site,"assets":str((args.assets or admin/"apps/admin/dist").resolve(strict=True)),
                     "maker":cookie(mh),"checker":cookie(ch),"model":browser_report["model_version_id"],"envelope":envelope(browser_report)}
            browser=subprocess.run([program,admin/"apps/admin/scripts/real-model-governance.mjs"],cwd=admin,input=json.dumps(fixture),
                                   stdout=subprocess.PIPE,text=True,timeout=150)
            print(browser.stdout,end="",flush=True);browser.check_returncode()
            require("PASS actual Admin Chrome" in browser.stdout,"No browser evidence")
            bm=browser_report["model_version_id"]
            require(sql(f"SELECT (SELECT count(*) FROM model_certifications WHERE model_version_id='{bm}'),(SELECT count(*) FROM model_registry_activations WHERE model_version_id='{bm}' AND maker_id<>checker_id),(SELECT count(*) FROM model_registry_disables WHERE model_version_id='{bm}')")=="1|1|1","Repeated or missing browser effects")
        print(json.dumps({"cpu_completion_checked":True,"weight_sha256":weight,"report_id":rid,"unauthorized_evidence_persisted":0,
                         "original_model_activations":1,"original_model_disables":1,"expired_local_profile_denied":True,"unpriced_quote_denied":True,"jobs_before_and_after_disable":1,"development_hold_preserved":41,
                         "physical_cpu_target":hardware,"license_legal_review":"development fixture; no production determination"}),flush=True)
    print("PASS signed model evidence, independent activation, profile binding, live Job admission and withdrawal without duplicate holds",flush=True)

if __name__=="__main__":main()
