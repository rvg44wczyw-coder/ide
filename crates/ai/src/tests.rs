//! Unit tests for `AiConfig` (load bounds + ordering logic). The
//! credential-driven `enabled()` filtering is exercised only through
//! `OllamaLocal` (unconditionally enabled) and structural guarantees, since
//! mutating process env vars (or asserting on their absence) is racy under
//! parallel test threads and would otherwise be environment-dependent.
//!
//! The transport/parse layer below is tested against a throwaway
//! `127.0.0.1` `TcpListener` serving canned HTTP bodies, never a real
//! provider: `HttpTransport::post_json`/`post_stream` and
//! `Provider::dispatch` take their URI from the `WireRequest` we build, so
//! the full SSE stream-parse path runs without network access.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;

use serde_json::json;

use crate::{
    classify_status, default_model, extract_delta, extract_text, fallback_eligible,
    parse_sse_event, resolve_role_route, truncate, try_in_order, AiConfig, AiError, ChatDelta,
    ChatMessage, ChatRequest, ChatRole, DefaultRouter, HttpTransport, PermissionMode, Provider,
    ProviderId, RoleRoute, Router, SseParser, TaskRole, WireRequest, OLLAMA_FIM_MODEL,
};

/// Minimal self-cleaning temp project root (creates `.ide/`).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ide-ai-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ide")).expect("create .ide");
        TempRoot(dir)
    }

    fn write_ai_json(&self, content: &str) {
        let mut f = std::fs::File::create(self.0.join(".ide").join("ai.json")).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn default_matches_doc_section_2_3() {
    let d = AiConfig::default();
    assert_eq!(
        d.provider_order,
        vec![
            ProviderId::Gemini,
            ProviderId::Groq,
            ProviderId::GitHubModels,
            ProviderId::OllamaLocal,
        ]
    );
    assert!(d.sanitize_local);
    assert_eq!(d.local_sanitize_threshold, 4.0);
    assert_eq!(d.cloud_sanitize_threshold, 3.5);
}

#[test]
fn enabled_providers_falls_back_to_ollama_alone_with_no_cloud_credentials_set() {
    // Assumes no GEMINI_API_KEY/GROQ_API_KEY/GITHUB_MODELS_TOKEN is set in
    // the test environment (true for CI and a typical dev shell) -- see
    // this file's module doc for why env-dependent assertions are scoped
    // to structural guarantees rather than exact env state. Cloud-first
    // `provider_order` means the three cloud entries are filtered out by
    // `enabled()` here, leaving only the always-enabled local provider.
    let d = AiConfig::default();
    let enabled = d.enabled_providers();
    assert_eq!(enabled, vec![ProviderId::OllamaLocal]);
}

#[test]
fn enabled_providers_dedupes_and_caps() {
    let config = AiConfig {
        provider_order: vec![ProviderId::OllamaLocal],
        ..AiConfig::default()
    };
    let enabled = config.enabled_providers();
    assert_eq!(
        enabled
            .iter()
            .filter(|id| **id == ProviderId::OllamaLocal)
            .count(),
        1
    );

    let wide = AiConfig {
        provider_order: vec![
            ProviderId::OllamaLocal,
            ProviderId::OllamaLocal,
            ProviderId::OllamaLocal,
            ProviderId::OllamaLocal,
            ProviderId::OllamaLocal,
        ],
        ..AiConfig::default()
    };
    assert!(wide.enabled_providers().len() <= 4);
}

#[test]
fn load_missing_file_returns_default() {
    let root = TempRoot::new("missing");
    assert_eq!(AiConfig::load(&root.0), AiConfig::default());
}

#[test]
fn load_malformed_file_returns_default() {
    let root = TempRoot::new("malformed");
    root.write_ai_json("not json at all {");
    assert_eq!(AiConfig::load(&root.0), AiConfig::default());
}

#[test]
fn load_parses_and_truncates_provider_order() {
    let root = TempRoot::new("valid");
    root.write_ai_json(
        r#"{
            "provider_order": [
                "OllamaLocal", "OllamaLocal", "OllamaLocal", "OllamaLocal",
                "OllamaLocal", "OllamaLocal", "OllamaLocal", "OllamaLocal"
            ],
            "sanitize_local": false,
            "local_sanitize_threshold": 2.0,
            "cloud_sanitize_threshold": 1.5
        }"#,
    );
    let config = AiConfig::load(&root.0);
    assert!(!config.sanitize_local);
    assert_eq!(config.local_sanitize_threshold, 2.0);
    assert_eq!(config.cloud_sanitize_threshold, 1.5);
    assert!(config.provider_order.len() <= 4);
}

