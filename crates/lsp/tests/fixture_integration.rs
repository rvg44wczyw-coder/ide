//! End-to-end tests against the fake LSP server fixture
//! (`tests/fixtures/fake_lsp_server.rs`, built by Cargo before this
//! target runs). Exercises `LspClient::start_with_command`'s real spawn
//! and JSON-RPC event-loop code path over real pipes, without depending
//! on `rust-analyzer` being installed on the test machine.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ide_lsp::{DiagnosticSeverity, LspClient, LspEvent, LspRequest, Position, Range};

const FIXTURE: &str = env!("CARGO_BIN_EXE_fake-lsp-server");

fn wait_until<T>(mut poll: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "condition did not become true in time"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn project_with_main_rs() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() {}").unwrap();
    (dir, root, main_rs)
}

/// A project root whose directory name is `scenario` -- the fixture
/// server reads this back out of `initialize`'s `rootUri` to decide how
/// to behave (see `tests/fixtures/fake_lsp_server.rs`).
fn project_with_scenario(scenario: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap().join(scenario);
    fs::create_dir_all(&root).unwrap();
    (dir, root)
}

#[test]
fn full_handshake_and_diagnostics_from_fixture_server() {
    let (_dir, root, main_rs) = project_with_main_rs();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    let event = wait_until(|| client.try_recv());
    match event {
        LspEvent::Diagnostics { path, diagnostics } => {
            assert_eq!(path, main_rs);
            assert_eq!(
                diagnostics.iter().map(|d| d.severity).collect::<Vec<_>>(),
                vec![
                    DiagnosticSeverity::Error,
                    DiagnosticSeverity::Warning,
                    DiagnosticSeverity::Information,
                    DiagnosticSeverity::Hint,
                ]
            );
            assert_eq!(diagnostics[0].message, "fixture error");
        }
        other => panic!("expected a Diagnostics event, got {other:?}"),
    }

    // The handshake has completed (we just received a post-`initialize`
    // notification), so this request takes the "ready" path straight
    // through `send_request` rather than the queue -- exercises that
    // branch distinctly from the pre-ready queueing covered below.
    client.send(LspRequest::DidClose { path: main_rs });
}

#[test]
fn requests_sent_before_ready_are_queued_and_invalid_paths_are_dropped_silently() {
    let (_dir, root, main_rs) = project_with_main_rs();

    let outside_dir = tempfile::tempdir().unwrap();
    let outside_path = fs::canonicalize(outside_dir.path())
        .unwrap()
        .join("evil.rs");
    fs::write(&outside_path, "fn evil() {}").unwrap();

    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    // Sent immediately after `start_with_command` returns, while the
    // background thread almost certainly hasn't finished the
    // `initialize` handshake yet -- exercises the "queued until ready"
    // path. One request deliberately escapes `project_root` and must be
    // dropped without derailing the connection or blocking the
    // legitimate request behind it.
    client.send(LspRequest::DidOpen {
        path: outside_path.clone(),
        text: "fn evil() {}".into(),
    });
    client.send(LspRequest::DidChange {
        path: outside_path.clone(),
        text: "fn evil() { /* changed */ }".into(),
    });
    client.send(LspRequest::DidClose { path: outside_path });
    client.send(LspRequest::DidOpen {
        path: main_rs.clone(),
        text: "fn main() {}".into(),
    });

    let event = wait_until(|| client.try_recv());
    match event {
        LspEvent::Diagnostics { path, .. } => assert_eq!(path, main_rs),
        other => panic!("expected a Diagnostics event, got {other:?}"),
    }
}

#[test]
fn server_closing_the_connection_after_init_surfaces_server_exited() {
    let (_dir, root) = project_with_scenario("exit-after-init");
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    let event = wait_until(|| client.try_recv());
    match event {
        LspEvent::ServerExited { message } => assert!(!message.is_empty()),
        other => panic!("expected ServerExited, got {other:?}"),
    }
}

#[test]
fn malformed_message_after_init_is_fatal_and_surfaces_server_exited() {
    let (_dir, root) = project_with_scenario("malformed-after-init");
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    // Per docs/features/rust-language-support.md §3: a malformed JSON-RPC
    // frame from the server is fatal, handled the same as a process
    // exit -- no attempt to resynchronize and keep reading mid-stream.
    let event = wait_until(|| client.try_recv());
    match event {
        LspEvent::ServerExited { message } => assert!(!message.is_empty()),
        other => panic!("expected ServerExited, got {other:?}"),
    }
}

