#!/usr/bin/env python3
"""Validate published artifact JSON contract structure and decimal boundaries."""
import hashlib
import json
from pathlib import Path
import random
import re
ROOT=Path(__file__).resolve().parents[1]
schema=json.loads((ROOT/'contracts/json-schema/artifacts.schema.json').read_text())
api=json.loads((ROOT/'contracts/openapi/artifacts.v1.json').read_text())
example=json.loads((ROOT/'contracts/examples/artifact-create.json').read_text())
assert api['components']['schemas']==schema['$defs']
assert example['sha256']==hashlib.sha256(b'hello').hexdigest() and example['size_bytes']=='5'
for limit,pattern in [(104857600,schema['$defs']['CreateUpload']['properties']['size_bytes']['pattern']),(100000000,schema['$defs']['CreateUpload']['allOf'][0]['then']['properties']['size_bytes']['pattern'])]:
    values=[0,1,99999999,100000000,104000000,104799999,104800000,104849999,104850000,104856999,104857000,104857599,104857600,104857601,999999999]
    values.extend(random.Random(41).sample(range(0,200000000),1000))
    for n in values:assert bool(re.fullmatch(pattern,str(n)))==(n<=limit),(n,limit)
    for invalid in ['01','-1','1e9','1.0',' 1','']:assert re.fullmatch(pattern,invalid) is None
for path,methods in api['paths'].items():
    for method,operation in methods.items():
        assert operation['security']==[{'UserSession':[]}]
        if method in ['post','put','delete']:assert {'Origin','X-CSRF-Token','Idempotency-Key'} <= {p['name'] for p in operation['parameters']}
print('PASS artifact contract structure, exact decimal bounds and example digest; not HTTP/auth certification')