#[test]
fn load_truncates_an_oversized_role_route_provider_order_too() {
    let root = TempRoot::new("role-route-oversized");
    root.write_ai_json(
        r#"{
            "role_routes": {
                "Planning": {
                    "provider_order": [
                        "OllamaLocal", "OllamaLocal", "OllamaLocal", "OllamaLocal",
                        "OllamaLocal", "OllamaLocal", "OllamaLocal", "OllamaLocal"
                    ]
                }
            }
        }"#,
    );
    let config = AiConfig::load(&root.0);
    let route = config.role_routes.get(&TaskRole::Planning).unwrap();
    assert!(route.provider_order.len() <= 4);
}

#[test]
fn load_partial_file_defaults_missing_fields() {
    let root = TempRoot::new("partial");
    root.write_ai_json(r#"{ "provider_order": ["OllamaLocal"] }"#);
    let config = AiConfig::load(&root.0);
    assert_eq!(config.provider_order, vec![ProviderId::OllamaLocal]);
    assert_eq!(config.local_sanitize_threshold, 4.0);
    assert_eq!(config.cloud_sanitize_threshold, 3.5);
    assert_eq!(config.agent_mode, PermissionMode::Plan);
}

#[test]
fn agent_mode_defaults_to_plan_the_safest_mode() {
    assert_eq!(AiConfig::default().agent_mode, PermissionMode::Plan);
    assert_eq!(PermissionMode::default(), PermissionMode::Plan);
}

#[test]
fn permission_mode_round_trips_through_json() {
    for mode in [
        PermissionMode::Plan,
        PermissionMode::Approve,
        PermissionMode::Auto,
    ] {
        let json = serde_json::to_string(&mode).unwrap();
        let back: PermissionMode = serde_json::from_str(&json).unwrap();
        assert_eq!(mode, back);
    }
}

#[test]
fn serialize_roundtrip_preserves_config() {
    let mut role_routes = HashMap::new();
    role_routes.insert(
        TaskRole::Planning,
        RoleRoute {
            provider_order: vec![ProviderId::Gemini],
            model_override: Some("gemini-2.0-flash".to_string()),
        },
    );
    let config = AiConfig {
        provider_order: vec![ProviderId::Groq, ProviderId::OllamaLocal],
        sanitize_local: false,
        local_sanitize_threshold: 2.5,
        cloud_sanitize_threshold: 1.5,
        role_routes,
        auto_route: true,
        classifier_provider: ProviderId::Groq,
        agent_mode: PermissionMode::Approve,
    };
    let json = serde_json::to_string(&config).unwrap();
    assert!(
        json.contains("\"Planning\""),
        "role_routes keys serialize as PascalCase variant names: {json}"
    );
    let back: AiConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back, config);
}

// ------------------------------------------------------------------------
// `resolve_role_route` / `parse_task_role_label` / `classify_task_role`
// (T55): role resolution and its always-safe fallback to `General`.
// ------------------------------------------------------------------------

#[test]
fn resolve_role_route_falls_back_to_top_level_order_when_role_is_unconfigured() {
    let config = AiConfig::default();
    let route = resolve_role_route(&config, TaskRole::Planning);
    assert_eq!(route.provider_order, config.provider_order);
    assert_eq!(route.model_override, None);
}

#[test]
fn resolve_role_route_falls_back_when_provider_order_is_empty() {
    let mut role_routes = HashMap::new();
    role_routes.insert(
        TaskRole::Coding,
        RoleRoute {
            provider_order: Vec::new(),
            model_override: Some("should-be-ignored".to_string()),
        },
    );
    let config = AiConfig {
        role_routes,
        ..AiConfig::default()
    };
    let route = resolve_role_route(&config, TaskRole::Coding);
    assert_eq!(route.provider_order, config.provider_order);
    assert_eq!(route.model_override, None);
}

#[test]
fn resolve_role_route_falls_back_when_every_listed_provider_is_disabled_at_runtime() {
    // Assumes no GEMINI_API_KEY is set in the test environment (see this
    // file's module doc) -- a role route naming only a disabled cloud
    // provider must not surface a harder failure than not configuring the
    // role at all.
    let mut role_routes = HashMap::new();
    role_routes.insert(
        TaskRole::Review,
        RoleRoute {
            provider_order: vec![ProviderId::Gemini],
            model_override: None,
        },
    );
    let config = AiConfig {
        role_routes,
        ..AiConfig::default()
    };
    let route = resolve_role_route(&config, TaskRole::Review);
    assert_eq!(route.provider_order, config.provider_order);
}

