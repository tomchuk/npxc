use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Per-package configuration stored at
/// `<config_dir>/packages/<sanitized_name>.toml`.
///
/// Example file:
/// ```toml
/// package = "@sylphx/pdf-reader-mcp"
/// version = "0.4.2"
///
/// [path_arguments]
/// "*"        = ["path", "file", "filename", "input"]
/// "read_pdf" = ["path"]
///
/// [non_path_arguments]
/// "*" = ["url", "query", "pattern"]
///
/// [runtime]
/// memory = "1g"
/// ```
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PackageConfig {
    /// Canonical npm package name (e.g. `@sylphx/pdf-reader-mcp`).
    pub package: Option<String>,

    /// Pinned version string (e.g. `"0.4.2"`).  When present, this takes
    /// precedence over `"latest"` but yields to a version embedded directly
    /// in the CLI package spec.
    pub version: Option<String>,

    /// Maps tool/function names (or `"*"` for a wildcard) to the list of
    /// argument names that should be treated as filesystem paths.
    #[serde(default)]
    pub path_arguments: HashMap<String, Vec<String>>,

    /// Maps tool/function names (or `"*"` for a wildcard) to the list of
    /// argument names that are explicitly *not* filesystem paths (e.g. URLs,
    /// query strings).
    #[serde(default)]
    pub non_path_arguments: HashMap<String, Vec<String>>,

    /// Optional container-runtime overrides for this package.
    pub runtime: Option<RuntimeOverride>,
}

/// Per-package container-runtime resource overrides.  Any field left as
/// `None` falls back to the global `[defaults]` value.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RuntimeOverride {
    pub memory: Option<String>,
    pub cpus: Option<String>,
    pub network: Option<String>,
}
