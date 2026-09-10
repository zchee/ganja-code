use super::{DEFAULT_VERSION, NEEDS_VERBOSE, Record, Script, Turn, refuse, replay};

fn argv(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|token| (*token).to_owned()).collect()
}

/// A double that accepted everything would let a regression in the argv
/// builder pass every suite.
#[test]
fn an_argv_without_verbose_is_refused_with_the_clis_own_sentence() {
    assert_eq!(refuse(&argv(&["-p", "--output-format", "stream-json"])), Some(1));
    assert!(NEEDS_VERBOSE.contains("requires --verbose"));
}

/// Posture C, enforced by the double as well as by the builders — two
/// independent statements rather than one restated.
#[test]
fn resume_is_refused_the_way_the_cli_refuses_an_option_it_does_not_have() {
    assert_eq!(refuse(&argv(&["--verbose", "--resume", "<sid>"])), Some(1));
}

#[test]
fn a_permission_mode_other_than_manual_is_refused() {
    assert_eq!(refuse(&argv(&["--verbose", "--permission-mode", "bypassPermissions"])), Some(1));
    assert_eq!(refuse(&argv(&["--verbose", "--permission-mode", "acceptEdits"])), Some(1));
    assert_eq!(refuse(&argv(&["--verbose", "--permission-mode", "manual"])), None);
}

#[test]
fn the_argv_the_wire_actually_builds_is_accepted() {
    let mut tokens = argv(&["-p", "--verbose", "--permission-mode", "manual"]);
    tokens.push("--session-id".to_owned());
    tokens.push("01998a00-0000-7000-8000-00000000000a".to_owned());

    assert_eq!(refuse(&tokens), None);
}

/// The wire runs `--version` at construction, so a double that did not answer
/// it would fail every suite before a frame.
#[test]
fn the_default_version_is_the_floor_the_wire_was_read_from() {
    assert_eq!(DEFAULT_VERSION, "2.1.263 (Claude Code)");
}

