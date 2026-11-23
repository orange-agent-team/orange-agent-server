#!/usr/bin/env python3
"""Upgrade existing postings, assemble a concurrent group and check immutable committed contents."""
import argparse
import os
from pathlib import Path
import selectors
import shutil
import subprocess
from urllib.parse import unquote,urlsplit

from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
MIGRATIONS=ROOT/"services/control-plane/migrations"


def check(url):
    parsed=urlsplit(url)
    env=dict(os.environ,PGHOST=parsed.hostname,PGPORT=str(parsed.port or 5432),PGUSER=unquote(parsed.username),
             PGPASSWORD=unquote(parsed.password or ""),PGDATABASE=parsed.path[1:],PGCONNECT_TIMEOUT="5",
             PGOPTIONS="-c statement_timeout=15000")
    psql=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
    command=[psql,"-X","-w","-Atq","-v","ON_ERROR_STOP=1"]
    def sql(statement):
        return subprocess.check_output(command,env=env,text=True,input=statement).strip()
    def migrate(file):
        subprocess.run([*command,"--single-transaction","-f",file],env=env,check=True,stdout=subprocess.DEVNULL)

    for file in sorted(MIGRATIONS.glob("*.sql")):
        if int(file.name.split("_",1)[0])<=70:migrate(file)
    sql("""BEGIN; INSERT INTO owners(owner_id) VALUES ('posting-owner');
        INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES('original','original','posting-owner');
        INSERT INTO ledger_entries VALUES ('original','debit',-17),('original','credit',17); COMMIT;""")
    digest_sql="""SELECT md5(string_agg(account||':'||amount,',' ORDER BY account))
                  FROM ledger_entries WHERE transaction_id='original'"""
    historic=sql(digest_sql)
    migrate(MIGRATIONS/"0071_seal_committed_postings.sql")
    def assert_closed(transaction):
        sql(f"""DO $$ DECLARE reason text; BEGIN
            INSERT INTO ledger_entries VALUES('{transaction}','new-debit',-3),('{transaction}','new-credit',3);
            RAISE EXCEPTION 'accepted a later balanced append';
            EXCEPTION WHEN check_violation THEN
                GET STACKED DIAGNOSTICS reason=CONSTRAINT_NAME;
                IF reason IS DISTINCT FROM 'ledger_postings_sealed' THEN RAISE; END IF;
            END $$;""")
    assert_closed("original")
    with subprocess.Popen(command,env=env,text=True,stdin=subprocess.PIPE,stdout=subprocess.PIPE) as writer:
        try:
            writer.stdin.write("""BEGIN;
INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES('fresh','correction:original','posting-owner');
INSERT INTO ledger_entries VALUES('fresh','credit',-17);
\\echo producer-open
""")
            writer.stdin.flush()
            with selectors.DefaultSelector() as ready:
                ready.register(writer.stdout,selectors.EVENT_READ)
                if not ready.select(10) or writer.stdout.readline().strip()!="producer-open":
                    raise RuntimeError("Producer did not reach its uncommitted posting group")
            assert_closed("fresh")
            writer.stdin.write("""SAVEPOINT partial;
INSERT INTO ledger_entries VALUES('fresh','unwanted',99);
ROLLBACK TO partial;
INSERT INTO ledger_entries VALUES('fresh','debit',17);
COMMIT;
""")
            writer.stdin.close()
            if writer.wait(timeout=10):
                raise RuntimeError("Original writer failed to commit its reversal")
        finally:
            if not writer.stdin.closed:writer.stdin.close()
    assert_closed("fresh")
    rejected=subprocess.run(command,env=env,text=True,capture_output=True,input="""BEGIN;
        INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES('bad','bad','posting-owner');
        INSERT INTO ledger_entries VALUES('bad','unbalanced',13); COMMIT;""")
    if not rejected.returncode or "unbalanced ledger transaction" not in rejected.stderr:
        raise RuntimeError("Unbalanced original group was not rejected")
    if (sql(digest_sql)!=historic or sql("SELECT count(*) FROM ledger_entries")!="4"
            or sql("SELECT count(*) FROM ledger_transactions")!="2"
            or sql("SELECT count(*) FROM orange_ledger_internal.open_posting_groups")!="0"):
        raise RuntimeError("Original contents or transaction-scoped assembly markers diverged")
    print("PASS immutable historical and newly committed postings, concurrent foreign writer rejected, "
          "balanced reversal and savepoint accepted, deferred imbalance rolled back")


if __name__ == "__main__":
    parser=argparse.ArgumentParser()
    parser.add_argument("--environment",type=Path,default=ROOT/".local")
    args=parser.parse_args()
    with database(args.environment.resolve()/"database-url","orange_sealed_ledger_") as (_,url):
        check(url)
