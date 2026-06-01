//! Integration tests for config loading and merging — `npxc::config`.
//!
//! Tests write real TOML files into `tempfile::tempdir()` instances and drive
//! them through the public config API, exercising the global ↔ package merge
//! logic end-to-end.

use npxc::config::{
    NetworkPolicy, ensure_version_pinned, load_package_config, package_config_path,
    pin_package_version, resolve_config,
};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Write `content` to `dir/npxc.toml` and return the path.
fn write_global_config(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
    let path = dir.join("npxc.toml");
    std::fs::write(&path, content).unwrap();
    path
}

/// Write `content` to `dir/packages/<filename>`.
fn write_package_config(dir: &std::path::Path, filename: &str, content: &str) {
    let pkg_dir = dir.join("packages");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(pkg_dir.join(filename), content).unwrap();
}

// ── global defaults, no package config ───────────────────────────────────────

#[test]
fn global_defaults_only() {
    let tmp = tempfile::tempdir().unwrap();
    // No npxc.toml and no package config → everything uses compiled defaults.
    let config_file = tmp.path().join("npxc.toml");

    let (eff, pkg_name, version) = resolve_config("simple-pkg", Some(&config_file)).unwrap();

    assert_eq!(pkg_name, "simple-pkg");
    assert_eq!(version, "latest");
    assert_eq!(eff.node_image, "node:lts-slim");
    assert_eq!(eff.memory, "512m");
    assert_eq!(eff.cpus, "1");
    assert_eq!(eff.network, NetworkPolicy::None);
    assert_eq!(eff.mount_mode, "ro");
    assert!(eff.path_arguments.is_empty());
    assert!(eff.non_path_arguments.is_empty());
    assert!(eff.version.is_none());
}

// ── package [runtime] overrides ───────────────────────────────────────────────

#[test]
fn package_runtime_memory_overrides_global() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("npxc.toml");

    write_package_config(
        tmp.path(),
        "pdf-tool.toml",
        r#"
package = "pdf-tool"
version = "1.0.0"

[runtime]
memory = "2g"
"#,
    );

    let (eff, _, _) = resolve_config("pdf-tool", Some(&config_file)).unwrap();

    assert_eq!(
        eff.memory, "2g",
        "[runtime] memory should override global default"
    );
    assert_eq!(eff.cpus, "1", "cpus should remain at global default");
    assert_eq!(
        eff.network,
        NetworkPolicy::None,
        "network should remain at global default"
    );
}

#[test]
fn package_runtime_partial_override_leaves_others_at_global() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("npxc.toml");

    write_package_config(
        tmp.path(),
        "mypkg.toml",
        r#"
package = "mypkg"

[runtime]
network = "bridge"
"#,
    );

    let (eff, _, _) = resolve_config("mypkg", Some(&config_file)).unwrap();

    assert_eq!(eff.network, NetworkPolicy::Named("bridge".to_string()));
    assert_eq!(
        eff.memory, "512m",
        "unset runtime fields fall back to global"
    );
    assert_eq!(eff.cpus, "1", "unset runtime fields fall back to global");
}

// ── path_arguments and non_path_arguments ────────────────────────────────────

#[test]
fn package_path_arguments_present_in_effective_config() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("npxc.toml");

    write_package_config(
        tmp.path(),
        "scope-tool.toml",
        r#"
package = "@scope/tool"
version = "0.1.0"

[path_arguments]
"*"        = ["path", "file"]
"read_pdf" = ["input"]
"#,
    );

    let (eff, _, _) = resolve_config("@scope/tool", Some(&config_file)).unwrap();

    assert_eq!(
        eff.path_arguments.get("*").unwrap(),
        &vec!["path".to_string(), "file".to_string()],
    );
    assert_eq!(
        eff.path_arguments.get("read_pdf").unwrap(),
        &vec!["input".to_string()],
    );
}

#[test]
fn package_non_path_arguments_present_in_effective_config() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("npxc.toml");

    write_package_config(
        tmp.path(),
        "filter-pkg.toml",
        r#"
package = "filter-pkg"

[non_path_arguments]
"*" = ["url", "query"]
"#,
    );

    let (eff, _, _) = resolve_config("filter-pkg", Some(&config_file)).unwrap();

    assert_eq!(
        eff.non_path_arguments.get("*").unwrap(),
        &vec!["url".to_string(), "query".to_string()],
    );
}

// ── pin_package_version + load_package_config round-trip ─────────────────────

