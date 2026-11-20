"""Check a local platform bundle against its own manifest before starting its binaries."""
from pathlib import Path
import hashlib
import json
import os

root=Path(__file__).resolve().parent
manifest=json.loads((root/"manifest.json").read_bytes())
for name,expected in manifest["sha256"].items():
    path=root/name
    if not path.resolve().is_relative_to(root) or path.is_symlink() or not path.is_file():
        raise SystemExit("Invalid bundle member: "+name)
    with path.open("rb") as source:
        actual=hashlib.file_digest(source,"sha256").hexdigest()
    if actual!=expected:raise SystemExit("Digest mismatch: "+name)
    if name.startswith("bin/") and not os.access(path,os.X_OK):
        raise SystemExit("Native entry lacks execute permission: "+name)
print(json.dumps({"source":manifest["source"],"target":manifest["target"],"verified_files":len(manifest["sha256"])}))
