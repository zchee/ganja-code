use std::path::{Path, PathBuf};

use ganja_core::teammate::SpawnSpec;
use ganja_team::{MemberName, TeamName, TeamsRoot};

use super::*;
use crate::shim;

/// A spawn to compose against. Nothing in an argv reads any of it — which
/// is itself the point of **AC-21**, and is why this can be one value.
fn spec() -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse("w1").expect("a member name"),
        team: TeamName::default_team(),
        lead: MemberName::lead(),
        root: TeamsRoot::new(PathBuf::from("/nonexistent/teams")),
        backend: MemberBackend::Grok,
        agent_type: "general".to_owned(),
        model: "whatever-the-person-configured".to_owned(),
        color: "blue".to_owned(),
        prompt: "the spawn prompt, which travels through the mailbox".to_owned(),
        cwd: PathBuf::from("/nonexistent/work"),
        plan_mode_required: false,
        parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
    }
}

/// The argv for a turn that has, or has not, a conversation to resume.
fn argv(session: Option<&str>) -> Vec<String> {
    let spec = spec();
    Grok.argv(&Turn {
        spec: &spec,
        text: "a teammate's words, which never reach a command line",
        prompt: Some(Path::new("/tmp/ganja-shim-xyz/prompt.txt")),
        session,
        deadline: shim::GROK_TURN_TIMEOUT,
    })
    .iter()
    .map(|token| token.to_string_lossy().into_owned())
    .collect()
}

/// The token after `flag`, if the argv carries one.
fn value(argv: &[String], flag: &str) -> Option<String> {
    argv.iter().position(|token| token == flag).and_then(|at| argv.get(at + 1)).cloned()
}

