#!/usr/bin/env python3
"""Upgrade existing postings, assemble a concurrent group and check immutable committed contents."""
import argparse
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import uuid
from urllib.parse import unquote,urlsplit

from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
MIGRATIONS=ROOT/"services/control-plane/migrations"


def check(url, restored_url):
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
    # Exercise the triggers as a plain writer. The role and every test grant
    # disappear when this single fixture transaction rolls back.
    role="orange_posting_writer_"+uuid.uuid4().hex
    sql(f"""BEGIN; CREATE ROLE {role} NOLOGIN;
        GRANT USAGE ON SCHEMA public TO {role};
        GRANT SELECT,INSERT ON ledger_transactions,ledger_entries TO {role};
        SET LOCAL ROLE {role};
        DO $$ BEGIN
            INSERT INTO orange_ledger_internal.open_posting_groups VALUES('original',pg_current_xact_id());
            RAISE EXCEPTION 'writer forged internal posting authority';
            EXCEPTION WHEN insufficient_privilege THEN NULL; END $$;
        INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES('role-group','role-group','posting-owner');
        INSERT INTO ledger_entries VALUES('role-group','debit',-1),('role-group','credit',1);
        SET CONSTRAINTS ALL IMMEDIATE;
        ROLLBACK;""")

    restore_name=urlsplit(restored_url).path[1:]
    if not restore_name.startswith("orange_seal_restore_"):
        raise ValueError("Owned empty restore database required")
    with tempfile.TemporaryDirectory() as folder:
        archive=Path(folder)/"development.dump"
        pg_dump=shutil.which("pg_dump") or "/opt/homebrew/opt/postgresql@17/bin/pg_dump"
        pg_restore=shutil.which("pg_restore") or "/opt/homebrew/opt/postgresql@17/bin/pg_restore"
        subprocess.run([pg_dump,"--no-password","--format=custom","--file",archive],env=env,check=True)
        subprocess.run([pg_restore,"--exit-on-error","--single-transaction","--no-owner","--no-password",
                        "--dbname",restore_name,archive],env=env,check=True)
    env["PGDATABASE"]=restore_name
    if sql(digest_sql)!=historic or sql("SELECT count(*) FROM ledger_entries")!="4":
        raise ValueError("Restored balances differ from original postings")
    assert_closed("original")
    assert_closed("fresh")
    print("PASS complete PostgreSQL dump/restore preserves journal, seals and original amounts; "
          "plain application writer can post without access to the private assembly table")
    print("PASS immutable historical and newly committed postings, concurrent foreign writer rejected, "
          "balanced reversal and savepoint accepted, deferred imbalance rolled back")


if __name__ == "__main__":
    parser=argparse.ArgumentParser()
    parser.add_argument("--environment",type=Path,default=ROOT/".local")
    args=parser.parse_args()
    url_file=args.environment.resolve()/"database-url"
    with database(url_file,"orange_sealed_ledger_") as (_,url), database(url_file,"orange_seal_restore_") as (_,restored):
        check(url,restored)
