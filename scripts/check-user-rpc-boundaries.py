#!/usr/bin/env python3.12
"""Verify producer RPC boundaries, actual fixed client projections, limits and event wire semantics."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT=Path(__file__).resolve().parents[1]


def read(path):return json.loads(path.read_bytes())


def require(test,message):
    if not test:raise ValueError(message)


def main():
    options=argparse.ArgumentParser()
    options.add_argument("--web-repo",type=Path,required=True)
    options.add_argument("--desktop-repo",type=Path,required=True)
    args=options.parse_args();web=args.web_repo.resolve();desktop=args.desktop_repo.resolve()
    policy=read(ROOT/"contracts/registry/user-rpc-authority.v1.json")
    owners=policy["authorities"];required_headers={v.lower() for v in policy["browser_write_singletons"]}
    require(policy["version"]==1 and required_headers=={"origin","x-csrf-token","idempotency-key"},"Unexpected mutation authority")
    specs={};found={};desktop_paths={}
    for group,binding in owners.items():
        family=binding["family"]
        if family not in specs:specs[family]=read(ROOT/"contracts/openapi"/(family+".v1.json"))
        spec=specs[family];by_id={op["operationId"]:(path,m,op) for path,item in spec["paths"].items() for m,op in item.items()}
        require(len(by_id)==sum(len(x) for x in spec["paths"].values()),"Duplicate operation in "+family)
        for operation in binding["operations"]:
            require(operation in by_id and operation not in found,"Missing or reused operation "+operation)
            path,method,entry=by_id[operation];found[operation]=(family,path,method)
            security=entry.get("security",spec.get("security"))
            require(security and len(security)==len(security[0])==1,"Ambiguous principal for "+operation)
            scheme=next(iter(security[0]));auth=spec["components"]["securitySchemes"][scheme]
            if "frb" in binding:
                require(method=="get" and auth.get("scheme")=="bearer" and path.startswith("/api/v1/automation/account/"),"Unexpected automation privilege")
                desktop_paths[path]=spec["paths"][path]
            else:
                require(auth.get("in")=="cookie" and auth.get("name")==policy["browser_cookie"],"Wrong browser principal")
            if "{owner}" in path:
                require(any(p.get("name")=="owner" and p.get("required") and p.get("in")=="path" for p in entry["parameters"]),"Unbound owner")
            if method in {"post","put","delete"}:
                headers={p["name"].lower() for p in entry["parameters"] if p.get("in")=="header" and p.get("required")}
                require(required_headers<=headers,"Missing write protection: "+operation)
            require(any(c.startswith("2") or c=="101" for c in entry["responses"]),"No success contract")
    for family in ("models","model-manifests","artifacts","pricing"):
        all_ops={op["operationId"] for item in specs[family]["paths"].values() for op in item.values()}
        require(all_ops=={op for op,(f,_,_) in found.items() if f==family},"Unclassified market operation in "+family)
    for family in ("models","artifacts","pricing","account"):
        require(specs[family]==read(web/"docs/contracts"/(family+".openapi.json")),"Web operation drift for "+family)
        source=ROOT/"contracts/json-schema"/(family+".schema.json")
        snap=web/"docs/contracts"/(family+".schema.json")
        require(hashlib.sha256(source.read_bytes()).digest()==hashlib.sha256(snap.read_bytes()).digest(),"Web schema drift for "+family)
    # Verify against the original producer commit as well as the current browser v1 contract.
    subprocess.run([sys.executable,desktop/"scripts/verify-personal-account-contract.py","--server-repo",ROOT],check=True)
    require(read(desktop/"contracts/personal-account/read-operations.json")["paths"]==desktop_paths,"Desktop bearer surface changed")
    surface=read(desktop/"config/bridge-tools.lock.json")["additional_public_modules"]["account"]["functions"]
    require(set(surface)=={"personalApiEndpoint",owners["wallet_key"]["frb"],owners["budget_key"]["frb"]},"Unexpected desktop account mutation")
    specs_by_family={family:data["components"].get("schemas",{}) for family,data in specs.items()}
    a=specs_by_family["artifacts"]
    def operation(name):
        family,path,method=found[name];return specs[family]["paths"][path][method]
    quote=specs_by_family["pricing"]["CreateTextQuote"]
    require(quote["additionalProperties"] is False and quote["properties"]["output_limit"]=={"type":"integer","minimum":1,"maximum":4096},"Unbounded quote output")
    binary=operation("putArtifactBytes")["requestBody"]["content"]
    require(set(binary)=={"application/octet-stream"} and binary["application/octet-stream"]["schema"]["maxLength"]==104857600,"Missing independent byte channel")
    require(a["CancelJob"]["additionalProperties"] is False and not a["CancelJob"].get("properties"),"Cancellation admits extra effects")
    cancel=operation("requestOriginatingUserJobCancellation")
    require("412" in cancel["responses"] and any(p.get("name")=="x-orange-expected-job-version" for p in cancel["parameters"]),"Missing stale-revision response")
    for field in ("gift","purchased","earned","reserved_gift","reserved_earned","reserved_purchased","pending_earned"):
        require(specs_by_family["account"]["WalletView"]["properties"][field]["type"]=="string","Lossy wallet amount")
    websocket=operation("streamOwnedJobHistory")["x-orange-websocket"]
    require(websocket=={"protocol":"orange.job-events.v1","delivery":"JobStreamFrame","ack":"JobStreamAck","ack_timeout_ms":5000,
                        "max_inbound_message_bytes":2048,"max_batch_bytes":65536,"max_concurrent_sockets_per_router":64,"max_concurrent_sockets_per_user":4},"Changed connection bounds")
    require(set(a["JobDurableEvent"]["properties"])=={"event_id","seq","event_type","attempt_id","occurred_at","payload_digest"} and a["JobDurableEvent"]["additionalProperties"] is False,"Sensitive or missing event fields")
    require(a["JobEventReset"]["properties"]["kind"]=={"const":"reset"} and a["JobEventPage"]["properties"]["kind"]=={"const":"page"} and a["JobEventSnapshot"]["properties"]["kind"]=={"const":"snapshot"},"Ambiguous reset or snapshot")
    print(json.dumps({"operation_count":len(found),"owner_categories":len(owners),"protected_browser_write_headers":sorted(required_headers),
                      "desktop_scopes":[owners["wallet_key"]["scope"],owners["budget_key"]["scope"]],"web_contract_families":4,"socket_protocol":websocket["protocol"]}))
    print("PASS canonical user RPC request, response, ownership, client projections and event boundaries")


if __name__=="__main__":main()
