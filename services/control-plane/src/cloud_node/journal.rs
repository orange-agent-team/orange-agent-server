use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct Entry {
    pub host: String,
    pub run: String,
    pub workspace: String,
    pub command: String,
    pub expected: u64,
    pub allocation: Option<String>,
    pub epoch: Option<u64>,
    pub input_sha: Option<String>,
    pub phase: Phase,
    #[serde(default)]
    pub readiness_digest: Option<String>,
    pub cleanup_digest: Option<String>,
}
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    ClaimIntent,
    Claimed,
    Starting,
    Ready,
    Cleaned,
}

pub(super) struct Journal {
    root: std::path::PathBuf,
    _guard: File,
}
impl Journal {
    pub fn open(path: &std::path::Path) -> Result<Self, Error> {
        if !path.is_absolute() {
            return Err(Error::Local);
        }
        if !path.exists() {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|_| Error::Local)?;
        }
        let meta = fs::symlink_metadata(path).map_err(|_| Error::Local)?;
        if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::Local);
        }
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(path.join("host.lock"))
            .map_err(|_| Error::Local)?;
        guard.try_lock().map_err(|_| Error::Local)?;
        Ok(Self {
            root: path.canonicalize().map_err(|_| Error::Local)?,
            _guard: guard,
        })
    }
    fn path(&self, workspace: &str) -> Result<std::path::PathBuf, Error> {
        if uuid::Uuid::parse_str(workspace)
            .map_err(|_| Error::Local)?
            .to_string()
            != workspace
        {
            return Err(Error::Local);
        }
        Ok(self.root.join(format!("{workspace}.json")))
    }
    pub fn load(&self, workspace: &str) -> Result<Option<Entry>, Error> {
        let path = self.path(workspace)?;
        match fs::symlink_metadata(&path) {
            Ok(meta)
                if meta.is_file()
                    && meta.permissions().mode() & 0o077 == 0
                    && meta.len() <= 8192 => {}
            Ok(_) => return Err(Error::Local),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Local),
        }
        let entry: Entry = serde_json::from_slice(&fs::read(path).map_err(|_| Error::Local)?)
            .map_err(|_| Error::Local)?;
        if entry.workspace != workspace {
            return Err(Error::Local);
        }
        Ok(Some(entry))
    }
    pub fn entries(&self) -> Result<Vec<Entry>, Error> {
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.root).map_err(|_| Error::Local)? {
            let path = item.map_err(|_| Error::Local)?.path();
            if path.extension().and_then(|v| v.to_str()) == Some("json") {
                entries.push(
                    self.load(
                        path.file_stem()
                            .and_then(|v| v.to_str())
                            .ok_or(Error::Local)?,
                    )?
                    .ok_or(Error::Local)?,
                );
            }
        }
        Ok(entries)
    }
    pub fn save(&self, e: &Entry) -> Result<(), Error> {
        let path = self.path(&e.workspace)?;
        let tmp = self.root.join(format!(".pending-{}", uuid::Uuid::new_v4()));
        let mut out = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|_| Error::Local)?;
        let data = serde_json::to_vec(e).map_err(|_| Error::Local)?;
        out.write_all(&data)
            .and_then(|_| out.sync_all())
            .map_err(|_| Error::Local)?;
        fs::rename(tmp, path)
            .and_then(|_| File::open(&self.root)?.sync_all())
            .map_err(|_| Error::Local)
    }
}
