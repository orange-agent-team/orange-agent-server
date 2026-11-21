#!/usr/bin/env python3
"""Validate the independent PRD-003 product matrix and optionally compare its planning sources."""
import argparse
from collections import Counter
import json
from pathlib import Path
import re

REPO = Path(__file__).resolve().parents[1]
DIRECTORY = REPO / "docs/product-scope"
SNAPSHOT = DIRECTORY / "catalog.json"
FIRST_TOOLS = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 24}
BASELINES = {
    "G3": (24, 6, 26, 8),
    "G4": (48, 16, 80, 20),
    "G5": (64, 32, 144, 32),
    "G6": (80, 48, 208, 40),
}
PROFILE_COUNTS = {"MP": 7, "MC": 5, "OS": 10, "HW": 6, "TR": 6, "EX": 4, "CX": 12}
PROFILE_STEPS = {"MP": "AG-003.04", "MC": "ML-007.04", "OS": "QA-004.04",
                 "HW": "ML-007.04", "TR": "AG-019.04", "EX": "AG-010.04", "CX": "API-006.04"}
PROFILE_SUITES = {"MP": "SS-03", "MC": "SS-03", "OS": "SS-04", "HW": "SS-04",
                  "TR": "SS-06", "EX": "SS-07", "CX": "SS-08"}


def require(ok, message):
    if not ok:
        raise ValueError(message)


def read(path):
    return json.loads(path.read_text())


def rows(path):
    for line in path.read_text().splitlines():
        if line.startswith("|") and line.endswith("|"):
            yield [cell.strip() for cell in line.strip("|").split("|")]


def baseline_task(feature):
    if feature.startswith("CN-"):
        return "AG-013.04", "SS-02"
    if feature.startswith("AU-"):
        return "AG-014.02", "SS-06"
    index = int(feature[3:])
    for upper, step in [(4,"AG-007.01"),(6,"AG-007.02"),(10,"AG-006.02"),(13,"AG-007.03"),
                        (15,"AG-013.04"),(17,"AG-012.04"),(20,"AG-022.04"),
                        (21,"ML-003.03"),(22,"ML-004.04"),(23,"ML-002.04"),(24,"AG-021.02")]:
        if index <= upper:
            return step, "SS-01"
    raise ValueError(feature)


def scope_from_plan(root):
    graph = read(root / "services_plan/execution/scope-graph.json")
    support = root / "plan/support-scope"
    future = root / "plan/future-goals/catalogs"
    descriptions = {}
    for path in [support/"01-tools.md", support/"06-automation.md", future/"tools.md", future/"scenarios.md"]:
        for row in rows(path):
            if re.fullmatch(r"(TK|AU)-\d\d", row[0]):
                descriptions[row[0]] = " · ".join(row[1:])
    connector_specs = {}
    for path in [support/"02-connectors.md",future/"connectors.md"]:
        for row in rows(path):
            if re.fullmatch(r"CN-\d{2}", row[0]):
                connector_specs.setdefault(row[0], []).append(row)
    result = {}
    for key, definition in sorted(graph["features"].items()):
        stage = definition["scope_stage"]
        earliest = stage.split()[0]
        if earliest == "G3" and key.startswith("TK-") and int(key[3:]) in FIRST_TOOLS:
            earliest = "G1"
        if stage == "G3 baseline":
            step, suite = baseline_task(key)
            packages, suites = [step], [suite]
        else:
            packages, suites = definition["package_references"], definition["acceptance"]
        if key.startswith("CN-"):
            connector,action=key.split("/",1)
            specs=connector_specs[connector]
            if stage=="G3 baseline":
                description=specs[0][1]+" · "+action+" · "+specs[0][3]
            elif int(connector[3:])>=17:
                description=specs[-1][1]+" · "+action+" · "+specs[-1][3]
            else:
                description=specs[-1][1]+" · "+action+" · 已授权对象域，撤权后拒绝。写操作验证批准、幂等和结果未知后恢复。"
        else:
            description=descriptions[key]
        result[key] = {"name": definition["name"], "first_gate": earliest,
                       "milestone": "M1" if earliest=="G1" else "M4" if stage=="G3 baseline" else "M5",
                       "work": packages, "acceptance": suites,
                       "specification": description,
                       "evidence_key": "product-scope/v1/"+key}
    profiles = {}
    for path in sorted(support.glob("*.md")):
        for row in rows(path):
            match = re.match(r"^(MP|MC|OS|HW|TR|EX|CX)-(\d{2})(?:\s|$)",row[0])
            if not match:
                continue
            prefix, number=match.groups()
            key=f"{prefix}-{number}"
            stage = row[1].split("/")[0] if prefix in {"MP","OS"} else "G3"
            if (prefix,number) in {("HW","05"),("EX","04")} or (prefix=="TR" and number!="01"):
                stage="G4"
            profiles[key]={"terms":row, "first_gate":stage, "work":[PROFILE_STEPS[prefix]],
                           "acceptance":[PROFILE_SUITES[prefix]], "source":"plan/support-scope/"+path.name}
    role_rows=list(rows(support/"05-team-permissions.md"))
    roles=role_rows[0][1:]
    permissions={r[0]:dict(zip(roles,r[1:])) for r in role_rows[2:] if len(r)==6}
    return {"version":1, "features":result, "profiles":profiles, "team_permissions":permissions}


