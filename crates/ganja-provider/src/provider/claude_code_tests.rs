//! The provider's own suite, and the fake CLI harness the sibling suites
//! share.
//!
//! **Nothing here spawns a `claude`.** The harness hands the wire's own spawn
//! seam a duplex whose far end is `ganja_testkit::fake_claude::replay`, so a
//! whole conversation — the dial, a tool ask, a refusal, an idle eviction —
//! runs in-process with no child at all.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use ganja_testkit::fake_claude::{Record, Script, Turn};
use tokio_util::sync::CancellationToken;

use super::{
    ClaudeCodeProvider, DEFAULT_MODEL, honest, owed, parse_version, remembered_ids, user_ids,
};
use crate::protocol::{FinishReason, Message, MessageId, Part, PartBody, PartId, ToolState};
use crate::provider::claude_code::process::{ChildIo, Signal, Spawner};
use crate::provider::{ChatRequest, Provider as _, ProviderError, ProviderEvent};
use crate::tool::ToolDefinition;

/// One fake CLI the wire spawned, and what it saw.
pub(crate) struct Spawned {
    /// The argv it was spawned with.
    pub(crate) argv: Vec<String>,
    /// The directory it was spawned in.
    pub(crate) cwd: PathBuf,
    /// What it recorded, readable while it is still running.
    pub(crate) record: Arc<Mutex<Record>>,
}

/// The spawn seam, wired to the fake instead of to a child.
pub(crate) struct FakeCli {
    script: Script,
    /// One entry per spawn, in order.
    pub(crate) spawns: Arc<Mutex<Vec<Spawned>>>,
    /// Every signal the wire sent, in order — empty is the assertion that EOF
    /// was enough.
    pub(crate) signals: Arc<Mutex<Vec<Signal>>>,
}

impl FakeCli {
    pub(crate) fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            spawns: Arc::new(Mutex::new(Vec::new())),
            signals: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// How many processes the wire has spawned.
    pub(crate) fn count(&self) -> usize {
        self.spawns.lock().expect("the spawn list").len()
    }

    /// The `n`th spawn's argv.
    pub(crate) fn argv(&self, at: usize) -> Vec<String> {
        self.spawns.lock().expect("the spawn list")[at].argv.clone()
    }

    /// The `n`th spawn's record, copied.
    pub(crate) fn record(&self, at: usize) -> Record {
        let spawns = self.spawns.lock().expect("the spawn list");

        spawns[at].record.lock().expect("the record").clone()
    }

    /// Every argv the wire has built, in spawn order.
    pub(crate) fn argvs(&self) -> Vec<Vec<String>> {
        let spawns = self.spawns.lock().expect("the spawn list");

        spawns.iter().map(|spawn| spawn.argv.clone()).collect()
    }
}

impl Spawner for FakeCli {
    fn spawn(
        &self,
        _bin: &Path,
        argv: &[OsString],
        env: &super::argv::ChildEnv,
    ) -> Result<ChildIo, ProviderError> {
        let spelled: Vec<String> =
            argv.iter().map(|token| token.to_string_lossy().into_owned()).collect();

        // The double refuses what the real CLI refuses, so a builder that
        // grew `--resume` back fails here as well as at `argv_tests.rs`.
        if let Some(token) =
            spelled.iter().find(|token| super::argv::NEVER_ANYWHERE.contains(&token.as_str()))
        {
            return Err(ProviderError::Transport(format!("error: unknown option '{token}'")));
        }
        if !spelled.iter().any(|token| token == "--verbose") {
            return Err(ProviderError::Transport(
                ganja_testkit::fake_claude::NEEDS_VERBOSE.to_owned(),
            ));
        }

        let session_id = spelled
            .iter()
            .position(|token| token == "--session-id")
            .and_then(|at| spelled.get(at + 1))
            .cloned()
            .unwrap_or_default();

        let record = Arc::new(Mutex::new(Record {
            argv: spelled.clone(),
            cwd: env.cwd.display().to_string(),
            session_id,
            ..Record::default()
        }));

        let (wire_stdin, cli_stdin) = tokio::io::duplex(1 << 18);
        let (cli_stdout, wire_stdout) = tokio::io::duplex(1 << 18);
        let (exited, wait) = tokio::sync::oneshot::channel();

        // A conversation outlives its processes on this wire, so the script
        // is the **conversation's** answers and each process picks up where
        // the last one stopped. Without that, a fresh record after an
        // eviction or a refusal would replay the turn that caused it.
        let played: usize = self
            .spawns
            .lock()
            .expect("the spawn list")
            .iter()
            .filter(|spawn| spawn.cwd == env.cwd)
            .map(|spawn| spawn.record.lock().expect("the record").turns_played)
            .sum();
        let mut script = self.script.clone();
        script.turns = script.turns.split_off(played.min(script.turns.len()));

        self.spawns.lock().expect("the spawn list").push(Spawned {
            argv: spelled,
            cwd: env.cwd.clone(),
            record: Arc::clone(&record),
        });

        tokio::spawn(async move {
            let code =
                ganja_testkit::fake_claude::replay(cli_stdin, cli_stdout, &script, &record).await;
            record.lock().expect("the record").exit = code;
            let _ = exited.send(code);
        });

        let signals = Arc::clone(&self.signals);

        Ok(ChildIo {
            stdin: Box::new(wire_stdin),
            stdout: Box::new(wire_stdout),
            stderr: None,
            exit: Box::pin(async move {
                let code = wait.await.unwrap_or(0);

                Ok(exit_status(code))
            }),
            kill: Box::new(move |signal| {
                signals.lock().expect("the signal list").push(signal);
            }),
        })
    }
}

