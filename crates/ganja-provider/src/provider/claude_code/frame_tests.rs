use serde_json::json;

use super::{Block, ControlRequest, Inbound, Initialize, Request, UserFrame};

/// Every inbound shape below is a frame the recording holds, copied from it
/// and reduced to the keys this wire reads. The file itself is read by
/// nothing — it is evidence a person judges — so what a test can pin is that
/// each recorded shape decodes to the value the wire acts on.
fn decoded(value: serde_json::Value) -> Inbound {
    super::decode(&value.to_string()).expect("a frame the recording holds")
}

#[test]
fn system_init_decodes_to_the_three_fields_the_wire_reads() {
    let frame = decoded(json!({
        "type": "system",
        "subtype": "init",
        "cwd": "/tmp/scratch",
        "session_id": "<sid-1>",
        "tools": ["mcp__ganja__ganja_ping"],
        "model": "claude-opus-5[1m]",
        "capabilities": ["interrupt_receipt_v1", "interrupt_cancel_queued_v1", "msg_lifecycle_v1"],
        "uuid": "<u-7>",
    }));

    let Inbound::Init(init) = frame else {
        panic!("system/init is a known frame");
    };
    assert_eq!(init.session_id, "<sid-1>");
    assert_eq!(init.model, "claude-opus-5[1m]");
    assert_eq!(init.capabilities.len(), 3);
}

/// Ten of the recording's twenty-one paid turns produced one. It is a known
/// frame, not an unknown one, because a wire that skipped it would report a
/// refused turn as a turn that produced nothing.
#[test]
fn a_vendor_safeguard_refusal_is_a_known_frame_carrying_its_own_explanation() {
    let frame = decoded(json!({
        "type": "system",
        "subtype": "model_refusal_no_fallback",
        "uuid": "<u-26>",
        "original_model": "claude-opus-5[1m]",
        "request_id": "req_011CetCuy6kSMHngZutxX8WR",
        "api_refusal_category": "reasoning_extraction",
        "api_refusal_explanation": "This request was blocked as it seems to violate Anthropic's \
             Terms of Service restrictions on reverse engineering or duplicating model outputs.",
        "refused_user_message_uuid": "<u-27>",
        "content": "",
    }));

    let Inbound::Refusal(refusal) = frame else {
        panic!("the refusal is a known frame");
    };
    assert_eq!(refusal.category, "reasoning_extraction");
    assert_eq!(refusal.original_model, "claude-opus-5[1m]");
    assert!(
        refusal.explanation.contains("duplicating model outputs"),
        "the explanation travels verbatim: a paraphrase of a vendor's compliance sentence is \
         not a thing to read second-hand"
    );
}

/// The pair a wire reading `subtype` gets wrong on eight of sixteen runs.
#[test]
fn a_result_is_read_by_is_error_and_never_by_subtype() {
    let refused = decoded(json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "stop_reason": "refusal",
        "result": "API Error",
        "usage": {"input_tokens": 0, "output_tokens": 0},
    }));
    let Inbound::Result(refused) = refused else {
        panic!("a result is a known frame");
    };
    assert!(refused.is_error, "`subtype: success` with `is_error: true` is a failure");

    // And the mirror: a subtype that reads like a failure with `is_error`
    // false is a turn that finished.
    let capped = decoded(json!({
        "type": "result",
        "subtype": "error_max_turns",
        "is_error": false,
        "result": "done",
        "usage": {},
    }));
    let Inbound::Result(capped) = capped else {
        panic!("a result is a known frame");
    };
    assert!(!capped.is_error);
}

#[test]
fn a_results_four_counters_map_onto_the_wires_own_names() {
    let frame = decoded(json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "pong",
        "usage": {
            "input_tokens": 4,
            "cache_creation_input_tokens": 3781,
            "cache_read_input_tokens": 3679,
            "output_tokens": 277,
        },
    }));

    let Inbound::Result(result) = frame else {
        panic!("a result is a known frame");
    };
    assert_eq!(result.usage.input, 4);
    assert_eq!(result.usage.output, 277);
    assert_eq!(result.usage.cache_read, 3679);
    assert_eq!(result.usage.cache_write, 3781);
    assert_eq!(result.text, "pong");
}

