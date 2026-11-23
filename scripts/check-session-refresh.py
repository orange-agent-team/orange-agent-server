#!/usr/bin/env python3
"""Verify user and Admin idle-session renewal using actual PostgreSQL row locks."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess

from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--environment", type=Path, default=ROOT / ".local")
    private = parser.parse_args().environment.resolve()
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup, "which", "cargo"], cwd=ROOT, text=True).strip())
    env = {k: v for k, v in os.environ.items() if not k.startswith("ORANGE_")}
    env.update(PATH=str(cargo.parent) + os.pathsep + env["PATH"], CARGO_NET_OFFLINE="true",
               ORANGE_TEST_USER_CONFIG_FILE=str(private / "identity/user-config.json"),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(private / "identity/admin-config.json"))
    cases = {"user_identity": ["session_refresh_after_idle_deadline_is_denied"],
             "admin_identity": ["session_refresh_after_idle_deadline_is_denied",
                                "session_refresh_keeps_only_grants_still_live_after_waiting"]}
    with database(private / "database-url", "orange_session_refresh_") as (_, url):
        env["ORANGE_TEST_DATABASE_URL"] = url
        for suite, names in cases.items():
            result = subprocess.run([cargo, "test", "--locked", "--offline", "-p", "orange-control-plane",
                                     "--features", "integration-tests", "--test", suite, "session_refresh_"],
                                    cwd=ROOT, env=env, stdout=subprocess.PIPE, text=True)
            print(result.stdout, flush=True)
            result.check_returncode()
            if not all(name + " ... ok" in result.stdout for name in names):
                raise RuntimeError("Missing session expiry check in " + suite)
    print("PASS user and Admin deadlines, unchanged expired rows, and surviving scoped grants")


if __name__ == "__main__":
    main()