#[test]
fn resolve_role_route_uses_the_configured_route_when_it_has_an_enabled_provider() {
    let mut role_routes = HashMap::new();
    role_routes.insert(
        TaskRole::Coding,
        RoleRoute {
            provider_order: vec![ProviderId::OllamaLocal],
            model_override: Some("codellama".to_string()),
        },
    );
    let config = AiConfig {
        role_routes,
        ..AiConfig::default()
    };
    let route = resolve_role_route(&config, TaskRole::Coding);
    assert_eq!(route.provider_order, vec![ProviderId::OllamaLocal]);
    assert_eq!(route.model_override.as_deref(), Some("codellama"));
}

#[test]
fn parse_task_role_label_matches_all_four_labels_case_insensitively() {
    assert_eq!(crate::parse_task_role_label("general"), TaskRole::General);
    assert_eq!(crate::parse_task_role_label("Planning"), TaskRole::Planning);
    assert_eq!(crate::parse_task_role_label("CODING"), TaskRole::Coding);
    assert_eq!(crate::parse_task_role_label("  review\n"), TaskRole::Review);
}

#[test]
fn parse_task_role_label_defaults_unrecognized_text_to_general() {
    assert_eq!(crate::parse_task_role_label(""), TaskRole::General);
    assert_eq!(
        crate::parse_task_role_label("I'm not sure, maybe planning?"),
        TaskRole::General
    );
    assert_eq!(crate::parse_task_role_label("garbage"), TaskRole::General);
}

#[tokio::test]
async fn classify_task_role_falls_back_to_general_when_the_call_times_out() {
    // A zero-duration timeout guarantees the timeout branch fires before
    // any real dispatch attempt completes (or even starts), independent of
    // network state -- proves the "never blocks/fails the real request"
    // contract without a live provider.
    let role = crate::classify_task_role_with_timeout(
        ProviderId::OllamaLocal,
        "refactor this function",
        false,
        std::time::Duration::from_nanos(1),
    )
    .await;
    assert_eq!(role, TaskRole::General);
}

#[tokio::test]
async fn classify_task_role_never_dispatches_to_an_uncredentialed_cloud_provider() {
    // Assumes no GEMINI_API_KEY is set in the test environment (see this
    // file's module doc and enabled_providers_falls_back_to_ollama_alone_
    // with_no_cloud_credentials_set for the same established assumption).
    // A generous 30s timeout would let a real (mistaken) network dispatch
    // attempt run to completion -- a fast return here proves the enabled()
    // guard short-circuited before any dispatch was attempted at all,
    // rather than merely tolerating one that happened to fail quickly
    // (hacker fix round, T55: classify_task_role must never contact a
    // provider the rest of this crate treats as disabled).
    let start = std::time::Instant::now();
    let role = crate::classify_task_role_with_timeout(
        ProviderId::Gemini,
        "hi",
        true,
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(role, TaskRole::General);
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "must short-circuit on the enabled() check, not attempt a real network dispatch"
    );
}

#[tokio::test]
async fn classify_task_role_public_entry_point_never_exceeds_the_bounded_timeout() {
    // Exercises the real public entry point (not the injectable-timeout
    // sibling above) -- whatever the outcome (a fast ConnectionRefused if
    // nothing is listening locally, or a real reply if it is), it must
    // never take meaningfully longer than CLASSIFY_TIMEOUT, proving the
    // "never blocks the real request" contract without asserting a
    // specific network outcome either way.
    let start = std::time::Instant::now();
    let _ = crate::classify_task_role(ProviderId::OllamaLocal, "hi", false).await;
    assert!(
        start.elapsed() < crate::CLASSIFY_TIMEOUT + std::time::Duration::from_secs(1),
        "classify_task_role must never exceed its own bounded timeout"
    );
}

// ------------------------------------------------------------------------
// ProviderId / Provider / model mapping (no network, no env mutation).
// ------------------------------------------------------------------------

#[test]
fn provider_id_basics() {
    assert!(ProviderId::OllamaLocal.enabled());
    assert!(!ProviderId::OllamaLocal.is_cloud());
    assert!(ProviderId::Gemini.is_cloud());
    assert!(ProviderId::Groq.is_cloud());
    assert!(ProviderId::GitHubModels.is_cloud());
    assert_eq!(ProviderId::OllamaLocal.credential_env(), None);
    assert_eq!(ProviderId::Gemini.credential_env(), Some("GEMINI_API_KEY"));
    assert_eq!(ProviderId::Groq.credential_env(), Some("GROQ_API_KEY"));
    assert_eq!(
        ProviderId::GitHubModels.credential_env(),
        Some("GITHUB_MODELS_TOKEN")
    );
    for id in [
        ProviderId::OllamaLocal,
        ProviderId::Gemini,
        ProviderId::Groq,
        ProviderId::GitHubModels,
    ] {
        assert!(id.endpoint().starts_with("http"));
        assert!(!id.label().is_empty());
        assert_eq!(Provider::from_id(id).id(), id);
    }
}

