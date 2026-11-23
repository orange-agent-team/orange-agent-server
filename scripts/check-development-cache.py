#!/usr/bin/env python3
"""Exercise authentication, key ACLs, retry, bounded shutdown and recreation of a real Redis fixture."""
import json
from pathlib import Path
import secrets
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from local_cache import exchange,load,record,require

TOOL=Path(__file__).with_name("local_cache.py")


def run(action,instance):
    return json.loads(subprocess.check_output([sys.executable,TOOL,action,"--instance",instance],text=True))


def state(result):
    source=Path(result["credential_file"])
    private=load(source)
    require(private["password"] not in json.dumps(result),"Raw credential disclosed")
    return private


def command(conn,*parts):
    auth=("AUTH","orange_cache",conn["password"])
    authorized,result=exchange(conn["port"],[auth,parts])
    require(authorized=="OK","Authentication failed")
    return result


def main():
    a="check-"+secrets.token_hex(6)
    b="check-"+secrets.token_hex(6)
    try:
        with ThreadPoolExecutor(max_workers=2) as pool:
            first,repeat=list(pool.map(lambda _:run("start",a),range(2)))
        require(first==repeat and first==run("status",a),"Retry changed cache identity")
        one=state(first)
        # A caller may die after Docker assigns a port and before writing the final state.
        pending=dict(one)
        del pending["port"]
        record(Path(first["credential_file"]),pending)
        recovered=run("status",a)
        require(recovered==first and state(recovered)==one,"Unfinished startup did not recover its original endpoint")
        two=state(run("start",b))
        active=one["prefix"]+"ready"
        sibling=two["prefix"]+"retained"
        require(exchange(one["port"],[("PING",)])==[("error","NOAUTH")],"Unauthenticated client admitted")
        require(command(one,"SET",active,"alpha","EX","120")=="OK" and command(one,"GET",active)==b"alpha","Read/write failed")
        require(0<command(one,"TTL",active)<=120,"TTL not enforced")
        require(command(one,"SET",sibling,"unexpected")==("error","NOPERM"),"Cross-namespace write admitted")
        require(command(one,"FLUSHDB")==("error","NOPERM"),"Destructive cache command admitted")
        require(command(two,"SET",sibling,"bravo")=="OK","Sibling seed failed")
        run("stop",a)
        run("stop",a)
        fresh=state(run("start",a))
        require(one["id"]!=fresh["id"] and fresh["prefix"]!=one["prefix"],"Restart reused namespace")
        require(exchange(fresh["port"],[("AUTH","orange_cache",one["password"])])==[("error","WRONGPASS")],"Old credential restored")
        require(command(fresh,"GET",fresh["prefix"]+"ready") is None and command(two,"GET",sibling)==b"bravo","Restart changed sibling state or recovered prior keys")
        print(json.dumps({"authenticated":True,"same_instance_on_concurrent_start":True,"interrupted_start_recovered":True,"foreign_write_denied":True,
                          "global_flush_denied":True,"expired_credentials_denied":True,"separate_namespace_after_restart":True,
                          "sibling_unchanged":True,"scope":"Two actual ephemeral local Linux ARM64 Redis processes and a clean replacement"}))
    finally:
        try:
            run("stop",a)
        finally:
            run("stop",b)


if __name__=="__main__":
    main()
