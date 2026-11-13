use anyhow::{anyhow, Result};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// Locations of the Claude Code folder and the ledger's own data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub claude_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    /// `$CLAUDE_CONFIG_DIR` else `~/.claude`; `$SLED_DATA_DIR` else `<claude folder>/session-ledger`.
    pub fn from_env() -> Result<Paths> {
        let claude_dir = match non_empty_env("CLAUDE_CONFIG_DIR") {
            Some(dir) => dir,
            None => {
                let home = non_empty_env("HOME")
                    .ok_or_else(|| anyhow!("HOME is not set and CLAUDE_CONFIG_DIR is not set"))?;
                home.join(".claude")
            }
        };
        let data_dir = non_empty_env("SLED_DATA_DIR")
            .unwrap_or_else(|| claude_dir.join("session-ledger"));
        Ok(Paths { claude_dir, data_dir })
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.claude_dir.join("projects")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.claude_dir.join("settings.json")
    }

    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("ledger.db")
    }

    pub fn config_file(&self) -> PathBuf {
        self.data_dir.join("config.toml")
    }

    pub fn hook_log(&self) -> PathBuf {
        self.data_dir.join("hook.log")
    }

    /// Removes group and other access from the data folder, the database files and the hook
    /// logs when they have it (unix). Never adds access, and skips symlinks, so nothing outside
    /// the data folder changes. Failures are ignored. Called by install, sweep and serve, not
    /// by the hook.
    pub fn tighten_permissions(&self) {
        let db = self.db_file();
        let side = |suffix: &str| {
            let mut p = db.clone().into_os_string();
            p.push(suffix);
            PathBuf::from(p)
        };
        strip_group_and_other(&self.data_dir);
        for file in [
            side("-wal"),
            side("-shm"),
            db,
            self.hook_log(),
            self.data_dir.join("hook.log.1"),
        ] {
            strip_group_and_other(&file);
        }
    }
}

/// Creates `dir` with mode 0700 on unix (missing parents get the usual mode). An existing
/// folder is left as it is.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        other => other,
    }
}

/// Open options whose newly created files get mode 0600 on unix.
pub fn private_file_options() -> OpenOptions {
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o
}

/// Sets a file's mode to 0600 on unix; failure is ignored.
pub fn make_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn strip_group_and_other(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::symlink_metadata(path) {
            let mode = m.permissions().mode();
            if !m.file_type().is_symlink() && mode & 0o077 != 0 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn non_empty_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
