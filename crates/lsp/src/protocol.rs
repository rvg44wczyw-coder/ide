use crate::error::LspError;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Content-Length above this is rejected before any body buffer is
/// allocated — a malicious or buggy server can't force an unbounded
/// allocation. Generous for any legitimate `publishDiagnostics`/response
/// payload from `rust-analyzer` (see
/// `docs/features/rust-language-support.md` §4).
pub const MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;

/// Total header bytes (across every header line of a single message)
/// above this is rejected — without this, `MAX_CONTENT_LENGTH` only
/// bounds the declared *body* size; a malicious or buggy server could
/// still force unbounded allocation via a single giant header line (or
/// unboundedly many small ones) before any `Content-Length` is ever
/// parsed. Generous for LSP's small, fixed header set.
const MAX_HEADER_BYTES: usize = 64 * 1024;

pub enum ReadOutcome {
    Message(Vec<u8>),
    /// Stream closed cleanly at a message boundary (no header bytes read
    /// yet for this message) — a normal process exit, not malformed
    /// input.
    Eof,
    /// A frame that doesn't conform to LSP's `Content-Length`-prefixed
    /// framing: bad/missing header, oversized length, or the stream
    /// closing mid-message. Always fatal to the connection — see
    /// `docs/features/rust-language-support.md` §3.
    Error(LspError),
}

/// Reads one `Content-Length`-framed LSP message. Never allocates more
/// than `MAX_CONTENT_LENGTH` bytes for a claimed body, and never panics
/// on malformed input — treats the subprocess's output as fully
/// untrusted.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> ReadOutcome {
    let mut content_length: Option<usize> = None;
    let mut headers_started = false;

    // Bounded to `MAX_HEADER_BYTES` total: without this, a server that
    // never sends a line-terminating `\n` (or sends unboundedly many
    // small header lines) could grow `line` without limit before
    // `MAX_CONTENT_LENGTH` ever comes into play. Once exhausted, further
    // reads report 0 bytes, which the existing EOF/"mid-headers" handling
    // below already turns into a fatal `Protocol` error.
    let mut limited = tokio::io::AsyncReadExt::take(reader, MAX_HEADER_BYTES as u64);

    loop {
        let mut line = String::new();
        let n = match limited.read_line(&mut line).await {
            Ok(n) => n,
            Err(e) => {
                return if headers_started {
                    ReadOutcome::Error(LspError::Protocol(format!("io error reading headers: {e}")))
                } else {
                    ReadOutcome::Eof
                };
            }
        };
        if n == 0 {
            return if headers_started {
                ReadOutcome::Error(LspError::Protocol(
                    "connection closed mid-headers".to_string(),
                ))
            } else {
                ReadOutcome::Eof
            };
        }
        headers_started = true;

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            if content_length.is_some() {
                return ReadOutcome::Error(LspError::Protocol(
                    "duplicate Content-Length header".to_string(),
                ));
            }
            let value = value.trim();
            let len: usize = match value.parse() {
                Ok(len) => len,
                Err(_) => {
                    return ReadOutcome::Error(LspError::Protocol(format!(
                        "invalid Content-Length header: {value:?}"
                    )));
                }
            };
            if len > MAX_CONTENT_LENGTH {
                return ReadOutcome::Error(LspError::Protocol(format!(
                    "Content-Length {len} exceeds cap of {MAX_CONTENT_LENGTH}"
                )));
            }
            content_length = Some(len);
        }
        // Other headers (e.g. Content-Type) are valid LSP framing and
        // simply ignored — v1 only needs the body length.
    }

    let Some(content_length) = content_length else {
        return ReadOutcome::Error(LspError::Protocol(
            "message headers missing Content-Length".to_string(),
        ));
    };

    // Headers are done -- read the (separately, more generously capped)
    // body straight from the underlying reader, not the header-bounded
    // `limited` wrapper. `Take` only limits/counts bytes; it shares the
    // inner reader's actual buffer, so this picks up exactly where the
    // header loop left off.
    let reader = limited.into_inner();
    let mut body = vec![0u8; content_length];
    if let Err(e) = tokio::io::AsyncReadExt::read_exact(reader, &mut body).await {
        return ReadOutcome::Error(LspError::Protocol(format!(
            "connection closed while reading body: {e}"
        )));
    }
    ReadOutcome::Message(body)
}

