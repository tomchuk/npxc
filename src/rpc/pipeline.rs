//! Bidirectional JSON-RPC pipeline that proxies MCP messages between the
//! host client (process stdio) and the sandboxed container (`ChildStdin` /
//! `ChildStdout`).
//!
//! ## Architecture
//!
//! ```text
//! process stdin  → client_to_server_task ──────────────→ container stdin
//!                          │ (error responses)
//!                          ↓
//!                   stdout_channel          container stdout → server_to_client_task
//!                          ↑                                        │ (rewritten)
//!                          └────────────── stdout_channel ←────────┘
//!                                                  │
//!                                          stdout_writer_task → process stdout
//!
//! container stderr ─────────────────── stderr_forwarder_task → process stderr
//! ```
//!
//! ## Shutdown
//!
//! A `watch::Sender<bool>` (wrapped in `Arc`) is shared between both pipeline
//! tasks. When either task exits — whether by EOF on its source or by an I/O
//! error — it sends `true` on the watch channel. Each task polls
//! `shutdown_rx.changed()` in its main `select!` loop and breaks when `true`
//! is observed. The `stdout_writer_task` exits naturally when both pipeline
//! tasks drop their `mpsc::Sender` copies.

use std::{path::PathBuf, sync::Arc};

use tokio::{
    io::BufReader,
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, watch},
};
use tracing::warn;

use crate::{
    config::EffectiveConfig,
    error::NpxcError,
    paths::{SessionState, identify_path_args, publish_file, validate_path},
    rpc::{
        framing,
        message::{
            MessageKind, extract_resources_read_uri, extract_tool_schemas, extract_tools_call,
            message_id, message_kind, replace_in_strings,
        },
    },
    runtime::{ContainerProcess, Session},
};

/// Immutable, shareable context passed to both pipeline tasks.
struct PipelineCtx {
    state: Arc<SessionState>,
    /// Host workspace directory mounted into the container as `/workspace`.
    session_dir: PathBuf,
    /// CWD scope against which published paths are validated.
    cwd: PathBuf,
    config: EffectiveConfig,
    /// When `true`, path translation is bypassed entirely.
    no_isolate: bool,
}

/// Whether a (possibly rewritten) message should be forwarded to its peer or
/// dropped (e.g. because an error response was already sent to the client).
enum Disposition {
    Forward,
    Drop,
}

/// Run the bidirectional JSON-RPC pipeline until either side closes.
///
/// Returns `Ok(())` on normal shutdown (client closed stdin or container
/// exited cleanly).
///
/// # Errors
///
/// Returns [`NpxcError::Runtime`] if the container process handles have already
/// been taken from `session` (i.e. the pipeline was started twice).
pub async fn run_pipeline(
    session: &mut Session,
    cwd: &std::path::Path,
    config: &EffectiveConfig,
    no_isolate: bool,
) -> Result<(), NpxcError> {
    // Take ownership of the container's I/O handles.
    let ContainerProcess {
        mut child,
        stdin: container_stdin,
        stdout: container_stdout,
        stderr: container_stderr,
    } = session
        .take_container()
        .ok_or_else(|| NpxcError::Runtime("container process already taken".into()))?;

    let ctx = Arc::new(PipelineCtx {
        state: Arc::clone(&session.state),
        session_dir: session.session_dir.clone(),
        cwd: cwd.to_path_buf(),
        config: config.clone(),
        no_isolate,
    });

    // ── stdout channel ──────────────────────────────────────────────────────
    // Both pipeline tasks write serialised JSON lines here; a dedicated writer
    // task drains the channel and writes to process stdout.  This prevents
    // interleaving and lets the c2s task inject JSON-RPC error responses back
    // to the client.
    let (stdout_tx, mut stdout_rx) = mpsc::channel::<String>(64);
    let c2s_stdout_tx = stdout_tx.clone();
    let s2c_stdout_tx = stdout_tx.clone();
    // Drop the original; the channel closes when both task copies are dropped.
    drop(stdout_tx);

    // ── shutdown watch channel ──────────────────────────────────────────────
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);

    // ── stdout_writer_task ──────────────────────────────────────────────────
    let stdout_writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = stdout_rx.recv().await {
            if let Err(e) = framing::write_raw_line(&mut out, &line).await {
                warn!("stdout write error: {e}");
                break;
            }
        }
    });

    // ── stderr_forwarder_task ───────────────────────────────────────────────
    // Byte-copy; the container may emit plain text diagnostics here.
    let stderr_forwarder = tokio::spawn(async move {
        let mut src = container_stderr;
        let mut dst = tokio::io::stderr();
        if let Err(e) = tokio::io::copy(&mut src, &mut dst).await {
            warn!("stderr forwarder error: {e}");
        }
    });

    // ── pipeline tasks ──────────────────────────────────────────────────────
    let c2s = tokio::spawn(client_to_server(
        Arc::clone(&ctx),
        container_stdin,
        c2s_stdout_tx,
        Arc::clone(&shutdown_tx),
        shutdown_rx.clone(),
    ));
    let s2c = tokio::spawn(server_to_client(
        Arc::clone(&ctx),
        container_stdout,
        s2c_stdout_tx,
        Arc::clone(&shutdown_tx),
        shutdown_rx.clone(),
    ));

    // Drop the Arc so the watch channel closes when both task copies drop.
    drop(shutdown_tx);

    // Wait for both pipeline tasks.  Their stdout_tx clones are dropped as they
    // exit, which will eventually drain the channel and stop the writer.
    let _ = tokio::join!(c2s, s2c);

    // Writer exits when the channel is empty and all senders are dropped.
    // Forwarder exits when the container closes its stderr (already exited).
    let _ = tokio::join!(stdout_writer, stderr_forwarder);

    // Reap the container child to avoid zombies.
    let _ = child.start_kill();
    let _ = child.wait().await;

    Ok(())
}

