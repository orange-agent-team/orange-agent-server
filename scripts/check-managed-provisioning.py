#!/usr/bin/env python3
"""Run the managed OCI driver with an expiring original grant and bounded local supervisor renewals."""
import argparse
import asyncio
import base64
import hashlib
import json
from pathlib import Path
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


async def verify(archive):
    identity = str(uuid.uuid4())
    body = b"local provisioning acceptance"
    deadline = int(time.time()) + 120
    original = int(time.time()) + 30
    binding = {"allocation_id":identity, "host_instance_id":"development-lease-check",
               "workspace_id":str(uuid.uuid4()), "run_id":str(uuid.uuid4()), "epoch":1,
               "input_sha256":hashlib.sha256(body).hexdigest(), "lease_until":original,
               "workspace_until":deadline}

    async def child(operation):
        return await asyncio.create_subprocess_exec(
            sys.executable, ROOT / "scripts/cloud-workspace-driver.py", operation, identity, "--archive", archive,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, cwd=ROOT)

    reserve = await child("reserve")
    data, _ = await asyncio.wait_for(reserve.communicate(json.dumps(binding).encode()+b"\n"), 40)
    if reserve.returncode or json.loads(data)["phase"] != "registered":
        raise RuntimeError("Guest reservation failed")
    opened = None
    renewal = None
    try:
        opened = await child("open")
        async def send(message):
            opened.stdin.write(message+b"\n")
            await opened.stdin.drain()

        async def renew():
            while True:
                await asyncio.sleep(3)
                until = min(deadline, int(time.time())+30)
                if until <= time.time():
                    raise RuntimeError("Qualification workspace reached its maximum")
                await send(f"renew {until}".encode())

        await send(json.dumps(dict(binding,content_base64=base64.b64encode(body).decode())).encode())
        renewal = asyncio.create_task(renew())
        async def observe(phase):
            data = await asyncio.wait_for(opened.stdout.readline(),45)
            if not data:
                raise RuntimeError(f"Managed guest exited before {phase}")
            value = json.loads(data)
            if value["phase"] != phase or value["allocation_id"] != identity:
                raise RuntimeError(f"Unexpected receipt while waiting for {phase}")
            return value

        ready = await observe("ready")
        if ready["input_staged"] is not True or ready["workload_uid"] != 65534:
            raise RuntimeError("No real isolated input")
        await asyncio.sleep(max(0,original-time.time()+1))
        permit = str(uuid.uuid4())
        operation = hashlib.sha256(permit.encode()).hexdigest()
        args = {"operation_id":operation,"permit_id":permit,"argument_digest":hashlib.sha256(b"list").hexdigest(),
                "allocation_id":identity,"workspace_epoch":1,"run_epoch":1,"expires_at":int(time.time())+9,"limit":4}
        await send(b"tool "+json.dumps(args).encode())
        listing = await observe("directory_result")
        if listing["permit_id"] != permit or listing["operation_id"] != operation or not any(
                r == {"name":"input.txt","kind":"file","bytes":str(len(body))} for r in listing["entries"]):
            raise RuntimeError("Real workspace operation did not survive original expiry")
        await send(b"stop")
        released = await observe("cleaned")
        if await asyncio.wait_for(opened.wait(),30) or not all(released[k] for k in
                ("sandbox_absent","files_absent","cgroup_absent")):
            raise RuntimeError("Guest process or cleanup receipt failed")
    finally:
        if renewal is not None:
            renewal.cancel()
            await asyncio.gather(renewal,return_exceptions=True)
        if opened is not None and opened.returncode is None:
            opened.stdin.close()
            await asyncio.wait_for(opened.wait(),60)
        recovery = await child("recover")
        data, _ = await asyncio.wait_for(recovery.communicate(),40)
        if recovery.returncode or json.loads(data)["phase"] != "cleaned":
            raise RuntimeError("Independent recovery of this test allocation failed")
    print("PASS real OCI/gVisor driver, operation after original expiry, bounded test-host renewals, "
          "independent sandbox, file and cgroup cleanup")


if __name__ == "__main__":
    cli = argparse.ArgumentParser()
    cli.add_argument("--archive",type=Path,default=ROOT/".local/gvisor-download/gvisor-aarch64.tar.bz2")
    asyncio.run(verify(cli.parse_args().archive.resolve()))
