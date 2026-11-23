#!/usr/bin/env python3
"""Run the non-GPU portion of the contributor restrictions on an actual private Linux OCI instance."""
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

ROOT=Path(__file__).resolve().parents[1]
assets=ROOT/"distribution/windows-worker/isolation"
with tempfile.TemporaryDirectory() as td:
    probe=Path(td)/"probe"
    subprocess.run([sys.executable,ROOT/"scripts/build-worker-isolation-probe.py","--target","aarch64-unknown-linux-musl",
                    "--output",probe],cwd=ROOT,check=True)
    data=io.BytesIO()
    with tarfile.open(fileobj=data,mode="w") as bundle:
        bundle.add(probe,arcname="probe",recursive=False)
        bundle.add(assets/"oci-base.json",arcname="config.json",recursive=False)
    cmd=["colima","ssh","--profile","orange-agent","--","sudo","-n","python3","-c",(ROOT/"scripts/worker/qualify-oci-policy-guest.py").read_text()]
    result=subprocess.run(cmd,input=data.getvalue(),stdout=subprocess.PIPE,timeout=90)
    result.check_returncode()
    report=json.loads(result.stdout)
    if (not report["cgroup_removed"] or not report["runtime_instance_removed"] or report["physical_gpu_verified"] or
        report["probe_sha256"]!=hashlib.sha256(probe.read_bytes()).hexdigest() or not all(report["isolated_namespaces"].values())):
        raise ValueError("Incomplete guest measurements")
    print(json.dumps(report),flush=True)
    print("PASS live private OCI namespaces, read-only root, private IPC, internet socket denial, cgroup limits and removal on Linux arm64")