/// Client → server task: read JSON-RPC messages from process stdin, translate
/// any host paths into container `/workspace` paths, and forward to the
/// container's stdin.
async fn client_to_server(
    ctx: Arc<PipelineCtx>,
    mut container_stdin: ChildStdin,
    stdout_tx: mpsc::Sender<String>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut stdin_reader = BufReader::new(tokio::io::stdin());

    'msg: loop {
        // Fast-path: respect a shutdown already in effect.
        if *shutdown_rx.borrow() {
            break;
        }

        tokio::select! {
            biased;

            line_opt = framing::read_line(&mut stdin_reader) => {
                let raw = match line_opt {
                    None => break,                                      // EOF on client stdin
                    Some(Err(e)) => { warn!("stdin read error: {e}"); break; }
                    Some(Ok(l)) => l,
                };

                let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    warn!("unparseable JSON from client, forwarding raw");
                    if framing::write_raw_line(&mut container_stdin, &raw).await.is_err() {
                        break;
                    }
                    continue 'msg;
                };

                // Translate path arguments unless isolation is disabled.
                let disposition = if ctx.no_isolate {
                    Disposition::Forward
                } else {
                    match message_kind(&value) {
                        MessageKind::Request { method } if method == "tools/call" => {
                            translate_tools_call(&ctx, &mut value, &stdout_tx).await
                        }
                        MessageKind::Request { method } if method == "resources/read" => {
                            translate_resources_read(&ctx, &mut value, &stdout_tx).await
                        }
                        _ => Disposition::Forward,
                    }
                };

                if matches!(disposition, Disposition::Forward)
                    && framing::write_line(&mut container_stdin, &value).await.is_err()
                {
                    break;
                }
            }

            // Yield on shutdown signal (sender dropped → is_err; true → break).
            result = shutdown_rx.changed() => {
                if result.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    // Signal the s2c task.
    let _ = shutdown_tx.send(true);
    // `container_stdin` is dropped here → container receives EOF on its stdin.
}

/// Translate host paths found in a `tools/call` request into container paths,
/// rewriting the message in place. On validation failure an error response is
/// queued to the client and the message is dropped.
async fn translate_tools_call(
    ctx: &PipelineCtx,
    value: &mut serde_json::Value,
    stdout_tx: &mpsc::Sender<String>,
) -> Disposition {
    let Some((tn, args)) = extract_tools_call(value) else {
        return Disposition::Forward;
    };
    let tool_name = tn.to_owned();
    let arguments = args.clone();

    // Snapshot schemas; never hold the lock across an await.
    let schemas = { ctx.state.tool_schemas.lock().clone() };

    // Returns (json_ptr_relative_to_arguments, raw_value), e.g. ("/path", ...)
    // or ("/sources/0/path", ...) for nested values.
    let path_args = identify_path_args(&tool_name, &arguments, &ctx.config, &schemas);
    tracing::debug!(
        tool = %tool_name,
        count = path_args.len(),
        args = ?path_args,
        "paths identified"
    );

    for (arg_ptr, raw_path) in path_args {
        let canonical = match validate_path(&raw_path, &ctx.cwd) {
            Ok(p) => p,
            Err(e) => {
                if let Some(err_json) = e.to_rpc_error_response(message_id(value)) {
                    let _ = stdout_tx.send(err_json).await;
                } else {
                    warn!("path validation error for {raw_path:?}: {e}");
                }
                return Disposition::Drop;
            }
        };

        let container_path =
            match publish_file(&canonical, &ctx.session_dir, &ctx.state.publications).await {
                Ok(p) => p,
                Err(e) => {
                    warn!("publish_file failed for {}: {e}", canonical.display());
                    return Disposition::Drop;
                }
            };

        // Rewrite using the JSON pointer so nested values are reached.
        let full_ptr = format!("/params/arguments{arg_ptr}");
        if let Some(v) = value.pointer_mut(&full_ptr) {
            *v = serde_json::Value::String(container_path);
        }
    }

    Disposition::Forward
}

/// Translate a `file://` host URI in a `resources/read` request into a
/// container URI, rewriting the message in place. On validation failure an
/// error response is queued to the client and the message is dropped.
async fn translate_resources_read(
    ctx: &PipelineCtx,
    value: &mut serde_json::Value,
    stdout_tx: &mpsc::Sender<String>,
) -> Disposition {
    // Copy the URI string before mutating `value`.
    let Some(uri) = extract_resources_read_uri(value).map(str::to_owned) else {
        return Disposition::Forward;
    };
    if !uri.starts_with("file://") {
        return Disposition::Forward;
    }

    // validate_path strips the file:// prefix internally.
    let canonical = match validate_path(&uri, &ctx.cwd) {
        Ok(p) => p,
        Err(e) => {
            if let Some(err_json) = e.to_rpc_error_response(message_id(value)) {
                let _ = stdout_tx.send(err_json).await;
            } else {
                warn!("URI validation error for {uri:?}: {e}");
            }
            return Disposition::Drop;
        }
    };

    let container_path =
        match publish_file(&canonical, &ctx.session_dir, &ctx.state.publications).await {
            Ok(p) => p,
            Err(e) => {
                warn!("publish_file failed for URI {uri:?}: {e}");
                return Disposition::Drop;
            }
        };

    // Rewrite the URI: file:// + container path.
    let new_uri = format!("file://{container_path}");
    if let Some(params) = value.get_mut("params") {
        params["uri"] = serde_json::Value::String(new_uri);
    }

    Disposition::Forward
}

/// Server → client task: read JSON-RPC messages from the container's stdout,
/// cache tool schemas, reverse-translate container paths back to host paths,
/// and forward to process stdout.
async fn server_to_client(
    ctx: Arc<PipelineCtx>,
    container_stdout: ChildStdout,
    stdout_tx: mpsc::Sender<String>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut stdout_reader = BufReader::new(container_stdout);

    'msg: loop {
        if *shutdown_rx.borrow() {
            break;
        }

        tokio::select! {
            biased;

            line_opt = framing::read_line(&mut stdout_reader) => {
                let raw = match line_opt {
                    None => break,                                     // container exited
                    Some(Err(e)) => { warn!("container stdout read error: {e}"); break; }
                    Some(Ok(l)) => l,
                };

                let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    warn!("unparseable JSON from container stdout, forwarding raw");
                    if stdout_tx.send(raw).await.is_err() {
                        break;
                    }
                    continue 'msg;
                };

                // Cache tool schemas from tools/list responses.
                if matches!(message_kind(&value), MessageKind::Response) {
                    let schemas = extract_tool_schemas(&value);
                    if !schemas.is_empty() {
                        // Lock, insert, unlock — no await inside.
                        let mut g = ctx.state.tool_schemas.lock();
                        for s in schemas {
                            g.insert(s.name.clone(), s);
                        }
                    }
                }

                // Reverse path translation: snapshot under the lock, release
                // before any await, then rewrite container paths → host paths.
                // Skip the whole-tree walk when nothing has been published yet.
                let snapshot = { ctx.state.publications.lock().reverse_snapshot() };
                if !snapshot.is_empty() {
                    replace_in_strings(&mut value, &snapshot);
                }

                let serialized = match serde_json::to_string(&value) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("failed to re-serialize server message: {e}");
                        continue 'msg;
                    }
                };

                if stdout_tx.send(serialized).await.is_err() {
                    break;
                }
            }

            result = shutdown_rx.changed() => {
                if result.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    // Signal the c2s task.
    let _ = shutdown_tx.send(true);
}
