#!/usr/bin/env python3
"""Readiness of the locally managed PostgreSQL cluster and the two OIDC application surfaces."""
import argparse
import fcntl
import http.client
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import time
from urllib.parse import unquote, urlsplit

ROOT=Path(__file__).resolve().parents[1]


class ConfigurationError(Exception):
    pass


class Unavailable(Exception):
    pass


def check(condition, reason):
    if not condition:
        raise ConfigurationError(reason)


def read_file(path, private=True):
    with os.fdopen(os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK),"rb") as stream:
        meta=os.fstat(stream.fileno())
        check(stat.S_ISREG(meta.st_mode) and meta.st_uid==os.getuid() and meta.st_size<=65536,
              "Invalid local configuration file: "+path.name)
        check(not private or meta.st_mode&0o077==0,"Private permissions required: "+path.name)
        result=stream.read(65537)
        check(len(result)<=65536,"Oversized configuration: "+path.name)
        return result


def config(directory):
    directory=directory.resolve(strict=True)
    for name in ("postgres","identity"):
        parent=directory/name
        check(parent.resolve(strict=True)==parent,"Linked development configuration directory")
    owned=json.loads(read_file(directory/"postgres/identity.json",private=False))
    data=directory/"postgres/data"
    check(owned["purpose"]=="orange-development-only" and owned["production"] is False and
          Path(owned["data_directory"])==data and data.resolve(strict=True)==data,"Wrong PostgreSQL cluster location")
    uri=urlsplit(read_file(directory/"database-url").decode().strip())
    check(uri.scheme=="postgresql" and uri.hostname=="127.0.0.1" and uri.port==owned["port"] and not uri.query and not uri.fragment
          and uri.username=="orange_local" and uri.path=="/orange_dev" and bool(uri.password),"Unexpected development database connection")
    configs=[json.loads(read_file(directory/("identity/"+role+"-config.json"))) for role in ("user","admin")]
    issuers=[]
    origins=[]
    for role,c in zip(("user","admin"),configs):
        issuer=urlsplit(c["issuer"])
        origin=urlsplit(c[role+"_origin"])
        check(c["environment"]=="dev" and issuer.scheme=="http" and issuer.hostname=="127.0.0.1" and issuer.port
              and issuer.path=="/realms/orange-dev" and issuer.username is None and not issuer.query and not issuer.fragment,
              "Unexpected development OIDC issuer")
        check(origin.scheme=="https" and origin.hostname=="127.0.0.1" and origin.port and not origin.username
              and origin.path in ("","/") and not origin.query and not origin.fragment,"Unexpected application origin")
        check(bool(c["client_id"]) and bool(c["client_secret"]),"Missing OIDC client configuration")
        issuers.append(issuer)
        origins.append(origin)
    check(issuers[0]==issuers[1] and origins[0]!=origins[1] and configs[0]["client_id"]!=configs[1]["client_id"],
          "Separate application origins and client identities are required")
    return data,uri,issuers[0]


def postgres(data,uri):
    executable=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
    env={k:v for k,v in os.environ.items() if not k.startswith("PG")}
    env.update(PGHOST=uri.hostname,PGPORT=str(uri.port),PGUSER=uri.username,PGPASSWORD=unquote(uri.password),
               PGDATABASE=uri.path[1:],PGCONNECT_TIMEOUT="2",PGOPTIONS="-c default_transaction_read_only=on -c statement_timeout=2000")
    sql="SELECT current_database(),current_user,current_setting('data_directory'),inet_server_addr(),inet_server_port()"
    result=subprocess.run([executable,"-X","-w","-At","-F","\t","-v","ON_ERROR_STOP=1","-c",sql],env=env,
                          capture_output=True,text=True,timeout=4)
    if result.returncode:
        raise Unavailable("PostgreSQL did not accept the read-only readiness query")
    check(result.stdout.strip().split("\t")==["orange_dev","orange_local",str(data),"127.0.0.1",str(uri.port)],
          "Connected server is not the expected managed cluster")


