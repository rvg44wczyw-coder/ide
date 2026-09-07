//! AI provider layer for hybrid local/cloud chat + FIM
//! (`docs/features/tui-ai-hybrid-fallback.md` §2.3, T49). Frontend-
//! independent: TUI-first, GUI later. Builds on `ide-core` (for the
//! `ProjectSettingsFile::Ai` read) and, when the `sanitizer` feature is
//! on (default), requires a mask before any cloud dispatch.
//!
//! **No `async_trait`, no `futures`/`tokio-stream`:** streaming is exposed
//! as a synchronous, channel-bound reader. Each provider's async task
//! pushes `ChatDelta`s into a caller-supplied
//! `std::sync::mpsc::Sender<Result<ChatDelta, AiError>>`, matching the
//! panel's existing background-thread + mpsc model.

use std::error::Error;
use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};

pub use crate::project::AiConfig;

mod project;

/// Provider identity + enablement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderId {
    OllamaLocal,
    Gemini,
    Groq,
    GitHubModels,
}

impl ProviderId {
    /// Env var that credentials this provider (`None` for Ollama, which
    /// needs none -- localhost).
    pub fn credential_env(self) -> Option<&'static str> {
        match self {
            ProviderId::OllamaLocal => None,
            ProviderId::Gemini => Some("GEMINI_API_KEY"),
            ProviderId::Groq => Some("GROQ_API_KEY"),
            ProviderId::GitHubModels => Some("GITHUB_MODELS_TOKEN"),
        }
    }

    /// Whether this provider is enabled *right now*, based on creds
    /// (Ollama is always enabled, cloud only with its env var set).
    pub fn enabled(self) -> bool {
        match self.credential_env() {
            None => true,
            Some(var) => std::env::var_os(var).is_some(),
        }
    }

    /// Is this a cloud (off-localhost) provider? Cloud routes are
    /// sanitizer-mandatory (see `stream_chat`'s gate).
    pub(self) fn is_cloud(self) -> bool {
        !matches!(self, ProviderId::OllamaLocal)
    }

    /// A human short label for the status line.
    pub fn label(self) -> &'static str {
        match self {
            ProviderId::OllamaLocal => "local (ollama)",
            ProviderId::Gemini => "cloud (gemini)",
            ProviderId::Groq => "cloud (groq)",
            ProviderId::GitHubModels => "cloud (github models)",
        }
    }

    /// The provider's fixed base endpoint (credentials never appear in a
    /// URL *path*, only as a query param/header -- `docs/features/
    /// tui-ai-hybrid-fallback.md` §4).
    pub fn endpoint(self) -> &'static str {
        match self {
            ProviderId::OllamaLocal => "http://localhost:11434/v1/chat/completions",
            ProviderId::Gemini => "https://generativelanguage.googleapis.com/v1beta/models",
            ProviderId::Groq => "https://api.groq.com/openai/v1/chat/completions",
            ProviderId::GitHubModels => "https://models.inference.ai.azure.com/chat/completions",
        }
    }
}

/// Chat message role. `Assistant` maps to Gemini's `role: "model"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
}

/// One prompt/context pair through the chain.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub text: String,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            text: text.into(),
        }
    }
}

/// JSON request shape as sent on the wire (OpenAI-compatible for
/// Ollama/Groq/GitHub Models; Gemini carries its own shape internally).
/// Unit-testable without network.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub model: String,
    /// Whether `messages` has already passed through `ide-sanitizer`'s
    /// masking. `stream_chat` refuses to reach a cloud provider unless this
    /// is `true` -- a *runtime* gate, not just the `sanitizer` compile-time
    /// feature, so a build with the feature on but a caller that skipped
    /// masking (or a config with masking disabled) still can't leak raw
    /// text to a cloud endpoint (`docs/features/tui-ai-hybrid-fallback.md`
    /// §5, `hacker` fix round). Local-only routes (`OllamaLocal`) ignore
    /// this field entirely.
    pub sanitized: bool,
}