/// An `ExitStatus` carrying `code`.
fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;

    std::process::ExitStatus::from_raw(code << 8)
}

/// A provider wired to `cli`, with its state under `home`.
pub(crate) fn wired(cli: &Arc<FakeCli>, home: &Path) -> ClaudeCodeProvider {
    ClaudeCodeProvider::with_parts(
        PathBuf::from("/nonexistent/claude"),
        "2.1.263 (Claude Code)".to_owned(),
        Arc::clone(cli) as Arc<dyn Spawner>,
        super::binding::Paths::under(home),
    )
}

/// A script whose every turn answers `pong`.
pub(crate) fn says(answers: &[&str]) -> Script {
    Script {
        turns: answers
            .iter()
            .map(|answer| Turn {
                text: vec![(*answer).to_owned()],
                result: (*answer).to_owned(),
                ..Turn::default()
            })
            .collect(),
        ..Script::default()
    }
}

/// A user message with a chosen id.
pub(crate) fn user(id: &str, text: &str) -> Message {
    let mut message = Message::user(text);
    message.id = MessageId::from(id.to_owned());

    message
}

/// A **request-only** user message with a chosen id — the shape the engine's
/// guards block takes (**D556**, Dv-22).
pub(crate) fn guards(id: &str, text: &str) -> Message {
    let mut message = Message::request_only_user(text);
    message.id = MessageId::from(id.to_owned());

    message
}

/// An assistant message with a chosen id.
pub(crate) fn assistant(id: &str, text: &str) -> Message {
    let mut message = Message::assistant("claude-opus-5");
    message.id = MessageId::from(id.to_owned());
    message.parts.push(Part::text(text));

    message
}

/// An assistant message carrying one finished call.
pub(crate) fn called(id: &str, call_id: &str, output: &str) -> Message {
    let mut message = Message::assistant("claude-opus-5");
    message.id = MessageId::from(id.to_owned());
    message.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::Tool {
            call_id: call_id.to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({}),
                output: output.to_owned(),
                title: String::new(),
                metadata: serde_json::json!({}),
                started: 0,
                completed: 0,
            },
        },
    });

    message
}

/// The roster a conversation turn offers.
pub(crate) fn roster() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "read".to_owned(),
        description: "reads a file".to_owned(),
        schema: serde_json::json!({"type": "object"}),
    }]
}

/// An ordinary conversation request over `messages`.
pub(crate) fn request(messages: Vec<Message>, turn_start: usize) -> ChatRequest {
    ChatRequest {
        model: DEFAULT_MODEL.to_owned(),
        system: Some("you are ganja".to_owned()),
        messages,
        turn_start,
        tools: roster(),
        effort_options: serde_json::Map::new(),
    }
}

/// Drains a turn's events.
pub(crate) async fn drain(
    stream: futures::stream::BoxStream<'static, ProviderEvent>,
) -> Vec<ProviderEvent> {
    // Longer than the silence watchdog, so a test driving *that* is not cut
    // off by this: what this bound is for is a turn that wedges, which is a
    // failure to report rather than a suite to hang.
    drain_within(stream, super::held::SILENCE_BOUND * 4).await
}

/// Drains a turn's events under a bound of the caller's choosing.
pub(crate) async fn drain_within(
    stream: futures::stream::BoxStream<'static, ProviderEvent>,
    bound: Duration,
) -> Vec<ProviderEvent> {
    tokio::time::timeout(bound, stream.collect()).await.expect("a turn ends within the bound")
}

/// Runs one turn and returns what it produced.
pub(crate) async fn turn(
    provider: &ClaudeCodeProvider,
    request: ChatRequest,
) -> Vec<ProviderEvent> {
    let stream = provider.stream(request, CancellationToken::new()).await.expect("the turn opens");

    drain(stream).await
}

