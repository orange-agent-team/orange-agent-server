//! Fixed managed-workspace bootstrap. Accept one bounded UTF-8 input and hold
//! its verified tmpfs copy until the supervisor stops this allocation.
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Write};
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
    let mut control = String::new();
    loop {
        control.clear();
        if input.by_ref().take(128).read_line(&mut control)? == 0 {
            return Err("supervisor disconnected".into());
        }
        if control == "continue\n" {
            break;
        }
        let limit = control
            .strip_prefix("list ")
            .and_then(|s| s.strip_suffix('\n'))
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| (1..=16).contains(n))
            .ok_or("unsupported workspace operation")?;
        let mut entries = Vec::new();
        let mut omitted = false;
        for item in fs::read_dir("/workspace")?.take(65) {
            let item = item?;
            let kind = item.file_type()?;
            let name = item.file_name();
            let Some(name) = name.to_str() else {
                omitted = true;
                continue;
            };
            if !kind.is_file() || name.len() > 128 {
                omitted = true;
                continue;
            }
            let mut encoded = String::new();
            for byte in name.bytes() {
                std::fmt::Write::write_fmt(&mut encoded, format_args!("{byte:02x}"))?;
            }
            entries.push((encoded, item.metadata()?.len()));
        }
        entries.sort();
        omitted |= entries.len() > limit;
        entries.truncate(limit);
        println!("DIRECTORY {} {}", entries.len(), u8::from(omitted));
        for (name, size) in entries {
            println!("FILE {name} {size}");
        }
        println!("END_DIRECTORY");
        io::stdout().flush()?;
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
