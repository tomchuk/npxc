use serde::{Deserialize, Serialize};

/// Top-level structure for `~/.config/npxc/npxc.toml` (or the path supplied
/// via `--config`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NpxcConfig {
    pub defaults: Defaults,
    pub paths: PathsConfig,
}

/// Execution defaults that can be overridden per-package via `[runtime]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub node_image: String,
    pub container_cli: String,
    pub network: String,
    pub memory: String,
    pub cpus: String,
    pub mount_mode: String,
    pub log_level: String,
}

/// Path-resolution strategy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Ordered list of strategies to apply when resolving tool arguments to
    /// filesystem paths.  Supported values: `"config"`, `"schema"`,
    /// `"heuristic"`.
    pub strategies: Vec<String>,
    pub heuristic: HeuristicConfig,
}

/// Heuristic rules used when the `"heuristic"` strategy is active.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HeuristicConfig {
    /// Treat arguments that look like absolute paths as file paths.
    pub absolute_prefix: bool,
    /// Treat arguments that start with `~` as file paths.
    pub home_prefix: bool,
    /// URI prefixes whose presence indicates a file path argument.
    pub uri_prefix: Vec<String>,
}

// ── Default implementations ──────────────────────────────────────────────────

impl Default for Defaults {
    fn default() -> Self {
        Self {
            node_image: "node:lts-slim".to_string(),
            container_cli: "container".to_string(),
            network: "none".to_string(),
            memory: "512m".to_string(),
            cpus: "1".to_string(),
            mount_mode: "ro".to_string(),
            log_level: "warn".to_string(),
        }
    }
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            strategies: vec![
                "config".to_string(),
                "schema".to_string(),
                "heuristic".to_string(),
            ],
            heuristic: HeuristicConfig::default(),
        }
    }
}

impl Default for HeuristicConfig {
    fn default() -> Self {
        Self {
            absolute_prefix: true,
            home_prefix: true,
            uri_prefix: vec!["file://".to_string()],
        }
    }
}
