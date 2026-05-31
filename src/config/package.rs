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
/// [env]
/// NODE_OPTIONS = "--max-old-space-size=256"
///
/// env_passthrough = ["OPENAI_API_KEY"]
///
/// [storage]
/// persist = true
///
/// [[mounts]]
/// host      = "."
/// container = "/project"
/// mode      = "ro"
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

    /// Literal environment variables injected into the container.
    ///
    /// These are non-secret values suitable for storing in config (e.g.
    /// `NODE_OPTIONS`, feature flags).  For secrets, use `env_passthrough`
    /// instead — those values come from npxc's own process environment and
    /// never touch the config file.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Names of environment variables to forward from npxc's own environment
    /// into the container.
    ///
    /// The container inherits the *value* at launch time; the name is the
    /// only thing that appears in config, so secrets (API keys, tokens) are
    /// never written to disk.
    ///
    /// Example: `env_passthrough = ["OPENAI_API_KEY", "GITHUB_TOKEN"]`
    #[serde(default)]
    pub env_passthrough: Vec<String>,

    /// Persistent and writable storage options.
    ///
    /// When `storage.persist` is `true`, a per-package host directory is
    /// created under the platform data directory and mounted read-write at
    /// `/data` inside the container.  This is the primary mechanism for
    /// state-bearing MCP servers such as `server-memory`.
    pub storage: Option<StorageConfig>,

    /// Extra filesystem mounts beyond the session workspace.
    ///
    /// Each entry is a `host → container` mapping with an optional mode.
    /// Host paths are validated to lie within the current working directory
    /// (same rules as per-file publication).
    #[serde(default)]
    pub mounts: Vec<MountConfig>,

    /// Network / egress policy for the package.
    ///
    /// When present, this `[network]` table is authoritative for the
    /// container's networking and takes precedence over the legacy
    /// `[runtime] network` string.
    pub network: Option<NetworkConfig>,
}

/// Per-package container-runtime resource overrides.  Any field left as
/// `None` falls back to the global `[defaults]` value.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RuntimeOverride {
    pub memory: Option<String>,
    pub cpus: Option<String>,
    pub network: Option<String>,
}

/// Persistent storage configuration for a package.
///
/// Example in `packages/<name>.toml`:
/// ```toml
/// [storage]
/// persist  = true
/// writable = ["/cache"]   # additional writable container paths
/// ```
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// When `true`, mount a per-package persistent directory read-write at
    /// `/data` inside the container.  The host directory is created on first
    /// run under `<platform_data_dir>/npxc/packages/<sanitized_name>/`.
    #[serde(default)]
    pub persist: bool,

    /// Additional container paths to make writable via `--tmpfs`.
    ///
    /// Unlike `persist`, these are *not* backed by a host directory — they
    /// are ephemeral tmpfs mounts that disappear when the container stops.
    /// Useful for packages that write to a fixed path but don't need
    /// durability.
    #[serde(default)]
    pub writable: Vec<String>,
}

/// A filesystem mount injected into the container beyond the session workspace.
///
/// Example in `packages/<name>.toml`:
/// ```toml
/// [[mounts]]
/// host      = "."            # relative to CWD, or absolute (validated)
/// container = "/workspace"
/// mode      = "ro"           # "ro" | "rw"  (default: "ro")
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    /// Host-side path.  Relative paths are resolved against the effective CWD.
    /// The resolved path must be within the CWD scope (same rules as per-file
    /// publication) unless `--no-isolate` is active.
    pub host: String,

    /// Absolute path inside the container where the host directory is mounted.
    pub container: String,

    /// Mount mode: `"ro"` for read-only (default) or `"rw"` for read-write.
    #[serde(default = "default_mount_mode")]
    pub mode: String,
}

fn default_mount_mode() -> String {
    "ro".to_string()
}

/// Network / egress policy for a package.
///
/// Example in `packages/<name>.toml`:
/// ```toml
/// [network]
/// mode  = "allowlist"        # "none" (default) | "open" | "allowlist"
/// allow = ["api.anthropic.com:443"]
/// ```
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Network mode:
    /// - `"none"` — no network interface (the default).
    /// - `"open"` — the built-in NAT network; unfiltered internet access.
    /// - `"allowlist"` — a per-session host-only network whose egress is
    ///   restricted to `allow`.
    ///
    /// Any other value is treated as a literal container network name.
    #[serde(default = "default_network_mode")]
    pub mode: String,

    /// Egress allowlist entries (`host[:port]` or `cidr[:port]`), used when
    /// `mode = "allowlist"`.  An empty list denies all egress.
    #[serde(default)]
    pub allow: Vec<String>,
}

fn default_network_mode() -> String {
    "none".to_string()
}
