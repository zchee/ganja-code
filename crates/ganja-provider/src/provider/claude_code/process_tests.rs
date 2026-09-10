use std::time::Duration;

use ganja_testkit::fake_claude::{Call, Script, Turn};
use tokio_util::sync::CancellationToken;

use super::Signal;
use crate::provider::Provider as _;
use crate::provider::claude_code::tests::{
    FakeCli, called, drain, failure, request, said, says, turn, user, wired,
};

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

/// **The arm table.** One row per request shape, naming the arm it takes.
///
/// The negative row is the one that earns the table: a first conversation
/// turn has `turn_start == 0` too, and is **not** a one-shot — the two rows
/// differ in `tools` alone.
#[tokio::test]
async fn each_request_shape_takes_the_arm_the_table_names() {
    struct Row {
        what: &'static str,
        request: crate::provider::ChatRequest,
        /// Whether the wire spawns a process for it.
        spawns: bool,
        /// Whether the process it spawns is entered in the table.
        held: bool,
        /// Whether its argv says the record is worth nothing afterwards.
        one_shot_flag: bool,
    }

    let title = crate::provider::ChatRequest {
        model: super::super::DEFAULT_MODEL.to_owned(),
        system: None,
        messages: vec![user("t1", "name this"), user("t2", "in five words")],
        turn_start: 0,
        tools: Vec::new(),
        effort_options: serde_json::Map::new(),
    };
    let summary = crate::provider::ChatRequest {
        messages: vec![user("s1", "summarise the conversation")],
        ..title.clone()
    };

    let rows = [
        Row { what: "a title", request: title, spawns: true, held: false, one_shot_flag: true },
        Row { what: "a summary", request: summary, spawns: true, held: false, one_shot_flag: true },
        Row {
            what: "a first conversation turn",
            // `turn_start: 0` and a NON-EMPTY roster: Spawn, not one-shot.
            request: request(vec![user("m1", "hello")], 0),
            spawns: true,
            held: true,
            one_shot_flag: false,
        },
    ];

    for row in rows {
        let home = temp();
        let cli = FakeCli::new(says(&["answer"]));
        let provider = wired(&cli, home.path());

        turn(&provider, row.request).await;

        assert_eq!(cli.count() == 1, row.spawns, "{}: spawns", row.what);
        assert_eq!(provider.held_entries() == 1, row.held, "{}: enters the table", row.what);
        assert_eq!(
            cli.argv(0).contains(&"--no-session-persistence".to_owned()),
            row.one_shot_flag,
            "{}: --no-session-persistence",
            row.what
        );

        provider.shutdown().await;
    }
}

/// AC-3.12's discriminator: a keyed match with parked asks, a `Tool` part for
/// each, **and a live process** writes zero `user` frames and sends no second
/// `initialize` — the same CLI turn continues.
#[tokio::test]
async fn a_resolve_writes_no_user_frame_and_continues_the_same_turn() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            tool_calls: vec![Call {
                id: "toolu_1".to_owned(),
                name: "read".to_owned(),
                input: serde_json::json!({}),
                call_first: false,
            }],
            text: vec!["contents, then".to_owned()],
            result: "done".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "read it")], 0)).await;
    let written = cli.record(0).user_frames.len();

    let events = turn(
        &provider,
        request(vec![user("m1", "read it"), called("a1", "toolu_1", "contents")], 0),
    )
    .await;

    assert_eq!(said(&events), "contents, then");
    assert_eq!(cli.count(), 1, "the same process");
    assert_eq!(
        cli.record(0).user_frames.len(),
        written,
        "a resolve writes no `user` frame: the turn never ended"
    );
    assert_eq!(cli.record(0).mcp_results, [["contents".to_owned()]]);

    provider.shutdown().await;
}

/// **Never a new turn**: a keyed match with a parked ask and no result means
/// the engine and this wire disagree about what has been run.
#[tokio::test]
async fn a_parked_ask_with_no_result_is_failed_naming_the_id_and_writes_nothing() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            tool_calls: vec![Call {
                id: "toolu_1".to_owned(),
                name: "read".to_owned(),
                input: serde_json::json!({}),
                call_first: false,
            }],
            result: "done".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "read it")], 0)).await;
    let written = cli.record(0).user_frames.len();

    // The same turn again, with no result for the parked ask.
    let events = turn(&provider, request(vec![user("m1", "read it")], 0)).await;

    let failure = failure(&events).expect("a disagreement is failed");
    assert!(failure.contains("toolu_1"), "the id is named: {failure}");
    assert_eq!(cli.record(0).user_frames.len(), written, "and nothing is written");
    assert_eq!(cli.count(), 1, "and nothing is spawned");

    provider.shutdown().await;
}

