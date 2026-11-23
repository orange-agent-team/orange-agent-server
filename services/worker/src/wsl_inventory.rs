//! Read the current user's WSL registrations without starting a distribution.
use serde::Serialize;
#[cfg(any(windows, test))]
use {
    crate::probe_command,
    std::{collections::BTreeSet, path::Path, time::Duration},
};

#[derive(Debug, Copy, Clone, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WslStatus {
    Observed,
    UnsupportedPlatform,
    ToolUnavailable,
    QueryFailed,
    InvalidOutput,
    TimedOut,
    CleanupUnconfirmed,
}

#[derive(Debug, Serialize, Eq, PartialEq)]
pub struct DistributionCounts {
    pub wsl1: u16,
    pub wsl2: u16,
}

#[derive(Debug, Serialize)]
pub struct WslInventory {
    pub version: u8,
    pub status: WslStatus,
    pub distributions: Option<DistributionCounts>,
}

#[cfg(any(windows, test))]
fn text(bytes: &[u8]) -> Result<String, WslStatus> {
    if bytes.len() > probe_command::MAX_OUTPUT {
        return Err(WslStatus::InvalidOutput);
    }
    let wide = bytes.starts_with(&[0xff, 0xfe]) || bytes.contains(&0);
    let decoded = if wide {
        let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
        if !bytes.len().is_multiple_of(2) {
            return Err(WslStatus::InvalidOutput);
        }
        let units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        String::from_utf16(&units).map_err(|_| WslStatus::InvalidOutput)?
    } else {
        std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .map_err(|_| WslStatus::InvalidOutput)?
            .to_owned()
    };
    if decoded
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(WslStatus::InvalidOutput);
    }
    Ok(decoded)
}

#[cfg(any(windows, test))]
fn counts(quiet: &str, verbose: &str) -> Result<DistributionCounts, WslStatus> {
    let mut expected = BTreeSet::new();
    for name in quiet.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if expected.len() >= 256
            || name.len() > 256
            || name.starts_with('*')
            || !expected.insert(name)
        {
            return Err(WslStatus::InvalidOutput);
        }
    }
    let mut result = DistributionCounts { wsl1: 0, wsl2: 0 };
    let mut rows = verbose
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    if expected.is_empty() {
        return if rows.next().is_none() {
            Ok(result)
        } else {
            Err(WslStatus::InvalidOutput)
        };
    }
    // The header is localized. The row count, unique names, state and final generation
    // are checked against an independent names-only listing. No name enters the report.
    rows.next().ok_or(WslStatus::InvalidOutput)?;
    let mut seen = BTreeSet::new();
    for row in rows {
        let row = row.strip_prefix('*').unwrap_or(row).trim_start();
        let (name_state, version) = row
            .rsplit_once(char::is_whitespace)
            .ok_or(WslStatus::InvalidOutput)?;
        // Prefer the complete name when registrations share a prefix. Names are
        // checked against the original set even after a previous row was matched.
        let name = expected
            .iter()
            .copied()
            .filter(|name| {
                name_state.strip_prefix(name).is_some_and(|tail| {
                    tail.starts_with(char::is_whitespace) && !tail.trim().is_empty()
                })
            })
            .max_by_key(|name| name.len())
            .ok_or(WslStatus::InvalidOutput)?;
        if !seen.insert(name) {
            return Err(WslStatus::InvalidOutput);
        }
        match version {
            "1" => result.wsl1 += 1,
            "2" => result.wsl2 += 1,
            _ => return Err(WslStatus::InvalidOutput),
        }
    }
    if seen != expected {
        return Err(WslStatus::InvalidOutput);
    }
    Ok(result)
}

#[cfg(any(windows, test))]
async fn inspect(tool: &Path) -> Result<DistributionCounts, WslStatus> {
    async fn list(tool: &Path, switch: &str) -> Result<String, WslStatus> {
        use probe_command::ProbeError;
        let bytes = probe_command::read(tool, &["--list", switch], Duration::from_secs(5))
            .await
            .map_err(|e| match e {
                ProbeError::Process => WslStatus::QueryFailed,
                ProbeError::Output => WslStatus::InvalidOutput,
                ProbeError::Timeout => WslStatus::TimedOut,
                ProbeError::Cleanup => WslStatus::CleanupUnconfirmed,
            })?;
        text(&bytes)
    }
    let quiet = list(tool, "--quiet").await?;
    if quiet.trim().is_empty() {
        return counts(&quiet, "");
    }
    counts(&quiet, &list(tool, "--verbose").await?)
}