/// A single streamed completion delta.
#[derive(Debug, Clone)]
pub struct ChatDelta {
    pub text: String,
}

/// Provider error taxonomy -- every variant has a status-line display
/// string via `thiserror`.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("connection refused (is the local model running?)")]
    ConnectionRefused,
    #[error("provider timed out")]
    Timeout,
    #[error("rate limited (HTTP 429/408)")]
    RateLimited,
    #[error("provider error: HTTP {0}")]
    Http(u16),
    #[error("stream ended without completion")]
    StreamEnded,
    #[error("this provider does not support FIM")]
    Unsupported,
    #[error("{0}")]
    Message(String),
}

/// Per-reply streaming cap (§4) -- a hostile/looping model can't balloon
/// memory past this many accumulated characters.
pub const MAX_REPLY_CHARS: usize = 200_000;
/// FIM context cap (§3.4): prefix and suffix are each truncated here.
pub const MAX_FIM_CONTEXT_CHARS: usize = 4096;
/// SSE parser buffer cap: a malicious/broken server that never sends the
/// blank-line event terminator (or a `data:` line without end) must not
/// grow `SseParser::buf` without bound -- `MAX_REPLY_CHARS` alone doesn't
/// cover this since that cap only applies to *extracted* text, checked
/// after an event is already fully parsed (`hacker` fix round).
pub const MAX_SSE_BUFFER_BYTES: usize = 1_048_576;
/// Per-chunk idle timeout on the SSE stream: a server that accepts the
/// connection, sends headers, then never sends another byte would
/// otherwise hang `dispatch`'s read loop forever with no error ever
/// reaching the panel (`hacker` fix round).
const STREAM_CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// FIM model served by Ollama (`local-tab-coder`).
pub const OLLAMA_FIM_MODEL: &str = "local-tab-coder";

/// The chat model name used for each provider when the router builds the
/// per-attempt request (model names are provider-specific, so the router
/// owns this mapping rather than accepting one string for every route).
/// Ollama's `local-coder` is the alias the user copies their 7B chat
/// model to (`ollama cp`); cloud names are fixed contract defaults in v1,
/// not user-configured.
pub fn default_model(id: ProviderId) -> &'static str {
    match id {
        ProviderId::OllamaLocal => "local-coder",
        ProviderId::Gemini => "gemini-2.0-flash",
        ProviderId::Groq => "llama-3.3-70b-versatile",
        ProviderId::GitHubModels => "gpt-4o-mini",
    }
}

/// A `hyper`-built HTTP/1.1 transport with TLS verification intact
/// (`hyper-rustls` native roots, `https_or_http`). Constructed once per
/// provider attempt.
pub(crate) struct HttpTransport {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<bytes::Bytes>,
    >,
}

