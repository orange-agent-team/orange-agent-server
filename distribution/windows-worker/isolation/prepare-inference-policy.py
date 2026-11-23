#!/usr/bin/env python3
"""Render or verify the reviewed WSL2 inference OCI configuration. Never installs or launches a workload."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import stat
import struct
import sys
import tempfile

HERE=Path(__file__).resolve().parent
PROFILE="wsl2-reviewed-text-v1"
KEY="opt/orange/session/api.key"
MODEL="opt/orange/model/weights.gguf"
ENGINE="opt/orange/bin/llama-server"
MAP_SIZE=65536
BASE_SHA="2fa2e7a0b4597af9a30dfedbfec8c1a3500857f30d74d888aec4841d94d34fdb"
WSL_SHA="67341de545ccfc3159d69d1920043037fb2cf48013d5930c3bd0a4cb39ec5d37"


class PolicyError(ValueError):
    pass


def require(value,code):
    if not value: raise PolicyError(code)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def pairs(entries):
    result={}
    for key,value in entries:
        require(key not in result,"duplicate_json_key")
        result[key]=value
    return result


def config_bytes(path,limit=65536):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    with os.fdopen(fd,"rb") as stream:
        before=os.fstat(fd)
        require(stat.S_ISREG(before.st_mode) and before.st_size<=limit,"invalid_policy_file")
        data=stream.read(limit+1)
        after=os.fstat(fd)
        require(len(data)<=limit and before.st_ctime_ns==after.st_ctime_ns,"invalid_policy_file")
    return data


def json_file(path):
    return json.loads(config_bytes(path),object_pairs_hook=pairs)


def reviewed_file(name,expected):
    path=HERE/name
    require(path.is_file() and not path.is_symlink() and path.stat().st_size<=65536,"invalid_reviewed_file")
    data=path.read_bytes()
    require(sha(data)==expected,"reviewed_policy_changed")
    return data


def base_policy():
    return json.loads(reviewed_file("oci-base.json",BASE_SHA),object_pairs_hook=pairs)


def canonical(document):
    return json.dumps(document,sort_keys=True,allow_nan=False,separators=(",",":"))


def wsl_config(document):
    sections={};name=None
    for line in document.splitlines():
        line=line.strip()
        if not line or line.startswith(("#",";")):continue
        if line.startswith("[") and line.endswith("]"):
            name=line[1:-1].lower()
            require(name not in sections,"repeated_wsl_section")
            sections[name]={}
        else:
            key,sep,value=line.partition("=");key=key.strip().lower()
            require(name is not None and sep and key not in sections[name],"invalid_wsl_setting")
            sections[name][key]=value.strip().lower()
    return sections


def subordinate_range(document):
    all_ranges=[]
    for entry in document.splitlines():
        if not entry.strip() or entry.lstrip().startswith("#"):continue
        fields=entry.strip().split(":")
        require(len(fields)==3 and fields[1].isascii() and fields[2].isascii() and
                fields[1].isdecimal() and fields[2].isdecimal(),"invalid_subordinate_map")
        name,start,size=fields[0],int(fields[1]),int(fields[2])
        require(0<size and start+size<=2**32-1,"invalid_subordinate_map")
        all_ranges.append((name,start,start+size))
    candidates=[(a,b) for n,a,b in all_ranges if n=="orange-worker"]
    require(len(candidates)==1,"dedicated_subordinate_range_required")
    start,end=candidates[0]
    require(start>=MAP_SIZE and end-start>=MAP_SIZE and
            not any(n!="orange-worker" and a<start+MAP_SIZE and b>start for n,a,b in all_ranges),"overlapping_or_short_subordinate_range")
    return start


def trusted_text(path,limit):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    with os.fdopen(fd,"rb") as stream:
        metadata=os.fstat(fd)
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_uid==0 and metadata.st_mode&0o022==0,"untrusted_system_file")
        data=stream.read(limit+1)
    require(len(data)<=limit,"oversized_system_file")
    return data.decode("utf-8",errors="strict")


def no_drive_mounts(text):
    for line in text.splitlines():
        left,sep,right=line.partition(" - ")
        fields=left.split();fs=right.split()
        require(sep and len(fields)>=6 and len(fs)>=3,"invalid_mountinfo")
        require(fs[0].lower() not in ("9p","drvfs") and not re.match(r"^/mnt/[a-zA-Z](?:/|$)",fields[4]),"windows_drive_mount_observed")


def target_binding():
    require(platform.system()=="Linux" and platform.machine()=="x86_64","windows_wsl2_guest_required")
    require("microsoft-standard-wsl2" in trusted_text("/proc/sys/kernel/osrelease",512).lower(),"wsl2_kernel_unverified")
    require(wsl_config(trusted_text("/etc/wsl.conf",8192))==wsl_config(reviewed_file("wsl.conf",WSL_SHA).decode()),"distribution_policy_differs")
    with open("/proc/self/mountinfo","rb") as f:
        observed=f.read(1024*1024+1)
    require(len(observed)<=1024*1024,"oversized_mountinfo")
    no_drive_mounts(observed.decode("utf-8"))
    try: dev=os.lstat("/dev/dxg")
    except FileNotFoundError:raise PolicyError("gpu_bridge_absent") from None
    except PermissionError:raise PolicyError("gpu_bridge_observation_denied") from None
    require(stat.S_ISCHR(dev.st_mode),"gpu_bridge_not_character_device")
    major,minor=os.major(dev.st_rdev),os.minor(dev.st_rdev)
    lines=trusted_text(f"/sys/dev/char/{major}:{minor}/uevent",8192).splitlines()
    require(major>0 and {"DEVNAME=dxg",f"MAJOR={major}",f"MINOR={minor}"}<=set(lines),"gpu_bridge_identity_unconfirmed")
    return major,minor,subordinate_range(trusted_text("/etc/subuid",1024*1024)),subordinate_range(trusted_text("/etc/subgid",1024*1024))


def linux_engine(stream,root,size):
    header=stream.read(64)
    require(len(header)==64 and header[:7]==b"\x7fELF\x02\x01\x01","x86_64_elf_engine_required")
    kind,machine,version,entry,phoff=struct.unpack_from("<HHIQQ",header,16)
    _,stride,count=struct.unpack_from("<HHH",header,52)
    require(kind in (2,3) and machine==62 and version==1 and stride==56 and 0<count<=512
            and 64<=phoff<=size-count*stride,"invalid_elf_program_headers")
    executable=False;interpreter=False
    for index in range(count):
        stream.seek(phoff+index*stride)
        segment,flags,offset,address,_,file_size,mem_size,_=struct.unpack("<IIQQQQQQ",stream.read(stride))
        require(offset+file_size<=size,"elf_segment_outside_file")
        if segment==1:
            require(mem_size>=file_size,"invalid_load_segment")
            executable|=bool(flags&1) and file_size>0 and address<=entry<address+file_size
        elif segment==3:
            require(not interpreter and 2<=file_size<=4096,"invalid_elf_interpreter")
            interpreter=True
            stream.seek(offset);raw=stream.read(file_size)
            require(raw.endswith(b"\0") and raw.count(b"\0")==1,"invalid_elf_interpreter")
            path=Path(raw[:-1].decode())
            require(path.is_absolute() and path.root=="/" and ".." not in path.parts,"invalid_elf_interpreter")
            loader=(root/path.relative_to("/")).resolve(strict=True)
            require(loader.is_relative_to(root.resolve()) and loader.is_file() and loader.stat().st_mode&0o005==0o005,
                    "unavailable_elf_interpreter")
    require(executable,"elf_entry_outside_executable_bytes")
    stream.seek(0)


def rootfs_digest(root,owners,expected_model):
    require(root.is_dir() and not root.is_symlink(),"reviewed_rootfs_required")
    tree=hashlib.sha256()
    def record(value):
        tree.update(json.dumps(value,ensure_ascii=True,separators=(",",":")).encode()+b"\n")
    count=0
    found=set()
    def visit(fd,prefix,depth):
        nonlocal count
        require(depth<=64,"rootfs_directory_depth")
        names=[]
        with os.scandir(fd) as children:
            for entry in children:
                count+=1
                require(count<=100000,"rootfs_inventory_limit")
                names.append(entry.name)
        for name in sorted(names):
            relative=prefix+name
            require(len(relative)<=4096,"rootfs_inventory_limit")
            item=os.stat(name,dir_fd=fd,follow_symlinks=False)
            mode=stat.S_IMODE(item.st_mode)
            sensitive=relative==KEY
            expected_uid,expected_gid=owners["key"] if sensitive else owners["image"]
            require((item.st_uid,item.st_gid)==(expected_uid,expected_gid) and not mode&0o6000,"untrusted_image_owner_or_setid")
            if any(member.startswith(relative+"/") for member in (KEY,MODEL,ENGINE)):
                require(stat.S_ISDIR(item.st_mode) and mode&0o001,"inaccessible_payload_directory")
            if relative in (KEY,MODEL,ENGINE):
                require(stat.S_ISREG(item.st_mode),"required_member_not_regular")
                found.add(relative)
            if stat.S_ISLNK(item.st_mode):
                link=os.readlink(name,dir_fd=fd)
                require(not os.path.isabs(link) and (root/relative).resolve(strict=True).is_relative_to(root.resolve()),"external_image_link")
                record([relative,"link",link])
                continue
            require(stat.S_ISDIR(item.st_mode) or stat.S_ISREG(item.st_mode),"unexpected_image_member")
            require(not mode&0o022,"writable_image_member")
            opened=os.open(name,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK,dir_fd=fd)
            try:
                meta=os.fstat(opened)
                require((item.st_dev,item.st_ino,item.st_mode)==(meta.st_dev,meta.st_ino,meta.st_mode),"changed_image_member")
                require(not os.listxattr(opened),"unreviewed_image_attributes")
                if stat.S_ISDIR(meta.st_mode):
                    record([relative,"directory",mode])
                    visit(opened,relative+"/",depth+1)
                else:
                    require(stat.S_ISREG(meta.st_mode) and meta.st_nlink==1,"unexpected_image_member")
                    with os.fdopen(opened,"rb",closefd=False) as f:
                        if sensitive:
                            require(mode==0o400 and re.fullmatch(rb"[0-9a-f]{64}\n",f.read(66)),"private_per_run_key_required")
                            content="private-session-key"
                        else:
                            if relative==ENGINE:
                                require(mode&0o005==0o005,"unexecutable_engine")
                                linux_engine(f,root,meta.st_size)
                            if relative==MODEL:require(mode&0o004,"unreadable_model")
                            content=hashlib.file_digest(f,"sha256").hexdigest()
                            if relative==MODEL:require(content==expected_model,"reviewed_model_digest_mismatch")
                    record([relative,"file",mode,content])
                end=os.fstat(opened)
                require((item.st_size,item.st_ctime_ns)==(end.st_size,end.st_ctime_ns),"changed_image_member")
            finally:os.close(opened)
    fd=os.open(root,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW)
    try:
        start=os.fstat(fd);mode=stat.S_IMODE(start.st_mode)
        require((start.st_uid,start.st_gid)==owners["image"] and not mode&0o6022 and mode&0o001 and not os.listxattr(fd),"invalid_image_root")
        record([".","directory",mode]);visit(fd,"",0)
        require(start.st_ctime_ns==os.fstat(fd).st_ctime_ns,"changed_image_member")
    finally:os.close(fd)
    require(found=={KEY,MODEL,ENGINE},"reviewed_rootfs_incomplete")
    return tree.hexdigest()


def policy(binding,root_digest):
    major,minor,uid,gid=binding
    spec=base_policy()
    linux=spec["linux"]
    linux["uidMappings"]=[{"containerID":0,"hostID":uid,"size":MAP_SIZE}]
    linux["gidMappings"]=[{"containerID":0,"hostID":gid,"size":MAP_SIZE}]
    linux["devices"]=[{"path":"/dev/dxg","type":"c","major":major,"minor":minor,"fileMode":0o600,"uid":65532,"gid":65532}]
    linux["resources"]["devices"].append({"allow":True,"type":"c","major":major,"minor":minor,"access":"rw"})
    spec["annotations"]["org.orange-agent.rootfs.sha256"]=root_digest
    return spec


def publish_config(target,spec):
    data=(json.dumps(spec,indent=2)+"\n").encode()
    with tempfile.NamedTemporaryFile(dir=target.parent,delete=False) as stream:
        staged=Path(stream.name)
        try:
            stream.write(data);stream.flush();os.fsync(stream.fileno())
            try: os.link(staged,target)
            except FileExistsError:
                require(canonical(json_file(target))==canonical(spec),"existing_oci_configuration_differs")
        finally:
            staged.unlink(missing_ok=True)


def main():
    cli=argparse.ArgumentParser(description=__doc__)
    cli.add_argument("action",choices=["describe","measure","render","verify"])
    cli.add_argument("--bundle",type=Path)
    cli.add_argument("--trusted-rootfs-sha256")
    args=cli.parse_args()
    if args.action=="describe":
        require(args.bundle is None and args.trusted_rootfs_sha256 is None,"unexpected_profile_input")
        print(json.dumps({"profile":PROFILE,"oci_base_sha256":sha(reviewed_file("oci-base.json",BASE_SHA)),
                          "wsl_conf_sha256":sha(reviewed_file("wsl.conf",WSL_SHA)),"runtime_verified":False,"supply_authorized":False}))
        return
    require(args.bundle is not None,"private_bundle_required")
    if args.action=="measure":
        require(args.trusted_rootfs_sha256 is None,"unexpected_profile_input")
    else:
        require(args.trusted_rootfs_sha256 and re.fullmatch(r"[0-9a-f]{64}",args.trusted_rootfs_sha256),"trusted_release_digest_required")
    binding=target_binding()
    require(args.bundle.is_dir() and not args.bundle.is_symlink() and (args.bundle.stat().st_uid,args.bundle.stat().st_gid)==binding[2:] and args.bundle.stat().st_mode&0o077==0,"private_bundle_required")
    bundle=args.bundle.resolve(strict=True)
    reviewed=base_policy()["annotations"]["org.orange-agent.weights.sha256"]
    owners={"image":(binding[2],binding[3]),"key":(binding[2]+65532,binding[3]+65532)}
    measured=rootfs_digest(bundle/"rootfs",owners,reviewed)
    if args.action=="measure":
        print(json.dumps({"profile":PROFILE,"rootfs_sha256":measured,"runtime_verified":False,"supply_authorized":False}))
        return
    require(measured==args.trusted_rootfs_sha256,"unreviewed_rootfs")
    expected=policy(binding,measured)
    target=bundle/"config.json"
    if args.action=="render":publish_config(target,expected)
    config=config_bytes(target)
    require(canonical(json.loads(config,object_pairs_hook=pairs))==canonical(expected),"oci_policy_differs")
    print(json.dumps({"profile":PROFILE,"rootfs_sha256":measured,"config_sha256":sha(config),"runtime_verified":False,"supply_authorized":False}))


if __name__=="__main__":
    try:main()
    except (PolicyError,OSError,UnicodeError,ValueError,RuntimeError) as error:
        print(json.dumps({"profile":PROFILE,"status":str(error) if isinstance(error,PolicyError) else "observation_unavailable",
                          "runtime_verified":False,"supply_authorized":False}))
        sys.exit(2)