/// The text a turn streamed.
pub(crate) fn said(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The failure a turn ended with, if any.
pub(crate) fn failure(events: &[ProviderEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        ProviderEvent::Failed(error) => Some(error.to_string()),
        _ => None,
    })
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

// ----------------------------------------------------------- the version

#[test]
fn the_version_the_recording_was_made_on_parses_and_the_floor_is_the_surveyed_build() {
    assert_eq!(parse_version("2.1.266 (Claude Code)"), Some((2, 1, 266)));
    assert_eq!(parse_version("2.1.263 (Claude Code)"), Some((2, 1, 263)));
    assert_eq!(super::VERSION_FLOOR, (2, 1, 263));
}

#[test]
fn anything_that_is_not_the_clis_own_version_line_parses_as_nothing() {
    for said in
        ["2.1.263", "2.1 (Claude Code)", "v2.1.263 (Claude Code)", "2.1.263.4 (Claude Code)", ""]
    {
        assert_eq!(parse_version(said), None, "{said}");
    }
}

#[test]
fn a_build_below_the_floor_sorts_below_it_and_one_above_does_not() {
    assert!(parse_version("2.1.262 (Claude Code)").expect("it parses") < super::VERSION_FLOOR);
    assert!(parse_version("2.1.267 (Claude Code)").expect("it parses") > super::VERSION_FLOOR);
}

// ------------------------------------------- sent / honest / owed

#[test]
fn the_user_ids_of_a_request_are_read_over_the_whole_of_messages() {
    // Before and after `turn_start` alike: which side a message is on is the
    // engine's taxonomy, and this wire deliberately does not ask.
    let request =
        request(vec![user("m1", "first"), assistant("m2", "a reply"), user("m3", "second")], 2);

    assert_eq!(user_ids(&request), ["m1", "m3"]);
}

/// **D556**, Dv-22. A request-only message is written like any other user
/// message and remembered like none: [`owed`] spans it, [`remembered_ids`]
/// does not, and so [`honest`] cannot mistake the next one's fresh id for an
/// id the transcript lost.
///
/// The three assertions are the whole of the fix. The last one is what was
/// broken: with the guards block's id in `sent`, a second continuation
/// carrying a *different* guards id in the same position read as a rewind, and
/// the wire closed a perfectly good process.
#[test]
fn a_request_only_message_is_owed_every_time_and_remembered_never() {
    let first = request(vec![user("m1", "prompt"), guards("g1", "keep going")], 0);

    assert_eq!(user_ids(&first), ["m1", "g1"], "it is a user message like any other");
    assert_eq!(remembered_ids(&first), ["m1"], "and conversation state it is not");
    assert_eq!(
        owed(&["m1".to_owned()], &first).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["g1"],
        "so a record that has read the prompt still owes the block"
    );

    // The next request rebuilds the block with a fresh id. A record that
    // remembered `m1` alone is still honest about it; one that had remembered
    // `g1` would not have been.
    let second = request(vec![user("m1", "prompt"), guards("g2", "keep going")], 0);
    assert!(
        honest(&["m1".to_owned()], &second),
        "a fresh guards id is not an id the transcript lost"
    );
    assert!(
        !honest(&["m1".to_owned(), "g1".to_owned()], &second),
        "and the pre-Dv-22 memory is exactly what read as a rewind"
    );
}

/// The other side of the same coin: a **real** rewind still reads as one, so
/// AC-4.4's arm keeps its meaning.
#[test]
fn a_transcript_message_that_is_gone_is_still_a_rewind() {
    let rewound = request(vec![user("m1", "first")], 0);

    assert!(
        !honest(&["m1".to_owned(), "m3".to_owned()], &rewound),
        "an ordinary user message the request no longer carries is a rewind"
    );
}

#[test]
fn a_record_that_has_read_a_prefix_of_what_ganja_holds_is_honest() {
    let request = request(vec![user("m1", "first"), assistant("m2", "r"), user("m3", "second")], 2);

    assert!(honest(&[], &request), "a record that has read nothing is honest");
    assert!(honest(&["m1".to_owned()], &request));
    assert!(honest(&["m1".to_owned(), "m3".to_owned()], &request));
}

/// The predicate that tells a `/rewind` from every other reason a process
/// might be missing.
#[test]
fn a_record_holding_a_message_ganja_no_longer_has_is_not_honest() {
    let request = request(vec![user("m1", "first"), user("m3", "second")], 1);

    assert!(!honest(&["m1".to_owned(), "m2-rewound".to_owned()], &request));
    assert!(
        !honest(&["m1".to_owned(), "m3".to_owned(), "m4".to_owned()], &request),
        "a record ahead of the request is not honest either"
    );
}

#[test]
fn what_is_owed_is_every_user_message_the_record_has_not_read_in_request_order() {
    let request = request(
        vec![user("m1", "first"), assistant("m2", "r"), user("m3", "steer"), user("m4", "second")],
        3,
    );

    let owed: Vec<&str> =
        owed(&["m1".to_owned()], &request).iter().map(|message| message.id.as_str()).collect();

    assert_eq!(owed, ["m3", "m4"], "a steer and the new prompt, in request order");
}

#[test]
fn several_owed_messages_become_one_frames_text_in_request_order() {
    let request = request(vec![user("m1", "steer"), user("m2", "prompt")], 1);
    let owed = owed(&[], &request);

    assert_eq!(super::owed_text(&owed), "steer\n\nprompt");
}

// ------------------------------------------------------------- one-shot

/// `turn_start == 0` alone is **not** a one-shot marker: a conversation's own
/// first turn has it too. The empty roster is the other half.
#[tokio::test]
async fn a_title_request_spawns_a_process_that_enters_no_table_and_writes_no_binding() {
    let home = temp();
    let cli = FakeCli::new(says(&["a title"]));
    let provider = wired(&cli, home.path());

    let events = turn(
        &provider,
        ChatRequest {
            model: DEFAULT_MODEL.to_owned(),
            system: None,
            messages: vec![user("m1", "name this"), user("m2", "in five words")],
            turn_start: 0,
            tools: Vec::new(),
            effort_options: serde_json::Map::new(),
        },
    )
    .await;

    assert_eq!(said(&events), "a title");
    assert_eq!(provider.held_entries(), 0, "a one-shot never enters the table");
    assert_eq!(cli.count(), 1);

    let record = cli.record(0);
    assert!(
        record.argv.contains(&"--no-session-persistence".to_owned()),
        "its record is worth nothing afterwards"
    );
    assert!(
        record.tools_list.is_empty(),
        "no server is declared, so nothing ever dials: {:?}",
        record.tools_list
    );

    let root = home.path().join("ganja").join("claude-code");
    let bindings: Vec<_> = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "json"))
                .collect()
        })
        .unwrap_or_default();
    assert!(bindings.is_empty(), "a one-shot writes no binding");
}

