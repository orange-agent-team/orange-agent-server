#!/usr/bin/env python3
"""Verify actual package ownership, typed dependencies, process isolation and atomicity."""
import argparse
from collections import Counter
import graphlib
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib

from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]
POLICY = json.loads((ROOT / "docs/engineering/workspace.json").read_bytes())


def require(condition, message):
    if not condition:
        raise ValueError(message)


def rust(root):
    if root == ROOT:
        channel = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    else:
        channel = json.loads((root / "config/bridge-tools.lock.json").read_bytes())["rust"]
    require(re.fullmatch(r"\d+\.\d+\.\d+", channel), "Exact Rust compiler required")
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup, "which", "--toolchain", channel, "cargo"], text=True).strip())
    env = dict(os.environ, CARGO_NET_OFFLINE="true", PATH=str(cargo.parent) + os.pathsep + os.environ["PATH"])
    version = subprocess.check_output([cargo.parent / "rustc", "--version"], env=env, text=True)
    require(version.startswith("rustc " + channel + " "), "Compiler resolution mismatch")
    return cargo, env


def metadata(root):
    cargo, env = rust(root)
    return json.loads(subprocess.check_output(
        [cargo, "metadata", "--locked", "--offline", "--no-deps", "--format-version", "1"], cwd=root, env=env
    ))["packages"]


def server_inventory(packages):
    projects = POLICY["projects"]
    require(set(projects) == {f"E{i:02}" for i in range(1, 12)}, "Core project IDs differ")
    require(Counter(row[0] for row in projects.values()) == {"server":7, "desktop":2, "web":1, "admin":1},
            "Repository allocation differs")
    expected = {name: path for repo, path, name in projects.values() if repo == "server" and path != "backends"}
    expected.update(POLICY["support"])
    actual = {p["name"]:str(Path(p["manifest_path"]).parent.relative_to(ROOT)) for p in packages}
    require(actual == expected, "Server workspace membership or owner differs")
    backend = tomllib.loads((ROOT / "backends/pyproject.toml").read_text())["project"]
    require(backend["name"] == projects["E09"][2] and (ROOT / "backends/uv.lock").is_file(), "E09 lock or name differs")
    bins = {t["name"]:p["name"] for p in packages for t in p["targets"] if "bin" in t["kind"]}
    require(bins == {k:v[0] for k,v in POLICY["processes"].items()}, "Missing or unexpected process entry point")


def check_edges(packages):
    normal = {}
    development = {}
    for p in packages:
        prod, tests = set(), set()
        for d in p["dependencies"]:
            if p["name"] == "orange-domain":
                raise ValueError("Domain acquired a dependency: " + d["name"])
            if "path" in d:
                path = Path(d["path"]).resolve()
                require(any(Path(q["manifest_path"]).parent == path and q["name"] == d["name"] for q in packages),
                        "Path dependency outside the owned workspace: " + str(path))
                (tests if d["kind"] == "dev" else prod).add(d["name"])
        normal[p["name"]] = prod
        if tests:
            development[p["name"]] = tests
    require(normal == {p:set(deps) for p,deps in POLICY["production_edges"].items()}, "Production edge policy differs")
    require(development == {p:set(deps) for p,deps in POLICY["development_edges"].items()}, "Test edge policy differs")
    print("Production build order:", *graphlib.TopologicalSorter(normal).static_order(), flush=True)


def git_blob(commit, path):
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "Consumer source must be an immutable commit")
    return subprocess.check_output(["git", "-C", ROOT, "show", commit + ":" + path])


def check_clients(clients):
    desktop = clients["desktop"]
    for _, (repo, location, name) in POLICY["projects"].items():
        if repo == "server":
            continue
        p = clients[repo] / location
        if repo in ("web", "admin"):
            pkg = json.loads((p / "package.json").read_bytes())
            lock = json.loads((p / "package-lock.json").read_bytes())
            require(pkg["name"] == lock["packages"][""]["name"] == name, "Wrong npm package owner")
            for entry in lock["packages"].values():
                require(not entry.get("link") and not entry.get("resolved","").startswith(("file:", "../", "/")),
                        "Frontend lock points to a shared local checkout")
            directory = clients[repo] / "docs/contracts"
            snapshot = json.loads((directory / "snapshot.json").read_bytes())
            require(snapshot["source_repository"] == "orange-agent-server" and snapshot["files"], "Missing API provenance")
            for file, item in snapshot["files"].items():
                data = (directory / file).read_bytes()
                require(hashlib.sha256(data).hexdigest() == item["sha256"] and
                        data == git_blob(snapshot["source_commit"], item["source"]), "Contract drift: " + file)
        elif location.startswith("crates/"):
            require(tomllib.loads((p / "Cargo.toml").read_text())["package"]["name"] == name, "Wrong bridge owner")
        else:
            require(re.search(r"^name:\s*" + re.escape(name) + r"\s*$", (p / "pubspec.yaml").read_text(), re.M)
                    and (p / "pubspec.lock").is_file(), "Wrong Flutter app or missing lock")
    lock = json.loads((desktop / "config/runtime.lock.json").read_bytes())
    git_blob(lock["origin_commit"], "crates/agent-runtime/Cargo.toml")
    subprocess.run([sys.executable, desktop / "scripts/import-runtime.py", "--verify"], cwd=desktop, check=True)
    packages = metadata(desktop)
    bridge = next(p for p in packages if p["name"] == POLICY["projects"]["E05"][2])
    imports = {d["name"]:Path(d["path"]).resolve() for d in bridge["dependencies"] if "path" in d}
    vendor = (desktop / lock["vendor_directory"]).resolve()
    require(set(imports) == {"orange-domain","orange-protocol","orange-agent-runtime","orange-model-runtime"} and
            all(path.is_relative_to(vendor) for path in imports.values()), "Bridge imports outside locked runtime")
    print("Verified client owners, fixed desktop runtime and both immutable API snapshot sets", flush=True)


