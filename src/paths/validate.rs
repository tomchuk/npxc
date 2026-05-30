use std::path::{Path, PathBuf};

use crate::error::NpxcError;

/// Validate and canonicalize a raw path string for CWD-scope containment.
///
/// Steps:
/// 1. Strip `file://` prefix (if present), then expand a leading `~/` to `$HOME`.
/// 2. `std::fs::canonicalize` (realpath). Returns `PathNotFound` on error.
/// 3. Verify the canonical path `starts_with` `canonicalize(cwd)`.
///    Returns `PathOutOfScope` if not. Symlinks that point outside CWD are
///    caught here because step 2 has already resolved them.
///
/// Error messages always carry the *original* `raw_path` string, not the
/// processed form.
///
/// # Errors
///
/// Returns [`NpxcError::PathNotFound`] if the path cannot be canonicalized
/// (does not exist or is unreadable), [`NpxcError::PathOutOfScope`] if the
/// canonical path is not contained within `cwd`, or [`NpxcError::Io`] if `cwd`
/// itself cannot be canonicalized.
pub fn validate_path(raw_path: &str, cwd: &Path) -> Result<PathBuf, NpxcError> {
    // Step 1 — strip file:// prefix, then expand ~/
    let stripped = raw_path.strip_prefix("file://").unwrap_or(raw_path);

    let expanded: PathBuf = if let Some(rest) = stripped.strip_prefix("~/") {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        home.join(rest)
    } else {
        PathBuf::from(stripped)
    };

    // Step 2 — realpath; any error is reported as PathNotFound with the
    // *original* raw_path so callers / users see what they actually passed in.
    let canonical = std::fs::canonicalize(&expanded)
        .map_err(|_| NpxcError::PathNotFound(raw_path.to_owned()))?;

    // Step 3 — verify containment within the CWD scope.
    let canonical_cwd = std::fs::canonicalize(cwd)?;

    if !canonical.starts_with(&canonical_cwd) {
        return Err(NpxcError::PathOutOfScope {
            path: raw_path.to_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
        });
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn valid_path_within_cwd() {
        let tmp = setup();
        let file = tmp.path().join("hello.txt");
        fs::write(&file, b"hi").unwrap();

        let result = validate_path(file.to_str().unwrap(), tmp.path());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn nonexistent_path_returns_not_found() {
        let tmp = setup();
        let raw = "/this/definitely/does/not/exist/anywhere";
        let err = validate_path(raw, tmp.path()).unwrap_err();
        assert!(matches!(err, NpxcError::PathNotFound(p) if p == raw));
    }

    #[test]
    fn path_outside_cwd_returns_out_of_scope() {
        let tmp = setup();
        // Create a real path that is NOT inside tmp
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("secret.txt");
        fs::write(&file, b"data").unwrap();

        let raw = file.to_str().unwrap();
        let err = validate_path(raw, tmp.path()).unwrap_err();
        assert!(matches!(err, NpxcError::PathOutOfScope { path, .. } if path == raw));
    }

    #[test]
    fn file_uri_prefix_is_stripped() {
        let tmp = setup();
        let file = tmp.path().join("doc.md");
        fs::write(&file, b"").unwrap();

        let raw = format!("file://{}", file.to_str().unwrap());
        let result = validate_path(&raw, tmp.path());
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn error_preserves_original_raw_path_with_file_uri() {
        let tmp = setup();
        let raw = "file:///no/such/path/anywhere";
        let err = validate_path(raw, tmp.path()).unwrap_err();
        // The error must carry the original URI, not the stripped form.
        assert!(matches!(err, NpxcError::PathNotFound(p) if p == raw));
    }
}