def check_coverage(data):
    features = data["features"]
    for gate, totals in BASELINES.items():
        subset = [k for k, v in features.items() if v["first_gate"][:2] <= gate]
        cn = [k for k in subset if k.startswith("CN-")]
        counts = (sum(k.startswith("TK-") for k in subset), len({k.split("/")[0] for k in cn}),
                  len(cn), sum(k.startswith("AU-") for k in subset))
        require(counts==totals, f"{gate} expected {totals}, got {counts}")
    require({k for k,v in features.items() if v["first_gate"]=="G1"}=={f"TK-{n:02d}" for n in FIRST_TOOLS},
            "G1 individual tool scope changed")
    keys = set()
    for key,v in features.items():
        require(v["work"] and v["acceptance"] and v["specification"], "missing contract: "+key)
        require(v["evidence_key"]=="product-scope/v1/"+key and v["evidence_key"] not in keys, key)
        keys.add(v["evidence_key"])
    expected_ids={f"TK-{n:02d}" for n in range(1,81)}|{f"AU-{n:02d}" for n in range(1,41)}
    require({k for k in features if not k.startswith("CN-")}==expected_ids,"individual feature IDs")
    require({k.split("/")[0] for k in features if k.startswith("CN-")}=={f"CN-{n:02d}" for n in range(1,49)},"connector identities")
    permissions=data["team_permissions"]
    require(len(permissions)>=10 and all(set(values)=={"owner","admin","member","billing","viewer"} for values in permissions.values()),"role/action coverage")
    observed=Counter(key.split("-")[0] for key in data["profiles"])
    require(observed==Counter(PROFILE_COUNTS), f"profile counts changed: {observed}")
    for key, v in data["profiles"].items():
        require(v["terms"] and v["work"] and v["acceptance"],key)


def check(core, catalog, step, planning_root, manifest=None):
    if step in ("all","entries"):
        scopes={r["id"] for r in core["capabilities"]}
        require(len(scopes)==len(core["capabilities"]) and {"embedding","image","transcription","team","top-up","refund","node-enrollment","local-text","operator","web-agent"} <=scopes,"missing or duplicate capability")
        for r in core["capabilities"]:
            require(len(r["acceptance"])>=15 and r["name"] and re.fullmatch(r"[A-Z]+-\d{3}\.\d{2}",r["owning_step"]),r["id"])
    if step in ("all","milestones"):
        require(set(core["milestones"])=={f"M{n}" for n in range(6)},"M0-M5 coverage")
        launch=core["public_launch"]
        require((launch["milestone"],launch["gate"],launch["registration"],launch["execution_authorized"])==("M4","G3","open_registration",False) and len(launch["conditions"])>=4,"public release boundary")
        require(all(r["milestone"] in core["milestones"] and re.fullmatch("G[0-6]",r["gate"]) for r in core["capabilities"]),"core milestones")
    if step in ("all","exclusions"):
        exclusions={r["id"]:r for r in core["deferred"]}
        require(exclusions["tts"]["disposition"]=="M5" and len(exclusions)==len(core["deferred"]),"TTS stage")
        require(all(exclusions[k]["disposition"]=="research_only" for k in ("cash-withdrawal","credit-transfer","public-worker-code","external-model-upload","distributed-training","chain-settlement")),"research boundary")
        require(not {r["id"] for r in core["capabilities"]}.intersection(exclusions),"deferred capability also in release")
    if step in ("all","coverage"):
        check_coverage(catalog)
        if planning_root:
            require(catalog==scope_from_plan(planning_root),"catalog differs from the approved planning sources")
            graph=read(planning_root/"services_plan/execution/scope-graph.json")
            steps={s for p in graph["original_parents"].values() for s in p["steps"]}
            units={u["id"] for u in read(manifest)["units"]} if manifest else set(graph["execution_nodes"])
            packages=set(graph["package_requirements"])
            for v in list(catalog["features"].values())+list(catalog["profiles"].values()):
                require(set(v["work"])<=steps|packages, str(v["work"]))
            for r in core["capabilities"]:
                require(r["owning_step"] in steps,r["owning_step"])
                require(r["validation_unit"] is None or r["validation_unit"] in units,r["validation_unit"])


if __name__=="__main__":
    parser=argparse.ArgumentParser()
    parser.add_argument("--step",choices=["all","entries","milestones","exclusions","coverage"],default="all")
    parser.add_argument("--planning-root",type=Path)
    parser.add_argument("--execution-manifest",type=Path,help="Explicit staged execution roster when comparing before promotion")
    parser.add_argument("--refresh",action="store_true")
    args=parser.parse_args()
    if args.refresh:
        require(args.planning_root,"catalog regeneration requires explicit planning sources")
        rebuilt=scope_from_plan(args.planning_root)
        check_coverage(rebuilt)
        SNAPSHOT.write_text(json.dumps(rebuilt,indent=2,ensure_ascii=False)+"\n")
    check(read(DIRECTORY/"core.json"), read(SNAPSHOT), args.step, args.planning_root, args.execution_manifest)
    print("PASS PRD-003 "+args.step+" product specification. Product execution and release qualification have independent evidence.")
