#!/usr/bin/env python3
"""Validate the exact contributor bundle policy and perform read-only observations inside the dedicated local Linux VM."""
import argparse
from contextlib import ExitStack
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import struct
import tarfile
import tempfile
import zipfile

ROOT=Path(__file__).resolve().parents[1]
ASSETS=ROOT/"distribution/windows-worker/isolation"


def module(name,source):
    spec=importlib.util.spec_from_file_location(name,source)
    result=importlib.util.module_from_spec(spec);spec.loader.exec_module(result)
    return result


def expect_error(fn):
    try: fn()
    except (ValueError,OSError):return
    raise AssertionError("invalid policy evidence was accepted")


def check(directory,guest):
    p=module("policy",directory/"prepare-inference-policy.py")
    obs=module("observation",directory/"inspect-target.py")
    template=p.base_policy()
    assert p.wsl_config(p.reviewed_file("wsl.conf",p.WSL_SHA).decode())=={
        "automount":{"enabled":"false","mountfstab":"false"},"interop":{"enabled":"false","appendwindowspath":"false"}}
    expect_error(lambda:p.wsl_config("[interop]\nenabled=false\nenabled=true"))
    assert p.subordinate_range("someone:65536:65536\norange-worker:131072:65536")==131072
    for ranges in ("orange-worker:0:65536","orange-worker:131072:65535","other:130000:10000\norange-worker:131072:65536"):
        expect_error(lambda r=ranges:p.subordinate_range(r))
    p.no_drive_mounts("1 0 8:1 / / ro - ext4 root ro")
    for entry in ("2 1 0:1 / /mnt/c rw - ext4 root rw","2 1 0:1 / /other rw - 9p drvfs rw"):
        expect_error(lambda x=entry:p.no_drive_mounts(x))
    rendered=p.policy((245,7,131072,196608),"a"*64)
    assert rendered["root"]=={"path":"rootfs","readonly":True} and "hooks" not in rendered
    proc=rendered["process"]
    assert proc["noNewPrivileges"] and proc["user"]=={"uid":65532,"gid":65532} and not any(proc["capabilities"].values())
    assert proc["args"][0]=="/opt/orange/bin/llama-server" and "/run/orange/inference.sock" in proc["args"]
    assert set(m["destination"] for m in rendered["mounts"])=={"/proc","/dev","/tmp","/run/orange"}
    assert all(m["source"]==m["type"] and m["type"] in ("proc","tmpfs") for m in rendered["mounts"])
    linux=rendered["linux"]
    assert all(set(n)=={"type"} for n in linux["namespaces"]) and {n["type"] for n in linux["namespaces"]}==set("user mount pid ipc uts network cgroup".split())
    assert linux["uidMappings"]==[{"containerID":0,"hostID":131072,"size":65536}]
    assert linux["gidMappings"]==[{"containerID":0,"hostID":196608,"size":65536}]
    assert linux["devices"]==[{"path":"/dev/dxg","type":"c","major":245,"minor":7,"fileMode":384,"uid":65532,"gid":65532}]
    rules=linux["resources"]["devices"]
    assert rules[0]=={"allow":False,"access":"rwm"} and all(r["allow"] and r["type"]=="c" and r["access"]=="rw" for r in rules[1:])
    assert {(r["major"],r["minor"]) for r in rules[1:]}=={(1,3),(1,5),(1,8),(1,9),(245,7)}
    assert linux["seccomp"]["defaultAction"]=="SCMP_ACT_ERRNO"
    permitted=[r for r in linux["seccomp"]["syscalls"] if r["action"]=="SCMP_ACT_ALLOW"]
    assert not {"mount","ptrace","unshare","setns","bpf","mknod","mknodat","socketcall"} & {n for r in permitted for n in r["names"]}
    assert [r["args"] for r in permitted if "socket" in r["names"]]==[[{"index":0,"value":1,"op":"SCMP_CMP_EQ"}]]
    with tempfile.TemporaryDirectory() as td:
        scratch=Path(td)
        cfg=scratch/"config.json"
        p.publish_config(cfg,rendered);p.publish_config(cfg,rendered)
        assert p.canonical(p.json_file(cfg))==p.canonical(rendered)
        changed=json.loads(cfg.read_bytes());changed["hooks"]={"prestart":[]}
        cfg.write_text(json.dumps(changed));expect_error(lambda:p.publish_config(cfg,rendered))
        assert "hooks" in json.loads(cfg.read_bytes())
        cfg.write_text('{"process":{},"process":{}}');expect_error(lambda:p.json_file(cfg))
        assert p.canonical({"value":True})!=p.canonical({"value":1})
        if guest:
            # Explicit synthetic ELF/weights. This tests image integrity and key handling,
            # not execution of that model or acceptance of these values on the real CLI.
            image=scratch/"rootfs";image.mkdir(mode=0o755);image.chmod(0o755)
            for rel in (p.KEY,p.MODEL,p.ENGINE):(image/rel).parent.mkdir(mode=0o755,parents=True,exist_ok=True)
            for directory in image.rglob("*"):
                if directory.is_dir(): directory.chmod(0o755)
            model=b"test weights";weights=image/p.MODEL;weights.write_bytes(model);weights.chmod(0o644)
            # One minimal, bounded executable load segment for the structural fixture.
            header=struct.pack("<16sHHIQQQIHHHHHH",b"\x7fELF\x02\x01\x01"+bytes(9),2,62,1,0x400078,64,0,0,64,56,1,0,0,0)
            segment=struct.pack("<IIQQQQQQ",1,5,0,0x400000,0,121,121,4096)
            elf=header+segment+b"\xc3"
            eng=image/p.ENGINE;eng.write_bytes(elf);eng.chmod(0o555)
            secret=image/p.KEY
            def change_key(value):
                if secret.exists():secret.chmod(0o600)
                secret.write_bytes(value);secret.chmod(0o400)
            change_key(b"a"*64+b"\n")
            owners={k:(os.geteuid(),os.getegid()) for k in ("image","key")}
            measure=lambda:p.rootfs_digest(image,owners,hashlib.sha256(model).hexdigest())
            first=measure();change_key(b"b"*64+b"\n");assert first==measure()
            change_key(b"");expect_error(measure);change_key(b"a"*64+b"\n")
            (image/p.MODEL).write_bytes(b"changed data");expect_error(measure)
            (image/p.MODEL).write_bytes(model)
            link=image/"external";link.symlink_to("/etc/passwd");expect_error(measure);link.unlink()
            link.hardlink_to(image/p.MODEL);expect_error(measure);link.unlink()
            fifo=image/"fifo";os.mkfifo(fifo);expect_error(measure);fifo.unlink()
            eng.chmod(0o444);expect_error(measure);eng.chmod(0o555)
            weights.chmod(0o600);expect_error(measure);weights.chmod(0o644)
            eng.parent.chmod(0o700);expect_error(measure);eng.parent.chmod(0o755)
            eng.chmod(0o755);eng.write_bytes(elf[:20]);expect_error(measure);eng.write_bytes(elf);eng.chmod(0o555)
            assert measure()==first
        report=obs.observe()
        assert not report["supply_authorized"] and not report["runtime_verified"]
        if guest:
            assert report["status"]=="observations_incomplete" and report["observations"]["gpu_bridge"]=="absent"
        print(json.dumps({"policy_digest":p.BASE_SHA,"guest":guest,"observation":report,
                          "artificial_inputs_for_positive_template":True}),flush=True)


