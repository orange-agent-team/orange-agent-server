#!/usr/bin/env python3
"""Exercise two real API binaries with the existing private development IdP and Chrome."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
from urllib.parse import unquote, urlsplit

from acceptance_origins import reserve_origins
from owned_postgres import database
from platform_package_smoke import listening
from verified_platform_archive import binaries

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--environment", type=Path, default=ROOT / ".local")
    parser.add_argument("--web-repo", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, help="Development binaries; default extracts this commit’s verified platform archive")
    parser.add_argument("--node", default=shutil.which("node"))
    args = parser.parse_args()
    private = args.environment.resolve(strict=True)
    web = args.web_repo.resolve(strict=True)
    if subprocess.check_output([args.node, "--version"], text=True).strip() != "v22.23.2":
        raise ValueError("Node 22.23.2 required")
    realm = private / "identity"
    fixture = realm / "fixture.json"
    sample = json.loads(fixture.read_bytes())["users"][0]
    api_configs = [realm / f"{role}-config.json" for role in ("user", "admin")]
    for role, profile, port in zip(("user", "admin"), api_configs, (43122, 43112), strict=True):
        cfg = json.loads(profile.read_bytes())
        if (cfg["issuer"] != "http://127.0.0.1:44180/realms/orange-dev" or cfg["environment"] != "dev"
                or cfg[role+"_origin"] != f"https://127.0.0.1:{port}" or cfg["client_id"] != "orange-"+role):
            raise ValueError("Dedicated local realm and clients required")
    reserve_origins(43112,43122)
    with binaries(args.bin_dir) as bindir, database(private / "database-url", "orange_session_login_") as (_, url):
        p = urlsplit(url)
        connection = dict(PATH="/usr/bin:/bin", PGHOST=p.hostname, PGPORT=str(p.port), PGUSER=unquote(p.username),
                          PGPASSWORD=unquote(p.password), PGDATABASE=p.path[1:])
        runtime = dict(PATH="/usr/bin:/bin", DATABASE_URL=url, ORANGE_ENV="development")
        with listening(bindir/"orange-user-api", dict(runtime, ORANGE_USER_BIND="127.0.0.1:0",
                       ORANGE_USER_CONFIG_FILE=str(api_configs[0])), "User API") as user, \
             listening(bindir/"orange-admin-api", dict(runtime, ORANGE_ADMIN_BIND="127.0.0.1:0",
                       ORANGE_ADMIN_CONFIG_FILE=str(api_configs[1])), "Admin API") as admin:
            sql = """BEGIN;
INSERT INTO admin_principals(principal_id,issuer,subject) VALUES(:'principal','http://127.0.0.1:44180/realms/orange-dev',:'subject');
INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at)
VALUES(:'principal',:'role','dev','platform','platform',floor(extract(epoch FROM clock_timestamp()))::bigint+600);
COMMIT;"""
            psql = shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
            subprocess.run([psql,"-X","-q","-w","-v","ON_ERROR_STOP=1","-v","principal="+sample["principal_id"],
                            "-v","subject="+sample["subject"],"-v","role="+sample["role"]],
                           env=connection,input=sql,text=True,check=True)
            payload = dict(user=user,admin=admin,webPackage=str(web/"apps/web/package.json"),
                           tls=str(web/".local/tls"),fixture=str(fixture))
            # Browser error text can contain an authorization URL. Keep private input/URLs out of reports.
            run = subprocess.run([args.node,ROOT/"scripts/session_login_browser.mjs"],input=json.dumps(payload),
                                 text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180)
            for role in ("user","admin"):
                mark = "PASS live Keycloak "+role+" login and logout"
                if mark not in run.stdout:
                    raise RuntimeError("Real "+role+" identity acceptance did not complete")
                print(mark, flush=True)
            if run.returncode != 0:
                raise RuntimeError("Browser acceptance did not exit cleanly")
    print("PASS two actual API processes, new PostgreSQL, isolated browser sessions and required Admin MFA")


if __name__ == "__main__":
    main()
