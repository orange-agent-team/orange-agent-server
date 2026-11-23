#!/usr/bin/env python3
"""Check parser/process boundaries and run the normal read-only Worker command on this host."""
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess

ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],CARGO_NET_OFFLINE="true")
tests=subprocess.run([cargo,"test","--locked","--offline","-p","orange-worker","--lib","gpu_inventory::tests"],
                     cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
print(tests.stdout,flush=True)
tests.check_returncode()
required=["multi_device_order_and_missing_measurements_are_explicit","contradictory_or_unbounded_reports_do_not_publish_partial_inventory"]
if os.name=="posix":required.append("subprocess_uses_only_read_arguments_times_out_and_discards_failed_output")
if not all("gpu_inventory::tests::"+name+" ... ok" in tests.stdout for name in required):
    raise RuntimeError("Missing inventory assertions")
subprocess.run([cargo,"build","--locked","--offline","-p","orange-worker","--bin","orange-worker"],cwd=ROOT,env=env,check=True)
metadata=json.loads(subprocess.check_output([cargo,"metadata","--locked","--offline","--format-version","1","--no-deps"],cwd=ROOT,env=env))
exe=Path(metadata["target_directory"])/"debug"/("orange-worker.exe" if os.name=="nt" else "orange-worker")
result=subprocess.run([exe,"probe-gpu"],capture_output=True,text=True,timeout=10)
info=json.loads(result.stdout)
if set(info)!={"version","os","architecture","status","devices"} or info["version"]!=1:
    raise ValueError("Unknown report shape")
status=info["status"]
if status=="observed":
    if result.returncode or not info["devices"]:raise ValueError("Missing devices")
    print("Actual NVIDIA driver reported",len(info["devices"]),"devices; inference and reclamation untested.")
elif not (result.returncode==2 and not info["devices"] and status in {"no_devices_observed","unsupported_platform",
        "driver_tool_unavailable","driver_query_failed","timed_out","invalid_driver_output","probe_cleanup_unconfirmed"}):
    raise ValueError("Failed query did not return a closed status")
if platform.system()=="Darwin" and status!="unsupported_platform":
    raise ValueError("macOS unexpectedly advertised NVIDIA devices")
extra=subprocess.run([exe,"probe-gpu","unapproved-path"],capture_output=True,timeout=5)
if extra.returncode==0 or extra.stdout:
    raise ValueError("Unexpected caller-selected executable acceptance")
print("PASS local Worker inventory CLI",info["os"],info["architecture"],status)
