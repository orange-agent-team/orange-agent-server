"""Check all four bundled manifests against immutable draft registration and scheduler admission."""
import copy
import json
from pathlib import Path
import subprocess
import uuid

CACHES={"text":"model-runtime","image":"image-model","embedding":"embedding-model","transcription":"transcription-model"}


def check(root, asset_root, tool, write, read, sql, scope_query):
    templates=root/"crates/model-runtime/model-manifests"
    results={}
    for descriptor in sorted(templates.glob("*.json")):
        manifest=json.loads(descriptor.read_bytes())
        modality=manifest["interface"]["modality"]
        if modality not in CACHES or modality in results:
            raise ValueError("Unknown or repeated model class")
        model="matrix-"+modality
        manifest["model_version"]=model
        def measure(value):
            return subprocess.run([tool,"verify-proposed",asset_root/CACHES[modality]],input=json.dumps(value).encode(),
                                  capture_output=True,timeout=480)
        original=measure(manifest)
        original.check_returncode()
        measurement=json.loads(original.stdout)
        assert measurement["model_version"]==model and measurement["verified_files"]==len(manifest["artifacts"])
        assert measurement["verified_bytes"]==manifest["resources"]["artifact_bytes"]
        missing=copy.deepcopy(manifest)
        del missing["artifacts"][0]["sha256"]
        assert write(missing,key=str(uuid.uuid4()))[0]==422
        altered=copy.deepcopy(manifest)
        altered["artifacts"][0]["sha256"]="a"*64 if manifest["artifacts"][0]["sha256"]!="a"*64 else "b"*64
        checked=measure(altered)
        if checked.returncode==0 or b"Integrity" not in checked.stderr and b"integrity" not in checked.stderr.lower():
            raise ValueError("Asset mutation did not produce an integrity failure: "+checked.stderr[:160].decode(errors="replace"))
        code,receipt=write(manifest,key=str(uuid.uuid4()))
        assert code==201 and receipt["manifest_sha256"]==measurement["manifest_sha256"] and receipt["model"]["status"]=="draft"
        assert write(altered,key=str(uuid.uuid4()))[0]==409
        status,stored=read("model-drafts/"+model+scope_query)
        assert status==200 and stored["manifest"]==manifest and stored["manifest_sha256"]==measurement["manifest_sha256"]
        candidate=str(uuid.uuid4())
        blocked=sql(f"""INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest)
                        VALUES('{candidate}','{model}','{manifest["artifacts"][0]["sha256"]}',repeat('c',64))""",must_pass=False)
        if blocked.returncode==0 or "no active matching certificate" not in blocked.stderr:
            raise ValueError("Unexpected execution profile admission")
        assert sql(f"SELECT registry_profile_current('{candidate}'),(SELECT count(*) FROM model_registry_activations WHERE model_version_id='{model}')")=="f|0"
        results[modality]=dict(assets=measurement["verified_files"],bytes=measurement["verified_bytes"],manifest_sha256=measurement["manifest_sha256"])
    assert set(results)==set(CACHES)
    print(json.dumps({"four_modality_admission":results,"missing_digest_refusals":4,"changed_asset_refusals":4,
                      "immutable_manifest_conflicts":4,"unapproved_execution_profile_refusals":4}),flush=True)
    print("PASS four real model caches, immutable draft readback and absent unapproved schedulable profiles",flush=True)
