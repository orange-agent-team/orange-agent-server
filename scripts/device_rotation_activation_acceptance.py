"""Exercise two generations of real owner-approved certificate rotation and the existing gRPC Worker client."""
import copy
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import subprocess
import uuid


def exercise(*,worker,origin,owner,headers,device,root,gw_cfg,programs,base,old,old_config,
             new_handoff,issue,sql,http,config,gateway,client_test,env,require,web=None,assets=None,site=None,login=None):
    old_request=old["request_id"]
    scope=f"/api/v1/owners/{owner}"
    def browser(mode,**values):
        node=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
        project=web.resolve(strict=True)
        files=(assets or project/"apps/web/dist").resolve(strict=True)
        require(site=="https://127.0.0.1:43122" and (files/"index.html").is_file(),"Expected configured development origin and static site")
        data={"api":origin,"origin":site,"login":login,"assets":str(files),"mode":mode,**values}
        result=subprocess.run([node,project/"apps/web/scripts/real-worker-rotation.mjs"],input=json.dumps(data),text=True,cwd=project,
                              stdout=subprocess.PIPE,timeout=100)
        print(result.stdout,end="",flush=True);result.check_returncode()
        require("PASS real owner rotation browser "+mode in result.stdout.splitlines(),"No real browser receipt")
    def handoff(cfg):
        request=str(uuid.uuid4())
        return json.loads(subprocess.check_output([worker,"prepare-certificate-rotation",cfg,owner,request],timeout=20))
    def authorize(value,normal=False):
        req=value["intent"]["replacement"]["request"]["request_id"]
        path=("worker-enrollments" if normal else "worker-certificate-rotations")+"/"+req
        return http(origin,"PUT",scope+"/"+path,dict(headers,**{"Idempotency-Key":req}),value["intent"]["replacement"] if normal else value)
    def status(req):
        code,record=http(origin,"GET",scope+"/worker-certificate-rotations/"+req,headers)
        require(code==200 and record["enrollment"]["request_id"]==req and record["enrollment"]["owner_id"]==owner,
                "Wrong owner-scoped rotation readback")
        return record
    def prepare(value):
        require(authorize(value)[0]==200 and authorize(value)[0]==200,"Owner did not authorize the exact rotation replay")
        req=value["intent"]["replacement"]["request"]["request_id"]
        record=status(req)
        require(record["node_id"]==value["intent"]["node_id"] and record["previous_request_id"]==value["intent"]["previous_request_id"]
                and record["previous_certificate_sha256"]==value["intent"]["previous_certificate_sha256"] and record["predecessor_eligible"]
                and record["activated_unix"] is None and record["overlap_until_unix"] is None,"Wrong immutable authorization binding")
        require(authorize(value,True)[0]==409,"An initial enrollment adopted an existing rotation request")
        issued=issue();require(issued.returncode==0,"Issuer did not stage the replacement")
        req=value["intent"]["replacement"]["request"]["request_id"]
        prior=json.loads(Path(old_config).read_bytes())
        prior.update(request_id=req,api_origin=origin,api_tls_ca_file=None,exchange_token_file=str(device/(req+".exchange-token")))
        return req,config(device/(req+".rotate-config.json"),prior)
    def activate(cfg):
        return subprocess.run([worker,"redeem-certificate-rotation",cfg],capture_output=True,timeout=15)
    def session(label,previous,current,deadline=None):
        with gateway(programs/"orange-worker-gateway",dict(base,ORANGE_WORKER_GATEWAY_CONFIG=str(gw_cfg),ORANGE_WORKER_GATEWAY_BIND="127.0.0.1:0")) as address:
            values={"gateway":address,"gateway_ca":str(root/"gateway-ca.pem"),"node":old["node_id"]}
            for name,r in (("previous",previous),("replacement",current)):
                values[name]={"leaf":str(device/(r["request_id"]+".client.pem")),"key":str(device/(r["request_id"]+".key.pem"))}
            if deadline:values["deadline"]=deadline
            fixture=config(root/(label+".json"),values)
            case="rotated_device_fences_old_control_and_preserves_node_identity"
            run=subprocess.run([*client_test,case,"--exact"],env=dict(env,ORANGE_TEST_ROTATION_SESSION=str(fixture)),
                               stdout=subprocess.PIPE,text=True,timeout=40)
            print(run.stdout,end="",flush=True);run.check_returncode()
            require("test "+case+" ... ok" in run.stdout,"Real mTLS rotation case missing")
    require(http(origin,"GET",scope+"/worker-certificate-rotations/"+old_request,headers)[0]==404,"An initial enrollment was presented as rotation")
    before=sql("SELECT count(*) FROM worker_enrollment_grants")
    for field in ("node_id","previous_certificate_sha256","previous_issuer_sha256","previous_request_id"):
        wrong=copy.deepcopy(new_handoff);wrong["intent"][field]=str(uuid.uuid4()) if field.endswith("_id") else "b"*64
        require(authorize(wrong)[0] in (409,422),"Changed predecessor admitted")
    forged=copy.deepcopy(new_handoff);forged["previous_key_signature"]="A"*86
    require(authorize(forged)[0] in (409,422),"Invalid old-key proof admitted")
    require(sql("SELECT count(*) FROM worker_enrollment_grants")==before,"Invalid proof left a grant")
    if web:
        browser("authorize",handoff=new_handoff)
        require(status(new_handoff["intent"]["replacement"]["request"]["request_id"])["activated_unix"] is None,
                "Browser did not persist original owner authorization")
    first=prepare(new_handoff);second=prepare(handoff(old_config))
    require(sql("SELECT count(*) FROM nodes")=="1" and sql("SELECT count(*) FROM worker_credentials")=="1","Preparation activated a new device")
    # Both authorized replacements existed before either one was redeemed.
    with ThreadPoolExecutor(max_workers=2) as pool:
        a,b=list(pool.map(activate,(first[1],second[1])))
    require(sorted([a.returncode==0,b.returncode==0])==[False,True],"Predecessor consumed other than once")
    (req,cfg),winner=(first,a) if a.returncode==0 else (second,b)
    installed=json.loads(winner.stdout)
    require(installed["node_id"]==old["node_id"] and installed["request_id"]==req,"Rotation created another node")
    replay=activate(cfg);require(replay.returncode==0 and json.loads(replay.stdout)["source"]=="local_receipt","Lost response not recoverable locally")
    current=json.loads((device/(req+".receipt.json")).read_bytes())
    current_record=status(req)
    lost=second[0] if req==first[0] else first[0]
    lost_record=status(lost)
    require(not lost_record["predecessor_eligible"] and lost_record["activated_unix"] is None and
            int(current_record["overlap_until_unix"])-int(current_record["activated_unix"])==120,"Lost activation outcome was misreported")
    active=sql("SELECT count(*) FROM worker_credentials WHERE active AND expires_at>clock_timestamp()")
    require(active=="2" and sql("SELECT count(*) FROM worker_certificate_rotation_receipts")=="1","Wrong overlap inventory")
    require(sql("SELECT overlap_until-activated_at=interval '2 minutes' FROM worker_certificate_rotation_receipts")=="t","Full overlap deadline not pinned")
    session("real-first-takeover",old,current)
    require(sql("SELECT count(*) FROM worker_credentials WHERE active")=="1","Accepted new Hello failed to retire old leaf")
    require(authorize(handoff(old_config))[0]==409,"A retired key authorized another replacement")
    last_value=handoff(cfg);next_req,next_cfg=prepare(last_value)
    # Shorten only the already-authoritative predecessor cutoff in this test-owned database. Exercise
    # the real passage of time and the case where an existing expiry is sooner than two minutes.
    sql(f"UPDATE worker_credentials SET expires_at=clock_timestamp()+interval '15 seconds' WHERE certificate_sha256='{current['certificate_sha256']}'")
    last=activate(next_cfg);require(last.returncode==0,"Current key could not rotate a second time")
    newest=json.loads((device/(next_req+".receipt.json")).read_bytes())
    deadline=int(sql(f"SELECT ceil(extract(epoch FROM overlap_until))::bigint FROM worker_certificate_rotation_receipts WHERE request_id='{next_req}'"))
    require(sql(f"SELECT c.expires_at=r.overlap_until AND r.overlap_until<r.activated_at+interval '16 seconds' FROM worker_credentials c JOIN worker_certificate_rotation_receipts r ON r.previous_certificate_sha256=c.certificate_sha256 WHERE r.request_id='{next_req}'")=="t","Rotation extended a preexisting expiration")
    session("real-expired-predecessor",current,newest,deadline)
    if web:
        browser("revoke",request=req)
        require(status(next_req)["enrollment"]["state"]=="revoked","Browser withdrawal failed to propagate to the next generation")
    require(http(origin,"DELETE",scope+"/worker-enrollments/"+old_request,dict(headers,**{"Idempotency-Key":old_request}))[0]==200,"Original consent withdrawal failed")
    require(sql("SELECT count(*) FROM worker_credentials WHERE active")=="0","A rotated credential survived root consent withdrawal")
    require(sql("SELECT count(*) FROM nodes")=="1" and sql("SELECT count(*) FROM jobs")=="0","Rotation allocated resources")
    print(json.dumps({"stable_node":True,"active_generations":2,"competing_redemptions":2,"initial_winners":1,
                      "legacy_hello_refused_after_takeover":True,"live_predecessor_denied_after_actual_deadline":True,
                      "root_withdrawal_deactivated_family":True,"nodes_created_by_rotation":0,"jobs_created":0}),flush=True)
    print("PASS authorized Worker rotation, two real mTLS generations, bounded expiry and family revocation",flush=True)
