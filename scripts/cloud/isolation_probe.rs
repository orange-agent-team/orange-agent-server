//! Fixed diagnostic payload for the local cloud workspace qualification.
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

fn probe() -> Result<(), Box<dyn std::error::Error>> {
    println!("ORANGE_WORKSPACE_PROBE_READY");
    std::io::stdout().flush()?;
    let mut release = String::new();
    std::io::stdin().read_line(&mut release)?;
    if release != "continue\n" {
        return Err("supervisor did not authorize probe".into());
    }
    let port: u16 = std::env::args()
        .nth(1)
        .ok_or("missing sentinel port")?
        .parse()?;
    let status = fs::read_to_string("/proc/self/status")?;
    let mut uid = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or("missing uid")?
        .split_whitespace()
        .skip(1);
    if !uid.all(|v| v == "65534") {
        return Err("unexpected workload identity".into());
    }
    let cap = status
        .lines()
        .find_map(|s| s.strip_prefix("CapEff:"))
        .ok_or("missing effective capabilities")?;
    if u64::from_str_radix(cap.trim(), 16)? != 0 {
        return Err("effective capabilities present".into());
    }
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok() {
        return Err("guest loopback sentinel reached".into());
    }
    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open("/unexpected-write")
        .is_ok()
    {
        return Err("unexpected writable root".into());
    }
    let marker = std::env::args().nth(2).ok_or("missing guest marker path")?;
    match fs::metadata(marker) {
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {}
        _ => return Err("guest marker visible or inaccessible for unknown reason".into()),
    }
    fs::write("/workspace/receipt", b"isolated workspace")?;
    let mut bytes = Vec::new();
    fs::File::open("/workspace/receipt")?.read_to_end(&mut bytes)?;
    if bytes != b"isolated workspace" {
        return Err("workspace roundtrip failed".into());
    }
    let mut allocation = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open("/workspace/capacity")?;
    let block = [7u8; 65536];
    let mut stopped = false;
    for _ in 0..32 {
        match allocation.write_all(&block) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(28) || e.kind() == ErrorKind::StorageFull => {
                stopped = true;
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }
    if !stopped {
        return Err("workspace exceeded tmpfs allocation".into());
    }
    println!("ORANGE_WORKSPACE_PROBE_OK");
    Ok(())
}
fn main() -> ExitCode {
    match probe() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Probe failed: {e}");
            ExitCode::FAILURE
        }
    }
}
