"""Deployment qualification for a disposable versioned S3 bucket on the pinned local backend."""
import base64
import hashlib
import json
from pathlib import Path
import subprocess
from urllib.parse import quote,urlencode,urlsplit
from xml.etree import ElementTree


def require(ok, detail):
    if not ok:
        raise RuntimeError(detail)


def digest(data):
    return base64.b64encode(hashlib.sha256(data).digest()).decode('ascii')


def private_requests(endpoint,bucket,credentials,folder):
    address=urlsplit(endpoint)
    require(address.scheme=="http" and address.hostname=="127.0.0.1" and address.port and not address.path,
            "Expected a private local test service")
    folder=Path(folder)
    def request(method,key="",query=None,data=None,headers=None,authenticated=True):
        # Only the isolated fixture can receive these transient credentials. The CLI receives
        # them via stdin, never as a command-line argument or a printed signed URL.
        path="/"+quote(bucket,safe="")+("/"+quote(key,safe="/") if key else "")
        url=endpoint+path+("?"+urlencode(query) if query else "")
        path_headers=folder/"version-response-headers";path_body=folder/"version-response-body"
        argv=["curl","--noproxy","*","--silent","--max-time","15","--connect-timeout","2","--request",method,
              "--dump-header",path_headers,"--output",path_body,"--write-out","%{http_code}"]
        conf=b""
        if authenticated:
            argv+=["--aws-sigv4","aws:amz:us-east-1:s3","--config","-"]
            conf=f'user = "{credentials["accessKey"]}:{credentials["secretKey"]}"\n'.encode()
        for name,value in (headers or {}).items():
            require(not any(c in name+value for c in "\r\n"),"Malformed test header")
            argv+=["--header",name+": "+value]
        if data is not None:
            payload=folder/"version-request-body"
            payload.write_bytes(data)
            argv+=["--data-binary","@"+str(payload)]
        result=subprocess.run([*argv,url],input=conf,capture_output=True,check=True,timeout=20)
        mapped={}
        for line in path_headers.read_text().splitlines():
            if ":" in line:
                name,value=line.split(":",1)
                mapped[name.lower()]=value.strip()
        return int(result.stdout),mapped,path_body.read_bytes()

    return request


def check(endpoint,bucket,credentials,folder):
    request=private_requests(endpoint,bucket,credentials,folder)
    xml=b'<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>'
    status,_,_=request("PUT",query={"versioning":""},data=xml,headers={"Content-Type":"application/xml"})
    require(status in (200,204),"Bucket versioning configuration failed: "+str(status))
    status,_,body=request("GET",query={"versioning":""})
    require(status==200 and any(e.tag.split("}")[-1]=="Status" and e.text=="Enabled" for e in ElementTree.fromstring(body).iter()),
            "Storage did not confirm bucket versioning")
    staging="staging/probe"
    published="published/probe"
    first=b"first immutable Orange artifact\n";second=b"later staging bytes\n"
    def put(key,content):
        status,h,_=request("PUT",key,data=content,headers={"Content-Type":"application/octet-stream","x-amz-checksum-sha256":digest(content)})
        version=h.get("x-amz-version-id")
        require(status==200 and version and version!="null" and len(version)<=1024,"No concrete version receipt for "+key+": "+str(status))
        return version
    original=put(staging,first)
    replacement=put(staging,second)
    require(original!=replacement,"Two writes returned the same source version")
    def pinned_read(key,version,expected):
        status,h,raw=request("GET",key,{"versionId":version})
        require(status==200 and h.get("x-amz-version-id")==version and raw==expected,"Incorrect pinned source bytes")
    pinned_read(staging,original,first)
    pinned_read(staging,replacement,second)
    require(request("GET",staging)[2]==second,"Fixture did not actually replace latest")
    src="/"+bucket+"/"+quote(staging,safe="/")+"?versionId="+quote(original,safe="")
    status,headers,reply=request("PUT",published,headers={"x-amz-copy-source":src})
    dest=headers.get("x-amz-version-id")
    require(status==200 and dest and dest!="null" and ElementTree.fromstring(reply).tag.split("}")[-1]=="CopyObjectResult",
            "Version-bound copy did not return a destination version")
    pinned_read(published,dest,first)
    require(request("GET",published,{"versionId":dest},authenticated=False)[0] in (401,403),"Anonymous published object access")
    for version in [replacement,original]:
        require(request("DELETE",staging,{"versionId":version})[0] in (200,204),"Cannot retire exact staging version")
    pinned_read(published,dest,first)
    require(request("GET",staging,{"versionId":original})[0]==404,"Deleted source version remained readable")
    wrong,_,error=request("PUT","staging/wrong-checksum",data=second,headers={"x-amz-checksum-sha256":digest(first)})
    code=None
    if error:
        try:code=next((e.text for e in ElementTree.fromstring(error).iter() if e.tag.split("}")[-1]=="Code"),None)
        except ElementTree.ParseError:pass
    print(json.dumps({"private_fixture":"SeaweedFS","versioning":True,"two_staging_versions":True,"published_from_pinned_source":True,
                      "destination_still_readable_after_source_deletion":True,"anonymous_read_denied":True,"mismatched_checksum_status":wrong,
                      "mismatched_checksum_error":code,"fixed_sha256":hashlib.sha256(first).hexdigest()}),flush=True)
    require(wrong==400 and code in {"BadDigest","InvalidDigest","XAmzContentChecksumMismatch"},
            "Storage did not reject mismatched explicit SHA-256; direct artifact admission remains unqualified")
    print("PASS real S3 version-bound copy, exact cleanup, private reads and checksum rejection",flush=True)
    return dest
