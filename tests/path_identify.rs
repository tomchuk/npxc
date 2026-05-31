//! Integration tests for path-argument identification — `npxc::paths::identify`.

use std::collections::HashMap;

use npxc::config::EffectiveConfig;
use npxc::paths::identify::identify_path_args;
use npxc::rpc::message::ToolSchema;
use serde_json::{Map, Value, json};

// ── helpers ───────────────────────────────────────────────────────────────────

fn base_config() -> EffectiveConfig {
    EffectiveConfig {
        node_image: String::new(),
        container_cli: String::new(),
        network: String::new(),
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

fn str_args(pairs: &[(&str, &str)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect()
}

fn make_schemas(tool: &str, properties: &Value) -> HashMap<String, ToolSchema> {
    let mut schemas = HashMap::new();
    schemas.insert(
        tool.to_string(),
        ToolSchema {
            name: tool.to_string(),
            input_schema: json!({ "type": "object", "properties": properties }),
        },
    );
    schemas
}

fn no_schemas() -> HashMap<String, ToolSchema> {
    HashMap::new()
}

/// Extract JSON pointer components from results for clean assertions.
fn ptrs(v: &[(String, String)]) -> Vec<&str> {
    v.iter().map(|(p, _)| p.as_str()).collect()
}

// ── config strategy ───────────────────────────────────────────────────────────

#[test]
fn config_strategy_wildcard_identifies_path_not_url() {
    let mut config = base_config();
    config.strategies = vec!["config".into()];
    config
        .path_arguments
        .insert("*".into(), vec!["path".into()]);

    let args = str_args(&[("path", "/foo/bar.txt"), ("url", "http://example.com")]);
    let result = identify_path_args("any_tool", &args, &config, &no_schemas());
    let ps = ptrs(&result);

    assert!(ps.contains(&"/path"), "path should be identified");
    assert!(!ps.contains(&"/url"), "url should not be identified");
}

#[test]
fn config_strategy_tool_specific_match() {
    let mut config = base_config();
    config.strategies = vec!["config".into()];
    config
        .path_arguments
        .insert("read_pdf".into(), vec!["path".into()]);

    let args = str_args(&[("path", "/tmp/doc.pdf")]);
    let result = identify_path_args("read_pdf", &args, &config, &no_schemas());
    assert!(ptrs(&result).contains(&"/path"));
}

#[test]
fn config_strategy_skips_non_string_values() {
    let mut config = base_config();
    config.strategies = vec!["config".into()];
    config
        .path_arguments
        .insert("*".into(), vec!["path".into()]);

    let mut args = Map::new();
    args.insert("path".into(), json!(42));
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    assert!(result.is_empty());
}

// ── heuristic strategy ────────────────────────────────────────────────────────

#[test]
fn heuristic_absolute_prefix_identifies_path() {
    let mut config = base_config();
    config.strategies = vec!["heuristic".into()];
    config.heuristic_absolute_prefix = true;

    let args = str_args(&[("input", "/absolute/path.txt"), ("name", "relative_name")]);
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    let ps = ptrs(&result);

    assert!(ps.contains(&"/input"), "absolute path should be identified");
    assert!(
        !ps.contains(&"/name"),
        "plain name should not be identified"
    );
}

#[test]
fn heuristic_relative_paths_not_identified() {
    let mut config = base_config();
    config.strategies = vec!["heuristic".into()];
    config.heuristic_absolute_prefix = true;
    config.heuristic_home_prefix = true;

    let args = str_args(&[("rel", "./relative/file.txt"), ("plain", "just_a_name")]);
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    assert!(
        result.is_empty(),
        "relative/plain args should not be identified: {result:?}"
    );
}

#[test]
fn heuristic_home_prefix_identifies_path() {
    let mut config = base_config();
    config.strategies = vec!["heuristic".into()];
    config.heuristic_home_prefix = true;

    let args = str_args(&[("path", "~/Documents/report.pdf")]);
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    assert!(ptrs(&result).contains(&"/path"), "~/… should be identified");
}

#[test]
fn heuristic_file_uri_prefix_identifies_path() {
    let mut config = base_config();
    config.strategies = vec!["heuristic".into()];
    config.heuristic_uri_prefix = vec!["file://".into()];

    let args = str_args(&[("src", "file:///tmp/data.txt")]);
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    assert!(
        ptrs(&result).contains(&"/src"),
        "file:// URI should be identified"
    );
}

#[test]
fn heuristic_nested_array_of_objects() {
    let mut config = base_config();
    config.strategies = vec!["heuristic".into()];
    config.heuristic_absolute_prefix = true;

    // Mirrors the real @sylphx/pdf-reader-mcp schema.
    let args: Map<String, Value> = json!({
        "sources": [
            { "path": "/Users/tom/docs/report.pdf" },
            { "url": "https://example.com/other.pdf" }
        ],
        "include_full_text": true
    })
    .as_object()
    .unwrap()
    .clone();

    let result = identify_path_args("read_pdf", &args, &config, &no_schemas());
    let ps = ptrs(&result);

    assert!(
        ps.contains(&"/sources/0/path"),
        "nested path not found: {ps:?}"
    );
    assert!(
        !ps.contains(&"/sources/1/url"),
        "https url should not be a path"
    );
}

// ── schema strategy ───────────────────────────────────────────────────────────

#[test]
fn schema_strategy_format_path_identifies_arg() {
    let mut config = base_config();
    config.strategies = vec!["schema".into()];
    let schemas = make_schemas(
        "read_file",
        &json!({
            "path": { "type": "string", "format": "path" },
            "count": { "type": "integer" },
        }),
    );

    let mut args = str_args(&[("path", "/some/file.txt")]);
    args.insert("count".into(), json!(5));

    let result = identify_path_args("read_file", &args, &config, &schemas);
    assert!(ptrs(&result).contains(&"/path"));
    assert!(!ptrs(&result).contains(&"/count"));
}

#[test]
fn schema_strategy_description_file_path_identifies_arg() {
    let mut config = base_config();
    config.strategies = vec!["schema".into()];
    let schemas = make_schemas(
        "process",
        &json!({ "input": { "type": "string", "description": "The file path to process" } }),
    );

    let args = str_args(&[("input", "/data/file.txt")]);
    let result = identify_path_args("process", &args, &config, &schemas);
    assert!(ptrs(&result).contains(&"/input"));
}

#[test]
fn schema_strategy_description_absolute_path_identifies_arg() {
    let mut config = base_config();
    config.strategies = vec!["schema".into()];
    let schemas = make_schemas(
        "scan",
        &json!({ "target": { "type": "string", "description": "Absolute path to the target directory" } }),
    );

    let args = str_args(&[("target", "/var/log")]);
    let result = identify_path_args("scan", &args, &config, &schemas);
    assert!(ptrs(&result).contains(&"/target"));
}

#[test]
fn schema_strategy_plain_string_property_not_identified() {
    let mut config = base_config();
    config.strategies = vec!["schema".into()];
    let schemas = make_schemas(
        "query",
        &json!({ "q": { "type": "string", "description": "Search query" } }),
    );

    let args = str_args(&[("q", "hello world")]);
    let result = identify_path_args("query", &args, &config, &schemas);
    assert!(
        !ptrs(&result).contains(&"/q"),
        "plain string should not be identified"
    );
}

// ── non_path suppression ──────────────────────────────────────────────────────

#[test]
fn non_path_suppression_overrides_heuristic() {
    let mut config = base_config();
    config.strategies = vec!["heuristic".into()];
    config.heuristic_absolute_prefix = true;
    config
        .non_path_arguments
        .insert("*".into(), vec!["url".into()]);

    let args = str_args(&[("url", "/looks/like/a/path"), ("path", "/real/path.txt")]);
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    let ps = ptrs(&result);

    assert!(!ps.contains(&"/url"), "url should be suppressed");
    assert!(ps.contains(&"/path"), "path should still be identified");
}

#[test]
fn non_path_tool_specific_suppression() {
    let mut config = base_config();
    config.strategies = vec!["heuristic".into()];
    config.heuristic_absolute_prefix = true;
    config
        .non_path_arguments
        .insert("render".into(), vec!["output".into()]);

    let args = str_args(&[("output", "/tmp/out.png"), ("input", "/tmp/in.png")]);

    let result_render = identify_path_args("render", &args, &config, &no_schemas());
    assert!(!ptrs(&result_render).contains(&"/output"));
    assert!(ptrs(&result_render).contains(&"/input"));

    let result_other = identify_path_args("other", &args, &config, &no_schemas());
    assert!(ptrs(&result_other).contains(&"/output"));
    assert!(ptrs(&result_other).contains(&"/input"));
}

// ── combined strategies ───────────────────────────────────────────────────────

#[test]
fn combined_config_and_heuristic_union() {
    let mut config = base_config();
    config.strategies = vec!["config".into(), "heuristic".into()];
    config.heuristic_absolute_prefix = true;
    config
        .path_arguments
        .insert("*".into(), vec!["explicit_path".into()]);

    let args = str_args(&[
        ("explicit_path", "not-absolute-but-in-config"),
        ("heuristic_path", "/absolute/via/heuristic"),
    ]);
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    let ps = ptrs(&result);

    assert!(
        ps.contains(&"/explicit_path"),
        "config strategy should identify explicit_path"
    );
    assert!(
        ps.contains(&"/heuristic_path"),
        "heuristic should identify absolute path"
    );
}

#[test]
fn empty_strategy_list_identifies_nothing() {
    let mut config = base_config();
    config.strategies = vec![];
    config.heuristic_absolute_prefix = true;
    config
        .path_arguments
        .insert("*".into(), vec!["path".into()]);

    let args = str_args(&[("path", "/some/path.txt")]);
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    assert!(
        result.is_empty(),
        "empty strategy list should identify nothing"
    );
}

#[test]
fn heuristic_only_strategy_ignores_config_map() {
    let mut config = base_config();
    config.strategies = vec!["heuristic".into()];
    config.heuristic_absolute_prefix = true;
    config
        .path_arguments
        .insert("*".into(), vec!["name".into()]);

    let args = str_args(&[("abs", "/absolute/path"), ("name", "some_plain_name")]);
    let result = identify_path_args("tool", &args, &config, &no_schemas());
    let ps = ptrs(&result);

    assert!(
        ps.contains(&"/abs"),
        "absolute path should be identified by heuristic"
    );
    assert!(
        !ps.contains(&"/name"),
        "non-absolute value should not be identified by heuristic"
    );
}
