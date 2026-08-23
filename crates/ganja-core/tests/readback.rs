//! A shim pane teammate's answers, read out of its CLI's own transcript
//! (**D515**).
//!
//! Neither upstream opencode nor Claude Code has a counterpart. What is
//! asserted is the whole of the read-back contract, per CLI, against
//! **recorded shapes**: `fixtures/readback/{codex-rollout,grok-updates,
//! agy-transcript}.jsonl` are excerpts of the files those three CLIs really
//! wrote for a pane teammate on 2026-08-23, redacted to this fixture's own
//! member and directory and shortened to the record kinds that decide
//! anything. A hand-invented shape would prove that the reader parses what
//! this test imagined.
//!
//! Its own binary, and that is not a style choice: each reader resolves its
//! CLI's home from the environment, so this suite writes `HOME`, `CODEX_HOME`
//! and `GROK_HOME` — process-wide state, which the house rule keeps in a
//! binary of its own. The one write happens before any test body runs.

use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
    time::SystemTime,
};

use ganja_core::teammate::{
    preamble::{self, Names},
    readback::{self, Cursor},
    shim_tui,
};
use ganja_protocol::team::MemberBackend;
use ganja_team::ShimCli;

/// The member every fixture's pasted message names — and therefore the
/// fingerprint every reader here is asked to find.
const WHO: Names<'static> = Names {
    name: "w1",
    team: "session-abcd1234",
    lead: "team-lead",
};

/// The directory the fixture panes were opened in, as the recordings carry
/// it: grok's reader is the one that narrows by it, so it has to be the same
/// path this side encodes.
const CWD: &str = "/tmp/ganja-readback-fixture";

/// codex's own rollout, as it wrote one.
const CODEX: &str = include_str!("fixtures/readback/codex-rollout.jsonl");
/// grok's own session updates, as it wrote them.
const GROK: &str = include_str!("fixtures/readback/grok-updates.jsonl");
/// agy's own transcript, as it wrote one.
const AGY: &str = include_str!("fixtures/readback/agy-transcript.jsonl");

/// A home holding all three CLIs' session trees, in each vendor's own layout,
/// with this binary's environment pointed at it.
///
/// Leaked rather than dropped: the readers resolve their homes from the
/// environment on every call, so a temporary directory taken away at the end
/// of the first test would leave the rest reading a path that is not there.
/// One directory under the system temp, named for this process, is what the
/// other environment-owning suites here do.
static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    let home = std::env::temp_dir().join(format!("ganja-readback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);

    // codex: <CODEX_HOME>/sessions/<yyyy>/<mm>/<dd>/rollout-*.jsonl
    let codex = home.join("codex-home");
    write(
        &codex.join("sessions/2026/08/23/rollout-2026-08-23T20-25-51-01a02e5e.jsonl"),
        CODEX,
    );
    // A second rollout of somebody else's conversation, in the same day
    // directory: the fingerprint is what has to tell them apart.
    write(
        &codex.join("sessions/2026/08/23/rollout-2026-08-23T20-59-00-01a02e9f.jsonl"),
        &CODEX.replace(WHO.name, "w2"),
    );

    // grok: <GROK_HOME>/sessions/<percent-encoded cwd>/<id>/updates.jsonl
    let grok = home.join("grok-home");
    let encoded = "%2Ftmp%2Fganja-readback-fixture";
    write(
        &grok.join(format!("sessions/{encoded}/01a02e5f-0000/updates.jsonl")),
        GROK,
    );

    // agy: $HOME/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript.jsonl
    write(
        &home.join(
            ".gemini/antigravity-cli/brain/03ea1ab7-0000/.system_generated/logs/transcript.jsonl",
        ),
        AGY,
    );

    // SAFETY: this binary's only writes to the environment, run exactly once
    // from a `LazyLock` every test body forces first, before any reader has
    // asked where a home is.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("CODEX_HOME", &codex);
        std::env::set_var("GROK_HOME", &grok);
    }

    home
});

/// One fixture onto disk, parents and all.
fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a fixture has a parent")).expect("the directory");
    std::fs::write(path, text).expect("the fixture is written");
}

/// The fingerprint the readers are asked for, composed the way a spawn
/// composes it rather than spelled here.
fn mark() -> String {
    preamble::opening(WHO)
}

