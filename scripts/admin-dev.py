#!/usr/bin/env python3
"""Run the development Admin API with explicitly provisioned local test identities."""
import argparse
from admin_fixture_roles import provision
import os
from pathlib import Path
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[1]
def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['provision', 'serve'])
    action = parser.parse_args().action
    url = (ROOT/'.local/database-url').read_text().strip()
    parsed = urlparse(url)
    if parsed.hostname != '127.0.0.1' or parsed.port != 55432 or parsed.path != '/orange_dev':
        raise SystemExit('Local fixture requires dedicated development database')
    env = os.environ.copy()
    if action == 'serve':
        env.update(ORANGE_ENV='development', DATABASE_URL=url,
                   ORANGE_ADMIN_CONFIG_FILE=str(ROOT/'.local/identity/admin-config.json'))
        os.execve(str(ROOT/'target/debug/orange-admin-api'), ['orange-admin-api'], env)
    provision(parsed,ROOT/'.local/identity/fixture.json')
if __name__ == '__main__':
    main()
