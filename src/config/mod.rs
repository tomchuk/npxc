pub mod global;
pub mod merge;
pub mod package;

pub use global::NpxcConfig;
pub use merge::EffectiveConfig;
pub use package::PackageConfig;

use std::path::{Path, PathBuf};

use crate::error::NpxcError;

// ── Name helpers ─────────────────────────────────────────────────────────────

/// Sanitize an npm package name into a filesystem-safe identifier.
///
/// Rules applied in order:
/// 1. Lowercase the entire string.
/// 2. Replace every `@` and `/` character with `-`.
/// 3. Strip any leading `-` characters that result from step 2.
///
/// # Examples
/// ```
/// # use npxc::config::sanitize_package_name;
/// assert_eq!(sanitize_package_name("@sylphx/pdf-reader-mcp"), "sylphx-pdf-reader-mcp");
/// assert_eq!(sanitize_package_name("express"),               "express");
/// assert_eq!(sanitize_package_name("@scope/name"),           "scope-name");
/// ```
#[must_use]
pub fn sanitize_package_name(pkg: &str) -> String {
    pkg.to_lowercase()
        .replace(['@', '/'], "-")
        .trim_start_matches('-')
        .to_string()
}

/// Split a package spec of the form `[@scope/]name[@version]` into a
/// `(name, version)` pair.
///
/// The split is performed on the **last** `@` that appears after the first
/// character, so the leading `@` in scoped packages is preserved. The search
/// is character-aware, so empty or non-ASCII input never panics.
///
/// # Examples
/// ```
/// # use npxc::config::parse_package_spec;
/// assert_eq!(parse_package_spec("@scope/name@1.2.3"), ("@scope/name".into(), Some("1.2.3".into())));
/// assert_eq!(parse_package_spec("@scope/name"),       ("@scope/name".into(), None));
/// assert_eq!(parse_package_spec("express@4.18.0"),    ("express".into(),     Some("4.18.0".into())));
/// assert_eq!(parse_package_spec("express"),           ("express".into(),     None));
/// ```
#[must_use]
pub fn parse_package_spec(spec: &str) -> (String, Option<String>) {
    // Byte offset of the last '@' that is not the leading scope marker (index
    // 0). `rfind` returns an offset on a valid UTF-8 boundary, so empty and
    // non-ASCII input are handled without panicking.
    let at = spec.rfind('@').filter(|&i| i > 0);

    match at {
        Some(pos) => {
            // `pos` indexes an ASCII '@' (one byte), so both slices are valid.
            let ver = &spec[pos + 1..];
            let version = if ver.is_empty() {
                None
            } else {
                Some(ver.to_string())
            };
            (spec[..pos].to_string(), version)
        }
        None => (spec.to_string(), None),
    }
}

/// Validate an npm package **name** (the portion of a spec before any version).
///
/// Accepts unscoped names (`express`) and scoped names (`@scope/pkg`). Each
/// segment must be non-empty, must not begin with `.` or `_`, and may contain
/// only ASCII letters, digits, `-`, `.`, and `_`. The total length must not
/// exceed npm's 214-character limit.
///
/// This is a security boundary as well as a correctness check: the name flows
/// into the image tag and into `npm install "<spec>"` inside the Dockerfile, so
/// rejecting shell metacharacters here prevents build-time command injection.
///
/// # Errors
///
/// Returns [`NpxcError::Config`] describing the first rule the name violates.
pub fn validate_package_name(name: &str) -> Result<(), NpxcError> {
    let reject = |reason: &str| {
        Err(NpxcError::Config(format!(
            "invalid package name {name:?}: {reason}"
        )))
    };

    if name.is_empty() {
        return reject("name is empty");
    }
    if name.len() > 214 {
        return reject("name exceeds the 214-character limit");
    }

    // Split a scoped name into its scope and package segments.
    let segments: Vec<&str> = if let Some(rest) = name.strip_prefix('@') {
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() != 2 {
            return reject("scoped names must be of the form @scope/name");
        }
        parts
    } else if name.contains('/') {
        return reject("unscoped names must not contain '/'");
    } else {
        vec![name]
    };

    for seg in segments {
        if seg.is_empty() {
            return reject("name segment is empty");
        }
        if seg.starts_with('.') || seg.starts_with('_') {
            return reject("name segment must not start with '.' or '_'");
        }
        if !seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        {
            return reject("name may contain only ASCII letters, digits, '-', '.', and '_'");
        }
    }

    Ok(())
}

