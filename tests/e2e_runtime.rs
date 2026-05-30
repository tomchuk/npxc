//! Real-runtime end-to-end checks for the security primitives `npxc` relies on.
//!
//! These assert that Apple `container` actually honours the flags `npxc`
//! passes — read-only volume mounts, a read-only root filesystem, and
//! `--network none` isolation. They shell out to the `container` CLI and
//! therefore require:
//!
//! - macOS on Apple Silicon,
//! - the `container` CLI on `PATH` (override with `NPXC_CONTAINER_CLI`),
//! - a started system service (`container system start`, or run `npxc doctor`),
//! - network access to pull the `node:lts-slim` base image once.
//!
//! They are gated behind the `e2e` feature so plain `cargo test` skips them:
//!
//! ```sh
//! cargo test --features e2e --test e2e_runtime
//! ```

#![cfg(feature = "e2e")]

use std::process::Command;

/// Image used for the runtime probes (already present after any `npxc` build).
const IMAGE: &str = "node:lts-slim";

/// Resolve the container CLI, honouring `NPXC_CONTAINER_CLI`.
fn container_cli() -> String {
    std::env::var("NPXC_CONTAINER_CLI").unwrap_or_else(|_| "container".to_string())
}

/// Run `container <args...>` and return `(success, stdout, stderr)`.
///
/// Returns `None` when the CLI is not installed, so the suite can be skipped
/// with a clear message rather than hard-failing on machines without the
/// runtime.
fn run(args: &[&str]) -> Option<(bool, String, String)> {
    let cli = container_cli();
    let output = match Command::new(&cli).args(args).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping e2e: cannot run `{cli}`: {e}");
            return None;
        }
    };
    Some((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

#[test]
fn read_only_volume_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, b"original").unwrap();

    let mount = format!("{}:/mnt:ro", dir.path().display());
    let Some((_ok, _out, _err)) = run(&[
        "run",
        "--rm",
        "-v",
        &mount,
        IMAGE,
        "sh",
        "-c",
        "echo HACKED > /mnt/f 2>/dev/null || true",
    ]) else {
        return; // CLI unavailable — skip.
    };

    // The host original must be untouched: a read-only bind mount must reject
    // the write rather than letting it pass through to the host file.
    let after = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        after, "original",
        "read-only `-v …:ro` mount was not enforced; the container modified the host file"
    );
}

#[test]
fn read_only_rootfs_is_enforced() {
    let Some((_ok, out, _err)) = run(&[
        "run",
        "--rm",
        "--read-only",
        "--tmpfs",
        "/tmp",
        IMAGE,
        "sh",
        "-c",
        "echo x > /nope 2>/dev/null && echo WRITABLE || echo READONLY",
    ]) else {
        return;
    };

    assert!(
        out.contains("READONLY"),
        "`--read-only` root filesystem was not enforced; got: {out:?}"
    );
}

#[test]
fn network_none_is_isolated() {
    // Attempt a raw TCP connect to a public IP (no DNS needed). With
    // `--network none` this must fail.
    let probe = "const s=require('net').connect(80,'1.1.1.1');\
                 s.setTimeout(3000);\
                 s.on('connect',()=>{console.log('NETWORK_UP');process.exit()});\
                 s.on('error',e=>{console.log('NETWORK_ISOLATED',e.code);process.exit()});\
                 s.on('timeout',()=>{console.log('NETWORK_ISOLATED_TIMEOUT');process.exit()});";

    let Some((_ok, out, _err)) = run(&[
        "run",
        "--rm",
        "--network",
        "none",
        IMAGE,
        "node",
        "-e",
        probe,
    ]) else {
        return;
    };

    assert!(
        out.contains("NETWORK_ISOLATED"),
        "`--network none` did not isolate the container; got: {out:?}"
    );
}
