//! Workspace sandboxing: every path a tool touches must resolve inside the
//! workspace or one of the explicitly allowed roots.
//!
//! Resolution is lexical (it does not require the path to exist) so that write
//! targets can be checked before creation; the tools additionally canonicalize
//! existing paths, which is what actually defeats symlink escapes.

use std::path::{Component, Path, PathBuf};

/// Roots an agent is allowed to operate in.
#[derive(Debug, Clone)]
pub struct Sandbox {
    workspace: PathBuf,
    allowed: Vec<PathBuf>,
    home: Option<PathBuf>,
}

impl Sandbox {
    /// `workspace` and `allowed` should be absolute; `~` is expanded.
    pub fn new(workspace: impl Into<PathBuf>, allowed: Vec<PathBuf>) -> Self {
        let home = home_dir();
        let workspace = normalize(&expand(workspace.into(), home.as_deref()));
        let allowed = allowed
            .into_iter()
            .map(|path| normalize(&expand(path, home.as_deref())))
            .collect();
        Self {
            workspace,
            allowed,
            home,
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn allowed_paths(&self) -> &[PathBuf] {
        &self.allowed
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// Expand `~`, resolve relative paths against the workspace, and normalize.
    pub fn resolve(&self, raw: &str) -> PathBuf {
        let expanded = expand(PathBuf::from(raw.trim()), self.home.as_deref());
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            self.workspace.join(expanded)
        };
        normalize(&candidate)
    }

    /// Whether `path` (already resolved) is inside the workspace or an allowed root.
    ///
    /// `Path::starts_with` compares whole components, so `/work` never matches
    /// `/workspace-other`.
    pub fn is_allowed(&self, path: &Path) -> bool {
        let path = normalize(path);
        path.starts_with(&self.workspace) || self.allowed.iter().any(|root| path.starts_with(root))
    }

    /// Render a path for display: workspace-relative when possible.
    pub fn display(&self, path: &Path) -> String {
        match path.strip_prefix(&self.workspace) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
            Ok(relative) => format!("./{}", relative.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

/// Lexically normalize a path: drop `.`, resolve `..`, keep the root.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                // `..` cancels a real directory name, but never another `..`.
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(".."),
            },
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Expand a leading `~` using `home`.
pub fn expand(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path;
    };
    let text = path.to_string_lossy().to_string();
    if text == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return home.join(rest);
    }
    // `~user` is not expanded: it is ambiguous and rarely intended.
    path
}

fn home_dir() -> Option<PathBuf> {
    for variable in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(variable) {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> Sandbox {
        Sandbox::new("/work/project", vec![PathBuf::from("/work/shared")])
    }

    #[test]
    fn relative_paths_resolve_inside_the_workspace() {
        let sandbox = sandbox();
        assert_eq!(
            sandbox.resolve("src/main.rs"),
            PathBuf::from("/work/project/src/main.rs")
        );
        assert_eq!(
            sandbox.resolve("./src/../Cargo.toml"),
            PathBuf::from("/work/project/Cargo.toml")
        );
        assert!(sandbox.is_allowed(&sandbox.resolve("src/main.rs")));
    }

    #[test]
    fn relative_escapes_are_normalized_and_rejected() {
        let sandbox = sandbox();
        let escape = sandbox.resolve("../../etc/passwd");
        assert_eq!(escape, PathBuf::from("/etc/passwd"));
        assert!(!sandbox.is_allowed(&escape));
    }

    #[test]
    fn absolute_paths_outside_are_rejected() {
        let sandbox = sandbox();
        assert!(!sandbox.is_allowed(Path::new("/etc/passwd")));
        assert!(!sandbox.is_allowed(Path::new("/work/project-other/file")));
        assert!(sandbox.is_allowed(Path::new("/work/project/file")));
        assert!(sandbox.is_allowed(Path::new("/work/shared/file")));
    }

    #[test]
    fn the_workspace_root_itself_is_allowed() {
        let sandbox = sandbox();
        assert!(sandbox.is_allowed(Path::new("/work/project")));
        assert_eq!(sandbox.display(Path::new("/work/project")), ".");
    }

    #[test]
    fn prefixes_are_compared_by_components() {
        let sandbox = Sandbox::new("/a/b", Vec::new());
        assert!(sandbox.is_allowed(Path::new("/a/b/c")));
        assert!(!sandbox.is_allowed(Path::new("/a/bc")));
        assert!(!sandbox.is_allowed(Path::new("/a")));
    }

    #[test]
    fn display_prefers_workspace_relative_paths() {
        let sandbox = sandbox();
        assert_eq!(
            sandbox.display(Path::new("/work/project/src/lib.rs")),
            "./src/lib.rs"
        );
        assert_eq!(sandbox.display(Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn tilde_is_expanded_when_a_home_is_known() {
        let home = home_dir().expect("tests run with HOME or USERPROFILE set");
        let sandbox = Sandbox::new("/work", vec![PathBuf::from("~/extra")]);
        assert_eq!(sandbox.resolve("~"), normalize(&home));
        assert_eq!(
            sandbox.resolve("~/notes.md"),
            normalize(&home.join("notes.md"))
        );
        assert!(sandbox.is_allowed(&sandbox.resolve("~/extra/file")));
        // `~user` is not expanded.
        assert_eq!(sandbox.resolve("~other/x"), PathBuf::from("/work/~other/x"));
    }

    #[test]
    fn normalization_keeps_leading_parent_dirs() {
        assert_eq!(normalize(Path::new("../../x")), PathBuf::from("../../x"));
        assert_eq!(normalize(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize(Path::new("a/../..")), PathBuf::from(".."));
        assert_eq!(normalize(Path::new("/a/../b")), PathBuf::from("/b"));
    }
}
