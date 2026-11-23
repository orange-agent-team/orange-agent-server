#!/usr/bin/env python3
"""Rebuild reference wallet balances in PostgreSQL and exercise the shipped read-only operator."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from urllib.parse import unquote, urlsplit

from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]


def exercise(url_file, folder):
    with database(url_file, "orange_wallet_check_") as (_, url):
        parsed = urlsplit(url)
        private = folder / "database-url"
        private.touch(mode=0o600)
        private.write_text(url)
        env = dict(os.environ, PGHOST=parsed.hostname, PGPORT=str(parsed.port or 5432),
                   PGUSER=unquote(parsed.username or ""), PGPASSWORD=unquote(parsed.password or ""),
                   PGDATABASE=parsed.path[1:], PGCONNECT_TIMEOUT="5")
        psql = shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
        base = [psql, "-X", "-w", "-Atq", "-v", "ON_ERROR_STOP=1"]
        def sql(statement):
            return subprocess.check_output(base, input=statement, text=True, env=env).strip()

        for migration in sorted((ROOT / "services/control-plane/migrations").glob("*.sql")):
            subprocess.run([*base, "--single-transaction", "-f", migration], check=True,
                           env=env, stdout=subprocess.DEVNULL)
        operator = [sys.executable, ROOT / "distribution/platform/reconcile-wallets.py",
                    "--database-url-file", private]
        def verify(code, owners, fields, missing=0):
            result = subprocess.run(operator, capture_output=True, text=True)
            if result.returncode != code:
                raise RuntimeError(f"Unexpected reconciliation exit {result.returncode}: {result.stderr}")
            report = json.loads(result.stdout)
            if (report["affected_owners"], report["mismatches_by_field"], report["missing_wallets"],
                report["unbalanced_transactions"]) != (owners, fields, missing, 0):
                raise ValueError(f"Unexpected read-only report: {report}")
            return report

        def change(key, entries, updates):
            # All caller values below are fixed acceptance fixtures in the disposable database.
            postings = ",".join(f"('{key}','{account}',{amount})" for account, amount in entries)
            sql(f"BEGIN; INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) "
                f"VALUES('{key}','{key}','buyer:personal'); "
                f"INSERT INTO ledger_entries(transaction_id,account,amount) VALUES {postings}; {updates}; COMMIT;")

        sql("INSERT INTO owners(owner_id) VALUES ('buyer:personal'),('provider:personal'); "
            "INSERT INTO wallets(owner_id) SELECT owner_id FROM owners;")
        verify(0, 0, {})
        exact = 9007199254740993
        change("seed", [("buyer:personal:gift:available",exact),("buyer:personal:purchased:available",200),
                        ("fixture:funding",-exact-200)],
               f"UPDATE wallets SET gift={exact},purchased=200 WHERE owner_id='buyer:personal'")
        change("hold", [("buyer:personal:gift:available",-40),("buyer:personal:gift:reserved",40)],
               "UPDATE wallets SET gift=gift-40,reserved_gift=40 WHERE owner_id='buyer:personal'")
        change("settled", [("buyer:personal:gift:reserved",-40),("buyer:personal:gift:available",10),
                           ("provider:personal:earned:pending",25),("platform:fees",5)],
               "UPDATE wallets SET reserved_gift=0,gift=gift+10 WHERE owner_id='buyer:personal'; "
               "UPDATE wallets SET pending_earned=25 WHERE owner_id='provider:personal'")
        change("refund-hold", [("buyer:personal:purchased:available",-80),("buyer:personal:purchased:refund-held",60),
                               ("buyer:personal:purchased:reserved",20)],
               "UPDATE wallets SET purchased=120,reserved_purchased=80 WHERE owner_id='buyer:personal'")
        verify(0, 0, {})
        change("refund-release", [("buyer:personal:purchased:refund-held",-60),("buyer:personal:purchased:available",60)],
               "UPDATE wallets SET purchased=180,reserved_purchased=20 WHERE owner_id='buyer:personal'")
        change("release-provider", [("provider:personal:earned:pending",-25),("provider:personal:earned:available",25)],
               "UPDATE wallets SET pending_earned=0,earned=25 WHERE owner_id='provider:personal'")
        clean = verify(0, 0, {})
        sql("UPDATE wallets SET purchased=purchased+1 WHERE owner_id='buyer:personal';")
        verify(2, 1, {"purchased":1})
        change("orphan", [("missing:wallet:earned:pending",1),("fixture:funding",-1)], "SELECT 1")
        verify(2, 2, {"purchased":1, "pending_earned":1}, 1)
        verify(2, 2, {"purchased":1, "pending_earned":1}, 1)
        if sql("SELECT purchased FROM wallets WHERE owner_id='buyer:personal'") != "181":
            raise ValueError("Operator changed the source balance")
        print("PASS wallet reconciliation: actual PostgreSQL, synthetic balanced postings, cross-owner income,"
              " refund and job holds, exact large integers, unexplained balance and orphan posting; "
              f"{clean['transactions_checked']} matched transactions before the deliberate drift.")


if __name__ == "__main__":
    cli = argparse.ArgumentParser()
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    args = cli.parse_args()
    with tempfile.TemporaryDirectory() as folder:
        exercise(args.environment.resolve()/"database-url", Path(folder))