#[test]
fn a_first_turn_mints_the_conversation_it_is_creating() {
    let argv = argv(None);

    assert_eq!(
        argv.iter()
            .map(String::as_str)
            .filter(|token| *token != value(&argv, "--session-id").unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![
            "--prompt-file",
            "/tmp/ganja-shim-xyz/prompt.txt",
            "--session-id",
            "--sandbox",
            "read-only",
            "--permission-mode",
            "dontAsk",
            "--output-format",
            "streaming-messages-json",
            "--include-partial-messages",
        ]
    );
    let minted = value(&argv, "--session-id").expect("a first turn names its own session");
    assert!(
        ganja_protocol::is_uuidv7(&minted),
        "a UUID-shaped id is what makes `--resume` mean an id rather than a title: {minted}"
    );
    assert!(
        !argv.iter().any(|token| token == "--resume"),
        "a first turn resumes nothing: {argv:?}"
    );
}

#[test]
fn two_first_turns_never_propose_one_conversation() {
    // **AC-19** at the composition level: `argv` is called once per turn,
    // so two members' first turns are two calls and must be two ids. The
    // end-to-end half of this is in `teammate_shim_grok.rs`.
    let first = value(&argv(None), "--session-id").expect("an id");
    let second = value(&argv(None), "--session-id").expect("an id");

    assert_ne!(first, second);
}

#[test]
fn a_resume_turn_names_the_conversation_and_repeats_the_posture() {
    let id = "01998ad0-0000-7000-8000-000000000000";

    assert_eq!(
        argv(Some(id)),
        vec![
            "--resume",
            id,
            "--prompt-file",
            "/tmp/ganja-shim-xyz/prompt.txt",
            "--permission-mode",
            "dontAsk",
            "--sandbox",
            "read-only",
            "--output-format",
            "streaming-messages-json",
            "--include-partial-messages",
        ]
    );
    assert!(
        !argv(Some(id)).iter().any(|token| token == "--session-id"),
        "`--session-id` is for a new conversation and does not resume"
    );
}

#[test]
fn the_sandbox_value_is_the_exact_byte_string_the_builtin_answers_to() {
    // `--sandbox` is unvalidated at clap, so an unrecognized value becomes
    // a *custom* profile that fails to load and hard-exits the child.
    // Measured on 1.0.6: `read_only` refuses naming `'read_only'`, where
    // `readonly` normalizes and refuses naming `'read-only'`. This is the
    // spelling that neither.
    assert_eq!(SANDBOX_VALUE, "read-only");
    for session in [None, Some("01998ad0-0000-7000-8000-000000000000")] {
        let argv = argv(session);
        assert_eq!(
            value(&argv, "--sandbox").as_deref(),
            Some("read-only"),
            "the bound is pinned on every turn: {argv:?}"
        );
        assert_eq!(
            value(&argv, "--permission-mode").as_deref(),
            Some("dontAsk"),
            "and the mode beside it: {argv:?}"
        );
    }
}

#[test]
fn no_never_composed_spelling_reaches_either_argv() {
    // Iterated rather than re-listed: [`NEVER_COMPOSED`] is the single
    // source, so a flag added to it is a flag this assertion picks up.
    for session in [None, Some("01998ad0-0000-7000-8000-000000000000")] {
        let argv = argv(session);
        for refused in NEVER_COMPOSED {
            assert!(
                !argv.iter().any(|token| token == refused),
                "{refused} must never be composed, and is in {argv:?}"
            );
        }
    }
}

#[test]
fn no_prompt_text_is_ever_on_a_command_line() {
    let spec = spec();
    let secret = "the words a peer said, which argv is world-readable through ps";
    let argv = Grok.argv(&Turn {
        spec: &spec,
        text: secret,
        prompt: Some(Path::new("/tmp/ganja-shim-xyz/prompt.txt")),
        session: None,
        deadline: shim::GROK_TURN_TIMEOUT,
    });

    assert!(
        !argv.iter().any(|token| token.to_string_lossy().contains(secret)),
        "argv is for flags; `--prompt-file` is what says where the prompt is"
    );
    assert_eq!(Grok.door(), Door::File);
}

#[test]
fn the_environment_carries_no_door_onto_the_posture() {
    // Empty rather than short: every flag this CLI needs is on the command
    // line, and `GROK_SANDBOX` is `--sandbox`'s own documented environment
    // source — carrying any `GROK_*` name would hand a person's exported
    // variable the posture they consented to at spawn.
    assert!(Grok.additions().is_empty());
    assert!(!Grok.additions().iter().any(|name| name.starts_with("GROK_")));
}

/// The shapes a probed `grok 1.0.6` actually printed, for a turn that
/// answered.
fn answered() -> String {
    [
            r#"{"type":"system","subtype":"init","session_id":"1c2f16a6-c5ed-4d60-9167-f34374890a6f","apiKeySource":"oauth","model":"grok-4.6","permissionMode":"dontAsk"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_0","role":"assistant","content":[]}},"session_id":"1c2f16a6-c5ed-4d60-9167-f34374890a6f"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"HELLO"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":41}}}"#,
            r#"{"type":"assistant","message":{"id":"msg_0","role":"assistant","content":[{"type":"thinking","thinking":"not mail","signature":"x"},{"type":"text","text":"HELLO"}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":3186,"num_turns":1,"result":"HELLO","stop_reason":"end_turn"}"#,
        ]
        .join("\n")
}

#[test]
fn a_turn_that_answered_is_one_mail_and_the_session_it_ran_in() {
    let reply = Grok.reply(&answered()).expect("a turn that answered");

    assert_eq!(reply.messages, vec!["HELLO"]);
    assert_eq!(
        reply.session.as_deref(),
        Some("1c2f16a6-c5ed-4d60-9167-f34374890a6f"),
        "the id a later turn resumes is the one the child said it was running"
    );
}

#[test]
fn thinking_is_not_a_teammate_talking() {
    let reply = Grok.reply(&answered()).expect("a turn that answered");

    assert!(!reply.messages.iter().any(|text| text.contains("not mail")), "{:?}", reply.messages);
}

#[test]
fn only_the_final_message_becomes_mail() {
    // A turn that runs tools says several things on its way to an answer.
    // The lead is owed the answer, not the narration — and the vendor's own
    // `result` is the strongest statement of which one that is.
    let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"let me look"},{"type":"tool_use","id":"t1","name":"hashline_read","input":{}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"it says four things"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"it says four things","stop_reason":"end_turn"}"#,
        ]
        .join("\n");

    let reply = Grok.reply(&stdout).expect("a turn that answered");

    assert_eq!(reply.messages, vec!["it says four things"]);
}