#[test]
fn provider_id_endpoints_are_fixed() {
    assert_eq!(
        ProviderId::OllamaLocal.endpoint(),
        "http://localhost:11434/v1/chat/completions"
    );
    assert!(ProviderId::Gemini
        .endpoint()
        .contains("generativelanguage.googleapis.com"));
    assert!(ProviderId::Groq.endpoint().contains("api.groq.com"));
    assert!(ProviderId::GitHubModels
        .endpoint()
        .contains("inference.ai.azure.com"));
}

#[test]
fn default_models_are_per_provider_and_nonempty() {
    let mut seen = std::collections::HashSet::new();
    for id in [
        ProviderId::OllamaLocal,
        ProviderId::Gemini,
        ProviderId::Groq,
        ProviderId::GitHubModels,
    ] {
        let model = default_model(id);
        assert!(!model.is_empty());
        assert!(seen.insert(model), "duplicate model for {id:?}: {model}");
    }
    assert_eq!(default_model(ProviderId::OllamaLocal), "local-coder");
}

#[test]
fn non_ollama_fim_is_unsupported() {
    let rt = runtime();
    for id in [
        ProviderId::Gemini,
        ProviderId::Groq,
        ProviderId::GitHubModels,
    ] {
        let provider = Provider::from_id(id);
        let err = rt
            .block_on(provider.complete_fim("<prefix>", "<suffix>"))
            .unwrap_err();
        assert!(matches!(err, AiError::Unsupported), "{id:?}: {err}");
    }
    assert_eq!(OLLAMA_FIM_MODEL, "local-tab-coder");
}

#[test]
fn chat_wire_openai_shape_for_non_gemini() {
    let req = ChatRequest {
        messages: vec![
            ChatMessage::user("hello"),
            ChatMessage {
                role: ChatRole::Assistant,
                text: "hi".into(),
            },
        ],
        model: "some-model".into(),
        sanitized: true,
    };
    for id in [
        ProviderId::OllamaLocal,
        ProviderId::Groq,
        ProviderId::GitHubModels,
    ] {
        let wire = Provider::from_id(id).chat_wire(&req).unwrap();
        assert!(!wire.gemini);
        assert_eq!(wire.uri, id.endpoint());
        let body = wire.body;
        assert_eq!(body["model"], json!("some-model"));
        assert_eq!(body["stream"], json!(true));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], json!("user"));
        assert_eq!(messages[0]["content"], json!("hello"));
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(messages[1]["content"], json!("hi"));
    }
}

#[test]
fn chat_wire_gemini_shape_populates_query_key() {
    let req = ChatRequest {
        messages: vec![
            ChatMessage::user("hello"),
            ChatMessage {
                role: ChatRole::Assistant,
                text: "hi".into(),
            },
        ],
        model: default_model(ProviderId::Gemini).into(),
        sanitized: true,
    };
    let wire = Provider::from_id(ProviderId::Gemini)
        .chat_wire(&req)
        .unwrap();
    assert!(wire.gemini);
    assert!(wire.bearer.is_none());
    assert!(wire.uri.contains(":streamGenerateContent?alt=sse&key="));
    assert!(wire.uri.contains(default_model(ProviderId::Gemini)));
    let contents = wire.body["contents"].as_array().unwrap();
    assert_eq!(contents[0]["role"], json!("user"));
    assert_eq!(contents[0]["parts"][0]["text"], json!("hello"));
    assert_eq!(contents[1]["role"], json!("model"));
    assert!(wire.body.get("messages").is_none());
}

// ------------------------------------------------------------------------
// Pure parse/decision functions.
// ------------------------------------------------------------------------

#[test]
fn classify_status_maps_limits_and_passthrough() {
    assert!(matches!(classify_status(408), AiError::RateLimited));
    assert!(matches!(classify_status(429), AiError::RateLimited));
    assert!(matches!(classify_status(500), AiError::Http(500)));
    assert!(matches!(classify_status(404), AiError::Http(404)));
}