impl HttpTransport {
    /// Fallible, not `.expect()`-panicking: a system with no usable native
    /// CA trust store (a minimal container/Linux install, e.g.) must not
    /// crash the background thread building this -- that thread's death
    /// would otherwise leak a permanently in-flight request with no error
    /// ever reaching the panel (`docs/features/tui-ai-hybrid-fallback.md`
    /// §5, `hacker` fix round). Built even for the Ollama-only local route,
    /// which never actually needs TLS, since `HttpsConnector` is the one
    /// connector type this transport uses for every provider.
    pub(crate) fn new() -> Result<Self, AiError> {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|e| AiError::Message(format!("failed to load native TLS roots: {e}")))?
            .https_or_http()
            .enable_http1()
            .build();
        let client: hyper_util::client::legacy::Client<
            hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
            http_body_util::Full<bytes::Bytes>,
        > = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(https);
        Ok(HttpTransport { client })
    }

    /// POST `json` to `uri`, return `(status, body_bytes)`. Never logs or
    /// echoes credentials: keys travel in the query/header, not the body,
    /// and an error string never includes the request payload.
    async fn post_json(
        &self,
        uri: &str,
        json: serde_json::Value,
        bearer: Option<&str>,
    ) -> Result<(u16, bytes::Bytes), AiError> {
        let body = serde_json::to_vec(&json)
            .map_err(|e| AiError::Message(format!("failed to serialize request: {e}")))?;
        let mut builder = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(uri)
            .header(hyper::header::CONTENT_TYPE, "application/json");
        if let Some(key) = bearer {
            builder = builder.header(hyper::header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let req = builder
            .body(http_body_util::Full::new(bytes::Bytes::from(body)))
            .map_err(|e| AiError::Message(format!("failed to build request: {e}")))?;
        let resp =
            tokio::time::timeout(std::time::Duration::from_secs(60), self.client.request(req))
                .await
                .map_err(|_| AiError::Timeout)?
                .map_err(map_conn_error)?;
        let status = resp.status().as_u16();
        // The header-fetch above and this body read are two separate
        // `await`s -- bounding only the first left a non-streaming request
        // (FIM) able to hang forever mid-body-read on a server that sends
        // headers promptly then stalls (`hacker` fix round).
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            http_body_util::BodyExt::collect(resp.into_body()),
        )
        .await
        .map_err(|_| AiError::Timeout)?
        .map_err(|e| AiError::Message(format!("failed to read response: {e}")))?;
        Ok((status, body.to_bytes()))
    }

    /// Streaming variant: returns the response status and a frame reader
    /// so the caller can parse SSE incrementally without buffering the
    /// whole reply.
    async fn post_stream(
        &self,
        uri: &str,
        json: serde_json::Value,
        bearer: Option<&str>,
    ) -> Result<(u16, BodyStream), AiError> {
        let body = serde_json::to_vec(&json)
            .map_err(|e| AiError::Message(format!("failed to serialize request: {e}")))?;
        let mut builder = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(uri)
            .header(hyper::header::CONTENT_TYPE, "application/json");
        if let Some(key) = bearer {
            builder = builder.header(hyper::header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let req = builder
            .body(http_body_util::Full::new(bytes::Bytes::from(body)))
            .map_err(|e| AiError::Message(format!("failed to build request: {e}")))?;
        let resp =
            tokio::time::timeout(std::time::Duration::from_secs(60), self.client.request(req))
                .await
                .map_err(|_| AiError::Timeout)?
                .map_err(map_conn_error)?;
        let status = resp.status().as_u16();
        Ok((
            status,
            BodyStream {
                body: resp.into_body(),
            },
        ))
    }
}

/// A manually-consumed body frame iterator -- uses `std::future::poll_fn`
/// against `http_body::Body::poll_frame`, so no `futures`/`tokio-stream`
/// dependency is needed (see the crate doc).
struct BodyStream {
    body: hyper::body::Incoming,
}

impl BodyStream {
    /// The next body chunk as text, `Ok`/`Err`, or `None` when the body
    /// is fully consumed. Trailer frames yield an empty string.
    async fn text(&mut self) -> Option<Result<String, AiError>> {
        let frame = std::future::poll_fn(|cx| {
            hyper::body::Body::poll_frame(std::pin::Pin::new(&mut self.body), cx)
        })
        .await?;
        match frame {
            Ok(f) => {
                if let Ok(data) = f.into_data() {
                    Some(Ok(String::from_utf8_lossy(&data).into_owned()))
                } else {
                    Some(Ok(String::new()))
                }
            }
            Err(e) => Some(Err(AiError::Message(format!("stream error: {e}")))),
        }
    }
}

fn map_conn_error(e: hyper_util::client::legacy::Error) -> AiError {
    if e.is_connect() {
        let mut cursor: Option<&(dyn Error + 'static)> = Some(&e);
        while let Some(src) = cursor {
            if src.to_string().to_lowercase().contains("refused") {
                return AiError::ConnectionRefused;
            }
            cursor = src.source();
        }
        return AiError::Message(format!("connect failed: {e}"));
    }
    AiError::Message(format!("request failed: {e}"))
}

/// The per-provider `Provider` dispatch (one variant per `ProviderId`).
/// Holds no transport state: each `stream_chat`/`complete_fim` call
/// constructs the shared `HttpTransport` it routes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Ollama,
    Gemini,
    Groq,
    GitHubModels,
}

impl Provider {
    /// Construct from its [`ProviderId`].
    pub fn from_id(id: ProviderId) -> Self {
        match id {
            ProviderId::OllamaLocal => Provider::Ollama,
            ProviderId::Gemini => Provider::Gemini,
            ProviderId::Groq => Provider::Groq,
            ProviderId::GitHubModels => Provider::GitHubModels,
        }
    }

    pub fn id(self) -> ProviderId {
        match self {
            Provider::Ollama => ProviderId::OllamaLocal,
            Provider::Gemini => ProviderId::Gemini,
            Provider::Groq => ProviderId::Groq,
            Provider::GitHubModels => ProviderId::GitHubModels,
        }
    }

    /// POST `request`, streaming SSE `data:` deltas into `tx`. Every
    /// delta (and the final `Ok(())`) is pushed; an error is pushed as an
    /// `Err` and also returned. Non-blocking to the caller when the caller
    /// drives it from a background thread's tokio runtime.
    ///
    /// Cloud routes are sanitizer-mandatory: with the crate's `sanitizer`
    /// feature disabled (a build without the masking guarantee), a cloud
    /// provider refuses to send -- `AiError::Message` (see §3.3). This
    /// gate is belt-and-suspenders on top of the panel's own
    /// sanitize-before-dispatch wiring.
    pub async fn stream_chat(
        &self,
        request: &ChatRequest,
        tx: Sender<Result<ChatDelta, AiError>>,
    ) -> Result<(), AiError> {
        // Runtime gate, not just the `sanitizer` compile-time feature: a
        // build with the feature enabled but a caller that skipped masking
        // (or a config with masking disabled) must not reach a cloud
        // provider with raw request text either (`hacker` fix round).
        if self.id().is_cloud() && (cfg!(not(feature = "sanitizer")) || !request.sanitized) {
            return Err(AiError::Message(
                "cloud route requires a sanitized request".to_string(),
            ));
        }
        let transport = HttpTransport::new()?;
        let wire = self.chat_wire(request)?;
        self.dispatch(&transport, wire, tx, false).await
    }

    /// FIM (fill-in-the-middle): prefix/suffix around a caret → completion
    /// text. Only `OllamaLocal` supports FIM in the default chain (cloud
    /// free-tiers have no FIM contract -- §3.1), so every other provider
    /// returns `AiError::Unsupported`. Returns the single completion
    /// string (non-streaming: FIM completions are short).
    pub async fn complete_fim(&self, prefix: &str, suffix: &str) -> Result<String, AiError> {
        if self.id() != ProviderId::OllamaLocal {
            return Err(AiError::Unsupported);
        }
        let prefix = truncate(prefix, MAX_FIM_CONTEXT_CHARS);
        let suffix = truncate(suffix, MAX_FIM_CONTEXT_CHARS);
        // qwen2.5-coder FIM template tokens.
        let prompt = format!("<fim_prefix>{prefix}<fim_suffix>{suffix}<fim_middle>");
        let uri = self.id().endpoint().to_string();
        let body = serde_json::json!({
            "model": OLLAMA_FIM_MODEL,
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
        });
        let transport = HttpTransport::new()?;
        let (status, bytes) = transport.post_json(&uri, body, None).await?;
        if status >= 400 {
            return Err(classify_status(status));
        }
        extract_text(&bytes, false).ok_or(AiError::StreamEnded)
    }

    /// Dispatch a prepared wire request: `fim=true` reads the whole body;
    /// otherwise it streams SSE lines and pushes each delta. Thin wrapper
    /// over [`Provider::dispatch_with_timeout`] fixing the real
    /// [`STREAM_CHUNK_TIMEOUT`] -- split out so tests can exercise the
    /// idle-timeout path itself with a short duration instead of a real
    /// 60-second wait.
    async fn dispatch(
        &self,
        transport: &HttpTransport,
        wire: WireRequest,
        tx: Sender<Result<ChatDelta, AiError>>,
        fim: bool,
    ) -> Result<(), AiError> {
        self.dispatch_with_timeout(transport, wire, tx, fim, STREAM_CHUNK_TIMEOUT)
            .await
    }

    async fn dispatch_with_timeout(
        &self,
        transport: &HttpTransport,
        wire: WireRequest,
        tx: Sender<Result<ChatDelta, AiError>>,
        fim: bool,
        chunk_timeout: std::time::Duration,
    ) -> Result<(), AiError> {
        if fim {
            let (status, bytes) = transport
                .post_json(&wire.uri, wire.body, wire.bearer.as_deref())
                .await?;
            if status >= 400 {
                return Err(classify_status(status));
            }
            let text = extract_text(&bytes, wire.gemini).ok_or(AiError::StreamEnded)?;
            let _ = tx.send(Ok(ChatDelta { text }));
            return Ok(());
        }

        let (status, mut stream) = transport
            .post_stream(&wire.uri, wire.body, wire.bearer.as_deref())
            .await?;
        if status >= 400 {
            return Err(classify_status(status));
        }

        let mut accumulated = 0usize;
        let mut parser = SseParser::default();
        loop {
            let next = match tokio::time::timeout(chunk_timeout, stream.text()).await {
                Ok(next) => next,
                Err(_) => return Err(AiError::Timeout),
            };
            let Some(chunk) = next else { break };
            let chunk = chunk?;
            parser.push(&chunk);
            if parser.len() > MAX_SSE_BUFFER_BYTES {
                return Err(AiError::Message(
                    "SSE event exceeded the maximum buffered size".to_string(),
                ));
            }
            for (data, is_done) in parser.drain_events() {
                if let Some(text) = extract_delta(&data, wire.gemini) {
                    let room = MAX_REPLY_CHARS.saturating_sub(accumulated);
                    if room == 0 {
                        return Ok(());
                    }
                    let clipped = truncate(&text, room);
                    if clipped.is_empty() {
                        return Ok(());
                    }
                    accumulated += clipped.len();
                    if tx.send(Ok(ChatDelta { text: clipped })).is_err() {
                        return Ok(());
                    }
                }
                if is_done {
                    return Ok(());
                }
            }
        }
        // Stream closed without a done marker. Any text already sent is a
        // partial-but-usable reply; a closed-with-nothing is StreamEnded.
        if accumulated == 0 {
            Err(AiError::StreamEnded)
        } else {
            Ok(())
        }
    }

    /// Build the wire request for chat (OpenAI shape or Gemini shape).
    fn chat_wire(&self, req: &ChatRequest) -> Result<WireRequest, AiError> {
        Ok(match self.id() {
            ProviderId::Gemini => {
                let contents: Vec<serde_json::Value> = req
                    .messages
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "role": match m.role {
                                ChatRole::User => "user",
                                ChatRole::Assistant => "model",
                            },
                            "parts": [{"text": m.text}],
                        })
                    })
                    .collect();
                let key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
                let uri = format!(
                    "{}/{}:streamGenerateContent?alt=sse&key={}",
                    self.id().endpoint(),
                    req.model,
                    key
                );
                WireRequest {
                    uri,
                    body: serde_json::json!({ "contents": contents }),
                    bearer: None,
                    gemini: true,
                }
            }
            _ => {
                let messages: Vec<serde_json::Value> = req
                    .messages
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "role": match m.role {
                                ChatRole::User => "user",
                                ChatRole::Assistant => "assistant",
                            },
                            "content": m.text,
                        })
                    })
                    .collect();
                WireRequest {
                    uri: self.id().endpoint().to_string(),
                    body: serde_json::json!({
                        "model": req.model,
                        "messages": messages,
                        "stream": true,
                    }),
                    bearer: self.bearer_token(),
                    gemini: false,
                }
            }
        })
    }

    fn bearer_token(&self) -> Option<String> {
        match self.id() {
            ProviderId::Groq => std::env::var("GROQ_API_KEY").ok(),
            ProviderId::GitHubModels => std::env::var("GITHUB_MODELS_TOKEN").ok(),
            _ => None, // Gemini's key is a query param; Ollama needs none
        }
    }
}

