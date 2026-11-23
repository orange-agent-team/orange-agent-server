//! Standalone Linux probe. No dependencies, privileged operations, remote connection or system configuration inputs.
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::UdpSocket;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn small(path: &str, limit: u64) -> io::Result<String> {
    let mut value = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut value)?;
    if value.len() as u64 > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(value)
}

fn presence(path: &str) -> &'static str {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_char_device() => "character_device",
        Ok(_) => "present",
        Err(e) if e.kind() == io::ErrorKind::NotFound => "absent",
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => "permission_denied",
        Err(_) => "unobserved",
    }
}

// OCI permits supplying the six mandatory devices as individual mounts. Accept
// only their matching kernel character-device identities, never a directory bind.
fn device_leaf(dest: &str, root: &str, kind: &str) -> bool {
    if !matches!(kind, "devtmpfs" | "tmpfs") || dest.strip_prefix("/dev") != Some(root) {
        return false;
    }
    let expected = match dest {
        "/dev/null" => 0x103,
        "/dev/zero" => 0x105,
        "/dev/full" => 0x107,
        "/dev/random" => 0x108,
        "/dev/urandom" => 0x109,
        "/dev/tty" => 0x500,
        "/dev/dxg" => return presence(dest) == "character_device",
        _ => return false,
    };
    fs::symlink_metadata(dest).is_ok_and(|m| m.file_type().is_char_device() && m.rdev() == expected)
}

// An opt-in comparison on a diagnostic-only file whose DAC permissions allow a
// write-open. The probe never writes to it. EROFS demonstrates the mount boundary.
fn readonly_control() -> &'static str {
    const PATH: &str = "/opt/orange/qa/readonly-control";
    let meta = match fs::symlink_metadata(PATH) {
        Ok(value) => value,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return "fixture_absent",
        Err(_) => return "unobserved",
    };
    if !meta.is_file()
        || meta.uid() != 65532
        || meta.mode() & 0o777 != 0o600
        || meta.nlink() != 1
        || !small(PATH, 64).is_ok_and(|s| s == "orange-readonly-control.v1\n")
    {
        return "unexpected_fixture";
    }
    match File::options().write(true).open(PATH) {
        Err(e) if e.raw_os_error() == Some(30) => "readonly_filesystem",
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => "permission_denied",
        Err(_) => "unobserved",
        Ok(_) => "write_open_allowed",
    }
}

// Exercise only this process's verified small hard limit. Never allocate tasks,
// GPU memory, network traffic or unbounded system resources.
fn descriptor_limit() -> &'static str {
    let Ok(limits) = small("/proc/self/limits", 16384) else {
        return "unobserved";
    };
    let bounded = limits
        .lines()
        .find_map(|l| l.strip_prefix("Max open files"))
        .is_some_and(|v| v.split_whitespace().collect::<Vec<_>>() == ["256", "256", "files"]);
    if !bounded
        || !fs::symlink_metadata("/dev/null")
            .is_ok_and(|m| m.file_type().is_char_device() && m.rdev() == 0x103)
    {
        return "unexpected_fixture";
    }
    let mut private = Vec::with_capacity(256);
    let mut cause = "limit_not_observed";
    for _ in 0..=256 {
        match File::open("/dev/null") {
            Ok(fd) => private.push(fd),
            Err(e) => {
                cause = if e.raw_os_error() == Some(24) {
                    "process_descriptor_limit"
                } else {
                    "unconfirmed_failure"
                };
                break;
            }
        }
    }
    drop(private);
    if cause == "process_descriptor_limit" && File::open("/dev/null").is_ok() {
        "process_limit_released"
    } else {
        cause
    }
}

