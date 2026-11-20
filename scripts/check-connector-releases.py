#!/usr/bin/env python3
"""Check the six pinned action contracts and the exact immutable database release seed."""
import hashlib
import json
from connector_catalog.policy import ROOT, FILE, MIGRATION, derive, sql_entries

catalog=json.loads(FILE.read_text())
adapters=catalog["adapters"]
expected={"github":6,"gitlab":6,"notion":4,"confluence":4,"postgres":3,"s3":3}
if {a["provider"]:len(a["actions"]) for a in adapters}!=expected or len(adapters)!=6:
    raise ValueError("G3 connector scope differs")
names={}
pins=set()
for a in adapters:
    doc=(ROOT/a["schema_file"]).read_bytes()
    if hashlib.sha256(doc).hexdigest()!=a["schema_sha256"]:raise ValueError("Schema differs: "+a["provider"])
    api=(ROOT/a["openapi_file"]).read_bytes()
    if hashlib.sha256(api).hexdigest()!=a["openapi_sha256"]:raise ValueError("OpenAPI differs: "+a["provider"])
    defs=json.loads(doc)["$defs"]
    actions=set()
    for item in a["actions"]:
        pin=derive(a,item,defs)
        if item["name"] in actions or pin in pins or pin!=item["execution_digest"]:
            raise ValueError("Invalid action policy pin")
        actions.add(item["name"]);pins.add(pin)
    names.setdefault(a["schema_file"],set()).update(actions)
for path,dispatched in names.items():
    defs=json.loads((ROOT/path).read_text())["$defs"]
    wire=set()
    for item in defs["Action"]["oneOf"]:
        op=item["properties"]["action"]
        wire.update(op["enum"] if "enum" in op else [op["const"]])
    if wire!=dispatched:raise ValueError("Unaccounted connector actions in "+path)
migration=MIGRATION.read_text().split("-- BEGIN RELEASE SEED\n")[1].split("\n-- END RELEASE SEED")[0]
if migration!=sql_entries(catalog):raise ValueError("Migration and compiled release catalog differ")
print("PASS six versioned provider profiles, 26 distinct read-only action pins, canonical contracts and database seed")