#[test]
fn fallback_eligible_classifies_each_variant() {
    let eligible = [
        AiError::ConnectionRefused,
        AiError::Timeout,
        AiError::RateLimited,
        AiError::StreamEnded,
        AiError::Http(500),
        AiError::Http(502),
        AiError::Http(503),
    ];
    for e in &eligible {
        assert!(fallback_eligible(e), "{e:?} should fall back");
    }
    let not_eligible = [
        AiError::Http(400),
        AiError::Http(429), // classified RateLimited, never constructed as Http
        AiError::Unsupported,
        AiError::Message("nope".into()),
    ];
    for e in &not_eligible {
        assert!(!fallback_eligible(e), "{e:?} should not fall back");
    }
}

#[test]
fn truncate_respects_max_and_char_boundaries() {
    assert_eq!(truncate("abc", 3), "abc");
    assert_eq!(truncate("abc", 2), "ab");
    assert_eq!(truncate("", 5), "");
    // "é" is 2 bytes: cutting at 3 must walk back to a char boundary.
    let s = "éééé";
    let out = truncate(s, 3);
    assert_eq!(out, "é");
    assert!(out.len() <= 3);
    assert_eq!(truncate("hello world", 200), "hello world");
}

// ------------------------------------------------------------------------
// SSE parsing.
// ------------------------------------------------------------------------

#[test]
fn parse_sse_event_extracts_data_lines_and_done_marker() {
    let (data, done) = parse_sse_event("data: hello\r\n\r\n").unwrap();
    assert_eq!(data, "hello");
    assert!(!done);
    let (data, done) = parse_sse_event("data: one\r\ndata: two\r\n\r\n").unwrap();
    assert_eq!(data, "one\ntwo");
    assert!(!done);
    let (data, done) = parse_sse_event("data: [DONE]\r\n\r\n").unwrap();
    assert_eq!(data, "");
    assert!(done);
}

#[test]
fn parse_sse_event_ignores_non_data_lines() {
    assert_eq!(parse_sse_event("id: 1\nevent: message\n\n"), None);
    assert_eq!(parse_sse_event(": a comment\n\n"), None);
}

#[test]
fn sse_parser_buffers_partial_events_between_chunks() {
    let mut parser = SseParser::default();
    parser.push("data: hel");
    assert!(parser.drain_events().is_empty());
    parser.push("lo\r\n\r\ndata: wor");
    assert_eq!(parser.drain_events(), vec![("hello".to_string(), false)]);
    parser.push("ld\r\n\r\n");
    assert_eq!(parser.drain_events(), vec![("world".to_string(), false)]);
}

#[test]
fn sse_parser_handles_crlf_and_done() {
    let mut parser = SseParser::default();
    parser.push("data: a\r\n\r\ndata: [DONE]\r\n\r\n");
    assert_eq!(
        parser.drain_events(),
        vec![("a".to_string(), false), (String::new(), true)]
    );
}

#[test]
fn extract_delta_openai_and_gemini_shapes() {
    let openai = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
    assert_eq!(extract_delta(openai, false), Some("hello".to_string()));
    // Role-only delta (no content) yields nothing.
    let role_only = r#"{"choices":[{"delta":{"role":"assistant"}}]}"#;
    assert_eq!(extract_delta(role_only, false), None);
    assert_eq!(extract_delta("not json", false), None);
    let gemini = r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#;
    assert_eq!(extract_delta(gemini, true), Some("hi".to_string()));
    let gemini_empty = r#"{"candidates":[{"content":{"parts":[]}}]}"#;
    assert_eq!(extract_delta(gemini_empty, true), None);
}

#[test]
fn extract_text_whole_body_shapes() {
    let openai = br#"{"choices":[{"message":{"content":"full answer"}}]}"#;
    assert_eq!(extract_text(openai, false), Some("full answer".to_string()));
    let gemini = br#"{"candidates":[{"content":{"parts":[{"text":"gem answer"}]}}]}"#;
    assert_eq!(extract_text(gemini, true), Some("gem answer".to_string()));
    assert_eq!(extract_text(b"{}", false), None);
    assert_eq!(extract_text(b"junk", true), None);
}

// ------------------------------------------------------------------------
// Commit-bin judgment: same request has no provider serving.
// ------------------------------------------------------------------------

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build test runtime")
}

#[tokio::test]
async fn router_with_no_enabled_providers_reports_error() {
    let (_tx, rx) = mpsc::channel();
    let err = DefaultRouter
        .chat(vec![ChatMessage::user("hi")], &[], None, true, _tx)
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::Message(msg) if msg.contains("no enabled providers")));
    assert!(rx.try_iter().next().is_none());
}

// ------------------------------------------------------------------------
// `try_in_order`: the router's provider-sequencing core, tested against a
// canned `attempt` closure so the full chain-of-fallbacks logic is proven
// without any real network call (regression coverage for the bug where
// the router used to give up after exactly two attempts regardless of how
// many providers were enabled).
// ------------------------------------------------------------------------

