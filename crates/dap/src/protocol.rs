use crate::error::DapError;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{timeout_at, Instant};

/// Content-Length above this is rejected before any body buffer is
/// allocated -- a malicious or buggy debug adapter can't force an
/// unbounded allocation. Same value and rationale as `ide-lsp`'s
/// `protocol::MAX_CONTENT_LENGTH`: generous for any legitimate DAP
/// response (a large `variables`/`stackTrace` body), tiny next to what an
/// attacker would need to send to matter.
pub const MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;

/// Total header bytes (across every header line of a single message)
/// above this is rejected -- without this, `MAX_CONTENT_LENGTH` only
/// bounds the declared *body* size; a malicious or buggy adapter could
/// still force unbounded allocation via a single giant header line (or
/// unboundedly many small ones) before any `Content-Length` is ever
/// parsed.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Once a message has visibly begun (its first header byte has arrived),
/// the adapter must finish sending the rest of it within this window.
/// Without this, an adapter that sends a `Content-Length` header it never
/// fully honors -- and simply keeps its process alive without closing its
/// output -- wedges the read forever: no `ReadOutcome::Error`/`Eof` is ever
/// produced, so `DapEvent::AdapterExited` never fires and the session looks
/// alive but is permanently stuck (found live during `hacker`'s review,
/// `docs/security-findings/ide-dap-2026-09-02.md`). Deliberately generous
/// and, just as deliberately, applies only from the first byte of a message
/// onward -- never to the idle wait for the *next* message to start, which
/// a healthy session (no breakpoint hit yet) can sit in arbitrarily long.
const IN_PROGRESS_READ_TIMEOUT: Duration = Duration::from_secs(30);

pub enum ReadOutcome {
    Message(Vec<u8>),
    /// Stream closed cleanly at a message boundary (no header bytes read
    /// yet for this message) -- a normal process exit, not malformed
    /// input.
    Eof,
    /// A frame that doesn't conform to DAP's `Content-Length`-prefixed
    /// framing (the same framing LSP uses): bad/missing header, oversized
    /// length, or the stream closing mid-message. Always fatal to the
    /// connection.
    Error(DapError),
}

/// Reads one `Content-Length`-framed DAP message. Never allocates more
/// than `MAX_CONTENT_LENGTH` bytes for a claimed body, and never panics
/// on malformed input -- treats the debug adapter subprocess's output as
/// fully untrusted.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> ReadOutcome {
    read_message_with_timeout(reader, IN_PROGRESS_READ_TIMEOUT).await
}

async fn read_message_with_timeout<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    in_progress_timeout: Duration,
) -> ReadOutcome {
    let mut content_length: Option<usize> = None;
    // `Some` from the moment the first header byte of this message has
    // arrived -- both "is this message in progress" (mirrors the old
    // `headers_started` flag exactly) and the deadline the rest of this
    // message must finish by.
    let mut deadline: Option<Instant> = None;

    let mut limited = tokio::io::AsyncReadExt::take(reader, MAX_HEADER_BYTES as u64);

    loop {
        let mut line = String::new();
        let read_line_result = match deadline {
            Some(dl) => match timeout_at(dl, limited.read_line(&mut line)).await {
                Ok(result) => result,
                Err(_) => {
                    return ReadOutcome::Error(DapError::Protocol(
                        "timed out waiting for the adapter to finish a message it had already started sending"
                            .to_string(),
                    ));
                }
            },
            None => limited.read_line(&mut line).await,
        };
        let n = match read_line_result {
            Ok(n) => n,
            Err(e) => {
                return if deadline.is_some() {
                    ReadOutcome::Error(DapError::Protocol(format!("io error reading headers: {e}")))
                } else {
                    ReadOutcome::Eof
                };
            }
        };
        if n == 0 {
            return if deadline.is_some() {
                ReadOutcome::Error(DapError::Protocol(
                    "connection closed mid-headers".to_string(),
                ))
            } else {
                ReadOutcome::Eof
            };
        }
        if deadline.is_none() {
            deadline = Some(Instant::now() + in_progress_timeout);
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            if content_length.is_some() {
                return ReadOutcome::Error(DapError::Protocol(
                    "duplicate Content-Length header".to_string(),
                ));
            }
            let value = value.trim();
            let len: usize = match value.parse() {
                Ok(len) => len,
                Err(_) => {
                    return ReadOutcome::Error(DapError::Protocol(format!(
                        "invalid Content-Length header: {value:?}"
                    )));
                }
            };
            if len > MAX_CONTENT_LENGTH {
                return ReadOutcome::Error(DapError::Protocol(format!(
                    "Content-Length {len} exceeds cap of {MAX_CONTENT_LENGTH}"
                )));
            }
            content_length = Some(len);
        }
        // Other headers (e.g. Content-Type) are valid DAP framing and
        // simply ignored.
    }

    let Some(content_length) = content_length else {
        return ReadOutcome::Error(DapError::Protocol(
            "message headers missing Content-Length".to_string(),
        ));
    };

    let dl = deadline.expect("deadline is set once any header line has been read");
    let reader = limited.into_inner();
    let mut body = vec![0u8; content_length];
    let body_result = match timeout_at(dl, tokio::io::AsyncReadExt::read_exact(reader, &mut body))
        .await
    {
        Ok(result) => result,
        Err(_) => {
            return ReadOutcome::Error(DapError::Protocol(
                "timed out waiting for the adapter to finish a message it had already started sending"
                    .to_string(),
            ));
        }
    };
    if let Err(e) = body_result {
        return ReadOutcome::Error(DapError::Protocol(format!(
            "connection closed while reading body: {e}"
        )));
    }
    ReadOutcome::Message(body)
}

