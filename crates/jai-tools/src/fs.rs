//! Filesystem tools: read, list, write, delete.

use crate::paths::PathGuard;
use crate::{FS_DELETE, FS_EDIT, FS_LIST, FS_READ, FS_WRITE};
use jai_core::error::{Error, Result};
use jai_core::text::truncate_bytes;
use jai_core::tool::ToolOutcome;
use serde::Deserialize;
use std::path::Path;

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

/// Arguments of [`FS_EDIT`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    pub path: String,
    /// Exact text to replace, copied from the file.
    pub old_string: String,
    /// Text that replaces it; must differ from `old_string`.
    pub new_string: String,
    /// How many non-overlapping occurrences must match (default: 1).
    #[serde(default)]
    pub expected_replacements: Option<usize>,
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

/// Edit a text file by replacing an exact snippet.
///
/// A full-file write costs the model one output token per byte it rewrites, so
/// changing three lines of a large file means regenerating the whole file --
/// slow, expensive, and easy to truncate. This tool sends only the snippet.
///
/// The replacement is *anchored*: `old_string` must occur exactly
/// `expected_replacements` times (default 1) in the file, or the edit fails with
/// an explanation. An ambiguous anchor is therefore a hard error instead of a
/// silent change in the wrong place.
pub async fn edit(guard: &PathGuard, args: &EditArgs) -> Result<ToolOutcome> {
    let expected = args.expected_replacements.unwrap_or(1);
    if expected == 0 {
        return Err(Error::tool(
            FS_EDIT,
            "expected_replacements must be at least 1",
        ));
    }
    if args.old_string.is_empty() {
        return Err(Error::tool(
            FS_EDIT,
            "old_string must not be empty (an empty snippet matches everywhere)",
        ));
    }
    if args.old_string == args.new_string {
        return Err(Error::tool(
            FS_EDIT,
            "old_string and new_string are identical: there is nothing to change",
        ));
    }

    // The file must already exist: creating one stays `filesystem_write`'s job.
    let path = guard.resolve_existing(&args.path)?;
    let metadata = std::fs::metadata(&path)
        .map_err(|err| Error::tool(FS_EDIT, format!("cannot stat: {err}")))?;
    if metadata.is_dir() {
        return Err(Error::tool(
            FS_EDIT,
            format!("`{}` is a directory (use {FS_WRITE})", guard.display(&path)),
        ));
    }
    let bytes =
        std::fs::read(&path).map_err(|err| Error::tool(FS_EDIT, format!("cannot read: {err}")))?;
    if bytes.iter().take(8 * 1024).any(|byte| *byte == 0) {
        return Err(Error::tool(
            FS_EDIT,
            format!("`{}` looks like a binary file", guard.display(&path)),
        ));
    }
    // Unlike `read`, an edit must not decode lossily: writing the replacement
    // back would corrupt every byte the lossy decode had mangled.
    let text = String::from_utf8(bytes).map_err(|_| {
        Error::tool(
            FS_EDIT,
            format!(
                "`{}` is not valid UTF-8 (binary or another encoding); use {FS_WRITE} to replace it",
                guard.display(&path)
            ),
        )
    })?;

    let found = text.matches(&args.old_string).count();
    if found != expected {
        return Err(Error::tool(
            FS_EDIT,
            mismatch_message(guard, &path, found, expected),
        ));
    }

    let updated = text.replacen(&args.old_string, &args.new_string, expected);
    write_atomic(&path, updated.as_bytes())
        .map_err(|err| Error::tool(FS_EDIT, format!("cannot write: {err}")))?;
    Ok(ToolOutcome::success(format!(
        "edited {}: replaced {found} occurrence{} ({} to {} bytes)",
        guard.display(&path),
        if found == 1 { "" } else { "s" },
        text.len(),
        updated.len()
    )))
}

/// Explain a failed anchor match in terms the model can act on.
fn mismatch_message(guard: &PathGuard, path: &Path, found: usize, expected: usize) -> String {
    let display = guard.display(path);
    if found == 0 {
        return format!(
            "`{display}` does not contain old_string (expected {expected} match{}); read the file \
             and copy the snippet exactly, including indentation",
            if expected == 1 { "" } else { "es" }
        );
    }
    format!(
        "old_string matches {found} time{} in `{display}` but expected_replacements is {expected}; \
         include more surrounding lines to make it unique, or set expected_replacements={found}",
        if found == 1 { "" } else { "s" }
    )
}

