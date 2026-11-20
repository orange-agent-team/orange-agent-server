"""Verify the local development bundle and install its wheels in a new Python 3.12 venv."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import sys
import venv

root = Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument("venv", type=Path)
args = parser.parse_args()
if sys.version_info[:2] != (3, 12) or (sys.platform, platform.machine()) != ("darwin", "arm64"):
    raise SystemExit("macOS arm64 and Python 3.12 required")
manifest = json.loads((root / "manifest.json").read_text())
for name, expected in manifest["sha256"].items():
    path = (root / name).resolve()
    if not path.is_relative_to(root) or not path.is_file():
        raise SystemExit("Bundle member missing")
    with path.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != expected:
            raise SystemExit("Bundle digest mismatch: " + name)
env = args.venv.absolute()
env.mkdir(mode=0o700, exist_ok=False)
venv.EnvBuilder(with_pip=True).create(env)
pip = [str(env / "bin/python"), "-I", "-m", "pip", "--isolated", "--disable-pip-version-check"]
subprocess.run(pip + ["install", "--no-index", "--no-cache-dir", "--only-binary=:all:",
                      "--find-links", str(root / "wheels"), "--require-hashes", "-r",
                      str(root / "requirements.txt")], check=True)
subprocess.run(pip + ["check"], check=True)
print("Installed and verified the offline CPU environment at", env)
