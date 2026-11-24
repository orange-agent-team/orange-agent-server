//! Launch a verified release binary against the schema it actually shipped.
use super::*;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub(super) struct HistoricalGateway {
    child: Child,
    _config: tempfile::NamedTempFile,
    pub(super) port: u16,
}
impl HistoricalGateway {
    pub(super) async fn start(version: u32, url: &str, artifacts: Option<&Path>) -> Self {
        let (variable, prefix) = match version {
            50 => ("ORANGE_HISTORICAL_GATEWAY_50", "orange_asr_old_"),
            51 => ("ORANGE_HISTORICAL_GATEWAY_51", "orange_audio_old_"),
            _ => panic!("unknown historical gateway"),
        };
        let db = reqwest::Url::parse(url).expect("owned database URL");
        assert!(matches!(db.scheme(), "postgres" | "postgresql"));
        assert!(matches!(
            db.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]")
        ));
        let name = db
            .path()
            .trim_start_matches('/')
            .strip_prefix(prefix)
            .expect("dedicated historical database");
        assert_eq!(name.len(), 32);
        assert!(name.bytes().all(|v| v.is_ascii_hexdigit()));
        let tls =
            PathBuf::from(std::env::var_os("ORANGE_WORKER_TLS_DIR").expect("test TLS directory"))
                .canonicalize()
                .expect("test TLS directory");
        let conf =
            tempfile::NamedTempFile::new_in(&tls).expect("private historical gateway config");
        serde_json::to_writer(conf.as_file(),&serde_json::json!({
            "gateway_instance_id":Uuid::new_v4().to_string(),"server_certificate_file":tls.join("server.pem"),
            "server_key_file":tls.join("server.key"),"client_ca_file":tls.join("ca.pem")
        })).expect("gateway config");
        conf.as_file().sync_all().expect("flush gateway config");
        let exe = PathBuf::from(std::env::var_os(variable).expect("verified historical binary"));
        assert!(exe.is_absolute() && exe.is_file());
        let mut cmd = Command::new(exe);
        cmd.env_clear()
            .env("DATABASE_URL", url)
            .env("ORANGE_ENV", "development")
            .env("ORANGE_WORKER_GATEWAY_CONFIG", conf.path())
            .env("ORANGE_WORKER_GATEWAY_BIND", "127.0.0.1:0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = artifacts {
            cmd.env("ORANGE_ARTIFACT_ROOT", path);
        }
        let mut child = cmd.spawn().expect("historical gateway");
        let mut lines = BufReader::new(child.stderr.take().expect("readiness")).lines();
        let port = tokio::time::timeout(Duration::from_secs(35), async {
            while let Some(line) = lines.next_line().await.expect("read startup event") {
                if let Some(value) = line
                    .strip_prefix("Development standalone Worker mTLS control on ")
                    .and_then(|s| s.split_once(';'))
                    .map(|v| v.0)
                {
                    let address: SocketAddr = value.parse().expect("gateway address");
                    assert!(address.ip().is_loopback() && address.port() != 0);
                    return address.port();
                }
            }
            panic!("historical gateway exited before readiness")
        })
        .await
        .expect("historical gateway startup deadline");
        // Keep the child's log pipe draining for its entire lifetime without echoing configuration.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Self {
            child,
            port,
            _config: conf,
        }
    }
    pub(super) async fn stop(mut self) {
        let pid = self
            .child
            .id()
            .expect("live historical gateway")
            .to_string();
        let signal = Command::new("/bin/kill")
            .args(["-INT", &pid])
            .status()
            .await
            .expect("signal owned gateway");
        assert!(signal.success());
        let status = tokio::time::timeout(Duration::from_secs(15), self.child.wait())
            .await
            .expect("historical gateway shutdown deadline")
            .expect("gateway exit");
        assert!(
            status.success(),
            "unexpected historical gateway exit: {status}"
        );
    }
}
