#!/usr/bin/env python3
"""Run the loopback user identity/organization API with private local configuration."""
import os
from pathlib import Path
from urllib.parse import urlparse
ROOT=Path(__file__).resolve().parents[1]
url=(ROOT/'.local/database-url').read_text().strip()
parsed=urlparse(url)
if parsed.hostname!='127.0.0.1' or parsed.port!=55432 or parsed.path!='/orange_dev':
    raise SystemExit('Dedicated development database required')
environment=os.environ.copy();environment.update(ORANGE_ENV='development',DATABASE_URL=url,
    ORANGE_USER_CONFIG_FILE=str(ROOT/'.local/identity/user-config.json'))
os.execve(str(ROOT/'target/debug/orange-user-api'),['orange-user-api'],environment)