/// Writes one `Content-Length`-framed DAP message.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

/// Encodes a client-initiated DAP request. `arguments` is omitted from
/// the envelope entirely when `None` (several DAP commands, e.g.
/// `threads`/`configurationDone`, take none) rather than serialized as
/// `null`, since some adapters are stricter about an absent key than an
/// explicit null one.
pub fn encode_request(seq: i64, command: &str, arguments: Option<Value>) -> Vec<u8> {
    let mut message = serde_json::Map::new();
    message.insert("seq".to_string(), json!(seq));
    message.insert("type".to_string(), json!("request"));
    message.insert("command".to_string(), json!(command));
    if let Some(arguments) = arguments {
        message.insert("arguments".to_string(), arguments);
    }
    serde_json::to_vec(&Value::Object(message))
        .expect("serializing a well-typed DAP request cannot fail")
}

/// Answers an adapter-initiated *request* (`docs/features/debugger.md`
/// §3.3) -- every reverse request this crate doesn't implement gets one
/// of these with `success: false`, never silence (which would leave the
/// adapter's request hanging) and never a panic on an unrecognized
/// `command`.
pub fn encode_response(
    seq: i64,
    request_seq: i64,
    command: &str,
    success: bool,
    message: Option<&str>,
) -> Vec<u8> {
    let mut response = serde_json::Map::new();
    response.insert("seq".to_string(), json!(seq));
    response.insert("type".to_string(), json!("response"));
    response.insert("request_seq".to_string(), json!(request_seq));
    response.insert("success".to_string(), json!(success));
    response.insert("command".to_string(), json!(command));
    if let Some(message) = message {
        response.insert("message".to_string(), json!(message));
    }
    serde_json::to_vec(&Value::Object(response))
        .expect("serializing a well-typed DAP response cannot fail")
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
        let body = br#"{"seq":1,"type":"event","event":"initialized"}"#;
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
            ReadOutcome::Error(DapError::Protocol(_)) => {}
            _ => panic!("expected a protocol error for EOF mid-headers"),
        }
    }

    #[tokio::test]
    async fn truncated_body_is_a_protocol_error() {
        let frame = b"Content-Length: 100\r\n\r\ntoo short";
        match read_from_bytes(frame).await {
            ReadOutcome::Error(DapError::Protocol(_)) => {}
            _ => panic!("expected a protocol error for truncated body"),
        }
    }

    #[tokio::test]
    async fn duplicate_content_length_header_is_rejected() {
        let frame = b"Content-Length: 2\r\nContent-Length: 999999\r\n\r\n{}";
        match read_from_bytes(frame).await {
            ReadOutcome::Error(DapError::Protocol(msg)) => {
                assert!(msg.contains("duplicate"), "message: {msg}");
            }
            _ => panic!("expected a protocol error for duplicate Content-Length headers"),
        }
    }

    #[tokio::test]
    async fn non_numeric_content_length_is_a_protocol_error() {
        let frame = b"Content-Length: not-a-number\r\n\r\n";
        match read_from_bytes(frame).await {
            ReadOutcome::Error(DapError::Protocol(_)) => {}
            _ => panic!("expected a protocol error"),
        }
    }

    #[tokio::test]
    async fn missing_content_length_is_a_protocol_error() {
        let frame = b"Content-Type: application/json\r\n\r\n";
        match read_from_bytes(frame).await {
            ReadOutcome::Error(DapError::Protocol(_)) => {}
            _ => panic!("expected a protocol error"),
        }
    }

    #[tokio::test]
    async fn header_line_without_a_newline_is_rejected_not_grown_unbounded() {
        let junk = vec![b'a'; MAX_HEADER_BYTES + 1];
        match read_from_bytes(&junk).await {
            ReadOutcome::Error(DapError::Protocol(_)) => {}
            _ => panic!("expected an unterminated oversized header line to be rejected"),
        }
    }

    #[tokio::test]
    async fn many_small_headers_exceeding_total_budget_is_rejected() {
        let mut bytes = Vec::new();
        for _ in 0..(MAX_HEADER_BYTES / 16 + 1) {
            bytes.extend_from_slice(b"X-Junk: filler\r\n");
        }
        match read_from_bytes(&bytes).await {
            ReadOutcome::Error(DapError::Protocol(_)) => {}
            _ => panic!("expected the header total to be rejected"),
        }
    }

    #[tokio::test]
    async fn oversized_content_length_is_rejected_before_allocating() {
        let frame = format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1);
        match read_from_bytes(frame.as_bytes()).await {
            ReadOutcome::Error(DapError::Protocol(msg)) => {
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
        let (mut tx, rx) = tokio::io::duplex(8);
        let mut reader = BufReader::new(rx);

        let body = br#"{"seq":1,"type":"event","event":"output","body":{"output":"hi"}}"#;
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
            ReadOutcome::Error(DapError::Protocol(_)) => {}
            _ => panic!("expected a protocol error for a body cut short by connection close"),
        }
        write_task.await.unwrap();
    }

    #[tokio::test]
    async fn clean_eof_with_no_bytes_sent_is_never_time_bounded() {
        // No bytes at all yet -- this must resolve immediately via Eof
        // (clean stream close), not via the in-progress timeout, since no
        // message has started.
        let outcome =
            read_message_with_timeout(&mut BufReader::new(&b""[..]), Duration::from_millis(1))
                .await;
        assert!(matches!(outcome, ReadOutcome::Eof));
    }

    #[tokio::test]
    async fn idle_wait_for_the_next_message_to_start_is_never_time_bounded() {
        // `tx` stays open but sends nothing -- a genuinely blocked read,
        // not an immediate EOF. With a 1ms in-progress timeout, a read
        // that *were* incorrectly time-bounded pre-first-byte would
        // resolve (with an error) almost instantly; asserting the call
        // is still pending after 50ms proves the timeout is only wired up
        // once a message has actually started, exactly as
        // `a_message_that_stalls_mid_body_times_out_instead_of_hanging_forever`
        // (below) proves the opposite for a message already in flight.
        let (_tx, rx) = tokio::io::duplex(64);
        let mut reader = BufReader::new(rx);

        let read = read_message_with_timeout(&mut reader, Duration::from_millis(1));
        tokio::select! {
            _ = read => panic!("expected the idle pre-first-byte wait to remain pending"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }

    #[tokio::test]
    async fn a_message_that_stalls_mid_body_times_out_instead_of_hanging_forever() {
        let (tx, rx) = tokio::io::duplex(64);
        let mut reader = BufReader::new(rx);

        // Header promises 100 bytes; only 10 are ever sent, and `tx` is
        // kept alive (not dropped) -- the adapter equivalent of staying
        // running without ever finishing or closing the frame.
        let _tx = {
            let mut tx = tx;
            tx.write_all(b"Content-Length: 100\r\n\r\n0123456789")
                .await
                .unwrap();
            tx
        };

        let outcome = read_message_with_timeout(&mut reader, Duration::from_millis(50)).await;
        match outcome {
            ReadOutcome::Error(DapError::Protocol(msg)) => {
                assert!(msg.contains("timed out"), "message: {msg}");
            }
            _ => panic!("expected a timeout error for a message stalled mid-body"),
        }
    }

    #[tokio::test]
    async fn a_message_that_stalls_mid_headers_times_out_instead_of_hanging_forever() {
        let (tx, rx) = tokio::io::duplex(64);
        let mut reader = BufReader::new(rx);

        // First header line arrives; the blank line that would end the
        // header block never does, and `tx` is kept alive.
        let _tx = {
            let mut tx = tx;
            tx.write_all(b"Content-Length: 5\r\n").await.unwrap();
            tx
        };

        let outcome = read_message_with_timeout(&mut reader, Duration::from_millis(50)).await;
        match outcome {
            ReadOutcome::Error(DapError::Protocol(msg)) => {
                assert!(msg.contains("timed out"), "message: {msg}");
            }
            _ => panic!("expected a timeout error for headers stalled mid-message"),
        }
    }

    #[test]
    fn encode_request_includes_seq_command_and_arguments() {
        let bytes = encode_request(3, "launch", Some(json!({"program": "/bin/x"})));
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["seq"], 3);
        assert_eq!(value["type"], "request");
        assert_eq!(value["command"], "launch");
        assert_eq!(value["arguments"]["program"], "/bin/x");
    }

    #[test]
    fn encode_request_omits_arguments_key_entirely_when_none() {
        let bytes = encode_request(1, "threads", None);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("arguments").is_none());
    }

    #[test]
    fn encode_response_carries_request_seq_success_and_command() {
        let bytes = encode_response(7, 3, "runInTerminal", false, Some("not supported"));
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["seq"], 7);
        assert_eq!(value["type"], "response");
        assert_eq!(value["request_seq"], 3);
        assert_eq!(value["success"], false);
        assert_eq!(value["command"], "runInTerminal");
        assert_eq!(value["message"], "not supported");
    }

    #[test]
    fn encode_response_omits_message_key_when_none() {
        let bytes = encode_response(2, 1, "initialize", true, None);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("message").is_none());
    }
}
