#!/usr/bin/env python3
"""Explicitly export canonical contract bytes into a separately versioned Admin checkout."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--admin-repo', required=True, type=Path)
args = parser.parse_args()
target = args.admin_repo.resolve()/'docs/contracts'
if not target.is_dir():
    raise SystemExit('Existing Admin contract directory required')
pairs = [('contracts/openapi/admin.v1.json', 'admin.openapi.json'),
         ('contracts/json-schema/admin.schema.json', 'admin.schema.json')]
hashes = {}
for source, name in pairs:
    data = (ROOT/source).read_bytes()
    json.loads(data)
    shutil.copyfile(ROOT/source, target/name)
    hashes[name] = {'source': source, 'sha256': hashlib.sha256(data).hexdigest()}
(target/'snapshot.json').write_text(json.dumps({'openapi_version': json.loads((ROOT/'contracts/openapi/admin.v1.json').read_text())['info']['version'], 'files': hashes}, indent=2)+'\n')
print('Canonical Admin contracts exported with exact SHA-256; commit snapshot with consumer changes')
