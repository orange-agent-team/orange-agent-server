#!/usr/bin/env python3
"""Real fixed counter, current quote authority and isolated credit settlement."""
import json
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
schema=json.loads((ROOT/'contracts/json-schema/pricing.schema.json').read_text())
api=json.loads((ROOT/'contracts/openapi/pricing.v1.json').read_text())
assert api['components']['schemas']==schema['$defs']
assert {'input_price','output_price','input_units','charge'}.isdisjoint(schema['$defs']['CreateTextQuote']['properties'])
env=os.environ.copy()
for flag in ['ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS']:env.pop(flag,None)
env['ORANGE_PRICED_WORKER_TESTS']='1';env.pop('ORANGE_WORKER_HOST_TESTS',None)
subprocess.run([os.sys.executable,'scripts/check-real-worker.py'],cwd=ROOT,env=env,check=True)
print('PASS trusted-pricing contracts and actual counter/PG/HTTP/mTLS/development-ledger flow; no real funds or production approval certification')