def check_client_builds(clients, evidence):
    cargo, env = rust(clients["desktop"])
    subprocess.run([cargo,"check","-p","orange-desktop-bridge","--all-targets","--locked","--offline"],
                   env=env, cwd=clients["desktop"], check=True)
    for name, repo in clients.items():
        source = subprocess.check_output(["git","-C",repo,"rev-parse","HEAD"], text=True).strip()
        receipts = evidence / "app-package-receipts" / source
        candidates = list(receipts.glob("orange-" + name + "-*.tar.gz.json"))
        require(len(candidates) == 1, "Need an unambiguous previously double-built " + name + " receipt for " + source)
        receipt = json.loads(candidates[0].read_bytes())
        archive = evidence / "packages" / receipt["archive"]
        require(archive.name + ".json" == candidates[0].name and receipt["source"] == source and
                archive.stat().st_size == receipt["bytes"], "Client build provenance differs")
        with archive.open("rb") as stream:
            require(hashlib.file_digest(stream, "sha256").hexdigest() == receipt["sha256"], "Archive digest differs")
        if name == "desktop":
            require(receipt["clean_builds"] == 2 and receipt["verification"]["bridge_probe"]["bridge_content_hash"] and
                    receipt["verification"]["local_signature_verified"], "Unverified native Flutter build")
        else:
            require(receipt["clean_offline_npm_installs"] == receipt["identical_archives"] == 2 and
                    receipt["smoke"]["chrome"] and receipt["smoke"]["no_external_asset_requests"], "Unverified frontend build")
        print("PASS", name, source, receipt["sha256"], flush=True)


def build_workspace(packages, uv_cache):
    cargo, env = rust(ROOT)
    for name in sorted(p["name"] for p in packages):
        subprocess.run([cargo,"check","--locked","--offline","-p",name,"--all-targets"], cwd=ROOT, env=env, check=True)
        print("PASS independent", name, flush=True)
    subprocess.run([cargo,"check","--locked","--offline","--workspace","--all-targets","--all-features"], cwd=ROOT, env=env, check=True)


    subprocess.run([sys.executable,ROOT/"scripts/package-embedding-backend.py","--uv-cache",uv_cache],
                   cwd=ROOT, env=dict(env, UV_OFFLINE="true"), check=True)


def verify_roles(url_file):
    cargo, env = rust(ROOT)
    opts = ["--locked","--offline"]
    supervisor = subprocess.run([cargo,"test",*opts,"-p","orange-model-runtime","--features","process-test-fixture",
                    "--test","supervisor"], cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE)
    print(supervisor.stdout, flush=True)
    supervisor.check_returncode()
    for case in ("ui_process_crash_triggers_owned_supervisor_cleanup", "owned_process_private_ipc_and_explicit_shutdown_release_lease",
                 "wrong_or_absent_parent_authentication_has_no_model_process_effect"):
        require("test " + case + " ... ok" in supervisor.stdout, "Missing real process test: " + case)
    tests = ("concurrent_budget_contention_is_atomic_and_cancel_restores_original_buckets",
             "failed_freeze_and_bad_quotes_leave_no_side_effect","database_enforces_immutable_balanced_ledger")
    with database(url_file, "orange_eng_roles_") as (_, url):
        for name in tests:
            result = subprocess.run([cargo,"test",*opts,"-p","orange-control-plane","--features","integration-tests","--test","postgres",name,
                                              "--","--exact"], cwd=ROOT, env=dict(env, ORANGE_TEST_DATABASE_URL=url), text=True, stdout=subprocess.PIPE)
            print(result.stdout, flush=True)
            result.check_returncode()
            require("1 passed; 0 failed" in result.stdout, "Expected one passing real PostgreSQL test: " + name)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--section", choices=["inventory","edges","builds","roles","clients","client-builds"], required=True)
    for project in ("desktop","web","admin"):
        parser.add_argument("--" + project, type=Path)
    parser.add_argument("--database-url-file", type=Path, default=ROOT / ".local/database-url")
    parser.add_argument("--uv-cache", type=Path, default=ROOT / ".local/uv-cache")
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    packages = metadata(ROOT)
    server_inventory(packages)
    if args.section == "edges":
        check_edges(packages)
    elif args.section in ("clients","client-builds"):
        clients = {name:getattr(args,name) for name in ("desktop","web","admin")}
        require(all(clients.values()), "Supply all three independent client repository paths for integration verification")
        clients = {name:path.resolve() for name,path in clients.items()}
        check_clients(clients)
        if args.section == "client-builds":
            require(args.evidence is not None, "Supply the local source-bound build evidence directory")
            check_client_builds(clients, args.evidence.resolve())
    elif args.section == "builds":
        build_workspace(packages, args.uv_cache.resolve())
    elif args.section == "roles":
        verify_roles(args.database_url_file)
    print("PASS architecture", args.section, flush=True)


if __name__ == "__main__":
    main()