#[test]
fn pin_and_load_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = "@scope/mypkg";

    pin_package_version(pkg, "2.3.4", tmp.path()).unwrap();

    let loaded = load_package_config(pkg, tmp.path())
        .unwrap()
        .expect("config file should exist after pinning");

    assert_eq!(loaded.version, Some("2.3.4".to_string()));
    assert_eq!(loaded.package, Some(pkg.to_string()));
}

#[test]
fn pin_preserves_existing_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = "preserve-pkg";

    // Write an initial config that contains runtime overrides.
    write_package_config(
        tmp.path(),
        "preserve-pkg.toml",
        r#"
package = "preserve-pkg"
version = "0.1.0"

[runtime]
memory = "1g"
"#,
    );

    // Pin a new version — must not lose the runtime field.
    pin_package_version(pkg, "0.2.0", tmp.path()).unwrap();

    let loaded = load_package_config(pkg, tmp.path()).unwrap().unwrap();
    assert_eq!(loaded.version, Some("0.2.0".to_string()));
    assert_eq!(
        loaded.runtime.as_ref().unwrap().memory,
        Some("1g".to_string()),
        "existing [runtime] fields must be preserved after a pin update"
    );
}

// ── ensure_version_pinned idempotency ─────────────────────────────────────────

#[test]
fn ensure_version_pinned_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = "idempotent-pkg";
    // ensure_version_pinned uses config_dir(Some(config_path)), which calls
    // config_path.parent().  Give it a plausible file path inside `tmp`.
    let config_file = tmp.path().join("npxc.toml");

    // First pin: creates the file.
    pin_package_version(pkg, "1.0.0", tmp.path()).unwrap();
    let path = package_config_path(pkg, tmp.path());
    let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();

    // Small sleep so mtime would differ if the file were rewritten.
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Same version → should be a no-op.
    ensure_version_pinned(pkg, "1.0.0", Some(&config_file)).unwrap();
    let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();

    assert_eq!(
        mtime1, mtime2,
        "file must not be rewritten when version already matches"
    );
}

#[test]
fn ensure_version_pinned_writes_when_version_differs() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = "update-pkg";
    let config_file = tmp.path().join("npxc.toml");

    pin_package_version(pkg, "1.0.0", tmp.path()).unwrap();

    // Different version → should update the file.
    ensure_version_pinned(pkg, "2.0.0", Some(&config_file)).unwrap();

    let loaded = load_package_config(pkg, tmp.path()).unwrap().unwrap();
    assert_eq!(loaded.version, Some("2.0.0".to_string()));
}

// ── resolve_config version resolution ────────────────────────────────────────

#[test]
fn resolve_config_version_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("npxc.toml");

    let (_, pkg_name, version) = resolve_config("@scope/pkg@1.2.3", Some(&config_file)).unwrap();

    assert_eq!(pkg_name, "@scope/pkg");
    assert_eq!(version, "1.2.3", "version embedded in spec should be used");
}

#[test]
fn resolve_config_no_version_defaults_to_latest() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("npxc.toml");

    let (_, pkg_name, version) = resolve_config("@scope/pkg", Some(&config_file)).unwrap();

    assert_eq!(pkg_name, "@scope/pkg");
    assert_eq!(version, "latest");
}

#[test]
fn resolve_config_uses_pinned_version_from_package_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("npxc.toml");

    // Pre-pin a version in the package config.
    pin_package_version("@scope/pkg", "0.4.2", tmp.path()).unwrap();

    let (_, pkg_name, version) = resolve_config("@scope/pkg", Some(&config_file)).unwrap();

    assert_eq!(pkg_name, "@scope/pkg");
    assert_eq!(
        version, "0.4.2",
        "pinned config version should take precedence over 'latest'"
    );
}

#[test]
fn resolve_config_spec_version_wins_over_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("npxc.toml");

    // Pin 0.4.2 in config, but pass @1.9.9 in spec — spec must win.
    pin_package_version("@scope/pkg", "0.4.2", tmp.path()).unwrap();

    let (_, _, version) = resolve_config("@scope/pkg@1.9.9", Some(&config_file)).unwrap();

    assert_eq!(
        version, "1.9.9",
        "CLI spec version must beat pinned config version"
    );
}

// ── custom global config values ───────────────────────────────────────────────

#[test]
fn global_config_custom_defaults_respected() {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = write_global_config(
        tmp.path(),
        r#"
[defaults]
memory = "1g"
cpus   = "2.0"
"#,
    );

    let (eff, _, _) = resolve_config("any-pkg", Some(&config_file)).unwrap();

    assert_eq!(eff.memory, "1g");
    assert_eq!(eff.cpus, "2.0");
    assert_eq!(
        eff.network,
        NetworkPolicy::None,
        "unset fields still use compiled defaults"
    );
}
