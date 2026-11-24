"""Generate and independently verify one image through the installed macOS release."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument("venv", type=Path)
parser.add_argument("model_cache", type=Path)
args = parser.parse_args()
template=json.loads(subprocess.check_output([root/"bin/orange-image","--describe-template"],timeout=15))
locked=json.loads((root/"config/local-image.lock.json").read_bytes())
if template["template"]!=locked["template"] or template["model_version"]!=locked["model_version"] or len(template["template_sha256"])!=64:
    raise ValueError("Installed image template differs from the archived lock")
with tempfile.TemporaryDirectory(prefix="orange-image-release-") as tmp:
    slot=Path(tmp)
    output=slot / "sample.png"
    request={"seed":7341,"prompt":"Studio photograph of one ripe orange on a white ceramic plate, soft window light and a pale green background."}
    result=subprocess.run([root/"bin/orange-image",args.venv.absolute()/"bin/python",args.model_cache.resolve(),slot,output],
                          input=json.dumps(request),text=True,capture_output=True,timeout=1830)
    if result.returncode:
        raise RuntimeError(result.stderr[-1200:])
    receipt=json.loads(result.stdout)
    if (receipt["model_version"]!="sdxl-base-1.0-euler-fp16-mps-v1" or receipt["seed"]!=request["seed"]
        or (receipt["width"],receipt["height"],receipt["steps"])!=(1024,1024,25)
        or receipt["sha256"]!=hashlib.sha256(output.read_bytes()).hexdigest()
        or list(slot.glob("image-attempt-*"))):
        raise ValueError("Installed image profile failed verification or cleanup")
    print(json.dumps({"template_sha256":template["template_sha256"],"manifest_sha256":template["manifest_sha256"],**{k:receipt[k] for k in ("model_version","width","height","steps","seed","sha256","rgb_sha256","bytes")}}))
