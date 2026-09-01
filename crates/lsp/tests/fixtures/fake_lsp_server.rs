//! A minimal fake LSP server speaking `Content-Length`-framed JSON-RPC
//! over stdio, used to exercise `LspClient::start_with_command`'s real
//! spawn + event-loop code path deterministically, without depending on
//! `rust-analyzer` being installed on the test machine.

use std::io::{self, BufRead, Write};

fn read_message(stdin: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let n = stdin.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let len = content_length?;
    let mut body = vec![0u8; len];
    stdin.read_exact(&mut body).ok()?;
    Some(body)
}

fn write_message(stdout: &mut impl Write, body: &[u8]) {
    write!(stdout, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    stdout.write_all(body).unwrap();
    stdout.flush().unwrap();
}

fn main() {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    let Some(init_body) = read_message(&mut input) else {
        return;
    };
    let init: serde_json::Value = serde_json::from_slice(&init_body).unwrap_or_default();
    let root_uri = init["params"]["rootUri"].as_str().map(str::to_string);
    let init_id = init.get("id").cloned().unwrap_or(serde_json::Value::Null);

    // The test scenario is encoded as the last path segment of `rootUri`
    // (learned from `initialize`'s params, same as a real server) --
    // lets each test pick a deterministic fixture behavior via its
    // project_root's directory name, with no shared/global state (env
    // vars would race across `cargo test`'s parallel test threads).
    let scenario = root_uri
        .as_deref()
        .map(|u| u.trim_end_matches('/'))
        .and_then(|u| u.rsplit('/').next())
        .unwrap_or_default()
        .to_string();

    if scenario == "reject-init" {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "error": { "code": -32602, "message": "fixture: rootUri rejected" },
        });
        write_message(&mut output, &serde_json::to_vec(&response).unwrap());
        return;
    }

    // Only the "code-actions" scenario advertises `codeActionProvider`
    // (with `resolveProvider: true`) -- every other scenario keeps the
    // pre-existing empty capabilities object, so `code_action_resolve_
    // provider` fail-closes to `false` for them exactly as it always has.
    // Likewise, only the "formatting" scenario advertises
    // `documentFormattingProvider`/`documentRangeFormattingProvider` --
    // every other scenario (including the default) leaves both absent, so
    // `document_formatting_provider`/`document_range_formatting_provider`
    // fail-close to `false`, exercising the no-wire-traffic capability gate.
    // Only the "rename" scenario advertises `renameProvider` (as an
    // options object with `prepareProvider: true`) -- every other scenario
    // leaves it absent, so `rename_provider`/`prepare_rename_provider`
    // fail-close to `false` for them (`docs/features/rename-refactoring.md`
    // §2.2, §4).
    let capabilities = if scenario == "code-actions" {
        serde_json::json!({ "codeActionProvider": { "resolveProvider": true } })
    } else if scenario == "formatting" {
        serde_json::json!({
            "documentFormattingProvider": true,
            "documentRangeFormattingProvider": true,
        })
    } else if scenario == "rename" {
        serde_json::json!({ "renameProvider": { "prepareProvider": true } })
    } else {
        serde_json::json!({})
    };
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": init_id,
        "result": { "capabilities": capabilities },
    });
    write_message(&mut output, &serde_json::to_vec(&response).unwrap());

    // Drain the `initialized` notification the client sends next.
    let _ = read_message(&mut input);

    match scenario.as_str() {
        "exit-after-init" => return,
        "malformed-after-init" => {
            write_message(&mut output, b"{not valid json");
        }
        "apply-edit-server-initiated" => {
            // A server-initiated `workspace/applyEdit` request (the first
            // request kind this fixture ever sends, rather than responds
            // to) -- exercises `handle_incoming`'s id+method branch over a
            // real round trip, both the emitted event and the literal
            // JSON-RPC response written back over stdin.
            if let Some(root_uri) = &root_uri {
                let uri = format!("{}/src/main.rs", root_uri.trim_end_matches('/'));
                let good_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 500,
                    "method": "workspace/applyEdit",
                    "params": {
                        "label": "Server applied",
                        "edit": { "changes": { (uri.clone()): [{
                            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                            "newText": "// server\n",
                        }] } },
                    },
                });
                write_message(&mut output, &serde_json::to_vec(&good_request).unwrap());

                // Wait for the client's reply before moving on -- proves
                // it actually wrote a response back over stdin, not just
                // emitted an event -- then surface what it said via a
                // diagnostics notification the test can observe.
                if let Some(body) = read_message(&mut input) {
                    let resp: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let applied = resp["result"]["applied"].as_bool().unwrap_or(false);
                    let ack = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": uri,
                            "diagnostics": [{
                                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                                "severity": 1,
                                "message": if applied { "apply-edit-ack" } else { "apply-edit-nack" },
                            }],
                        },
                    });
                    write_message(&mut output, &serde_json::to_vec(&ack).unwrap());
                }

                // A second request whose edit targets a path outside the
                // project root -- the client must reject it (path
                // validation) and reply `applied: false`, with no
                // `WorkspaceEditReady` event for this one.
                let bad_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 501,
                    "method": "workspace/applyEdit",
                    "params": {
                        "edit": { "changes": { "file:///definitely/outside/the/project/root.rs": [{
                            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                            "newText": "// escape\n",
                        }] } },
                    },
                });
                write_message(&mut output, &serde_json::to_vec(&bad_request).unwrap());
                if let Some(body) = read_message(&mut input) {
                    let resp: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let applied = resp["result"]["applied"].as_bool().unwrap_or(true);
                    let ack = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": uri,
                            "diagnostics": [{
                                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                                "severity": 1,
                                "message": if applied {
                                    "escape-should-have-failed"
                                } else {
                                    "apply-edit-escape-rejected"
                                },
                            }],
                        },
                    });
                    write_message(&mut output, &serde_json::to_vec(&ack).unwrap());
                }
            }
        }
        "flood-diagnostics" => {
            // Floods many publishDiagnostics notifications for the *same*
            // path back-to-back with no delay, each carrying a distinct
            // marker message -- regression coverage for the
            // per-path-coalescing fix to the event channel's backpressure
            // handling (see docs/security-findings/rust-lsp-dev-*.md
            // finding 1). A well-behaved client must not grow memory
            // unboundedly here and must still end up delivering the
            // *last* message's diagnostics.
            if let Some(root_uri) = &root_uri {
                let uri = format!("{}/src/main.rs", root_uri.trim_end_matches('/'));
                for i in 0..300 {
                    let notification = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": uri,
                            "diagnostics": [{
                                "range": {
                                    "start": {"line": 0, "character": 0},
                                    "end": {"line": 0, "character": 1},
                                },
                                "severity": 1,
                                "message": format!("flood-{i}"),
                            }],
                        },
                    });
                    write_message(&mut output, &serde_json::to_vec(&notification).unwrap());
                }
            }
        }
        "flood-large-diagnostics" => {
            // Several large publishDiagnostics messages back-to-back with
            // no delay -- regression coverage for bounding the reader
            // task's own internal channel (see
            // docs/security-findings/rust-lsp-dev-*.md finding 3): a
            // well-behaved client must stay responsive (no hang) and
            // still end up delivering the final diagnostics even if the
            // consumer doesn't drain promptly while the flood is
            // in-flight.
            if let Some(root_uri) = &root_uri {
                let uri = format!("{}/src/main.rs", root_uri.trim_end_matches('/'));
                let diagnostic = |message: String| {
                    serde_json::json!({
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1},
                        },
                        "severity": 1,
                        "message": message,
                    })
                };
                for i in 0..30 {
                    let diagnostics: Vec<_> = (0..20_000)
                        .map(|_| diagnostic(format!("flood-large-{i}")))
                        .collect();
                    let notification = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": { "uri": uri, "diagnostics": diagnostics },
                    });
                    write_message(&mut output, &serde_json::to_vec(&notification).unwrap());
                }
            }
        }
        "diagnostics-with-spurious-id" => {
            // A spec-illegal notification (has both "method" AND a numeric
            // "id" -- notifications never carry an id) immediately
            // followed by a normal one for the same path. Regression
            // coverage for docs/security-findings/rust-lsp-dev-find-usages-
            // *.md finding 2: `handle_incoming`'s id-bearing-message branch
            // must not swallow this before the method-based dispatch runs.
            if let Some(root_uri) = &root_uri {
                let uri = format!("{}/src/main.rs", root_uri.trim_end_matches('/'));
                let malicious = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 999999,
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": uri,
                        "diagnostics": [{
                            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                            "severity": 1,
                            "message": "spurious-id-diagnostic",
                        }],
                    },
                });
                write_message(&mut output, &serde_json::to_vec(&malicious).unwrap());
            }
        }
        "bad-framing-after-init" => {
            // Not a `write_message` call: a raw, non-numeric
            // Content-Length header, which `protocol::read_message`
            // must reject as `ReadOutcome::Error` (not panic, not hang).
            output
                .write_all(b"Content-Length: not-a-number\r\n\r\n")
                .unwrap();
            output.flush().unwrap();
        }
        _ => {
            // Default: announce diagnostics of every severity for
            // `<rootUri>/src/main.rs` right away, so tests that create a
            // file at that fixed path can assert they reach
            // `LspClient::try_recv` as an `LspEvent::Diagnostics`.
            if let Some(root_uri) = &root_uri {
                let uri = format!("{}/src/main.rs", root_uri.trim_end_matches('/'));
                let diagnostic = |severity: u8, message: &str| {
                    serde_json::json!({
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1},
                        },
                        "severity": severity,
                        "message": message,
                    })
                };
                let notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": uri,
                        "diagnostics": [
                            diagnostic(1, "fixture error"),
                            diagnostic(2, "fixture warning"),
                            diagnostic(3, "fixture information"),
                            diagnostic(4, "fixture hint"),
                        ],
                    },
                });
                write_message(&mut output, &serde_json::to_vec(&notification).unwrap());
            }
        }
    }

    // Keep draining requests (didOpen/didChange/shutdown/...) until the
    // client sends `exit` or closes its end of the pipe. Responds to
    // `textDocument/references` requests with one canned location at
    // `<root_uri>/src/main.rs` -- exercises the find-usages request/
    // response correlation over a real round trip (see
    // tests/fixture_integration.rs's `references_request_receives_a_
    // correlated_response`), regardless of which scenario branch above
    // ran first.
    loop {
        match read_message(&mut input) {
            Some(body) => {
                let msg: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let method = msg.get("method").and_then(|m| m.as_str());
                if method == Some("exit") {
                    return;
                }
                if method == Some("textDocument/references") {
                    if let (Some(root_uri), Some(id)) = (&root_uri, msg.get("id").cloned()) {
                        let uri = format!("{}/src/main.rs", root_uri.trim_end_matches('/'));
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": [{
                                "uri": uri,
                                "range": {
                                    "start": {"line": 7, "character": 2},
                                    "end": {"line": 7, "character": 10},
                                },
                            }],
                        });
                        write_message(&mut output, &serde_json::to_vec(&response).unwrap());
                    }
                }
                if method == Some("textDocument/codeAction") {
                    // `context.only == ["source.organizeImports"]` is
                    // `OrganizeImports`'s own whole-document query
                    // (`docs/features/code-generation.md` §3.4) -- answered
                    // distinctly from an ordinary caret-position query so a
                    // test can tell the two request shapes apart. Returns a
                    // single entry needing `codeAction/resolve`, exercising
                    // `OrganizeImports`'s own resolve round trip (its own
                    // pending-id slot, distinct from `ApplyCodeAction`'s).
                    let only: Vec<&str> = msg["params"]["context"]["only"]
                        .as_array()
                        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    if let (Some(root_uri), Some(id)) = (&root_uri, msg.get("id").cloned()) {
                        let uri = format!("{}/src/main.rs", root_uri.trim_end_matches('/'));
                        let response = if only == ["source.organizeImports"] {
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": [
                                    { "title": "Organize imports", "data": { "marker": true } },
                                ],
                            })
                        } else {
                            // Three entries covering all three cases
                            // `docs/features/code-actions.md` §3.1
                            // describes: a bare `Command` (unsupported, no
                            // edit), a `CodeAction` with an `edit` already
                            // present (applied directly), and a
                            // `CodeAction` with `data` but no `edit` (needs
                            // `codeAction/resolve` -- only offered because
                            // this scenario's `initialize` response
                            // advertised `resolveProvider: true` above).
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": [
                                    { "title": "Bare command", "command": "noop.command" },
                                    {
                                        "title": "Direct edit",
                                        "edit": { "changes": { (uri.clone()): [{
                                            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                                            "newText": "// direct\n",
                                        }] } },
                                    },
                                    { "title": "Needs resolve", "data": { "marker": true } },
                                ],
                            })
                        };
                        write_message(&mut output, &serde_json::to_vec(&response).unwrap());
                    }
                }
                if method == Some("codeAction/resolve") {
                    if let (Some(root_uri), Some(id)) = (&root_uri, msg.get("id").cloned()) {
                        let uri = format!("{}/src/main.rs", root_uri.trim_end_matches('/'));
                        let title = msg["params"]["title"].as_str().unwrap_or_default();
                        // Distinct marker text for `OrganizeImports`'s own
                        // resolve request, so a test can tell it apart from
                        // `ApplyCodeAction`'s equally-shaped resolve.
                        let new_text = if title == "Organize imports" {
                            "// organized\n"
                        } else {
                            "// resolved\n"
                        };
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "title": title,
                                "edit": { "changes": { (uri): [{
                                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                                    "newText": new_text,
                                }] } },
                            },
                        });
                        write_message(&mut output, &serde_json::to_vec(&response).unwrap());
                    }
                }
                if method == Some("textDocument/formatting") {
                    // Only reachable when the "formatting" scenario's
                    // `initialize` response advertised
                    // `documentFormattingProvider: true` above -- returns a
                    // single deterministic `TextEdit` so the live test can
                    // assert the exact converted `WorkspaceEdit`.
                    if let Some(id) = msg.get("id").cloned() {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": [{
                                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                                "newText": "// formatted\n",
                            }],
                        });
                        write_message(&mut output, &serde_json::to_vec(&response).unwrap());
                    }
                }
                if method == Some("textDocument/rangeFormatting") {
                    // Same shape, distinct marker text so a test can tell
                    // the two request kinds' responses apart.
                    if let Some(id) = msg.get("id").cloned() {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": [{
                                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 0}},
                                "newText": "// range-formatted\n",
                            }],
                        });
                        write_message(&mut output, &serde_json::to_vec(&response).unwrap());
                    }
                }
                if method == Some("textDocument/prepareRename") {
                    // Only reachable when the "rename" scenario's
                    // `initialize` response advertised
                    // `renameProvider.prepareProvider: true` above --
                    // returns a bare `Range` (one of `PrepareRenameResponse`'s
                    // three untagged shapes), which the client reads only
                    // as "did this parse at all" (renameable: true).
                    if let Some(id) = msg.get("id").cloned() {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 7},
                            },
                        });
                        write_message(&mut output, &serde_json::to_vec(&response).unwrap());
                    }
                }
                if method == Some("textDocument/rename") {
                    // Only reachable when the "rename" scenario's
                    // `initialize` response advertised `renameProvider`
                    // above -- returns a `WorkspaceEdit` for the file the
                    // request named, so the live test can assert the exact
                    // converted edit (`docs/features/rename-refactoring.md`
                    // §3.4).
                    if let Some(id) = msg.get("id").cloned() {
                        let uri = msg["params"]["textDocument"]["uri"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "changes": { (uri): [{
                                "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7}},
                                "newText": "renamed",
                            }] } },
                        });
                        write_message(&mut output, &serde_json::to_vec(&response).unwrap());
                    }
                }
            }
            None => return,
        }
    }
}
