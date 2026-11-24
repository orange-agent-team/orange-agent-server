#!/usr/bin/env python3.12
"""Exercise the normal first-run entry point and the real environment name collision checks."""
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from environment_topology import topology

ROOT=Path(__file__).resolve().parents[1]


def require(value,message):
    if not value:raise ValueError(message)


def main():
    templates=json.loads((ROOT/"config/environment-names.json").read_bytes())
    regions=json.loads((ROOT/"config/service-regions.json").read_bytes())
    entry=ROOT/"scripts/developer-first-run.py"
    def invoke(*args,environment=None):
        return subprocess.run([sys.executable,entry,*args],cwd=ROOT,env=environment,capture_output=True,text=True,timeout=15)
    legacy=invoke("map")
    require(legacy.returncode==0 and json.loads(legacy.stdout)==templates,"Raw environment map compatibility changed")
    first=invoke("topology","--run-id","e4b66781-2334-4894-82f7-c9784734f820")
    require(first.returncode==0,"Namespaced read-only topology failed")
    result=json.loads(first.stdout)
    require(result["default_rehearsal"]=="test" and set(regions["regions"])=={"us","jp","cn","hk"},"Incorrect confirmed regions or rehearsal default")
    rows=result["external_name_reservations"]
    require(len(rows)==8 and all(r["domain"].endswith(".invalid") and r["provision"]=="external" for r in rows),"Missing external name reservations")
    require(len({(r['environment'],r['region']) for r in rows})==8,"Repeated region environment")
    second=topology(templates,regions,"other-run")
    require(result["local"]["development"]==second["local"]["development"] and rows==second["external_name_reservations"],"Changed unrelated names")
    for field in ("object_prefix","account_scope","identity_container"):
        require(result["local"]["test"][field]!=second["local"]["test"][field],"Two test runs shared "+field)
    faults=[
        ("production","domain",templates["staging"]["domain"]),
        ("staging","database",templates["production"]["database"]),
        ("production","database_role","orange_local"),
        ("test","identity_container",templates["development"]["identity_container"]),
        ("production","identity",templates["staging"]["identity"]),
        ("staging","account_scope",templates["production"]["account_scope"]),
        ("staging","object_prefix","production/{region}/temporary/"),
        ("production","object_prefix","production/../dev/"),
        ("staging","domain","127.0.0.1"),
        ("production","domain","api.{region}.example.com"),
        ("test","domain","api.prod.us.orange.invalid"),
        ("development","database_role","orange_prod_us_service"),
        ("production","account_scope","external-shared"),
        ("staging","object_prefix","test/{run}/"),
        ("development","object_prefix","/dev/local/"),
        ("production","identity_container","external-identity"),
    ]
    for environment,field,value in faults:
        altered=copy.deepcopy(templates);altered[environment][field]=value
        try:topology(altered,regions,"audit")
        except ValueError:continue
        raise ValueError("Accepted overlapping or unsafe "+environment+"."+field)
    for bad in ("", "A/B", "../prod", "two--runs", "{region}", "abc\n", "x"*64):
        denied=invoke("topology","--run-id",bad)
        require(denied.returncode!=0 and not denied.stdout,"Invalid test run produced a topology")
    with tempfile.TemporaryDirectory() as private:
        secret="disposable-override-value"
        env=dict(os.environ,TMPDIR=private,ORANGE_ENV="production",ORANGE_USER_CONFIG_FILE=str(Path(private)/"absent"),
                 DATABASE_URL="postgresql://"+secret+"@production.invalid/orange_prod",PATH=private)
        for stage in ("production","staging"):
            for action in ("doctor","verify"):
                blocked=invoke(action,"--environment",stage,environment=env)
                require(blocked.returncode==2 and secret not in blocked.stderr+blocked.stdout and not list(Path(private).iterdir()),
                        "External rehearsal or tooling ran")
        default=invoke("map",environment=env)
        require(default.returncode==0 and json.loads(default.stdout)==templates and not list(Path(private).iterdir()),"Ambient production settings altered the development map")
    print(json.dumps({"external_reservations":len(rows),"regions":regions["regions"],"test_runs_compared":2,
                      "rejected_namespace_mutations":len(faults),"external_doctor_and_rehearsal_refusals":4,
                      "real_provider_accounts_provisioned":0,"real_external_domains_checked":0}))
    print("PASS original OPS-001.01 logical environment boundaries, two isolated test scopes, four scoped regions and local-only rehearsal selection")


if __name__=="__main__":main()