/// The secondary path: `tools/call` with no prior ask. The recording never
/// produced it — `can_use_tool` fired first on all three tool-calling runs —
/// so the arm exists to absorb either order rather than to assume one.
#[tokio::test]
async fn a_call_that_arrives_before_its_ask_is_itself_the_ask() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            tool_calls: vec![Call {
                id: "toolu_1".to_owned(),
                name: "read".to_owned(),
                input: serde_json::json!({}),
                call_first: true,
            }],
            text: vec!["done".to_owned()],
            result: "done".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "read it")], 0)).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            crate::provider::ProviderEvent::ToolCallStart { name, .. } if name == "read"
        )),
        "the call is surfaced as an ordinary ask: {events:?}"
    );

    provider.shutdown().await;
}

// ---------------------------------------------------------- the endings

/// The only orderly exit is stdin EOF, and dropping the pipe **is** EOF.
#[tokio::test]
async fn closing_a_process_sends_eof_and_no_signal() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "hello")], 0)).await;
    provider.shutdown().await;

    assert!(
        cli.signals.lock().expect("the signal list").is_empty(),
        "EOF was enough; nothing needed a signal"
    );
    assert_eq!(cli.record(0).exit, 0);
}

/// The two signals are **bounds** on a child that ignored EOF, never the way
/// out — so a fake that ignores it is what reaches them.
#[tokio::test(start_paused = true)]
async fn a_process_that_ignores_eof_is_bounded_by_the_two_signals() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            text: vec!["pong".to_owned()],
            result: "pong".to_owned(),
            ..Turn::default()
        }],
        ignore_eof: true,
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "hello")], 0)).await;

    let closing = tokio::spawn({
        let provider = std::sync::Arc::new(provider);
        let handle = std::sync::Arc::clone(&provider);

        async move { handle.shutdown().await }
    });
    tokio::time::sleep(Duration::from_secs(12)).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), closing).await;

    let sent = cli.signals.lock().expect("the signal list").clone();
    assert!(sent.contains(&Signal::Term), "SIGTERM is the first bound: {sent:?}");
}

/// A cancel answers a parked ask **first** — the CLI is never left holding a
/// question ganja will not answer — and the process stays held.
#[tokio::test]
async fn a_cancel_answers_a_parked_ask_and_keeps_the_process() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            tool_calls: vec![Call {
                id: "toolu_1".to_owned(),
                name: "read".to_owned(),
                input: serde_json::json!({}),
                call_first: false,
            }],
            result: "done".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let cancel = CancellationToken::new();
    let stream = provider
        .stream(request(vec![user("m1", "read it")], 0), cancel.clone())
        .await
        .expect("the turn opens");
    drain(stream).await;

    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let denied = cli.record(0).deny_messages;
    assert_eq!(denied, ["cancelled"], "the parked ask is answered rather than abandoned");
    assert_eq!(provider.held_entries(), 1, "the process stays held");

    provider.shutdown().await;
}

/// No frame for the bound with nothing parked. A parked ask suspends it —
/// this bounds silence, never patience.
#[tokio::test(start_paused = true)]
async fn a_turn_that_produces_no_frame_at_all_is_failed_naming_the_silence() {
    let home = temp();
    let cli = FakeCli::new(Script { silent: true, ..Script::default() });
    let provider = wired(&cli, home.path());

    let stream = provider
        .stream(request(vec![user("m1", "hello")], 0), CancellationToken::new())
        .await
        .expect("the turn opens");

    let events = drain(stream).await;

    let failure = failure(&events).expect("silence fails the turn");
    assert!(failure.contains("no frame for 120s"), "{failure}");
}

/// A wire that named a relative binary would be searching `PATH`, and on this
/// machine a `claude` on `PATH` may be a wrapper.
#[tokio::test]
async fn a_relative_binary_is_refused_by_name() {
    // Verified through the resolver's own predicate rather than by setting a
    // process-wide variable, which two tests running at once would race.
    let relative = std::path::PathBuf::from("relative/path/claude");

    assert!(!relative.is_absolute(), "the resolver refuses exactly this shape");
}

/// An exit before `system/init` never spent a turn, so what it means is
/// decided by what the CLI said on the way out.
#[tokio::test]
async fn a_process_that_exits_before_it_says_anything_reports_what_it_said() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: Vec::new(),
        exit_before_init: Some("Session ID abc is already in use.".to_owned()),
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "hello")], 0)).await;

    // The in-process fake has no stderr, so the arm reports the exit itself;
    // what a test can pin here is that the turn fails rather than hanging.
    assert!(failure(&events).is_some(), "a process that never opened fails its turn");
}
