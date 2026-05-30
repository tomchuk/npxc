//! Integration tests for NDJSON framing — `npxc::rpc::framing`.
//!
//! Uses `std::io::Cursor<Vec<u8>>` as an in-memory `AsyncRead` and `Vec<u8>`
//! as an in-memory `AsyncWrite` so no sockets or files are required.

use npxc::rpc::framing::{FrameReader, read_line, write_line, write_raw_line};
use serde_json::{Value, json};
use tokio::io::BufReader;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_reader(data: Vec<u8>) -> BufReader<std::io::Cursor<Vec<u8>>> {
    BufReader::new(std::io::Cursor::new(data))
}

// ── round-trip: single object ─────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_single_object() {
    let value = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}});

    let mut buf: Vec<u8> = Vec::new();
    write_line(&mut buf, &value).await.unwrap();

    let mut reader = make_reader(buf);
    let line = read_line(&mut reader).await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed, value);

    // Exactly one message — next read is EOF.
    assert!(read_line(&mut reader).await.is_none());
}

// ── round-trip: multiple messages ─────────────────────────────────────────────

#[tokio::test]
async fn round_trip_multi_message() {
    let messages = [
        json!({"id": 1, "method": "initialize"}),
        json!({"id": 2, "result": {"capabilities": {}}}),
        json!({"method": "notifications/progress", "params": {}}),
    ];

    let mut buf: Vec<u8> = Vec::new();
    for msg in &messages {
        write_line(&mut buf, msg).await.unwrap();
    }

    let mut reader = make_reader(buf);
    for expected in &messages {
        let line = read_line(&mut reader).await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(&parsed, expected);
    }

    // EOF after all messages are consumed.
    assert!(read_line(&mut reader).await.is_none());
}

// ── blank lines are skipped ───────────────────────────────────────────────────

#[tokio::test]
async fn blank_lines_are_skipped() {
    let value = json!({"id": 3, "method": "ping"});
    let json_str = serde_json::to_string(&value).unwrap();

    // Two leading blank lines, then the payload, then a trailing blank.
    let data = format!("\n\n{json_str}\n\n");

    let mut reader = make_reader(data.into_bytes());
    let line = read_line(&mut reader).await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed, value);

    // No second message (the trailing blank is skipped, then EOF).
    assert!(read_line(&mut reader).await.is_none());
}

// ── empty input returns None ──────────────────────────────────────────────────

#[tokio::test]
async fn empty_input_returns_none() {
    let mut reader = make_reader(Vec::new());
    assert!(read_line(&mut reader).await.is_none());
}

// ── large payload (~1 MB) round-trips correctly ───────────────────────────────

#[tokio::test]
async fn large_payload_round_trips() {
    let big_string = "x".repeat(1024 * 1024);
    let value = json!({"data": big_string, "id": 99});

    let mut buf: Vec<u8> = Vec::new();
    write_line(&mut buf, &value).await.unwrap();

    let mut reader = make_reader(buf);
    let line = read_line(&mut reader).await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&line).unwrap();

    assert_eq!(
        parsed["data"].as_str().unwrap().len(),
        1024 * 1024,
        "1 MB string should survive a framing round-trip"
    );
    assert_eq!(parsed["id"], 99);
}

// ── write_raw_line: pre-serialized string round-trips ─────────────────────────

#[tokio::test]
async fn write_raw_line_round_trips() {
    let raw = r#"{"id":100,"method":"custom/op","params":{"key":"value"}}"#;

    let mut buf: Vec<u8> = Vec::new();
    write_raw_line(&mut buf, raw).await.unwrap();

    let mut reader = make_reader(buf);
    let line = read_line(&mut reader).await.unwrap().unwrap();
    assert_eq!(line, raw);
}

// ── FrameReader: basic wrapping behaviour ─────────────────────────────────────

#[tokio::test]
async fn frame_reader_basic() {
    let value = json!({"id": 1, "method": "initialize"});

    let mut buf: Vec<u8> = Vec::new();
    write_line(&mut buf, &value).await.unwrap();

    let reader = BufReader::new(std::io::Cursor::new(buf));
    let mut frame_reader = FrameReader::new(reader);

    let line = frame_reader.next_line().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed, value);

    assert!(frame_reader.next_line().await.is_none(), "should be EOF");
}

// ── FrameReader: blank lines (including whitespace-only) are skipped ──────────

#[tokio::test]
async fn frame_reader_skips_blank_and_whitespace_lines() {
    let value = json!({"id": 2, "result": {}});
    let json_str = serde_json::to_string(&value).unwrap();

    // Mix of empty lines and a whitespace-only line before the payload.
    let data = format!("\n\n   \n{json_str}\n");

    let reader = BufReader::new(std::io::Cursor::new(data.into_bytes()));
    let mut frame_reader = FrameReader::new(reader);

    let line = frame_reader.next_line().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed, value);

    assert!(frame_reader.next_line().await.is_none(), "should be EOF");
}

// ── FrameReader: multi-message sequence ──────────────────────────────────────

#[tokio::test]
async fn frame_reader_multi_message() {
    let messages = [
        json!({"id": 10, "method": "tools/list"}),
        json!({"id": 11, "result": {"tools": []}}),
    ];

    let mut buf: Vec<u8> = Vec::new();
    for msg in &messages {
        write_line(&mut buf, msg).await.unwrap();
    }

    let reader = BufReader::new(std::io::Cursor::new(buf));
    let mut frame_reader = FrameReader::new(reader);

    for expected in &messages {
        let line = frame_reader.next_line().await.unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(&parsed, expected);
    }

    assert!(frame_reader.next_line().await.is_none());
}