/// Validate a resolved package **version** string.
///
/// Accepts either a concrete [semver](https://semver.org) version (e.g.
/// `1.2.3`, `0.4.2-rc.1`) or a dist-tag composed solely of ASCII letters,
/// digits, `-`, `.`, and `_` and beginning with an alphanumeric (e.g.
/// `latest`, `next`). Range operators (`^`, `~`, `>`, `<`, `=`, spaces) are
/// rejected because they are invalid in an OCI image tag and/or are shell
/// metacharacters; this keeps such characters out of the Dockerfile
/// `npm install` step, as [`validate_package_name`] does for names.
///
/// # Errors
///
/// Returns [`NpxcError::Config`] if `version` is neither a semver version nor a
/// permitted dist-tag.
pub fn validate_version(version: &str) -> Result<(), NpxcError> {
    if semver::Version::parse(version).is_ok() {
        return Ok(());
    }

    let is_tag = !version.is_empty()
        && version.starts_with(|c: char| c.is_ascii_alphanumeric())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'));

    if is_tag {
        Ok(())
    } else {
        Err(NpxcError::Config(format!(
            "invalid version {version:?}: expected a semver version (e.g. 1.2.3) or a \
             dist-tag (e.g. latest); version ranges are not supported"
        )))
    }
}

// ── Directory / path helpers ─────────────────────────────────────────────────

/// Return the npxc configuration directory.
///
/// Resolution order:
/// 1. If `config_override` is `Some(path)` — return `path`'s **parent**
///    directory (the file lives inside the config dir).
/// 2. XDG / platform config dir via
///    `directories::ProjectDirs::from("", "", "npxc")`.
/// 3. Hard fallback: `~/.config/npxc`.
///
/// # Errors
///
/// Returns [`NpxcError::Config`] if `config_override` is a path without a
/// parent directory.
pub fn config_dir(config_override: Option<&PathBuf>) -> Result<PathBuf, NpxcError> {
    if let Some(p) = config_override {
        let parent = p.parent().ok_or_else(|| {
            NpxcError::Config(format!(
                "Config path has no parent directory: {}",
                p.display()
            ))
        })?;
        return Ok(parent.to_path_buf());
    }

    if let Some(proj) = directories::ProjectDirs::from("", "", "npxc") {
        return Ok(proj.config_dir().to_path_buf());
    }

    // Hard fallback: ~/.config/npxc
    let home = directories::BaseDirs::new()
        .map_or_else(|| PathBuf::from("~"), |bd| bd.home_dir().to_path_buf());
    Ok(home.join(".config").join("npxc"))
}

/// Compute the path to the package-specific config file.
///
/// Result: `<config_dir>/packages/<sanitized_name>.toml`
#[must_use]
pub fn package_config_path(pkg_name: &str, config_dir: &Path) -> PathBuf {
    config_dir
        .join("packages")
        .join(format!("{}.toml", sanitize_package_name(pkg_name)))
}

// ── Loaders ──────────────────────────────────────────────────────────────────

/// Load the global `npxc.toml` config.
///
/// * If `config_path` is `Some`, that path is used directly.
/// * Otherwise the path defaults to `<config_dir>/npxc.toml`.
/// * If the resolved file does not exist, [`NpxcConfig::default`] is returned
///   (no error).
///
/// # Errors
///
/// Returns [`NpxcError::Io`] if the file exists but cannot be read, or
/// [`NpxcError::TomlDe`] if its contents are not valid TOML.
pub fn load_global_config(config_path: Option<&PathBuf>) -> Result<NpxcConfig, NpxcError> {
    let path = match config_path {
        Some(p) => p.clone(),
        None => config_dir(None)?.join("npxc.toml"),
    };

    if !path.exists() {
        return Ok(NpxcConfig::default());
    }

    let content = std::fs::read_to_string(&path)?;
    let config: NpxcConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Load the per-package config for `pkg_name`.
///
/// Returns `Ok(None)` if the file does not exist.
///
/// # Errors
///
/// Returns [`NpxcError::Io`] if the file exists but cannot be read, or
/// [`NpxcError::TomlDe`] if its contents are not valid TOML.
pub fn load_package_config(
    pkg_name: &str,
    config_dir: &Path,
) -> Result<Option<PackageConfig>, NpxcError> {
    let path = package_config_path(pkg_name, config_dir);

    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)?;
    let config: PackageConfig = toml::from_str(&content)?;
    Ok(Some(config))
}