/// Replace the contents of `path` without ever leaving a half-written file
/// behind: write a sibling temporary file, carry the original permissions over,
/// then rename it onto the target (atomic within one filesystem).
fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let temp = parent.join(format!(".{name}.jai-{}.tmp", std::process::id()));
    let result = (|| {
        std::fs::write(&temp, contents)?;
        if let Ok(metadata) = std::fs::metadata(path) {
            // A rename would otherwise turn an executable script into a plain file.
            std::fs::set_permissions(&temp, metadata.permissions())?;
        }
        match std::fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            // Windows refuses to rename onto an existing path.
            Err(_) => {
                let fallback = std::fs::write(path, contents);
                let _ = std::fs::remove_file(&temp);
                fallback
            }
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
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
    async fn edit_replaces_one_snippet_and_keeps_the_rest() {
        let dir = workspace("edit");
        let file = dir.join("app.rs");
        let original = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        fs::write(&file, original).unwrap();
        let guard = guard(&dir);
        let outcome = edit(
            &guard,
            &EditArgs {
                path: "app.rs".into(),
                old_string: "let x = 1;".into(),
                new_string: "let x = 2;".into(),
                expected_replacements: None,
            },
        )
        .await
        .unwrap();
        assert!(
            outcome.content.contains("replaced 1 occurrence"),
            "{outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "fn main() {\n    let x = 2;\n    println!(\"{x}\");\n}\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn edit_requires_an_unambiguous_anchor() {
        let dir = workspace("edit-ambiguous");
        fs::write(dir.join("two.txt"), "x = 1\nx = 1\n").unwrap();
        let guard = guard(&dir);
        let ambiguous = edit(
            &guard,
            &EditArgs {
                path: "two.txt".into(),
                old_string: "x = 1".into(),
                new_string: "x = 2".into(),
                expected_replacements: None,
            },
        )
        .await
        .unwrap_err();
        assert!(
            ambiguous.to_string().contains("matches 2 times"),
            "{ambiguous}"
        );
        // The file is untouched after a refused edit.
        assert_eq!(
            fs::read_to_string(dir.join("two.txt")).unwrap(),
            "x = 1\nx = 1\n"
        );

        // The model can opt into replacing every occurrence explicitly.
        let both = edit(
            &guard,
            &EditArgs {
                path: "two.txt".into(),
                old_string: "x = 1".into(),
                new_string: "x = 2".into(),
                expected_replacements: Some(2),
            },
        )
        .await
        .unwrap();
        assert!(both.content.contains("replaced 2 occurrences"), "{both:?}");
        assert_eq!(
            fs::read_to_string(dir.join("two.txt")).unwrap(),
            "x = 2\nx = 2\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn edit_rejects_missing_anchors_no_ops_and_nonsense() {
        let dir = workspace("edit-errors");
        fs::write(dir.join("a.txt"), "hello world\n").unwrap();
        let guard = guard(&dir);
        let cases = [
            (
                EditArgs {
                    path: "a.txt".into(),
                    old_string: "goodbye".into(),
                    new_string: "hi".into(),
                    expected_replacements: None,
                },
                "does not contain old_string",
            ),
            (
                EditArgs {
                    path: "a.txt".into(),
                    old_string: "hello".into(),
                    new_string: "hello".into(),
                    expected_replacements: None,
                },
                "identical",
            ),
            (
                EditArgs {
                    path: "a.txt".into(),
                    old_string: String::new(),
                    new_string: "hi".into(),
                    expected_replacements: None,
                },
                "must not be empty",
            ),
            (
                EditArgs {
                    path: "a.txt".into(),
                    old_string: "hello".into(),
                    new_string: "hi".into(),
                    expected_replacements: Some(0),
                },
                "at least 1",
            ),
            (
                EditArgs {
                    path: "missing.txt".into(),
                    old_string: "hello".into(),
                    new_string: "hi".into(),
                    expected_replacements: None,
                },
                "cannot access",
            ),
        ];
        for (args, expected) in cases {
            let error = edit(&guard, &args).await.unwrap_err();
            assert!(error.to_string().contains(expected), "{expected}: {error}");
        }
        assert_eq!(
            fs::read_to_string(dir.join("a.txt")).unwrap(),
            "hello world\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn edit_refuses_binaries_directories_and_escapes() {
        let dir = workspace("edit-guard");
        fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join(".env"), "A=1").unwrap();
        let guard = guard(&dir);

        let binary = edit(
            &guard,
            &EditArgs {
                path: "bin.dat".into(),
                old_string: "a".into(),
                new_string: "b".into(),
                expected_replacements: None,
            },
        )
        .await
        .unwrap_err();
        assert!(binary.to_string().contains("binary"), "{binary}");

        // Valid UTF-8, but not a text file either (latin-1 bytes).
        fs::write(dir.join("latin1.dat"), [0xff, 0xfe, 0x41]).unwrap();
        let not_utf8 = edit(
            &guard,
            &EditArgs {
                path: "latin1.dat".into(),
                old_string: "A".into(),
                new_string: "B".into(),
                expected_replacements: None,
            },
        )
        .await
        .unwrap_err();
        assert!(
            not_utf8.to_string().contains("not valid UTF-8"),
            "{not_utf8}"
        );

        let directory = edit(
            &guard,
            &EditArgs {
                path: "sub".into(),
                old_string: "a".into(),
                new_string: "b".into(),
                expected_replacements: None,
            },
        )
        .await
        .unwrap_err();
        assert!(directory.to_string().contains("directory"), "{directory}");

        for path in ["../outside.txt", ".env"] {
            let error = edit(
                &guard,
                &EditArgs {
                    path: path.into(),
                    old_string: "a".into(),
                    new_string: "b".into(),
                    expected_replacements: None,
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.exit_code(), 6, "{path}: {error}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_preserves_permissions_and_leaves_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workspace("edit-mode");
        let script = dir.join("run.sh");
        fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        edit(
            &guard(&dir),
            &EditArgs {
                path: "run.sh".into(),
                old_string: "echo hi".into(),
                new_string: "echo hello".into(),
                expected_replacements: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(&script).unwrap(),
            "#!/bin/sh\necho hello\n"
        );
        let mode = fs::metadata(&script).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the executable bit must survive an edit");
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".jai-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
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
