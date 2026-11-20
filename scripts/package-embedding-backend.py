#!/usr/bin/env python3
"""Build the E09 development wheel twice and verify its reproducibility and source content."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
from zipfile import ZipFile

root = Path(__file__).resolve().parents[1]
source = root / "backends"
dest = root / ".local/python-dist/reproducible"
dest.mkdir(parents=True, exist_ok=True)
env = dict(
    os.environ,
    SOURCE_DATE_EPOCH="946684800",
    UV_CACHE_DIR=str(root / ".local/uv-cache"),
)
with tempfile.TemporaryDirectory(prefix="orange-wheel-") as scratch:
    outputs = []
    for index in range(2):
        out = Path(scratch) / str(index)
        subprocess.run(
            [
                "uv",
                "build",
                "--offline",
                "--wheel",
                "--python",
                "3.12",
                "--out-dir",
                str(out),
            ],
            cwd=source,
            env=env,
            check=True,
        )
        (wheel,) = out.glob("*.whl")
        with ZipFile(wheel) as archive:
            for file in (source / "orange_inference_backends").glob("*.py"):
                if archive.read(str(file.relative_to(source))) != file.read_bytes():
                    raise ValueError("wheel does not contain verified runner source")
        outputs.append(wheel.read_bytes())
    if outputs[0] != outputs[1]:
        raise ValueError("independent builds produced different wheels")
    artifact = dest / wheel.name
    if artifact.exists() and artifact.read_bytes() != outputs[0]:
        raise ValueError("existing release artifact differs")
    artifact.write_bytes(outputs[0])
    report = {
        "wheel": artifact.name,
        "sha256": hashlib.sha256(outputs[0]).hexdigest(),
        "size": len(outputs[0]),
        "identical_builds": 2,
    }
    (dest / "build.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))
