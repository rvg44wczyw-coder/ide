//! A minimal fake debug adapter speaking `Content-Length`-framed DAP JSON
//! over stdio, used to exercise `DapClient::start`'s real spawn +
//! event-loop code path deterministically, without depending on a real
//! adapter (`codelldb`, `debugpy`, ...) being installed on the test
//! machine.

use std::io::{self, BufRead, Write};

fn read_message(stdin: &mut impl BufRead) -> Option<serde_json::Value> {
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
    serde_json::from_slice(&body).ok()
}

fn write_message(stdout: &mut impl Write, value: &serde_json::Value) {
    let body = serde_json::to_vec(value).unwrap();
    write!(stdout, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    stdout.write_all(&body).unwrap();
    stdout.flush().unwrap();
}

fn main() {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    let mut next_seq: i64 = 1;

    // The test scenario has to be known before the very first response
    // (`initialize`'s, whose capabilities some scenarios vary) -- DAP has
    // no equivalent of LSP's `rootUri` to carry it on `initialize` itself
    // (`initialize`'s arguments are entirely internal to `ide_dap::
    // DapClient::start`, not caller-configurable). Since `DapClient::
    // start` does spawn this process with `current_dir` set to the
    // test's own per-test temp project root, a marker file dropped there
    // before spawning is the one channel available this early -- no
    // shared/global state, safe under `cargo test`'s parallel test
    // threads (mirrors `crates/lsp/tests/fixtures/fake_lsp_server.rs`'s
    // own rootUri-derived-directory-name trick, adapted to this crate's
    // different handshake shape).
    let scenario =
        std::fs::read_to_string(std::env::current_dir().unwrap().join(".fixture-scenario"))
            .unwrap_or_default();

    let Some(init) = read_message(&mut input) else {
        return;
    };
    let init_seq = init
        .get("seq")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);

    if scenario == "reject-init" {
        write_message(
            &mut output,
            &serde_json::json!({
                "seq": next_seq,
                "type": "response",
                "request_seq": init_seq,
                "success": false,
                "command": "initialize",
                "message": "fixture: initialize rejected",
            }),
        );
        return;
    }

    let capabilities = if scenario == "configuration-done" {
        serde_json::json!({ "supportsConfigurationDoneRequest": true })
    } else {
        serde_json::json!({})
    };
    write_message(
        &mut output,
        &serde_json::json!({
            "seq": next_seq,
            "type": "response",
            "request_seq": init_seq,
            "success": true,
            "command": "initialize",
            "body": capabilities,
        }),
    );
    next_seq += 1;

    if scenario == "malformed-after-init" {
        output
            .write_all(b"Content-Length: not-a-number\r\n\r\n")
            .unwrap();
        output.flush().unwrap();
        return;
    }

    // `initialized` event -- unprompted, matches a real adapter's
    // handshake timing (it may fire before or after the `launch`
    // response; this fixture fires it right away).
    write_message(
        &mut output,
        &serde_json::json!({ "seq": next_seq, "type": "event", "event": "initialized" }),
    );
    next_seq += 1;

    if scenario == "reverse-request" {
        // An adapter-initiated request the client implements nothing
        // for -- exercises the generic `{success: false}` decline over a
        // real round trip.
        write_message(
            &mut output,
            &serde_json::json!({
                "seq": next_seq,
                "type": "request",
                "command": "runInTerminal",
                "arguments": { "cwd": "/", "args": ["echo", "hi"] },
            }),
        );
        let reverse_request_seq = next_seq;
        next_seq += 1;
        if let Some(reply) = read_message(&mut input) {
            let accepted = reply["success"].as_bool().unwrap_or(true);
            let matches_seq = reply["request_seq"].as_i64() == Some(reverse_request_seq);
            write_message(
                &mut output,
                &serde_json::json!({
                    "seq": next_seq,
                    "type": "event",
                    "event": "output",
                    "body": {
                        "category": "console",
                        "output": if !accepted && matches_seq { "decline-confirmed" } else { "decline-missing" },
                    },
                }),
            );
        }
    }

    loop {
        let Some(msg) = read_message(&mut input) else {
            return;
        };
        let command = msg["command"].as_str().unwrap_or("");
        let seq = msg["seq"].as_i64().unwrap_or(0);
        match command {
            "launch" | "attach" => {
                next_seq += 1;
                write_message(
                    &mut output,
                    &serde_json::json!({
                        "seq": next_seq, "type": "response", "request_seq": seq,
                        "success": true, "command": command,
                    }),
                );
                if scenario == "stopped-on-launch" {
                    next_seq += 1;
                    write_message(
                        &mut output,
                        &serde_json::json!({
                            "seq": next_seq, "type": "event", "event": "stopped",
                            "body": { "reason": "breakpoint", "threadId": 1, "allThreadsStopped": true },
                        }),
                    );
                }
            }
            "setBreakpoints" => {
                let lines: Vec<i64> = msg["arguments"]["breakpoints"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|b| b["line"].as_i64()).collect())
                    .unwrap_or_default();
                let breakpoints: Vec<serde_json::Value> = lines
                    .iter()
                    .map(|&line| {
                        // Even lines "verify"; odd lines don't -- lets a
                        // live test assert both outcomes over one request.
                        serde_json::json!({ "line": line, "verified": line % 2 == 0 })
                    })
                    .collect();
                next_seq += 1;
                write_message(
                    &mut output,
                    &serde_json::json!({
                        "seq": next_seq, "type": "response", "request_seq": seq,
                        "success": true, "command": "setBreakpoints",
                        "body": { "breakpoints": breakpoints },
                    }),
                );
            }
            "configurationDone" => {
                // A distinct Output event announcing receipt -- lets a
                // live test prove the *negative* case too (the client
                // must never actually transmit this when the adapter's
                // `initialize` response didn't advertise
                // `supportsConfigurationDoneRequest`), not just that a
                // positive round trip works.
                next_seq += 1;
                write_message(
                    &mut output,
                    &serde_json::json!({
                        "seq": next_seq, "type": "event", "event": "output",
                        "body": { "category": "console", "output": "got-configurationDone" },
                    }),
                );
                next_seq += 1;
                write_message(
                    &mut output,
                    &serde_json::json!({
                        "seq": next_seq, "type": "response", "request_seq": seq,
                        "success": true, "command": "configurationDone",
                    }),
                );
            }
            "threads" => {
                next_seq += 1;
                if scenario == "request-fails" {
                    write_message(
                        &mut output,
                        &serde_json::json!({
                            "seq": next_seq, "type": "response", "request_seq": seq,
                            "success": false, "command": "threads",
                            "message": "fixture: threads always fails",
                        }),
                    );
                } else {
                    write_message(
                        &mut output,
                        &serde_json::json!({
                            "seq": next_seq, "type": "response", "request_seq": seq,
                            "success": true, "command": "threads",
                            "body": { "threads": [{"id": 1, "name": "main"}] },
                        }),
                    );
                }
            }
            "stackTrace" => {
                // One frame inside the project root (the fixture's own
                // cwd, which the test spawns it with), one escaping it --
                // exercises the client's path-validation rule over a
                // real round trip rather than only in a unit test.
                let cwd = std::env::current_dir().unwrap();
                let inside = cwd.join("inside.rs");
                std::fs::write(&inside, "").unwrap();
                next_seq += 1;
                write_message(
                    &mut output,
                    &serde_json::json!({
                        "seq": next_seq, "type": "response", "request_seq": seq,
                        "success": true, "command": "stackTrace",
                        "body": {
                            "stackFrames": [
                                {
                                    "id": 1, "name": "inside_root",
                                    "source": { "path": inside.to_string_lossy() },
                                    "line": 1, "column": 1,
                                },
                                {
                                    "id": 2, "name": "outside_root",
                                    "source": { "path": "/definitely/outside/the/project/root.rs" },
                                    "line": 2, "column": 1,
                                },
                            ],
                        },
                    }),
                );
            }
            "disconnect" => {
                next_seq += 1;
                write_message(
                    &mut output,
                    &serde_json::json!({
                        "seq": next_seq, "type": "response", "request_seq": seq,
                        "success": true, "command": "disconnect",
                    }),
                );
                next_seq += 1;
                write_message(
                    &mut output,
                    &serde_json::json!({ "seq": next_seq, "type": "event", "event": "terminated" }),
                );
                return;
            }
            _ => {}
        }
    }
}