/// A prepared, transport-agnostic request (URI + JSON body + bearer).
struct WireRequest {
    uri: String,
    body: serde_json::Value,
    bearer: Option<String>,
    gemini: bool,
}

fn classify_status(status: u16) -> AiError {
    match status {
        408 | 429 => AiError::RateLimited,
        _ => AiError::Http(status),
    }
}

/// Extract a text chunk from one SSE `data:` payload (None if the payload
/// carried no text -- e.g. a role-only delta).
fn extract_delta(data: &str, gemini: bool) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    if gemini {
        v.get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .and_then(|a| a.first())
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .map(ToOwned::to_owned)
    } else {
        v.get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
            .map(ToOwned::to_owned)
    }
}

/// Extract the single text reply from a whole (non-streaming) JSON body.
fn extract_text(bytes: &[u8], gemini: bool) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if gemini {
        v.get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .and_then(|a| a.first())
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .map(ToOwned::to_owned)
    } else {
        v.get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(ToOwned::to_owned)
    }
}

/// A minimal SSE stream splitter: buffers chunk text, emits complete
/// *events* (terminated by a blank line) while keeping a trailing partial
/// event buffered. Handles multi-line JSON `data:` blocks and `\r\n`.
#[derive(Default)]
struct SseParser {
    buf: String,
}

