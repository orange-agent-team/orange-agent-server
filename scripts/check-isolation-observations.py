#!/usr/bin/env python3
"""Exercise the release pin, mTLS session boundary and real Worker with disposable PostgreSQL."""
import argparse
from contextlib import nullcontext
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import zipfile

from owned_postgres import database
from verified_platform_archive import binaries

ROOT = Path(__file__).resolve().parents[1]
PROTOCOL_TEST = "isolation::tests::pinned_release_keeps_real_missing_observations_separate_from_execution_authority"


def run_and_require(command, env, markers):
    pending = set(markers)
    with subprocess.Popen(command, cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE) as child:
        for line in child.stdout:
            print(line, end="", flush=True)
            pending.difference_update([name for name in pending if name in line])
        if child.wait() or pending:
            raise RuntimeError("Command failed or missing verification: " + ", ".join(sorted(pending)))


def check_release(archive):
    release = json.loads((ROOT / "config/worker-isolation-release.v1.json").read_bytes())
    policy = ROOT / "distribution/windows-worker/isolation/oci-base.json"
    if hashlib.sha256(policy.read_bytes()).hexdigest() != release["oci_policy_sha256"]:
        raise ValueError("Local OCI policy differs from the reviewed release")
    if release["wasm_enabled"] is not False or release["supply_authorized_by_report"] is not False:
        raise ValueError("Unreviewed authority in release")
    if archive:
        with archive.open("rb") as source:
            if hashlib.file_digest(source, "sha256").hexdigest() != release["windows_archive_sha256"]:
                raise ValueError("Wrong diagnostic reference archive")
            source.seek(0)
            with zipfile.ZipFile(source) as package:
                for name, key in [("orange-isolation-probe","linux_probe_sha256"),("oci-base.json","oci_policy_sha256")]:
                    with package.open("orange-worker/isolation/" + name) as member:
                        if hashlib.file_digest(member, "sha256").hexdigest() != release[key]:
                            raise ValueError("Changed release member: " + name)
        print("Verified distributed Linux probe and OCI policy", release["windows_archive_sha256"], flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--environment", type=Path, default=ROOT / ".local")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--host", action="store_true", help="Exercise the ordinary CPU Worker startup")
    mode.add_argument("--installed", action="store_true", help="Use the two-build macOS archive for this commit")
    parser.add_argument("--probe-archive", type=Path, help="Compare the pinned reference Windows archive and its members")
    args = parser.parse_args()
    private = args.environment.resolve()
    check_release(args.probe_archive)
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup, "which", "cargo"], text=True, cwd=ROOT).strip())
    env = {k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
    env.update(PATH=str(cargo.parent) + os.pathsep + env["PATH"], CARGO_NET_OFFLINE="true",
               ORANGE_TEST_USER_CONFIG_FILE=str(private / "identity/user-config.json"),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(private / "identity/admin-config.json"))
    run_and_require([cargo,"test","--locked","--offline","-p","orange-protocol","--features","grpc","--lib",
                     PROTOCOL_TEST,"--","--exact"], env, ["test " + PROTOCOL_TEST + " ... ok"])
    with binaries(None) if args.installed else nullcontext(None) as installed:
        with database(private / "database-url", "orange_isolation_") as (_, url):
            env["ORANGE_TEST_DATABASE_URL"] = url
            if args.host or args.installed:
                if installed:
                    env["ORANGE_TEST_ISOLATION_PACKAGE_BIN"] = str(installed)
                env["ORANGE_TEST_MODEL_RUNTIME_ROOT"] = str(private / "model-runtime")
                command = "check-worker-host.py"
                tests = ["actual_worker_binary_uses_real_os_admission_and_exits_on_owned_signal",
                         "actual_worker_binary_keeps_candidate_unsubmitted_and_logs_no_body",
                         "actual_user_api_and_output_gateway_share_one_writer_for_real_worker_delivery"]
            else:
                env["ORANGE_ISOLATION_REPORT_TESTS"] = "1"
                command = "check-worker-session.py"
                tests = ["private_observation_is_epoch_bound_bounded_and_never_grants_execution",
                         "isolated_report_waiting_on_the_node_lock_rolls_back_after_capture_expiry"]
            run_and_require([sys.executable,ROOT/"scripts"/command], env, [test+" ... ok" for test in tests])
    if args.installed:
        print("PASS extracted Worker and User API/gateway exchanged the durable isolation receipt and delivered real CPU output")
    print("PASS bounded session-owned diagnostic submission with no GPU or Wasm authority")


if __name__ == "__main__":
    main()
