#!/usr/bin/env python3
"""Verify security design coverage, then trace named defenses through real local entry points."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]
MODEL = json.loads((ROOT / "docs/security/trust-model.json").read_bytes())


def require(ok, message):
    if not ok:
        raise ValueError(message)


def assets():
    domains = MODEL["domains"]
    require({"consumer","desktop","admin","platform","cloud_host","workload","supplier","worker","merchant","identity","publisher"} == set(domains), "Incomplete asset domains")
    require(all(d["assets"] and d["authority"] for d in domains.values()), "Missing data or authority")
    edges = {}
    for source, target, control in MODEL["flows"]:
        require(source in domains and target in domains and source != target and (source,target) not in edges and control.strip(),
                "Invalid or duplicate trust crossing")
        edges[source,target] = control
    require(set(domains) == {d for pair in edges for d in pair}, "Isolated trust domain")
    require(("cloud_host","workload") in edges and ("supplier","worker") in edges and ("supplier","workload") not in edges,
            "Hosted tools and supplier inference must have separate authorities")
    require(domains["supplier"]["authority"] != domains["workload"]["authority"], "Host and workload authority collapsed")


def coverage():
    risks = MODEL["risks"]
    require(len({r["id"] for r in risks}) == len(risks), "Duplicate risk IDs")
    require({"input parsing","credential misuse","resource fraud","payment replay","supply chain","supplier visibility",
             "hosted workspace boundary","stale assignment"} <= {r["surface"] for r in risks}, "Missing reviewed surface")
    require({"supplier-to-consumer","consumer-to-supplier"} <= {d for r in risks for d in r["direction"]}, "Missing two-sided risk")
    require({"funds","privacy","host"} == {r["impact"] for r in risks}, "Missing reviewed impact")
    for r in risks:
        require(r["domain"] in MODEL["domains"] and r["gate"] in {"first","later"}, "Unowned risk or gate")


def controls():
    for risk in MODEL["risks"]:
        require(all(re.fullmatch(r"E(?:0[1-9]|1[01])", owner) for owner in risk["owner"].split("/")), "Unknown control owner")
        require(risk["control"].strip() and risk["residual"].strip() and risk["residual"] != risk["control"],
                "Missing residual risk or control")
        source = (ROOT / risk["source"]).resolve()
        require(source.is_relative_to(ROOT) and source.is_file(), "Missing control source")
        if risk["id"] != "R-workload":
            require("fn " + risk["test"] + "(" in source.read_text(), "Named regression missing: " + risk["test"])
    require({r["gate"] for r in MODEL["risks"]} == {"first","later"}, "Missing deferred or first-release decisions")


def verified_output(command, env, cases):
    result = subprocess.run(command, cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, flush=True)
    result.check_returncode()
    for case in cases:
        require(re.search(r"^test (?:\S+::)*" + re.escape(case) + r" \.\.\. ok$", result.stdout, re.M),
                "Required local case did not execute: " + case)


def review(private):
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"], text=True).strip())
    env = dict(os.environ, CARGO_NET_OFFLINE="true", PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
    cases = {r["id"]:r["test"] for r in MODEL["risks"]}
    for project, ids in [("orange-model-runtime",["R-input","R-package"]),("orange-worker",["R-device","R-reuse"])]:
        for key in ids:
            verified_output([cargo,"test","-p",project,"--lib","--locked","--offline",cases[key]],env,[cases[key]])
    with database(private/"database-url","orange_security_") as (_, url):
        local = dict(env, ORANGE_TEST_DATABASE_URL=url, ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"))
        verified_output([cargo,"test","-p","orange-control-plane","--features","integration-tests","--locked","--offline",
                         "--test","admin_identity",cases["R-authority"],"--","--exact"],local,[cases["R-authority"]])
        verified_output([sys.executable,ROOT/"scripts/check-worker-session.py"],local,[cases["R-host-privacy"]])
    verified_output([sys.executable,ROOT/"scripts/check-test-checkout.py","--database-url-file",private/"database-url",
                     "--identity-config",private/"identity/user-config.json"],env,[cases["R-billing"]])
    lock = json.loads((ROOT/"config/cloud-isolation.lock.json").read_bytes())
    subprocess.run([sys.executable, ROOT/"scripts/check-cloud-isolation.py", "--archive",
                    private/"gvisor-download"/lock["archive"]], cwd=ROOT, env=env, check=True)
    proof = json.loads((ROOT/".local/cloud-isolation/result.json").read_bytes())
    require(proof["qualified"] and proof["archive_sha256"] == lock["sha256"] and proof["sandbox_and_cgroup_removed"]
            and proof["root_readonly"] and proof["effective_capabilities"] == 0 and proof["workload_uid"] != 0
            and proof["network"] == "none" and proof["sentinel_unreachable"], "Local guest isolation check differs")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--section",required=True,choices=["assets","exposure","controls","review"])
    parser.add_argument("--private-dir",type=Path,default=ROOT/".local")
    args = parser.parse_args()
    assets()
    if args.section != "assets":
        coverage()
    if args.section in ("controls","review"):
        controls()
    if args.section == "review":
        review(args.private_dir.resolve())
    print("PASS security design",args.section,flush=True)


if __name__ == "__main__":
    main()