impl SseParser {
    fn push(&mut self, chunk: &str) {
        self.buf.push_str(chunk);
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    /// Drain any complete events from the buffer, returning their `data:`
    /// payloads and whether the event was the `[DONE]` marker(s).
    fn drain_events(&mut self) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        loop {
            let boundary = find_event_boundary(&self.buf);
            let Some(end) = boundary else {
                break;
            };
            let event = self.buf[..end].to_string();
            self.buf = self.buf[end..].trim_start_matches('\r').to_string();
            if !self.buf.starts_with('\n') {
                // boundary consumes the blank line's first \n normally;
                // see find_event_boundary for the exact offsets.
                self.buf = self.buf.strip_prefix('\n').unwrap_or(&self.buf).to_string();
            }
            if let Some((data, done)) = parse_sse_event(&event) {
                out.push((data, done));
            }
        }
        out
    }
}

/// Finds the end offset of the first complete SSE event: an event ends at
/// a blank line (`\n\n` or `\r\n\r\n`). Returns the offset *after* the
/// terminating newline of the blank line.
fn find_event_boundary(buf: &str) -> Option<usize> {
    let bytes = buf.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\n' {
            // next byte closes the event if it is another \n.
            if bytes[i + 1] == b'\n' {
                return Some(i + 2);
            }
            // handle \r\n\r\n
            if bytes[i + 1] == b'\r' {
                let j = i + 1;
                if j + 1 < bytes.len() && bytes[j + 1] == b'\n' {
                    return Some(j + 2);
                }
            }
        }
    }
    None
}

