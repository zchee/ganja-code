use std::path::PathBuf;

use ganja_team::{MemberName, TeamName, TeamsRoot};

use super::*;
use crate::teammate::{SpawnSpec, shim};

/// A spawn to compose against. Nothing in an argv reads any of it — which
/// is itself the point of **AC-21**, and is why this can be one value.
fn spec() -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse("w1").expect("a member name"),
        team: TeamName::default_team(),
        lead: MemberName::lead(),
        root: TeamsRoot::new(PathBuf::from("/nonexistent/teams")),
        backend: MemberBackend::Codex,
        agent_type: "general".to_owned(),
        model: "whatever-the-person-configured".to_owned(),
        color: "blue".to_owned(),
        prompt: "the spawn prompt, which travels through the mailbox".to_owned(),
        cwd: PathBuf::from("/nonexistent/work"),
        plan_mode_required: false,
        parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
    }
}

/// The argv for a turn that has, or has not, seen a conversation id.
fn argv(session: Option<&str>) -> Vec<String> {
    let spec = spec();
    Codex
        .argv(&Turn {
            spec: &spec,
            text: "a teammate's words, which never reach a command line",
            prompt: None,
            session,
            deadline: shim::CODEX_TURN_TIMEOUT,
        })
        .iter()
        .map(|token| token.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn a_first_turn_states_the_posture_twice_and_takes_its_prompt_on_stdin() {
    // Byte for byte, and the two `-c` tokens include their quotes: `-c`'s
    // own help parses the value as TOML and falls back to a literal, so
    // `sandbox_mode="read-only"` is a TOML string where the unquoted
    // spelling is a bare word that happens to work.
    assert_eq!(
        argv(None),
        vec![
            "exec",
            "--json",
            "--enable",
            "send_async_message",
            "-s",
            "read-only",
            "-c",
            "sandbox_mode=\"read-only\"",
            "-c",
            "approval_policy=\"never\"",
            "--color",
            "never",
            "-",
        ]
    );
}

#[test]
fn a_resume_turn_carries_the_posture_without_the_flag_resume_does_not_have() {
    // `codex exec resume` has no `-s` — the vendor's own `--help` lists it
    // on `exec` and not here — which is the whole reason the `-c` form is
    // composed on both turns rather than only on the one that lacks a flag.
    assert_eq!(
        argv(Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")),
        vec![
            "exec",
            "resume",
            "01a01b4f-174e-7fe2-8abd-ba8e51156c43",
            "--json",
            "--enable",
            "send_async_message",
            "-c",
            "sandbox_mode=\"read-only\"",
            "-c",
            "approval_policy=\"never\"",
            "-",
        ]
    );
    assert!(
        !argv(Some("x")).iter().any(|token| token == "-s"),
        "a resume turn that carried `-s` would be a parse error, not a tighter posture"
    );
}

#[test]
fn the_posture_is_pinned_on_every_turn_and_not_only_the_first() {
    for session in [None, Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")] {
        let argv = argv(session);
        for pinned in [SANDBOX_OVERRIDE, APPROVAL_OVERRIDE] {
            assert!(argv.iter().any(|token| token == pinned), "{pinned} is missing from {argv:?}");
        }
    }
}

#[test]
fn no_never_composed_spelling_reaches_either_argv() {
    // Iterated rather than re-listed: [`NEVER_COMPOSED`] is the single
    // source, so a flag added to it is a flag this assertion picks up.
    for session in [None, Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")] {
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
fn the_only_config_overrides_are_the_two_pinned_posture_keys() {
    // Narrower than "the two are present", and deliberately: `-c` can set
    // any key the config file can, so the danger is the third one nobody
    // listed rather than the two that are.
    for session in [None, Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")] {
        let argv = argv(session);
        let overrides: Vec<&String> = argv
            .iter()
            .zip(argv.iter().skip(1))
            .filter_map(|(flag, value)| (flag == "-c" || flag == "--config").then_some(value))
            .collect();
        assert_eq!(overrides.len(), PINNED_KEYS.len(), "in {argv:?}");
        for value in overrides {
            let key = value.split('=').next().expect("a key before the equals");
            assert!(PINNED_KEYS.contains(&key), "{key} is not one of the pinned posture keys");
        }
    }
}

#[test]
fn no_prompt_text_is_ever_on_a_command_line() {
    let spec = spec();
    let secret = "the words a peer said, which argv is world-readable through ps";
    let argv = Codex.argv(&Turn {
        spec: &spec,
        text: secret,
        prompt: None,
        session: None,
        deadline: shim::CODEX_TURN_TIMEOUT,
    });
    assert!(
        !argv.iter().any(|token| token.to_string_lossy().contains(secret)),
        "argv is for flags; `-` is what says the prompt is on stdin"
    );
    assert_eq!(Codex.door(), Door::Stdin);
}

#[test]
fn a_thread_started_line_is_where_a_later_turn_gets_its_id() {
    let reply = Codex
        .reply(concat!(
            r#"{"type":"thread.started","thread_id":"01a01b4f-174e-7fe2-8abd-ba8e51156c43"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":31723,"output_tokens":6}}"#,
            "\n",
        ))
        .expect("the shapes a probed 0.149.0-alpha.1 actually printed");
    assert_eq!(reply.session.as_deref(), Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43"));
}

#[test]
fn every_agent_message_becomes_one_mail_in_arrival_order() {
    // **AC-5**. `send_async_message` is what lets a turn say something
    // before it ends, so a mid-turn item and a final one both arrive and
    // folding them into one mail would lose the order a reader needs.
    let reply = Codex
            .reply(concat!(
                r#"{"type":"thread.started","thread_id":"t-1"}"#,
                "\n",
                r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"starting on it"}}"#,
                "\n",
                r#"{"type":"item.completed","item":{"id":"item_1","type":"reasoning","text":"not a teammate talking"}}"#,
                "\n",
                r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"done"}}"#,
                "\n",
                r#"{"type":"turn.completed","usage":{}}"#,
                "\n",
            ))
            .expect("a two-message turn");
    assert_eq!(reply.messages, vec!["starting on it", "done"]);
}

#[test]
fn an_item_that_is_not_a_teammate_talking_is_not_mail() {
    let reply = Codex
            .reply(concat!(
                r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"ls"}}"#,
                "\n",
                r#"{"type":"turn.completed","usage":{}}"#,
                "\n",
            ))
            .expect("a turn that only ran a command");
    assert!(reply.messages.is_empty(), "{:?}", reply.messages);
}