/// The negative row: the two differ in `tools` alone.
#[tokio::test]
async fn a_first_conversation_turn_is_a_spawn_and_not_a_one_shot() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "hello")], 0)).await;

    assert_eq!(said(&events), "pong");
    assert_eq!(provider.held_entries(), 1, "a conversation's process is held");
    assert!(
        !cli.argv(0).contains(&"--no-session-persistence".to_owned()),
        "a held process's record is the continuity this wire has"
    );
    assert!(
        super::binding::load(
            &super::binding::Paths::under(home.path())
                .binding(&crate::provider::ids::derived(&MessageId::from("m1".to_owned())))
        )
        .is_some(),
        "and it writes a binding"
    );

    provider.shutdown().await;
}

// ---------------------------------------------------------- continuity

/// A second turn on a held process writes one `user` frame and no
/// `initialize`.
#[tokio::test]
async fn a_second_turn_rides_the_held_process_and_writes_one_frame() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    let events = turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2),
    )
    .await;

    assert_eq!(said(&events), "two");
    assert_eq!(cli.count(), 1, "one process, two turns");

    let record = cli.record(0);
    assert_eq!(record.user_frames.len(), 2);
    assert_eq!(record.user_frames[1], "second", "the newest run alone");
}

/// A request with nothing new is a turn about nothing.
#[tokio::test]
async fn a_continue_with_nothing_owed_is_failed_naming_the_key() {
    let home = temp();
    let cli = FakeCli::new(says(&["one"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    let events =
        turn(&provider, request(vec![user("m1", "first"), assistant("a1", "one")], 1)).await;

    let failure = failure(&events).expect("a turn about nothing is failed");
    assert!(failure.contains("no new message"), "{failure}");
    assert_eq!(cli.record(0).user_frames.len(), 1, "and nothing is written");
}

/// The D547/D549 shape: a block appended after a finished reply, with nothing
/// parked. It is a user message, so it is owed, so it rides one frame.
#[tokio::test]
async fn a_team_continuation_block_is_one_frame_on_the_held_process() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    turn(
        &provider,
        request(
            vec![user("m1", "first"), assistant("a1", "one"), user("m2", "CONTINUE: next task")],
            2,
        ),
    )
    .await;

    assert_eq!(cli.count(), 1, "no EOF, no --session-id, no preamble");
    assert_eq!(cli.record(0).user_frames[1], "CONTINUE: next task");
}

#[tokio::test]
async fn two_owed_messages_ride_one_frame_in_request_order() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    turn(
        &provider,
        request(
            vec![
                user("m1", "first"),
                assistant("a1", "one"),
                user("m2", "steer"),
                user("m3", "block"),
            ],
            3,
        ),
    )
    .await;

    assert_eq!(cli.record(0).user_frames[1], "steer\n\nblock", "two frames would be two turns");
}

// ------------------------------------------------------ the result arms

#[tokio::test]
async fn a_served_turn_reports_the_four_counters_and_finishes() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            text: vec!["pong".to_owned()],
            result: "pong".to_owned(),
            usage: ganja_testkit::fake_claude::Usage {
                input_tokens: 4,
                output_tokens: 277,
                cache_read_input_tokens: 3679,
                cache_creation_input_tokens: 3781,
            },
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    let usage = events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .expect("a served turn reports what it cost");
    assert_eq!(usage.input_tokens, 4);
    assert_eq!(usage.output_tokens, 277);
    assert_eq!(usage.cache_read_tokens, 3679);
    assert_eq!(usage.cache_write_tokens, 3781);
    assert_eq!(events.last(), Some(&ProviderEvent::Finish(FinishReason::Completed)));

    provider.shutdown().await;
}

