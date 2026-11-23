#!/usr/bin/env python3
"""Local review gates. Contribution code runs only in the offline Linux job."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT=Path(__file__).resolve().parents[1]
JOBS={
    "quick":{"trust":"isolated contribution","requires":["locked rustc, rustfmt, Clippy, protoc"],"checks":["domain-format-lint-tests-wire"]},
    "clients":{"trust":"trusted fixed checkouts","requires":["desktop","web","admin","local development services"],
               "checks":["regenerate-bridge-wire","clean-rust-flutter-web-admin"]},
    "stateful":{"trust":"trusted fixed checkout","requires":["local PostgreSQL, OIDC and disposable Worker credentials"],
                "checks":["empty-cluster-and-issuer","simulated-worker-roundtrip","reset-with-sibling"]},
    "faults":{"trust":"isolated contribution","requires":["locked Linux quick-check inputs"],"checks":["positive-and-negative-verdicts"]},
}


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--job",choices=["matrix",*JOBS],required=True)
    cli.add_argument("--candidate",help="Local hashed contribution; applies only to the isolated jobs")
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    for kind in ("desktop","web","admin"):
        cli.add_argument("--"+kind,type=Path)
    args=cli.parse_args()
    if args.job=="matrix":
        print(json.dumps(JOBS,indent=2))
        return
    private=args.environment.resolve()
    steps=[]
    def add(key,script,*options):
        steps.append((key,[sys.executable,str(ROOT/"scripts"/script),*map(str,options)]))
    if args.job in ("quick","faults"):
        options=["--inputs",private/"linux"]
        if args.candidate:
            options+=["--candidate",args.candidate]
        if args.job=="faults":
            options+=["--faults"]
        add(JOBS[args.job]["checks"][0],"check-isolated-ci.py",*options)
    else:
        if args.candidate:
            raise ValueError("Only the isolated job accepts contributor source")
        if args.job=="clients":
            if not args.desktop or not args.web or not args.admin:
                raise ValueError("Provide the three explicit, trusted client checkouts")
            add("regenerate-bridge-wire","qualify-toolchain.py","--section","upgrade","--desktop",args.desktop.resolve())
            add("clean-rust-flutter-web-admin","qualify-toolchain.py","--section","reproduce","--asset-cache",private,
                "--desktop",args.desktop.resolve(),"--web",args.web.resolve(),"--admin",args.admin.resolve())
        else:
            add("empty-cluster-and-issuer","check-development-cold-start.py")
            add("simulated-worker-roundtrip","check-development-lifecycle.py","--section","worker","--environment",private)
            add("reset-with-sibling","check-development-lifecycle.py","--section","restart","--environment",private)
    if [key for key,_ in steps]!=JOBS[args.job]["checks"]:
        raise RuntimeError("Selected CI plan differs from the required job")
    env={k:v for k,v in os.environ.items() if k in {"PATH","HOME","USER","LOGNAME","LANG","LC_ALL","TMPDIR",
                                                    "CARGO_TARGET_DIR","CARGO_BUILD_JOBS","DEVELOPER_DIR","SDKROOT"}}
    env.update(CI="true",CARGO_NET_OFFLINE="true",CARGO_INCREMENTAL="0",CARGO_PROFILE_DEV_DEBUG="0",
               FLUTTER_SUPPRESS_ANALYTICS="true",DASH__SUPPRESS_ANALYTICS="true")
    source=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,env=env,text=True).strip()
    completed=[]
    for key,command in steps:
        start=time.monotonic()
        print("RUN",key,flush=True)
        subprocess.run(command,cwd=ROOT,env=env,check=True)
        completed.append({"check":key,"seconds":round(time.monotonic()-start,3)})
    print(json.dumps({"job":args.job,"trusted_runner":source,"candidate":args.candidate or source,"completed":completed,"passed":True}))


if __name__=="__main__":
    main()
