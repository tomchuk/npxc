//! Integration tests for CWD-scope path validation — `npxc::paths::validate`.
//!
//! Each test uses `tempfile::tempdir()` to get a real on-disk directory so that
//! `std::fs::canonicalize` works correctly.

use std::fs;
use std::path::PathBuf;

use npxc::error::NpxcError;
use npxc::paths::validate::validate_path;

// ── helpers ───────────────────────────────────────────────────────────────────

fn write_file(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, b"").unwrap();
}

// ── basic in-scope path ───────────────────────────────────────────────────────

#[test]
fn valid_path_within_cwd_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("hello.txt");
    write_file(&file);

    let result = validate_path(file.to_str().unwrap(), tmp.path());
    assert!(result.is_ok(), "expected Ok, got {result:?}");
    assert_eq!(result.unwrap(), fs::canonicalize(&file).unwrap());
}

// ── path outside CWD ─────────────────────────────────────────────────────────

#[test]
fn path_outside_cwd_returns_out_of_scope() {
    let cwd = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let file = elsewhere.path().join("secret.txt");
    write_file(&file);

    let raw = file.to_str().unwrap();
    let err = validate_path(raw, cwd.path()).unwrap_err();
    assert!(
        matches!(err, NpxcError::PathOutOfScope { .. }),
        "expected PathOutOfScope, got {err:?}"
    );
}

// ── non-existent path ─────────────────────────────────────────────────────────

#[test]
fn nonexistent_path_returns_path_not_found() {
    let cwd = tempfile::tempdir().unwrap();
    let raw = cwd.path().join("no_such_file.txt");
    let raw_str = raw.to_str().unwrap();

    let err = validate_path(raw_str, cwd.path()).unwrap_err();
    assert!(
        matches!(err, NpxcError::PathNotFound(ref p) if p == raw_str),
        "expected PathNotFound({raw_str}), got {err:?}"
    );
}

// ── file:// prefix is stripped ────────────────────────────────────────────────

#[test]
fn file_uri_prefix_is_stripped() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("doc.md");
    write_file(&file);

    let uri = format!("file://{}", file.to_str().unwrap());
    let result = validate_path(&uri, tmp.path());
    assert!(result.is_ok(), "file:// URI should be accepted: {result:?}");
}

#[test]
fn file_uri_error_preserves_original_raw_path() {
    let tmp = tempfile::tempdir().unwrap();
    let raw = format!("file://{}", tmp.path().join("no_such.txt").display());

    let err = validate_path(&raw, tmp.path()).unwrap_err();
    // The error must carry the original URI, not the stripped path.
    assert!(
        matches!(err, NpxcError::PathNotFound(ref p) if p == &raw),
        "error should carry original URI, got {err:?}"
    );
}

// ── ~/ prefix is expanded to $HOME ────────────────────────────────────────────

#[test]
fn home_prefix_is_expanded() {
    let home_str = std::env::var("HOME").expect("HOME env var must be set");
    let home_path = fs::canonicalize(&home_str).expect("HOME must be canonicalisable");

    // Create a temp directory inside $HOME so the file is both expandable
    // via ~/ AND inside the CWD we declare.
    let tmp = tempfile::Builder::new()
        .tempdir_in(&home_path)
        .expect("tempdir inside HOME");

    let file = tmp.path().join("homefile.txt");
    write_file(&file);

    let canonical_file = fs::canonicalize(&file).unwrap();
    let rel_to_home = canonical_file
        .strip_prefix(&home_path)
        .expect("file should be under HOME");

    let tilde_path = format!("~/{}", rel_to_home.display());

    // CWD = canonical tmp dir so the file is in scope.
    let canonical_cwd = fs::canonicalize(tmp.path()).unwrap();
    let result = validate_path(&tilde_path, &canonical_cwd);
    assert!(result.is_ok(), "~/… path should be accepted: {result:?}");
}

// ── ../../ path that resolves inside CWD is accepted ─────────────────────────

#[test]
fn dotdot_resolving_inside_cwd_is_accepted() {
    let cwd = tempfile::tempdir().unwrap();

    // Create a subdirectory and a target file at cwd level.
    let sub = cwd.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let target = cwd.path().join("target.txt");
    write_file(&target);

    // cwd/sub/../target.txt  ──canonicalizes──▶  cwd/target.txt  (inside cwd)
    let path_str = format!("{}/sub/../target.txt", cwd.path().display());
    let result = validate_path(&path_str, cwd.path());
    assert!(
        result.is_ok(),
        "dotdot resolving inside CWD should be accepted: {result:?}"
    );
}

// ── ../../ path that resolves outside CWD is rejected ────────────────────────

#[test]
fn dotdot_resolving_outside_cwd_is_rejected() {
    let outer = tempfile::tempdir().unwrap();
    let inner: PathBuf = outer.path().join("inner");
    fs::create_dir(&inner).unwrap();

    // Create the escape target at the outer level (outside `inner`).
    let escape = outer.path().join("secret.txt");
    write_file(&escape);

    // inner/../secret.txt  ──canonicalizes──▶  outer/secret.txt  (outside inner)
    let path_str = format!("{}/inner/../secret.txt", outer.path().display());
    let err = validate_path(&path_str, &inner).unwrap_err();
    assert!(
        matches!(err, NpxcError::PathOutOfScope { .. }),
        "dotdot escaping CWD should be rejected: {err:?}"
    );
}

// ── symlink inside CWD pointing outside is rejected ──────────────────────────

#[test]
#[cfg(unix)]
fn symlink_inside_cwd_pointing_outside_is_rejected() {
    use std::os::unix::fs::symlink;

    let cwd = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();

    // Create a real file outside CWD.
    let real_file = outside.path().join("real.txt");
    write_file(&real_file);

    // Symlink at cwd/link.txt → outside/real.txt
    let link_path = cwd.path().join("link.txt");
    symlink(&real_file, &link_path).unwrap();

    let raw = link_path.to_str().unwrap();
    let err = validate_path(raw, cwd.path()).unwrap_err();
    assert!(
        matches!(err, NpxcError::PathOutOfScope { .. }),
        "symlink pointing outside CWD should be rejected: {err:?}"
    );
}
