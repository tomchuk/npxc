use std::{
    collections::{HashMap, HashSet},
    hash::BuildHasher,
};

use serde_json::{Map, Value};

use crate::{config::merge::EffectiveConfig, rpc::message::ToolSchema};

/// Identify path-like string values within `arguments`.
///
/// Returns `(json_pointer, string_value)` pairs where the pointer is relative
/// to the `arguments` object (e.g. `"/path"` for a top-level key or
/// `"/sources/0/path"` for a value nested inside an array).
///
/// Strategies are tried in order; results are unioned. After all strategies
/// have run, any entry whose final pointer component appears in
/// `non_path_arguments` for this tool is removed.
pub fn identify_path_args<S: BuildHasher>(
    tool_name: &str,
    arguments: &Map<String, Value>,
    config: &EffectiveConfig,
    schemas: &HashMap<String, ToolSchema, S>,
) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    // Track pointers already classified to avoid duplicates across strategies.
    let mut seen: HashSet<String> = HashSet::new();

    for strategy in &config.strategies {
        let results: Vec<(String, String)> = match strategy.as_str() {
            "config" => apply_config_strategy(tool_name, arguments, config),
            "schema" => apply_schema_strategy(tool_name, arguments, schemas),
            "heuristic" => apply_heuristic_strategy(arguments, config, &seen),
            _ => vec![],
        };
        for (ptr, val) in results {
            if seen.insert(ptr.clone()) {
                found.push((ptr, val));
            }
        }
    }

    // Build suppression set (wildcard + tool-specific non-path keys).
    let mut suppressed: HashSet<&str> = HashSet::new();
    if let Some(g) = config.non_path_arguments.get("*") {
        suppressed.extend(g.iter().map(String::as_str));
    }
    if let Some(t) = config.non_path_arguments.get(tool_name) {
        suppressed.extend(t.iter().map(String::as_str));
    }

    // Suppress by matching the last component of the pointer.
    found.retain(|(ptr, _)| {
        let last = ptr.rsplit('/').next().unwrap_or("");
        !suppressed.contains(last)
    });

    found
}

// ─── Strategy implementations ────────────────────────────────────────────────

/// **Config strategy** — use the explicit path-argument lists from
/// `config.path_arguments`. Only examines top-level argument keys.
fn apply_config_strategy(
    tool_name: &str,
    arguments: &Map<String, Value>,
    config: &EffectiveConfig,
) -> Vec<(String, String)> {
    let mut names: HashSet<&str> = HashSet::new();
    if let Some(g) = config.path_arguments.get("*") {
        names.extend(g.iter().map(String::as_str));
    }
    if let Some(t) = config.path_arguments.get(tool_name) {
        names.extend(t.iter().map(String::as_str));
    }

    names
        .iter()
        .filter_map(|&name| {
            let val = arguments.get(name)?.as_str()?;
            Some((format!("/{name}"), val.to_owned()))
        })
        .collect()
}

/// **Schema strategy** — inspect the MCP tool's `inputSchema` for top-level
/// properties that look like paths (`format: path/uri` or path-like
/// `description`). Only examines top-level argument keys.
fn apply_schema_strategy<S: BuildHasher>(
    tool_name: &str,
    arguments: &Map<String, Value>,
    schemas: &HashMap<String, ToolSchema, S>,
) -> Vec<(String, String)> {
    let Some(schema) = schemas.get(tool_name) else {
        return vec![];
    };
    let Some(props) = schema
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
    else {
        return vec![];
    };

    arguments
        .iter()
        .filter_map(|(key, val)| {
            let s = val.as_str()?;
            let prop = props.get(key)?;
            if is_path_property(prop) {
                Some((format!("/{key}"), s.to_owned()))
            } else {
                None
            }
        })
        .collect()
}