#[tokio::test]
async fn try_in_order_walks_every_provider_in_order_before_the_one_that_succeeds() {
    use std::sync::{Arc, Mutex};

    let order = [
        ProviderId::Gemini,
        ProviderId::Groq,
        ProviderId::GitHubModels,
        ProviderId::OllamaLocal,
    ];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_for_closure = calls.clone();

    let result = try_in_order(&order, std::time::Duration::from_millis(0), move |id| {
        let calls = calls_for_closure.clone();
        async move {
            calls.lock().unwrap().push(id);
            if id == ProviderId::OllamaLocal {
                Ok(())
            } else {
                Err(AiError::RateLimited)
            }
        }
    })
    .await;

    assert_eq!(result.unwrap(), ProviderId::OllamaLocal);
    assert_eq!(*calls.lock().unwrap(), order.to_vec());
}

#[tokio::test]
async fn try_in_order_stops_immediately_on_a_non_fallback_eligible_error() {
    use std::sync::{Arc, Mutex};

    let order = [
        ProviderId::Gemini,
        ProviderId::Groq,
        ProviderId::OllamaLocal,
    ];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_for_closure = calls.clone();

    let result = try_in_order(&order, std::time::Duration::from_millis(0), move |id| {
        let calls = calls_for_closure.clone();
        async move {
            calls.lock().unwrap().push(id);
            Err(AiError::Http(404))
        }
    })
    .await;

    assert!(matches!(result, Err(AiError::Http(404))));
    // Only the first provider was tried -- a credential/request problem
    // isn't fixed by switching providers, so the chain must not continue.
    assert_eq!(*calls.lock().unwrap(), vec![ProviderId::Gemini]);
}

#[tokio::test]
async fn try_in_order_returns_the_last_error_once_every_provider_is_exhausted() {
    let order = [
        ProviderId::Gemini,
        ProviderId::Groq,
        ProviderId::OllamaLocal,
    ];

    let result = try_in_order(&order, std::time::Duration::from_millis(0), |_id| async {
        Err(AiError::RateLimited)
    })
    .await;

    assert!(matches!(result, Err(AiError::RateLimited)));
}

#[tokio::test]
async fn try_in_order_retries_a_single_provider_when_it_is_the_only_one_enabled() {
    use std::sync::{Arc, Mutex};

    // Mirrors `DefaultRouter::chat`'s own single-provider duplication.
    let order = [ProviderId::OllamaLocal, ProviderId::OllamaLocal];
    let attempt_count = Arc::new(Mutex::new(0usize));
    let count_for_closure = attempt_count.clone();

    let result = try_in_order(&order, std::time::Duration::from_millis(0), move |_id| {
        let count = count_for_closure.clone();
        async move {
            let mut count = count.lock().unwrap();
            *count += 1;
            if *count < 2 {
                Err(AiError::RateLimited)
            } else {
                Ok(())
            }
        }
    })
    .await;

    assert_eq!(result.unwrap(), ProviderId::OllamaLocal);
    assert_eq!(*attempt_count.lock().unwrap(), 2);
}

// ------------------------------------------------------------------------
// Transport against a throwaway localhost HTTP server (no real provider).
// ------------------------------------------------------------------------

/// Serve `handler` on an ephemeral `127.0.0.1` port and return that port.
fn mock_server(handler: impl FnOnce(TcpStream) + Send + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            handler(stream);
        }
    });
    port
}

/// Drain the request line + headers so the server and client don't
/// interleave.
fn drain_request(sock: &mut TcpStream) {
    let mut buf = [0u8; 512];
    let _ = sock.read(&mut buf);
}

/// Read the request head and return it (for asserting on headers).
fn read_request_head(sock: &mut TcpStream) -> String {
    let mut buf = [0u8; 512];
    let _ = sock.read(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

fn http_head(status: u16, content_type: &str, body_len: usize) -> String {
    format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {body_len}\r\nconnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "ERR" }
    )
}

