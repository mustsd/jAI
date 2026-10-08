//! Sensitive-path detection.
//!
//! The list is not a security boundary by itself — the workspace sandbox is —
//! but it stops the common accidents: `.env` files, SSH/GPG/AWS material, and
//! private key files, even when they sit inside the workspace.

use std::path::Path;

/// Directory names that are never touched, wherever they appear in a path.
pub const SENSITIVE_DIR_NAMES: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".password-store",
    ".1password",
    ".netrc.d",
    ".jai",
];

/// Home-relative directory paths that are never touched.
pub const SENSITIVE_HOME_DIRS: &[&str] =
    &[".config/gcloud", ".config/gh", ".docker", ".terraform.d"];

/// Absolute files that are never touched.
pub const SENSITIVE_ABSOLUTE_FILES: &[&str] = &[
    "/etc/shadow",
    "/etc/sudoers",
    "/etc/passwd",
    "/etc/ssh/ssh_host_rsa_key",
];

/// File names (with or without extension) that are never touched.
pub const SENSITIVE_FILE_NAMES: &[&str] = &[
    ".env",
    ".netrc",
    ".npmrc",
    ".pgpass",
    ".my.cnf",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "credentials",
    "secrets",
];

/// Extensions that are never touched.
pub const SENSITIVE_EXTENSIONS: &[&str] = &["pem", "key", "pfx", "p12", "jks", "keystore"];

/// Why `path` is sensitive, if it is.
pub fn sensitive_reason(path: &Path, home: Option<&Path>) -> Option<String> {
    let text = path.to_string_lossy().replace('\\', "/");
    let normalized = text.to_ascii_lowercase();

    for name in SENSITIVE_DIR_NAMES {
        if path.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(name)
        }) {
            return Some(format!("`{name}` holds credentials"));
        }
    }

    if let Some(home) = home {
        let home = home.to_string_lossy().replace('\\', "/");
        for relative in SENSITIVE_HOME_DIRS {
            let candidate = format!("{}/{relative}", home.trim_end_matches('/'));
            if normalized.starts_with(&candidate.to_ascii_lowercase()) {
                return Some(format!("`~/{relative}` holds credentials"));
            }
        }
    }

    if SENSITIVE_ABSOLUTE_FILES.contains(&normalized.as_str()) {
        return Some("system credential file".to_string());
    }

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if file_name.is_empty() {
        return None;
    }
    let stem = match file_name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => file_name.clone(),
    };
    if SENSITIVE_FILE_NAMES.contains(&file_name.as_str())
        || SENSITIVE_FILE_NAMES.contains(&stem.as_str())
    {
        return Some(format!("`{file_name}` holds credentials"));
    }
    if file_name.starts_with(".env.") || file_name == ".env" {
        return Some("environment files hold credentials".to_string());
    }
    if let Some((_, extension)) = file_name.rsplit_once('.') {
        if SENSITIVE_EXTENSIONS.contains(&extension) {
            return Some(format!("`*.{extension}` files hold key material"));
        }
    }
    None
}

/// Match a whole path or its file name against a user-supplied pattern.
pub fn matches_pattern(pattern: &str, path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    if glob_match(pattern, &text) {
        return true;
    }
    match path.file_name() {
        Some(name) => glob_match(pattern, &name.to_string_lossy()),
        None => false,
    }
}

/// Simple glob: `*` matches any run of characters, `?` matches one.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || chars_eq(pattern[p], text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn chars_eq(a: char, b: char) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn dotenv_files_are_sensitive_at_any_depth() {
        assert!(sensitive_reason(Path::new("/work/.env"), None).is_some());
        assert!(sensitive_reason(Path::new("/work/config/.env.production"), None).is_some());
        assert!(sensitive_reason(Path::new("/work/app.env"), None).is_none());
    }

    #[test]
    fn key_material_is_sensitive() {
        assert!(sensitive_reason(Path::new("/work/server.pem"), None).is_some());
        assert!(sensitive_reason(Path::new("/work/server.key"), None).is_some());
        assert!(sensitive_reason(Path::new("/work/keystore.jks"), None).is_some());
        assert!(sensitive_reason(Path::new("/work/notes.md"), None).is_none());
    }

    #[test]
    fn credential_stores_are_sensitive() {
        let home = PathBuf::from("/home/user");
        assert!(sensitive_reason(&home.join(".ssh/id_ed25519"), Some(&home)).is_some());
        assert!(sensitive_reason(&home.join(".aws/credentials"), Some(&home)).is_some());
        assert!(sensitive_reason(Path::new("/work/credentials.json"), None).is_some());
        assert!(sensitive_reason(Path::new("/work/secrets.yaml"), None).is_some());
        assert!(sensitive_reason(&home.join(".config/gcloud/x.json"), Some(&home)).is_some());
        assert!(sensitive_reason(&home.join("projects/app/src/main.rs"), Some(&home)).is_none());
    }

    #[test]
    fn system_files_are_sensitive() {
        assert!(sensitive_reason(Path::new("/etc/shadow"), None).is_some());
        assert!(sensitive_reason(Path::new("/etc/sudoers"), None).is_some());
        assert!(sensitive_reason(Path::new("/etc/hosts"), None).is_none());
    }

    #[test]
    fn jai_state_is_not_readable_by_tools() {
        let home = PathBuf::from("/home/user");
        assert!(sensitive_reason(&home.join(".jai/jai.db"), Some(&home)).is_some());
        assert!(sensitive_reason(&home.join(".jai/config.toml"), Some(&home)).is_some());
    }

    #[test]
    fn glob_matching_supports_star_and_question_mark() {
        assert!(glob_match("*.key", "server.key"));
        assert!(glob_match("secrets.*", "secrets.yaml"));
        assert!(glob_match("?.env", "a.env"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("*.key", "server.pem"));
        assert!(!glob_match("a*b*c", "ab"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(glob_match("**/*.pem", "/x/y/z.pem"));
    }

    #[test]
    fn user_patterns_match_paths_or_file_names() {
        let path = Path::new("/work/private/notes.txt");
        assert!(matches_pattern("*/private/*", path));
        assert!(matches_pattern("notes.txt", path));
        assert!(!matches_pattern("*.md", path));
    }

    #[test]
    fn unicode_paths_do_not_panic() {
        let path = Path::new("/work/项目/密钥.pem");
        assert!(sensitive_reason(path, None).is_some());
        assert!(glob_match("*.pem", "/work/项目/密钥.pem"));
        assert!(sensitive_reason(Path::new("/work/项目/main.rs"), None).is_none());
    }
}
