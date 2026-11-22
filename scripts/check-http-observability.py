#!/usr/bin/env python3
"""Build and verify all three control-plane HTTP executables."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
from http_metrics_smoke import check_programs, ROOT, ROLES


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--private-dir",type=Path,default=ROOT/".local")
    args=cli.parse_args()
    private=args.private_dir.resolve()
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    env=dict(os.environ,CARGO_NET_OFFLINE="true",PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
    targets=[option for _,name,_,_ in ROLES for option in ("--bin",name)]
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-control-plane",*targets],cwd=ROOT,env=env,check=True)
    meta=json.loads(subprocess.check_output([cargo,"metadata","--locked","--offline","--no-deps","--format-version","1"],cwd=ROOT,env=env))
    output=Path(meta["target_directory"])/"debug"
    check_programs(output, private/"database-url", private/"identity/user-config.json",
                   private/"identity/admin-config.json", ROLES)


if __name__=="__main__":
    main()
