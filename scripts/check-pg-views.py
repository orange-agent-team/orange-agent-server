#!/usr/bin/env python3
"""Exercise CN-05 against a disposable source and a dedicated PostgreSQL login."""
import argparse
import os
import json
from pathlib import Path
import secrets
import shutil
import subprocess
from urllib.parse import urlsplit, urlunsplit, unquote
import uuid
from owned_postgres import database
from connector_browser_fixture import prepare_browser

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--database-url-file", type=Path, required=True)
parser.add_argument("--identity-config", required=True, type=Path)
parser.add_argument("--web-repo", type=Path)
args = parser.parse_args()
browser_env=prepare_browser(args.web_repo,root,"CN05")
role = "cn05_" + uuid.uuid4().hex
password = secrets.token_hex(32)
psql = shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
base = urlsplit(args.database_url_file.read_text().strip())
if base.hostname not in ("127.0.0.1", "localhost", "::1"):
    raise SystemExit("Owned loopback database required")
env = dict(os.environ, PGHOST=base.hostname, PGPORT=str(base.port or 5432),
           PGUSER=unquote(base.username or ""), PGPASSWORD=unquote(base.password or ""), PGCONNECT_TIMEOUT="5")

def sql(db, statement):
    subprocess.run([psql, "-X", "-q", "-w", "-v", "ON_ERROR_STOP=1"], input=statement,
                   text=True, env=dict(env, PGDATABASE=db), check=True)

sql(base.path[1:], f"CREATE ROLE {role} LOGIN PASSWORD '{password}'")
try:
    with database(args.database_url_file, "orange_views_") as (name, url):
        sql(name, f"""
            REVOKE ALL ON DATABASE "{name}" FROM PUBLIC;
            GRANT CONNECT ON DATABASE "{name}" TO {role};
            CREATE SCHEMA source;
            CREATE TABLE source.orders(seq bigint PRIMARY KEY, name text, internal_note text);
            INSERT INTO source.orders VALUES (10,'橙子','private'),(20,'pear','private');
            CREATE VIEW source.sales AS SELECT seq,name FROM source.orders;
            GRANT USAGE ON SCHEMA source TO {role};
            GRANT SELECT ON source.sales TO {role};
        """)
        reader = urlunsplit(base._replace(netloc=f"{role}:{password}@127.0.0.1:{base.port or 5432}",path="/"+name,query="",fragment=""))
        rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
        cargo = Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
        subprocess.run([cargo,"test","--locked","--offline","-p","orange-control-plane","--features","pg-view-tests","--test","pg_views"],
                       cwd=root,env=dict(os.environ, PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],
                                         ORANGE_VIEW_FIXTURE_ADMIN=url, ORANGE_VIEW_FIXTURE_READER=reader),check=True)
        sql(name, f"GRANT SELECT ON source.sales TO {role}; UPDATE source.orders SET name='橙子' WHERE seq=10")
        registration = {"database": name, "read_role": role, "views": {"sales": {"schema": "source", "relation": "sales", "columns": {"name": "text", "seq":"int8"}, "paging_key":"seq"}}}
        source = {"source_id": "sales-db", "address":"127.0.0.1", "port":base.port or 5432, "registration": registration, "password":password}
        with database(args.database_url_file, "orange_view_app_") as (_, platform):
            subprocess.run([cargo,"test","--locked","--offline","-p","orange-control-plane","--features","pg-view-tests,integration-tests","--test","user_identity","pg_consent::scoped_source_reads"],
                cwd=root,env=dict(os.environ, PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"], ORANGE_TEST_DATABASE_URL=platform,
                **browser_env, ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()), ORANGE_VIEW_FIXTURE_ADMIN=url,ORANGE_VIEW_FIXTURE_CONFIG=json.dumps(source)),check=True)

finally:
    sql(base.path[1:], f"DROP ROLE {role}")
