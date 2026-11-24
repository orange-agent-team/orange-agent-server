"""Actual User API, PostgreSQL and isolated S3. Never print a reusable capability."""
from concurrent.futures import ThreadPoolExecutor
from hashlib import sha256
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import secrets
import socket
import runpy
import shutil
import subprocess
import time
from urllib.parse import urlsplit,unquote,urlencode,parse_qs
from uuid import uuid4

from artifact_object_versions import private_requests
from owned_postgres import database
from platform_package_smoke import listening


def require(ok,msg):
    if not ok: raise RuntimeError(msg)


def run(api,dbfile,identity,folder,endpoint,bucket,credentials,s3,fence,expiry,web,site,png,rgb,old_api,direct_upload,direct_expiry,upgrade_remote):
    def secret(path,value):
        with os.fdopen(os.open(path,os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600),"w") as stream:json.dump(value,stream)
    prefix="orange-fixture";cfg=folder/"artifact-config.json";keys=folder/"artifact-identity.json"
    secret(keys,{"access_key":credentials["accessKey"],"secret_key":credentials["secretKey"]})
    secret(cfg,{"endpoint":endpoint,"bucket":bucket,"region":"us-east-1","prefix":prefix,"credentials_file":str(keys)})
    client_config=json.loads(identity.read_bytes())
    if web:
        with socket.socket() as probe:
            probe.bind(("127.0.0.1",0));port=probe.getsockname()[1]
        client_config["user_origin"]=f"https://127.0.0.1:{port}"
        identity=folder/"api-user-config.json";secret(identity,client_config)
    origin=client_config["user_origin"]
    del client_config
    with database(dbfile,"orange_object_versions_") as (_,url):
        db=urlsplit(url)
        pg=dict(os.environ,PGHOST=db.hostname,PGPORT=str(db.port or 5432),PGUSER=unquote(db.username),PGPASSWORD=unquote(db.password),PGDATABASE=db.path[1:])
        def sql(statement):
            return subprocess.check_output([shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql","-X","-w","-Atq","-v","ON_ERROR_STOP=1"],
                                           input=statement,env=pg,text=True).strip()
        settings=dict(PATH=os.environ["PATH"],ORANGE_ENV="development",DATABASE_URL=url,ORANGE_USER_CONFIG_FILE=str(identity),
                      ORANGE_USER_BIND="127.0.0.1:0",ORANGE_ARTIFACT_S3_CONFIG=str(cfg))
        def member():
            user=str(uuid4());token=secrets.token_hex(32);csrf=secrets.token_hex(32)
            sql(f"""INSERT INTO owners(owner_id) VALUES('{user}');
                    INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{user}','s3-artifact-dev','{user}','{user}');
                    INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                    SELECT '{sha256(token.encode()).hexdigest()}','{uuid4()}','{user}','{csrf}',t,t+1200,t+3600 FROM (SELECT floor(extract(epoch FROM now()))::bigint t) clk;""")
            return user,token,csrf
        def wire(method,path,actor,data=None,key=None):
            conn=HTTPConnection("127.0.0.1",urlsplit(address).port,timeout=30)
            try:
                headers={"Cookie":"__Host-orange-user="+actor[1]}
                if key:headers.update({"Origin":origin,"X-CSRF-Token":actor[2],"Idempotency-Key":key})
                if isinstance(data,dict):headers["Content-Type"]="application/json";data=json.dumps(data).encode()
                elif data is not None:headers["Content-Type"]="application/octet-stream"
                conn.request(method,path,body=data,headers=headers);res=conn.getresponse();body=res.read()
                require(res.getheader("Cache-Control")=="no-store","Response can be cached")
                return res.status,body
            finally:conn.close()
        def grant(who,k):
            status,body=wire("POST",saved+"/download-authorizations",who,{"version_id":version},k)
            return status,json.loads(body)
        def get_signed(item):
            url=urlsplit(item["url"]);target=urlsplit(endpoint)
            require(url.scheme==target.scheme and url.netloc==target.netloc,"Unexpected storage origin")
            args=parse_qs(url.query)
            require(args["versionId"]==[provider] and 0<int(args["X-Amz-Expires"][0])<=60,"Unpinned or overlong capability")
            conn=HTTPConnection("127.0.0.1",target.port,timeout=12)
            try:
                conn.request("GET",url.path+"?"+url.query);res=conn.getresponse();body=res.read()
                if res.status!=200:
                    from xml.etree import ElementTree
                    try:code=next((v.text for v in ElementTree.fromstring(body).iter() if v.tag.split("}")[-1]=="Code"),None)
                    except ElementTree.ParseError:code=None
                    raise RuntimeError("Signed GET denied: "+str((res.status,code)))
                require(body==data and res.getheader("x-amz-version-id")==provider,"Wrong versioned signed bytes")
                require(res.getheader("Content-Disposition")=="attachment" and res.getheader("Content-Type")=="application/octet-stream"
                        and "no-store" in (res.getheader("Cache-Control") or ""),"Signed object response was not a private download")
            finally:conn.close()
        provider_call=private_requests(endpoint,bucket,credentials,folder)
        if old_api:
            previous=dict(settings)
            expected_schema="88" if upgrade_remote else "87"
            if not upgrade_remote:
                del previous["ORANGE_ARTIFACT_S3_CONFIG"]
                disk=folder/"old-private-root";disk.mkdir(mode=0o700);previous["ORANGE_ARTIFACT_ROOT"]=str(disk)
            with listening(old_api.resolve(strict=True),previous,"User API") as address:
                require(sql("SELECT max(version) FROM _sqlx_migrations WHERE success")==expected_schema,"Unexpected schema from the prior executable")
                prior_user=member();prior_upload=str(uuid4());prior_bytes=b"retained private object from schema 87"
                prior_base="/api/v1/owners/"+prior_user[0]+"/uploads"
                status,raw=wire("POST",prior_base,prior_user,{"upload_id":prior_upload,"media_type":"text/plain","size_bytes":str(len(prior_bytes)),"sha256":sha256(prior_bytes).hexdigest()},prior_upload)
                require(status==200,"Cannot prepare original upload");prior_version=json.loads(raw)["version_id"]
                prior_path=prior_base+"/"+prior_upload
                require(wire("PUT",prior_path+"/content",prior_user,prior_bytes,prior_upload)[0]==200,"Prior file not available")
                original=sql("SELECT jsonb_agg(to_jsonb(a) ORDER BY a.artifact_id) FROM artifacts a")
                old_read=prior_path+"/content?"+urlencode({"version_id":prior_version})
                if upgrade_remote:
                    prior_key=str(uuid4());download=prior_path+"/download-authorizations"
                    status,raw=wire("POST",download,prior_user,{"version_id":prior_version},prior_key);require(status==200,"Pre-upgrade signed read unavailable")
                    old_cut=json.loads(raw)["expires_at_unix"]
                    pins=sql("SELECT jsonb_agg(to_jsonb(p) ORDER BY p.storage_key) FROM artifact_s3_versions p")
            with listening(api,previous,"User API") as address:
                require(sql("SELECT max(version) FROM _sqlx_migrations WHERE success")=="89" and sql("SELECT jsonb_agg(to_jsonb(a) ORDER BY a.artifact_id) FROM artifacts a")==original,
                        "Migration changed the original artifact row")
                require(wire("GET",old_read,prior_user)==(200,prior_bytes),"Existing filesystem content changed across upgrade")
                if upgrade_remote:
                    status,raw=wire("POST",download,prior_user,{"version_id":prior_version},prior_key)
                    require(status==200 and json.loads(raw)["expires_at_unix"]==old_cut and sql("SELECT jsonb_agg(to_jsonb(p) ORDER BY p.storage_key) FROM artifact_s3_versions p")==pins,
                            "Migration changed a provider pin or extended an outstanding signed authority")
                else:
                    status,raw=wire("POST",prior_path+"/download-authorizations",prior_user,{"version_id":prior_version},str(uuid4()))
                    require(status==503 and json.loads(raw).get("code")=="DIRECT_STORAGE_UNAVAILABLE","Filesystem root issued an S3 address")
                require(wire("PUT",prior_path+"/content",prior_user,prior_bytes,prior_upload)[0]==200,"Existing upload retry failed")
                print("PASS actual schema "+expected_schema+" to 89 upgrade; preserved original bytes and previous transfer authority",flush=True)
            if upgrade_remote:
                probe=subprocess.Popen([old_api.resolve(strict=True)],env=previous,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
                try:_,why=probe.communicate(timeout=15)
                except subprocess.TimeoutExpired:
                    probe.terminate();probe.wait(timeout=10);raise RuntimeError("Old writer started on the upgraded DB")
                require(probe.returncode!=0 and b"STARTUP_MIGRATION_FAILED" in why,"Downgrade did not reject the unknown storage migration")
                print("PASS schema-88 writer refuses the schema-89 database",flush=True)
        with listening(api,settings,"User API") as address:
            if old_api and not upgrade_remote:
                require(wire("GET",old_read,prior_user)[0]!=200,"A different root exposed a prior filesystem object")
            author,other=member(),member()
            upload=str(uuid4());base="/api/v1/owners/"+author[0]+"/uploads";saved=base+"/"+upload
            data=b"fixed-version Orange data"*5000
            expected=sha256(data).hexdigest()
            status,body=wire("POST",base,author,{"upload_id":upload,"size_bytes":str(len(data)),"media_type":"application/octet-stream","sha256":expected},upload)
            require(status==200,"Upload intent rejected: "+str((status,json.loads(body).get("code"))))
            result=json.loads(body);version=result["version_id"];artifact=result["artifact_id"]
            require(grant(author,str(uuid4()))[0]==409,"Staging object yielded a published capability")
            status,body=wire("PUT",saved+"/content",author,data,upload)
            require(status==200 and json.loads(body)["state"]=="available","Upload failed: "+str((status,json.loads(body).get("code"))))
            sqlrow=sql(f"""SELECT t.published_key,v.provider_version FROM artifacts a JOIN artifact_transfers t ON t.transfer_id=a.published_transfer_id
                           JOIN artifact_s3_versions v ON v.root_id=a.root_id AND v.storage_key=t.published_key WHERE a.artifact_id='{artifact}'""").split("|")
            require(len(sqlrow)==2 and sqlrow[1]!="null","Missing immutable provider receipt")
            key,provider=sqlrow
            # Independently revoke after the initial content read and before the final issuing transaction.
            racing=secrets.token_hex(32);racing_csrf=secrets.token_hex(32)
            sql(f"""INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
                SELECT '{sha256(racing.encode()).hexdigest()}','{uuid4()}','{author[0]}','{racing_csrf}',t,t+1200,t+3600
                FROM (SELECT floor(extract(epoch FROM now()))::bigint t) clk""")
            retired=(author[0],racing,racing_csrf)
            fence.path="/"+bucket+"/"+prefix+"/"+key
            with ThreadPoolExecutor(max_workers=1) as threads:
                pending=threads.submit(grant,retired,str(uuid4()))
                try:
                    require(fence.received.wait(12),"Real version HEAD did not reach the issuance boundary")
                    require(wire("DELETE","/api/v1/session",retired,key=str(uuid4()))[0]==204,"Concurrent logout failed")
                finally:fence.release.set()
                status,error=pending.result(timeout=20)
                require(status in (401,403) and "url" not in error,"Revoked in-flight request received a capability")
            fence.path=None
            # The second writer must fail before listening, without changing the root identity.
            contender=subprocess.Popen([api],env=settings,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
            try: _,diagnostic=contender.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                contender.terminate();contender.wait(timeout=10);raise RuntimeError("Two writers admitted to one root")
            require(contender.returncode!=0 and b"ARTIFACT_WRITER_BUSY" in diagnostic,"Second writer did not fail on its storage lease")
            req=str(uuid4());status,g=grant(author,req)
            require(status==200 and g["artifact_id"]==artifact and g["version_id"]==version and g["sha256"]==expected,"Authorization did not match owned object")
            get_signed(g)
            empty=str(uuid4());path=base+"/"+empty
            status,body=wire("POST",base,author,{"upload_id":empty,"size_bytes":"0","media_type":"text/plain","sha256":sha256(b"").hexdigest()},empty)
            require(status==200,"Empty artifact intent failed")
            ev=json.loads(body)["version_id"]
            status,body=wire("PUT",path+"/content",author,b"",empty)
            require(status==200,"Empty object publication failed: "+str((status,json.loads(body).get("code"))))
            status,body=wire("GET",path+"/content?"+urlencode({"version_id":ev}),author)
            require(status==200 and body==b"","Empty immutable provider version failed")
            status,body=wire("POST",path+"/download-authorizations",author,{"version_id":ev},req)
            require(status==409 and "url" not in json.loads(body),"Download key was rebound to a different object")
            status,body=wire("PUT",saved+"/content",author,data,upload)
            require(status==200,"Identical upload replay rejected")
            require(grant(other,str(uuid4()))[0] in (403,404),"Foreign owner acquired download authority")
            newer=folder/"external-newer-version";newer.write_bytes(b"different version under the same storage key")
            require(s3("PUT",bucket+"/"+prefix+"/"+key,newer)==200,"Fixture could not replace provider latest")
            get_signed(g)
            query="?"+urlencode({"version_id":version})
            status,body=wire("GET",saved+"/content"+query,author)
            require(status==200 and body==data,"Proxy read latest instead of published version")
            failed_upload=str(uuid4());failed_path=base+"/"+failed_upload
            status,body=wire("POST",base,author,{"upload_id":failed_upload,"size_bytes":str(len(data)),"media_type":"application/octet-stream","sha256":expected},failed_upload)
            require(status==200,"Fault fixture upload intent failed")
            failed_id=json.loads(body)["artifact_id"];failed_version=json.loads(body)["version_id"]
            sql(f"""CREATE FUNCTION injected_provider_receipt_loss() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
                 IF EXISTS(SELECT FROM artifact_transfers WHERE artifact_id='{failed_id}' AND published_key=NEW.storage_key) THEN
                     RAISE EXCEPTION 'isolated fault after fixed provider copy'; END IF;
                 RETURN NEW; END $$;
                 CREATE TRIGGER injected_provider_receipt BEFORE INSERT ON artifact_s3_versions FOR EACH ROW EXECUTE FUNCTION injected_provider_receipt_loss();""")
            status,_=wire("PUT",failed_path+"/content",author,data,failed_upload)
            require(status>=400,"Failed provider receipt was acknowledged")
            state=sql(f"SELECT a.state,t.state,t.published_key FROM artifacts a JOIN artifact_transfers t USING(artifact_id) WHERE a.artifact_id='{failed_id}'").split("|")
            require(state[:2]==["staging","orphan"],"Private failed transfer was exposed")
            orphan=state[2]
            pos,meta,orphan_body=provider_call("GET",prefix+"/"+orphan)
            orphan_version=meta.get("x-amz-version-id")
            require(pos==200 and orphan_version and orphan_body==data,"Fault did not follow the real provider copy")
            status,body=wire("POST",failed_path+"/download-authorizations",author,{"version_id":failed_version},str(uuid4()))
            require(status==409 and "url" not in json.loads(body),"Uncommitted copy was signed")
            # The unrelated fixture key shares a prefix, but never a transfer identity.
            require(s3("PUT",bucket+"/"+prefix+"/"+orphan+"-unrelated",newer)==200,"Cannot establish adjacent key")
            sql(f"""DROP TRIGGER injected_provider_receipt ON artifact_s3_versions;
                    UPDATE artifact_transfers SET created_at=clock_timestamp()-interval '3 hours',expires_at=clock_timestamp()-interval '2 hours'
                    WHERE artifact_id='{failed_id}' AND state='orphan';""")
            if direct_upload:
                from verify_direct_ingress import prepare
                direct=prepare(wire,sql,author,other)
        # A separate process must reopen the root and resolve the original PostgreSQL version pin.
        with listening(api,settings,"User API") as address:
            if direct_upload:
                from verify_direct_ingress import recover
                direct_stage=recover(direct,wire,sql,author,provider_call,prefix)
            deadline=time.monotonic()+18
            while sql(f"SELECT state FROM artifact_transfers WHERE published_key='{orphan}'")!="purged" and time.monotonic()<deadline:
                time.sleep(.15)
            require(sql(f"SELECT state FROM artifact_transfers WHERE published_key='{orphan}'")=="purged","Unbound copy was not reclaimed")
            require(provider_call("GET",prefix+"/"+orphan,{"versionId":orphan_version})[0]==404,"Orphan provider version still exists")
            require(provider_call("GET",prefix+"/"+orphan+"-unrelated")[0]==200,"Cleanup removed an unrelated key")
            status,body=wire("PUT",failed_path+"/content",author,data,failed_upload)
            require(status==200 and json.loads(body)["version_id"]==failed_version,"Original upload could not recover")
            require(sql(f"SELECT count(*) FROM artifact_transfers WHERE artifact_id='{failed_id}' AND state='attached'")=="1","Duplicate attachment after recovery")
            status,again=grant(author,req)
            require(status==200 and again["expires_at_unix"]==g["expires_at_unix"],"Replay extended the persisted deadline")
            get_signed(again)
            status,wrong=wire("POST",saved+"/download-authorizations",author,{"version_id":str(uuid4())},str(uuid4()))
            require(status==409 and "url" not in json.loads(wrong),"Wrong logical version admitted")
            require(wire("DELETE",saved,author,key=upload)[0]==200,"Deletion failed")
            status,denied=grant(author,req);require(status in (403,409,410) and "url" not in denied,"Deleted object issued a new URL")
            # Previously issued capability may still read during its original TTL.
            get_signed(g)
            n=sql(f"SELECT (SELECT count(*) FROM artifact_download_grants WHERE owner_id='{author[0]}'),(SELECT count(*) FROM jobs),(SELECT count(*) FROM ledger_transactions)")
            require(n=="1|0|0","Repeated or denied reads changed grant/accounting state")
            if direct_upload:
                from verify_direct_ingress import deletion_during_copy
                deletion_during_copy(wire,sql,author,fence,prefix,bucket)
            if direct_expiry:
                from verify_direct_ingress import expire_and_reclaim
                expire_and_reclaim(direct,wire,sql,provider_call,prefix,author)
            if web:
                # Configure the actual test bucket's CORS for this HTTPS application origin.
                cors=f'<CORSConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><CORSRule><AllowedOrigin>{origin}</AllowedOrigin><AllowedMethod>GET</AllowedMethod><AllowedMethod>PUT</AllowedMethod><AllowedHeader>*</AllowedHeader><ExposeHeader>Content-Length</ExposeHeader></CORSRule></CORSConfiguration>'.encode()
                status,_,_=provider_call("PUT",query={"cors":""},data=cors,headers={"Content-Type":"application/xml"})
                require(status in (200,204),"Private provider CORS registration failed: "+str(status))
                pixels,rgb=(png.read_bytes(),rgb) if png else runpy.run_path(str(Path(__file__).with_name("check-authorized-image-inspection.py")))["sample"]()
                picture=str(uuid4());image_sha=sha256(pixels).hexdigest()
                status,raw=wire("POST",base,author,{"upload_id":picture,"sha256":image_sha,"size_bytes":str(len(pixels)),"media_type":"image/png"},picture)
                require(status==200,"Browser upload intent failed");image=json.loads(raw)
                require(wire("PUT",base+"/"+picture+"/content",author,pixels,picture)[0]==200,"Browser upload failed")
                web=web.resolve(strict=True)
                browser_file=folder/"real-browser-input.png";browser_file.write_bytes(pixels)
                case={"directUpload":direct_upload,"pngFile":str(browser_file),"api":address,"origin":origin,"storage":endpoint,"site":str((site or web/"apps/web/dist").resolve(strict=True)),
                      "tls":str(web/".local/tls"),"token":author[1],"csrf":author[2],"owner":author[0],"upload":picture,
                      "artifact":image["artifact_id"],"rgb":rgb,"sha256":image_sha,"bytes":len(pixels)}
                subprocess.run([Path.home()/".nvm/versions/node/v22.23.2/bin/node",web/"apps/web/scripts/real-versioned-download.mjs"],
                               input=json.dumps(case),text=True,check=True,timeout=110)
                require(sql("SELECT (SELECT count(*) FROM jobs),(SELECT count(*) FROM ledger_transactions)")=="0|0","Browser transfer changed financial facts")
            if expiry:
                extra_key=str(uuid4());empty_route=path+"/download-authorizations"
                status,raw=wire("POST",empty_route,author,{"version_id":ev},extra_key);empty_grant=json.loads(raw)
                require(status==200,"Timed empty grant failed")
                end=int(empty_grant["expires_at_unix"]);time.sleep(max(0,end+2-time.time()))
                # The platform must not renew the original request after expiry even for a live, readable version.
                status,raw=wire("POST",empty_route,author,{"version_id":ev},extra_key)
                require(status==408 and "url" not in json.loads(raw),"Expired idempotent read extended its deadline")
                signed=urlsplit(empty_grant["url"]);conn=HTTPConnection("127.0.0.1",signed.port,timeout=12)
                try:
                    conn.request("GET",signed.path+"?"+signed.query);reply=conn.getresponse();reply.read()
                    require(reply.status==403,"Provider admitted an expired API-issued capability")
                finally:conn.close()
                print("PASS actual provider and API replay expiration at the stored deadline",flush=True)
            marker=folder/"new-root-marker";marker.write_bytes(str(uuid4()).encode())
            require(s3("PUT",bucket+"/"+prefix+"/.orange-artifact-root",marker)==200,"Root drift fixture failed")
            status,body=wire("POST",path+"/download-authorizations",author,{"version_id":ev},str(uuid4()))
            require(status>=400 and "url" not in json.loads(body),"Storage root replacement yielded a download URL")
            print(json.dumps({"revoked_during_provider_read":True,"orphan_version_removed":True,"adjacent_key_preserved":True,"original_upload_recovered":True,"exclusive_writer":True,"empty_version":True,"root_replacement_denied":True,"provider_pinned":True,"proxy_latest_replacement_ignored":True,"signed_latest_replacement_ignored":True,
                              "process_restart_replay":True,"foreign_issue":False,"deleted_issue":False,"grant_rows_before_expiry_case":1,"jobs":0,"ledger_transactions":0}),flush=True)
