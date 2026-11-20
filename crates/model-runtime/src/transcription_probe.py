"""Measure a local interpreter and selected loaded engine components without exporting paths."""
import hashlib
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import platform
import stat
import sys
import sysconfig


def sha(path):
    # Resolve the venv interpreter link; hash the object behind the opened regular descriptor.
    fd = os.open(Path(path).resolve(strict=True), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as f:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= 512 * 1024 * 1024:
            raise ValueError("unsupported component")
        return hashlib.file_digest(f, "sha256").hexdigest()


pin = json.loads(sys.stdin.buffer.read(32769))
versions = {n: importlib.metadata.version(n) for n in pin["packages"]}
if versions != pin["packages"] or sys.implementation.name != "cpython":
    raise ValueError("unapproved runtime versions")
os_name = {"darwin": "macos", "linux": "linux"}.get(sys.platform)
cpu = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}.get(platform.machine())
if not os_name or not cpu or os_name+"-"+cpu != pin["target"] or sys.version_info[:2] != (3,12):
    raise ValueError("unqualified target")
components = {}
for name in pin["critical_modules"]:
    spec = importlib.util.find_spec(name)
    if not spec or not spec.origin:
        raise ValueError("missing engine module")
    components[name] = sha(spec.origin)
print(json.dumps({"target": pin["target"], "python_version":platform.python_version(),
                  "python_abi":sysconfig.get_config_var("SOABI"),"python_sha256":sha(sys.executable),
                  "packages":versions,"modules":components}, sort_keys=True, separators=(",",":")))