#[test]
fn post_json_returns_status_and_body() {
    let body = r#"{"reply":"ok"}"#;
    let head = http_head(200, "application/json", body.len());
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(body.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let transport = HttpTransport::new().unwrap();
    let (status, bytes) = runtime()
        .block_on(transport.post_json(
            &format!("http://127.0.0.1:{port}/v1/foo"),
            json!({"q": 1}),
            None,
        ))
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(String::from_utf8_lossy(&bytes), body);
}

#[test]
fn post_json_surfaces_non_2xx_status() {
    let head = http_head(429, "application/json", 0);
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let transport = HttpTransport::new().unwrap();
    let (status, _) = runtime()
        .block_on(transport.post_json(&format!("http://127.0.0.1:{port}/"), json!({}), None))
        .unwrap();
    assert_eq!(status, 429);
}

#[test]
fn post_json_sends_bearer_authorization_header() {
    let body = r#"{"reply":"ok"}"#;
    let head = http_head(200, "application/json", body.len());
    let port = mock_server(move |mut sock| {
        let req = read_request_head(&mut sock);
        assert!(
            req.to_lowercase().contains("authorization: bearer sekrit"),
            "missing bearer header in: {req}"
        );
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(body.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let transport = HttpTransport::new().unwrap();
    runtime()
        .block_on(transport.post_json(
            &format!("http://127.0.0.1:{port}/v1/foo"),
            json!({"q": 1}),
            Some("sekrit"),
        ))
        .expect("post_json succeeds");
}

#[test]
fn post_stream_sends_bearer_authorization_header() {
    let sse_head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    let port = mock_server(move |mut sock| {
        let req = read_request_head(&mut sock);
        assert!(
            req.to_lowercase().contains("authorization: bearer sekrit"),
            "missing bearer header in: {req}"
        );
        let _ = sock.write_all(sse_head.as_bytes());
        let _ = sock.write_all(body.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: Some("sekrit".into()),
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx, false))
        .expect("stream completes");
    let deltas: Vec<ChatDelta> = rx.iter().map(|r| r.unwrap()).collect();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].text, "hi");
}

#[test]
fn dispatch_streaming_surfaces_error_status() {
    let head = "HTTP/1.1 500 ERROR\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, _rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    let err = runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx, false))
        .unwrap_err();
    assert!(matches!(err, AiError::Http(500)), "{err}");
}

#[test]
fn dispatch_times_out_on_a_stalled_stream() {
    // Server sends SSE headers, then never writes another byte -- the
    // connection stays open (no `connection: close`, socket held past the
    // end of this test's own timeout window). Regression for the
    // permanent-wedge bug: without a per-chunk idle timeout this would
    // hang the caller forever.
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(head.as_bytes());
        std::thread::sleep(std::time::Duration::from_secs(5));
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, _rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    let err = runtime()
        .block_on(Provider::Ollama.dispatch_with_timeout(
            &transport,
            wire,
            tx,
            false,
            std::time::Duration::from_millis(50),
        ))
        .unwrap_err();
    assert!(matches!(err, AiError::Timeout), "{err}");
}

#[test]
fn dispatch_rejects_an_sse_stream_that_never_terminates_an_event() {
    // A malformed/hostile server that keeps sending bytes with no blank
    // line (SSE event terminator) must not grow `SseParser::buf`
    // unboundedly -- regression for the buffer-size cap.
    let oversized = "x".repeat(crate::MAX_SSE_BUFFER_BYTES + 1);
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
        oversized.len()
    );
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(oversized.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, _rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    let err = runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx, false))
        .unwrap_err();
    assert!(
        matches!(&err, AiError::Message(m) if m.contains("maximum buffered size")),
        "{err}"
    );
}

#[test]
fn stream_chat_refuses_a_cloud_request_that_is_not_sanitized() {
    // Runtime gate (§5 hacker fix round): even with the request otherwise
    // well-formed, an unsanitized request to a cloud provider is refused
    // before any network I/O -- no mock server is started, so a bug that
    // let this through would surface as a connection-refused error
    // instead of this specific message.
    let (tx, _rx) = mpsc::channel();
    let request = ChatRequest {
        messages: vec![ChatMessage::user("hello")],
        model: default_model(ProviderId::Gemini).into(),
        sanitized: false,
    };
    let err = runtime()
        .block_on(Provider::from_id(ProviderId::Gemini).stream_chat(&request, tx))
        .unwrap_err();
    assert!(
        matches!(&err, AiError::Message(m) if m.contains("sanitized")),
        "{err}"
    );
}

#[test]
fn dispatch_fim_branch_surfaces_error_status() {
    let head =
        "HTTP/1.1 404 NOT FOUND\r\ncontent-type: application/json\r\ncontent-length: 0\r\n\r\n";
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, _rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/fim"),
        body: json!({"stream": false}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    let err = runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx, true))
        .unwrap_err();
    assert!(matches!(err, AiError::Http(404)), "{err}");
}