// ── Writers ───────────────────────────────────────────────────────────────────

/// Write (or update) the per-package config file, setting `version`.
///
/// Any existing fields in the file are preserved; only `version` is
/// overwritten.  The packages directory is created if it does not exist.
///
/// # Errors
///
/// Returns [`NpxcError::Io`] if the existing file cannot be read or the new
/// contents cannot be written, [`NpxcError::TomlDe`] if the existing file is
/// malformed, or [`NpxcError::TomlSer`] if serialization fails.
pub fn pin_package_version(
    pkg_name: &str,
    version: &str,
    config_dir: &Path,
) -> Result<(), NpxcError> {
    let path = package_config_path(pkg_name, config_dir);

    // Load existing config or start with a fresh one that records the name.
    let mut config = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        toml::from_str::<PackageConfig>(&content)?
    } else {
        PackageConfig {
            package: Some(pkg_name.to_string()),
            ..Default::default()
        }
    };

    config.version = Some(version.to_string());

    // Ensure the packages/ directory exists before writing.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let serialized = toml::to_string_pretty(&config)?;
    std::fs::write(&path, serialized)?;
    Ok(())
}

// ── High-level resolution ─────────────────────────────────────────────────────

/// Fully resolve the configuration for a package spec.
///
/// Steps:
/// 1. Parse `pkg_spec` into `(package_name, version_from_spec)`.
/// 2. Load the global config (honouring `config_path`).
/// 3. Load the per-package config (if it exists).
/// 4. Merge into an [`EffectiveConfig`].
/// 5. Resolve the concrete version string using the priority chain:
///    CLI spec > pinned config > `"latest"`.
/// 6. Validate the resolved name and version.
///
/// Returns `(EffectiveConfig, package_name_without_version, resolved_version)`.
///
/// # Errors
///
/// Returns [`NpxcError::Config`] if the package name or resolved version is
/// invalid (see [`validate_package_name`] / [`validate_version`]), and
/// propagates any error from [`config_dir`], [`load_global_config`], or
/// [`load_package_config`].
pub fn resolve_config(
    pkg_spec: &str,
    config_path: Option<&PathBuf>,
) -> Result<(EffectiveConfig, String, String), NpxcError> {
    let (pkg_name, version_from_spec) = parse_package_spec(pkg_spec);
    validate_package_name(&pkg_name)?;

    let cdir = config_dir(config_path)?;
    let global = load_global_config(config_path)?;
    let pkg_config = load_package_config(&pkg_name, &cdir)?;

    let effective = merge::merge(&global, pkg_config.as_ref());
    validate_runtime(&effective)?;

    // Version priority: CLI spec > pinned config > "latest"
    let resolved_version = version_from_spec
        .or_else(|| effective.version.clone())
        .unwrap_or_else(|| "latest".to_string());
    validate_version(&resolved_version)?;

    Ok((effective, pkg_name, resolved_version))
}

/// Validate runtime fields of a merged [`EffectiveConfig`].
///
/// `mount_mode` is checked strictly because it is interpolated into the
/// container `-v <dir>:/workspace:<mode>` flag and only `ro`/`rw` are
/// meaningful. A non-standard `network` is allowed (it may be a user-defined
/// network) but logged at `warn`, since a typo such as `noen` would silently
/// remove the intended `none` isolation.
fn validate_runtime(effective: &EffectiveConfig) -> Result<(), NpxcError> {
    match effective.mount_mode.as_str() {
        "ro" | "rw" => {}
        other => {
            return Err(NpxcError::Config(format!(
                "invalid mount_mode {other:?}: expected \"ro\" or \"rw\""
            )));
        }
    }

    if !matches!(effective.network.as_str(), "none" | "bridge") {
        tracing::warn!(
            network = %effective.network,
            "non-standard network value; container isolation may be weakened \
             (expected \"none\" or \"bridge\")"
        );
    }

    Ok(())
}

