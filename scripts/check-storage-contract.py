#!/usr/bin/env python3
"""Exercise the API-005 storage contract on owned, disposable loopback databases."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]


def run_case(cargo, environment, target, case):
    command = [cargo, "test", "--offline", "--locked", "-p", "orange-control-plane",
               "--features", "integration-tests", "--test", target, case, "--", "--exact"]
    result = subprocess.run(command, cwd=ROOT, env=environment, text=True, stdout=subprocess.PIPE)
    print(result.stdout, end="", flush=True)
    result.check_returncode()
    if (f"test {case} ... ok" not in result.stdout.splitlines()
            or "1 passed; 0 failed" not in result.stdout):
        raise RuntimeError(f"Missing acceptance case {target}::{case}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--section", required=True, choices=["keys", "transactions", "retention", "upgrade", "lifecycle", "operational-retention"])
    parser.add_argument("--environment", type=Path, default=ROOT / ".local")
    args = parser.parse_args()
    private = args.environment.resolve()
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup, "which", "--toolchain", "1.97.1", "cargo"], text=True).strip())
    env = {key: value for key, value in os.environ.items() if not key.startswith("ORANGE_")}
    env.update(PATH=str(cargo.parent) + os.pathsep + env["PATH"], CARGO_NET_OFFLINE="true")
    url_file = private / "database-url"

    def exercise(script, *flags):
        subprocess.run([sys.executable, ROOT / "scripts" / script, *flags], cwd=ROOT, env=env, check=True)

    if args.section == "operational-retention":
        with database(url_file, "orange_operational_") as (_, url):
            run_case(cargo, dict(env, ORANGE_TEST_DATABASE_URL=url), "operational_retention",
                     "aged_notification_prefix_preserves_recent_pending_and_independent_financial_facts")
    elif args.section == "lifecycle":
        with database(url_file, "orange_privacy_class_") as (_, url):
            run_case(cargo, dict(env, ORANGE_TEST_DATABASE_URL=url), "storage_contract",
                     "lifecycle_catalogue_has_current_storage_and_ownership_anchors")
    elif args.section == "retention":
        exercise("check-retention-batches.py", "--environment", private)
    elif args.section == "upgrade":
        with database(url_file, "orange_storage_upgrade_") as (_, url):
            run_case(cargo, dict(env, ORANGE_TEST_RETENTION_UPGRADE_DATABASE_URL=url), "storage_contract",
                     "schema68_history_upgrades_in_place_and_duplicate_business_keys_still_rollback")
        exercise("check-developer-troubleshooting.py", "--environment", private)
    elif args.section == "keys":
        with database(url_file, "orange_storage_keys_") as (_, url):
            run_case(cargo, dict(env, ORANGE_TEST_DATABASE_URL=url), "storage_contract",
                     "migrated_database_exposes_the_declared_transaction_keys_and_indexes")
    else:
        with database(url_file, "orange_storage_tx_") as (_, url):
            for target, case in [
                ("postgres", "failed_freeze_and_bad_quotes_leave_no_side_effect"),
                ("postgres", "concurrent_budget_contention_is_atomic_and_cancel_restores_original_buckets"),
                ("events", "domain_commit_produces_ordered_replay_and_duplicates_do_not_add_events"),
                ("queue", "database_effect_and_completion_commit_together_and_stale_lease_never_applies"),
            ]:
                run_case(cargo, dict(env, ORANGE_TEST_DATABASE_URL=url), target, case)
        exercise("check-worker-contract.py", "--section", "execution", "--environment", private)
        exercise("check-test-checkout.py", "--database-url-file", url_file,
                 "--identity-config", private / "identity/user-config.json")
    prefix = "SEC-004" if args.section in ("lifecycle","operational-retention") else "API-005"
    print(f"PASS {prefix} {args.section}", flush=True)


if __name__ == "__main__":
    main()
