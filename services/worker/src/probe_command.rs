//! Bounded, fixed-argument local driver queries.
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub(crate) const MAX_OUTPUT: usize = 65536;
pub(crate) enum ProbeError {
    Process,
    Output,
    Timeout,
    Cleanup,
}
pub(crate) async fn read(
    tool: &Path,
    arguments: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, ProbeError> {
    let mut child = Command::new(tool)
        .args(arguments)
        .current_dir(tool.parent().ok_or(ProbeError::Process)?)
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ProbeError::Process)?;
    let pipe = child.stdout.take().ok_or(ProbeError::Process)?;
    let run = async {
        let mut output = Vec::new();
        pipe.take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut output)
            .await
            .map_err(|_| ProbeError::Process)?;
        if output.len() > MAX_OUTPUT {
            return Err(ProbeError::Output);
        }
        if !child
            .wait()
            .await
            .map_err(|_| ProbeError::Process)?
            .success()
        {
            return Err(ProbeError::Process);
        }
        Ok(output)
    };
    let result = tokio::time::timeout(timeout, run)
        .await
        .unwrap_or(Err(ProbeError::Timeout));
    if result.is_err() {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .map_err(|_| ProbeError::Cleanup)?
            .map_err(|_| ProbeError::Cleanup)?;
    }
    result
}