/// The banner is the CLI's own, not model speech. Emitting it would put it in
/// ganja's transcript, which a later preamble would render as an
/// `[Assistant]` line — the exact shape the safeguard refused 3 of 3.
#[tokio::test]
async fn a_refused_turn_fails_naming_the_category_and_emits_no_text_at_all() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn { refused: true, ..Turn::default() }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    assert_eq!(said(&events), "", "the API Error banner is not model speech");
    let failure = failure(&events).expect("a refused turn fails");
    assert!(failure.contains("reasoning_extraction"), "{failure}");
    assert!(failure.contains("duplicating model outputs"), "verbatim: {failure}");
}

/// `is_error` is the field, and `subtype` is never consulted: a `result` with
/// `subtype: "success"` and `is_error: true` is a failure.
#[tokio::test]
async fn a_turn_whose_result_carries_is_error_is_failed_whatever_its_subtype_says() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn { refused: true, ..Turn::default() }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    assert!(failure(&events).is_some());
    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::Finish(FinishReason::Completed))),
        "a failed turn does not also finish"
    );
}

// ------------------------------------------------------------- thinking

#[tokio::test]
async fn readable_thinking_streams_and_two_blocks_are_separated() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            thinking: vec!["first thought".to_owned(), "second thought".to_owned()],
            text: vec!["pong".to_owned()],
            result: "pong".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    let thinking: Vec<&ProviderEvent> = events
        .iter()
        .filter(|event| {
            matches!(event, ProviderEvent::ReasoningDelta(_) | ProviderEvent::ReasoningBreak)
        })
        .collect();
    assert_eq!(
        thinking,
        [
            &ProviderEvent::ReasoningDelta("first thought".to_owned()),
            &ProviderEvent::ReasoningBreak,
            &ProviderEvent::ReasoningDelta("second thought".to_owned()),
        ],
        "without a break two summaries splice into one thought"
    );

    provider.shutdown().await;
}

/// Under no `--thinking-display` the text is withheld and only the signature
/// arrives, so there is nothing for a transcript to carry.
#[tokio::test]
async fn a_thinking_block_with_no_text_streams_nothing() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            thinking: vec![String::new()],
            text: vec!["pong".to_owned()],
            result: "pong".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    assert!(!events.iter().any(|event| matches!(event, ProviderEvent::ReasoningDelta(_))));

    provider.shutdown().await;
}

// -------------------------------------------------------- known frames

#[tokio::test]
async fn the_two_known_system_subtypes_and_an_unknown_one_all_leave_the_turn_alone() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            known_system: vec!["thinking_tokens".to_owned(), "status".to_owned()],
            unknown_system: Some("invented_for_this_test".to_owned()),
            text: vec!["pong".to_owned()],
            result: "pong".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    assert_eq!(said(&events), "pong");
    assert!(failure(&events).is_none(), "a frame this build skips does not fail a turn");

    provider.shutdown().await;
}

/// The CLI echoes every `control_response` this side sends. Every scripted
/// turn exercises the drop-by-`request_id`, because every one of them dials.
#[tokio::test]
async fn a_conversation_completes_although_every_answer_it_sent_comes_back_echoed() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    assert_eq!(said(&events), "pong");
    assert_eq!(
        cli.record(0).tools_list,
        ["read"],
        "the dial happened, so its answers were echoed and dropped"
    );

    provider.shutdown().await;
}

// ----------------------------------------------------------- the roster

/// Declared **bare**: the CLI prefixes what this side declares.
#[tokio::test]
async fn the_roster_reaches_the_cli_under_the_registrys_own_names() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    assert_eq!(cli.record(0).tools_list, ["read"]);

    provider.shutdown().await;
}

#[tokio::test]
async fn the_records_own_prompt_is_the_one_the_request_carried() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    assert_eq!(
        cli.record(0).system_prompt.as_deref(),
        Some(["you are ganja".to_owned()].as_slice())
    );

    provider.shutdown().await;
}