#[test]
fn a_cancelled_turn_says_so_in_words_and_keeps_the_conversation() {
    // **The measured shape**, byte for byte what a probed 1.0.6 printed for
    // a turn whose `write` nothing approved: `stop_reason: "cancelled"` and
    // a one-word `errors: ["cancelled"]`, on a *zero* exit.
    //
    // Two things have to be true of the answer and neither is obvious. It
    // is not an `Err`, because this build read the stream perfectly and a
    // refusal that reads as unreadable output is a refusal nobody acts on;
    // and the session survives, because a cancelled turn is a live
    // conversation the next message should resume rather than a second one
    // to start.
    let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"write"}}}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["cancelled"],"stop_reason":"cancelled","num_turns":1}"#,
        ]
        .join("\n");

    let reply = Grok.reply(&stdout).expect("a cancelled turn is readable");
    let refused = reply.refused.expect("and it says why there is no answer");

    assert!(refused.contains("cancelled this turn"), "{refused}");
    assert!(refused.contains("`write`"), "and which tool: {refused}");
    assert!(
        refused.contains("Reading takes no approval"),
        "and what still works, which is the whole of what a grok teammate is \
             for: {refused}"
    );
    assert_eq!(reply.session.as_deref(), Some("s-1"));
    assert!(reply.messages.is_empty(), "{:?}", reply.messages);
}

#[test]
fn a_tool_named_only_in_the_partial_stream_is_still_named() {
    // The one thing `--include-partial-messages` buys a shape that reads
    // its child's stdout to the end: a message cut off mid-call never
    // arrives as a whole `assistant` record, so the partial
    // `content_block_start` is the only place that call exists.
    let whole_message_only = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["cancelled"],"stop_reason":"cancelled"}"#,
        ]
        .join("\n");

    let unnamed =
        Grok.reply(&whole_message_only).expect("still readable").refused.expect("still a refusal");

    assert!(
        !unnamed.contains("last tool"),
        "with no partial there is nothing to name, and it says nothing rather than \
             guessing: {unnamed}"
    );
}

#[test]
fn a_turn_that_said_something_before_stopping_still_delivers_those_words() {
    // The words and the reason travel together: a turn may answer half a
    // question and *then* ask for a tool nothing approves, and the half is
    // still owed to whoever asked.
    let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"it has three facts; let me write them down"},{"type":"tool_use","id":"t1","name":"write"}]}}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["cancelled"],"stop_reason":"cancelled"}"#,
        ]
        .join("\n");

    let reply = Grok.reply(&stdout).expect("a readable turn");

    assert_eq!(reply.messages, vec!["it has three facts; let me write them down"]);
    assert!(reply.refused.is_some(), "and the reason beside them");
}

#[test]
fn a_turn_the_vendor_failed_carries_the_vendors_own_reason() {
    // The shape an unauthenticated turn actually printed, cut to the field
    // this side reads. Not a cancel, so it takes the other arm — and it is
    // still a reason rather than a parse failure, because this side read it.
    let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"stop_reason":null,"errors":["Internal error: Unauthorized (401) from https://cli-chat-proxy.grok.com/v1/responses"]}"#,
        ]
        .join("\n");

    let refused = Grok
        .reply(&stdout)
        .expect("a failed turn is still readable")
        .refused
        .expect("and it says what the vendor said");

    assert!(refused.contains("Unauthorized (401)"), "{refused}");
    assert!(
        !refused.contains("cancelled this turn"),
        "an authentication failure is not an unapproved tool ask: {refused}"
    );
}

#[test]
fn a_line_this_build_cannot_read_does_not_cost_a_turn_that_otherwise_succeeded() {
    let stdout = [
        "a line that is not JSON at all",
        r#"{"type":"a.kind.this.build.has.never.heard.of","whatever":true}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"answered"}"#,
    ]
    .join("\n");

    let reply = Grok.reply(&stdout).expect("the readable half is readable");

    assert_eq!(reply.messages, vec!["answered"]);
}

#[test]
fn output_carrying_no_record_at_all_is_refused_rather_than_read_as_silence() {
    let refusal =
        Grok.reply("this is not the shape any driver reads\n").expect_err("garbage is refused");

    assert!(refusal.contains(OUTPUT_FORMAT), "{refusal}");
}

#[test]
fn a_turn_cut_off_before_its_message_completed_still_answers_with_what_arrived() {
    // The deltas are the only place those words exist, which is the second
    // reason `--include-partial-messages` is composed.
    let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}}"#,
        ]
        .join("\n");

    let reply = Grok.reply(&stdout).expect("what arrived is what there is");

    assert_eq!(reply.messages, vec!["half an answer"]);
}

/// The pane-mode recording (**D512**), compared against rather than
/// re-typed — the P27 posture-probe pattern: two literals agreeing proves
/// only that somebody typed carefully.
const TUI_PROBE: &str = include_str!("../tests/fixtures/grok-tui-probe.txt");

