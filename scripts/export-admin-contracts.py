#!/usr/bin/env python3
"""Export the immutable producer's paired OpenAPI and JSON Schema into an Admin checkout."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

from admin_contract_consistency import check_contract

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--admin-repo", required=True, type=Path)
args = parser.parse_args()
target = args.admin_repo.resolve() / "docs/contracts"
if not target.is_dir():
    raise SystemExit("Existing Admin contract directory required")
commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
pairs = {"admin.openapi.json": "contracts/openapi/admin.v1.json",
         "admin.schema.json": "contracts/json-schema/admin.schema.json"}
payloads = {name: subprocess.check_output(["git", "show", f"{commit}:{source}"], cwd=ROOT)
            for name, source in pairs.items()}
api = json.loads(payloads["admin.openapi.json"])
check_contract(api, json.loads(payloads["admin.schema.json"]))
snapshot = {"openapi_version": api["info"]["version"], "source_repository": "orange-agent-server",
            "source_commit": commit, "files": {name: {"source": path, "sha256": hashlib.sha256(payloads[name]).hexdigest()}
                                               for name, path in pairs.items()}}
for name, raw in payloads.items():
    (target / name).write_bytes(raw)
(target / "snapshot.json").write_text(json.dumps(snapshot, indent=2) + "\n")
print("Exported fixed Admin contracts from", commit)
