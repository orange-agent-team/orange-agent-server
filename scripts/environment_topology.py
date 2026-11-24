"""Render and validate logical environment names. No credentials, provider APIs or provisioning."""
import re

FIELDS = {"stage", "identity", "database", "object_prefix", "domain", "provision", "database_role",
          "identity_container", "account_scope"}
LOCAL = ("development", "test")
EXTERNAL = ("staging", "production")
LABEL = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
DATABASE = re.compile(r"[a-z][a-z0-9_]{0,62}")


def resolve_template(template, *, region=None, run=None):
    row = {}
    allowed = {}
    if region is not None:
        allowed["{region}"] = region
    if run is not None:
        allowed["{run}"] = run
    for key, value in template.items():
        if value is None and key == "identity_container":
            row[key] = None
            continue
        if not isinstance(value, str) or not value:
            raise ValueError("Invalid environment field: " + key)
        for token, replacement in allowed.items():
            value = value.replace(token, replacement)
        if "{" in value or "}" in value:
            raise ValueError("Unresolved or out-of-scope name in " + key)
        row[key] = value
    if not DATABASE.fullmatch(row["database"]) or not DATABASE.fullmatch(row["database_role"]):
        raise ValueError("Invalid PostgreSQL database or role")
    for key in ("stage","identity","account_scope","identity_container"):
        if row[key] is not None and (len(row[key])>63 or not LABEL.fullmatch(row[key])):
            raise ValueError("Invalid logical identity in " + key)
    prefix=row["object_prefix"]
    if not prefix.endswith("/") or any(not LABEL.fullmatch(part) for part in prefix[:-1].split("/")):
        raise ValueError("Object prefix must have unambiguous path components")
    return row


def topology(templates, region_config, run):
    if not isinstance(run,str) or len(run)>63 or not LABEL.fullmatch(run):
        raise ValueError("Use a lowercase DNS label for the test run")
    if set(templates)!=set(LOCAL+EXTERNAL) or any(not isinstance(row,dict) or set(row)!=FIELDS for row in templates.values()):
        raise ValueError("Incomplete environment catalogue")
    if not isinstance(region_config,dict) or set(region_config)!={"version","regions"} or type(region_config["version"]) is not int or region_config["version"]!=1:
        raise ValueError("Unknown region catalogue")
    regions=region_config["regions"]
    if not isinstance(regions,list) or not regions or any(not isinstance(r,str) or len(r)>63 or not LABEL.fullmatch(r) for r in regions) or len(set(regions))!=len(regions):
        raise ValueError("Distinct service region labels are required")
    local={}
    for stage in LOCAL:
        row=resolve_template(templates[stage],run=run if stage=="test" else None)
        if (row["stage"]!=("dev" if stage=="development" else "test") or row["domain"]!="127.0.0.1" or
            row["database"]!="orange_dev" or row["database_role"]!="orange_local" or row["identity"]!="orange-dev" or not row["identity_container"]):
            raise ValueError("Local fixture settings escape the development namespace")
        if (stage=="development" and row["provision"]!="local") or (stage=="test" and row["provision"]!="disposable isolated cluster and identity service per run"):
            raise ValueError("Wrong local provisioning mode")
        local[stage]=row
    for name in ("identity_container","account_scope","object_prefix"):
        if "{run}" not in templates["test"][name] or local["development"][name]==local["test"][name]:
            raise ValueError("Missing per-run test isolation for "+name)
    deployments=[]
    for stage in EXTERNAL:
        for region in regions:
            row=resolve_template(templates[stage],region=region)
            labels=row["domain"].split(".")
            if (row["stage"]!=("prod" if stage=="production" else stage) or row["provision"]!="external" or
                row["identity_container"] is not None or len(labels)<2 or labels[-1]!="invalid" or len(row["domain"])>253 or
                any(len(label)>63 or not LABEL.fullmatch(label) for label in labels)):
                raise ValueError("Deployment names require a distinct reserved domain and external provisioning")
            deployments.append({"environment":stage,"region":region,**row})
    for key in ("domain","identity","database","database_role","account_scope"):
        names=[row[key] for row in deployments]
        if len(set(names))!=len(names) or set(names)&{row[key] for row in local.values()}:
            raise ValueError("Overlapping environment or region in "+key)
    prefixes=[row["object_prefix"] for row in [*local.values(),*deployments]]
    for i,prefix in enumerate(prefixes):
        if any(prefix.startswith(other) or other.startswith(prefix) for other in prefixes[i+1:]):
            raise ValueError("Overlapping object namespaces")
    return {"test_run":run,"default_rehearsal":"test","local":local,"external_name_reservations":deployments}
