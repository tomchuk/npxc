//! Connection-time hostname extraction for egress policy.
//!
//! Before forwarding a guest TCP flow, the [`super::endpoint`] forwarder peeks
//! the first bytes to recover the destination **hostname** the policy filters
//! on:
//! - port 443 → the TLS **SNI** from the `ClientHello` ([`tls_sni`]);
//! - port 80 → the HTTP **`Host`** header ([`http_host`]).
//!
//! Both client-speaks-first, so the bytes are available immediately. The peeked
//! bytes are accumulated into a caller-owned `prefix` buffer and **replayed** to
//! the upstream socket once the flow is allowed, so nothing is lost. Peeking is
//! bounded by [`PEEK_TIMEOUT`] and [`PEEK_MAX`] so a non-conforming or
//! server-speaks-first protocol can't stall the flow — on timeout/cap the
//! hostname is simply unknown and the policy falls back to IP/port matching.
//!
//! The wire parsers ([`sni_from_client_hello`], [`host_from_http`]) are pure and
//! unit-tested against captured bytes; the async peek loop just feeds them.

use std::time::Duration;

use tls_parser::nom::Err as NomErr;
use tls_parser::{
    TlsExtension, TlsMessage, TlsMessageHandshake, parse_tls_client_hello_extensions,
    parse_tls_plaintext,
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::timeout;

/// Maximum time to wait for enough bytes to identify a hostname.
const PEEK_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum bytes to buffer while peeking. A `ClientHello` or HTTP header block
/// well under this; beyond it we give up and let IP/port matching decide.
const PEEK_MAX: usize = 8192;

/// Result of attempting to extract a hostname from the bytes seen so far.
#[derive(Debug, PartialEq, Eq)]
enum Peek {
    /// A hostname was found.
    Found(String),
    /// Parsed enough to know no hostname is present here.
    Absent,
    /// More bytes are needed to decide.
    Incomplete,
}

/// Peek the TLS SNI from a flow, accumulating consumed bytes into `prefix`.
///
/// Returns the lowercased server name, or `None` if it can't be determined
/// within the time/byte budget.
pub async fn tls_sni<R: AsyncRead + Unpin>(stream: &mut R, prefix: &mut Vec<u8>) -> Option<String> {
    peek_with(stream, prefix, sni_from_client_hello).await
}

/// Peek the HTTP `Host` header from a flow, accumulating consumed bytes into
/// `prefix`. Returns the lowercased host (without any `:port`), or `None`.
pub async fn http_host<R: AsyncRead + Unpin>(
    stream: &mut R,
    prefix: &mut Vec<u8>,
) -> Option<String> {
    peek_with(stream, prefix, host_from_http).await
}

/// Read from `stream` into `prefix` until `parse` resolves, the byte cap is hit,
/// or the read times out / ends.
async fn peek_with<R, F>(stream: &mut R, prefix: &mut Vec<u8>, parse: F) -> Option<String>
where
    R: AsyncRead + Unpin,
    F: Fn(&[u8]) -> Peek,
{
    let mut chunk = [0u8; 1024];
    loop {
        match parse(prefix) {
            Peek::Found(host) => return Some(host),
            Peek::Absent => return None,
            Peek::Incomplete => {}
        }
        if prefix.len() >= PEEK_MAX {
            return None;
        }
        match timeout(PEEK_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => prefix.extend_from_slice(&chunk[..n]),
            // EOF, read error, or timeout: give up; IP/port matching decides.
            _ => return None,
        }
    }
}

/// TLS record content type for a handshake message; a real `ClientHello`
/// always starts with this byte.
const TLS_HANDSHAKE_CONTENT_TYPE: u8 = 0x16;

/// Extract the SNI hostname from a buffer that should begin with a TLS
/// `ClientHello` record.
fn sni_from_client_hello(buf: &[u8]) -> Peek {
    // A TLS handshake record begins with 0x16; bail out fast on anything else
    // rather than buffering non-TLS traffic that happens to reach port 443.
    if buf
        .first()
        .is_some_and(|&b| b != TLS_HANDSHAKE_CONTENT_TYPE)
    {
        return Peek::Absent;
    }
    let record = match parse_tls_plaintext(buf) {
        Ok((_, record)) => record,
        Err(NomErr::Incomplete(_)) => return Peek::Incomplete,
        Err(_) => return Peek::Absent,
    };

    for msg in &record.msg {
        let TlsMessage::Handshake(TlsMessageHandshake::ClientHello(hello)) = msg else {
            continue;
        };
        let Some(ext) = hello.ext else {
            return Peek::Absent;
        };
        return match parse_tls_client_hello_extensions(ext) {
            Ok((_, extensions)) => extensions
                .iter()
                .find_map(sni_hostname)
                .map_or(Peek::Absent, Peek::Found),
            Err(NomErr::Incomplete(_)) => Peek::Incomplete,
            Err(_) => Peek::Absent,
        };
    }
    // A complete record that wasn't a ClientHello: not what we're looking for.
    Peek::Absent
}

/// Pull the first valid UTF-8 host name out of an SNI extension.
fn sni_hostname(ext: &TlsExtension) -> Option<String> {
    let TlsExtension::SNI(names) = ext else {
        return None;
    };
    names
        .iter()
        .find_map(|(_, raw)| std::str::from_utf8(raw).ok())
        .map(str::to_ascii_lowercase)
}

/// Extract the `Host` header value (host only, no port) from buffered HTTP
/// request bytes.
fn host_from_http(buf: &[u8]) -> Peek {
    let headers_end = find_subslice(buf, b"\r\n\r\n");

    // Only scan complete lines: up to the end of headers, or up to the last
    // newline seen so far, so a partially-received `Host:` line isn't misread.
    let region = match headers_end {
        Some(end) => &buf[..end],
        None => match buf.iter().rposition(|&b| b == b'\n') {
            Some(pos) => &buf[..pos],
            None => &[],
        },
    };

    for line in region.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() >= 5
            && line[..5].eq_ignore_ascii_case(b"host:")
            && let Some(host) = parse_host_value(&line[5..])
        {
            return Peek::Found(host);
        }
    }

    if headers_end.is_some() {
        Peek::Absent
    } else {
        Peek::Incomplete
    }
}

