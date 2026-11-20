"""Canonical execution-pin derivation for the G3 connector release."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FILE = ROOT / "config/connector-releases.v1.json"
MIGRATION = ROOT / "services/control-plane/migrations/0050_connector_grant_release_pins.sql"

def digest(value):
    return hashlib.sha256(json.dumps(value,ensure_ascii=False,separators=(",",":"),sort_keys=True).encode()).hexdigest()

def expand(node, definitions):
    if isinstance(node,dict):
        if "$ref" in node:
            if set(node)!={"$ref"} or not node["$ref"].startswith("#/$defs/"):
                raise ValueError("Only local connector schema references are allowed")
            return expand(definitions[node["$ref"][len("#/$defs/"):]],definitions)
        return {key:expand(v,definitions) for key,v in node.items()}
    if isinstance(node,list):
        return [expand(v,definitions) for v in node]
    return node

def derive(adapter,action,definitions):
    name=action["name"]
    selected=[]
    for variant in definitions["Action"]["oneOf"]:
        tag=variant["properties"]["action"]
        if name==tag.get("const") or name in tag.get("enum",[]):
            shape=expand(variant,definitions)
            shape["properties"]["action"]={"const":name}
            selected.append(shape)
    if len(selected)!=1:raise ValueError("Action must name exactly one contract variant: "+name)
    output=action["output"]
    if output.startswith("pg."):
        alternatives=definitions["Output"]["properties"]["data"]["oneOf"]
        selector=output.split(".")[1]
        output_shape=next(v for v in alternatives if selector in v["properties"])
    elif output.startswith("Page:"):
        output_shape=expand(definitions["Page"],definitions)
        output_shape["properties"]["data"]["items"]=expand(definitions[output.split(":")[1]],definitions)
    else:output_shape=definitions[output]
    return digest({"domain":"orange.connector.action.v1","provider":adapter["provider"],"action":name,
        "upstream_effect":"read_only","local_effect":action["local_effect"],"scope_rule":adapter["scope_rule"],
        "input":selected[0],"output":expand(output_shape,definitions)})

def sql_entries(catalog):
    lines=[]
    # Fixed identifiers and digests only; untrusted response content never enters migrations.
    def literal(s):
        return "'"+s.replace("'","''")+"'"
    for a in catalog["adapters"]:
        pins={action["name"]:action["execution_digest"] for action in a["actions"]}
        values=[a["provider"],a["adapter_version"],a["api_revision"]]
        lines.append("INSERT INTO connector_releases(provider,release_id,api_revision,allowed_actions) VALUES("+
                     ",".join(map(literal,values))+","+literal(json.dumps(pins,sort_keys=True,separators=(",",":")))+"::jsonb);")
    return "\n".join(lines)
