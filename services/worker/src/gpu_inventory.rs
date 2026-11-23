//! One local, read-only NVIDIA management observation. Never produces a resource grant.
use serde::Serialize;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

const MAX_OUTPUT: usize = 65536;
const MAX_DEVICES: usize = 64;
const QUERY: &str = "--query-gpu=uuid,pci.bus_id,driver_version,memory.total,memory.free,name";

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct GpuObservation {
    pub uuid: String,
    pub pci_bus_id: Option<String>,
    pub driver_version: String,
    pub total_mib: Option<String>,
    pub free_mib: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryStatus {
    Observed,
    NoDevicesObserved,
    UnsupportedPlatform,
    DriverToolUnavailable,
    DriverQueryFailed,
    TimedOut,
    InvalidDriverOutput,
    ProbeCleanupUnconfirmed,
}

#[derive(Debug, Serialize)]
pub struct Inventory {
    pub version: u8,
    pub os: &'static str,
    pub architecture: &'static str,
    pub status: InventoryStatus,
    pub devices: Vec<GpuObservation>,
}

fn missing(value: &str) -> bool {
    matches!(value, "N/A" | "[N/A]" | "[Not Supported]" | "Not Supported")
}

fn mib(value: &str) -> Result<Option<u64>, InventoryStatus> {
    if missing(value) {
        return Ok(None);
    }
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(InventoryStatus::InvalidDriverOutput);
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| InventoryStatus::InvalidDriverOutput)
}

fn bus_id(value: &str) -> Result<Option<String>, InventoryStatus> {
    if missing(value) {
        return Ok(None);
    }
    let (address, function) = value
        .rsplit_once('.')
        .ok_or(InventoryStatus::InvalidDriverOutput)?;
    let parts: Vec<_> = address.split(':').collect();
    if parts.len() != 3
        || !matches!(parts[0].len(), 4 | 8)
        || parts[1].len() != 2
        || parts[2].len() != 2
        || function.len() != 1
        || !function.bytes().all(|b| (b'0'..=b'7').contains(&b))
        || !parts
            .iter()
            .all(|v| v.bytes().all(|b| b.is_ascii_hexdigit()))
        || u8::from_str_radix(parts[2], 16).map_err(|_| InventoryStatus::InvalidDriverOutput)? > 31
    {
        return Err(InventoryStatus::InvalidDriverOutput);
    }
    Ok(Some(value.to_ascii_lowercase()))
}

fn decode(bytes: &[u8]) -> Result<Vec<GpuObservation>, InventoryStatus> {
    use InventoryStatus::InvalidDriverOutput as Invalid;
    if bytes.len() > MAX_OUTPUT {
        return Err(Invalid);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Invalid)?;
    let mut devices = Vec::new();
    let mut identities = BTreeSet::new();
    for line in text.lines() {
        if devices.len() >= MAX_DEVICES || line.is_empty() {
            return Err(Invalid);
        }
        // Only the final field is free text. Never split a comma inside the product name.
        let mut columns = line.splitn(6, ',').map(str::trim);
        let raw = columns
            .next()
            .ok_or(Invalid)?
            .strip_prefix("GPU-")
            .ok_or(Invalid)?;
        let id = Uuid::parse_str(raw).map_err(|_| Invalid)?;
        if id.is_nil()
            || !id.hyphenated().to_string().eq_ignore_ascii_case(raw)
            || !identities.insert(id)
        {
            return Err(Invalid);
        }
        let pci = bus_id(columns.next().ok_or(Invalid)?)?;
        let driver = columns.next().ok_or(Invalid)?;
        if driver.len() > 32
            || !driver.contains('.')
            || !driver
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(Invalid);
        }
        let total = mib(columns.next().ok_or(Invalid)?)?;
        let free = mib(columns.next().ok_or(Invalid)?)?;
        if matches!(total, Some(0)) || total.zip(free).is_some_and(|(t, f)| f > t) {
            return Err(Invalid);
        }
        let mut name = columns.next().ok_or(Invalid)?;
        if name.starts_with('"') || name.ends_with('"') {
            name = name
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .ok_or(Invalid)?;
        }
        if name.is_empty()
            || missing(name)
            || name.len() > 128
            || name.contains('"')
            || name.chars().any(char::is_control)
        {
            return Err(Invalid);
        }
        devices.push(GpuObservation {
            uuid: format!("GPU-{id}"),
            pci_bus_id: pci,
            driver_version: driver.to_string(),
            total_mib: total.map(|n| n.to_string()),
            free_mib: free.map(|n| n.to_string()),
            name: name.to_string(),
        });
    }
    devices.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    Ok(devices)
}

fn system_tools() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        vec![
            PathBuf::from("/usr/bin/nvidia-smi"),
            PathBuf::from("/usr/lib/wsl/lib/nvidia-smi"),
        ]
    }
    #[cfg(target_os = "windows")]
    {
        [
            ("SystemRoot", "System32\\nvidia-smi.exe"),
            ("ProgramW6432", "NVIDIA Corporation\\NVSMI\\nvidia-smi.exe"),
        ]
        .into_iter()
        .filter_map(|(var, suffix)| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .filter(|base| {
                    use std::path::{Component, Prefix};
                    base.is_absolute()
                        && matches!(base.components().next(),Some(Component::Prefix(p))
                        if matches!(p.kind(),Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
                        && !base.components().any(|c| c == Component::ParentDir)
                })
                .map(|base| base.join(suffix))
        })
        .collect()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Vec::new()
    }
}