def installed_sources(scratch,powershell):
    destination=scratch/"isolation";destination.mkdir()
    source=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    dist=ROOT/".local/windows-worker-distributions"
    records=(json.loads(f.read_bytes()) for f in dist.glob("*.json"))
    record,=[r for r in records if r["source"]==source]
    assert record["identical_archives"]==2 and not record["windows_runtime_verified"] and not record["physical_gpu_verified"]
    archive=dist/record["archive"]
    assert archive.parent==dist and not archive.is_symlink()
    with archive.open("rb") as f:
        assert hashlib.file_digest(f,"sha256").hexdigest()==record["sha256"]
    with zipfile.ZipFile(archive) as z:
        assert z.testzip() is None and len(z.namelist())==len(set(z.namelist()))
        manifest=json.loads(z.read("orange-worker/manifest.json"))
        assert manifest["source"]==source
        (scratch/"manifest.json").write_bytes(z.read("orange-worker/manifest.json"))
        (scratch/"Test-WorkerInventory.ps1").write_bytes(z.read("orange-worker/Test-WorkerInventory.ps1"))
        for name in ("README.md","wsl.conf","oci-base.json","prepare-inference-policy.py","inspect-target.py","orange-isolation-probe","release.json"):
            payload=z.read("orange-worker/isolation/"+name)
            assert hashlib.sha256(payload).hexdigest()==manifest["sha256"]["isolation/"+name]
            (destination/name).write_bytes(payload)
    builder=module("probe_builder",ROOT/"scripts/build-worker-isolation-probe.py")
    builder.verify_image(destination/"orange-isolation-probe","x86_64-unknown-linux-musl")
    result=json.loads(subprocess.check_output([sys.executable,destination/"prepare-inference-policy.py","describe"],text=True))
    assert result["profile"]=="wsl2-reviewed-text-v1" and not result["supply_authorized"]
    print("PASS installed Windows package and x86_64 Linux static diagnostic",record["sha256"],flush=True)
    if powershell:
        command=[powershell,"-NoProfile","-NonInteractive","-File",scratch/"Test-WorkerInventory.ps1","-CheckIsolationFiles"]
        env=dict(os.environ,POWERSHELL_TELEMETRY_OPTOUT="1",POWERSHELL_UPDATECHECK="Off",XDG_CONFIG_HOME=str(scratch/"config"),
                 XDG_DATA_HOME=str(scratch/"data"),XDG_CACHE_HOME=str(scratch/"cache"))
        metadata=json.loads(subprocess.check_output(command,cwd=scratch,env=env,text=True,timeout=20))
        info=metadata["isolation_files"]
        assert metadata["source"]==source and info["target"]=="x86_64-unknown-linux-musl"
        assert not info["supply_authorized"] and not info["runtime_verified"] and len(info["member_sha256"])==7
        assert info["wasm_enabled"] is False and info["report_version"]==1
        expected=manifest["sha256"]["isolation/orange-isolation-probe"]
        assert info["member_sha256"]["isolation/orange-isolation-probe"]==expected
        # Keep the original copy intact while checking that the reader refuses tampering.
        original=destination/"wsl.conf";saved=destination/"saved-wsl.conf"
        original.rename(saved)
        try:
            original.write_bytes(b"[interop]\nenabled=true\n")
            denied=subprocess.run(command,cwd=scratch,env=env,capture_output=True,timeout=20)
            assert denied.returncode!=0 and not denied.stdout.strip()
        finally:
            original.unlink(missing_ok=True);saved.rename(original)
        changed=dict(manifest,wasm_enabled=True)
        (scratch/"manifest.json").write_text(json.dumps(changed))
        try:
            denied=subprocess.run(command,cwd=scratch,env=env,capture_output=True,timeout=20)
            assert denied.returncode!=0 and not denied.stdout.strip()
        finally:
            (scratch/"manifest.json").write_text(json.dumps(manifest))
        print("PASS packaged PowerShell verifies the seven isolation payloads and rejects a changed distribution configuration")
    return destination


