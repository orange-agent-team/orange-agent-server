#!/usr/bin/env python3
"""Exercise the deletion admission boundary and existing source revocation with real database/object I/O."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--environment", type=Path, default=ROOT / ".local")
private = parser.parse_args().environment.resolve()
rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo = Path(subprocess.check_output([rustup, "which", "cargo"], cwd=ROOT, text=True).strip())
env = dict(os.environ, PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"], CARGO_NET_OFFLINE="true",
           ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"))
subprocess.run([sys.executable,ROOT/"scripts/check-artifact-contract.py"],cwd=ROOT,check=True)
with database(private/"database-url","orange_active_source_") as (_,url):
    env["ORANGE_TEST_DATABASE_URL"] = url
    result = subprocess.run([cargo,"test","--locked","--offline","-p","orange-control-plane","--features",
                             "integration-tests","--lib","artifacts::tests::","--","--test-threads=2"],
                            cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
    print(result.stdout,flush=True)
    result.check_returncode()
    for name in ["user_deletion_preserves_active_input_until_reference_release",
                 "real_http_enforces_cookie_csrf_streaming_integrity_and_revocation",
                 "active_reference_survives_deletion_retention_and_cannot_be_added_after_delete"]:
        if "artifacts::tests::"+name+" ... ok" not in result.stdout:
            raise RuntimeError("Missing original or new deletion qualification")
    scoped=dict(env,ORANGE_CANCEL_PROTOCOL_TESTS="1")
    subprocess.run([sys.executable,ROOT/"scripts/check-worker-session.py"],cwd=ROOT,env=scoped,check=True)
print("PASS user deletion refuses active references and retains immediate authorized revocation")
