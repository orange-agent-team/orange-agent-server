#!/usr/bin/env python3
"""Export the complete user-Web contract set from a single local server commit."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--web-repo", required=True, type=Path)
args = parser.parse_args()
target = args.web_repo.resolve() / "docs/contracts"
if not target.is_dir():
    raise SystemExit("Existing user Web contract directory required")
commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
entries = {}
payloads = {}
for family in ("user", "account", "artifacts", "pricing", "cloud-agent", "pg-connector", "s3-connector"):
    for source, name in [
        (f"contracts/json-schema/{family}.schema.json", f"{family}.schema.json"),
        (f"contracts/openapi/{family}.v1.json", f"{family}.openapi.json"),
    ]:
        raw = subprocess.check_output(["git", "show", f"{commit}:{source}"], cwd=root)
        json.loads(raw)
        entries[name] = {"source": source, "sha256": hashlib.sha256(raw).hexdigest()}
        payloads[name] = raw
snapshot = {"version": "0.11.0", "source_repository": "orange-agent-server", "source_commit": commit, "files": entries}
for name, raw in payloads.items():
    (target / name).write_bytes(raw)
(target / "snapshot.json").write_text(json.dumps(snapshot, indent=2) + "\n")
print("Exported", len(entries), "pinned user contract files from", commit)