#[test]
fn a_line_this_build_cannot_read_does_not_cost_a_turn_that_otherwise_succeeded() {
    // A future version printing one more event kind, or one more field, is
    // the vendor's business — and failing a turn over it would make every
    // codex release a ganja outage.
    let reply = Codex
            .reply(concat!(
                "a line that is not JSON at all\n",
                r#"{"type":"an.event.kind.this.build.has.never.heard.of","whatever":true}"#,
                "\n",
                r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"answered"}}"#,
                "\n",
            ))
            .expect("the readable half is still readable");
    assert_eq!(reply.messages, vec!["answered"]);
}

#[test]
fn output_carrying_no_event_at_all_is_refused_rather_than_read_as_silence() {
    // **AC-8**'s garbage arm: a clean exit with unreadable stdout becomes a
    // structured failure mail, where an empty [`Reply`] would become a
    // teammate that answered nothing and said nothing about it.
    let refusal =
        Codex.reply("this is not the shape any driver reads\n").expect_err("garbage is refused");
    assert!(refusal.contains("--json"), "{refusal}");
}

#[test]
fn a_turn_the_vendor_failed_is_refused_with_the_vendors_own_reason() {
    let refusal = Codex
        .reply(concat!(
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"the model is not available"}}"#,
            "\n",
        ))
        .expect_err("a failed turn is a failure");
    assert!(refusal.contains("the model is not available"), "{refusal}");
}