// ------------------------------------------------------- served model

/// The served name is the vendor's own spelling of what it chose. A request
/// cannot ask for a spelling, so it is surfaced and never compared.
#[tokio::test]
async fn the_served_model_is_none_until_a_turn_and_then_the_vendors_own_spelling() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    assert_eq!(provider.served_model(), None, "nothing has been served yet");

    turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    let served = provider.served_model().expect("a turn served something");
    assert_eq!(served.requested, DEFAULT_MODEL);
    assert_eq!(served.served, "claude-opus-5[1m]");

    provider.shutdown().await;
}

#[tokio::test]
async fn a_model_fallback_moves_the_served_model_and_no_process() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            fallback: Some("claude-sonnet-5".to_owned()),
            text: vec!["pong".to_owned()],
            result: "pong".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "ping")], 0)).await;

    assert_eq!(provider.served_model().expect("served").served, "claude-sonnet-5");
    assert_eq!(cli.count(), 1, "a spelling moves no process");
    assert_eq!(provider.held_entries(), 1);

    provider.shutdown().await;
}

/// Two spellings on two spawns of one key produce **no** divergence: the
/// entry is not closed and no third process appears.
#[tokio::test]
async fn two_served_spellings_on_one_key_produce_no_divergence() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![
            Turn {
                init_model: Some("claude-opus-5".to_owned()),
                text: vec!["one".to_owned()],
                result: "one".to_owned(),
                ..Turn::default()
            },
            Turn {
                init_model: Some("claude-opus-5[1m]".to_owned()),
                text: vec!["two".to_owned()],
                result: "two".to_owned(),
                ..Turn::default()
            },
        ],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    assert_eq!(provider.served_model().expect("served").served, "claude-opus-5");

    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2),
    )
    .await;

    assert_eq!(provider.served_model().expect("served").served, "claude-opus-5[1m]");
    assert_eq!(cli.count(), 1, "a spelling is never compared, so nothing respawned");

    provider.shutdown().await;
}

/// Every other builtin inherits `None` from the trait, which is the honest
/// answer for a wire whose vendor serves what it was asked and that holds no
/// process to evict — and inherits a `shutdown` that returns at once, which
/// is the honest answer for a wire that holds nothing between turns.
#[tokio::test]
async fn every_other_wire_inherits_none_and_a_shutdown_that_returns() {
    let fake = crate::provider::fake::FakeProvider::default();

    assert_eq!(fake.served_model(), None);
    assert_eq!(fake.last_eviction(), None);
    // It returns rather than merely compiles: a default that awaited
    // something would hang every frontend's exit path.
    tokio::time::timeout(Duration::from_secs(1), fake.shutdown())
        .await
        .expect("the inherited shutdown returns at once");
}

/// The three defaults are pinned against the **trait**, not against one wire.
///
/// Enumerating the builtins would prove less and cost more: most of them
/// cannot be constructed without a credential, and what matters is that a
/// wire which overrides nothing gets these three answers. So this is the
/// smallest `Provider` there is.
#[tokio::test]
async fn a_wire_that_overrides_nothing_gets_the_three_defaults() {
    struct Inherits;

    #[async_trait::async_trait]
    impl crate::provider::Provider for Inherits {
        fn id(&self) -> &str {
            "inherits"
        }

        async fn stream(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
            Ok(futures::stream::empty().boxed())
        }
    }

    let wire = Inherits;

    assert_eq!(wire.served_model(), None);
    assert_eq!(wire.last_eviction(), None);
    assert!(wire.rate_windows().is_empty());
    assert!(wire.plan_windows().is_empty());
    tokio::time::timeout(Duration::from_secs(1), wire.shutdown())
        .await
        .expect("a wire that holds nothing closes nothing");
}

/// And the one override in the workspace does the closing, reached through
/// `dyn Provider` — which is how a frontend will call it.
#[tokio::test]
async fn the_one_wire_that_holds_processes_closes_them_through_the_trait() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider: Arc<dyn crate::provider::Provider> = Arc::new(wired(&cli, home.path()));

    let request = request(vec![user("m1", "hello")], 0);
    let stream = provider.stream(request, CancellationToken::new()).await.expect("the turn opens");
    drain(stream).await;

    provider.shutdown().await;

    assert_eq!(cli.record(0).exit, 0, "the child was closed, by EOF");
    assert!(cli.signals.lock().expect("the signal list").is_empty(), "and closed the orderly way");
}

// ------------------------------------------------------- the tool bridge

