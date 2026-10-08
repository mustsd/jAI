//! Path resolution and enforcement shared by the filesystem tools.
//!
//! Two layers, deliberately redundant:
//! 1. lexical resolution + deny rules (so write targets can be checked before
//!    they exist),
//! 2. canonicalization of paths that do exist (so symlinks cannot escape the
//!    workspace).

use crate::FS_WRITE;
use jai_core::error::{Error, Result};
use jai_policy::{Sandbox, check_path_access};
use std::path::{Path, PathBuf};

/// Guard used by every tool that touches the filesystem.
#[derive(Debug, Clone)]
pub struct PathGuard {
    sandbox: Sandbox,
    deny_paths: Vec<String>,
}

impl PathGuard {
    pub fn new(workspace: PathBuf, allowed: Vec<PathBuf>, deny_paths: Vec<String>) -> Self {
        let workspace = canonical_or_self(workspace);
        let allowed = allowed.into_iter().map(canonical_or_self).collect();
        Self {
            sandbox: Sandbox::new(workspace, allowed),
            deny_paths,
        }
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    pub fn display(&self, path: &Path) -> String {
        self.sandbox.display(path)
    }

    /// Resolve a model-provided path and apply the policy rules.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        let path = self.sandbox.resolve(raw);
        self.check(&path)?;
        Ok(path)
    }

    /// Resolve a path that must already exist, defeating symlink escapes.
    pub fn resolve_existing(&self, raw: &str) -> Result<PathBuf> {
        let path = self.resolve(raw)?;
        let canonical = path.canonicalize().map_err(|err| {
            Error::tool(
                "filesystem",
                format!("cannot access `{}`: {err}", self.display(&path)),
            )
        })?;
        self.check(&canonical)?;
        Ok(canonical)
    }

    /// Resolve a write target: the parent must exist and stay inside the sandbox.
    pub fn resolve_for_write(&self, raw: &str, create_dirs: bool) -> Result<PathBuf> {
        let path = self.resolve(raw)?;
        if create_dirs {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    Error::tool(
                        FS_WRITE,
                        format!("cannot create `{}`: {err}", self.display(parent)),
                    )
                })?;
                self.check(&canonical_or_self(parent.to_path_buf()))?;
            }
            return Ok(path);
        }
        let Some(parent) = path.parent() else {
            return Err(Error::tool(
                FS_WRITE,
                format!("`{}` has no parent directory", self.display(&path)),
            ));
        };
        let canonical_parent = parent.canonicalize().map_err(|err| {
            Error::tool(
                FS_WRITE,
                format!(
                    "parent directory `{}` does not exist ({err}); pass create_dirs=true to create it",
                    self.display(parent)
                ),
            )
        })?;
        self.check(&canonical_parent)?;
        let name = path.file_name().ok_or_else(|| {
            Error::tool(
                FS_WRITE,
                format!("`{}` is not a file name", self.display(&path)),
            )
        })?;
        Ok(canonical_parent.join(name))
    }

    /// Re-apply the policy rules to an already resolved path.
    pub fn check(&self, path: &Path) -> Result<()> {
        check_path_access(&self.sandbox, &self.deny_paths, path).map_err(Error::policy)
    }
}

fn canonical_or_self(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jai-paths-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn guard(dir: &Path) -> PathGuard {
        PathGuard::new(dir.to_path_buf(), Vec::new(), Vec::new())
    }

    #[test]
    fn files_inside_the_workspace_resolve() {
        let dir = workspace_dir("inside");
        fs::write(dir.join("a.txt"), "hi").unwrap();
        let guard = guard(&dir);
        let resolved = guard.resolve_existing("a.txt").unwrap();
        assert!(resolved.ends_with("a.txt"));
        assert_eq!(guard.display(&resolved), "./a.txt");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn escapes_are_rejected() {
        let dir = workspace_dir("escape");
        let guard = guard(&dir);
        let error = guard.resolve("../../etc/passwd").unwrap_err();
        assert_eq!(error.exit_code(), 6);
        assert!(
            error.to_string().contains("protected")
                || error.to_string().contains("outside the workspace"),
            "{error}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sensitive_names_are_rejected_even_inside_the_workspace() {
        let dir = workspace_dir("sensitive");
        fs::write(dir.join(".env"), "SECRET=1").unwrap();
        fs::write(dir.join("server.pem"), "key").unwrap();
        let guard = guard(&dir);
        assert!(guard.resolve(".env").is_err());
        assert!(guard.resolve("server.pem").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_targets_need_an_existing_parent_unless_asked() {
        let dir = workspace_dir("write");
        let guard = guard(&dir);
        let error = guard
            .resolve_for_write("missing/file.txt", false)
            .unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
        let with_dirs = guard.resolve_for_write("missing/file.txt", true).unwrap();
        assert!(with_dirs.ends_with("missing/file.txt"));
        assert!(dir.join("missing").is_dir());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_the_workspace() {
        let dir = workspace_dir("symlink");
        let outside = workspace_dir("symlink-target");
        fs::write(outside.join("secret.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();

        let guard = guard(&dir);
        let error = guard.resolve_existing("link/secret.txt").unwrap_err();
        assert_eq!(error.exit_code(), 6, "{error}");
        assert!(
            error.to_string().contains("outside the workspace"),
            "{error}"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }
}
