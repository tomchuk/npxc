use std::collections::HashMap;

use super::{global::NpxcConfig, package::PackageConfig};

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
    pub network: String,
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

    // Resolve memory / cpus / network: package runtime wins over global defaults.
    let (network, memory, cpus) = match pkg.and_then(|c| c.runtime.as_ref()) {
        Some(rt) => (
            rt.network.clone().unwrap_or_else(|| d.network.clone()),
            rt.memory.clone().unwrap_or_else(|| d.memory.clone()),
            rt.cpus.clone().unwrap_or_else(|| d.cpus.clone()),
        ),
        None => (d.network.clone(), d.memory.clone(), d.cpus.clone()),
    };

    // Pull per-package fields (defaults are empty/None when no config exists).
    let (version, path_arguments, non_path_arguments) = match pkg {
        Some(c) => (
            c.version.clone(),
            c.path_arguments.clone(),
            c.non_path_arguments.clone(),
        ),
        None => (None, HashMap::new(), HashMap::new()),
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
    }
}