/// Parse one complete SSE event into its `data:` payload (all `data:`
/// lines joined by `\n`) and whether it's a `[DONE]` marker.
fn parse_sse_event(event: &str) -> Option<(String, bool)> {
    let mut data_lines: Vec<String> = Vec::new();
    for line in event.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            data_lines.push(rest.to_string());
        }
        // For this API we ignore `event:`/`id:`/comments -- OpenAI and
        // Gemini only emit data.
    }
    if data_lines.is_empty() {
        return None;
    }
    let done = data_lines == ["[DONE]"];
    if done {
        return Some((String::new(), true));
    }
    Some((data_lines.join("\n"), false))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    } else {
        s.to_string()
    }
}

/// Full-chain router: walks every enabled provider in `order` once, in
/// order, stopping at the first success. On a fallback-eligible error a
/// fixed 500 ms backoff separates attempts, then the next provider in
/// `order` is tried; a non-fallback-eligible error aborts the chain
/// immediately (§3.1). When only one provider is enabled it is retried
/// once (a single flaky provider is worth one retry even with nothing to
/// fall back to). The number of attempts is naturally bounded by
/// `AiConfig::enabled_providers`'s `MAX_PROVIDERS` cap upstream, so this
/// never needs its own limit. Deltas for whichever attempt runs push into
/// `tx`; the final `ProviderId` that served is returned. The per-attempt
/// `ChatRequest` (with that provider's `default_model`) is built here, so
/// the caller never has to know which provider will serve.
///
/// Async (no `async_trait`, inherently): the caller supplies the tokio
/// runtime/thread -- the panel's single background thread owns it and
/// concurrently drains `tx`, so deltas stream live instead of arriving in
/// one burst when the reply completes.
pub trait Router {
    fn chat(
        &self,
        messages: Vec<ChatMessage>,
        order: &[ProviderId],
        sanitized: bool,
        tx: Sender<Result<ChatDelta, AiError>>,
    ) -> impl std::future::Future<Output = Result<ProviderId, AiError>> + Send;
}

