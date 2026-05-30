//! Integration tests for the core path-rewriting pipeline logic.
//!
//! These tests exercise the `publish_file` + `replace_in_strings` round-trip
//! that forms the heart of the client→server and server→client pipelines.
//! No container process is required.
//!
//! Gated by `#![cfg(not(feature = "e2e"))]`: runs in plain `cargo test` but is
//! skipped when `--features e2e` is passed, where real end-to-end tests that
//! require an actual container runtime would take over.

#![cfg(not(feature = "e2e"))]

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use npxc::paths::publish::{PublicationCache, publish_file};
use npxc::rpc::message::replace_in_strings;
use serde_json::json;

// ── helpers ───────────────────────────────────────────────────────────────────

fn fresh_cache() -> Arc<Mutex<PublicationCache>> {
    Arc::new(Mutex::new(PublicationCache::new()))
}

// ── publish_file basics ───────────────────────────────────────────────────────

#[tokio::test]
async fn publish_file_returns_workspace_path() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let file = src.path().join("document.pdf");
    std::fs::write(&file, b"%PDF-1.4 fake content").unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();

    let cache = fresh_cache();
    let container_path = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();

    assert!(
        container_path.starts_with("/workspace/"),
        "container path should be under /workspace/: {container_path}"
    );
    assert!(
        container_path.ends_with("/document.pdf"),
        "container path should preserve the original filename: {container_path}"
    );
}

#[tokio::test]
async fn publish_file_deduplicates_same_file() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let file = src.path().join("report.pdf");
    std::fs::write(&file, b"content").unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();

    let cache = fresh_cache();
    let path1 = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();
    let path2 = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();

    assert_eq!(
        path1, path2,
        "publishing the same file twice must return the same container path"
    );
}

// ── reverse_snapshot ─────────────────────────────────────────────────────────

#[tokio::test]
async fn reverse_snapshot_after_single_publish() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let file = src.path().join("notes.txt");
    std::fs::write(&file, b"hello").unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();

    let cache = fresh_cache();
    let container_path = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();

    let snapshot = cache.lock().reverse_snapshot();
    assert_eq!(snapshot.len(), 1, "exactly one entry after one publish");

    let (cont, host) = &snapshot[0];
    assert_eq!(cont, &container_path);
    assert_eq!(host, canonical.to_str().unwrap());
}

#[tokio::test]
async fn reverse_snapshot_multiple_files() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let f1 = src.path().join("file1.txt");
    let f2 = src.path().join("file2.txt");
    std::fs::write(&f1, b"aaa").unwrap();
    std::fs::write(&f2, b"bbb").unwrap();
    let c1 = std::fs::canonicalize(&f1).unwrap();
    let c2 = std::fs::canonicalize(&f2).unwrap();

    let cache = fresh_cache();
    let cp1 = publish_file(&c1, session.path(), &cache).await.unwrap();
    let cp2 = publish_file(&c2, session.path(), &cache).await.unwrap();

    assert_ne!(cp1, cp2, "distinct files must get distinct container paths");

    let snapshot: HashMap<String, String> = cache.lock().reverse_snapshot().into_iter().collect();

    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[&cp1], c1.to_str().unwrap());
    assert_eq!(snapshot[&cp2], c2.to_str().unwrap());
}

// ── simulated client→server (c2s) path rewriting ─────────────────────────────
//
// When a tools/call arrives from the client with a host path in the arguments,
// the pipeline should replace that path with the container path before
// forwarding to the server.

#[tokio::test]
async fn c2s_host_path_replaced_with_container_path() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let file = src.path().join("analysis.pdf");
    std::fs::write(&file, b"%PDF").unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();

    let cache = fresh_cache();
    let container_path = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();

    // Simulate the tools/call as received from the client (contains host path).
    let mut call_msg = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "read_pdf",
            "arguments": { "path": canonical.to_str().unwrap() }
        }
    });

    // Forward mapping: host path → container path.
    let replacements = vec![(
        canonical.to_str().unwrap().to_string(),
        container_path.clone(),
    )];
    replace_in_strings(&mut call_msg, &replacements);

    let rewritten = call_msg["params"]["arguments"]["path"].as_str().unwrap();
    assert_eq!(
        rewritten, container_path,
        "host path in tools/call must be rewritten to container path"
    );
}

#[tokio::test]
async fn c2s_unrelated_args_not_modified() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let file = src.path().join("doc.pdf");
    std::fs::write(&file, b"data").unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();

    let cache = fresh_cache();
    let container_path = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();

    let mut call_msg = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": {
                "path": canonical.to_str().unwrap(),
                "query": "some search term",
                "limit": 10
            }
        }
    });

    let replacements = vec![(canonical.to_str().unwrap().to_string(), container_path)];
    replace_in_strings(&mut call_msg, &replacements);

    // Non-path args must be unchanged.
    assert_eq!(
        call_msg["params"]["arguments"]["query"].as_str().unwrap(),
        "some search term"
    );
    assert_eq!(call_msg["params"]["arguments"]["limit"], json!(10));
}

// ── simulated server→client (s2c) path rewriting ─────────────────────────────
//
// When the container sends a response containing a /workspace/…/file path, the
// pipeline should replace it with the original host path before forwarding to
// the client.

