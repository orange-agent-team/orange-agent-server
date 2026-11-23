#!/usr/bin/env python3
"""Create real legacy journals and prove upgrade, exact bindings and downgrade refusal."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import time

from journal_schema import verify_current_journal

if not __debug__:
    raise SystemExit("Migration verification requires active assertions")

ROOT = Path(__file__).resolve().parents[1]
PRIOR = {
    34: "2a82e8e018b696adc32e5bb66f1f39e73d558982",
    33: "7876359b73b86c54292e6231da32179556994299",
    29: "94cfd1fc3e27dd2fb43438757af6d6b863b6e98d",
    32: "5349843fa7225df110df733e49e66daf73f509b1",
    31: "938bc63654127951ae52810ba91f44af4eafecf2",
    30: "19533647c8f3d50885f7a8b19a0707f93edc6682",
}
env = dict(os.environ, CARGO_NET_OFFLINE="true")
target = Path(env.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
env["CARGO_TARGET_DIR"] = str(target)
cargo = Path(subprocess.check_output(
    [shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup", "which", "--toolchain", "1.97.1", "cargo"],
    cwd=ROOT, text=True).strip())
env["PATH"] = str(cargo.parent) + os.pathsep + env["PATH"]


def build(root, out):
    subprocess.run([cargo, "build", "--locked", "--offline", "-p", "orange-agent-runtime", "--example", "local-agent"],
                   cwd=root, env=env, check=True)
    shutil.copy2(target / "debug/examples/local-agent", out)


def verify(base, prior, old, new):
    state = base / "state"
    workspace = base / "workspace"
    state.mkdir()
    workspace.mkdir()
    database = state / "journal.sqlite"
    file = state / "private.json"
    config = {"run": {"owner": "migration-owner", "run_id": "old-run", "policy": "local-read", "environment": "pending",
                      "model_version": "offline", "max_steps": 3, "max_invocations": 2, "max_output_tokens": 32,
                      "deadline_unix": int(time.time()) + 600},
              "prompt": "An existing journal entry", "grant_id": "workspace", "workspace": str(workspace), "journal": str(database),
              "private_roots": [str(state)], "model_socket": str(state / "unused.sock"),
              "model_key_file": str(state / "unused.key"), "context_tokens": 4096}

    def run(executable, *args, manifest=None):
        file.write_text(json.dumps(config))
        result = subprocess.run([executable, file, *args], env={"PATH": "/usr/bin:/bin"}, input=json.dumps(manifest) if manifest is not None else None, capture_output=True, text=True, check=True)
        return json.loads(result.stdout)

    def snapshot(current=False):
        # Both child processes have exited. Recover committed WAL pages on a private copy
        # so a read cannot create lock files or checkpoint the journal under verification.
        with tempfile.TemporaryDirectory(dir=base) as copy_dir:
            replica = Path(copy_dir) / database.name
            shutil.copyfile(database, replica)
            for suffix in ("-wal", "-journal"):
                sidecar = database.with_name(database.name + suffix)
                if sidecar.exists():
                    shutil.copyfile(sidecar, replica.with_name(replica.name + suffix))
            db = sqlite3.connect(replica)
            try:
                db.execute("PRAGMA query_only=ON")
                version = verify_current_journal(db) if current else db.execute("PRAGMA user_version").fetchone()[0]
                return version, db.execute("SELECT config_digest,messages,phase FROM runtime_runs WHERE run_id='old-run'").fetchone()
            finally:
                db.close()


    config["run"]["environment"] = run(old, "grant-info")["environment"]
    old_pin = None
    if prior in (32,33,34):
        config["run"]["model_version"]="qwen7b-reviewed-cpu-v1"
        old_pin=next(x["pin"] for x in run(old,"skills") if x["pin"]["id"]=="orange.code-review")
    elif prior == 31:
        manifest = {"id":"local.migration-note","version":"1.0.0","name":"Migrated note","scenario":"AU-02",
                    "instruction":"Keep amounts exactly as given.","tools":[]}
        old_pin = run(old,"review-skill", manifest=manifest)["pin"]
        assert run(old,"install-skill",old_pin["digest"],"0",manifest=manifest)["installation_revision"] == 1
    elif prior == 30:
        old_pin = next(x["pin"] for x in run(old, "skills") if x["pin"]["id"] == "orange.text-summary")
    if old_pin:
        config["skill"] = old_pin
        assert run(old, "enable-skill", "0")["revision"] == 1
    seeded = run(old, "start")
    assert seeded["phase"] == "ready"
    version, before = snapshot()
    assert version == prior and before

    registry = run(new, "skills")
    template = next(x for x in registry if x["pin"]["id"] == "orange.text-summary")
    assert {x["pin"]["id"] for x in registry if x["enabled"]} == ({old_pin["id"]} if old_pin else set())
    destination = json.loads((ROOT / "config/journal-schema.json").read_bytes())["user_version"]
    assert snapshot(current=True) == (destination, before) and run(new, "skill-binding") == old_pin
    assert run(new,"model-observation") is None
    config["skill"] = template["pin"]
    assert run(new, "enable-skill", "0")["revision"] == 1
    if prior == 29:
        run(new, "skill-binding")  # Persist the selected configuration for the retry.
        assert subprocess.run([new, file, "start"], capture_output=True).returncode != 0
    config["run"]["run_id"] = "new-skill-run"
    assert run(new, "start")["phase"] == "ready" and run(new, "skill-binding") == template["pin"]
    config.pop("skill")
    config["run"]["run_id"] = "old-run"
    file.write_text(json.dumps(config))
    def durable_fingerprint():
        return {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                for p in (database, database.with_name(database.name + "-wal"), database.with_name(database.name + "-journal"))
                if p.exists()}

    fingerprint = durable_fingerprint()
    refused = subprocess.run([old, file, "status"], capture_output=True, text=True)
    assert refused.returncode != 0 and "Unsupported" in refused.stderr, (refused.returncode, refused.stderr)
    assert durable_fingerprint() == fingerprint and snapshot(current=True) == (destination, before)
    print(f"PASS genuine schema {prior} -> {destination}, unchanged messages, original binding and activation, old executable refused before mutation")


def main():
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix="orange-skills-migration-") as tmp:
        base = Path(tmp).resolve(strict=True)
        new = base / "new-agent"
        build(ROOT, new)
        for version, commit in PRIOR.items():
            case = base / str(version)
            source = case / "prior"
            source.mkdir(parents=True)
            archive = case / "source.tar"
            subprocess.run(["git", "archive", "--format=tar", "--output", archive, commit], cwd=ROOT, check=True)
            subprocess.run(["tar", "-xf", archive, "-C", source], check=True)
            old = case / "old-agent"
            build(source, old)
            verify(case, version, old, new)


if __name__ == "__main__":
    main()