/// Nothing in this crate runs a tool: the ask is surfaced, the engine runs
/// it, and the next `stream()` carries the result.
#[tokio::test]
async fn a_tool_ask_is_surfaced_as_an_ordinary_call_and_the_step_ends() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn {
            tool_calls: vec![ganja_testkit::fake_claude::Call {
                id: "toolu_1".to_owned(),
                name: "read".to_owned(),
                input: serde_json::json!({"path": "/f"}),
                call_first: false,
            }],
            result: "pong".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "read it")], 0)).await;

    assert_eq!(
        events[0],
        ProviderEvent::ToolCallStart { id: "toolu_1".to_owned(), name: "read".to_owned() },
        "the registry's own name, not the model-facing one"
    );
    assert_eq!(
        events[1],
        ProviderEvent::ToolCallDelta {
            id: "toolu_1".to_owned(),
            json: r#"{"path":"/f"}"#.to_owned(),
        }
    );
    assert_eq!(events[2], ProviderEvent::ToolCallEnd { id: "toolu_1".to_owned() });
    assert_eq!(events[3], ProviderEvent::Finish(FinishReason::Completed));
    assert_eq!(events.len(), 4, "the step ends there; the engine runs the tool next");

    assert_eq!(provider.held_entries(), 1, "and the process is still held, waiting");

    provider.shutdown().await;
}

/// The execution-site invariant, stated where a grep will find it.
///
/// The two names are **assembled** rather than written, so that the gate's
/// own `grep -rn` over `src/provider/claude_code*` finds nothing — including
/// this file, which that glob also matches. A test that spelled them would
/// make the gate it exists to serve report itself.
#[test]
fn this_crates_claude_code_modules_name_no_tool_runtime() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provider");
    let mut sources = vec![root.join("claude_code.rs")];
    sources.extend(
        std::fs::read_dir(root.join("claude_code"))
            .expect("the module directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|kind| kind == "rs")),
    );

    for source in sources {
        let text = std::fs::read_to_string(&source).expect("a source file");
        for name in [concat!("Tool", "Ctx"), concat!("Registry", "::")] {
            assert!(
                !text.contains(name),
                "{} names {name}: nothing in this crate may run a tool",
                source.display()
            );
        }
    }
}

/// Every child this wire spawns runs in a directory this wire sealed — the
/// `--version` probe included, which used to be handed the shared temporary
/// directory, per-user on macOS and world-writable `/tmp` on Linux, while every
/// turn-taking child already ran in a `0700` scratch directory (CC-10).
///
/// The name is **assembled** for the reason the test above gives: the glob it
/// reads is the one a `grep -rn` over the module would.
#[test]
fn no_source_of_this_wire_hands_a_child_the_shared_temporary_directory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provider");
    let mut sources = vec![root.join("claude_code.rs")];
    sources.extend(
        std::fs::read_dir(root.join("claude_code"))
            .expect("the module directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|kind| kind == "rs")),
    );

    let name = concat!("temp", "_dir");
    for source in sources {
        let text = std::fs::read_to_string(&source).expect("a source file");
        assert!(
            !text.contains(name),
            "{} names {name}: a child of this wire runs in a directory the wire sealed",
            source.display()
        );
    }
}

// ------------------------------------------- the plan windows (Dv-19)

/// **D556**, Dv-19. Both recorded shapes convert to the **same** window, and
/// what the wire answers is `plan_windows` — never `rate_windows`, which stays
/// empty because this wire receives no rate-limit headers at all.
///
/// The two shapes are the recording's own: `rate_limit_event` sends a fraction
/// and a unix second (`claude-code-replay-run1.json:328-337`), `get_usage` an
/// integer percent and an ISO-8601 string (`:712-721`). One reader, because a
/// caller that had to know which it was holding would be a second place for
/// the two to disagree.
#[tokio::test]
async fn both_recorded_rate_shapes_convert_to_the_same_plan_windows() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(says(&["hi"]));
    let wire = wired(&cli, home.path());

    // The event's own shape: fraction, unix second, and the window it is about
    // named once beside the set.
    *wire.slots.rate.lock().expect("the rate slot") = Some(serde_json::json!({
        "status": "allowed",
        "resetsAt": 1_788_982_800_u64,
        "rateLimitType": "five_hour",
        "unifiedWindows": {
            "five_hour": {"utilization": 0.69, "resetsAt": 1_788_982_800_u64},
            "seven_day": {"utilization": 0.45, "resetsAt": 1_789_005_600_u64},
        }
    }));
    let from_event = wire.plan_windows();

    // The control response's: integer percent, ISO-8601, and the map itself.
    *wire.slots.rate.lock().expect("the rate slot") = Some(serde_json::json!({
        "five_hour": {"utilization": 69, "resets_at": "2026-09-09T19:40:00.000Z"},
        "seven_day": {"utilization": 45, "resets_at": "2026-09-10T02:00:00.000Z"},
    }));
    let from_usage = wire.plan_windows();

    for (event, usage) in from_event.iter().zip(&from_usage) {
        assert_eq!(event.name, usage.name);
        assert!(
            (event.used_percent - usage.used_percent).abs() < 1e-9,
            "the same utilization either way: {event:?} vs {usage:?}"
        );
        assert_eq!(event.resets_at, usage.resets_at, "and the same instant: {event:?}");
        assert_eq!(event.window_minutes, usage.window_minutes);
    }
    assert_eq!(from_event.len(), 2, "the vendor's two windows: {from_event:?}");
    assert_eq!(from_event[0].name, "five_hour");
    assert!((from_event[0].used_percent - 69.0).abs() < 1e-9, "{from_event:?}");
    assert_eq!(from_event[0].window_minutes, Some(300));
    assert_eq!(from_event[1].name, "seven_day");
    assert_eq!(from_event[1].window_minutes, Some(10_080));
    assert_eq!(
        from_event[0].limit_name.as_deref(),
        Some("five_hour"),
        "the window this account is being judged against, named beside the set"
    );

    // The negative row: nothing here is a throttling budget.
    assert!(
        wire.rate_windows().is_empty(),
        "this wire hears no rate-limit headers, and says so rather than inventing counts"
    );
}