#[test]
fn an_assistant_frames_three_block_kinds_decode() {
    let frame = decoded(json!({
        "type": "assistant",
        "message": {"content": [
            {"type": "text", "text": "pong"},
            {"type": "thinking", "thinking": "", "signature": "<sig>"},
            {"type": "tool_use", "id": "toolu_1", "name": "mcp__ganja__ganja_ping", "input": {}},
            {"type": "something_new"},
        ]},
    }));

    let Inbound::Assistant(blocks) = frame else {
        panic!("an assistant frame is a known frame");
    };
    assert_eq!(blocks.len(), 3, "a block kind this build does not know is skipped, not fatal");
    assert_eq!(blocks[0], Block::Text("pong".to_owned()));
    assert_eq!(
        blocks[1],
        Block::Thinking(String::new()),
        "under no --thinking-display the text is withheld and only the signature arrives"
    );
    assert!(matches!(&blocks[2], Block::ToolUse { id, .. } if id == "toolu_1"));
}

#[test]
fn a_can_use_tool_carries_the_id_its_call_is_matched_back_by() {
    let frame = decoded(json!({
        "type": "control_request",
        "request_id": "<u-10>",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "mcp__ganja__ganja_ping",
            "display_name": "Ganja Ping",
            "input": {},
            "permission_suggestions": [],
            "tool_use_id": "toolu_012wBWYYsLVb2FBinW2xhSgp",
        },
    }));

    let Inbound::ControlRequest { request_id, request } = frame else {
        panic!("a control request is a known frame");
    };
    assert_eq!(request_id, "<u-10>");
    let Request::CanUseTool { tool_name, tool_use_id, .. } = request else {
        panic!("can_use_tool is a known subtype");
    };
    assert_eq!(tool_name, "mcp__ganja__ganja_ping");
    assert_eq!(tool_use_id, "toolu_012wBWYYsLVb2FBinW2xhSgp");
}

#[test]
fn an_mcp_message_carries_the_server_it_is_addressed_to() {
    let frame = decoded(json!({
        "type": "control_request",
        "request_id": "<u-11>",
        "request": {
            "subtype": "mcp_message",
            "server_name": "ganja",
            "message": {"method": "tools/list", "jsonrpc": "2.0", "id": 1},
        },
    }));

    let Inbound::ControlRequest { request, .. } = frame else {
        panic!("a control request is a known frame");
    };
    let Request::Mcp { server_name, message } = request else {
        panic!("mcp_message is a known subtype");
    };
    assert_eq!(server_name, "ganja");
    assert_eq!(message["method"], "tools/list");
}

/// Never silence: a `control_request` the CLI is waiting on does not time
/// out, so an unanswered one hangs the turn where a named refusal ends it
/// readably.
#[test]
fn a_control_request_subtype_this_build_does_not_know_is_named_rather_than_ignored() {
    let frame = decoded(json!({
        "type": "control_request",
        "request_id": "<u-99>",
        "request": {"subtype": "some_future_ask"},
    }));

    let Inbound::ControlRequest { request, .. } = frame else {
        panic!("a control request is a known frame");
    };
    assert_eq!(request, Request::Unknown { subtype: "some_future_ask".to_owned() });

    let line =
        super::control_error_line("<u-99>", "this build does not implement `some_future_ask`");
    let sent: serde_json::Value = serde_json::from_str(line.trim()).expect("a JSON line");
    assert_eq!(sent["response"]["subtype"], "error");
    assert_eq!(sent["response"]["request_id"], "<u-99>");
    assert!(
        sent["response"]["error"].as_str().is_some_and(|said| said.contains("some_future_ask"))
    );
}

#[test]
fn a_system_subtype_this_build_does_not_know_is_unknown_and_the_two_it_does_are_not() {
    assert_eq!(
        decoded(json!({"type": "system", "subtype": "invented_for_this_test"})),
        Inbound::Unknown {
            kind: "system".to_owned(),
            subtype: Some("invented_for_this_test".to_owned()),
        }
    );

    // Known-and-skipped, so the unknown-subtype log line stays a signal.
    for subtype in ["thinking_tokens", "status"] {
        assert_eq!(
            decoded(json!({"type": "system", "subtype": subtype, "estimated_tokens": 12})),
            Inbound::KnownSystem { subtype: subtype.to_owned() }
        );
    }
}

/// The fixture's own `assistant` frames carry a reduced key set — the first
/// W2 lane omitted `timestamp`, `request_id` and `tool_use_meta`, which the
/// CLI does emit — so a decoder that insisted on the whole set would break on
/// exactly the keys the fixture cannot under-test.
#[test]
fn a_frame_carrying_fields_this_build_does_not_read_still_decodes() {
    let frame = decoded(json!({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "pong"}]},
        "session_id": "<sid-1>",
        "uuid": "<u-18>",
        "timestamp": "2026-09-09T16:47:22.876Z",
        "request_id": "req_011Cet",
        "tool_use_meta": {"anything": true},
        "parent_tool_use_id": null,
        "a_field_no_release_has_yet": 7,
    }));

    assert_eq!(frame, Inbound::Assistant(vec![Block::Text("pong".to_owned())]));
}

