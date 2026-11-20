#!/usr/bin/env python3
"""Check current development distributions and reject substitution of their archive or receipt."""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--receipt",type=Path,action="append",default=[],
                    help="Audit a particular development build instead of the three current-source packages")
args=parser.parse_args()
if sys.version_info[:2]!=(3,12) or not __debug__:
    raise SystemExit("Python 3.12 with assertions required")
head=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
receipts=args.receipt
if not receipts:
    for profile in ("embedding","transcription","image"):
        found=[p for p in (ROOT/".local"/(profile+"-distributions")).glob("*.tar.json")
               if (lambda data:data["source"]==data["packaging_source"]==head)(json.loads(p.read_bytes()))]
        if len(found)!=1:raise SystemExit("A unique qualified "+profile+" release is required at "+head)
        receipts.append(found[0])
catalog=json.loads((ROOT/"distribution/notices/sources.json").read_bytes())
audit=[sys.executable,ROOT/"scripts/audit-development-distribution.py"]
for receipt in receipts:
    meta=json.loads(receipt.read_bytes())
    output=ROOT/".local/distribution-inventories"/(meta["sha256"]+".json")
    subprocess.run(audit+["--receipt",receipt,"--output",output],check=True)
    data=json.loads(output.read_bytes())
    assert len(data["wheels"])==meta["offline_wheels"] and data["native_programs"] and data["cargo"]
    applies={(x["package"],x["version"]):x for x in catalog}
    checked=0
    for wheel in data["wheels"]:
        source=applies.get((wheel["name"].lower(),wheel["version"]))
        if source:
            assert source in wheel.get("supplemental_notices",[]), (wheel["name"],"notice omitted")
            checked+=1
    with tempfile.TemporaryDirectory(prefix="orange-inventory-negative-") as temp:
        temp=Path(temp)
        source=receipt.parent/meta["archive"];target=temp/meta["archive"]
        # Write an unrelated small byte string under the expected filename. The trusted package is untouched.
        target.write_bytes(b"not a released archive")
        bad=temp/receipt.name
        bad.write_text(json.dumps(dict(meta,bytes=target.stat().st_size)))
        dest=temp/"invalid.json"
        result=subprocess.run(audit+["--receipt",bad,"--output",dest],text=True,capture_output=True)
        assert result.returncode!=0 and "Archive hash mismatch" in result.stderr and not dest.exists()
        bad.write_text(json.dumps(dict(meta,archive="../"+source.name)))
        result=subprocess.run(audit+["--receipt",bad,"--output",dest],text=True,capture_output=True)
        assert result.returncode!=0 and "Invalid content-addressed archive name" in result.stderr and not dest.exists()
    print("Verified",meta.get("profile","embedding"),"inventory and",checked,"exact upstream notice supplements",flush=True)
