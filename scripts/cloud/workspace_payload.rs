//! Fixed managed-workspace bootstrap. Accept one bounded UTF-8 input and hold
//! its verified tmpfs copy until the supervisor stops this allocation.
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::process::ExitCode;

fn start() -> Result<(), Box<dyn std::error::Error>> {
    let status = fs::read_to_string("/proc/self/status")?;
    let uid = status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .ok_or("uid unavailable")?;
    let caps = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .ok_or("capabilities unavailable")?;
    if uid.split_whitespace().collect::<Vec<_>>() != ["65534"; 4]
        || u64::from_str_radix(caps.trim(), 16)? != 0
    {
        return Err("unexpected workload identity".into());
    }
    let mut input = io::stdin().lock();
    let mut length = [0u8; 4];
    input.read_exact(&mut length)?;
    let size = u32::from_be_bytes(length) as usize;
    if !(1..=262144).contains(&size) {
        return Err("invalid input size".into());
    }
    let mut data = vec![0; size];
    input.read_exact(&mut data)?;
    std::str::from_utf8(&data)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open("/workspace/input.txt")?;
    file.write_all(&data)?;
    file.sync_all()?;
    drop(file);
    if fs::read("/workspace/input.txt")? != data {
        return Err("workspace input differs".into());
    }
    drop(data);
    println!("ORANGE_WORKSPACE_PROBE_READY");
    io::stdout().flush()?;
    let mut control = [0u8; 9];
    input.read_exact(&mut control)?;
    if &control != b"continue\n" {
        return Err("unexpected lifecycle command".into());
    }
    println!("ORANGE_WORKSPACE_PROBE_OK");
    Ok(())
}
fn main() -> ExitCode {
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Workspace preparation failed: {e}");
            ExitCode::FAILURE
        }
    }
}
