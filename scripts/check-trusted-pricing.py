#!/usr/bin/env python3
"""Real fixed counter, current quote authority and isolated credit settlement."""
from contextlib import ExitStack
from owned_postgres import database
import json
import os
from pathlib import Path
import subprocess
ROOT=Path(__file__).resolve().parents[1]
schema=json.loads((ROOT/'contracts/json-schema/pricing.schema.json').read_text())
api=json.loads((ROOT/'contracts/openapi/pricing.v1.json').read_text())
assert api['components']['schemas']==schema['$defs']
assert {'input_price','output_price','input_units','charge'}.isdisjoint(schema['$defs']['CreateTextQuote']['properties'])
models = json.loads((ROOT/'contracts/json-schema/models.schema.json').read_bytes())
catalog = json.loads((ROOT/'contracts/openapi/models.v1.json').read_bytes())
assert models["$defs"] == json.loads(json.dumps(catalog["components"]["schemas"]).replace("#/components/schemas/","#/$defs/"))
assert all(models["$defs"][k] == schema["$defs"][k] for k in ("TextPriceView","ApiError"))
env=os.environ.copy()
env.pop('ORANGE_WEB_COMPUTE_TESTS',None)
env.pop('ORANGE_WITHDRAW_TESTS',None)
env.pop('ORANGE_QUEUE_EXPIRY_TESTS',None)
for flag in ['ORANGE_JOB_QUERY_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS']:env.pop(flag,None)
env['ORANGE_PRICED_WORKER_TESTS']='1';env.pop('ORANGE_WORKER_HOST_TESTS',None)
with ExitStack() as fixtures:
    if not env.get("ORANGE_TEST_DATABASE_URL"):
        _,url=fixtures.enter_context(database(Path(env.get("ORANGE_TEST_DATABASE_URL_FILE",ROOT/".local/database-url")),"orange_trusted_pricing_"))
        env["ORANGE_TEST_DATABASE_URL"]=url
    subprocess.run([os.sys.executable,'scripts/check-real-worker.py'],cwd=ROOT,env=env,check=True)
print('PASS trusted-pricing contracts and actual counter/PG/HTTP/mTLS/development-ledger flow; no real funds or production approval certification')