fn observe(hold: bool, exercise: bool) -> io::Result<bool> {
    let status = small("/proc/self/status", 65536)?;
    let fact = |key: &str| {
        status.lines().find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v.trim())
        })
    };
    let ids = |key: &str| {
        fact(key).is_some_and(|v| v.split_whitespace().collect::<Vec<_>>() == ["65532"; 4])
    };
    let identity = ids("Uid") && ids("Gid");
    let no_priv = fact("NoNewPrivs") == Some("1");
    let seccomp = fact("Seccomp") == Some("2");
    let no_caps = ["CapEff", "CapPrm", "CapInh", "CapAmb", "CapBnd"]
        .iter()
        .all(|k| fact(k).is_some_and(|v| u64::from_str_radix(v, 16) == Ok(0)));
    let net = small("/proc/net/dev", 65536)?;
    let external = net
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(iface, _)| iface.trim() != "lo")
        .count();
    let ip = match UdpSocket::bind("127.0.0.1:0") {
        Ok(_) => "creation_allowed",
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => "permission_denied",
        Err(_) => "unobserved",
    };
    let mounts = small("/proc/self/mountinfo", 1024 * 1024)?;
    let mut readonly = false;
    let mut tmp = false;
    let mut control = false;
    let mut unexpected = 0;
    let mut device_leaves = 0;
    for line in mounts.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            return Err(io::ErrorKind::InvalidData.into());
        };
        let fields: Vec<_> = left.split_whitespace().collect();
        if fields.len() < 6 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let kind = right.split_whitespace().next().unwrap_or("");
        let option = |key: &str| fields[5].split(',').any(|v| v == key);
        let private_tmp = kind == "tmpfs"
            && fields[3] == "/"
            && ["rw", "nodev", "nosuid", "noexec"]
                .iter()
                .all(|key| option(key));
        match fields[4] {
            "/" => readonly = option("ro"),
            "/tmp" => tmp = private_tmp,
            "/run/orange" => control = private_tmp,
            "/proc" if kind == "proc" && option("ro") => {}
            "/dev" if kind == "tmpfs" => {}
            "/proc/bus" | "/proc/fs" | "/proc/irq" | "/proc/sys"
                if kind == "proc" && option("ro") => {}
            "/proc/acpi"
            | "/proc/asound"
            | "/proc/kcore"
            | "/proc/keys"
            | "/proc/latency_stats"
            | "/proc/scsi"
            | "/proc/timer_list"
            | "/proc/sysrq-trigger"
            | "/proc/sys/fs/binfmt_misc"
                if matches!(kind, "devtmpfs" | "tmpfs") => {}
            path if device_leaf(path, fields[3], kind) => device_leaves += 1,
            _ => unexpected += 1,
        }
    }
    let unix = UnixStream::pair()
        .and_then(|(mut writer, mut reader)| {
            writer.write_all(b"oa")?;
            let mut reply = [0; 2];
            reader.read_exact(&mut reply)?;
            Ok(reply == *b"oa")
        })
        .unwrap_or(false);
    let windows = presence("/mnt/c");
    let docker = presence("/var/run/docker.sock");
    let containerd = presence("/run/containerd/containerd.sock");
    let interop = presence("/run/WSL");
    let gpu = presence("/dev/dxg");
    let private = identity
        && no_priv
        && seccomp
        && no_caps
        && external == 0
        && ip == "permission_denied"
        && readonly
        && tmp
        && control
        && unix
        && unexpected == 0
        && [windows, docker, containerd, interop]
            .iter()
            .all(|v| matches!(*v, "absent" | "permission_denied"));
    let (readonly_fixture, fd_limit) = if !exercise {
        ("not_requested", "not_requested")
    } else if private {
        (readonly_control(), descriptor_limit())
    } else {
        (
            "unconfirmed_process_boundary",
            "unconfirmed_process_boundary",
        )
    };
    let controls_ok = !exercise
        || (readonly_fixture == "readonly_filesystem" && fd_limit == "process_limit_released");
    let checked = private && controls_ok && gpu == "character_device";
    // All interpolated strings below are fixed observation enums; never echo process or mount data.
    println!(
        r#"{{"protocol":1,"status":"{}","uid_gid_match":{identity},"no_new_privileges":{no_priv},"seccomp_filter":{seccomp},"capabilities_empty":{no_caps},"root_readonly":{readonly},"private_tmp":{tmp},"private_control":{control},"unix_round_trip":{unix},"unexpected_mounts":{unexpected},"device_leaf_mounts":{device_leaves},"nonloopback_interfaces":{external},"inet_socket":"{ip}","windows_volume":"{windows}","container_socket":"{docker}","containerd_socket":"{containerd}","interop":"{interop}","gpu_bridge":"{gpu}","process_policy_match":{private},"readonly_fixture":"{readonly_fixture}","descriptor_test":"{fd_limit}","runtime_verified":false,"supply_authorized":false}}"#,
        if checked {
            "observations_match"
        } else {
            "observations_incomplete"
        }
    );
    io::stdout().flush()?;
    if hold {
        std::thread::sleep(Duration::from_secs(5));
    }
    Ok(checked)
}
fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let hold = args.as_slice() == [std::ffi::OsStr::new("--hold-for-inspection")];
    let exercise = hold || args.as_slice() == [std::ffi::OsStr::new("--bounded-controls")];
    if !args.is_empty() && !exercise {
        std::process::exit(2);
    }
    match observe(hold, exercise) {
        Ok(true) => {}
        Ok(false) => std::process::exit(2),
        Err(_) => {
            println!(
                r#"{{"protocol":1,"status":"observation_unavailable","runtime_verified":false,"supply_authorized":false}}"#
            );
            std::process::exit(2);
        }
    }
}
