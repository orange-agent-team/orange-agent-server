//! Shared bootstrap configuration for the development, user and admin HTTP servers.
use crate::telemetry::ApiRole;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fmt, fs,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct StartupError {
    pub code: &'static str,
    pub field: &'static str,
    pub source: &'static str,
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (field={}, source={})",
            self.code, self.field, self.source
        )
    }
}
impl std::error::Error for StartupError {}

type Result<T> = std::result::Result<T, StartupError>;

fn error(code: &'static str, field: &'static str, source: &'static str) -> StartupError {
    StartupError {
        code,
        field,
        source,
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Layer {
    environment: Option<String>,
    diagnostic_log: Option<String>,
    bind: Option<String>,
    metrics_bind: Option<String>,
    region: Option<String>,
    identity_file: Option<String>,
    database_url_file: Option<String>,
}

fn read_bytes(
    path: &Path,
    field: &'static str,
    source: &'static str,
    limit: u64,
) -> Result<Vec<u8>> {
    let fail = || error("CONFIG_FILE_UNREADABLE", field, source);
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| fail())?;
    let metadata = file.metadata().map_err(|_| fail())?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(fail());
    }
    #[cfg(unix)]
    if field == "database" {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(error("CONFIG_SECRET_PERMISSIONS", field, source));
        }
    }
    let mut data = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|_| fail())?;
    if data.len() as u64 > limit {
        return Err(fail());
    }
    Ok(data)
}

struct Cli {
    file: Option<String>,
    describe: bool,
    layer: Layer,
}

impl Cli {
    fn parse() -> Result<Self> {
        let mut parsed = Self {
            file: None,
            describe: false,
            layer: Layer::default(),
        };
        let mut args = env::args_os().skip(1);
        while let Some(flag) = args.next() {
            let (slot, field) = match flag.to_str() {
                Some("--describe-config") if !parsed.describe => {
                    parsed.describe = true;
                    continue;
                }
                Some("--config") => (&mut parsed.file, "config"),
                Some("--environment") => (&mut parsed.layer.environment, "environment"),
                Some("--diagnostic-log") => (&mut parsed.layer.diagnostic_log, "diagnostic_log"),
                Some("--bind") => (&mut parsed.layer.bind, "bind"),
                Some("--metrics-bind") => (&mut parsed.layer.metrics_bind, "metrics_bind"),
                Some("--region") => (&mut parsed.layer.region, "region"),
                Some("--identity-file") => (&mut parsed.layer.identity_file, "identity_file"),
                Some("--database-url-file") => (&mut parsed.layer.database_url_file, "database"),
                _ => return Err(error("CONFIG_ARGUMENT_INVALID", "argument", "cli")),
            };
            if slot.is_some() {
                return Err(error("CONFIG_CONFLICT", field, "cli"));
            }
            let value = args
                .next()
                .and_then(|s| s.into_string().ok())
                .filter(|s| !s.starts_with("--") && !s.is_empty())
                .ok_or_else(|| error("CONFIG_VALUE_MISSING", field, "cli"))?;
            *slot = Some(value);
        }
        Ok(parsed)
    }
}

fn variable(name: &str, field: &'static str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            Err(error("CONFIG_VALUE_INVALID", field, "environment"))
        }
    }
}

struct Setting {
    value: Option<String>,
    source: &'static str,
}
impl Setting {
    fn resolve(
        default: Option<&str>,
        file: Option<String>,
        environment: Option<String>,
        cli: Option<String>,
    ) -> Self {
        for (value, source) in [(cli, "cli"), (environment, "environment"), (file, "file")] {
            if value.is_some() {
                return Self { value, source };
            }
        }
        Self {
            value: default.map(str::to_owned),
            source: "default",
        }
    }
    fn required(&self, field: &'static str) -> Result<&str> {
        self.value
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| error("CONFIG_VALUE_MISSING", field, self.source))
    }
    fn address(&self, field: &'static str) -> Result<SocketAddr> {
        let a: SocketAddr = self
            .required(field)?
            .parse()
            .map_err(|_| error("CONFIG_VALUE_INVALID", field, self.source))?;
        if !a.ip().is_loopback() {
            return Err(error("CONFIG_ADDRESS_FORBIDDEN", field, self.source));
        }
        Ok(a)
    }
}

// No Debug implementation: database and identity values are confidential.
pub struct ApiSettings {
    pub bind: SocketAddr,
    pub diagnostic_log: bool,
    pub metrics_bind: Option<SocketAddr>,
    pub database_url: String,
    identity_path: PathBuf,
    sources: BTreeMap<&'static str, &'static str>,
    region: String,
    describe: bool,
    role: ApiRole,
}

