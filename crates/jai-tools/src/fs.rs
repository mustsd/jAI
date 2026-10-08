//! Filesystem tools: read, list, write, delete.

use crate::paths::PathGuard;
use crate::{FS_DELETE, FS_LIST, FS_READ, FS_WRITE};
use jai_core::error::{Error, Result};
use jai_core::text::truncate_bytes;
use jai_core::tool::ToolOutcome;
use serde::Deserialize;

/// Arguments of [`FS_READ`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub path: String,
    /// Upper bound on returned bytes; still capped by the configured limit.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

/// Arguments of [`FS_LIST`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    /// Directory to list; defaults to the workspace root.
    #[serde(default)]
    pub path: Option<String>,
}

/// Arguments of [`FS_WRITE`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
    /// Create missing parent directories (default: false).
    #[serde(default)]
    pub create_dirs: Option<bool>,
    /// Overwrite an existing file (default: true, matching most agents).
    #[serde(default)]
    pub overwrite: Option<bool>,
}

/// Arguments of [`FS_DELETE`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    pub path: String,
    /// Delete a directory and everything inside it (default: false).
    #[serde(default)]
    pub recursive: Option<bool>,
}

/// Read a text file inside the sandbox.
pub async fn read(
    guard: &PathGuard,
    args: &ReadArgs,
    max_output_bytes: usize,
) -> Result<ToolOutcome> {
    let path = guard.resolve_existing(&args.path)?;
    let metadata = std::fs::metadata(&path)
        .map_err(|err| Error::tool(FS_READ, format!("cannot stat: {err}")))?;
    if metadata.is_dir() {
        return Err(Error::tool(
            FS_READ,
            format!("`{}` is a directory (use {FS_LIST})", guard.display(&path)),
        ));
    }
    let bytes =
        std::fs::read(&path).map_err(|err| Error::tool(FS_READ, format!("cannot read: {err}")))?;
    if bytes.iter().take(8 * 1024).any(|byte| *byte == 0) {
        return Err(Error::tool(
            FS_READ,
            format!("`{}` looks like a binary file", guard.display(&path)),
        ));
    }
    let cap = match args.max_bytes {
        Some(requested) => requested.clamp(1, max_output_bytes),
        None => max_output_bytes,
    };
    let text = String::from_utf8_lossy(&bytes);
    let (content, truncated) = truncate_bytes(&text, cap);
    Ok(ToolOutcome {
        ok: true,
        content,
        truncated,
    })
}

/// List a directory inside the sandbox.
pub async fn list(guard: &PathGuard, args: &ListArgs, max_entries: usize) -> Result<ToolOutcome> {
    let raw = args.path.as_deref().unwrap_or(".");
    let path = guard.resolve_existing(raw)?;
    if !path.is_dir() {
        return Err(Error::tool(
            FS_LIST,
            format!("`{}` is not a directory", guard.display(&path)),
        ));
    }
    let mut entries: Vec<String> = Vec::new();
    let mut truncated = false;
    let read_dir = std::fs::read_dir(&path)
        .map_err(|err| Error::tool(FS_LIST, format!("cannot list: {err}")))?;
    let mut names: Vec<(String, bool)> = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|err| Error::tool(FS_LIST, format!("{err}")))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        names.push((name, is_dir));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, is_dir) in names {
        if entries.len() >= max_entries {
            truncated = true;
            break;
        }
        if is_dir {
            entries.push(format!("{name}/"));
        } else {
            entries.push(name);
        }
    }
    let mut content = if entries.is_empty() {
        format!("{} is empty", guard.display(&path))
    } else {
        entries.join("\n")
    };
    if truncated {
        content.push_str(&format!("\n…[truncated at {max_entries} entries]"));
    }
    Ok(ToolOutcome {
        ok: true,
        content,
        truncated,
    })
}