/// A spawn time every fixture this binary writes is newer than.
///
/// The bound itself is asserted by
/// [`a_conversation_older_than_the_member_is_never_latched`]; everywhere else
/// what is under test is the reading, so the time is put out of the way.
fn spawned() -> SystemTime {
    SystemTime::UNIX_EPOCH
}

/// What one CLI's reader finds and carries, from nothing.
fn carried(cli: ShimCli) -> (Option<PathBuf>, Vec<String>, Cursor) {
    LazyLock::force(&HOME);
    let reader = readback::of(cli);
    let found = reader.find(&mark(), Path::new(CWD), spawned());
    let mut cursor = Cursor::default();
    let answers = found
        .as_deref()
        .map(|path| reader.answers(path, &mut cursor))
        .unwrap_or_default();

    (found, answers, cursor)
}

/// The fixtures carry the preamble **this build composes**, not one it used
/// to: a recording is only evidence about the shipped shape if the message it
/// records is the shipped message.
#[test]
fn each_fixture_holds_the_preamble_this_build_pastes() {
    for (cli, backend, recorded) in [
        (ShimCli::Codex, MemberBackend::Codex, CODEX),
        (ShimCli::Grok, MemberBackend::Grok, GROK),
        (ShimCli::Agy, MemberBackend::Agy, AGY),
    ] {
        let composed = shim_tui::preamble(WHO, backend, "Explain this codebase");
        assert!(
            recorded.contains(&composed.replace('\n', "\\n")),
            "{cli:?}'s fixture records a preamble this build no longer sends"
        );
    }
}

/// **codex.** Every assistant message, in arrival order — the contract its
/// own headless driver already mails by, and the one
/// [`readback::answers_clause`] states.
#[test]
fn codex_carries_every_message_it_printed_in_order() {
    let (found, answers, cursor) = carried(ShimCli::Codex);

    let path = found.expect("codex's rollout is found by the fingerprint");
    assert!(
        path.to_string_lossy().contains("01a02e5e"),
        "and it is this member's conversation, not the one beside it: {}",
        path.display()
    );
    assert_eq!(
        answers,
        [
            "I'll map the crates first, then trace one prompt through the engine.",
            "The workspace is twelve members; the engine carries no terminal dependency.",
        ],
        "every message, in the order the rollout holds them"
    );
    assert!(cursor.bytes > 0, "an append-only transcript is tailed");

    // A second look carries nothing: the cursor is what makes a two-second
    // poll safe to run against a file nobody has added to.
    let reader = readback::of(ShimCli::Codex);
    let mut again = cursor;
    assert!(reader.answers(&path, &mut again).is_empty());
    assert_eq!(again, cursor, "and it did not move");
}

/// **grok.** One answer per finished turn: the last thing it said before its
/// own `turn_completed`, with the narration on the way to it left where it
/// is.
#[test]
fn grok_carries_the_last_thing_it_said_before_each_turn_ended() {
    let (found, answers, cursor) = carried(ShimCli::Grok);

    assert!(
        found
            .expect("grok's updates are found")
            .ends_with("updates.jsonl"),
        "grok's transcript is the session's own updates file"
    );
    assert_eq!(
        answers,
        ["# ganja-code\n\nA terminal-first agent, twelve crates."],
        "the turn's last message, and not the narration before it"
    );
    assert_eq!(
        cursor,
        Cursor {
            bytes: 0,
            answers: 1
        },
        "a re-read transcript counts answers rather than bytes"
    );
}

/// **agy.** One answer per finished turn: a `PLANNER_RESPONSE` that carries
/// content, never the ones carrying only its thinking or its tool calls.
#[test]
fn agy_carries_the_planner_response_that_holds_an_answer() {
    let (found, answers, cursor) = carried(ShimCli::Agy);

    assert!(
        found
            .expect("agy's transcript is found")
            .ends_with("transcript.jsonl")
    );
    assert_eq!(
        answers,
        ["### Executive Overview\n\nganja-code is a terminal-first AI coding agent in Rust."],
        "the record that carried content, and none of the ones that did not"
    );
    assert_eq!(cursor.answers, 1);
}

