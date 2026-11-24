#!/usr/bin/env python3.12
"""Check crash/retry and wrong-owner behavior with private journals and optional real Docker objects."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid

from linux_platform_inputs import local_image_id
from sdxl_device_recovery import DockerInventory, reconcile, store_journal

IMAGE = "sha256:" + "2" * 64
CONFIG = "sha256:" + "3" * 64
CID = "a" * 64
PROOF = {"image": IMAGE, "config_digest": CONFIG}


def make(slot, device, nonce, image, cid, phase):
    directory = Path(tempfile.mkdtemp(prefix="attempt-", dir=slot))
    identity = directory.stat()
    (slot / "attempt.lock").touch(mode=0o600)
    record = dict(phase=phase, attempt=nonce, image=image, device=device, container=cid,
                  private_directory=directory.name, baseline_used_mib=16, directory_device=identity.st_dev, directory_inode=identity.st_ino)
    store_journal(slot, record)
    return record


class Inventory:
    def __init__(self, record):
        self.row = {"Id": CID, "Name": "/orange-sdxl-" + record["device"].lower(), "Image": CONFIG,
                    "Config": {"Labels": {"orange.owned": record["attempt"]}}, "State": {"Running": False, "Status": "exited"}}
        self.present = CID
        self.removes = 0

    def find(self, name):
        return self.present

    def inspect(self, cid):
        return self.row

    def remove_stopped(self, cid):
        assert self.present == cid
        self.removes += 1
        self.present = None


def fixtures():
    for case in ("exited", "running", "foreign", "image", "wrong-id", "unknown-create", "unknown-recovery", "high-memory",
                 "directory-swapped", "unlinked-after-remove", "without-id", "remove-lost", "lock-busy", "manifest-id"):
        import fcntl
        with tempfile.TemporaryDirectory() as tmp:
            slot = Path(tmp)
            device = "GPU-" + str(uuid.uuid4())
            nonce = str(uuid.uuid4())
            state = "removing" if case == "unlinked-after-remove" else "creating" if case in {"unknown-create", "without-id"} else "exited"
            r = make(slot, device, nonce, IMAGE, None if state == "creating" else CID, state)
            inventory = Inventory(r)
            if case == "running": inventory.row["State"] = {"Running": True, "Status": "running"}
            if case == "foreign": inventory.row["Config"]["Labels"]["orange.owned"] = str(uuid.uuid4())
            if case == "image": inventory.row["Image"] = "sha256:" + "5" * 64
            if case == "manifest-id": inventory.row["Image"] = IMAGE
            if case == "wrong-id": inventory.present = "b" * 64
            if case in {"unknown-create", "unknown-recovery", "unlinked-after-remove"}: inventory.present = None
            directory = slot / r["private_directory"]
            if case == "unlinked-after-remove": directory.rmdir()
            if case == "directory-swapped":
                directory.rename(slot / "previous")
                directory.mkdir(mode=0o700)
            def driver(device_id, baseline):
                assert device_id == device and baseline == 16
                return case != "high-memory"
            if case == "remove-lost":
                original = inventory.remove_stopped
                def lose_response(cid):
                    original(cid)
                    raise TimeoutError("simulated daemon response loss")
                inventory.remove_stopped = lose_response
            with (slot / "attempt.lock").open() as guard:
                if case == "lock-busy": fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
                accepted = case in {"exited", "without-id", "unlinked-after-remove", "manifest-id"}
                try:
                    receipt = reconcile(slot, device, PROOF, inventory, driver)
                except (RuntimeError, OSError):
                    assert not accepted and (slot / "attempt.json").is_file(), case
                    if case == "remove-lost":
                        assert json.loads((slot / "attempt.json").read_bytes())["phase"] == "removing"
                        assert reconcile(slot, device, PROOF, inventory, driver)["recovered"]
                        assert inventory.removes == 1
                    else:
                        assert inventory.removes == 0 and directory.exists(), case
                else:
                    assert accepted and receipt["attempt_id"] == nonce and not receipt["output_published"]
                    assert not directory.exists() and not (slot / "attempt.json").exists()
    print("PASS 14 recovery scenarios, including incomplete creation, busy locks, foreign ownership and lost removal response; driver readings are fixtures", flush=True)


def real_docker(path, context):
    proof = json.loads(path.read_bytes())
    engine = ["docker", "--context", context]
    endpoints, = json.loads(subprocess.check_output(["docker", "context", "inspect", context], timeout=12))
    assert endpoints["Endpoints"]["docker"]["Host"].startswith("unix://")
    image = local_image_id(engine, {"image": "orange-dev-input/sdxl-cuda@" + proof["image"], "config_digest": proof["config_digest"], "platform":"linux/amd64"})
    assert image and proof["no_device_denied"] and not proof["hardware_certified"]
    inventory = DockerInventory(engine)
    nonce = str(uuid.uuid4())
    device = "GPU-" + str(uuid.uuid4())
    name = "orange-sdxl-" + device.lower()
    with tempfile.TemporaryDirectory() as tmp:
        slot = Path(tmp)
        cid = subprocess.check_output([*engine, "create", "--platform", "linux/amd64", "--pull=never", "--network", "none",
                                      "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--name", name,
                                      "--label", "orange.owned=" + nonce, image, "describe"], text=True, timeout=25).strip()
        try:
            row = inventory.inspect(cid)
            assert row["Name"] == "/" + name and row["Config"]["Labels"]["orange.owned"] == nonce
            print("Docker container immutable image reference", row["Image"], flush=True)
            record = make(slot, device, str(uuid.uuid4()), proof["image"], cid, "starting")
            try: reconcile(slot, device, proof, inventory, lambda *_: True)
            except RuntimeError: pass
            else: raise AssertionError("Wrong owner accepted")
            assert inventory.find(name) == cid
            record["attempt"] = nonce
            store_journal(slot, record)
            subprocess.run([*engine, "start", "-a", cid], timeout=45, check=True, stdout=subprocess.DEVNULL)
            assert inventory.inspect(cid)["State"]["Status"] == "exited"
            try: reconcile(slot, device, proof, inventory, lambda *_: False)
            except RuntimeError: pass
            else: raise AssertionError("Missing driver evidence accepted")
            assert inventory.find(name) == cid and (slot / "attempt.json").exists()
            result = reconcile(slot, device, proof, inventory, lambda *_: True)
            assert result["recovered"] and not result["output_published"] and inventory.find(name) is None
        finally:
            if inventory.find(name) == cid:
                row = inventory.inspect(cid)
                if row["Name"] == "/" + name and row["Config"]["Labels"]["orange.owned"] == nonce and not row["State"]["Running"]:
                    inventory.remove_stopped(cid)
    print("PASS actual local Docker lifecycle, extracted AMD64 describe process, rejected foreign attempt and unobserved device; final driver samples are fixtures, hardware not certified", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--qualification", type=Path)
    parser.add_argument("--context", default="colima-orange-agent")
    args = parser.parse_args()
    fixtures()
    if args.qualification:
        real_docker(args.qualification, args.context)