#[test]
fn the_environment_carries_the_credential_home_and_no_posture_door() {
    assert_eq!(Codex.additions(), &["CODEX_HOME"]);
    // `CODEX_PERMISSION_PROFILE` is the door this omission closes: it names
    // the very permission profile a turn's own rollout records, and
    // enumeration is what keeps it out.
    assert!(
        !Codex.additions().iter().any(|name| name.contains("PERMISSION")),
        "no environment door onto the posture may be carried"
    );
}

/// The pane-mode recording (**D512**), compared against rather than
/// re-typed — the P27 posture-probe pattern: two literals agreeing proves
/// only that somebody typed carefully.
const TUI_PROBE: &str = include_str!("../../tests/fixtures/codex-tui-probe.txt");

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
    Codex.tui_argv().iter().map(|token| token.to_string_lossy().into_owned()).collect()
}

#[test]
fn the_tui_argv_is_the_launch_line_the_pane_probe_ran() {
    // Byte for byte against the recording, binary included: the two `-c`
    // values carry their quotes in the fixture exactly as they do in
    // [`SANDBOX_OVERRIDE`] and [`APPROVAL_OVERRIDE`], because that is how
    // the binary received them.
    let recorded = recorded_launch();
    let (binary, floors) =
        recorded.split_first().expect("a binary, then the floors it was launched with");

    assert_eq!(*binary, BINARY);
    assert_eq!(tui(), floors);
    // And the recording says those words reached the composer, which is
    // what makes them a launch line and not a parse experiment.
    let outcome = TUI_PROBE
        .lines()
        .find_map(|line| line.strip_prefix("outcome: "))
        .expect("the recording says what the launch reached");
    assert!(outcome.starts_with("composer reached"), "{outcome}");
}

#[test]
fn the_ready_marker_is_the_empty_composer_the_probe_captured() {
    // The line under `composer capture`, minus the prompt glyph it was
    // drawn after: a poll reads a captured pane, and the pane shows the
    // glyph, so what is pinned is the placeholder and what is stripped is
    // the decoration.
    let captured = TUI_PROBE
        .lines()
        .skip_while(|line| !line.starts_with("composer capture"))
        .nth(1)
        .expect("the recording captured the empty composer");
    let placeholder = captured
        .strip_prefix("› ")
        .expect("the composer draws its placeholder after the prompt glyph");

    assert_eq!(READY_MARKER, placeholder);
}

#[test]
fn the_tui_argv_carries_the_posture_and_none_of_the_headless_machinery() {
    let tui = tui();
    // Both pinned tokens, as the same bytes the headless argv carries.
    for pinned in [SANDBOX_OVERRIDE, APPROVAL_OVERRIDE] {
        assert!(tui.iter().any(|token| token == pinned), "{tui:?}");
    }
    // Every word here is a word of the headless first turn — one posture
    // rule, not a second one written for panes.
    let headless = argv(None);
    for token in &tui {
        assert!(headless.contains(token), "{token} is not a headless word");
    }
    // And none of the headless wire: no subcommand, no output flags, no
    // prompt door. An interactive codex given `-` would be asked to read a
    // prompt from a stdin that is a pty.
    for headless_only in ["exec", "--json", "--enable", FEATURE, "-s", "--color", "-"] {
        assert!(
            !tui.iter().any(|token| token == headless_only),
            "{headless_only} is the headless wire's, and is in {tui:?}"
        );
    }
    // The `-c` narrowing the headless argv is held to, held here too: the
    // danger of that flag is the key nobody listed.
    let overrides: Vec<&String> = tui
        .iter()
        .zip(tui.iter().skip(1))
        .filter_map(|(flag, value)| (flag == "-c" || flag == "--config").then_some(value))
        .collect();
    assert_eq!(overrides.len(), PINNED_KEYS.len(), "in {tui:?}");
    for value in overrides {
        let key = value.split('=').next().expect("a key before the equals");
        assert!(PINNED_KEYS.contains(&key), "{key} is not one of the pinned posture keys");
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
