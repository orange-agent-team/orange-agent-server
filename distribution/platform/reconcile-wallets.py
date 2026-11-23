#!/usr/bin/env python3
"""Compare wallet balances to immutable postings on an explicitly selected loopback database."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from urllib.parse import unquote, urlsplit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database-url-file", type=Path, required=True)
    args = parser.parse_args()
    url = urlsplit(args.database_url_file.read_text().strip())
    if (url.scheme not in {"postgres", "postgresql"} or url.hostname not in {"localhost", "127.0.0.1", "::1"}
            or not url.username or not url.path.strip("/") or url.query or url.fragment):
        raise ValueError("An explicit local PostgreSQL database is required")
    env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
    env.update(PGHOST=url.hostname, PGPORT=str(url.port or 5432), PGUSER=unquote(url.username),
               PGPASSWORD=unquote(url.password or ""), PGDATABASE=unquote(url.path[1:]), PGCONNECT_TIMEOUT="5",
               PGOPTIONS="-c default_transaction_read_only=on -c statement_timeout=60000 -c search_path=pg_catalog,public")
    psql = shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
    sql = Path(__file__).resolve().parent / "sql/reconcile-wallets.sql"
    result = subprocess.run([psql, "-X", "-w", "-At", "-v", "ON_ERROR_STOP=1", "-f", sql],
                            env=env, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError("Database reconciliation query failed")
    report = json.loads(result.stdout)
    print(json.dumps(report, sort_keys=True))
    return 2 if report["affected_owners"] or report["unbalanced_transactions"] else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError):
        sys.exit("Unable to complete read-only wallet reconciliation. Check local configuration and table access.")
