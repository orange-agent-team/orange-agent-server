use super::*;
use base64::Engine;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Clone)]
pub(super) struct Driver {
    script: std::path::PathBuf,
    archive: std::path::PathBuf,
    python: std::path::PathBuf,
}
impl Driver {
    pub fn new(
        root: &std::path::Path,
        archive: &std::path::Path,
        python: &std::path::Path,
    ) -> Result<Self, Error> {
        // Operator-provided installation files must match the source built into this Host.
        const FILES: &[(&str, &[u8])] = &[
            (
                "scripts/cloud-workspace-driver.py",
                include_bytes!("../../../../scripts/cloud-workspace-driver.py"),
            ),
            (
                "scripts/cloud_tools.py",
                include_bytes!("../../../../scripts/cloud_tools.py"),
            ),
            (
                "scripts/cloud/qualify_guest.py",
                include_bytes!("../../../../scripts/cloud/qualify_guest.py"),
            ),
            (
                "scripts/cloud/workspace_payload.rs",
                include_bytes!("../../../../scripts/cloud/workspace_payload.rs"),
            ),
            (
                "config/cloud-isolation.lock.json",
                include_bytes!("../../../../config/cloud-isolation.lock.json"),
            ),
        ];
        let root = root.canonicalize().map_err(|_| Error::Local)?;
        for (name, expected) in FILES {
            let path = root.join(name);
            if !path.is_file()
                || path.canonicalize().map_err(|_| Error::Local)? != path
                || std::fs::read(path).map_err(|_| Error::Local)? != *expected
            {
                return Err(Error::Local);
            }
        }
        let archive = archive.canonicalize().map_err(|_| Error::Local)?;
        let python = python.canonicalize().map_err(|_| Error::Local)?;
        if !python.is_file() || !archive.is_file() {
            return Err(Error::Local);
        }
        Ok(Self {
            script: root.join("scripts/cloud-workspace-driver.py"),
            archive,
            python,
        })
    }
    fn command(&self, verb: &str, allocation: &str) -> Command {
        let mut cmd = Command::new(&self.python);
        cmd.env_clear();
        for var in ["PATH", "HOME", "TMPDIR"] {
            if let Some(v) = std::env::var_os(var) {
                cmd.env(var, v);
            }
        }
        cmd.arg("-B")
            .arg("-E")
            .arg("-s")
            .arg(&self.script)
            .arg(verb)
            .arg(allocation)
            .arg("--archive")
            .arg(&self.archive)
            .kill_on_drop(true)
            .stderr(Stdio::inherit());
        cmd
    }
    pub async fn reserve(
        &self,
        lease: &host::WorkspaceLease,
        sha: &str,
        maximum: i64,
    ) -> Result<Value, Error> {
        let packet = json!({"host_instance_id":lease.host_instance_id,"run_id":lease.run_id,"workspace_id":lease.workspace_id,
            "allocation_id":lease.allocation_id,"epoch":lease.epoch,"input_sha256":sha,
            "lease_until":lease.lease_until.ok_or(Error::Driver)?.seconds,"workspace_until":maximum});
        let mut child = self
            .command("reserve", &lease.allocation_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|_| Error::Driver)?;
        let mut input = child.stdin.take().ok_or(Error::Driver)?;
        let mut bytes = serde_json::to_vec(&packet).map_err(|_| Error::Driver)?;
        bytes.push(b'\n');
        input.write_all(&bytes).await.map_err(|_| Error::Driver)?;
        drop(input);
        let response = tokio::time::timeout(Duration::from_secs(35), child.wait_with_output())
            .await
            .map_err(|_| Error::Driver)?
            .map_err(|_| Error::Driver)?;
        if !response.status.success() || response.stdout.len() > 8192 {
            return Err(Error::Driver);
        }
        serde_json::from_slice(&response.stdout).map_err(|_| Error::Driver)
    }
    pub async fn recover(&self, allocation: &str) -> Result<Value, Error> {
        let output = tokio::time::timeout(
            Duration::from_secs(45),
            self.command("recover", allocation)
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| Error::Driver)?
        .map_err(|_| Error::Driver)?;
        if !output.status.success() || output.stdout.len() > 16384 {
            return Err(Error::Driver);
        }
        serde_json::from_slice(&output.stdout).map_err(|_| Error::Driver)
    }
    pub async fn start(
        &self,
        lease: &host::WorkspaceLease,
        input: &host::WorkspaceInput,
        maximum: i64,
    ) -> Result<Session, Error> {
        let mut child = self
            .command("open", &lease.allocation_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|_| Error::NeverLaunched)?;
        let mut stdin = child.stdin.take().ok_or(Error::Driver)?;
        let stdout = BufReader::new(child.stdout.take().ok_or(Error::Driver)?).lines();
        let packet = json!({"host_instance_id":lease.host_instance_id,"run_id":lease.run_id,"workspace_id":lease.workspace_id,
            "allocation_id":lease.allocation_id,"epoch":lease.epoch,"input_sha256":input.sha256,
            "lease_until":lease.lease_until.ok_or(Error::Driver)?.seconds,"workspace_until":maximum,
            "content_base64":base64::engine::general_purpose::STANDARD.encode(&input.content)});
        let mut bytes = serde_json::to_vec(&packet).map_err(|_| Error::Driver)?;
        bytes.push(b'\n');
        stdin.write_all(&bytes).await.map_err(|_| Error::Driver)?;
        Ok(Session {
            child,
            stdin: Some(stdin),
            stdout,
        })
    }
}
pub(super) struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
}
impl Session {
    pub async fn next(&mut self) -> Result<Value, Error> {
        let line = self
            .stdout
            .next_line()
            .await
            .map_err(|_| Error::Driver)?
            .ok_or(Error::Driver)?;
        if line.len() > 16384 {
            return Err(Error::Driver);
        }
        serde_json::from_str(&line).map_err(|_| Error::Driver)
    }
    pub async fn renew(&mut self, expiry: i64) -> Result<(), Error> {
        self.stdin
            .as_mut()
            .ok_or(Error::Driver)?
            .write_all(format!("renew {expiry}\n").as_bytes())
            .await
            .map_err(|_| Error::Driver)
    }
    pub async fn stop(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"stop\n").await;
            let _ = stdin.shutdown().await;
        }
    }
    pub async fn exit(mut self) -> Result<(), Error> {
        self.stdin.take();
        let result = tokio::time::timeout(Duration::from_secs(40), self.child.wait())
            .await
            .map_err(|_| Error::Driver)?
            .map_err(|_| Error::Driver)?;
        if !result.success() {
            return Err(Error::Driver);
        }
        Ok(())
    }
}
