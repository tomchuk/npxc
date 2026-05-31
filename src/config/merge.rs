use std::collections::HashMap;

use super::{
    global::{Defaults, NpxcConfig},
    package::{MountConfig, PackageConfig, StorageConfig},
};

/// Resolved network / egress policy for a single invocation.
///
/// Produced by [`merge`] from the package `[network]` table (authoritative
/// when present) or the legacy `network` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No network interface (`--network none`).
    None,
    /// A named container network passed to `--network` verbatim.
    Named(String),
    /// A per-session isolated host-only network with an egress allowlist.
    Allowlist { allow: Vec<String> },
}

impl std::fmt::Display for NetworkPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkPolicy::None => write!(f, "none"),
            NetworkPolicy::Named(name) => write!(f, "{name}"),
            NetworkPolicy::Allowlist { allow } => {
                write!(f, "allowlist ({} rule(s))", allow.len())
            }
        }
    }
}

/// The fully-resolved, ready-to-use configuration for a single invocation.
///
/// Produced by [`merge`] from a [`NpxcConfig`] and an optional
/// [`PackageConfig`].  All values are concrete strings — no more `Option`
/// indirection for fields that have a global default.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    // ── Image / runtime ──────────────────────────────────────────────────────
    pub node_image: String,
    pub container_cli: String,
    pub network: NetworkPolicy,
    pub memory: String,
    pub cpus: String,
    pub mount_mode: String,
    pub log_level: String,

    // ── Path-resolution strategies ───────────────────────────────────────────
    pub strategies: Vec<String>,
    pub heuristic_absolute_prefix: bool,
    pub heuristic_home_prefix: bool,
    pub heuristic_uri_prefix: Vec<String>,

    // ── Per-package fields (empty / None when no package config exists) ──────
    /// Pinned version from the package config file (before CLI-spec override).
    pub version: Option<String>,
    /// Tool → path-argument names mapping from the package config.
    pub path_arguments: HashMap<String, Vec<String>>,
    /// Tool → non-path-argument names mapping from the package config.
    pub non_path_arguments: HashMap<String, Vec<String>>,

    /// Literal environment variables to inject into the container (`-e K=V`).
    pub env: HashMap<String, String>,
    /// Names of environment variables to forward from npxc's process env
    /// into the container (`-e K`).
    pub env_passthrough: Vec<String>,

    /// Persistent and writable storage options from the package config.
    pub storage: Option<StorageConfig>,

    /// Extra filesystem mounts from the package config.
    pub mounts: Vec<MountConfig>,
}

/// Merge a global config with optional per-package overrides into a single
/// [`EffectiveConfig`].
///
/// Resolution order for resource fields:
/// 1. Package `[runtime]` override (if present and not `None`)
/// 2. Global `[defaults]`
#[must_use]
pub fn merge(global: &NpxcConfig, pkg: Option<&PackageConfig>) -> EffectiveConfig {
    let d = &global.defaults;
    let p = &global.paths;

    // Resolve memory / cpus: package runtime wins over global defaults.
    let (memory, cpus) = match pkg.and_then(|c| c.runtime.as_ref()) {
        Some(rt) => (
            rt.memory.clone().unwrap_or_else(|| d.memory.clone()),
            rt.cpus.clone().unwrap_or_else(|| d.cpus.clone()),
        ),
        None => (d.memory.clone(), d.cpus.clone()),
    };

    let network = resolve_network_policy(pkg, d);

    // Pull per-package fields (defaults are empty/None when no config exists).
    let (version, path_arguments, non_path_arguments, env, env_passthrough, storage, mounts) =
        match pkg {
            Some(c) => (
                c.version.clone(),
                c.path_arguments.clone(),
                c.non_path_arguments.clone(),
                c.env.clone(),
                c.env_passthrough.clone(),
                c.storage.clone(),
                c.mounts.clone(),
            ),
            None => (
                None,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                Vec::new(),
                None,
                Vec::new(),
            ),
        };

    EffectiveConfig {
        node_image: d.node_image.clone(),
        container_cli: d.container_cli.clone(),
        network,
        memory,
        cpus,
        mount_mode: d.mount_mode.clone(),
        log_level: d.log_level.clone(),
        strategies: p.strategies.clone(),
        heuristic_absolute_prefix: p.heuristic.absolute_prefix,
        heuristic_home_prefix: p.heuristic.home_prefix,
        heuristic_uri_prefix: p.heuristic.uri_prefix.clone(),
        version,
        path_arguments,
        non_path_arguments,
        env,
        env_passthrough,
        storage,
        mounts,
    }
}

/// Resolve the network policy.
///
/// The package `[network]` table is authoritative when present; otherwise the
/// legacy `[runtime] network` string (falling back to `[defaults] network`)
/// is mapped: `"none"` → [`NetworkPolicy::None`], anything else → a named
/// network.
fn resolve_network_policy(pkg: Option<&PackageConfig>, d: &Defaults) -> NetworkPolicy {
    if let Some(nc) = pkg.and_then(|c| c.network.as_ref()) {
        return match nc.mode.as_str() {
            "allowlist" => NetworkPolicy::Allowlist {
                allow: nc.allow.clone(),
            },
            "open" => NetworkPolicy::Named("default".to_string()),
            "none" => NetworkPolicy::None,
            // Any other value is treated as a literal network name.
            other => NetworkPolicy::Named(other.to_string()),
        };
    }

    let legacy = pkg
        .and_then(|c| c.runtime.as_ref())
        .and_then(|r| r.network.clone())
        .unwrap_or_else(|| d.network.clone());
    match legacy.as_str() {
        "none" => NetworkPolicy::None,
        other => NetworkPolicy::Named(other.to_string()),
    }
}