/// The whole conversation, in-process: the dial, one turn, and the record it
/// leaves.
#[tokio::test]
async fn a_scripted_turn_dials_answers_and_records_what_it_saw() {
    let script = Script {
        turns: vec![Turn {
            text: vec!["pong".to_owned()],
            result: "pong".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    };
    let record =
        std::sync::Mutex::new(Record { session_id: "<sid-1>".to_owned(), ..Record::default() });

    // The wire's side of the two pipes, driven by hand: an `initialize` that
    // declares no server (so nothing dials), then one prompt.
    let (mut wire_stdin, cli_stdin) = tokio::io::duplex(1 << 16);
    let (cli_stdout, wire_stdout) = tokio::io::duplex(1 << 16);

    let played = tokio::spawn(async move { replay(cli_stdin, cli_stdout, &script, &record).await });

    {
        use tokio::io::AsyncWriteExt as _;

        wire_stdin
            .write_all(
                b"{\"type\":\"control_request\",\"request_id\":\"r1\",\"request\":{\"subtype\":\"initialize\",\"sdkMcpServers\":[]}}\n\
                  {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"ping\"}}\n",
            )
            .await
            .expect("the wire writes");
        wire_stdin.flush().await.expect("it flushes");
    }

    // Read until the turn's `result`.
    let mut lines = {
        use tokio::io::AsyncBufReadExt as _;

        tokio::io::BufReader::new(wire_stdout).lines()
    };
    let mut kinds = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let frame: serde_json::Value = serde_json::from_str(&line).expect("a JSON line");
        let kind = frame["type"].as_str().unwrap_or_default().to_owned();
        let done = kind == "result";
        kinds.push(kind);
        if done {
            break;
        }
    }

    drop(wire_stdin);
    let code = tokio::time::timeout(std::time::Duration::from_secs(5), played)
        .await
        .expect("the fake ends on EOF")
        .expect("the task did not panic");

    assert_eq!(code, 0, "a served turn exits 0");
    assert!(kinds.contains(&"system".to_owned()), "system/init opens every turn: {kinds:?}");
    assert!(kinds.contains(&"assistant".to_owned()));
    assert_eq!(kinds.last().map(String::as_str), Some("result"));
}

/// The exit code is the last `result`'s `is_error`, which is what makes a
/// refused turn exit 1.
#[tokio::test]
async fn a_refused_turn_exits_one() {
    let script =
        Script { turns: vec![Turn { refused: true, ..Turn::default() }], ..Script::default() };
    let record = std::sync::Mutex::new(Record::default());

    let (mut wire_stdin, cli_stdin) = tokio::io::duplex(1 << 16);
    let (cli_stdout, wire_stdout) = tokio::io::duplex(1 << 16);
    let played = tokio::spawn(async move { replay(cli_stdin, cli_stdout, &script, &record).await });

    {
        use tokio::io::AsyncWriteExt as _;

        wire_stdin
            .write_all(
                b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"ping\"}}\n",
            )
            .await
            .expect("the wire writes");
        wire_stdin.flush().await.expect("it flushes");
    }

    // Drain what it says, then EOF.
    let mut lines = {
        use tokio::io::AsyncBufReadExt as _;

        tokio::io::BufReader::new(wire_stdout).lines()
    };
    let mut saw_refusal = false;
    while let Ok(Some(line)) = lines.next_line().await {
        let frame: serde_json::Value = serde_json::from_str(&line).expect("a JSON line");
        if frame["subtype"] == "model_refusal_no_fallback" {
            saw_refusal = true;
            assert_eq!(frame["api_refusal_category"], "reasoning_extraction");
        }
        if frame["type"] == "result" {
            assert_eq!(frame["subtype"], "success", "the pair a wire reading subtype gets wrong");
            assert_eq!(frame["is_error"], true);
            break;
        }
    }

    drop(wire_stdin);
    let code = tokio::time::timeout(std::time::Duration::from_secs(5), played)
        .await
        .expect("the fake ends on EOF")
        .expect("the task did not panic");

    assert!(saw_refusal);
    assert_eq!(code, 1);
}

/// **D556**, Dv-18. A script that names models makes the `initialize` reply
/// carry them in the CLI's own shape, and a script that names none makes it
/// carry **no `models` key at all**.
///
/// The omission is the half worth pinning: every suite written before this
/// field exists asserts on a reply that has no such key, and a fake that
/// started sending an empty array would be describing a seat that may name
/// nothing.
#[tokio::test]
async fn the_initialize_reply_carries_models_only_when_a_script_names_them() {
    for named in [true, false] {
        let script = Script {
            models: if named {
                vec![
                    ("default".to_owned(), "Default (recommended)".to_owned()),
                    ("opus".to_owned(), "Opus".to_owned()),
                ]
            } else {
                Vec::new()
            },
            ..Script::default()
        };
        let reply = initialize_reply(&script).await;

        if named {
            assert_eq!(
                reply["models"],
                serde_json::json!([
                    {"value": "default", "displayName": "Default (recommended)"},
                    {"value": "opus", "displayName": "Opus"},
                ]),
                "the CLI's own per-entry shape, in the script's order"
            );
        } else {
            assert!(
                reply.get("models").is_none(),
                "a script naming none leaves the key off entirely: {reply}"
            );
        }
        // Untouched either way, so the addition is additive.
        assert_eq!(reply["output_style"], "default");
        assert_eq!(reply["current_permission_mode"], "default");
    }
}

/// Drives one fake far enough to read its answer to an `initialize`.
async fn initialize_reply(script: &Script) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    let record = std::sync::Mutex::new(Record::default());
    let (mut wire_stdin, cli_stdin) = tokio::io::duplex(1 << 16);
    let (cli_stdout, wire_stdout) = tokio::io::duplex(1 << 16);
    let script = script.clone();
    let played = tokio::spawn(async move { replay(cli_stdin, cli_stdout, &script, &record).await });

    wire_stdin
        .write_all(
            b"{\"type\":\"control_request\",\"request_id\":\"r1\",\"request\":{\"subtype\":\"initialize\",\"sdkMcpServers\":[]}}\n",
        )
        .await
        .expect("the wire writes");
    wire_stdin.flush().await.expect("it flushes");

    let mut lines = tokio::io::BufReader::new(wire_stdout).lines();
    let answer = loop {
        let line = lines.next_line().await.expect("the fake answers").expect("before it ends");
        let frame: serde_json::Value = serde_json::from_str(&line).expect("a JSON line");
        if frame["type"] == "control_response" {
            break frame["response"]["response"].clone();
        }
    };

    drop(wire_stdin);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), played).await;

    answer
}
