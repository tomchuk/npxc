//! Live integration probe for an MCP server running under npxc.
//!
//! Spawns `npxc @sylphx/pdf-reader-mcp`, runs three scenarios in sequence,
//! and pretty-prints every JSON-RPC response it receives.
//!
//! Usage:
//!   cargo run --release --example `mcp_probe`
//!   cargo run --release --example `mcp_probe` /path/to/file.pdf
//!
//! Defaults to `examples/sample.pdf` relative to the current working directory.
//!
//! Scenarios:
//!   1. Probe       — initialize + tools/list
//!   2. Read PDF    — tools/call with the given file (must be under CWD)
//!   3. Scope test  — tools/call with /etc/passwd → expect -32602 error

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::sleep;

const PACKAGE: &str = "@sylphx/pdf-reader-mcp";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pdf = std::env::args().nth(1).map_or_else(
        || std::env::current_dir().unwrap().join("examples/sample.pdf"),
        PathBuf::from,
    );
    let pdf = pdf
        .canonicalize()
        .expect("PDF not found — pass a path as an argument or place a PDF at examples/sample.pdf");

    println!("PDF under test: {}", pdf.display());

    // ── Scenario 1: probe ────────────────────────────────────────────────────
    run_scenario(
        "PROBE — initialize + tools/list",
        PACKAGE,
        &[
            init_msg(1),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#.into(),
        ],
        Duration::from_secs(5),
    )
    .await?;

    // ── Scenario 2: read PDF ─────────────────────────────────────────────────
    run_scenario(
        "READ PDF — happy path",
        PACKAGE,
        &[
            init_msg(1),
            serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "read_pdf",
                    "arguments": {
                        "sources": [{ "path": pdf }],
                        "include_metadata": true,
                        "include_page_count": true,
                        "include_full_text": true
                    }
                }
            }))?,
        ],
        Duration::from_secs(20),
    )
    .await?;

    // ── Scenario 3: path outside CWD ────────────────────────────────────────
    run_scenario(
        "OUT-OF-SCOPE PATH — expect error -32602",
        PACKAGE,
        &[
            init_msg(1),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_pdf","arguments":{"sources":[{"path":"/etc/passwd"}]}}}"#.into(),
        ],
        Duration::from_secs(5),
    )
    .await?;

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn init_msg(id: u32) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "mcp-probe", "version": "0.1.0" }
        }
    })
    .to_string()
}

async fn run_scenario(
    title: &str,
    package: &str,
    messages: &[String],
    wait: Duration,
) -> anyhow::Result<()> {
    let bar = "═".repeat(title.len() + 4);
    println!("\n╔{bar}╗");
    println!("║   {title}   ║");
    println!("╚{bar}╝\n");

    let npxc = find_npxc();
    let mut child = Command::new(&npxc)
        .arg(package)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // Spawn a task that reads every response line and pretty-prints it.
    let printer = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or(line)),
                Err(_) => println!("{line}"),
            }
        }
    });

    // Send each message with a short pause between them.
    for msg in messages {
        stdin.write_all(msg.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        sleep(Duration::from_millis(200)).await;
    }

    // Hold stdin open so the server has time to respond, then close it.
    sleep(wait).await;
    drop(stdin); // EOF → npxc shuts down gracefully

    printer.await?;
    let _ = child.wait().await;

    Ok(())
}

/// Locate the npxc binary relative to this example's executable.
///
/// When running via `cargo run --example`, the example binary is placed in
/// `target/{profile}/examples/mcp_probe` and the main binary is one level up
/// at `target/{profile}/npxc`.
fn find_npxc() -> PathBuf {
    // current_exe() → target/{profile}/examples/mcp_probe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(examples_dir) = exe.parent() {
            if let Some(profile_dir) = examples_dir.parent() {
                let candidate = profile_dir.join("npxc");
                if candidate.exists() {
                    return candidate;
                }
            }
        }
    }

    // Fallback: look in the standard build directories relative to CWD.
    for profile in ["release", "debug"] {
        let p = PathBuf::from("target").join(profile).join("npxc");
        if p.exists() {
            return p;
        }
    }

    panic!(
        "npxc binary not found — run `cargo build --release` before `cargo run --example mcp_probe`"
    );
}