#[tokio::test]
async fn s2c_container_path_replaced_with_host_path() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let file = src.path().join("output.txt");
    std::fs::write(&file, b"result data").unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();

    let cache = fresh_cache();
    let container_path = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();

    // Simulate a tools/call response from the container (contains container path).
    let mut response = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "result": {
            "content": [{
                "type": "text",
                "text": format!("Processed file at {container_path}, done.")
            }],
            "isError": false
        }
    });

    // Reverse snapshot gives (container_path, host_path_string) pairs.
    let snapshot = cache.lock().reverse_snapshot();
    replace_in_strings(&mut response, &snapshot);

    let result_text = response["result"]["content"][0]["text"].as_str().unwrap();
    let host_path = canonical.to_str().unwrap();

    assert!(
        result_text.contains(host_path),
        "host path should appear in rewritten response: {result_text}"
    );
    assert!(
        !result_text.contains(&container_path),
        "container path should no longer appear in rewritten response: {result_text}"
    );
}

#[tokio::test]
async fn s2c_multiple_occurrences_all_replaced() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let file = src.path().join("chart.png");
    std::fs::write(&file, b"\x89PNG").unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();

    let cache = fresh_cache();
    let container_path = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();

    // The path appears twice in the response.
    let mut response = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "result": {
            "content": [
                {"type": "text", "text": format!("First ref: {container_path}")},
                {"type": "text", "text": format!("Second ref: {container_path}")},
            ]
        }
    });

    let snapshot = cache.lock().reverse_snapshot();
    replace_in_strings(&mut response, &snapshot);

    let host = canonical.to_str().unwrap();
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(host)
    );
    assert!(
        response["result"]["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains(host)
    );
}

// ── replace_in_strings: deep nested JSON ─────────────────────────────────────

#[test]
fn replace_in_strings_handles_deeply_nested_json() {
    let mut v = json!({
        "result": {
            "content": [
                {"type": "text",  "text": "/workspace/uuid-abc/report.pdf"},
                {"type": "text",  "text": "no paths here"},
            ],
            "metadata": {
                "source": "/workspace/uuid-abc/report.pdf",
                "size": 1024,
                "valid": true
            }
        }
    });

    let replacements = vec![(
        "/workspace/uuid-abc/report.pdf".to_string(),
        "/home/user/documents/report.pdf".to_string(),
    )];
    replace_in_strings(&mut v, &replacements);

    assert_eq!(
        v["result"]["content"][0]["text"].as_str().unwrap(),
        "/home/user/documents/report.pdf",
    );
    assert_eq!(
        v["result"]["content"][1]["text"].as_str().unwrap(),
        "no paths here",
        "unrelated strings must not be modified"
    );
    assert_eq!(
        v["result"]["metadata"]["source"].as_str().unwrap(),
        "/home/user/documents/report.pdf",
    );
    // Non-string values must be untouched.
    assert_eq!(v["result"]["metadata"]["size"], json!(1024));
    assert_eq!(v["result"]["metadata"]["valid"], json!(true));
}

#[test]
fn replace_in_strings_object_keys_not_modified() {
    let mut v = json!({ "/workspace/uuid/file.txt": "the value" });
    let replacements = vec![(
        "/workspace/uuid/file.txt".to_string(),
        "/host/path/file.txt".to_string(),
    )];
    replace_in_strings(&mut v, &replacements);

    // Key is unchanged; its value didn't match the pattern so also unchanged.
    assert!(
        v.get("/workspace/uuid/file.txt").is_some(),
        "object keys must not be modified"
    );
    assert_eq!(v["/workspace/uuid/file.txt"], json!("the value"));
}

// ── full round-trip: publish then rewrite both directions ─────────────────────

#[tokio::test]
async fn full_c2s_s2c_round_trip() {
    let src = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();

    let file = src.path().join("paper.pdf");
    std::fs::write(&file, b"%PDF-1.5 content").unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();
    let host_path_str = canonical.to_str().unwrap().to_string();

    let cache = fresh_cache();

    // ── c2s: publish and rewrite the outgoing tools/call ──────────────────
    let container_path = publish_file(&canonical, session.path(), &cache)
        .await
        .unwrap();

    let mut call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "read_pdf", "arguments": { "path": host_path_str } }
    });
    let forward = vec![(host_path_str.clone(), container_path.clone())];
    replace_in_strings(&mut call, &forward);

    assert_eq!(
        call["params"]["arguments"]["path"].as_str().unwrap(),
        container_path,
        "c2s: argument must contain container path after rewrite"
    );

    // ── s2c: rewrite the incoming response ────────────────────────────────
    let mut response = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {
            "content": [{"type": "text", "text": format!("Summary of {container_path}")}],
            "isError": false
        }
    });
    let reverse = cache.lock().reverse_snapshot();
    replace_in_strings(&mut response, &reverse);

    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains(&host_path_str),
        "s2c: response must contain host path after reverse rewrite: {text}"
    );
    assert!(
        !text.contains(&container_path),
        "s2c: container path must be gone after reverse rewrite: {text}"
    );
}
