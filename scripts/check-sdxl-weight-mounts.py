#!/usr/bin/env python3.12
"""Exercise the installed SDXL engine's input staging boundary inside a private, networkless Docker volume."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import uuid

from linux_platform_inputs import local_image_id

ROOT = Path(__file__).resolve().parents[1]
SEED = """
import base64,json,os,sys
from pathlib import Path
root=Path('/weights')
for name,encoded in json.load(sys.stdin).items():
    folder,filename=name.split('/')
    assert folder in ('tokenizer','tokenizer_2') and filename in ('merges.txt','vocab.json','tokenizer_config.json','special_tokens_map.json')
    directory=root/folder
    directory.mkdir(exist_ok=True,mode=0o700)
    path=directory/filename
    path.write_bytes(base64.b64decode(encoded,validate=True))
    path.chmod(0o600)
    os.chown(path,65532,65532)
    os.chown(directory,65532,65532)
root.chmod(0o700)
os.chown(root,65532,65532)
"""
PROBE = """
import errno,json,os,sys
from pathlib import Path
sys.path.insert(0,'/opt/orange/app')
import runner
torch=runner.environment()
assert not torch.cuda.is_available()
assert not any(p.name.startswith('eth') for p in Path('/sys/class/net').iterdir())
test=sys.argv[1]
weights=Path('/weights')
work=Path('/scratch')
out=Path('/result')
assert runner.readonly_weights()==(test!='writable')
if test!='writable':
    try:(weights/'probe-write').touch(exist_ok=False)
    except OSError as error:assert error.errno==errno.EROFS
    else:raise AssertionError('weight volume writable')
def refusal(prompt,exception,detail):
    try:runner.generate({'attempt_id':'ad9f8814-940c-44e2-9c2e-7c456a9442e1','prompt':prompt,'seed':22},torch)
    except exception as e:
        if detail is not None:assert str(e)==detail, (type(e).__name__, str(e))
    else:raise AssertionError('job unexpectedly generated output')
    assert not list(work.glob('verified-weights-*')) and not (out/'candidate.png').exists()
if test=='writable':refusal('orange',runner.Rejected,'read-only weight mount required')
elif test=='tampered':refusal('orange',ValueError,'model asset changed')
elif test=='approved-tokenizers':
    marker=out/'already-owned'
    marker.write_bytes(b'keep')
    refusal('orange',runner.Rejected,'empty private output directory required')
    assert marker.read_bytes()==b'keep'
    marker.unlink()
    refusal('orange '*90,runner.Rejected,'prompt token limit')
    refusal('orange',FileNotFoundError,None)
else:raise AssertionError('unknown check')
print(json.dumps({'case':test,'candidate_png':False,'private_staging_remaining':False,'physical_cuda':False}),flush=True)
"""


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--qualification",type=Path,default=ROOT/"distribution/sdxl-linux/qualification.json")
    parser.add_argument("--qualification-sha256",default="3134f8d884184cec7b189ab1577acf3ab33139de838f81bc68d923c2045b7fd9")
    parser.add_argument("--context", default="colima-orange-agent")
    parser.add_argument("--weights", type=Path, default=ROOT/".local/image-model")
    args = parser.parse_args()
    engine = ["docker", "--context", args.context]
    endpoint, = json.loads(subprocess.check_output(["docker", "context", "inspect",args.context],timeout=10))
    if not endpoint["Endpoints"]["docker"]["Host"].startswith("unix://"):
        raise RuntimeError("Local Docker context required")
    raw=args.qualification.read_bytes()
    if hashlib.sha256(raw).hexdigest()!=args.qualification_sha256:
        raise RuntimeError("Different operator-approved image qualification")
    proof=json.loads(raw)
    if not proof["no_device_denied"] or proof["hardware_certified"] or not proof.get("runner_installations"):
        raise RuntimeError("Unqualified target runner revision")
    image = local_image_id(engine, {"image": "orange-dev-input/sdxl-cuda@"+proof["image"], "config_digest": proof["config_digest"], "platform":"linux/amd64"})
    if not image:
        raise RuntimeError("Import the qualified Linux AMD64 SDXL archive")
    files = json.loads((ROOT/"config/local-image.lock.json").read_bytes())["files"]
    approved = {}
    for rel, spec in files.items():
        if rel.split("/")[0] not in ("tokenizer", "tokenizer_2"):
            continue
        asset = args.weights/rel
        data = asset.read_bytes()
        if asset.is_symlink() or len(data) != spec["bytes"] or hashlib.sha256(data).hexdigest() != spec["sha256"]:
            raise RuntimeError("Tokenizer asset changed: "+rel)
        approved[rel] = base64.b64encode(data).decode("ascii")
    if len(approved) != 8:
        raise RuntimeError("Unexpected tokenizer set")
    owner = uuid.uuid4().hex
    volume = "orange-sdxl-weight-inspection-"+owner
    subprocess.run([*engine,"volume","create","--label","orange.owned="+owner,volume],check=True,stdout=subprocess.DEVNULL,timeout=20)
    common = [*engine,"run","--rm","-i","--pull=never","--platform","linux/amd64","--network","none","--read-only",
              "--cap-drop","ALL","--security-opt","no-new-privileges","--pids-limit","128","--memory","2600m",
              "--tmpfs","/tmp:rw,nosuid,nodev,noexec,mode=1777,size=64m","--entrypoint","/usr/local/bin/python3.12"]
    mount = "type=volume,source="+volume+",target=/weights,volume-nocopy"
    def seed(entries):
        subprocess.run([*common,"--user","0:0","--cap-add","CHOWN","--cap-add","DAC_OVERRIDE","--cap-add","FOWNER",
                        "--mount",mount,image,"-I","-B","-c",SEED],input=json.dumps(entries).encode(),check=True,timeout=90)
    results = []
    try:
        seed(approved)
        for case in ("writable","approved-tokenizers","tampered"):
            if case == "tampered":
                key = "tokenizer/merges.txt"
                altered = bytearray(base64.b64decode(approved[key]))
                altered[-1] ^= 1
                seed({key:base64.b64encode(altered).decode("ascii")})
            spec = mount if case=="writable" else mount+",readonly"
            raw = subprocess.check_output([*common,"--user","65532:65532","--mount",spec,
                            "--tmpfs","/scratch:rw,nosuid,nodev,uid=65532,gid=65532,mode=0700,size=32m",
                            "--tmpfs","/result:rw,nosuid,nodev,uid=65532,gid=65532,mode=0700,size=4m",
                            image,"-I","-B","-c",PROBE,case],timeout=300)
            result=json.loads(raw)
            if result!={"case":case,"candidate_png":False,"private_staging_remaining":False,"physical_cuda":False}:
                raise RuntimeError("Invalid installed runner response")
            results.append(result)
            print("PASS installed weight boundary",case,flush=True)
    finally:
        actual=subprocess.check_output([*engine,"volume","inspect","--format",'{{index .Labels "orange.owned"}}',volume],text=True,timeout=15).strip()
        if actual != owner:
            raise RuntimeError("Volume ownership changed")
        subprocess.run([*engine,"volume","rm",volume],check=True,stdout=subprocess.DEVNULL,timeout=20)
    print(json.dumps({"image":proof["image"],"tokenizer_assets":len(approved),"cases":results,"owned_volume_removed":True}),flush=True)


if __name__=="__main__":
    main()
