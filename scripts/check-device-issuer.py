#!/usr/bin/env python3
"""Sign a real owner-approved development CSR, redeem it once and revoke the live mTLS session."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager, nullcontext
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import re
import secrets
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from urllib.parse import unquote,urlsplit
import uuid

from owned_postgres import database
from platform_package_smoke import listening
from verified_platform_archive import binaries

ROOT=Path(__file__).resolve().parents[1]


def require(ok,msg):
    if not ok:raise ValueError(msg)


def http(origin,method,path,headers,body=None):
    c=HTTPConnection("127.0.0.1",urlsplit(origin).port,timeout=10)
    try:
        c.request(method,path,None if body is None else json.dumps(body),dict(headers,**{"Content-Type":"application/json"}))
        r=c.getresponse();return r.status,json.loads(r.read(65536))
    finally:c.close()


@contextmanager
def gateway(binary,env):
    with subprocess.Popen([binary],env=env,stderr=subprocess.PIPE,stdout=subprocess.DEVNULL,text=True) as child:
        try:
            with selectors.DefaultSelector() as poll:
                poll.register(child.stderr,selectors.EVENT_READ)
                deadline=time.monotonic()+30
                while time.monotonic()<deadline:
                    if child.poll() is not None:raise RuntimeError("Gateway exited")
                    if poll.select(.3):
                        line=child.stderr.readline()
                        m=re.search(r"Development standalone Worker mTLS control on (127\.0\.0\.1:[0-9]+);",line)
                        if m:break
                else:raise RuntimeError("Gateway readiness timeout")
            yield "https://"+m[1]
        finally:
            if child.poll() is None:
                child.send_signal(signal.SIGINT)
                try:child.wait(timeout=15)
                except subprocess.TimeoutExpired:child.kill();child.wait();raise


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    cli.add_argument("--installed",action="store_true")
    cli.add_argument("--device-client",action="store_true",help="Run the real Worker CLI through ambiguous and corrupted redemption responses")
    cli.add_argument("--rotate-device",action="store_true",help="Activate competing replacements and test bounded overlap through the real mTLS gateway")
    cli.add_argument("--rotation-handoff",action="store_true",help="Verify current-key ownership over a fresh key and owner-bound handoff")
    cli.add_argument("--withdraw-issuer",action="store_true")
    args=cli.parse_args();private=args.environment.resolve()
    if args.rotate_device:args.rotation_handoff=True
    if args.rotation_handoff:args.device_client=True
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","cargo"],cwd=ROOT,text=True).strip())
    env={k:v for k,v in os.environ.items() if not k.startswith(("ORANGE_","STRIPE_","DATABASE_"))}
    env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true")
    # Compile the client before issuing short-lived credentials or creating a live browser session.
    client_test=[cargo,"test","--locked","--offline","-p","orange-control-plane","--features","worker-enrollment-tests","--test","worker_sessions"]
    build=subprocess.check_output([*client_test,"--no-run","--message-format=json-render-diagnostics"],cwd=ROOT,env=env,text=True)
    clients=[item["executable"] for item in map(json.loads,build.splitlines()) if item.get("executable")
             and item["target"]["name"]=="worker_sessions" and item["target"]["kind"]==["test"]]
    require(len(clients)==1,"Expected one compiled Worker session test harness")
    client_test=[clients[0]]
    if args.installed:context=binaries(None)
    else:
        subprocess.run([cargo,"build","--offline","--locked","-p","orange-worker","-p","orange-control-plane","--bin","orange-worker",
                        "--bin","orange-user-api","--bin","orange-worker-gateway",*(["--bin","orange-admin-api"] if args.withdraw_issuer else [])],cwd=ROOT,env=env,check=True)
        meta=json.loads(subprocess.check_output([cargo,"metadata","--no-deps","--format-version","1","--locked"],cwd=ROOT,env=env))
        context=nullcontext(Path(meta["target_directory"])/"debug")
    openssl="/opt/homebrew/opt/openssl@3/bin/openssl"
    if not Path(openssl).is_file():openssl=shutil.which("openssl")
    with context as programs, tempfile.TemporaryDirectory(prefix="orange-real-issuer-") as temp, database(private/"database-url","orange_issuer_") as (_,url):
        root=Path(temp);ca=root/"signer";device=root/"device";gw=root/"gateway"
        for folder in (ca,device,gw):folder.mkdir(mode=0o700)
        def crypto(*options):return subprocess.check_output([openssl,*options],cwd=root,stderr=subprocess.PIPE)
        def config(path,data):
            path.write_text(json.dumps(data));path.chmod(0o600);return path
        crypto("req","-x509","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-batch","-days","2",
               "-subj","/CN=Orange disposable device authority","-keyout",ca/"ca.key","-out",ca/"ca.pem",
               "-addext","basicConstraints=critical,CA:TRUE","-addext","keyUsage=critical,keyCertSign,cRLSign")
        ca_der=crypto("x509","-in",ca/"ca.pem","-outform","DER")
        issuer_hash=hashlib.sha256(ca_der).hexdigest();(ca/"ca.pem").chmod(0o600)
        cfg={"version":1,"environment":"development","region_id":"local-test","ca_key_file":str(ca/"ca.key"),
             "ca_certificate_file":str(ca/"ca.pem"),"ca_certificate_sha256":issuer_hash}
        signer=config(ca/"signer.json",cfg)
        # The gateway owns a separate server key. Only the public issuer certificate is provisioned to its verifier.
        crypto("req","-x509","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-batch","-days","2","-subj","/CN=Gateway CA",
               "-keyout",root/"gateway-ca.key","-out",root/"gateway-ca.pem","-addext","basicConstraints=critical,CA:TRUE","-addext","keyUsage=critical,keyCertSign,cRLSign")
        crypto("req","-new","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-batch","-subj","/CN=localhost","-keyout",gw/"server.key","-out",gw/"server.csr")
        (gw/"server.ext").write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n")
        crypto("x509","-req","-in",gw/"server.csr","-CA",root/"gateway-ca.pem","-CAkey",root/"gateway-ca.key","-set_serial","2","-days","1",
               "-extfile",gw/"server.ext","-out",gw/"server.pem")
        shutil.copyfile(ca/"ca.pem",gw/"client-ca.pem")
        gw_cfg=config(gw/"gateway.json",{"gateway_instance_id":str(uuid.uuid4()),"server_certificate_file":str(gw/"server.pem"),
            "server_key_file":str(gw/"server.key"),"client_ca_file":str(gw/"client-ca.pem")})
        sql_url=urlsplit(url)
        pg=dict(env,PGHOST=sql_url.hostname,PGPORT=str(sql_url.port),PGUSER=unquote(sql_url.username),PGPASSWORD=unquote(sql_url.password),PGDATABASE=sql_url.path[1:])
        psql=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
        def sql(statement):return subprocess.check_output([psql,"-X","-w","-At","-v","ON_ERROR_STOP=1"],input=statement,text=True,env=pg).strip()
        base=dict(PATH=env["PATH"],ORANGE_ENV="development",DATABASE_URL=url)
        user_config=private/"identity/user-config.json";site=json.loads(user_config.read_bytes())["user_origin"]
        rid=str(uuid.uuid4());owner=str(uuid.uuid4());receipt_id=str(uuid.uuid4())
        req=json.loads(subprocess.check_output([programs/"orange-worker","prepare-enrollment",device,rid]))
        secret=secrets.token_urlsafe(32)
        if args.device_client:
            def get_handoff(_):
                return json.loads(subprocess.check_output([programs/"orange-worker","prepare-enrollment-handoff",device,rid]))
            with ThreadPoolExecutor(max_workers=2) as pool:handoffs=list(pool.map(get_handoff,range(2)))
            handoff=handoffs[0];secret=(device/(rid+".exchange-token")).read_text()
            require(handoff==handoffs[1] and handoff["request"]==req and
                    handoff["exchange_token_sha256"]==hashlib.sha256(secret.encode()).hexdigest() and secret not in json.dumps(handoff),
                    "Handoff identity, commitment or privacy changed")
        else:handoff={"request":req,"exchange_token_sha256":hashlib.sha256(secret.encode()).hexdigest()}
        session=secrets.token_hex(32);csrf=secrets.token_hex(32)
        path=f"/api/v1/owners/{owner}/worker-enrollments/{rid}"
        headers={"Cookie":"__Host-orange-user="+session,"X-CSRF-Token":csrf,"Origin":site,"Idempotency-Key":rid}
        redeem="/api/v1/worker-enrollments/"+rid+"/redeem"
        auth={"Authorization":"Orange-Enrollment "+secret,"Idempotency-Key":receipt_id}
        rb={"version":1,"redemption_id":receipt_id}
        def issue(path=signer):
            return subprocess.run([programs/"orange-worker-gateway","issue-one",path],env=base,capture_output=True,text=True,timeout=30)
        with listening(programs/"orange-user-api",dict(base,ORANGE_USER_CONFIG_FILE=str(user_config),ORANGE_USER_BIND="127.0.0.1:0"),"User API") as origin:
            sql(f"""INSERT INTO owners(owner_id) VALUES('{owner}');
            INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{owner}','issuer-fixture','{owner}','{owner}');
            INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
            SELECT '{hashlib.sha256(session.encode()).hexdigest()}','{uuid.uuid4()}','{owner}','{csrf}',t,t+600,t+1200 FROM (SELECT extract(epoch FROM now())::bigint t) clock;""")
            code,view=http(origin,"PUT",path,headers,handoff)
            require(code==200 and view["state"]=="authorized","Grant failed")
            require(http(origin,"POST",redeem,auth,rb)[0]==409,"Unsigned grant redeemed")
            bad=issue(config(ca/"wrong.json",dict(cfg,ca_certificate_sha256="0"*64)))
            require(bad.returncode!=0 and not bad.stdout and sql("SELECT count(*) FROM worker_enrollment_certificates")=="0","Unpinned CA signed a grant")
            wrong_key=issue(config(ca/"wrong-key.json",dict(cfg,ca_key_file=str(gw/"server.key"))))
            require(wrong_key.returncode!=0 and not wrong_key.stdout and sql("SELECT count(*) FROM worker_enrollment_certificates")=="0","Unrelated private key signed a grant")
            require(sql("SELECT count(*) FROM worker_issuer_public_keys")=="0","Unverified issuer created public identity metadata")
            with ThreadPoolExecutor(max_workers=2) as parallel:
                answers=list(parallel.map(lambda _:issue(),range(2)))
            for result in answers:result.check_returncode()
            require({json.loads(r.stdout)["prepared_request"] for r in answers}=={None,rid},"Duplicate certificate issuance")
            public_key=ca/"public-spki.pem";public_key.write_bytes(crypto("x509","-in",ca/"ca.pem","-pubkey","-noout"))
            spki_digest=hashlib.sha256(crypto("pkey","-pubin","-in",public_key,"-outform","DER")).hexdigest()
            require(sql(f"SELECT spki_sha256 FROM worker_issuer_public_keys WHERE issuer_sha256='{issuer_hash}'")==spki_digest,
                    "Issuer directory public key differs from the actual CA")
            require(sql("SELECT count(*) FROM worker_credentials")=="0" and http(origin,"GET",path,headers)[1]["state"]=="certificate_prepared","Preparation activated a credential")
            require(http(origin,"POST",redeem,dict(auth,Authorization="Orange-Enrollment "+secrets.token_urlsafe(32)),rb)[0]==401,"Foreign commitment redeemed")
            require(http(origin,"POST",redeem,dict(auth,Cookie=headers["Cookie"]),rb)[0]==401,"Mixed browser authority redeemed")
            if args.device_client:
                from device_certificate_client_acceptance import check
                installed_receipt,installed_pem=check(programs/"orange-worker",origin,root,device,ca,gw,issuer_hash,secret,rid,sql)
                if args.rotation_handoff:
                    from device_rotation_handoff_acceptance import qualify
                    new_rotation=qualify(programs/"orange-worker",device,owner,rid,installed_receipt,sql)

                receipt_id=installed_receipt["redemption_id"];rb["redemption_id"]=receipt_id;auth["Idempotency-Key"]=receipt_id
            with ThreadPoolExecutor(max_workers=2) as parallel:
                replies=list(parallel.map(lambda _:http(origin,"POST",redeem,auth,rb),range(2)))
            require(replies[0]==replies[1] and replies[0][0]==200,"Expected one stable redemption")
            issued=replies[0][1];node=str(uuid.UUID(issued["node_id"]))
            require(issued["issuer_sha256"]==issuer_hash and issued["spki_sha256"]==req["spki_sha256"] and issued["request_id"]==rid,"Wrong receipt")
            if args.device_client:
                require(issued==installed_receipt and installed_pem.read_text()==issued["certificate_pem"],"Installed files differ from backend history")
                leaf=installed_pem
            else:
                leaf=device/"leaf.pem";leaf.write_text(issued["certificate_pem"]);leaf.chmod(0o600)
            require(hashlib.sha256(crypto("x509","-in",leaf,"-outform","DER")).hexdigest()==issued["certificate_sha256"],"Wrong leaf bytes")
            crypto("verify","-purpose","sslclient","-CAfile",ca/"ca.pem",leaf)
            require(sql("SELECT expires_at>clock_timestamp() AND expires_at<=clock_timestamp()+interval '24 hours' FROM worker_enrollment_certificates")=="t","Certificate lifetime outside development policy")
            second=str(uuid.uuid4())
            require(http(origin,"POST",redeem,dict(auth,**{"Idempotency-Key":second}),dict(rb,redemption_id=second))[0]==409,"Token activated a second identity")
            require(sql("SELECT count(*) FROM nodes")=="1" and sql("SELECT count(*) FROM worker_credentials WHERE active")=="1","Wrong authentication effects")
            if args.rotate_device:
                from device_rotation_activation_acceptance import exercise
                exercise(worker=programs/"orange-worker",origin=origin,owner=owner,headers=headers,device=device,root=root,
                         gw_cfg=gw_cfg,programs=programs,base=base,old=issued,old_config=device/"redeem-config.json",new_handoff=new_rotation,
                         issue=issue,sql=sql,http=http,config=config,gateway=gateway,client_test=client_test,env=env,require=require)
                return
            # Losing the authorizing role after signing permanently withdraws the grant, even after regrant.
            team,team_req=str(uuid.uuid4()),str(uuid.uuid4())
            sql(f"""INSERT INTO owners(owner_id) VALUES('{team}'); INSERT INTO organizations(organization_id,name) VALUES('{team}','Issuer team');
                     INSERT INTO memberships(organization_id,user_id,role) VALUES('{team}','{owner}','admin');""")
            team_csr=json.loads(subprocess.check_output([programs/"orange-worker","prepare-enrollment",device,team_req]))
            team_secret=secrets.token_urlsafe(32);team_path=f"/api/v1/owners/{team}/worker-enrollments/{team_req}"
            require(http(origin,"PUT",team_path,dict(headers,**{"Idempotency-Key":team_req}),{"request":team_csr,
                    "exchange_token_sha256":hashlib.sha256(team_secret.encode()).hexdigest()})[0]==200,"Manager grant refused")
            signed=issue();signed.check_returncode();require(json.loads(signed.stdout)["prepared_request"]==team_req,"Team CSR not signed")
            sql(f"UPDATE memberships SET role='viewer' WHERE organization_id='{team}' AND user_id='{owner}'; UPDATE memberships SET role='admin' WHERE organization_id='{team}' AND user_id='{owner}';")
            require(http(origin,"GET",team_path,headers)[1]["state"]=="revoked","Restored role revived old signed grant")
            lost_id=str(uuid.uuid4())
            require(http(origin,"POST","/api/v1/worker-enrollments/"+team_req+"/redeem",{"Authorization":"Orange-Enrollment "+team_secret,"Idempotency-Key":lost_id},
                         {"version":1,"redemption_id":lost_id})[0]==401,"Revoked team issuer activated a node")
            with gateway(programs/"orange-worker-gateway",dict(base,ORANGE_WORKER_GATEWAY_CONFIG=str(gw_cfg),ORANGE_WORKER_GATEWAY_BIND="127.0.0.1:0")) as address:
                fixture=config(root/"live.json",{"gateway":address,"gateway_ca":str(root/"gateway-ca.pem"),"leaf":str(leaf),"private_key":str(device/(rid+".key.pem")),
                    "node":node,"revoke_url":origin+path,"cookie":headers["Cookie"],"csrf":csrf,"origin":site,"request":rid})
                case="newly_redeemed_leaf_authenticates_the_real_worker_then_live_revocation_fences_it"
                result=subprocess.run([*client_test,case,"--exact"],cwd=ROOT,env=dict(env,ORANGE_TEST_ENROLLED_DEVICE=str(fixture)),text=True,stdout=subprocess.PIPE)
                print(result.stdout,end="",flush=True);result.check_returncode();require("test "+case+" ... ok" in result.stdout,"Missing actual gateway revocation")
            require(http(origin,"POST",redeem,auth,rb)[0]==401 and sql("SELECT count(*) FROM worker_credentials WHERE active")=="0","Revoked token replayed")
            require(sql("SELECT count(*) FROM worker_session_audit WHERE kind='session.claimed'")=="1" and sql("SELECT count(*) FROM jobs")=="0","Unexpected control or job effects")
            if args.withdraw_issuer:
                from issuer_withdrawal_acceptance import exercise
                exercise(programs=programs,base=base,private=private,root=root,device=device,ca=ca,gw=gw,gw_cfg=gw_cfg,signer=signer,cfg=cfg,
                    issuer_hash=issuer_hash,spki_digest=spki_digest,origin=origin,owner=owner,headers=headers,sql=sql,crypto=crypto,config=config,
                    issue=issue,http=http,gateway=gateway,listening=listening,require=require,client_test=client_test,env=env,repo=ROOT)
        print(json.dumps({"scenario":"single-owner-withdrawal","installed":args.installed,"real_device_csr":True,"prepared_certificates":2,"team_grant_permanently_withdrawn":True,"worker_credentials":1,"active_after_revoke":0,
                          "real_mtls_welcome":True,"live_heartbeat_refused":True,"jobs":0,"production_signer":False}),flush=True)
    if args.device_client:print("PASS device CLI recovered the original redemption and its installed leaf authenticated the live Worker")
    print("PASS dedicated short certificate issuer, one-time enrollment and immediate live device revocation")

if __name__=="__main__":main()
