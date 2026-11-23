"""Private Linux VM entry point; launch one static, non-networked diagnostic inside the reviewed constraints."""
import hashlib
from contextlib import contextmanager
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid

def check(value,message):
    if not value:raise ValueError(message)


def guest_range(path,uid):
    import pwd
    user=pwd.getpwuid(uid).pw_name
    entries=[line.split(":") for line in Path(path).read_text().splitlines() if line.strip()]
    suitable=[(int(start),int(size)) for account,start,size in entries if account in (str(uid),user) and int(size)>=65536 and int(start)>=65536]
    start,size=next(iter(suitable),(0,0))
    check(start and all(not (int(a)<start+65536 and int(a)+int(n)>start) for acct,a,n in entries if acct not in (str(uid),user)),
          "A distinct existing subordinate range is required for this guest-only test")
    return start


@contextmanager
def private_test_tree():
    base=Path(tempfile.mkdtemp(prefix="orange-oci-policy-"))
    try:
        yield base
    finally:
        if not (base/"cleanup-unconfirmed").exists():
            shutil.rmtree(base)


def main():
    check(os.geteuid()==0 and os.uname().machine=="aarch64","Dedicated arm64 Linux test guest required")
    uid=int(os.environ.get("SUDO_UID","-1"));check(uid>0,"Expected unprivileged initiating guest account")
    gid_base=guest_range("/etc/subgid",uid);uid_base=guest_range("/etc/subuid",uid)
    runtime=shutil.which("runc");check(runtime,"Local OCI runtime unavailable")
    with private_test_tree() as base:
        base.chmod(0o711)
        bundle=base/"bundle";bundle.mkdir(mode=0o700);os.chown(bundle,uid_base,gid_base)
        state=base/"state";state.mkdir(mode=0o700)
        fs=bundle/"rootfs";fs.mkdir()
        with tarfile.open(fileobj=sys.stdin.buffer,mode="r|") as input:
            names=set()
            for member in input:
                check(member.isreg() and member.name in ("probe","config.json") and member.name not in names and member.size<2*1024**2,
                      "Invalid diagnostic package")
                names.add(member.name)
                dest=fs/"probe" if member.name=="probe" else bundle/"config.json"
                with dest.open("xb") as output:shutil.copyfileobj(input.extractfile(member),output)
            check(names=={"probe","config.json"},"Incomplete diagnostic package")
        probe=fs/"probe";hash_value=hashlib.sha256(probe.read_bytes()).hexdigest()
        document=(bundle/"config.json").read_bytes()
        check(hashlib.sha256(document).hexdigest()=="2fa2e7a0b4597af9a30dfedbfec8c1a3500857f30d74d888aec4841d94d34fdb","Unreviewed OCI template")
        cfg=json.loads(document)
        check(cfg["process"]["args"][0]=="/opt/orange/bin/llama-server" and cfg["linux"]["seccomp"]["architectures"]==["SCMP_ARCH_X86_64"],
              "Unexpected base policy")
        # The only profile substitutions are explicitly reported; no GPU or model execution is claimed.
        cfg["process"]["args"]=["/probe","--hold-for-inspection"]
        cfg["linux"]["seccomp"]["architectures"]=["SCMP_ARCH_AARCH64"]
        cfg["linux"]["uidMappings"]=[{"containerID":0,"hostID":uid_base,"size":65536}]
        cfg["linux"]["gidMappings"]=[{"containerID":0,"hostID":gid_base,"size":65536}]
        instance="orange-policy-"+uuid.uuid4().hex
        cfg["linux"]["cgroupsPath"]="/"+instance
        (bundle/"config.json").write_text(json.dumps(cfg))
        for folder in ["proc","dev","tmp","run","run/orange"]:
            directory=fs/folder;directory.mkdir(mode=0o755);os.chown(directory,uid_base,gid_base)
        probe.chmod(0o555);os.chown(probe,uid_base,gid_base);os.chown(fs,uid_base,gid_base)
        args=[runtime,"--root",str(state)]
        cgroup=Path("/sys/fs/cgroup")/instance
        (base/"cleanup-unconfirmed").write_text(instance)
        with tempfile.TemporaryFile() as errors:
            child=subprocess.Popen([*args,"run","--no-new-keyring","--bundle",bundle,instance],stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE,stderr=errors,text=True)
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout,selectors.EVENT_READ)
                    check(selector.select(25),"Diagnostic failed to reach its bounded observation point")
                    line=child.stdout.readline()
                if not line:
                    errors.seek(0);raise RuntimeError("OCI diagnostic startup failed: "+errors.read(4096).decode(errors="replace"))
                observation=json.loads(line)
                check(observation["process_policy_match"] and observation["gpu_bridge"]=="absent" and not observation["supply_authorized"],
                      "Unexpected diagnostic observations: "+str(observation))
                running=json.loads(subprocess.check_output([*args,"state",instance]))
                check(running["status"]=="running" and running["id"]==instance,"Unexpected OCI instance")
                pid=int(running["pid"])
                namespaces={}
                for kind in ["user","mnt","pid","ipc","uts","net","cgroup"]:
                    namespaces[kind]=os.readlink(f"/proc/{pid}/ns/{kind}")!=os.readlink(f"/proc/self/ns/{kind}")
                check(all(namespaces.values()),"Shared host namespace")
                cgroup_line=Path(f"/proc/{pid}/cgroup").read_text().strip().split(":")
                check(len(cgroup_line)==3 and cgroup_line[:2]==["0",""] and cgroup_line[2]=="/"+instance,"Wrong private cgroup membership")
                cgroup=Path("/sys/fs/cgroup")/instance
                limits={key:(cgroup/key).read_text().strip() for key in ("memory.max","memory.swap.max","cpu.max","pids.max")}
                check(limits=={"memory.max":str(4*1024**3),"memory.swap.max":"0","cpu.max":"200000 100000","pids.max":"128"},"Kernel resource limits differ")
                check(child.wait(timeout=12)==2,"CPU-only diagnostic must report missing hardware")
            finally:
                # Scope teardown to the unique instance and newly created runtime state tree.
                subprocess.run([*args,"delete","--force",instance],capture_output=True,timeout=15)
                if child.poll() is None:child.kill();child.wait(timeout=8)
                child.stdout.close()
                remaining=json.loads(subprocess.check_output([*args,"list","--format","json"]))
                check(not remaining,"Unconfirmed OCI instance cleanup; retained private receipt and state")
                deadline=time.monotonic()+5
                while cgroup.exists() and time.monotonic()<deadline:time.sleep(.05)
                check(not cgroup.exists(),"Unconfirmed cgroup removal; retained private receipt and state")
                (base/"cleanup-unconfirmed").unlink()
            print(json.dumps({"vm_architecture":"aarch64","runc":subprocess.check_output([runtime,"--version"],text=True).splitlines()[0],
                              "probe_sha256":hash_value,"process":observation,"isolated_namespaces":namespaces,"kernel_limits":limits,
                              "cgroup_removed":True,"runtime_instance_removed":True,"physical_gpu_verified":False,
                              "substitutions":["architecture","diagnostic executable","guest-owned subordinate ranges","no GPU"]}))


if __name__=="__main__":
    main()