#[test]
fn a_rate_limit_event_carries_the_vendors_own_window_object() {
    let frame = decoded(json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": "allowed",
            "resetsAt": 1_788_982_800_u64,
            "rateLimitType": "five_hour",
            "overageStatus": "rejected",
            "unifiedWindows": {"five_hour": {"utilization": 0.68, "resetsAt": 1_788_982_800_u64}},
        },
        "uuid": "<u-12>",
    }));

    let Inbound::RateLimit(info) = frame else {
        panic!("a rate limit event is a known frame");
    };
    assert_eq!(info["status"], "allowed");
    assert_eq!(info["unifiedWindows"]["five_hour"]["utilization"], 0.68);
}

/// `{isAuthenticating: false, output: []}` on every run of the recording, a
/// fully logged-in CLI included — so this frame says whether a login is in
/// flight and never whether a credential exists.
#[test]
fn auth_status_decodes_and_says_nothing_about_whether_a_credential_exists() {
    assert_eq!(
        decoded(json!({"type": "auth_status", "isAuthenticating": false, "output": []})),
        Inbound::AuthStatus { is_authenticating: false, output: Vec::new() }
    );
}

#[test]
fn a_user_frame_says_whether_it_is_the_clis_own_echo() {
    assert_eq!(
        decoded(json!({"type": "user", "message": {}, "isReplay": true})),
        Inbound::User { is_replay: true }
    );
    assert_eq!(
        decoded(json!({"type": "user", "message": {}})),
        Inbound::User { is_replay: false },
        "a tool_result frame is a real conversation frame, not a replay ack"
    );
}

#[test]
fn a_frame_type_this_build_does_not_know_is_skipped_by_name() {
    assert_eq!(
        decoded(json!({"type": "some_future_frame"})),
        Inbound::Unknown { kind: "some_future_frame".to_owned(), subtype: None }
    );
}

#[test]
fn a_line_that_is_not_json_is_reported_rather_than_read_past() {
    let error = super::decode("not a frame at all").expect_err("a line that is not JSON");

    assert!(error.contains("not a frame at all"), "the line is named, so a person can see it");
}

// ------------------------------------------------------------- outbound

/// A record on the CLI's own preset sends **no** `systemPrompt` key: a
/// `null` there would be this side asserting something about a field the
/// recording never carries.
#[test]
fn a_record_on_the_clis_own_preset_sends_no_system_prompt_key() {
    let line = super::initialize_line(
        "req-1",
        &Initialize {
            system_prompt: None,
            sdk_mcp_servers: vec!["ganja".to_owned()],
            sdk_mcp_server_configs: json!({"ganja": {"timeout": 3_600_000}}),
        },
    );
    let sent: serde_json::Value = serde_json::from_str(line.trim()).expect("a JSON line");

    assert_eq!(sent["request"]["subtype"], "initialize");
    assert!(
        sent["request"].get("systemPrompt").is_none(),
        "absent, never null: {}",
        sent["request"]
    );
    assert_eq!(sent["request"]["sdkMcpServers"], json!(["ganja"]));
    assert_eq!(sent["request"]["sdkMcpServerConfigs"]["ganja"]["timeout"], 3_600_000);
}

#[test]
fn a_replaced_prompt_rides_the_initialize_as_a_list() {
    let line = super::initialize_line(
        "req-1",
        &Initialize {
            system_prompt: Some(vec!["you are ganja".to_owned()]),
            sdk_mcp_servers: Vec::new(),
            sdk_mcp_server_configs: json!({}),
        },
    );
    let sent: serde_json::Value = serde_json::from_str(line.trim()).expect("a JSON line");

    assert_eq!(sent["request"]["systemPrompt"], json!(["you are ganja"]));
}

#[test]
fn a_user_frame_is_one_line_and_never_a_subagents() {
    let line =
        super::user_line(&UserFrame { content: "hello".to_owned(), parent_tool_use_id: None });

    assert!(line.ends_with('\n'), "the CLI reads a frame per line");
    assert_eq!(line.matches('\n').count(), 1);

    let sent: serde_json::Value = serde_json::from_str(line.trim()).expect("a JSON line");
    assert_eq!(sent["type"], "user");
    assert_eq!(sent["message"]["role"], "user");
    assert_eq!(sent["message"]["content"], "hello");
    assert_eq!(sent["parent_tool_use_id"], serde_json::Value::Null);
}