/// The launch line the recording says the pane ran, binary first.
fn recorded_launch() -> Vec<&'static str> {
    TUI_PROBE
        .lines()
        .find_map(|line| line.strip_prefix("launch: "))
        .expect("the recording names the launch line it probed")
        .split_whitespace()
        .collect()
}

/// What the driver composes for a pane, as strings.
fn tui() -> Vec<String> {
    Grok.tui_argv().iter().map(|token| token.to_string_lossy().into_owned()).collect()
}

#[test]
fn the_tui_argv_is_the_launch_line_the_pane_probe_ran() {
    // Byte for byte against the recording, binary included — and the
    // `read-only` bytes matter here for the reason the headless test
    // states: `--sandbox` is unvalidated at clap, so a near-spelling is a
    // custom profile that fails to load.
    let recorded = recorded_launch();
    let (binary, floors) =
        recorded.split_first().expect("a binary, then the floors it was launched with");

    assert_eq!(*binary, BINARY);
    assert_eq!(tui(), floors);
    // The flags parsed on both recordings: under a symlinked home what
    // happened next was the vendor's refusal, not a parse error — the
    // outcome a pane is meant to keep in front of a person — and under a
    // real one the composer.
    let outcomes: Vec<&str> =
        TUI_PROBE.lines().filter(|line| line.trim_start().starts_with("outcome (")).collect();
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    // Keyed on what each recording says about the home rather than on
    // position, so a third recording fails this loudly instead of
    // shifting which line answers which question.
    let symlinked = outcomes
        .iter()
        .find(|line| line.contains("a symlink"))
        .expect("the symlinked-home recording");
    let real = outcomes
        .iter()
        .find(|line| line.contains("a real directory"))
        .expect("the real-home recording");
    assert!(symlinked.contains("flags parse;"), "{symlinked}");
    assert!(real.contains("composer reached"), "{real}");
    let refusal = TUI_PROBE
        .lines()
        .find_map(|line| line.strip_prefix("error: "))
        .expect("the recording carries the vendor's own refusal verbatim");
    assert!(refusal.contains("could not apply the 'read-only' sandbox profile"), "{refusal}");
}

#[test]
fn the_ready_marker_is_the_composer_glyph_the_probe_captured() {
    // The line under `composer capture`, minus the box border it was
    // drawn inside: grok's composer carries no placeholder, so what the
    // recording shows with nothing typed is the border and the glyph, and
    // the glyph is the marker.
    let captured = TUI_PROBE
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("composer capture"))
        .nth(1)
        .expect("the recording captured the empty composer");
    let glyph = captured
        .trim()
        .strip_prefix('│')
        .and_then(|inner| inner.strip_suffix('│'))
        .expect("the composer line is drawn inside a box")
        .trim();

    assert_eq!(READY_MARKER, glyph);
    // And nothing provisional is left: the recording no longer carries a
    // marker read out of somebody's source instead of off a screen.
    assert!(
        !TUI_PROBE.lines().any(|line| line.trim_start().starts_with("provisional marker")),
        "the composer was captured; the provisional line has no reader left"
    );
}

#[test]
fn the_tui_argv_carries_the_posture_and_none_of_the_headless_machinery() {
    let tui = tui();
    assert_eq!(value(&tui, "--sandbox").as_deref(), Some(SANDBOX_VALUE));
    assert_eq!(value(&tui, "--permission-mode").as_deref(), Some(PERMISSION_MODE));
    // Every word here is a word of the headless first turn — one posture
    // rule, not a second one written for panes.
    let headless = argv(None);
    for token in &tui {
        assert!(headless.contains(token), "{token} is not a headless word");
    }
    // And none of the headless wire: no prompt door of any kind (their
    // absence is what makes that vendor start a TUI at all), no minted or
    // resumed id, no output flags.
    for headless_only in [
        "--prompt-file",
        "--session-id",
        "--resume",
        "--output-format",
        OUTPUT_FORMAT,
        "--include-partial-messages",
    ] {
        assert!(
            !tui.iter().any(|token| token == headless_only),
            "{headless_only} is the headless wire's, and is in {tui:?}"
        );
    }
}

#[test]
fn no_never_composed_spelling_reaches_the_tui_argv() {
    // Iterated rather than re-listed, exactly as for the headless argvs.
    let tui = tui();
    for refused in NEVER_COMPOSED {
        assert!(
            !tui.iter().any(|token| token == refused),
            "{refused} must never be composed, and is in {tui:?}"
        );
    }
}