impl ApiSettings {
    pub fn load(role: ApiRole) -> Result<Self> {
        let cli = Cli::parse()?;
        let (bind_env, id_env, port) = match role {
            ApiRole::User => (
                "ORANGE_USER_BIND",
                "ORANGE_USER_CONFIG_FILE",
                "127.0.0.1:43103",
            ),
            ApiRole::Admin => (
                "ORANGE_ADMIN_BIND",
                "ORANGE_ADMIN_CONFIG_FILE",
                "127.0.0.1:43102",
            ),
            ApiRole::Development => (
                "ORANGE_BIND",
                "ORANGE_DEV_IDENTITIES_FILE",
                "127.0.0.1:43100",
            ),
        };
        let file_choice = Setting::resolve(
            None,
            None,
            variable("ORANGE_API_SETTINGS_FILE", "config")?,
            cli.file,
        );
        let file: Layer = if let Some(path) = file_choice.value.as_ref() {
            serde_json::from_slice(&read_bytes(
                Path::new(path),
                "config",
                file_choice.source,
                65536,
            )?)
            .map_err(|_| error("CONFIG_FILE_INVALID", "config", file_choice.source))?
        } else {
            Layer::default()
        };
        let environment = Setting::resolve(
            None,
            file.environment,
            variable("ORANGE_ENV", "environment")?,
            cli.layer.environment,
        );
        if environment.required("environment")? != "development" {
            return Err(error(
                "CONFIG_ENVIRONMENT_FORBIDDEN",
                "environment",
                environment.source,
            ));
        }
        let region = Setting::resolve(
            Some("LOCAL"),
            file.region,
            variable("ORANGE_REGION", "region")?,
            cli.layer.region,
        );
        if !matches!(
            region.required("region")?,
            "LOCAL" | "US" | "JP" | "CN-MAINLAND" | "CN-HK"
        ) {
            return Err(error("CONFIG_VALUE_INVALID", "region", region.source));
        }
        let address = Setting::resolve(
            Some(port),
            file.bind,
            variable(bind_env, "bind")?,
            cli.layer.bind,
        );
        let bind = address.address("bind")?;
        let metrics = Setting::resolve(
            None,
            file.metrics_bind,
            variable("ORANGE_METRICS_BIND", "metrics_bind")?,
            cli.layer.metrics_bind,
        );
        let metrics_bind = match metrics.value.as_deref() {
            None | Some("off") => None,
            Some(_) => Some(metrics.address("metrics_bind")?),
        };
        if bind.port() != 0 && metrics_bind == Some(bind) {
            return Err(error("CONFIG_CONFLICT", "metrics_bind", metrics.source));
        }
        let identity = Setting::resolve(
            None,
            file.identity_file,
            variable(id_env, "identity_file")?,
            cli.layer.identity_file,
        );
        let resolve_path = |raw: &str, source: &str| {
            if source == "file"
                && let Some(parent) = file_choice
                    .value
                    .as_deref()
                    .map(Path::new)
                    .and_then(Path::parent)
            {
                return parent.join(raw);
            }
            PathBuf::from(raw)
        };
        let identity_path = resolve_path(identity.required("identity_file")?, identity.source);
        let direct = variable("DATABASE_URL", "database")?;
        let env_ref = variable("ORANGE_DATABASE_URL_FILE", "database")?;
        if cli.layer.database_url_file.is_none() && direct.is_some() && env_ref.is_some() {
            return Err(error("CONFIG_CONFLICT", "database", "environment"));
        }
        let dbfile = Setting::resolve(
            None,
            file.database_url_file,
            env_ref,
            cli.layer.database_url_file,
        );
        let (database_url, db_source) = if dbfile.source != "cli"
            && let Some(url) = direct
        {
            (url, "environment")
        } else {
            let path = resolve_path(dbfile.required("database")?, dbfile.source);
            let value = String::from_utf8(read_bytes(&path, "database", dbfile.source, 16384)?)
                .map_err(|_| error("CONFIG_VALUE_INVALID", "database", dbfile.source))?;
            (value.trim().to_owned(), dbfile.source)
        };
        if !database_url.starts_with("postgresql:") && !database_url.starts_with("postgres:")
            || database_url
                .parse::<sqlx::postgres::PgConnectOptions>()
                .is_err()
        {
            return Err(error("CONFIG_VALUE_INVALID", "database", db_source));
        }
        let log = Setting::resolve(
            Some("off"),
            file.diagnostic_log,
            variable("ORANGE_DIAGNOSTIC_LOG", "diagnostic_log")?,
            cli.layer.diagnostic_log,
        );
        let diagnostic_log = match log.value.as_deref() {
            Some("on") => true,
            Some("off") => false,
            _ => return Err(error("CONFIG_VALUE_INVALID", "diagnostic_log", log.source)),
        };
        let sources = BTreeMap::from([
            ("diagnostic_log", log.source),
            ("config", file_choice.source),
            ("environment", environment.source),
            ("region", region.source),
            ("bind", address.source),
            ("metrics_bind", metrics.source),
            ("identity_file", identity.source),
            ("database", db_source),
        ]);
        Ok(Self {
            diagnostic_log,
            bind,
            metrics_bind,
            database_url,
            identity_path,
            sources,
            describe: cli.describe,
            role,
            region: region.required("region")?.to_owned(),
        })
    }
    pub fn identity<T: DeserializeOwned>(&self) -> Result<T> {
        let src = self.sources["identity_file"];
        serde_json::from_slice(&read_bytes(
            &self.identity_path,
            "identity_file",
            src,
            2 * 1024 * 1024,
        )?)
        .map_err(|_| error("CONFIG_IDENTITY_INVALID", "identity_file", src))
    }
    pub fn validate_development(&self, identities: &[crate::http::DevIdentity]) -> Result<()> {
        crate::http::DevIdentity::validate_all(identities).map_err(|_| {
            error(
                "CONFIG_IDENTITY_INVALID",
                "identity_file",
                self.sources["identity_file"],
            )
        })
    }
    pub fn validate_oidc(&self, env: &str, id: &str, secret: &str) -> Result<()> {
        if env != "dev" || id.is_empty() || secret.is_empty() {
            return Err(error(
                "CONFIG_IDENTITY_INVALID",
                "identity_file",
                self.sources["identity_file"],
            ));
        }
        Ok(())
    }
    pub fn report(&self) -> Value {
        json!({"role":self.role.name(),"environment":"development","region":self.region,"bind":self.bind,"metrics_bind":self.metrics_bind,"diagnostic_log":self.diagnostic_log,"sources":self.sources})
    }
    pub fn describe(&self) -> bool {
        if self.describe {
            println!("{}", self.report());
        }
        self.describe
    }
}