/// **The window is records, not bytes.** codex writes its own instructions,
/// the project's `AGENTS.md` and whatever its plugins add before the first
/// user message: on the machine this was built on that put the pasted
/// sentence at byte 265,806 of a 273KB rollout, and a byte window small
/// enough to be worth having answered "not this member's" for a transcript
/// that plainly was (measured 2026-08-24, the first live run of the relay).
#[test]
fn a_fingerprint_far_past_any_byte_window_is_still_found() {
    LazyLock::force(&HOME);
    let padded = HOME
        .join("codex-home")
        .join("sessions/2026/08/22/rollout-2026-08-22T00-00-00-01a02222.jsonl");
    // Half a megabyte of the CLI's own preamble records, then this member's
    // own message — the shape the live run met.
    let filler = format!(
        "{}\n",
        serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "developer",
                        "content": [{"type": "text", "text": "x".repeat(4096)}]},
        })
    );
    let mut text = filler.repeat(128);
    text.push_str(&CODEX.replace(WHO.team, "session-padded1"));
    write(&padded, &text);
    assert!(text.len() > 512 * 1024, "the fixture is past any window");

    let found = readback::of(ShimCli::Codex).find(
        &preamble::opening(Names {
            team: "session-padded1",
            ..WHO
        }),
        Path::new(CWD),
        spawned(),
    );
    assert_eq!(
        found.as_deref(),
        Some(padded.as_path()),
        "a fingerprint half a megabyte in is still this member's session"
    );
}

/// **A second turn.** The lead pastes again, the CLI answers again, and the
/// reader carries only what is new — the case a two-second poll spends its
/// whole life in, and the one a cursor exists for. Each shape is driven the
/// way its CLI grows a file: codex appends records, grok and agy are re-read
/// whole.
///
/// Over **its own copies** of the three recordings, under a member of its
/// own: this binary's tests share one home, and a test that appended to the
/// files the others read would pass or fail on the order they happened to run
/// in.
#[test]
fn a_second_turn_carries_only_what_the_cli_added() {
    LazyLock::force(&HOME);
    let who = Names {
        name: "w-turns",
        ..WHO
    };
    let mark = preamble::opening(who);
    let mine = |text: &str| text.replace(&preamble::opening(WHO), &mark);
    let grow = |path: &Path, tail: &str| {
        let grown = format!(
            "{}{tail}",
            std::fs::read_to_string(path).expect("the transcript reads")
        );
        std::fs::write(path, grown).expect("the transcript grows");
    };

    // codex: an append-only rollout gains one more assistant message.
    let rollout = HOME
        .join("codex-home")
        .join("sessions/2026/08/21/rollout-2026-08-21T00-00-00-01a02111.jsonl");
    write(&rollout, &mine(CODEX));
    let codex = readback::of(ShimCli::Codex);
    let mut cursor = Cursor::default();
    assert_eq!(
        codex.find(&mark, Path::new(CWD), spawned()).as_deref(),
        Some(rollout.as_path())
    );
    assert_eq!(codex.answers(&rollout, &mut cursor).len(), 2);
    grow(
        &rollout,
        &format!(
            "{}\n",
            serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant",
                            "content": [{"type": "output_text", "text": "The second turn's answer."}]},
            })
        ),
    );
    assert_eq!(
        codex.answers(&rollout, &mut cursor),
        ["The second turn's answer."],
        "only the record the CLI added"
    );

    // grok: a second finished turn, and only its own last message.
    let updates = HOME
        .join("grok-home")
        .join("sessions/%2Ftmp%2Fganja-readback-fixture/01a02e5f-turns/updates.jsonl");
    write(&updates, &mine(GROK));
    let grok = readback::of(ShimCli::Grok);
    let mut cursor = Cursor::default();
    assert_eq!(grok.answers(&updates, &mut cursor).len(), 1);
    let update = |value: serde_json::Value| {
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp": 1_787_484_500u64,
                "method": "session/update",
                "params": {"sessionId": "01a02e5f-turns", "update": value},
            })
        )
    };
    grow(
        &updates,
        &format!(
            "{}{}{}",
            update(serde_json::json!({"sessionUpdate": "agent_message_chunk",
                                      "content": {"type": "text", "text": "narrating again"}})),
            update(serde_json::json!({"sessionUpdate": "agent_message_chunk",
                                      "content": {"type": "text", "text": "The second turn's whole answer."}})),
            update(serde_json::json!({"sessionUpdate": "turn_completed"})),
        ),
    );
    assert_eq!(
        grok.answers(&updates, &mut cursor),
        ["The second turn's whole answer."],
        "the new turn's last message, and neither its narration nor the first turn again"
    );

    // agy: a second content-bearing planner response.
    let transcript = HOME.join(
        ".gemini/antigravity-cli/brain/03ea1ab7-turns/.system_generated/logs/transcript.jsonl",
    );
    write(&transcript, &mine(AGY));
    let agy = readback::of(ShimCli::Agy);
    let mut cursor = Cursor::default();
    assert_eq!(agy.answers(&transcript, &mut cursor).len(), 1);
    grow(
        &transcript,
        &format!(
            "{}\n",
            serde_json::json!({
                "step_index": 40, "source": "MODEL", "type": "PLANNER_RESPONSE",
                "status": "DONE", "created_at": "2026-08-23T11:30:00Z",
                "content": "The second turn's answer.",
            })
        ),
    );
    assert_eq!(
        agy.answers(&transcript, &mut cursor),
        ["The second turn's answer."],
        "only the record the CLI added"
    );
}