fn is_path_property(prop: &Value) -> bool {
    if let Some(fmt) = prop.get("format").and_then(Value::as_str) {
        let f = fmt.to_lowercase();
        if f == "path" || f == "uri" {
            return true;
        }
    }
    if let Some(desc) = prop.get("description").and_then(Value::as_str) {
        let d = desc.to_lowercase();
        if d.contains("file path") || d.contains("filesystem path") || d.contains("absolute path") {
            return true;
        }
    }
    false
}

/// **Heuristic strategy** — recursively walk *all* string values in the
/// arguments tree (including those nested in objects and arrays) and classify
/// any that look like paths by value shape.
///
/// Skips pointers already in `already_seen`. Relative paths are not
/// classified.
fn apply_heuristic_strategy(
    arguments: &Map<String, Value>,
    config: &EffectiveConfig,
    already_seen: &HashSet<String>,
) -> Vec<(String, String)> {
    let mut results = Vec::new();
    collect_heuristic(
        &Value::Object(arguments.clone()),
        "",
        config,
        already_seen,
        &mut results,
    );
    results
}

fn collect_heuristic(
    value: &Value,
    ptr: &str,
    config: &EffectiveConfig,
    already_seen: &HashSet<String>,
    out: &mut Vec<(String, String)>,
) {
    match value {
        Value::String(s) => {
            if !already_seen.contains(ptr) && is_path_by_heuristic(s, config) {
                out.push((ptr.to_owned(), s.clone()));
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                collect_heuristic(v, &format!("{ptr}/{i}"), config, already_seen, out);
            }
        }
        Value::Object(obj) => {
            for (k, v) in obj {
                collect_heuristic(v, &format!("{ptr}/{k}"), config, already_seen, out);
            }
        }
        _ => {}
    }
}

