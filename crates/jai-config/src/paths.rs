//! Filesystem layout for jAI's local state.
//!
//! Everything jAI stores lives under a single root directory (`~/.jai` by
//! default), which is treated as sensitive: on Unix the directory is created
//! `0700` and files `0600` (see `docs/security.md`).

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

const ROOT_DIR_NAME: &str = ".jai";

/// Root directory plus the well-known paths inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
    config_file: PathBuf,
}

impl Paths {
    /// Discover the state directory.
    ///
    /// Order: `JAI_HOME`, then `$HOME/.jai`, then `%USERPROFILE%\.jai`, then
    /// `./.jai` as a last resort. Keeping a plain `.jai` directory under the
    /// user profile works identically on macOS, Linux and Windows.
    pub fn discover() -> Self {
        let root = root_from_env(env::var_os("JAI_HOME"))
            .or_else(|| {
                env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|home| PathBuf::from(home).join(ROOT_DIR_NAME))
            })
            .or_else(|| {
                env::var_os("USERPROFILE")
                    .filter(|v| !v.is_empty())
                    .map(|user| PathBuf::from(user).join(ROOT_DIR_NAME))
            })
            .unwrap_or_else(|| PathBuf::from(ROOT_DIR_NAME));
        let config_file = match env::var_os("JAI_CONFIG").filter(|v| !v.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => root.join("config.toml"),
        };
        Self { root, config_file }
    }

    /// Build a layout rooted at an explicit directory (used by tests).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let config_file = root.join("config.toml");
        Self { root, config_file }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn database_file(&self) -> PathBuf {
        self.root.join("jai.db")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Create the directory tree with owner-only permissions.
    pub fn ensure(&self) -> io::Result<()> {
        for dir in [
            self.root.clone(),
            self.logs_dir(),
            self.sessions_dir(),
            self.cache_dir(),
        ] {
            ensure_private_dir(&dir)?;
        }
        Ok(())
    }
}

fn root_from_env(value: Option<OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Create `dir` (and parents) then restrict it to the owner on Unix.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict_to_owner(dir)
}

/// Best-effort owner-only permissions for a directory (`0700` on Unix).
pub fn restrict_to_owner(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o7777;
        if mode != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    {
        // Windows ACLs are not narrowed yet; the directory still lives under
        // the user profile, which is user-scoped by default. See docs/security.md.
        let _ = dir;
    }
    Ok(())
}

/// Best-effort owner-only permissions for a file (`0600` on Unix).
pub fn restrict_file_to_owner(file: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(file)?.permissions().mode() & 0o7777;
        if mode != 0o600 {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_root_exposes_expected_paths() {
        let paths = Paths::with_root("/tmp/jai-test");
        assert_eq!(paths.database_file(), PathBuf::from("/tmp/jai-test/jai.db"));
        assert_eq!(paths.config_file(), Path::new("/tmp/jai-test/config.toml"));
        assert!(paths.logs_dir().ends_with("logs"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        assert!(root_from_env(Some(OsString::new())).is_none());
        assert!(root_from_env(None).is_none());
        assert_eq!(
            root_from_env(Some(OsString::from("/x/.jai"))),
            Some(PathBuf::from("/x/.jai"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_creates_owner_only_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("jai-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::with_root(&dir);
        paths.ensure().unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "state directory must be owner-only");
        let log_mode = std::fs::metadata(paths.logs_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(log_mode, 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
