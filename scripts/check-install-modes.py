#!/usr/bin/env python3
"""Verify read-only WSL queries and the independent consumer/contributor entry points."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PS = ROOT / "distribution/windows-worker/Test-WorkerInventory.ps1"


def powershell(tool):
    # Standalone in an empty directory: a consumer must not need the Worker binary or manifest.
    with tempfile.TemporaryDirectory() as tmp:
        source = Path(tmp) / PS.name
        shutil.copyfile(PS, source)
        env = dict(os.environ, POWERSHELL_TELEMETRY_OPTOUT="1", POWERSHELL_UPDATECHECK="Off",
                   XDG_CACHE_HOME=tmp, XDG_CONFIG_HOME=tmp, XDG_DATA_HOME=tmp)
        command = [tool, "-NoProfile", "-NonInteractive", "-File", source]
        result = subprocess.run([*command, "-Mode", "Consume"], cwd=tmp, env=env, capture_output=True,
                                text=True, check=True, timeout=20)
        report = json.loads(result.stdout)
        if (report.get("mode") != "consume" or report.get("wsl_required") is not False
                or report.get("local_worker_required") is not False or "inventory" in report or "environment" in report):
            raise ValueError("Standalone consumer mode opened a contributor probe")
        contrib = subprocess.run([*command, "-Mode", "Contribute"], cwd=tmp, env=env,
                                 capture_output=True, text=True, timeout=20)
        if not contrib.returncode or contrib.stdout.strip():
            raise ValueError("Contributor reported without a verified Windows package")
    print("PASS standalone consumer mode and refused contributor probe without a verified package")


def native():
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup, "which", "cargo"], cwd=ROOT, text=True).strip())
    env = dict(os.environ, PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"], CARGO_NET_OFFLINE="true")
    tests = subprocess.run([cargo, "test", "-p", "orange-worker", "--locked", "--offline", "--lib", "wsl_inventory::tests"],
                           cwd=ROOT, env=env, stdout=subprocess.PIPE, text=True, check=True)
    print(tests.stdout, flush=True)
    required = ["localized_rows_match_private_names_and_only_publish_counts"]
    if os.name == "posix":
        required.append("only_uses_the_two_read_queries_and_keeps_failures_distinct")
    if any("wsl_inventory::tests::"+case+" ... ok" not in tests.stdout for case in required):
        raise ValueError("Named WSL test missing")
    subprocess.run([cargo, "build", "-p", "orange-worker", "--locked", "--offline", "--bin", "orange-worker"],
                   cwd=ROOT, env=env, check=True)
    meta=json.loads(subprocess.check_output([cargo,"metadata","--no-deps","--format-version","1","--locked"],cwd=ROOT,env=env))
    binary=Path(meta["target_directory"])/"debug"/("orange-worker.exe" if os.name=="nt" else "orange-worker")
    result=subprocess.run([binary,"probe-wsl"],capture_output=True,timeout=18)
    report=json.loads(result.stdout)
    if set(report)!={"version","status","distributions"} or report["version"]!=1:
        raise ValueError("Unexpected WSL report")
    if report["status"]=="observed":
        d=report["distributions"]
        if result.returncode!=0 or set(d)!={"wsl1","wsl2"} or any(type(v) is not int or not 0<=v<=256 for v in d.values()):
            raise ValueError("Invalid WSL distribution counts")
    elif result.returncode!=2 or report["distributions"] is not None:
        raise ValueError("Unconfirmed observations must not publish counts")
    if os.name!="nt" and report["status"]!="unsupported_platform":
        raise ValueError("Unsupported host advertised WSL")
    invalid=subprocess.run([binary,"probe-wsl","--install"],capture_output=True,timeout=5)
    if not invalid.returncode or invalid.stdout:
        raise ValueError("Probe accepted extra arguments")
    print("PASS read-only WSL reporting on",platform.system(),report["status"])


if __name__=="__main__":
    parser=argparse.ArgumentParser()
    parser.add_argument("--powershell",type=Path,help="Run the portable installation-mode script qualification instead")
    args=parser.parse_args()
    if args.powershell:
        powershell(args.powershell.resolve(strict=True))
    else:
        native()
