"""Exercise an API-authorized direct staging upload; secrets and signed URLs never enter logs."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
from http.client import HTTPConnection
import json
import time
from urllib.parse import urlsplit
from uuid import uuid4
from xml.etree import ElementTree


def require(ok,why):
    if not ok:raise RuntimeError(why)


def provider(authorization,data,method="PUT"):
    endpoint=urlsplit(authorization["url"])
    if endpoint.hostname!="127.0.0.1" or not endpoint.port:raise ValueError("Private fixture required")
    conn=HTTPConnection(endpoint.hostname,endpoint.port,timeout=15)
    try:
        conn.request(method,endpoint.path+"?"+endpoint.query,body=data,headers=authorization["required_headers"])
        reply=conn.getresponse();raw=reply.read()
        return reply.status,reply.getheader("x-amz-version-id"),raw
    finally:conn.close()


def prepare(wire,sql,author,other):
    upload=str(uuid4());grant_key=str(uuid4());payload=b"direct S3 input for Orange\n"*100
    sha=hashlib.sha256(payload).hexdigest();collection="/api/v1/owners/"+author[0]+"/uploads";path=collection+"/"+upload
    status,raw=wire("POST",collection,author,{"upload_id":upload,"size_bytes":str(len(payload)),"sha256":sha,"media_type":"text/plain"},upload)
    require(status==200,"Upload intent unavailable");v=json.loads(raw);version=v["version_id"]
    def issue():return wire("POST",path+"/upload-authorizations",author,{"version_id":version},grant_key)
    with ThreadPoolExecutor(max_workers=2) as threads:
        first,second=threads.map(lambda _:issue(),range(2))
    require(first[0]==200 and second[0]==200,"Concurrent staging issuance failed: "+str([first[0],second[0]])+" "+str(json.loads(first[1]).get("code")))
    cap=json.loads(first[1]);again=json.loads(second[1]);transfer=cap["transfer_id"]
    extra=str(uuid4())
    st,body=wire("POST",collection,author,{"upload_id":extra,"size_bytes":str(len(payload)),"media_type":"text/plain","sha256":sha},extra)
    require(st==200,"Secondary intent missing");extra_version=json.loads(body)["version_id"]
    require(wire("POST",collection+"/"+extra+"/upload-authorizations",author,{"version_id":extra_version},grant_key)[0]==409,
            "Same grant key bound to two uploads")
    require(cap["state"]=="authorized" and cap["artifact_id"]==v["artifact_id"] and cap["version_id"]==version and cap["upload_id"]==upload and
            cap["expires_at_unix"]==again["expires_at_unix"] and again["transfer_id"]==transfer,"Wrong grant identity")
    headers=cap["required_headers"]
    require(headers["if-none-match"]=="*" and headers["content-length"]==str(len(payload)) and "x-amz-checksum-sha256" in headers,
            "Missing signed upload constraints")
    watch=path+"/upload-authorizations/"+transfer;confirm=path+"/upload-confirmations"
    def state():
        status,raw=wire("GET",watch,author);require(status==200,"Missing private upload observation");return json.loads(raw)
    require(state()["state"]=="unconfirmed","New PUT claimed to be complete")
    require(wire("GET",watch,other)[0] in (403,404),"Foreign actor observed staging")
    require(wire("POST",path+"/upload-authorizations",other,{"version_id":version},grant_key)[0] in (403,404),"Foreign actor obtained a PUT")
    require(provider(cap,b"x"*len(payload))[0]==400,"Provider accepted different bytes under fixed checksum")
    require(state()["state"]=="unconfirmed","Failed checksum produced an observable object")
    status,saved,_=provider(cap,payload);require(status==200 and saved and saved!="null","Actual signed PUT failed: "+str(status))
    # Ignore the original success response and recover the immutable source version through the User API.
    observed=state();require(observed["state"]=="ready_to_confirm" and observed["provider_version"]==saved,"Lost PUT response not recoverable")
    require(provider(cap,payload)[0]==412,"A repeated conditional PUT created another provider version")
    require(wire("GET",path+"/content?version_id="+version,author)[0]!=200,"Unconfirmed staging readable")
    request={"version_id":version,"transfer_id":transfer,"provider_version":saved}
    require(wire("POST",confirm,other,request,transfer)[0] in (403,404),"Foreign confirmer admitted")
    sql(f"""CREATE FUNCTION injected_direct_copy_loss() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF EXISTS(SELECT FROM artifact_transfers WHERE transfer_id='{transfer}' AND published_key=NEW.storage_key) THEN RAISE EXCEPTION 'isolated direct copy receipt loss'; END IF;
        RETURN NEW; END $$; CREATE TRIGGER inject_direct_receipt BEFORE INSERT ON artifact_s3_versions FOR EACH ROW EXECUTE FUNCTION injected_direct_copy_loss();""")
    require(wire("POST",confirm,author,request,transfer)[0]>=400,"Unrecorded copy declared available")
    require(state()["state"]=="verifying" and state()["provider_version"]==saved,"Original confirmation attempt disappeared")
    require(wire("POST",confirm,author,{**request,"provider_version":"different-version"},transfer)[0]==409,"Confirmation replaced its immutable version")
    sql("DROP TRIGGER inject_direct_receipt ON artifact_s3_versions;")
    return dict(transfer=transfer,upload=upload,view=v,request=request,payload=payload,grant=cap,key=grant_key,path=path,watch=watch,confirm=confirm)


def recover(case,wire,sql,author,objects,prefix):
    t=case["transfer"];req=case["request"];v=case["view"]
    stage,dest=sql(f"SELECT stage_key,published_key FROM artifact_transfers WHERE transfer_id='{t}'").split("|")
    status,meta,data=objects("GET",prefix+"/"+dest)
    require(status==200 and data==case["payload"],"Crash was not after provider copy")
    concrete=meta["x-amz-version-id"]
    with ThreadPoolExecutor(max_workers=2) as threads:
        replies=list(threads.map(lambda _:wire("POST",case["confirm"],author,req,t),range(2)))
    require(all(r[0]==200 for r in replies),"Original confirmation could not recover: "+str([(status,json.loads(body).get("code")) for status,body in replies]))
    require(replies[0]==replies[1] and json.loads(replies[0][1])["state"]=="available","Competing confirmations produced different outcomes")
    require(wire("GET",case["path"]+"/content?version_id="+v["version_id"],author)==(200,data),"Published bytes changed")
    status,raw=wire("POST",case["path"]+"/upload-authorizations",author,{"version_id":v["version_id"]},case["key"])
    require(status==200 and json.loads(raw)["state"]=="available" and "url" not in json.loads(raw),"Replay issued a new staging capability after attachment")
    status,_,versions=objects("GET",query={"versions":"","prefix":prefix+"/"+dest})
    require(status==200 and sum(1 for x in ElementTree.fromstring(versions) if x.tag.split("}")[-1]=="Version")==1,
            "Lost confirmation replay made multiple published versions")
    bound=sql(f"SELECT provider_version FROM artifact_s3_versions p JOIN artifacts a USING(root_id) WHERE p.storage_key='{dest}' AND a.artifact_id='{v['artifact_id']}'")
    require(bound==concrete,"Reconciliation did not recover the original provider receipt")
    print(json.dumps({"real_direct_ingress":True,"conditional_retry_denied":True,"source_version_recovered":True,"copy_response_lost":True,
                      "process_restarted":True,"debit_rows":int(sql("SELECT count(*) FROM ledger_transactions")),"publish_versions":1}),flush=True)
    return stage


def deletion_during_copy(wire,sql,author,fence,prefix,bucket):
    upload=str(uuid4());data=b"retire during immutable copy";base="/api/v1/owners/"+author[0]+"/uploads";path=base+"/"+upload
    st,body=wire("POST",base,author,{"upload_id":upload,"size_bytes":str(len(data)),"media_type":"text/plain","sha256":hashlib.sha256(data).hexdigest()},upload)
    require(st==200,"Deletion race intent missing");version=json.loads(body)["version_id"]
    st,body=wire("POST",path+"/upload-authorizations",author,{"version_id":version},str(uuid4()));require(st==200,"Temporary upload rejected")
    cap=json.loads(body);transfer=cap["transfer_id"]
    st,src,_=provider(cap,data);require(st==200 and src,"No physical source for deletion race")
    key=sql(f"SELECT published_key FROM artifact_transfers WHERE transfer_id='{transfer}'")
    fence.path="/"+bucket+"/"+prefix+"/"+key;fence.method="PUT";fence.received.clear();fence.release.clear()
    req={"transfer_id":transfer,"version_id":version,"provider_version":src}
    with ThreadPoolExecutor(max_workers=1) as threads:
        queued=threads.submit(wire,"POST",path+"/upload-confirmations",author,req,transfer)
        try:
            require(fence.received.wait(12),"Did not observe the real versioned CopyObject response")
            require(wire("DELETE",path,author,key=upload)[0]==200,"Deletion lost its database update")
        finally:fence.release.set()
        status,body=queued.result(timeout=25)
        require(status in (403,410) and "url" not in json.loads(body),"Late copy published deleted input")
    fence.path=None;fence.method="HEAD"
    require(wire("POST",path+"/upload-confirmations",author,req,transfer)[0] in (403,410),"Deleted confirmation replay succeeded")
    require(wire("POST",path+"/download-authorizations",author,{"version_id":version},str(uuid4()))[0] in (403,410),"Deleted copy received a valid download grant")
    require(sql(f"SELECT state,published_transfer_id IS NULL FROM artifacts WHERE version_id='{version}'")=="deleted|t","Deleted artifact was attached")
    print("PASS real provider copy completed before logical deletion; late confirmation, replay and new download grants refused",flush=True)


def expire_and_reclaim(case,wire,sql,objects,prefix,author):
    t=case["transfer"];cap=case["grant"];path=case["path"];data=case["payload"]
    stage,published=sql(f"SELECT stage_key,published_key FROM artifact_transfers WHERE transfer_id='{t}'").split("|")
    original=case["request"]["provider_version"]
    require(sql(f"SELECT stage_deleted,reserved_bytes FROM artifact_transfers WHERE transfer_id='{t}'")==f"f|{len(data)*2}",
            "Capacity was released while the upload capability was live")
    require(objects("GET",prefix+"/"+stage,{"versionId":original})[2]==data,"Stage disappeared while grant live")
    until=int(cap["expires_at_unix"])
    time.sleep(max(0,until+2-time.time()))
    require(provider(cap,data)[0]==403,"A new PUT started with an expired credential")
    limit=time.monotonic()+75
    while time.monotonic()<limit and sql(f"SELECT stage_deleted FROM artifact_transfers WHERE transfer_id='{t}'")!="t":time.sleep(.5)
    require(sql(f"SELECT stage_deleted,reserved_bytes FROM artifact_transfers WHERE transfer_id='{t}'")==f"t|{len(data)}",
            "Application cleanup did not confirm its original stage and release exactly its reservation")
    require(objects("GET",prefix+"/"+stage,{"versionId":original})[0]==404,"Original staged bytes survived")
    status,h,raw=objects("GET",prefix+"/"+stage)
    require(status==200 and raw==b"" and h.get("x-amz-meta-orange-closed")=="1","Missing empty conditional-write fence")
    require(wire("GET",path+"/content?version_id="+case["view"]["version_id"],author)==(200,data),"Cleanup damaged published version")
    print("PASS API-issued five-minute PUT expired on the real clock; exact staged bytes removed, empty fence retained and published bytes unchanged",flush=True)
