"""Match the packaged CUDA model proposal to verified local weights and a real Admin draft."""
import copy
import json
from pathlib import Path
import subprocess
import uuid

from linux_platform_inputs import local_image_id


def require(ok,message):
    if not ok:raise ValueError(message)


def check(root,assets,operator,post,read,sql,scope):
    directory=root/"distribution/sdxl-linux"
    manifest=json.loads((directory/"model-version.json").read_bytes())
    authority=json.loads((directory/"qualification.json").read_bytes())
    model=manifest["model_version"]
    require(manifest["pricing"]=={"policy":"local_unpriced"} and manifest["resources"]["device"]=="nvidia_gpu" and
            manifest["runtime"]=={"engine":"diffusers","image_digest":authority["image"]},"Model runtime binding differs")
    engine=["docker","--context","colima-orange-agent"]
    image=local_image_id(engine,{"image":"orange-dev-input/sdxl-cuda@"+authority["image"],"config_digest":authority["config_digest"],"platform":"linux/amd64"})
    require(image,"Qualified offline OCI image is not imported")
    described=json.loads(subprocess.check_output([*engine,"run","--rm","--pull=never","--network","none","--read-only","--platform","linux/amd64",
            "--security-opt","no-new-privileges","--cap-drop","ALL","--user","65532:65532",image,"describe"],timeout=30))
    spec=manifest["interface"];actual=described["template"]
    require(described["model_version"]==model and described["weight_lock_sha256"]==manifest["source"]["asset_lock_sha256"] and
            actual["device"]=="cuda" and actual["images"]==1 and all(spec[key]==actual[key] for key in ("width","height","steps")),
            "Installed executable and proposed model differ")
    raw=json.dumps(manifest).encode()
    measured=json.loads(subprocess.check_output([operator,"verify-proposed",assets/"image-model"],input=raw,timeout=480))
    require(measured["verified_files"]==len(manifest["artifacts"]) and measured["verified_bytes"]==manifest["resources"]["artifact_bytes"],"Actual weight set differs")
    req=str(uuid.uuid4())
    status,receipt=post(manifest,key=req)
    require(status==201 and receipt["model"]["status"]=="draft" and receipt["manifest_sha256"]==measured["manifest_sha256"],"Admin draft differs from measured bytes")
    require(post(manifest,key=req)==(200,receipt),"Draft replay changed identity")
    status,readback=read("model-drafts/"+model+scope)
    require(status==200 and readback["manifest"]==manifest,"Unable to read the stored proposal")
    changed=copy.deepcopy(manifest)
    changed["runtime"]["image_digest"]="sha256:"+"1"*64
    require(post(changed,key=str(uuid.uuid4()))[0]==409,"Published draft identity could be changed")
    attempted=str(uuid.uuid4())
    denied=sql(f"""INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest)
                VALUES('{attempted}','{model}','{manifest["artifacts"][0]["sha256"]}',repeat('c',64))""",must_pass=False)
    require(denied.returncode!=0 and "no active matching certificate" in denied.stderr,"Uncertified version became schedulable")
    require(sql(f"SELECT registry_profile_current('{attempted}'),(SELECT count(*) FROM model_registry_activations WHERE model_version_id='{model}')")=="f|0","Draft received an activation")
    print(json.dumps({"sdxl_oci_draft":model,"manifest_sha256":measured["manifest_sha256"],"oci_image":authority["image"],
                      "verified_files":measured["verified_files"],"verified_bytes":measured["verified_bytes"],"idempotent_admin_write":True,
                      "active_profiles":0,"physical_gpu":False}),flush=True)