/// Writes one `Content-Length`-framed LSP message.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

pub fn encode_request(id: u64, method: &str, params: impl Serialize) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("serializing well-typed LSP params cannot fail")
}

pub fn encode_notification(method: &str, params: impl Serialize) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
    .expect("serializing well-typed LSP params cannot fail")
}

/// Answers a server-initiated *request* (`id` is whatever the server sent
/// -- number, string, or (per spec, though no server does this in
/// practice) null; echoed back verbatim, not reinterpreted). The first
/// time this client has ever needed to reply to the server rather than
/// the other way around (see `docs/features/code-actions.md` §3.5).
pub fn encode_response(id: Value, result: impl Serialize) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
    .expect("serializing well-typed LSP result cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    async fn read_from_bytes(bytes: &[u8]) -> ReadOutcome {
        let mut reader = BufReader::new(bytes);
        read_message(&mut reader).await
    }

    #[tokio::test]
    async fn reads_a_well_formed_message() {
        let body = br#"{"jsonrpc":"2.0","method":"ping","params":{}}"#;
        let frame = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = frame.into_bytes();
        bytes.extend_from_slice(body);

        match read_from_bytes(&bytes).await {
            ReadOutcome::Message(got) => assert_eq!(got, body),
            _ => panic!("expected a message"),
        }
    }

    #[tokio::test]
    async fn ignores_extra_headers() {
        let body = b"{}";
        let frame = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut bytes = frame.into_bytes();
        bytes.extend_from_slice(body);

        match read_from_bytes(&bytes).await {
            ReadOutcome::Message(got) => assert_eq!(got, body),
            _ => panic!("expected a message"),
        }
    }

    #[tokio::test]
    async fn clean_eof_before_any_bytes_is_not_an_error() {
        match read_from_bytes(b"").await {
            ReadOutcome::Eof => {}
            _ => panic!("expected clean EOF"),
        }
    }

    #[tokio::test]
    async fn eof_mid_headers_is_a_protocol_error() {
        match read_from_bytes(b"Content-Length: 10\r\n").await {
            ReadOutcome::Error(LspError::Protocol(_)) => {}
            _ => panic!("expected a protocol error for EOF mid-headers"),
        }
    }

    #[tokio::test]
    async fn truncated_body_is_a_protocol_error() {
        let frame = b"Content-Length: 100\r\n\r\ntoo short";
        match read_from_bytes(frame).await {
            ReadOutcome::Error(LspError::Protocol(_)) => {}
            _ => panic!("expected a protocol error for truncated body"),
        }
    }

    #[tokio::test]
    async fn duplicate_content_length_header_is_rejected() {
        // A second `Content-Length` header must not silently overwrite the
        // first (last-wins) -- treated as malformed framing instead.
        let frame = b"Content-Length: 2\r\nContent-Length: 999999\r\n\r\n{}";
        match read_from_bytes(frame).await {
            ReadOutcome::Error(LspError::Protocol(msg)) => {
                assert!(msg.contains("duplicate"), "message: {msg}");
            }
            _ => panic!("expected a protocol error for duplicate Content-Length headers"),
        }
    }

    #[tokio::test]
    async fn non_numeric_content_length_is_a_protocol_error() {
        let frame = b"Content-Length: not-a-number\r\n\r\n";
        match read_from_bytes(frame).await {
            ReadOutcome::Error(LspError::Protocol(_)) => {}
            _ => panic!("expected a protocol error"),
        }
    }

    #[tokio::test]
    async fn missing_content_length_is_a_protocol_error() {
        let frame = b"Content-Type: application/json\r\n\r\n";
        match read_from_bytes(frame).await {
            ReadOutcome::Error(LspError::Protocol(_)) => {}
            _ => panic!("expected a protocol error"),
        }
    }

    #[tokio::test]
    async fn header_line_without_a_newline_is_rejected_not_grown_unbounded() {
        // No `\r\n` anywhere -- without a header-byte cap, `read_line`
        // would keep growing its buffer waiting for a terminator that
        // never comes. `MAX_HEADER_BYTES` must cut this off well before
        // that, turning it into a fatal protocol error instead.
        let junk = vec![b'a'; MAX_HEADER_BYTES + 1];
        match read_from_bytes(&junk).await {
            ReadOutcome::Error(LspError::Protocol(_)) => {}
            _ => panic!("expected an unterminated oversized header line to be rejected"),
        }
    }

    #[tokio::test]
    async fn many_small_headers_exceeding_total_budget_is_rejected() {
        // Each line is short and individually well-formed; only the
        // running total across many lines exceeds `MAX_HEADER_BYTES`.
        let mut bytes = Vec::new();
        for _ in 0..(MAX_HEADER_BYTES / 16 + 1) {
            bytes.extend_from_slice(b"X-Junk: filler\r\n");
        }
        match read_from_bytes(&bytes).await {
            ReadOutcome::Error(LspError::Protocol(_)) => {}
            _ => panic!("expected the header total to be rejected"),
        }
    }

    #[tokio::test]
    async fn oversized_content_length_is_rejected_before_allocating() {
        let frame = format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1);
        match read_from_bytes(frame.as_bytes()).await {
            ReadOutcome::Error(LspError::Protocol(msg)) => {
                assert!(msg.contains("exceeds cap"));
            }
            _ => panic!("expected the oversized length to be rejected"),
        }
    }

    #[tokio::test]
    async fn content_length_exactly_at_cap_is_accepted() {
        let body = vec![b'x'; MAX_CONTENT_LENGTH];
        let frame = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = frame.into_bytes();
        bytes.extend_from_slice(&body);

        match read_from_bytes(&bytes).await {
            ReadOutcome::Message(got) => assert_eq!(got.len(), MAX_CONTENT_LENGTH),
            _ => panic!("expected the message at exactly the cap to be accepted"),
        }
    }

    #[tokio::test]
    async fn partial_write_then_rest_is_read_correctly() {
        // Simulates a slow writer: the header arrives, then the body
        // trickles in across multiple writes/reads rather than all at
        // once, exercising `read_exact`'s handling of partial reads.
        let (mut tx, rx) = tokio::io::duplex(8);
        let mut reader = BufReader::new(rx);

        let body = br#"{"jsonrpc":"2.0","method":"partial"}"#;
        let frame = format!("Content-Length: {}\r\n\r\n", body.len());

        let write_task = tokio::spawn(async move {
            tx.write_all(frame.as_bytes()).await.unwrap();
            for chunk in body.chunks(5) {
                tx.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        });

        match read_message(&mut reader).await {
            ReadOutcome::Message(got) => assert_eq!(got, body),
            _ => panic!("expected a message"),
        }
        write_task.await.unwrap();
    }

    #[tokio::test]
    async fn stream_closed_after_partial_body_is_a_protocol_error() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = BufReader::new(rx);

        let write_task = tokio::spawn(async move {
            tx.write_all(b"Content-Length: 50\r\n\r\npartial")
                .await
                .unwrap();
            drop(tx);
        });

        match read_message(&mut reader).await {
            ReadOutcome::Error(LspError::Protocol(_)) => {}
            _ => panic!("expected a protocol error for a body cut short by connection close"),
        }
        write_task.await.unwrap();
    }

    #[test]
    fn encode_request_includes_id_method_and_params() {
        let bytes = encode_request(1, "initialize", json!({"processId": 42}));
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["processId"], 42);
        assert_eq!(value["jsonrpc"], "2.0");
    }

    #[test]
    fn encode_notification_has_no_id() {
        let bytes = encode_notification("initialized", json!({}));
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("id").is_none());
        assert_eq!(value["method"], "initialized");
    }

    #[test]
    fn encode_response_echoes_the_given_id_and_carries_a_result_not_a_method() {
        let bytes = encode_response(json!(7), json!({"applied": true}));
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["result"]["applied"], true);
        assert!(value.get("method").is_none());
        assert_eq!(value["jsonrpc"], "2.0");
    }
}