def main():
    parser=argparse.ArgumentParser();parser.add_argument("--linux",action="store_true")
    parser.add_argument("--powershell",type=Path);parser.add_argument("--installed",action="store_true");parser.add_argument("--inside",type=Path);args=parser.parse_args()
    if args.powershell and not args.installed:parser.error("--powershell requires --installed")
    if args.inside:
        check(args.inside,True);return
    with ExitStack() as stack:
        directory=installed_sources(Path(stack.enter_context(tempfile.TemporaryDirectory())),args.powershell.resolve(strict=True) if args.powershell else None) if args.installed else ASSETS
        check(directory,False)
        if args.linux:guest_check(directory)
    print("PASS reviewed WSL/OCI configuration, strict device rules and closed admission; Windows/GPU isolation and inference require target verification")


def guest_check(directory):
    data=io.BytesIO()
    with tarfile.open(fileobj=data,mode="w") as tar:
        for path in [directory/n for n in ("prepare-inference-policy.py","inspect-target.py","oci-base.json","wsl.conf")]+[Path(__file__)]:
            tar.add(path,arcname=path.name,recursive=False)
    script="""import io,sys,subprocess,tarfile,tempfile
from pathlib import Path
with tempfile.TemporaryDirectory(prefix='orange-policy-') as work:
 with tarfile.open(fileobj=io.BytesIO(sys.stdin.buffer.read()),mode='r:') as archive:
  assert all(f.isreg() and Path(f.name).name==f.name for f in archive)
  archive.extractall(work,filter='data')
 subprocess.run([sys.executable,'-B','check-worker-isolation-policy.py','--inside',work],cwd=work,check=True)
"""
    result=subprocess.run(["colima","ssh","--profile","orange-agent","--","python3","-c",script],input=data.getvalue(),timeout=90)
    result.check_returncode()


if __name__=="__main__":
    main()
