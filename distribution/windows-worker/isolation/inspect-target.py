#!/usr/bin/env python3
"""Read the current diagnostic process's isolation facts. Never reads model inputs or connects to a service."""
import errno
import json
import os
from pathlib import Path
import platform
import socket
import stat
import sys


def read_small(path,limit=131072):
    with open(path,"rb") as stream:
        data=stream.read(limit+1)
        if len(data)>limit:raise ValueError("oversized observation")
        return data.decode("utf8",errors="strict")


def path_state(path):
    try:
        return "character_device" if stat.S_ISCHR(os.lstat(path).st_mode) else "present"
    except FileNotFoundError:return "absent"
    except PermissionError:return "permission_denied"
    except OSError:return "unobserved"


def device_leaf(dest,source,kind):
    numbers={"/dev/null":(1,3),"/dev/zero":(1,5),"/dev/full":(1,7),"/dev/random":(1,8),
             "/dev/urandom":(1,9),"/dev/tty":(5,0)}
    if (dest not in numbers and dest!="/dev/dxg") or source!="/"+dest.rsplit("/",1)[1] or kind not in ("tmpfs","devtmpfs"):return False
    try: metadata=os.lstat(dest)
    except OSError:return False
    return stat.S_ISCHR(metadata.st_mode) and (dest=="/dev/dxg" or (os.major(metadata.st_rdev),os.minor(metadata.st_rdev))==numbers[dest])


def mount_facts(text):
    root=None;windows_drives=0;unexpected=0;devices=0;private_tmp=set()
    allowed={"/proc":"proc","/dev":"tmpfs","/tmp":"tmpfs","/run/orange":"tmpfs"}
    masked={"/proc/acpi","/proc/asound","/proc/kcore","/proc/keys","/proc/latency_stats","/proc/scsi",
            "/proc/timer_list","/proc/sysrq-trigger","/proc/sys/fs/binfmt_misc"}
    read_only={"/proc/bus","/proc/fs","/proc/irq","/proc/sys"}
    for line in text.splitlines():
        left,separator,right=line.partition(" - ")
        fields=left.split();fs=right.split()
        if not separator or len(fields)<6 or len(fs)<3:raise ValueError("incomplete mountinfo")
        dest=fields[4];flags=set(fields[5].split(","))
        if fs[0].lower() in ("drvfs","9p") or dest.split("/")[:2]==["","mnt"]:
            windows_drives+=1
        if dest=="/":
            if root is not None:raise ValueError("multiple roots")
            root="ro" in flags
        elif dest in allowed:
            if fs[0]!=allowed[dest]:unexpected+=1
            if dest in ("/tmp","/run/orange") and fields[3]=="/" and {"nodev","nosuid","noexec","rw"}<=flags:
                private_tmp.add(dest)
        elif dest in read_only:
            if fs[0]!="proc" or "ro" not in flags:unexpected+=1
        elif dest in masked:
            if fs[0] not in ("tmpfs","devtmpfs"):unexpected+=1
        elif device_leaf(dest,fields[3],fs[0]):devices+=1
        else:
            unexpected+=1
    if root is None:raise ValueError("missing root")
    return {"root_readonly":root,"host_mount_entries":windows_drives,"unexpected_mount_entries":unexpected,"device_leaf_mounts":devices,
            "private_scratch":private_tmp=={"/tmp","/run/orange"}}


def internet_socket():
    try:
        with socket.socket(socket.AF_INET,socket.SOCK_STREAM):return "creation_allowed"
    except OSError as exc:
        return "permission_denied" if exc.errno in (errno.EPERM,errno.EACCES) else "unobserved"


def observe():
    result={"profile":"wsl2-reviewed-text-v1","source":"current_process","runtime_verified":False,"supply_authorized":False}
    if platform.system()!="Linux":
        result["status"]="unsupported_platform"
        return result
    status=dict(line.split(":",1) for line in read_small("/proc/self/status").splitlines() if ":" in line)
    capabilities=all(int(status[key].strip(),16)==0 for key in ("CapEff","CapPrm","CapBnd","CapAmb","CapInh"))
    interfaces=[line.split(":",1)[0].strip() for line in read_small("/proc/net/dev").splitlines() if ":" in line]
    mount=mount_facts(read_small("/proc/self/mountinfo"))
    paths={name:path_state(path) for name,path in {"windows_volume":"/mnt/c","docker_socket":"/var/run/docker.sock",
           "containerd_socket":"/run/containerd/containerd.sock","wsl_interop":"/run/WSL"}.items()}
    gpu=path_state("/dev/dxg")
    net=internet_socket()
    observed={"no_new_privileges":status["NoNewPrivs"].strip()=="1","seccomp_filter":status["Seccomp"].strip()=="2",
        "empty_capabilities":capabilities,"unprivileged_identity":os.geteuid()==65532 and os.getegid()==65532,
        "nonloopback_interfaces":len([v for v in interfaces if v!="lo"]),"internet_socket":net,"prohibited_paths":paths,
        "gpu_bridge":gpu,**mount}
    matched=(all(observed[k] for k in ("no_new_privileges","seccomp_filter","empty_capabilities","unprivileged_identity",
                  "root_readonly","private_scratch")) and observed["nonloopback_interfaces"]==0 and net=="permission_denied"
             and all(v in ("absent","permission_denied") for v in paths.values()) and gpu=="character_device"
             and mount["host_mount_entries"]==0 and mount["unexpected_mount_entries"]==0)
    return {**result,"status":"observations_match" if matched else "observations_incomplete","observations":observed}


if __name__=="__main__":
    try:
        if len(sys.argv)!=1:raise ValueError("no input accepted")
        report=observe()
    except (OSError,ValueError,KeyError,UnicodeError):
        report={"profile":"wsl2-reviewed-text-v1","status":"observation_unavailable","runtime_verified":False,"supply_authorized":False}
    print(json.dumps(report,sort_keys=True))
    sys.exit(0 if report["status"]=="observations_match" else 2)
