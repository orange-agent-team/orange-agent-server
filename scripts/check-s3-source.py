#!/usr/bin/env python3
"""Use an isolated real SeaweedFS S3 implementation, with separate writer and reader keys."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
from urllib.parse import quote
from owned_postgres import database
from s3_delivery_fixture import delayed_source
from connector_browser_fixture import prepare_browser

parser = argparse.ArgumentParser()
parser.add_argument("--database-url-file", type=Path)
parser.add_argument("--identity-config", type=Path)
parser.add_argument("--web-repo", type=Path)
args = parser.parse_args()
if args.web_repo and not args.database_url_file:
    raise SystemExit("Browser acceptance requires database and identity fixtures")
if bool(args.database_url_file) != bool(args.identity_config):
    raise SystemExit("Specify both database and identity fixtures")
os.umask(0o077)
root = Path(__file__).resolve().parents[1]
browser_env=prepare_browser(args.web_repo,root,"CN06")
pin = json.loads((root / "config/s3-fixture.lock.json").read_text())
weed = root / ".local/seaweedfs/weed"
if (
    not weed.is_file()
    or hashlib.sha256(weed.read_bytes()).hexdigest() != pin["weed_sha256"]
):
    raise SystemExit("Verified SeaweedFS 4.52 macOS arm64 fixture required")
rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo = Path(
    subprocess.check_output(
        [rustup, "which", "--toolchain", "1.97.1", "cargo"], text=True
    ).strip()
)
env = dict(os.environ, PATH=str(cargo.parent) + os.pathsep + os.environ["PATH"])
cmd = [
    cargo,
    "test",
    "--offline",
    "--locked",
    "-p",
    "orange-control-plane",
    "--features",
    "s3-source-tests",
    "--test",
    "s3_source",
]
subprocess.run(cmd + ["--no-run"], cwd=root, env=env, check=True)
with tempfile.TemporaryDirectory(prefix="oa-s3-", dir="/tmp") as tmp:
    folder = Path(tmp)
    bucket = "registered-data"

    def account(name, actions):
        return {
            "name": name,
            "credentials": [
                {"accessKey": secrets.token_hex(12), "secretKey": secrets.token_hex(24)}
            ],
            "actions": actions,
        }

    owner = account("fixture-writer", ["Admin", "Read", "Write", "List"])
    reader = account("source-reader", ["Read:" + bucket, "List:" + bucket])
    identity = folder / "s3.json"
    identity.write_text(json.dumps({"identities": [owner, reader]}))
    identity.chmod(0o600)
    ports = {}
    reservations = []
    for name in (
        "master.port",
        "filer.port",
        "s3.port",
        "volume.port",
        "admin.port",
        "master.port.grpc",
        "filer.port.grpc",
        "s3.port.grpc",
        "volume.port.grpc",
        "admin.port.grpc",
    ):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        ports[name] = s.getsockname()[1]
        reservations.append(s)
    port = ports["s3.port"]
    endpoint = f"http://127.0.0.1:{port}"

    def call(identity, method, path, data=None):
        c = identity["credentials"][0]
        options = [
            shutil.which("curl") or "curl",
            "-q",
            "--noproxy",
            "*",
            "--silent",
            "--connect-timeout",
            "2",
            "--max-time",
            "30",
            "--aws-sigv4",
            "aws:amz:us-east-1:s3",
            "--config",
            "-",
            "-X",
            method,
            "--output",
            os.devnull,
            "--write-out",
            "%{http_code}",
        ]
        if data is not None:
            options += ["--upload-file", str(data)]
        r = subprocess.run(
            options + [endpoint + "/" + quote(path, safe="/")],
            input=f'user = "{c["accessKey"]}:{c["secretKey"]}"\n',
            capture_output=True,
            text=True,
        )
        return int(r.stdout or 0) if r.returncode == 0 else 0

    service_args = [
        weed,
        "mini",
        "-dir=" + tmp,
        "-ip=127.0.0.1",
        "-ip.bind=127.0.0.1",
        "-admin.ui=false",
        "-admin.port.legacy=0",
        "-s3.port.iceberg=0",
        "-s3.port.lance=0",
        "-s3.config=" + str(identity),
        "-filer.localSocket=" + str(folder / "filer.sock"),
        "-volume.max=6",
        "-master.volumeSizeLimitMB=32",
    ]
    service_args.extend("-" + name + "=" + str(n) for name, n in ports.items())
    for s in reservations:
        s.close()
    evidence = root / ".local/seaweedfs"
    with (evidence / "service.log").open("w") as log:
        service = subprocess.Popen(
            service_args,
            cwd=folder,
            env={"PATH": "/usr/bin:/bin", "TMPDIR": tmp},
            stdout=log,
            stderr=log,
        )
        case = None
        try:
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline and service.poll() is None:
                if call(owner, "GET", "") == 200:
                    break
                time.sleep(0.3)
            else:
                raise RuntimeError(
                    "Private S3 service did not become ready; see its local log"
                )
            if call(owner, "PUT", bucket) != 200:
                raise RuntimeError("Owned bucket creation failed")
            for name, content in [
                ("a.txt", b"confirmed source data"),
                ("b.txt", b"b"),
                ("empty.txt", b""),
                ("large.bin", None),
                ("nested/橙子.txt", b"utf8"),
                ("percent 100%.txt", b"literal percent"),
                ("../private.txt", b"outside prefix"),
            ]:
                data = folder / "seed"
                if content is None:
                    with data.open("wb") as f:
                        f.truncate(20 * 1024 * 1024 + 1)
                else:
                    data.write_bytes(content)
                path = (
                    bucket + "/team/private.txt"
                    if name == "../private.txt"
                    else bucket + "/team/approved/" + name
                )
                code = call(owner, "PUT", path, data)
                if code != 200:
                    raise RuntimeError(f"Owned object upload failed: {code}")
            if call(reader, "PUT", bucket + "/team/approved/disallowed", data) != 403:
                raise RuntimeError("S3 source role must deny writes")
            creds = reader["credentials"][0]
            case = subprocess.Popen(
                cmd,
                cwd=root,
                env=dict(
                    env,
                    ORANGE_CN06_FENCE=tmp,
                    ORANGE_CN06_PORT=str(port),
                    ORANGE_CN06_KEY=creds["accessKey"],
                    ORANGE_CN06_SECRET=creds["secretKey"],
                ),
            )
            deadline = time.monotonic() + 90
            while not (folder / "head-recorded").exists():
                if case.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("Fixture did not reach version fence")
                time.sleep(0.05)
            data.write_bytes(b"updated source data")
            if call(owner, "PUT", bucket + "/team/approved/a.txt", data) != 200:
                raise RuntimeError("Fixture object replacement failed")
            (folder / "object-replaced").touch()
            if case.wait(timeout=45):
                raise RuntimeError("Source checks failed")
            if args.database_url_file:
                with data.open("wb") as f:
                    f.truncate(3 * 1024 * 1024)
                if (
                    call(owner, "PUT", bucket + "/team/approved/medium.bin", data)
                    != 200
                ):
                    raise RuntimeError("Fixture attachment creation failed")
                with (
                    database(args.database_url_file, "orange_object_app_") as (_, url),
                    delayed_source(port, folder) as relay,
                ):
                    config = {
                        "source_id": "documents",
                        "registration": {
                            "address": "127.0.0.1",
                            "port": relay,
                            "region": "us-east-1",
                            "bucket": bucket,
                            "prefix": "team/approved",
                        },
                        "access_key": creds["accessKey"],
                        "secret_key": creds["secretKey"],
                    }
                    subprocess.run(
                        [
                            cargo,
                            "test",
                            "--locked",
                            "--offline",
                            "-p",
                            "orange-control-plane",
                            "--features",
                            "s3-source-tests,integration-tests",
                            "--test",
                            "user_identity",
                            "s3_consent::",
                        ],
                        env=dict(
                            env,
                            ORANGE_CN06_FENCE=tmp,
                            **browser_env,
                            ORANGE_S3_SOURCE_FIXTURE=json.dumps(config),
                            ORANGE_TEST_DATABASE_URL=url,
                            ORANGE_TEST_USER_CONFIG_FILE=str(
                                args.identity_config.resolve()
                            ),
                        ),
                        cwd=root,
                        check=True,
                    )

        finally:
            for proc in [case, service]:
                if proc is not None and proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()
print(
    "PASS real signed loopback S3 list/head/read, separate read-only role, prefix isolation, bounds and replacement fencing"
)