def document(issuer,path):
    conn=http.client.HTTPConnection(issuer.hostname,issuer.port,timeout=2)
    try:
        conn.request("GET",path,headers={"Accept":"application/json"})
        res=conn.getresponse()
        if res.status!=200:
            raise Unavailable("Development OIDC endpoint returned HTTP "+str(res.status))
        blob=res.read(262145)
        check(len(blob)<=262144,"Development OIDC document too large")
        return json.loads(blob)
    finally:
        conn.close()


def oidc(issuer):
    discovery=document(issuer,issuer.path+"/.well-known/openid-configuration")
    check(discovery["issuer"]==issuer.geturl(),"Discovery issuer mismatch")
    for key in ("authorization_endpoint","token_endpoint","jwks_uri"):
        target=urlsplit(discovery[key])
        check(target.scheme==issuer.scheme and target.netloc==issuer.netloc and target.path.startswith(issuer.path+"/")
              and not target.query and not target.fragment,"OIDC discovery references a different authority")
    keys=document(issuer,urlsplit(discovery["jwks_uri"]).path)["keys"]
    check(any(k.get("use")=="sig" and k.get("kid") and k.get("kty")=="RSA" and k.get("n") and k.get("e")
              for k in keys),"No advertised RSA signing key")
    check(all(not ({"d","p","q","dp","dq","qi","k"} & k.keys()) for k in keys),"OIDC published non-public key material")


def start_owned(directory):
    directory.mkdir(mode=0o700,exist_ok=True)
    check(directory.resolve(strict=True)==directory,"Provisioning requires an owned physical directory")
    descriptor=os.open(directory/".local-start.lock",os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW,0o600)
    with os.fdopen(descriptor,"rb+") as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        try:
            data,uri,issuer=config(directory)
            postgres(data,uri)
            oidc(issuer)
            return
        except (FileNotFoundError,ConnectionRefusedError,Unavailable):
            pass
        for command in (["local-postgres.py","start"],["local-user-identity.py"]):
            result=subprocess.run([sys.executable,ROOT/"scripts"/command[0],*command[1:]],cwd=ROOT,
                                  stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            if result.returncode:
                raise Unavailable("Development setup failed in "+command[0])


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    cli.add_argument("--start",action="store_true",help="Start the repository-owned development PostgreSQL and OIDC services")
    cli.add_argument("--wait-seconds",type=int,choices=range(0,61),metavar="0..60",default=0)
    args=cli.parse_args()
    if args.start:
        if args.environment.resolve()!=(ROOT/".local").resolve():
            print("Provisioning requires this checkout's own development directory",file=sys.stderr)
            return 2
        try:
            start_owned(ROOT/".local")
        except (OSError,ValueError,TypeError,KeyError,ConfigurationError,Unavailable,subprocess.TimeoutExpired,http.client.HTTPException) as err:
            print("Development setup unavailable: "+(str(err) if isinstance(err,(ConfigurationError,Unavailable)) else type(err).__name__),file=sys.stderr)
            return 1
    deadline=time.monotonic()+args.wait_seconds
    while True:
        try:
            data,uri,issuer=config(args.environment)
            postgres(data,uri)
            oidc(issuer)
        except (ConfigurationError,KeyError,TypeError,ValueError) as err:
            print("Invalid development configuration: "+(str(err) if isinstance(err,ConfigurationError) else type(err).__name__),file=sys.stderr)
            return 2
        except (OSError,Unavailable,subprocess.TimeoutExpired,http.client.HTTPException) as err:
            if time.monotonic()>=deadline:
                print("Development dependencies unavailable: "+(str(err) if isinstance(err,Unavailable) else type(err).__name__),file=sys.stderr)
                return 1
            time.sleep(min(.25,max(0,deadline-time.monotonic())))
            continue
        print(json.dumps({"ready":True,"database":"orange_dev","postgres_port":uri.port,"verified_cluster_directory":True,
                          "oidc_port":issuer.port,"public_signing_keys":True,"separate_client_configuration":True,
                          "scope":"Local dependency readiness; login, MFA and authorization have separate tests"}))
        return 0


if __name__=="__main__":
    sys.exit(main())