/// A member whose CLI has written nothing is answered with **nothing**, not
/// with somebody else's conversation — the whole reason the search is a
/// fingerprint rather than "the newest file".
#[test]
fn a_member_with_no_session_of_its_own_finds_none() {
    LazyLock::force(&HOME);
    let stranger = preamble::opening(Names {
        name: "w9",
        team: "session-abcd1234",
        lead: "team-lead",
    });

    for cli in [ShimCli::Codex, ShimCli::Grok, ShimCli::Agy] {
        assert_eq!(
            readback::of(cli).find(&stranger, Path::new(CWD), spawned()),
            None,
            "{cli:?} answered a member that never spawned"
        );
    }
}

/// **grok's directory is the one it resolved, not the one it was handed.**
///
/// That vendor names a session directory after the *canonical* path, so a
/// pane opened through a symlink — `/tmp` on macOS, or anybody's symlinked
/// worktree — writes under a spelling this side never composed. Before this
/// was measured the reader encoded what it was handed and simply never found
/// the transcript: the pane went quiet and said so 90 seconds later, which is
/// the worst way for a feature to fail. The symlink here is the suite's own,
/// so the assertion does not depend on how this machine spells its temp
/// directory.
#[test]
fn groks_directory_is_canonicalized_the_way_that_vendor_writes_it() {
    LazyLock::force(&HOME);
    let real = HOME.join("worktree-real");
    let link = HOME.join("worktree-link");
    std::fs::create_dir_all(&real).expect("the real directory");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).expect("the symlink");

    let who = Names {
        name: "w-link",
        ..WHO
    };
    let mark = preamble::opening(who);
    let encoded: String = std::fs::canonicalize(&real)
        .expect("the real path resolves")
        .to_string_lossy()
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect();
    let updates = HOME
        .join("grok-home")
        .join(format!("sessions/{encoded}/01a02e5f-link/updates.jsonl"));
    write(&updates, &GROK.replace(&preamble::opening(WHO), &mark));

    assert_eq!(
        readback::of(ShimCli::Grok)
            .find(&mark, &link, spawned())
            .as_deref(),
        Some(updates.as_path()),
        "a pane opened through a symlink still finds the session grok wrote"
    );
}

/// A conversation older than the member is never latched, whatever it holds.
///
/// The other half of the correlation bound: a CLI that is resumed carries its
/// own history, and a session that once read this sentence — from a mailbox,
/// from this repository's fixtures — must not be mistaken for the one this
/// member's paste opened.
#[test]
fn a_conversation_older_than_the_member_is_never_latched() {
    LazyLock::force(&HOME);
    let later = SystemTime::now() + std::time::Duration::from_secs(3600);

    for cli in [ShimCli::Codex, ShimCli::Grok, ShimCli::Agy] {
        assert_eq!(
            readback::of(cli).find(&mark(), Path::new(CWD), later),
            None,
            "{cli:?} latched a conversation that predates the spawn"
        );
    }
}