#[cfg(windows)]
fn system_wsl() -> Option<std::path::PathBuf> {
    use std::path::{Component, PathBuf, Prefix};
    let key = windows_registry::LOCAL_MACHINE
        .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()?;
    let base = PathBuf::from(key.get_string("SystemRoot").ok()?);
    if !base.is_absolute()
        || !matches!(base.components().next(),Some(Component::Prefix(p)) if matches!(p.kind(),Prefix::Disk(_)))
        || base.components().any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    let exe = base.join(r"System32\wsl.exe");
    exe.is_file().then_some(exe)
}

pub async fn observe() -> WslInventory {
    #[cfg(windows)]
    let result = match system_wsl() {
        Some(tool) => inspect(&tool).await,
        None => Err(WslStatus::ToolUnavailable),
    };
    #[cfg(not(windows))]
    let result = Err(WslStatus::UnsupportedPlatform);
    match result {
        Ok(counts) => WslInventory {
            version: 1,
            status: WslStatus::Observed,
            distributions: Some(counts),
        },
        Err(status) => WslInventory {
            version: 1,
            status,
            distributions: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localized_rows_match_private_names_and_only_publish_counts() {
        let quiet = "Ubuntu\r\n本地 环境\r\n";
        let verbose = "  名称              状态     版本\r\n* 本地 环境           已停止   1\r\n  Ubuntu             运行中   2\r\n";
        let utf16 = |s: &str| {
            s.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        };
        let observed = counts(
            &text(&utf16(quiet)).expect("decode UTF-16 names"),
            &text(&utf16(verbose)).expect("decode localized table"),
        )
        .expect("parse both listings");
        assert_eq!(
            serde_json::to_value(&observed).expect("serialize count-only report"),
            serde_json::json!({"wsl1":1,"wsl2":1})
        );
        assert_eq!(text(quiet.as_bytes()).expect("decode UTF-8 report"), quiet);
        assert_eq!(
            text(&[0xff, 0xfe, 0x00, 0xd8]),
            Err(WslStatus::InvalidOutput)
        );
        assert_eq!(
            counts("", "").expect("read empty inventory"),
            DistributionCounts { wsl1: 0, wsl2: 0 }
        );
        assert_eq!(
            counts(
                "Lab\nLab tools",
                "header\n* Lab tools   Running 2\n  Lab   Stopped 1"
            )
            .expect("resolve overlapping names"),
            DistributionCounts { wsl1: 1, wsl2: 1 }
        );
        for (names, rows) in [
            ("A\nA", "header\nA Running 2"),
            ("A", "header\nB Running 2"),
            ("A\nB", "header\nA Running 2"),
            ("A", "header\nA Running 3"),
            ("A", "header\nA   2"),
            ("A", "header\nA Running 2\nA Running 2"),
            ("WSL requires setup", "please install before use"),
        ] {
            assert_eq!(counts(names, rows), Err(WslStatus::InvalidOutput));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn only_uses_the_two_read_queries_and_keeps_failures_distinct() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let temp = tempfile::tempdir().expect("private fixture directory");
        let exe = temp.path().join("wsl-fixture");
        let write = |script: &str| {
            fs::write(&exe, format!("#!/bin/sh\n{script}\n")).expect("write query fixture");
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o700))
                .expect("set fixture permissions");
        };
        write(
            r#"[ "$#" -eq 2 ] && [ "$1" = '--list' ] || exit 7
case "$2" in --quiet) echo private-name ;; --verbose) printf 'NAME STATE VERSION\n* private-name Stopped 2\n';; *) exit 8;; esac"#,
        );
        assert_eq!(
            inspect(&exe).await.expect("read WSL 2 observation"),
            DistributionCounts { wsl1: 0, wsl2: 1 }
        );
        write(r#"[ "$2" = '--quiet' ] || exit 7"#);
        assert_eq!(
            inspect(&exe).await.expect("read empty registration list"),
            DistributionCounts { wsl1: 0, wsl2: 0 }
        );
        write("exit 8");
        assert_eq!(inspect(&exe).await, Err(WslStatus::QueryFailed));
        write("head -c 65537 /dev/zero");
        assert_eq!(inspect(&exe).await, Err(WslStatus::InvalidOutput));
    }
}