#[test]
fn the_two_control_requests_this_wire_makes_are_spelled_the_way_the_cli_reads_them() {
    assert_eq!(ControlRequest::Interrupt.subtype(), "interrupt");
    assert_eq!(ControlRequest::GetUsage.subtype(), "get_usage");

    let line = super::control_request_line("req-9", ControlRequest::Interrupt);
    let sent: serde_json::Value = serde_json::from_str(line.trim()).expect("a JSON line");
    assert_eq!(sent["request"]["subtype"], "interrupt");
    assert_eq!(sent["request_id"], "req-9");
}

#[test]
fn a_success_answer_carries_the_id_the_cli_will_echo_it_under() {
    let line = super::control_response_line("<u-10>", &json!({"behavior": "allow"}));
    let sent: serde_json::Value = serde_json::from_str(line.trim()).expect("a JSON line");

    assert_eq!(sent["type"], "control_response");
    assert_eq!(sent["response"]["subtype"], "success");
    assert_eq!(sent["response"]["request_id"], "<u-10>");
    assert_eq!(sent["response"]["response"]["behavior"], "allow");
}

/// The echo, decoded: it carries the id **this side** minted, which is the
/// whole of how a wire tells one from an answer of its own.
#[test]
fn the_clis_echo_of_an_answer_decodes_as_a_control_response_under_that_answers_id() {
    let sent = super::control_response_line("<u-10>", &json!({"behavior": "allow"}));
    let echoed = decoded(serde_json::from_str(sent.trim()).expect("a JSON line"));

    assert_eq!(
        echoed,
        Inbound::ControlResponse {
            request_id: "<u-10>".to_owned(),
            response: Some(json!({"behavior": "allow"})),
            error: None,
        }
    );
}

#[test]
fn an_error_answer_decodes_with_its_reason_and_no_payload() {
    let sent = super::control_error_line("<u-99>", "no such thing");
    let read = decoded(serde_json::from_str(sent.trim()).expect("a JSON line"));

    assert_eq!(
        read,
        Inbound::ControlResponse {
            request_id: "<u-99>".to_owned(),
            response: None,
            error: Some("no such thing".to_owned()),
        }
    );
}

// ------------------------------------------------------ the replay fixture

/// Run 1's inbound frames, derived from the recording under the same scrub.
///
/// The recording itself (`claude-code-sdk-mcp-probe.txt`) is read by
/// **nothing** — it is evidence a person judges, and a test that read it
/// could be made green by editing it. This is the derived artefact a test may
/// read: the same frames, as JSON, so a decoder change that broke one of them
/// reddens here.
const RUN_1: &str = include_str!("../../../tests/fixtures/claude-code-replay-run1.json");

/// AC-3.3. Every frame, and none of them `Unknown` — an unknown one would be
/// a frame the wire silently skipped on a run it was read from.
#[test]
fn every_inbound_frame_of_run_one_decodes_to_something_this_build_knows() {
    let frames: Vec<serde_json::Value> =
        serde_json::from_str(RUN_1).expect("the replay fixture is a JSON array");

    assert!(frames.len() >= 20, "run 1 is the deepest turn the recording holds: {}", frames.len());

    for frame in &frames {
        let read = super::read(frame);
        assert!(
            !matches!(read, Inbound::Unknown { .. }),
            "a frame from the run this wire was read from is unknown: {frame}"
        );
    }
}

/// The kinds that run actually produced, so a decoder that stopped
/// recognising one is named rather than merely counted.
#[test]
fn the_replay_fixture_carries_every_kind_the_wire_acts_on() {
    let frames: Vec<serde_json::Value> =
        serde_json::from_str(RUN_1).expect("the replay fixture is a JSON array");
    let read: Vec<Inbound> = frames.iter().map(super::read).collect();

    let has = |what: fn(&Inbound) -> bool| read.iter().any(what);

    assert!(has(|frame| matches!(frame, Inbound::Init(_))), "system/init");
    assert!(has(|frame| matches!(frame, Inbound::Refusal(_))), "the vendor safeguard's refusal");
    assert!(has(|frame| matches!(frame, Inbound::Assistant(_))), "assistant");
    assert!(has(|frame| matches!(frame, Inbound::Result(_))), "result");
    assert!(has(|frame| matches!(frame, Inbound::ControlRequest { .. })), "control_request");
    assert!(has(|frame| matches!(frame, Inbound::ControlResponse { .. })), "the echo");
    assert!(has(|frame| matches!(frame, Inbound::RateLimit(_))), "rate_limit_event");
    assert!(has(|frame| matches!(frame, Inbound::AuthStatus { .. })), "auth_status");
    assert!(has(|frame| matches!(frame, Inbound::User { .. })), "user");
}

