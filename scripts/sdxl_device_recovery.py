"""Recover one stopped local development container without starting work or publishing output."""
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import uuid


def require(value, reason):
    if not value:
        raise RuntimeError(reason)


def sync_directory(path):
    directory = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def store_journal(slot, record):
    import tempfile
    with tempfile.NamedTemporaryFile(mode="w", dir=slot, delete=False) as out:
        temporary = Path(out.name)
        try:
            json.dump(record, out, sort_keys=True)
            out.flush()
            os.fsync(out.fileno())
            os.replace(temporary, slot / "attempt.json")
            sync_directory(slot)
        finally:
            temporary.unlink(missing_ok=True)


class DockerInventory:
    def __init__(self, engine):
        self.engine = engine

    def find(self, name):
        rows = subprocess.check_output(
            [*self.engine, "ps", "-a", "--no-trunc", "--filter", f"name=^/{name}$", "--format", "{{.ID}}"],
            text=True, timeout=15,
        ).splitlines()
        require(len(rows) <= 1 and all(re.fullmatch("[a-f0-9]{64}", value) for value in rows),
                "Ambiguous device container inventory")
        return rows[0] if rows else None

    def inspect(self, container):
        row, = json.loads(subprocess.check_output([*self.engine, "inspect", container], timeout=15))
        return row

    def remove_stopped(self, container):
        subprocess.run([*self.engine, "rm", container], check=True, timeout=20, stdout=subprocess.DEVNULL)


def reconcile(slot, device, proof, inventory, observe):
    """Attempt lock is held across both observations, cleanup, and durable fence removal."""
    lock_fd = os.open(slot / "attempt.lock", os.O_RDWR | os.O_NOFOLLOW)
    with os.fdopen(lock_fd, "r+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        journal = slot / "attempt.json"
        with os.fdopen(os.open(journal, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW), "rb") as stream:
            file = os.fstat(stream.fileno())
            require(stat.S_ISREG(file.st_mode) and file.st_uid == os.getuid() and file.st_nlink == 1 and 0 < file.st_size <= 4096,
                    "Invalid device journal")
            record = json.load(stream)
        phase = record["phase"]
        require(phase in {"creating", "starting", "exited", "release_confirmed", "recovery_required", "removing"},
                "Unrecognized device journal phase")
        require(record["image"] == proof["image"] and record["device"].lower() == device.lower()
                and str(uuid.UUID(record["attempt"])) == record["attempt"], "Different device, image or attempt")
        require(type(record["baseline_used_mib"]) is int and record["baseline_used_mib"] >= 0
                and re.fullmatch(r"attempt-[a-z0-9_]{8}", record["private_directory"]), "Invalid attempt directory or baseline")
        attempt = slot / record["private_directory"]
        if os.path.lexists(attempt):
            meta = attempt.lstat()
            require(stat.S_ISDIR(meta.st_mode) and meta.st_uid == os.getuid() and meta.st_mode & 0o077 == 0
                    and (meta.st_dev, meta.st_ino) == (record["directory_device"], record["directory_inode"]),
                    "Attempt directory identity changed")
        else:
            require(phase == "removing", "Attempt directory missing")
        name = "orange-sdxl-" + device.lower()
        expected = record["container"]
        require(expected is None or re.fullmatch("[a-f0-9]{64}", expected), "Invalid recorded container")
        container = inventory.find(name)
        if container is None:
            require(phase == "removing" and expected is not None, "Container absence cannot establish the outcome of a previous request")
        else:
            require(expected is None or expected == container, "Different container occupies the device slot")
            row = inventory.inspect(container)
            require(row["Id"] == container and row["Name"] == "/" + name and row["Image"] in (proof["config_digest"], proof["image"])
                    and row["Config"]["Labels"].get("orange.owned") == record["attempt"], "Container ownership or image changed")
            require(row["State"]["Running"] is False and row["State"]["Status"] in {"created", "exited"},
                    "Device container has not stopped")
        require(observe(device, record["baseline_used_mib"]), "GPU release observations missing or elevated")
        if container is not None:
            record.update(phase="removing", container=container)
            store_journal(slot, record)
            inventory.remove_stopped(container)
        require(inventory.find(name) is None, "Container deletion not confirmed")
        if attempt.exists():
            shutil.rmtree(attempt)
        journal.unlink()
        sync_directory(slot)
        return {"attempt_id": record["attempt"], "device_uuid": device, "recovered": True, "output_published": False}