/// Persist a version pin for `pkg_name` unless it is already set to `version`.
///
/// This is a no-op when the existing config already records the same version,
/// avoiding unnecessary file writes.
///
/// # Errors
///
/// Propagates any error from [`config_dir`], [`load_package_config`], or
/// [`pin_package_version`].
pub fn ensure_version_pinned(
    pkg_name: &str,
    version: &str,
    config_path: Option<&PathBuf>,
) -> Result<(), NpxcError> {
    let cdir = config_dir(config_path)?;
    let existing = load_package_config(pkg_name, &cdir)?;

    // Skip if the pinned version already matches.
    if let Some(cfg) = &existing {
        if cfg.version.as_deref() == Some(version) {
            return Ok(());
        }
    }

    pin_package_version(pkg_name, version, &cdir)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── sanitize_package_name ─────────────────────────────────────────────────

    #[test]
    fn sanitize_scoped_package() {
        assert_eq!(
            sanitize_package_name("@sylphx/pdf-reader-mcp"),
            "sylphx-pdf-reader-mcp"
        );
    }

    #[test]
    fn sanitize_unscoped_package() {
        assert_eq!(sanitize_package_name("express"), "express");
    }

    #[test]
    fn sanitize_scoped_no_hyphen() {
        assert_eq!(sanitize_package_name("@scope/name"), "scope-name");
    }

    #[test]
    fn sanitize_preserves_existing_hyphens() {
        assert_eq!(sanitize_package_name("@org/my-package"), "org-my-package");
    }

    // ── parse_package_spec ────────────────────────────────────────────────────

    #[test]
    fn parse_scoped_with_version() {
        let (name, ver) = parse_package_spec("@scope/name@1.2.3");
        assert_eq!(name, "@scope/name");
        assert_eq!(ver, Some("1.2.3".to_string()));
    }

    #[test]
    fn parse_scoped_without_version() {
        let (name, ver) = parse_package_spec("@scope/name");
        assert_eq!(name, "@scope/name");
        assert_eq!(ver, None);
    }

    #[test]
    fn parse_unscoped_with_version() {
        let (name, ver) = parse_package_spec("express@4.18.0");
        assert_eq!(name, "express");
        assert_eq!(ver, Some("4.18.0".to_string()));
    }

    #[test]
    fn parse_unscoped_without_version() {
        let (name, ver) = parse_package_spec("express");
        assert_eq!(name, "express");
        assert_eq!(ver, None);
    }

    #[test]
    fn parse_empty_spec_does_not_panic() {
        assert_eq!(parse_package_spec(""), (String::new(), None));
    }

    #[test]
    fn parse_non_ascii_leading_char_does_not_panic() {
        // Regression: byte-slicing `spec[1..]` used to panic on a multi-byte
        // leading character.
        let (name, ver) = parse_package_spec("é-pkg");
        assert_eq!(name, "é-pkg");
        assert_eq!(ver, None);
    }

    #[test]
    fn parse_scope_only_marker() {
        assert_eq!(parse_package_spec("@"), ("@".to_string(), None));
    }

    #[test]
    fn parse_trailing_at_yields_no_version() {
        let (name, ver) = parse_package_spec("@scope/name@");
        assert_eq!(name, "@scope/name");
        assert_eq!(ver, None);
    }

    // ── validate_package_name ─────────────────────────────────────────────────

    #[test]
    fn validate_name_accepts_common_forms() {
        for ok in [
            "express",
            "@scope/name",
            "@sylphx/pdf-reader-mcp",
            "my.pkg_name-1",
        ] {
            assert!(validate_package_name(ok).is_ok(), "{ok:?} should be valid");
        }
    }

    #[test]
    fn validate_name_rejects_injection_and_malformed() {
        for bad in [
            "",               // empty
            "foo\";rm -rf /", // shell metacharacters
            "foo bar",        // space
            "@scope",         // scope without name
            "@scope/",        // empty package segment
            "@/name",         // empty scope segment
            ".hidden",        // leading dot
            "_private",       // leading underscore
            "foo/bar",        // unscoped slash
            "foo$(touch x)",  // command substitution
        ] {
            assert!(
                validate_package_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    // ── validate_version ──────────────────────────────────────────────────────

    #[test]
    fn validate_version_accepts_semver_and_tags() {
        for ok in ["1.2.3", "0.4.2-rc.1", "10.0.0+build.5", "latest", "next"] {
            assert!(validate_version(ok).is_ok(), "{ok:?} should be valid");
        }
    }

    #[test]
    fn validate_version_rejects_unsafe_ranges_and_injection() {
        // Rejected because they contain characters that are invalid in an OCI
        // tag and/or are shell metacharacters: `^`, `<`, `>`, spaces, backticks.
        for bad in ["^1.2.0", ">=1 <2", "", "v1.0.0 ; rm", "`whoami`"] {
            assert!(validate_version(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    // ── merge ─────────────────────────────────────────────────────────────────

    #[test]
    fn merge_defaults_when_no_package_config() {
        let global = NpxcConfig::default();
        let eff = merge::merge(&global, None);
        assert_eq!(eff.memory, "512m");
        assert_eq!(eff.cpus, "1");
        assert_eq!(eff.network, "none");
        assert_eq!(eff.node_image, "node:lts-slim");
        assert!(eff.version.is_none());
        assert!(eff.path_arguments.is_empty());
    }

    #[test]
    fn merge_package_runtime_overrides_globals() {
        use package::RuntimeOverride;
        let global = NpxcConfig::default();
        let pkg = PackageConfig {
            version: Some("1.0.0".to_string()),
            runtime: Some(RuntimeOverride {
                memory: Some("2g".to_string()),
                cpus: None,
                network: Some("bridge".to_string()),
            }),
            ..Default::default()
        };
        let eff = merge::merge(&global, Some(&pkg));
        assert_eq!(eff.memory, "2g");
        assert_eq!(eff.cpus, "1"); // falls back to global default
        assert_eq!(eff.network, "bridge");
        assert_eq!(eff.version, Some("1.0.0".to_string()));
    }

    // ── pin / load round-trip ─────────────────────────────────────────────────

    #[test]
    fn pin_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cdir = dir.path();
        let pkg = "@scope/mypkg";

        // Pin a version.
        pin_package_version(pkg, "2.3.4", cdir).expect("pin");

        // Load it back.
        let loaded = load_package_config(pkg, cdir).expect("load").expect("Some");
        assert_eq!(loaded.version, Some("2.3.4".to_string()));
        assert_eq!(loaded.package, Some(pkg.to_string()));
    }

    #[test]
    fn ensure_version_pinned_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cdir = dir.path().join("npxc");
        std::fs::create_dir_all(&cdir).unwrap();
        let pkg = "mypackage";
        let cfg_file = cdir.join("npxc.toml");

        // Pin twice with the same version — no error, file written once.
        pin_package_version(pkg, "1.0.0", &cdir).expect("first pin");
        let path = package_config_path(pkg, &cdir);
        let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Small sleep to ensure mtime would differ if the file were rewritten.
        std::thread::sleep(std::time::Duration::from_millis(10));

        ensure_version_pinned(pkg, "1.0.0", Some(&cfg_file)).expect("idempotent");
        let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert_eq!(mtime1, mtime2, "file should not have been rewritten");
    }

    #[test]
    fn load_missing_global_config_returns_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nonexistent = dir.path().join("npxc.toml");
        let cfg = load_global_config(Some(&nonexistent)).expect("load");
        assert_eq!(cfg.defaults.node_image, "node:lts-slim");
    }

    #[test]
    fn load_missing_package_config_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = load_package_config("no-such-pkg", dir.path()).expect("ok");
        assert!(result.is_none());
    }
}