async fn query(tool: &Path, timeout: Duration) -> Result<Vec<GpuObservation>, InventoryStatus> {
    let mut child = Command::new(tool)
        .args([QUERY, "--format=csv,noheader,nounits"])
        .current_dir(tool.parent().ok_or(InventoryStatus::DriverQueryFailed)?)
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| InventoryStatus::DriverQueryFailed)?;
    let pipe = child
        .stdout
        .take()
        .ok_or(InventoryStatus::DriverQueryFailed)?;
    let run = async {
        let mut output = Vec::new();
        pipe.take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut output)
            .await
            .map_err(|_| InventoryStatus::DriverQueryFailed)?;
        if output.len() > MAX_OUTPUT {
            return Err(InventoryStatus::InvalidDriverOutput);
        }
        if !child
            .wait()
            .await
            .map_err(|_| InventoryStatus::DriverQueryFailed)?
            .success()
        {
            return Err(InventoryStatus::DriverQueryFailed);
        }
        decode(&output)
    };
    let result = tokio::time::timeout(timeout, run)
        .await
        .unwrap_or(Err(InventoryStatus::TimedOut));
    if result.is_err() {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .map_err(|_| InventoryStatus::ProbeCleanupUnconfirmed)?
            .map_err(|_| InventoryStatus::ProbeCleanupUnconfirmed)?;
    }
    result
}

pub async fn observe() -> Inventory {
    let tools = system_tools();
    let result = if !cfg!(any(target_os = "linux", target_os = "windows")) {
        Err(InventoryStatus::UnsupportedPlatform)
    } else if let Some(tool) = tools.iter().find(|path| path.is_file()) {
        query(tool, Duration::from_secs(5)).await
    } else {
        Err(InventoryStatus::DriverToolUnavailable)
    };
    let (status, devices) = match result {
        Ok(devices) if devices.is_empty() => (InventoryStatus::NoDevicesObserved, devices),
        Ok(devices) => (InventoryStatus::Observed, devices),
        Err(status) => (status, Vec::new()),
    };
    Inventory {
        version: 1,
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        status,
        devices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "GPU-31000000-0000-4000-8000-000000000001";
    const SECOND: &str = "GPU-31000000-0000-4000-8000-000000000002";

    #[test]
    fn multi_device_order_and_missing_measurements_are_explicit() {
        let rows = format!(
            "{SECOND}, N/A, 591.04, [N/A], [Not Supported], NVIDIA synthetic B\r\n\
                            {FIRST}, 00000000:65:00.0, 591.04, 98304, 97000, \"NVIDIA synthetic, A\"\r\n"
        );
        let devices = decode(rows.as_bytes()).expect("two synthetic board records");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].uuid, FIRST);
        assert_eq!(devices[0].name, "NVIDIA synthetic, A");
        assert_eq!(devices[0].total_mib.as_deref(), Some("98304"));
        assert_eq!(devices[0].free_mib.as_deref(), Some("97000"));
        assert_eq!(devices[1].pci_bus_id, None);
        assert_eq!(devices[1].free_mib, None);
        assert_eq!(devices[1].total_mib, None);
        assert!(decode(b"").expect("no rows").is_empty());
    }

    #[test]
    fn contradictory_or_unbounded_reports_do_not_publish_partial_inventory() {
        let valid = format!("{FIRST}, 0000:65:00.0, 591.04, 1024, 3, reviewed board");
        for invalid in [
            format!("{valid}\n{valid}"),
            valid.replace(FIRST, "0"),
            valid.replace(FIRST, "GPU-00000000-0000-0000-0000-000000000000"),
            valid.replace("65:00.0", "65:20.0"),
            valid.replace("65:00.0", "65:00.8"),
            valid.replace("591.04", "N/A"),
            valid.replace("1024, 3", "3, 1024"),
            valid.replace("1024, 3", "0, 0"),
            valid.replace("1024, 3", "1.5, 0"),
            valid.replace("reviewed board", ""),
            valid.replace("reviewed board", "board\tcontrol"),
            "x".repeat(MAX_OUTPUT + 1),
        ] {
            assert_eq!(
                decode(invalid.as_bytes()),
                Err(InventoryStatus::InvalidDriverOutput)
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subprocess_uses_only_read_arguments_times_out_and_discards_failed_output() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().expect("fixture root");
        let tool = fixture.path().join("test-driver");
        for (script, expected) in [
            (
                format!(
                    "#!/bin/sh\n[ \"$#\" -eq 2 ] && [ \"$1\" = '{QUERY}' ] && [ \"$2\" = '--format=csv,noheader,nounits' ] || exit 8\nprintf '%s\\n' '{FIRST}, N/A, 591.04, N/A, N/A, driver fixture'\n"
                ),
                None,
            ),
            (
                "#!/bin/sh\nexec /bin/sleep 5\n".into(),
                Some(InventoryStatus::TimedOut),
            ),
            (
                "#!/bin/sh\nexit 9\n".into(),
                Some(InventoryStatus::DriverQueryFailed),
            ),
            (
                "#!/bin/sh\nhead -c 65537 /dev/zero\n".into(),
                Some(InventoryStatus::InvalidDriverOutput),
            ),
        ] {
            std::fs::write(&tool, script).expect("fixture command");
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700))
                .expect("fixture mode");
            let timeout = if expected == Some(InventoryStatus::TimedOut) {
                Duration::from_millis(150)
            } else {
                Duration::from_secs(5)
            };
            let result = query(&tool, timeout).await;
            if let Some(status) = expected {
                assert_eq!(result, Err(status));
            } else {
                assert_eq!(result.expect("read-only invocation").len(), 1);
            }
        }
    }
}
