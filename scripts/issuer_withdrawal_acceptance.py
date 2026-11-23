"""Verify maker/checker authority and actual signer/activation/control effects together."""
import hashlib
import json
import secrets
import subprocess
import time
import uuid


def exercise(*,programs,base,private,root,device,ca,gw,gw_cfg,signer,cfg,issuer_hash,spki_digest,origin,owner,headers,sql,crypto,config,
             issue,http,gateway,listening,require,client_test,env,repo):
    def new_device(sign,activate=True):
        rid=str(uuid.uuid4());handoff=json.loads(subprocess.check_output([programs/"orange-worker","prepare-enrollment-handoff",device,rid]))
        require(http(origin,"PUT",f"/api/v1/owners/{owner}/worker-enrollments/{rid}",dict(headers,**{"Idempotency-Key":rid}),handoff)[0]==200,"Owner approval")
        response=issue(sign)
        require(response.returncode==0,"Signer refused device: "+response.stderr.strip())
        require(json.loads(response.stdout)["prepared_request"]==rid,"Wrong prepared leaf")
        receipt_id=str(uuid.uuid4());auth={"Authorization":"Orange-Enrollment "+(device/(rid+".exchange-token")).read_text(),"Idempotency-Key":receipt_id}
        url="/api/v1/worker-enrollments/"+rid+"/redeem";body={"version":1,"redemption_id":receipt_id}
        if not activate:return url,auth,body
        status,receipt=http(origin,"POST",url,auth,body);require(status==200,"Device redemption")
        leaf=device/(rid+".pem");leaf.write_text(receipt["certificate_pem"]);leaf.chmod(0o600)
        return {"node":receipt["node_id"],"leaf":str(leaf),"key":str(device/(rid+".key.pem"))}
    def root_config(label,same_key):
        key=ca/"ca.key" if same_key else ca/(label+".key")
        cert=ca/(label+".pem");params=["-key",key] if same_key else ["-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-keyout",key]
        crypto("req","-x509",*params,"-nodes","-batch","-days","2","-subj","/CN="+label,"-out",cert,
               "-addext","basicConstraints=critical,CA:TRUE","-addext","keyUsage=critical,keyCertSign,cRLSign")
        cert.chmod(0o600);pin=hashlib.sha256(crypto("x509","-in",cert,"-outform","DER")).hexdigest()
        return config(ca/(label+".json"),dict(cfg,ca_key_file=str(key),ca_certificate_file=str(cert),ca_certificate_sha256=pin)),cert
    alias,alias_cert=root_config("Alias",True);other,other_cert=root_config("Independent",False)
    target=new_device(signer);pending=new_device(alias,False);survivor=new_device(other)
    (gw/"client-ca.pem").write_bytes((ca/"ca.pem").read_bytes()+alias_cert.read_bytes()+other_cert.read_bytes())
    # Pre-upgrade stream fixture: no historical chain attribution exists for this row.
    legacy=str(uuid.uuid4());legacy_leaf=hashlib.sha256(legacy.encode()).hexdigest()
    sql(f"""INSERT INTO nodes(node_id,owner_id,environment,region_id,state) VALUES('{legacy}','{owner}','dev','local-test','online');
          INSERT INTO worker_credentials(certificate_sha256,node_id,enrollment_user_id,not_before,expires_at)
          VALUES('{legacy_leaf}','{legacy}','{owner}',now()-interval '1 minute',now()+interval '1 hour');
          INSERT INTO worker_sessions(node_id,gateway_instance_id,connection_epoch,session_id,certificate_sha256,capabilities_digest,last_seen_at,expires_at)
          VALUES('{legacy}','legacy',1,'{uuid.uuid4()}','{legacy_leaf}',repeat('a',64),now(),now()+interval '5 minutes');""")
    admin_cfg=private/"identity/admin-config.json";site=json.loads(admin_cfg.read_bytes())["admin_origin"]
    def principal(role,kind,scope):
        who=str(uuid.uuid4());token=secrets.token_hex(32);csrf=secrets.token_hex(32)
        sql(f"""INSERT INTO admin_principals(principal_id,issuer,subject) VALUES('{who}','issuer-approval-fixture','{who}');
            INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) SELECT '{who}','{role}','dev','{kind}','{scope}',extract(epoch FROM now())::bigint+600;
            INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
            SELECT '{hashlib.sha256(token.encode()).hexdigest()}','{who}','{csrf}',n,n+600,n+1200 FROM (SELECT extract(epoch FROM now())::bigint n)t;""")
        return who,{"Cookie":"__Host-orange-admin="+token,"X-CSRF-Token":csrf,"Origin":site}
    maker,mh=principal("platform_admin","platform","platform");checker,ch=principal("security_operator","platform","platform");_,rh=principal("security_operator","region","local-test")
    with listening(programs/"orange-admin-api",dict(base,ORANGE_ADMIN_CONFIG_FILE=str(admin_cfg),ORANGE_ADMIN_BIND="127.0.0.1:0"),"Admin API") as admin:
        prefix="/admin/v1/device-issuer-keys/"+spki_digest+"?environment=dev&scope_kind=platform&scope_id=platform"
        def read(h):return http(admin,"GET",prefix,h)
        require(read(rh)[0]==403,"Region reader saw platform key detail")
        code,view=read(mh);require(code==200 and view["state"]=="observed" and view["observed_ca_certificates"]=="2","Wrong signer identity or alias impact")
        scope={"environment":"dev","kind":"platform","scope_id":"platform"}
        draft={"scope":scope,"target_version":view["target_version"],"ticket_id":"issuer-qualification","reason":"Disposable signer withdrawal qualification",
               "action_type":"device_issuer.withdraw","payload":{"signing_key_sha256":spki_digest}}
        def post(path,h,data,key,etag=None):
            hh=dict(h,**{"Idempotency-Key":key})
            if etag:hh["If-Match"]='"'+etag+'"'
            return http(admin,"POST",path,hh,data)
        action_path="/admin/v1/actions"
        sql(f"UPDATE admin_sessions SET auth_time=extract(epoch FROM now())::bigint-360 WHERE principal_id='{maker}'")
        require(post(action_path,mh,draft,"stale-issuer-maker")[0]==403,"Stale step-up accepted")
        sql(f"UPDATE admin_sessions SET auth_time=extract(epoch FROM now())::bigint WHERE principal_id='{maker}'")
        code,doc=post(action_path,mh,draft,"withdraw-key")
        require(code==201 and doc["state"]=="pending_approval" and doc["approval_policy"]=="maker_checker","No independent approval")
        action=doc["action_id"];path=action_path+"/"+action
        approval={"decision":"approve","payload_digest":doc["payload_digest"],"policy_version":doc["policy_version"],"reason":"Independent security review"}
        execute={k:approval[k] for k in ("payload_digest","policy_version")}
        require(post(path+"/approvals",mh,approval,"self",doc["version"])[0]==403,"Maker self-approved")
        require(post(path+"/approvals",rh,approval,"cross-region",doc["version"])[0]==403,"Regional checker crossed scope")
        require(post(path+"/execute",mh,execute,"unapproved",doc["version"])[0]==409,"Unapproved action executed")
        code,ok=post(path+"/approvals",ch,approval,"approved",doc["version"]);require(code==200 and ok["state"]=="ready","Checker failed")
        sql(f"UPDATE admin_grants SET revoked=true WHERE principal_id='{checker}'")
        require(post(path+"/execute",mh,execute,"inactive-checker",ok["version"])[0]==403,"Inactive checker authorized execution")
        require(sql("SELECT count(*) FROM worker_issuer_withdrawals")=="0","Denied action mutated authority")
        # A direct write referencing a different key must also fail at the storage boundary.
        sql(f"""DO $$ BEGIN
             BEGIN
               INSERT INTO worker_issuer_withdrawals(signing_key_sha256,action_id,maker_id,checker_id)
               VALUES(repeat('0',64),'{action}','{maker}','{checker}');
               RAISE EXCEPTION 'unbound key accepted';
             EXCEPTION WHEN check_violation THEN NULL; END;
             END $$;""")
        # Use a fresh approval from a distinct live checker after the negative case.
        _,other_checker=principal("security_operator","platform","platform")
        draft=dict(draft,ticket_id="independent-reissue");code,doc=post(action_path,mh,draft,"second-action");require(code==201,"Fresh action")
        path=action_path+"/"+doc["action_id"];approval.update(payload_digest=doc["payload_digest"],policy_version=doc["policy_version"])
        execute={k:approval[k] for k in ("payload_digest","policy_version")}
        code,ok=post(path+"/approvals",other_checker,approval,"fresh-checker",doc["version"]);require(code==200,"Fresh independent approval")
        with gateway(programs/"orange-worker-gateway",dict(base,ORANGE_WORKER_GATEWAY_CONFIG=str(gw_cfg),ORANGE_WORKER_GATEWAY_BIND="127.0.0.1:0")) as endpoint:
            ready=root/"signer.ready";executed=root/"signer.executed"
            context=config(root/"signer-session.json",dict(gateway=endpoint,ca=str(root/"gateway-ca.pem"),target=target,survivor=survivor,ready=str(ready),executed=str(executed)))
            case="independent_admin_withdrawal_fences_the_live_signer_and_preserves_another_path"
            with subprocess.Popen([*client_test,case,"--exact"],cwd=repo,env=dict(env,ORANGE_TEST_SIGNER_SESSION=str(context)),text=True,stdout=subprocess.PIPE) as proc:
                try:
                    until=time.monotonic()+20
                    while not ready.exists() and time.monotonic()<until and proc.poll() is None:time.sleep(.025)
                    require(ready.exists(),"Both real control streams did not open")
                    status,impact=read(mh)
                    require(status==200 and impact.get("live_verified_sessions")=="1" and impact.get("live_unattributed_sessions")=="1",
                            f"Wrong path impact: status={status} verified={impact.get('live_verified_sessions')} unknown={impact.get('live_unattributed_sessions')}")
                    code,out=post(path+"/execute",mh,execute,"commit-withdrawal",ok["version"])
                    require(code==202 and out["state"]=="succeeded" and out["business_ref"]==spki_digest,f"Issuer withdrawal failed: HTTP {code}, state={out.get('state')}, code={out.get('code')}")
                    require(post(path+"/execute",mh,execute,"commit-withdrawal",ok["version"])==(code,out),"Lost response duplicated withdrawal")
                    executed.write_bytes(b"done");stdout=proc.communicate(timeout=25)[0];print(stdout,flush=True)
                    require(proc.returncode==0 and "test "+case+" ... ok" in stdout,"Missing live signer fencing")
                finally:
                    if proc.poll() is None:proc.kill();proc.wait()
        final=read(mh)[1]
        require(final["withdrawal_action_id"]==doc["action_id"] and final["state"]=="withdrawn"
                and final["valid_issued_credentials"]=="0" and final["live_verified_sessions"]=="0" and final["live_unattributed_sessions"]=="0",
                "Wrong retained action or remaining affected authority")
        require(sql("SELECT revision FROM worker_issuer_policy_version WHERE singleton")=="1","Invalid policy revision")
        sql(f"""DO $$ BEGIN
             BEGIN DELETE FROM worker_issuer_withdrawals WHERE signing_key_sha256='{spki_digest}';
               RAISE EXCEPTION 'withdrawal deleted';
             EXCEPTION WHEN check_violation THEN NULL; END;
             END $$;""")
        require(sql("SELECT count(*) FROM worker_issuer_withdrawals WHERE maker_id<>checker_id")=="1","Duplicate or self-approved record")
        require(sql(f"SELECT (expires_at<=clock_timestamp())::int FROM worker_sessions WHERE node_id='{legacy}'")=="1","Old unattributed control not expired")
        require(http(origin,"POST",*pending)[0]==401,"Alias redeemed after key withdrawal")
        reissued,_=root_config("ReissuedAfterWithdrawal",True)
        for candidate in (signer,alias,reissued):
            denied=issue(candidate);require(denied.returncode!=0 and not denied.stdout,"Withdrawn key signed or registered as a new alias")
        new_device(other)
        require(sql("SELECT count(*) FROM jobs")=="0","Issuer action granted execution")
    print("PASS two independent platform principals withdrew one verified signing key across CA aliases, issued and prepared leaves, and live control; another issuer remained usable")