/// Startup errors are public diagnostics. Underlying parser, database and issuer messages may contain secrets.
pub fn exit(result: std::result::Result<(), Box<dyn std::error::Error>>) -> std::process::ExitCode {
    let Err(cause) = result else {
        return std::process::ExitCode::SUCCESS;
    };
    let envelope = if let Some(config) = cause.downcast_ref::<StartupError>() {
        json!({"event":"startup_failed","code":config.code,"class":"configuration","field":config.field,"source":config.source,"retryable":false})
    } else if matches!(
        cause.downcast_ref::<crate::artifacts::ArtifactError>(),
        Some(crate::artifacts::ArtifactError::Busy)
    ) {
        json!({"event":"startup_failed","code":"ARTIFACT_WRITER_BUSY","class":"storage","retryable":true})
    } else if matches!(
        cause.downcast_ref::<crate::store::StoreError>(),
        Some(crate::store::StoreError::Migration(_))
    ) {
        json!({"event":"startup_failed","code":"STARTUP_MIGRATION_FAILED","class":"migration","field":"database",
               "retryable":false,"recovery":"inspect_migration_history"})
    } else {
        let (class, retryable) = match cause
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind)
        {
            Some(std::io::ErrorKind::PermissionDenied) => ("permission", false),
            Some(std::io::ErrorKind::AddrInUse) => ("conflict", false),
            Some(std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput) => {
                ("configuration", false)
            }
            Some(std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionRefused) => {
                ("unavailable", true)
            }
            _ if matches!(
                cause.downcast_ref::<sqlx::Error>().or_else(|| {
                    match cause.downcast_ref::<crate::store::StoreError>() {
                        Some(crate::store::StoreError::Database(db)) => Some(db),
                        _ => None,
                    }
                }),
                Some(sqlx::Error::PoolTimedOut | sqlx::Error::Io(_))
            ) =>
            {
                ("unavailable", true)
            }
            _ => ("internal", false),
        };
        json!({"event":"startup_failed","code":"SERVICE_START_FAILED","class":class,"retryable":retryable})
    };
    eprintln!("{envelope}");
    std::process::ExitCode::FAILURE
}