#[test]
fn dispatch_streaming_stops_when_readers_leave() {
    let sse_head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\n";
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(sse_head.as_bytes());
        let _ = sock.write_all(body.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    // No receiver: the first `send` fails and dispatch must stop cleanly
    // rather than spin or error.
    let (tx, rx) = mpsc::channel();
    drop(rx);
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx, false))
        .expect("dispatch exits cleanly when the reader is gone");
}

#[test]
fn dispatch_streaming_keeps_partial_without_done_marker() {
    // Server closes the connection mid-stream without `[DONE]`: the reply
    // accumulated so far must still be delivered (`Ok`), matching how
    // providers that never emit the marker behave.
    let sse_head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\r\n\r\n";
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(sse_head.as_bytes());
        let _ = sock.write_all(body.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx, false))
        .expect("partial reply still completes Ok");
    let deltas: Vec<ChatDelta> = rx.iter().map(|r| r.unwrap()).collect();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].text, "hello");
}

#[test]
fn dispatch_streaming_stops_at_reply_cap() {
    let sse_head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    // Two events: first fills the buffer to the cap, second is ignored
    // because room == 0 after the first.
    let chunk1_len = crate::MAX_REPLY_CHARS;
    let chunk1 = "a".repeat(chunk1_len);
    let body = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{chunk1}\"}}}}]}}\r\n\r\n\
         data: {{\"choices\":[{{\"delta\":{{\"content\":\"nope\"}}}}]}}\r\n\r\n\
         data: [DONE]\r\n\r\n"
    );
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(sse_head.as_bytes());
        // Send in two chunks to stress the body-frame parser.
        let mid = body.len() / 2;
        let _ = sock.write_all(&body.as_bytes()[..mid]);
        let _ = sock.write_all(&body.as_bytes()[mid..]);
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx.clone(), false))
        .expect("dispatch exits cleanly at the cap");
    drop(tx);
    let deltas: Vec<ChatDelta> = rx.iter().map(|r| r.unwrap()).collect();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].text.len(), crate::MAX_REPLY_CHARS);
}

#[test]
fn sse_parser_buffers_a_crlf_crlf_boundary_split_across_chunks() {
    let mut parser = SseParser::default();
    parser.push("data: one\r\n\r\ndata: two\r\n\r");
    assert_eq!(parser.drain_events(), vec![("one".to_string(), false)]);
    parser.push("\n");
    assert_eq!(parser.drain_events(), vec![("two".to_string(), false)]);
}

#[test]
fn post_json_reports_connection_refused() {
    // Bind then drop: nothing listening on this port anymore.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let transport = HttpTransport::new().unwrap();
    let err = runtime()
        .block_on(transport.post_json(&format!("http://127.0.0.1:{port}/"), json!({}), None))
        .unwrap_err();
    assert!(matches!(err, AiError::ConnectionRefused), "{err}");
}

#[test]
fn dispatch_streams_sse_deltas_end_to_end() {
    // Split the SSE body across two socket writes to exercise the parser's
    // partial-event buffering on the live path.
    let sse_head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let first = "data: {\"choices\":[{\"delta\":{\"content\":\"hel";
    let rest = "lo\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(sse_head.as_bytes());
        let _ = sock.write_all(first.as_bytes());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = sock.write_all(rest.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });

    let (tx, rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx, false))
        .expect("stream completes");
    let deltas: Vec<ChatDelta> = rx
        .try_iter()
        .map(|r| r.expect("no transport error in deltas"))
        .collect();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].text, "hello");
}

#[test]
fn dispatch_returns_stream_ended_on_empty_body() {
    let sse_head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(sse_head.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, _rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/v1/chat/completions"),
        body: json!({"stream": true}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    let err = runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx, false))
        .unwrap_err();
    assert!(matches!(err, AiError::StreamEnded), "{err}");
}

#[test]
fn dispatch_fim_branch_pushes_whole_body_text() {
    let body = r#"{"choices":[{"message":{"content":"complete"}}]}"#;
    let head = http_head(200, "application/json", body.len());
    let port = mock_server(move |mut sock| {
        drain_request(&mut sock);
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(body.as_bytes());
        let _ = sock.shutdown(Shutdown::Both);
    });
    let (tx, rx) = mpsc::channel();
    let wire = WireRequest {
        uri: format!("http://127.0.0.1:{port}/fim"),
        body: json!({"stream": false}),
        bearer: None,
        gemini: false,
    };
    let transport = HttpTransport::new().unwrap();
    runtime()
        .block_on(Provider::Ollama.dispatch(&transport, wire, tx.clone(), true))
        .expect("fim completes");
    drop(tx);
    let got: Vec<ChatDelta> = rx.iter().map(|r| r.unwrap()).collect();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].text, "complete");
}
