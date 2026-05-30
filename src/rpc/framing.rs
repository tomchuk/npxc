//! MCP stdio transport framing: newline-delimited JSON.
//!
//! One JSON object per line, `\n`-terminated. Lines can be large (multi-MB
//! payloads); the default buffer growth of `tokio::io::BufReader` and `Lines`
//! is used — no fixed-size cap.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Read one non-blank line from an async buffered reader.
///
/// Skips blank lines. Returns `None` at EOF, `Some(Err(_))` on I/O error, or
/// `Some(Ok(line))` on success. The returned string has its trailing `\r\n` or
/// `\n` stripped.
pub async fn read_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Option<Result<String, std::io::Error>> {
    loop {
        let mut buf = String::new();
        match reader.read_line(&mut buf).await {
            Err(e) => return Some(Err(e)),
            Ok(0) => return None, // EOF
            Ok(_) => {
                let trimmed = buf.trim_end_matches(['\n', '\r']);
                if !trimmed.is_empty() {
                    return Some(Ok(trimmed.to_owned()));
                }
                // blank line — keep reading
            }
        }
    }
}

/// Write a JSON value as a newline-terminated line to an async writer.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if `value` cannot be serialized (kind
/// [`InvalidData`](std::io::ErrorKind::InvalidData)) or if the underlying
/// write fails.
pub async fn write_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    value: &serde_json::Value,
) -> Result<(), std::io::Error> {
    let serialized = serde_json::to_string(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_raw_line(writer, &serialized).await
}

/// Write a raw string (already serialized JSON) followed by a single `\n`.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if the underlying write fails.
pub async fn write_raw_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    line: &str,
) -> Result<(), std::io::Error> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await
}

// ---------------------------------------------------------------------------
// FrameReader — convenience wrapper around `Lines<BufReader<R>>`
// ---------------------------------------------------------------------------

/// Convenience wrapper that owns a `BufReader<R>` and exposes line-oriented
/// MCP message reading via `AsyncBufReadExt::lines()`.
///
/// Prefer [`FrameReader`] when the reader is owned; use the bare [`read_line`]
/// function when you need to share a `&mut BufReader`.
pub struct FrameReader<R> {
    inner: tokio::io::Lines<BufReader<R>>,
}

impl<R: tokio::io::AsyncRead + Unpin> FrameReader<R> {
    /// Wrap a `BufReader<R>` for line-oriented reading.
    ///
    /// The `BufReader` is consumed and its internal buffer is reused by the
    /// `Lines` adapter.
    pub fn new(reader: BufReader<R>) -> Self {
        Self {
            inner: reader.lines(),
        }
    }

    /// Read the next non-blank line.
    ///
    /// Returns `None` at EOF, `Some(Err(_))` on I/O error, or `Some(Ok(line))`
    /// on success. The line does **not** include a trailing newline.
    pub async fn next_line(&mut self) -> Option<Result<String, std::io::Error>> {
        loop {
            match self.inner.next_line().await {
                Err(e) => return Some(Err(e)),
                Ok(None) => return None,
                Ok(Some(line)) => {
                    if !line.trim().is_empty() {
                        return Some(Ok(line));
                    }
                    // blank line — keep reading
                }
            }
        }
    }
}
