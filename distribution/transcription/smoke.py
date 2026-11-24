"""Exercise the installed CPU transcription executable against verified recorded speech."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument("venv", type=Path)
parser.add_argument("model_dir", type=Path)
args = parser.parse_args()
pin = json.loads((root / "config/local-transcription.lock.json").read_text())
contract=json.loads(subprocess.check_output([root/"bin/orange-transcribe","--describe-input"],timeout=10))
if contract["model_version"]!=pin["model_version"] or contract["language_codes"]!=pin["language_codes"]:
    raise ValueError("Installed transcription policy differs from its immutable release")
spec = pin["qualification_sample"]
cache = args.model_dir.resolve()
sample = cache / "jfk.wav"
if hashlib.sha256(sample.read_bytes()).hexdigest() != spec["sha256"]:
    raise ValueError("Supply the pinned qualification recording alongside the approved model")
with tempfile.TemporaryDirectory(prefix="orange-speech-") as tmp:
    slot = Path(tmp)
    args = [args.venv.absolute() / "bin/python", cache, slot, sample]
    def run(action, *extra):
        data = subprocess.check_output([root / "bin/orange-transcribe", action, *args, *extra],
                                       env={"PATH": "/usr/bin:/bin"}, timeout=120)
        return json.loads(data)
    quote = run("measure")
    receipt = slot / "accepted.json"
    receipt.write_text(json.dumps(quote))
    output = run("confirm", receipt)
    usage, t = output["measure"], output["transcript"]
    reference = "and so my fellow americans ask not what your country can do for you ask what you can do for your country".split()
    if (output["audio_sha256"] != spec["sha256"] or output["model_version"] != pin["model_version"]
        or usage != quote["measure"] or (usage["duration_ms"],usage["normalized_samples"]) != (11000,176000)
        or t["language"] != "en" or not t["segments"] or re.findall(r"[a-z]+",t["text"].lower()) != reference):
        raise ValueError("Installed transcription profile returned unexpected speech or measurement")
    print(json.dumps({"manifest_sha256":contract["manifest_sha256"], "model": output["model_version"], "language": t["language"], "compute_type": t["compute_type"],
                      "samples": usage["normalized_samples"], "duration_ms": usage["duration_ms"],
                      "reference_words_matched": True, "segments": len(t["segments"])}))
