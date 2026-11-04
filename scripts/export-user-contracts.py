#!/usr/bin/env python3
"""Copy canonical user contract bytes to an explicit separately versioned Web checkout."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser();parser.add_argument('--web-repo',required=True,type=Path);args=parser.parse_args()
target=args.web_repo.resolve()/'docs/contracts'
if not target.is_dir():raise SystemExit('Existing user Web contract directory required')
files={}
for source,name in [('contracts/json-schema/user.schema.json','user.schema.json'),('contracts/openapi/user.v1.json','user.openapi.json')]:
    raw=(ROOT/source).read_bytes();json.loads(raw);shutil.copyfile(ROOT/source,target/name)
    files[name]={'source':source,'sha256':hashlib.sha256(raw).hexdigest()}
(target/'snapshot.json').write_text(json.dumps({'version':'0.1.0','files':files},indent=2)+'\n')
print('Canonical user contract snapshots exported with SHA256; consumer builds use local copies only')