fn is_path_by_heuristic(value: &str, config: &EffectiveConfig) -> bool {
    if config.heuristic_absolute_prefix && value.starts_with('/') {
        return true;
    }
    if config.heuristic_home_prefix && value.starts_with("~/") {
        return true;
    }
    config
        .heuristic_uri_prefix
        .iter()
        .any(|p| value.starts_with(p.as_str()))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_config() -> EffectiveConfig {
        EffectiveConfig {
            node_image: String::new(),
            container_cli: String::new(),
            network: crate::config::merge::NetworkPolicy::None,
            memory: String::new(),
            cpus: String::new(),
            mount_mode: String::new(),
            log_level: String::new(),
            strategies: vec![],
            heuristic_absolute_prefix: false,
            heuristic_home_prefix: false,
            heuristic_uri_prefix: vec![],
            version: None,
            path_arguments: HashMap::new(),
            non_path_arguments: HashMap::new(),
            env: HashMap::new(),
            env_passthrough: Vec::new(),
            storage: None,
            mounts: Vec::new(),
        }
    }

    /// Extract just the JSON pointer components for assertion convenience.
    fn ptrs(v: &[(String, String)]) -> Vec<&str> {
        v.iter().map(|(p, _)| p.as_str()).collect()
    }

    // ── config strategy ──────────────────────────────────────────────────────

    #[test]
    fn config_strategy_wildcard_match() {
        let mut config = base_config();
        config.strategies = vec!["config".into()];
        config
            .path_arguments
            .insert("*".into(), vec!["file".into()]);

        let args = json!({"file": "/tmp/a.txt", "count": 3})
            .as_object()
            .unwrap()
            .clone();
        let result = identify_path_args("any_tool", &args, &config, &HashMap::new());
        assert!(ptrs(&result).contains(&"/file"));
        assert!(!ptrs(&result).contains(&"/count"));
    }

    #[test]
    fn config_strategy_tool_specific_match() {
        let mut config = base_config();
        config.strategies = vec!["config".into()];
        config
            .path_arguments
            .insert("my_tool".into(), vec!["output".into()]);

        let args = json!({"output": "/out/report.pdf"})
            .as_object()
            .unwrap()
            .clone();
        let result = identify_path_args("my_tool", &args, &config, &HashMap::new());
        assert!(ptrs(&result).contains(&"/output"));
    }

    #[test]
    fn config_strategy_ignores_non_string_values() {
        let mut config = base_config();
        config.strategies = vec!["config".into()];
        config
            .path_arguments
            .insert("*".into(), vec!["path".into()]);

        let args = json!({"path": 42}).as_object().unwrap().clone();
        let result = identify_path_args("t", &args, &config, &HashMap::new());
        assert!(result.is_empty());
    }

    // ── schema strategy ──────────────────────────────────────────────────────

    fn make_schema(tool: &str, props: &serde_json::Value) -> HashMap<String, ToolSchema> {
        let mut schemas = HashMap::new();
        schemas.insert(
            tool.to_owned(),
            ToolSchema {
                name: tool.to_owned(),
                input_schema: json!({ "type": "object", "properties": props }),
            },
        );
        schemas
    }

    #[test]
    fn schema_strategy_format_path() {
        let mut config = base_config();
        config.strategies = vec!["schema".into()];
        let schemas = make_schema("tool", &json!({"src": {"format": "path"}}));

        let args = json!({"src": "/etc/hosts"}).as_object().unwrap().clone();
        let result = identify_path_args("tool", &args, &config, &schemas);
        assert!(ptrs(&result).contains(&"/src"));
    }

    #[test]
    fn schema_strategy_format_uri_case_insensitive() {
        let mut config = base_config();
        config.strategies = vec!["schema".into()];
        let schemas = make_schema("tool", &json!({"u": {"format": "URI"}}));

        let args = json!({"u": "file:///tmp/x"}).as_object().unwrap().clone();
        let result = identify_path_args("tool", &args, &config, &schemas);
        assert!(ptrs(&result).contains(&"/u"));
    }

    #[test]
    fn schema_strategy_description_file_path() {
        let mut config = base_config();
        config.strategies = vec!["schema".into()];
        let schemas = make_schema(
            "tool",
            &json!({"p": {"description": "The file path to read"}}),
        );

        let args = json!({"p": "/a/b"}).as_object().unwrap().clone();
        let result = identify_path_args("tool", &args, &config, &schemas);
        assert!(ptrs(&result).contains(&"/p"));
    }

    #[test]
    fn schema_strategy_description_absolute_path() {
        let mut config = base_config();
        config.strategies = vec!["schema".into()];
        let schemas = make_schema(
            "tool",
            &json!({"x": {"description": "Absolute path on disk"}}),
        );

        let args = json!({"x": "/root"}).as_object().unwrap().clone();
        let result = identify_path_args("tool", &args, &config, &schemas);
        assert!(ptrs(&result).contains(&"/x"));
    }

    #[test]
    fn schema_strategy_no_match_for_plain_string_property() {
        let mut config = base_config();
        config.strategies = vec!["schema".into()];
        let schemas = make_schema("tool", &json!({"name": {"type": "string"}}));

        let args = json!({"name": "Alice"}).as_object().unwrap().clone();
        let result = identify_path_args("tool", &args, &config, &schemas);
        assert!(result.is_empty());
    }

    // ── heuristic strategy ───────────────────────────────────────────────────

    #[test]
    fn heuristic_absolute_prefix() {
        let mut config = base_config();
        config.strategies = vec!["heuristic".into()];
        config.heuristic_absolute_prefix = true;

        let args = json!({"p": "/usr/bin/env", "q": "relative"})
            .as_object()
            .unwrap()
            .clone();
        let result = identify_path_args("t", &args, &config, &HashMap::new());
        assert!(ptrs(&result).contains(&"/p"));
        assert!(!ptrs(&result).contains(&"/q"));
    }

    #[test]
    fn heuristic_home_prefix() {
        let mut config = base_config();
        config.strategies = vec!["heuristic".into()];
        config.heuristic_home_prefix = true;

        let args = json!({"h": "~/Documents/note.txt"})
            .as_object()
            .unwrap()
            .clone();
        let result = identify_path_args("t", &args, &config, &HashMap::new());
        assert!(ptrs(&result).contains(&"/h"));
    }

    #[test]
    fn heuristic_uri_prefix() {
        let mut config = base_config();
        config.strategies = vec!["heuristic".into()];
        config.heuristic_uri_prefix = vec!["file://".into(), "s3://".into()];

        let args = json!({"a": "file:///tmp/x", "b": "s3://bucket/key", "c": "just text"})
            .as_object()
            .unwrap()
            .clone();
        let result = identify_path_args("t", &args, &config, &HashMap::new());
        assert!(ptrs(&result).contains(&"/a"));
        assert!(ptrs(&result).contains(&"/b"));
        assert!(!ptrs(&result).contains(&"/c"));
    }

    #[test]
    fn heuristic_relative_paths_not_classified() {
        let mut config = base_config();
        config.strategies = vec!["heuristic".into()];
        config.heuristic_absolute_prefix = true;

        let args = json!({"r": "./foo/bar", "r2": "../baz"})
            .as_object()
            .unwrap()
            .clone();
        let result = identify_path_args("t", &args, &config, &HashMap::new());
        assert!(result.is_empty());
    }

    /// Heuristic recurses into nested arrays and objects.
    #[test]
    fn heuristic_nested_array_of_objects() {
        let mut config = base_config();
        config.strategies = vec!["heuristic".into()];
        config.heuristic_absolute_prefix = true;

        // Mirrors the real @sylphx/pdf-reader-mcp schema.
        let args = json!({
            "sources": [
                { "path": "/Users/tom/docs/report.pdf" },
                { "url": "https://example.com/other.pdf" }
            ],
            "include_full_text": true
        })
        .as_object()
        .unwrap()
        .clone();

        let result = identify_path_args("read_pdf", &args, &config, &HashMap::new());
        let ps = ptrs(&result);
        assert!(
            ps.contains(&"/sources/0/path"),
            "nested path not found: {ps:?}"
        );
        assert!(!ps.contains(&"/sources/1/url"), "url should not be a path");
    }

    // ── non-path suppression ─────────────────────────────────────────────────

    #[test]
    fn non_path_suppression_removes_classified_key() {
        let mut config = base_config();
        config.strategies = vec!["heuristic".into()];
        config.heuristic_absolute_prefix = true;
        config
            .non_path_arguments
            .insert("*".into(), vec!["p".into()]);

        let args = json!({"p": "/etc/passwd"}).as_object().unwrap().clone();
        let result = identify_path_args("t", &args, &config, &HashMap::new());
        assert!(result.is_empty());
    }

    #[test]
    fn non_path_tool_specific_suppression() {
        let mut config = base_config();
        config.strategies = vec!["heuristic".into()];
        config.heuristic_absolute_prefix = true;
        config
            .non_path_arguments
            .insert("my_tool".into(), vec!["p".into()]);

        let args = json!({"p": "/etc/passwd"}).as_object().unwrap().clone();

        let r1 = identify_path_args("my_tool", &args, &config, &HashMap::new());
        assert!(r1.is_empty());

        let r2 = identify_path_args("other_tool", &args, &config, &HashMap::new());
        assert!(ptrs(&r2).contains(&"/p"));
    }

    // ── union / ordering ─────────────────────────────────────────────────────

    #[test]
    fn union_semantics_across_strategies() {
        let mut config = base_config();
        config.strategies = vec!["config".into(), "heuristic".into()];
        config.heuristic_absolute_prefix = true;
        config
            .path_arguments
            .insert("*".into(), vec!["explicit".into()]);

        let args = json!({"explicit": "/a", "auto": "/b", "skip": "text"})
            .as_object()
            .unwrap()
            .clone();
        let result = identify_path_args("t", &args, &config, &HashMap::new());
        let ps = ptrs(&result);

        assert!(ps.contains(&"/explicit"));
        assert!(ps.contains(&"/auto"));
        assert!(!ps.contains(&"/skip"));
    }
}