/// The `system/init` at the head of the run's **second** turn differs from the
/// first in `uuid` alone, so nothing may treat one as first.
#[test]
fn the_run_re_emits_system_init_at_the_head_of_every_turn() {
    let frames: Vec<serde_json::Value> =
        serde_json::from_str(RUN_1).expect("the replay fixture is a JSON array");
    let inits: Vec<Inbound> =
        frames.iter().map(super::read).filter(|frame| matches!(frame, Inbound::Init(_))).collect();

    assert!(inits.len() >= 2, "run 1 took three turns: {}", inits.len());
    assert_eq!(inits[0], inits[1], "the wire reads the same three fields from each");
}

// ------------------------------------------------------- bounded reading

/// An error is a thing that gets logged and carried around, and a frame this
/// side cannot read may be megabytes of somebody else's output — so the error
/// describes the line rather than embedding it (CC-12).
#[test]
fn a_decode_failure_names_the_lines_length_and_its_first_bytes_and_not_the_line() {
    let line = format!("not json {}", "x".repeat(100_000));

    let refused = super::decode(&line).expect_err("that is not a frame");

    assert!(refused.contains("100009 bytes"), "the length is not named: {refused}");
    assert!(refused.contains("not json xxx"), "nothing of it is quoted: {refused}");
    assert!(refused.len() < 400, "the whole line came along: {} bytes", refused.len());
}

/// The quote is cut by **characters**, so a multi-byte line does not panic on
/// a byte boundary.
#[test]
fn the_quote_of_an_unreadable_line_cuts_on_a_character_boundary() {
    let line = "あ".repeat(1_000);

    let refused = super::decode(&line).expect_err("that is not a frame");

    assert!(refused.contains('…'), "the cut is admitted: {refused}");
}

/// `lines()` allocates whatever one line contains, and these bytes are the
/// peer's: a frame that never ended would be read into memory until the
/// machine gave out (CC-12).
#[tokio::test]
async fn a_line_past_the_bound_is_reported_by_length_and_the_next_line_still_reads() {
    // A bound this test can reach: the shipped one is 16 MiB, and what is
    // being proved is the arithmetic rather than the number.
    let over = "x".repeat(super::MAX_LINE + 10);
    let feed = format!("{over}\n{{\"type\":\"system\",\"subtype\":\"init\"}}\n");
    let mut lines = super::Lines::new(std::io::Cursor::new(feed.into_bytes()));

    assert_eq!(
        lines.next().await.expect("a read"),
        Some(super::Line::TooLong(super::MAX_LINE + 10)),
        "the whole length is reported, not what was kept"
    );
    assert!(
        matches!(lines.next().await.expect("a read"), Some(super::Line::Read(line)) if line.contains("init")),
        "and the frame after it is read normally"
    );
    assert_eq!(lines.next().await.expect("a read"), None, "then EOF");
}

/// A child that dies mid-frame has still said what it said.
#[tokio::test]
async fn a_last_line_with_no_newline_is_still_a_line() {
    let mut lines = super::Lines::new(std::io::Cursor::new(b"{\"type\":\"result\"}".to_vec()));

    assert_eq!(
        lines.next().await.expect("a read"),
        Some(super::Line::Read("{\"type\":\"result\"}".to_owned()))
    );
    assert_eq!(lines.next().await.expect("a read"), None);
}

/// Two frames in one buffer, and an empty line between them: the reader is
/// the frame boundary, so it has to agree with `lines()` on the ordinary
/// cases it replaced.
#[tokio::test]
async fn the_bounded_reader_splits_where_the_newlines_are() {
    let mut lines = super::Lines::new(std::io::Cursor::new(b"one\n\ntwo\n".to_vec()));

    assert_eq!(lines.next().await.expect("a read"), Some(super::Line::Read("one".to_owned())));
    assert_eq!(lines.next().await.expect("a read"), Some(super::Line::Read(String::new())));
    assert_eq!(lines.next().await.expect("a read"), Some(super::Line::Read("two".to_owned())));
    assert_eq!(lines.next().await.expect("a read"), None);
}