/// Write a text file inside the sandbox.
pub async fn write(guard: &PathGuard, args: &WriteArgs) -> Result<ToolOutcome> {
    let create_dirs = args.create_dirs.unwrap_or(false);
    let overwrite = args.overwrite.unwrap_or(true);
    let path = guard.resolve_for_write(&args.path, create_dirs)?;
    if path.exists() && !overwrite {
        return Err(Error::tool(
            FS_WRITE,
            format!(
                "`{}` already exists and overwrite=false",
                guard.display(&path)
            ),
        ));
    }
    if let Ok(metadata) = path.metadata() {
        if metadata.is_dir() {
            return Err(Error::tool(
                FS_WRITE,
                format!("`{}` is a directory", guard.display(&path)),
            ));
        }
    }
    std::fs::write(&path, args.content.as_bytes())
        .map_err(|err| Error::tool(FS_WRITE, format!("cannot write: {err}")))?;
    Ok(ToolOutcome::success(format!(
        "wrote {} bytes to {}",
        args.content.len(),
        guard.display(&path)
    )))
}

/// Delete a file, or a directory when `recursive` is set.
pub async fn delete(guard: &PathGuard, args: &DeleteArgs) -> Result<ToolOutcome> {
    let recursive = args.recursive.unwrap_or(false);
    let path = guard.resolve(&args.path)?;
    // Deleting through a symlink must not delete the link target outside the
    // workspace, so resolve the link and re-check before removing anything.
    let target = path.canonicalize().unwrap_or_else(|_| path.clone());
    guard.check(&target)?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|err| {
        Error::tool(
            FS_DELETE,
            format!("cannot access `{}`: {err}", guard.display(&path)),
        )
    })?;
    if metadata.is_dir() {
        if recursive {
            std::fs::remove_dir_all(&path)
                .map_err(|err| Error::tool(FS_DELETE, format!("cannot remove directory: {err}")))?;
        } else {
            std::fs::remove_dir(&path).map_err(|err| {
                Error::tool(
                    FS_DELETE,
                    format!("cannot remove directory (pass recursive=true): {err}"),
                )
            })?;
        }
        return Ok(ToolOutcome::success(format!(
            "removed directory {}",
            guard.display(&path)
        )));
    }
    std::fs::remove_file(&path)
        .map_err(|err| Error::tool(FS_DELETE, format!("cannot remove file: {err}")))?;
    Ok(ToolOutcome::success(format!(
        "removed file {}",
        guard.display(&path)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jai-fs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn guard(dir: &Path) -> PathGuard {
        PathGuard::new(dir.to_path_buf(), Vec::new(), Vec::new())
    }

    #[tokio::test]
    async fn read_returns_file_contents() {
        let dir = workspace("read");
        fs::write(dir.join("hello.txt"), "你好, world\n").unwrap();
        let outcome = read(
            &guard(&dir),
            &ReadArgs {
                path: "hello.txt".into(),
                max_bytes: None,
            },
            4096,
        )
        .await
        .unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.content, "你好, world\n");
        assert!(!outcome.truncated);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn read_truncates_and_flags_it() {
        let dir = workspace("read-truncate");
        fs::write(dir.join("big.txt"), "x".repeat(1000)).unwrap();
        let outcome = read(
            &guard(&dir),
            &ReadArgs {
                path: "big.txt".into(),
                max_bytes: Some(10),
            },
            4096,
        )
        .await
        .unwrap();
        assert!(outcome.truncated);
        assert!(outcome.content.starts_with(&"x".repeat(10)));
        assert!(outcome.content.ends_with("[truncated]"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn read_rejects_directories_binaries_and_escapes() {
        let dir = workspace("read-errors");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        let guard = guard(&dir);

        let dir_error = read(
            &guard,
            &ReadArgs {
                path: "sub".into(),
                max_bytes: None,
            },
            4096,
        )
        .await
        .unwrap_err();
        assert!(dir_error.to_string().contains("directory"), "{dir_error}");

        let binary_error = read(
            &guard,
            &ReadArgs {
                path: "bin.dat".into(),
                max_bytes: None,
            },
            4096,
        )
        .await
        .unwrap_err();
        assert!(
            binary_error.to_string().contains("binary"),
            "{binary_error}"
        );

        let escape = read(
            &guard,
            &ReadArgs {
                path: "../../etc/hosts".into(),
                max_bytes: None,
            },
            4096,
        )
        .await
        .unwrap_err();
        assert_eq!(escape.exit_code(), 6);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn list_sorts_entries_and_marks_directories() {
        let dir = workspace("list");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("b.txt"), "b").unwrap();
        fs::write(dir.join("a.txt"), "a").unwrap();
        let outcome = list(&guard(&dir), &ListArgs { path: None }, 100)
            .await
            .unwrap();
        assert_eq!(outcome.content, "a.txt\nb.txt\nsrc/");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn list_reports_truncation_and_empty_directories() {
        let dir = workspace("list-limits");
        fs::create_dir_all(dir.join("empty")).unwrap();
        for index in 0..5 {
            fs::write(dir.join(format!("f{index}")), "x").unwrap();
        }
        let guard = guard(&dir);
        let truncated = list(&guard, &ListArgs { path: None }, 2).await.unwrap();
        assert!(truncated.truncated);
        assert!(truncated.content.contains("truncated at 2 entries"));
        let empty = list(
            &guard,
            &ListArgs {
                path: Some("empty".into()),
            },
            10,
        )
        .await
        .unwrap();
        assert!(empty.content.contains("is empty"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn write_creates_files_with_optional_directories() {
        let dir = workspace("write");
        let guard = guard(&dir);
        let outcome = write(
            &guard,
            &WriteArgs {
                path: "notes/todo.md".into(),
                content: "- ship jAI".into(),
                create_dirs: Some(true),
                overwrite: None,
            },
        )
        .await
        .unwrap();
        assert!(outcome.content.contains("wrote"));
        assert_eq!(
            fs::read_to_string(dir.join("notes/todo.md")).unwrap(),
            "- ship jAI"
        );

        let without_dirs = write(
            &guard,
            &WriteArgs {
                path: "other/x.txt".into(),
                content: "x".into(),
                create_dirs: None,
                overwrite: None,
            },
        )
        .await
        .unwrap_err();
        assert!(
            without_dirs.to_string().contains("does not exist"),
            "{without_dirs}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn write_honours_overwrite_and_rejects_escapes() {
        let dir = workspace("write-guard");
        fs::write(dir.join("existing.txt"), "old").unwrap();
        let guard = guard(&dir);
        let refused = write(
            &guard,
            &WriteArgs {
                path: "existing.txt".into(),
                content: "new".into(),
                create_dirs: None,
                overwrite: Some(false),
            },
        )
        .await
        .unwrap_err();
        assert!(refused.to_string().contains("overwrite=false"), "{refused}");

        let escape = write(
            &guard,
            &WriteArgs {
                path: "../outside.txt".into(),
                content: "x".into(),
                create_dirs: None,
                overwrite: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(escape.exit_code(), 6);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn delete_removes_files_and_directories() {
        let dir = workspace("delete");
        fs::write(dir.join("a.txt"), "a").unwrap();
        fs::create_dir_all(dir.join("nested/deep")).unwrap();
        fs::write(dir.join("nested/deep/x.txt"), "x").unwrap();
        let guard = guard(&dir);

        let file = delete(
            &guard,
            &DeleteArgs {
                path: "a.txt".into(),
                recursive: None,
            },
        )
        .await
        .unwrap();
        assert!(file.content.contains("removed file"));
        assert!(!dir.join("a.txt").exists());

        let non_empty = delete(
            &guard,
            &DeleteArgs {
                path: "nested".into(),
                recursive: Some(false),
            },
        )
        .await
        .unwrap_err();
        assert!(
            non_empty.to_string().contains("recursive=true"),
            "{non_empty}"
        );

        let recursive = delete(
            &guard,
            &DeleteArgs {
                path: "nested".into(),
                recursive: Some(true),
            },
        )
        .await
        .unwrap();
        assert!(recursive.content.contains("removed directory"));
        assert!(!dir.join("nested").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn delete_refuses_sensitive_targets() {
        let dir = workspace("delete-sensitive");
        fs::write(dir.join(".env"), "A=1").unwrap();
        let error = delete(
            &guard(&dir),
            &DeleteArgs {
                path: ".env".into(),
                recursive: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.exit_code(), 6);
        assert!(error.to_string().contains("protected"), "{error}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