/// Default router implementation (the panel's background thread calls
/// this from inside its own tokio runtime -- see `ai_panel.rs`).
pub struct DefaultRouter;

impl Router for DefaultRouter {
    async fn chat(
        &self,
        messages: Vec<ChatMessage>,
        order: &[ProviderId],
        sanitized: bool,
        tx: Sender<Result<ChatDelta, AiError>>,
    ) -> Result<ProviderId, AiError> {
        let order: Vec<ProviderId> = order.iter().copied().filter(|id| id.enabled()).collect();
        if order.is_empty() {
            return Err(AiError::Message("no enabled providers".to_string()));
        }
        let attempts: Vec<ProviderId> = if order.len() == 1 {
            vec![order[0], order[0]]
        } else {
            order
        };
        try_in_order(&attempts, std::time::Duration::from_millis(500), |id| {
            let provider = Provider::from_id(id);
            let request = ChatRequest {
                messages: messages.clone(),
                model: default_model(id).to_string(),
                sanitized,
            };
            let tx = tx.clone();
            async move { provider.stream_chat(&request, tx).await }
        })
        .await
    }
}

/// Pure sequencing core of the router: calls `attempt` once per id in
/// `order` (sleeping `backoff` between attempts, skipped before the
/// first), stopping at the first success or the first non-fallback-
/// eligible error, and returning the id that served. Split out from
/// `DefaultRouter::chat` so this control flow -- which providers get
/// tried, in what order, and when the chain gives up -- is unit-testable
/// against a canned `attempt` closure instead of requiring a real
/// network stack (every `ProviderId::endpoint()` but the local one is a
/// real internet host, so the full `Provider::stream_chat` path can't be
/// pointed at a test server).
async fn try_in_order<F, Fut>(
    order: &[ProviderId],
    backoff: std::time::Duration,
    mut attempt: F,
) -> Result<ProviderId, AiError>
where
    F: FnMut(ProviderId) -> Fut,
    Fut: std::future::Future<Output = Result<(), AiError>>,
{
    let mut last_err = None;
    for (i, &id) in order.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(backoff).await;
        }
        match attempt(id).await {
            Ok(()) => return Ok(id),
            Err(e) if fallback_eligible(&e) => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or(AiError::Message("no provider served".to_string())))
}

/// Whether a provider error triggers a fallback to the next provider
/// (§3.1): connection refusals, timeouts, rate limits, 5xx HTTP, and
/// premature stream endings. Everything else (4xx, unsupported, malformed
/// request) is not retried -- a request/credential problem masked over by
/// switching providers.
pub fn fallback_eligible(e: &AiError) -> bool {
    matches!(
        e,
        AiError::ConnectionRefused | AiError::Timeout | AiError::RateLimited | AiError::StreamEnded
    ) || matches!(e, AiError::Http(c) if *c >= 500)
}

#[cfg(test)]
mod tests;