/// An unwarmed call answers `rate_limits: null`, and **no reading** is an
/// empty list — never a window at zero, which would draw as a budget freshly
/// full.
#[tokio::test]
async fn an_unwarmed_account_reports_no_windows_rather_than_empty_ones() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(says(&["hi"]));
    let wire = wired(&cli, home.path());

    assert!(wire.plan_windows().is_empty(), "nothing has been heard yet");

    *wire.slots.rate.lock().expect("the rate slot") = Some(serde_json::Value::Null);
    assert!(wire.plan_windows().is_empty(), "and a null reading is still nothing");
}

// ------------------------------------------- the compaction shape (Dv-21)

/// **AC-4.5**, at the provider seam (**Dv-21**). A compaction summary runs as
/// a **one-shot** that enters no table entry, and the conversation turn after
/// it opens a fresh record whose opening frame is the **prompt alone** — no
/// summary text anywhere.
///
/// Driven here rather than through `Command::Compact`, which on this wire does
/// nothing at all: `session.rs`'s manual-compaction path needs a context
/// window to know what fits, only the catalog can say, and this provider has no
/// rows (`session.rs:2862-2874`). What the AC is *about* is the wire's own
/// behaviour when a summary is `messages[0]`, and that is exactly what a
/// request with an empty roster and `turn_start == 0` produces — the one-shot
/// marker `stream()` reads.
///
/// This is §ADR 2's stated cost, pinned so nobody later reads it as a bug. On a
/// wire that never resumes, the summary is assistant text, and assistant text
/// is the one thing the vendor safeguard refused every time it was offered — so
/// what survives a compaction here is the user's own words.
#[tokio::test]
async fn a_compaction_summary_is_a_one_shot_and_reaches_the_next_record_as_nothing() {
    const SUMMARY: &str = "SUMMARY-OF-EVERYTHING-SO-FAR";

    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(says(&["a title", "carrying on"]));
    let wire = wired(&cli, home.path());

    // The summary request: one message, no roster. Both halves are what make
    // it a one-shot, and the roster is the half that matters — a conversation's
    // own first turn has `turn_start == 0` too, and offers tools.
    let mut summary = request(vec![user("m1", "summarize the conversation so far")], 0);
    summary.tools = Vec::new();
    drain(wire.stream(summary, CancellationToken::new()).await.expect("the one-shot runs")).await;

    assert_eq!(wire.held_entries(), 0, "a one-shot enters no table entry of its own");
    assert_eq!(cli.count(), 1, "it did spawn, though");
    assert!(
        cli.record(0).cwd.ends_with("one-shot"),
        "in the scratch directory every one-shot shares: {}",
        cli.record(0).cwd
    );

    // The conversation that follows it: the summary is the assistant's, so the
    // engine carries it as an assistant message and the wire renders it as
    // nothing at all.
    let carried = request(vec![assistant("m2", SUMMARY), user("m3", "carry on then")], 1);
    drain(wire.stream(carried, CancellationToken::new()).await.expect("the turn runs")).await;

    assert_eq!(cli.count(), 2, "the turn after a compaction opens a record of its own");
    assert_eq!(wire.held_entries(), 1, "and that one is the conversation's, held");

    let opening = cli.record(1).user_frames.first().cloned().expect("it was handed a frame");
    assert!(opening.contains("carry on then"), "which opens with the prompt: {opening:?}");
    assert!(
        !opening.contains(SUMMARY),
        "and carries no summary text at all, which is what ADR 2 costs: {opening:?}"
    );
    assert!(!opening.contains("[Assistant]"), "no assistant label either: {opening:?}");
}