#[test]
fn server_rejecting_initialize_surfaces_server_exited() {
    let (_dir, root) = project_with_scenario("reject-init");
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    let event = wait_until(|| client.try_recv());
    match event {
        LspEvent::ServerExited { message } => {
            assert!(message.contains("rootUri rejected"), "message: {message}");
        }
        other => panic!("expected ServerExited, got {other:?}"),
    }
}

#[test]
fn bad_wire_framing_after_init_is_fatal_and_surfaces_server_exited() {
    let (_dir, root) = project_with_scenario("bad-framing-after-init");
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    let event = wait_until(|| client.try_recv());
    match event {
        LspEvent::ServerExited { message } => assert!(!message.is_empty()),
        other => panic!("expected ServerExited, got {other:?}"),
    }
}

#[test]
fn flooding_diagnostics_for_one_path_is_coalesced_not_delivered_one_to_one() {
    // Regression coverage for the event-channel backpressure fix
    // (docs/security-findings/rust-lsp-dev-*.md finding 1): the fixture
    // fires 300 `publishDiagnostics` notifications for the same path
    // back-to-back with no delay and no draining on this side until the
    // flood is long over. A per-path coalescing scheme must both avoid
    // delivering all 300 individually and still end up with the *last*
    // message's content, not an arbitrary earlier one.
    let (_dir, root) = project_with_scenario("flood-diagnostics");
    // `project_with_scenario` only creates the root directory itself --
    // `validate_path` requires the target to actually exist on disk, so
    // the file the fixture will report diagnostics for must be created
    // here (unlike `project_with_main_rs`, which does this already).
    fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() {}").unwrap();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    // Deliberately don't drain while the flood happens.
    std::thread::sleep(Duration::from_millis(400));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut messages_for_main_rs = Vec::new();
    while Instant::now() < deadline {
        match client.try_recv() {
            Some(LspEvent::Diagnostics { path, diagnostics }) if path == main_rs => {
                messages_for_main_rs.push(diagnostics[0].message.clone());
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    assert!(
        !messages_for_main_rs.is_empty(),
        "expected at least one coalesced Diagnostics delivery for the flooded path"
    );
    assert!(
        messages_for_main_rs.len() < 300,
        "expected fewer than 300 individual deliveries (coalescing should collapse the \
         backlog), got {}",
        messages_for_main_rs.len()
    );
    assert_eq!(
        messages_for_main_rs.last(),
        Some(&"flood-299".to_string()),
        "the final delivered diagnostics must reflect the last message sent, not an \
         earlier one dropped mid-flood"
    );
}

#[test]
fn flooding_large_messages_stays_responsive_and_delivers_final_state() {
    // Regression coverage for the reader-task backpressure fix
    // (docs/security-findings/rust-lsp-dev-*.md finding 3): the fixture
    // fires 30 large publishDiagnostics messages (20,000 diagnostics
    // each) back-to-back with no delay. This test deliberately doesn't
    // drain while the flood is in-flight, then requires the client to
    // still be fully responsive afterward -- no hang while backpressure
    // is engaged, and the final delivered diagnostics must reflect the
    // last message sent, not an earlier/partial one.
    let (_dir, root) = project_with_scenario("flood-large-diagnostics");
    let main_rs = root.join("src/main.rs");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(&main_rs, "fn main() {}").unwrap();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    // Deliberately don't drain while the flood happens -- this is exactly
    // the condition finding 3 needed to trigger reader-task backpressure.
    std::thread::sleep(Duration::from_millis(300));

    let mut last_message_for_main_rs = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match client.try_recv() {
            Some(LspEvent::Diagnostics { path, diagnostics }) if path == main_rs => {
                last_message_for_main_rs = Some(diagnostics[0].message.clone());
            }
            Some(LspEvent::ServerExited { message }) => {
                panic!("expected the client to stay responsive under the flood, got ServerExited: {message}")
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    assert_eq!(
        last_message_for_main_rs,
        Some("flood-large-29".to_string()),
        "expected the final delivered diagnostics to reflect the last large message sent"
    );

    // The client must still be fully usable after the flood -- exercises
    // the "ready" send_request path once backpressure has cleared.
    client.send(LspRequest::DidClose { path: main_rs });
}

#[test]
fn references_request_receives_a_correlated_response() {
    // Regression coverage for the request/response correlation added by
    // find-usages (docs/features/find-usages.md §3/§4): a real round trip
    // through `send_request`'s id allocation, the fixture's canned
    // response, and `handle_incoming`'s id-matching + URI/path validation
    // -- not just the pure-logic unit tests in `client.rs`.
    let (_dir, root, main_rs) = project_with_main_rs();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    // Drain the default scenario's initial diagnostics notification first
    // so it isn't mistaken for the References response below.
    let _ = wait_until(|| client.try_recv());

    client.send(LspRequest::References {
        path: main_rs.clone(),
        position: Position {
            line: 3,
            character: 1,
        },
    });

    let event = wait_until(|| client.try_recv());
    match event {
        LspEvent::References { locations } => {
            assert_eq!(locations.len(), 1);
            assert_eq!(locations[0].path, main_rs);
            assert_eq!(locations[0].range.start.line, 7);
        }
        other => panic!("expected a References event, got {other:?}"),
    }
}

#[test]
fn publish_diagnostics_with_a_spurious_id_field_is_still_delivered() {
    // Regression coverage for docs/security-findings/rust-lsp-dev-find-
    // usages-*.md finding 2: a `publishDiagnostics` notification that also
    // carries a spurious numeric `"id"` field (spec-illegal, but the wire
    // parser doesn't enforce that) must still reach the method-based
    // dispatch, not be silently swallowed by `handle_incoming`'s
    // id-bearing-message branch before it ever gets there.
    let (_dir, root) = project_with_scenario("diagnostics-with-spurious-id");
    fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() {}").unwrap();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    let event = wait_until(|| client.try_recv());
    match event {
        LspEvent::Diagnostics { path, diagnostics } => {
            assert_eq!(path, main_rs);
            assert_eq!(diagnostics[0].message, "spurious-id-diagnostic");
        }
        other => panic!(
            "expected the spurious-id notification's diagnostics to still be delivered, got {other:?}"
        ),
    }
}

#[test]
fn code_action_request_and_apply_cover_all_branches_over_a_real_round_trip() {
    // Regression coverage for A8 (docs/features/code-actions.md §3.1/§3.3):
    // a real `textDocument/codeAction` round trip through the fixture,
    // then `ApplyCodeAction` exercised against all three response shapes
    // -- a bare `Command` (unsupported), a `CodeAction` with an edit
    // already present (applied directly, no wire traffic), and a
    // `CodeAction` needing `codeAction/resolve` (a second real round
    // trip) -- plus a stale index.
    let (_dir, root) = project_with_scenario("code-actions");
    fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() {}").unwrap();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv()); // drain the default diagnostics

    client.send(LspRequest::CodeAction {
        path: main_rs.clone(),
        position: Position {
            line: 0,
            character: 0,
        },
    });

    let actions = match wait_until(|| client.try_recv()) {
        LspEvent::CodeAction { path, actions } => {
            assert_eq!(path, main_rs);
            actions
        }
        other => panic!("expected a CodeAction event, got {other:?}"),
    };
    assert_eq!(actions.len(), 3);
    assert_eq!(actions[0].title, "Bare command");
    assert_eq!(actions[1].title, "Direct edit");
    assert_eq!(actions[2].title, "Needs resolve");

    // Index 0: a bare Command -- unsupported, settles with no edit and no
    // wire traffic.
    client.send(LspRequest::ApplyCodeAction { index: 0 });
    match wait_until(|| client.try_recv()) {
        LspEvent::WorkspaceEditReady { edit, label } => {
            assert_eq!(edit, None);
            assert_eq!(label, None);
        }
        other => panic!("expected an empty WorkspaceEditReady event, got {other:?}"),
    }

    // Index 1: already has an edit -- applied directly, no resolve round trip.
    client.send(LspRequest::ApplyCodeAction { index: 1 });
    match wait_until(|| client.try_recv()) {
        LspEvent::WorkspaceEditReady { edit, label } => {
            assert_eq!(label, Some("Direct edit".to_string()));
            let edit = edit.expect("expected a converted edit");
            assert_eq!(edit.edits[0].path, main_rs);
            assert_eq!(edit.edits[0].text_edits[0].new_text, "// direct\n");
        }
        other => panic!("expected a WorkspaceEditReady event with an edit, got {other:?}"),
    }

    // Index 2: needs resolving -- round trips through codeAction/resolve.
    client.send(LspRequest::ApplyCodeAction { index: 2 });
    match wait_until(|| client.try_recv()) {
        LspEvent::WorkspaceEditReady { edit, label } => {
            assert_eq!(label, Some("Needs resolve".to_string()));
            let edit = edit.expect("expected a converted edit");
            assert_eq!(edit.edits[0].text_edits[0].new_text, "// resolved\n");
        }
        other => panic!("expected a resolved WorkspaceEditReady event, got {other:?}"),
    }

    // A stale/out-of-range index settles the same way as "not found".
    client.send(LspRequest::ApplyCodeAction { index: 99 });
    match wait_until(|| client.try_recv()) {
        LspEvent::WorkspaceEditReady { edit, label } => {
            assert_eq!(edit, None);
            assert_eq!(label, None);
        }
        other => panic!("expected an empty WorkspaceEditReady event, got {other:?}"),
    }
}

#[test]
fn organize_imports_round_trips_through_its_own_resolve_slot_distinct_from_apply_code_action() {
    // Regression coverage for D4 (docs/features/code-generation.md §2.1,
    // §3.4): `OrganizeImports` is a one-shot, whole-document request that
    // never touches `last_code_actions`/`ApplyCodeAction`'s own
    // `pending_resolve_id` slot -- exercised here as a real round trip
    // through the fixture (which answers `context.only ==
    // ["source.organizeImports"]` with a single entry needing
    // `codeAction/resolve`, distinct from the ordinary three-entry
    // response `code_action_request_and_apply_cover_all_branches_over_a_
    // real_round_trip` above exercises).
    let (_dir, root) = project_with_scenario("code-actions");
    fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() {}").unwrap();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv()); // drain the default diagnostics

    client.send(LspRequest::OrganizeImports {
        path: main_rs.clone(),
    });

    match wait_until(|| client.try_recv()) {
        LspEvent::WorkspaceEditReady { edit, label } => {
            assert_eq!(label, Some("Optimize Imports".to_string()));
            let edit = edit.expect("expected a converted edit");
            assert_eq!(edit.edits[0].path, main_rs);
            assert_eq!(edit.edits[0].text_edits[0].new_text, "// organized\n");
        }
        other => panic!("expected a resolved WorkspaceEditReady event, got {other:?}"),
    }
}

#[test]
fn organize_imports_settles_with_no_edit_when_the_server_has_nothing_to_organize() {
    // The default scenario advertises no `codeActionProvider` at all --
    // `OrganizeImports` still sends its one-shot query (there's no
    // ambient cache/capability gate the way `Format`/`FormatRange` have),
    // and an empty/unusable response settles as "nothing to apply",
    // exactly the rust-analyzer-specific outcome §1 documents as
    // guaranteed.
    let (_dir, root, main_rs) = project_with_main_rs();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv());

    client.send(LspRequest::OrganizeImports { path: main_rs });

    match wait_until(|| client.try_recv()) {
        LspEvent::WorkspaceEditReady { edit, label } => {
            assert_eq!(edit, None);
            assert_eq!(label, None);
        }
        other => panic!("expected an empty WorkspaceEditReady event, got {other:?}"),
    }
}

#[test]
fn apply_code_action_with_no_prior_code_action_request_settles_immediately_with_no_edit() {
    // Regression coverage proving `ApplyCodeAction`'s not-found branch
    // needs no wire round trip at all: `last_code_actions` is still empty
    // (no `CodeAction` request was ever sent this run), yet
    // `WorkspaceEditReady` arrives promptly rather than hanging waiting on
    // a response nothing will ever send. Deliberately uses the default
    // scenario, which implements no `codeAction`-related methods at all --
    // if this ever regressed into sending wire traffic, the fixture would
    // simply never reply and `wait_until` would time out.
    let (_dir, root, main_rs) = project_with_main_rs();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv());

    client.send(LspRequest::ApplyCodeAction { index: 0 });
    match wait_until(|| client.try_recv()) {
        LspEvent::WorkspaceEditReady { edit, label } => {
            assert_eq!(edit, None);
            assert_eq!(label, None);
        }
        other => panic!("expected an empty WorkspaceEditReady event, got {other:?}"),
    }

    client.send(LspRequest::DidClose { path: main_rs });
}

#[test]
fn server_initiated_apply_edit_is_answered_and_a_path_escaping_edit_is_rejected() {
    // Regression coverage for A8's `workspace/applyEdit` handling
    // (docs/features/code-actions.md §3.5): the fixture sends a
    // server-initiated request unprompted, and this test confirms both
    // the emitted `LspEvent::WorkspaceEditReady` AND the literal JSON-RPC
    // response the client wrote back over stdin (via the fixture's own
    // ack/nack diagnostics, which only fire once it has read that
    // response) -- for both a valid edit and one that escapes the project
    // root.
    let (_dir, root) = project_with_scenario("apply-edit-server-initiated");
    fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() {}").unwrap();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();

    let mut saw_ready_edit = false;
    let mut saw_ack = false;
    let mut saw_escape_rejected = false;
    let mut saw_unexpected_second_ready_edit = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !saw_escape_rejected {
        match client.try_recv() {
            Some(LspEvent::WorkspaceEditReady { edit, label }) => {
                if saw_ready_edit {
                    saw_unexpected_second_ready_edit = true;
                } else {
                    saw_ready_edit = true;
                    assert_eq!(label, Some("Server applied".to_string()));
                    let edit = edit.expect("expected a converted edit");
                    assert_eq!(edit.edits[0].path, main_rs);
                    assert_eq!(edit.edits[0].text_edits[0].new_text, "// server\n");
                }
            }
            Some(LspEvent::Diagnostics { diagnostics, .. }) => {
                for d in diagnostics {
                    match d.message.as_str() {
                        "apply-edit-ack" => saw_ack = true,
                        "apply-edit-escape-rejected" => saw_escape_rejected = true,
                        other => panic!("unexpected diagnostic message: {other}"),
                    }
                }
            }
            Some(other) => panic!("unexpected event: {other:?}"),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    assert!(
        saw_ready_edit,
        "expected a WorkspaceEditReady for the valid server-initiated edit"
    );
    assert!(
        saw_ack,
        "expected the fixture's ack, proving the client replied applied:true"
    );
    assert!(
        saw_escape_rejected,
        "expected the fixture's rejection ack for the path-escaping edit"
    );
    assert!(
        !saw_unexpected_second_ready_edit,
        "the path-escaping edit must not emit its own WorkspaceEditReady event"
    );
}

#[test]
fn format_with_no_capability_advertised_settles_immediately_with_no_edit() {
    // Regression coverage for `send_format_request`'s capability-gate
    // no-wire-traffic path (docs/features/formatting.md §3.3): the
    // default scenario advertises neither `documentFormattingProvider`
    // nor `documentRangeFormattingProvider`, so both `Format` and
    // `FormatRange` must settle with `edit: None` promptly, with no wire
    // round trip -- if this ever regressed into sending a request, the
    // fixture (which implements neither method outside the "formatting"
    // scenario) would never reply and `wait_until` would time out.
    let (_dir, root, main_rs) = project_with_main_rs();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv());

    client.send(LspRequest::Format {
        path: main_rs.clone(),
        tab_size: 4,
        insert_spaces: true,
    });
    match wait_until(|| client.try_recv()) {
        LspEvent::FormatReady { path, edit } => {
            assert_eq!(path, main_rs);
            assert_eq!(edit, None);
        }
        other => panic!("expected an empty FormatReady event, got {other:?}"),
    }

    client.send(LspRequest::FormatRange {
        path: main_rs.clone(),
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        },
        tab_size: 4,
        insert_spaces: true,
    });
    match wait_until(|| client.try_recv()) {
        LspEvent::FormatReady { path, edit } => {
            assert_eq!(path, main_rs);
            assert_eq!(edit, None);
        }
        other => panic!("expected an empty FormatReady event, got {other:?}"),
    }
}

#[test]
fn format_and_format_range_round_trip_through_the_fixture_when_supported() {
    // Regression coverage for the happy path: the "formatting" scenario
    // advertises both capabilities, so both request kinds must actually
    // reach the fixture over the wire and come back as a converted
    // `WorkspaceEdit` carrying the *request's* own path (the response
    // itself carries none -- docs/features/formatting.md §3.1/§4).
    let (_dir, root) = project_with_scenario("formatting");
    fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() {}").unwrap();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv()); // drain the default diagnostics

    client.send(LspRequest::Format {
        path: main_rs.clone(),
        tab_size: 4,
        insert_spaces: true,
    });
    match wait_until(|| client.try_recv()) {
        LspEvent::FormatReady { path, edit } => {
            assert_eq!(path, main_rs);
            let edit = edit.expect("expected a converted edit");
            assert_eq!(edit.edits[0].path, main_rs);
            assert_eq!(edit.edits[0].text_edits[0].new_text, "// formatted\n");
        }
        other => panic!("expected a FormatReady event with an edit, got {other:?}"),
    }

    client.send(LspRequest::FormatRange {
        path: main_rs.clone(),
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        },
        tab_size: 4,
        insert_spaces: true,
    });
    match wait_until(|| client.try_recv()) {
        LspEvent::FormatReady { path, edit } => {
            assert_eq!(path, main_rs);
            let edit = edit.expect("expected a converted edit");
            assert_eq!(edit.edits[0].text_edits[0].new_text, "// range-formatted\n");
        }
        other => panic!("expected a FormatReady event with an edit, got {other:?}"),
    }
}

#[test]
fn rename_with_no_capability_advertised_settles_immediately_with_no_edit() {
    // Regression coverage for `PrepareRename`/`Rename`'s capability-gate
    // no-wire-traffic path (docs/features/rename-refactoring.md §3, §4):
    // the default scenario advertises no `renameProvider`, so
    // `PrepareRename` must settle `renameable: true` (§2.1's "not a
    // negative signal" rule) and `Rename` must settle `edit: None`,
    // both promptly with no wire round trip -- if this ever regressed
    // into sending a request, the fixture (which implements neither
    // method outside the "rename" scenario) would never reply and
    // `wait_until` would time out.
    let (_dir, root, main_rs) = project_with_main_rs();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv());

    let position = Position {
        line: 0,
        character: 3,
    };
    client.send(LspRequest::PrepareRename {
        path: main_rs.clone(),
        position,
    });
    match wait_until(|| client.try_recv()) {
        LspEvent::PrepareRenameReady { path, renameable } => {
            assert_eq!(path, main_rs);
            assert!(renameable);
        }
        other => panic!("expected a PrepareRenameReady event, got {other:?}"),
    }

    client.send(LspRequest::Rename {
        path: main_rs.clone(),
        position,
        new_name: "renamed".to_string(),
    });
    match wait_until(|| client.try_recv()) {
        LspEvent::RenameReady {
            path,
            new_name,
            edit,
        } => {
            assert_eq!(path, main_rs);
            assert_eq!(new_name, "renamed");
            assert_eq!(edit, None);
        }
        other => panic!("expected an empty RenameReady event, got {other:?}"),
    }
}

#[test]
fn prepare_rename_and_rename_round_trip_through_the_fixture_when_supported() {
    // Regression coverage for the happy path: the "rename" scenario
    // advertises `renameProvider` with `prepareProvider: true`, so both
    // request kinds must actually reach the fixture over the wire.
    let (_dir, root) = project_with_scenario("rename");
    fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    fs::write(&main_rs, "fn main() {}").unwrap();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv()); // drain the default diagnostics

    let position = Position {
        line: 0,
        character: 3,
    };
    client.send(LspRequest::PrepareRename {
        path: main_rs.clone(),
        position,
    });
    match wait_until(|| client.try_recv()) {
        LspEvent::PrepareRenameReady { path, renameable } => {
            assert_eq!(path, main_rs);
            assert!(renameable);
        }
        other => panic!("expected a PrepareRenameReady event, got {other:?}"),
    }

    client.send(LspRequest::Rename {
        path: main_rs.clone(),
        position,
        new_name: "renamed".to_string(),
    });
    match wait_until(|| client.try_recv()) {
        LspEvent::RenameReady {
            path,
            new_name,
            edit,
        } => {
            assert_eq!(path, main_rs);
            assert_eq!(new_name, "renamed");
            let edit = edit.expect("expected a converted edit");
            assert_eq!(edit.edits[0].path, main_rs);
            assert_eq!(edit.edits[0].text_edits[0].new_text, "renamed");
        }
        other => panic!("expected a RenameReady event with an edit, got {other:?}"),
    }
}

#[test]
fn dropping_client_returns_promptly() {
    let (_dir, root, _main_rs) = project_with_main_rs();
    let mut client = LspClient::start_with_command(&root, FIXTURE, &[]).unwrap();
    let _ = wait_until(|| client.try_recv());

    let start = Instant::now();
    drop(client);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "dropping LspClient should not block on the subprocess"
    );
}