/// Normalize a `Host:` header value: trim whitespace, drop any `:port`, lowercase.
fn parse_host_value(raw: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(raw).ok()?.trim();
    if value.is_empty() {
        return None;
    }
    // Bracketed IPv6 literal: `[::1]` or `[::1]:8080`.
    let host = if let Some(rest) = value.strip_prefix('[') {
        rest.split_once(']').map_or(value, |(addr, _)| addr)
    } else if value.bytes().filter(|&b| b == b':').count() == 1 {
        // Exactly one colon: `host:port` → strip the port.
        value.rsplit_once(':').map_or(value, |(host, _)| host)
    } else {
        value
    };
    Some(host.to_ascii_lowercase())
}

/// Index of the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal TLS 1.2 `ClientHello` record carrying an SNI extension for
    /// `example.com`. Hand-assembled so the parser has something realistic.
    /// Lengths are tiny and fixed, so the `as` truncations can't actually lose
    /// data here.
    #[allow(clippy::cast_possible_truncation)]
    fn client_hello_with_sni(sni: &str) -> Vec<u8> {
        let sni = sni.as_bytes();

        // server_name extension body: list len, name type (0), name len, name.
        let mut sni_ext = Vec::new();
        sni_ext.extend_from_slice(&((sni.len() + 3) as u16).to_be_bytes()); // server_name_list len
        sni_ext.push(0); // host_name type
        sni_ext.extend_from_slice(&(sni.len() as u16).to_be_bytes());
        sni_ext.extend_from_slice(sni);

        // extension: type 0x0000 (server_name) + length + body.
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&0u16.to_be_bytes());
        extensions.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes());
        extensions.extend_from_slice(&sni_ext);

        // ClientHello body.
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // client_version TLS 1.2
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0); // session_id len
        body.extend_from_slice(&2u16.to_be_bytes()); // cipher_suites len
        body.extend_from_slice(&[0x00, 0x2f]); // one cipher suite
        body.push(1); // compression_methods len
        body.push(0); // null compression
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);

        // Handshake header: type 0x01 (ClientHello) + 3-byte length.
        let mut handshake = Vec::new();
        handshake.push(0x01);
        let blen = body.len();
        handshake.extend_from_slice(&[(blen >> 16) as u8, (blen >> 8) as u8, blen as u8]);
        handshake.extend_from_slice(&body);

        // TLS record header: type 0x16 (handshake), version, length.
        let mut record = Vec::new();
        record.push(0x16);
        record.extend_from_slice(&[0x03, 0x01]); // legacy record version
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    #[test]
    fn sni_parser_extracts_hostname() {
        let hello = client_hello_with_sni("Example.COM");
        assert_eq!(
            sni_from_client_hello(&hello),
            Peek::Found("example.com".to_string())
        );
    }

    #[test]
    fn sni_parser_reports_incomplete_on_partial_record() {
        let hello = client_hello_with_sni("example.com");
        // Truncate mid-record: the parser should ask for more, not fail.
        assert_eq!(sni_from_client_hello(&hello[..10]), Peek::Incomplete);
    }

    #[test]
    fn sni_parser_rejects_non_tls() {
        assert_eq!(
            sni_from_client_hello(b"GET / HTTP/1.1\r\n\r\n"),
            Peek::Absent
        );
    }

    #[tokio::test]
    async fn tls_sni_reads_from_stream_and_captures_prefix() {
        let hello = client_hello_with_sni("api.example.com");
        let mut stream = Cursor::new(hello.clone());
        let mut prefix = Vec::new();
        let sni = tls_sni(&mut stream, &mut prefix).await;
        assert_eq!(sni.as_deref(), Some("api.example.com"));
        // The bytes consumed while peeking are preserved for replay upstream.
        assert_eq!(prefix, hello);
    }

    #[test]
    fn http_host_parser_extracts_host() {
        let req = b"GET /path HTTP/1.1\r\nHost: Example.com\r\nAccept: */*\r\n\r\n";
        assert_eq!(host_from_http(req), Peek::Found("example.com".to_string()));
    }

    #[test]
    fn http_host_parser_strips_port() {
        let req = b"GET / HTTP/1.1\r\nHost: example.com:8443\r\n\r\n";
        assert_eq!(host_from_http(req), Peek::Found("example.com".to_string()));
    }

    #[test]
    fn http_host_parser_incomplete_without_terminator() {
        // Host line not yet fully received (no trailing CRLF after it).
        let req = b"GET / HTTP/1.1\r\nHos";
        assert_eq!(host_from_http(req), Peek::Incomplete);
    }

    #[test]
    fn http_host_parser_absent_when_headers_end_without_host() {
        let req = b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n";
        assert_eq!(host_from_http(req), Peek::Absent);
    }

    #[tokio::test]
    async fn http_host_reads_from_stream() {
        let req = b"GET / HTTP/1.1\r\nHost: svc.internal\r\n\r\n".to_vec();
        let mut stream = Cursor::new(req.clone());
        let mut prefix = Vec::new();
        let host = http_host(&mut stream, &mut prefix).await;
        assert_eq!(host.as_deref(), Some("svc.internal"));
        assert_eq!(prefix, req);
    }
}
