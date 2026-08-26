use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{FutureExt as _, StreamExt as _, stream::BoxStream};
use ganja_core::{
    Engine, SessionId, SessionInfo, Storage,
    provider::{FakeProvider, fake},
    storage::VERSION,
};
use ganja_protocol::{
    Event as CoreEvent, FinishReason, HeldId, HeldOutcome, HoldCause, Message, Part, PartBody,
    PartId, PermissionId, PermissionReply, QuestionId, QuestionInfo, QuestionOption, RedactedText,
    ToolState, Usage,
};
use ratatui::{
    Terminal,
    backend::{Backend, ClearType, TestBackend},
    crossterm::event::{
        Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    style::{Color, Modifier},
};
use tempfile::TempDir;

use super::{
    App, BACKTRACK_HINT, Chooser, Cleared, Dropdown, ESC_CHORD, FRAME, Help, JoinHandle,
    ListDialog, MAX_EVENT_LOG, MessageId, Mode, NO_EFFORTS, Palette, PendingDialog, Permission,
    RevertScope, Rewind, WireListing, permission_reply,
};

/// The session every hand-built fixture event happens in. One pinned id,
/// used consistently, so a test that one day cares which session an event
/// named has something stable to assert on.
fn session() -> SessionId {
    SessionId::from("ses_fixture".to_owned())
}
use ganja_tool::registry;

use crate::{
    binder, clipboard, command,
    component::{self, effort, files::Row as MenuRow, mcp, sessions},
    escrepair::EscRepair,
    event::AppEvent,
    history, lister, mention,
    theme::{DEFAULT_THEME, Themes},
};

fn engine() -> Engine {
    engine_asking(fake::MODEL)
}

/// The same, asking for a model of the caller's choosing — which is what
/// the pricing tests need, since prices come from a catalog the fake
/// model is not in.
fn engine_asking(model: &str) -> Engine {
    Engine::new(
        Arc::new(FakeProvider::default()),
        model,
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
    )
}

/// An app over the builtin themes: no disk is read, so a test never
/// sees the machine's own theme directory or stored pick.
fn app() -> App {
    App::new(engine(), None, Themes::builtin())
}

/// An app whose engine writes into, and lists from, a store in `directory`.
///
/// The picker asks the engine what it holds, so a picker test needs the
/// real storage path rather than a stub: what it renders is what
/// [`Engine::sessions`] read off the disk.
fn persistent_app(directory: &TempDir) -> App {
    App::new(
        Engine::persistent(
            Arc::new(FakeProvider::default()),
            fake::MODEL,
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
            Storage::open(directory.path().join("storage")),
        ),
        None,
        Themes::builtin(),
    )
}

/// Stores one session under `id`, last touched `ago` milliseconds before
/// `now`, carrying `tokens` of accumulated input and one message.
fn store_session(
    directory: &TempDir,
    id: &str,
    title: Option<&str>,
    now: u64,
    ago: u64,
    tokens: u64,
) {
    let storage = Storage::open(directory.path().join("storage"));
    let updated = now.saturating_sub(ago);
    let info = SessionInfo {
        effort: None,
        id: SessionId::from(id.to_owned()),
        version: VERSION,
        title: title.map(str::to_owned),
        created: updated,
        updated,
        usage: Usage {
            input_tokens: tokens,
            ..Usage::default()
        },
        context_tokens: 0,
        summary: None,
        agent: None,
        model: None,
        activated_tools: std::collections::BTreeSet::new(),
        parent: None,
        revert: None,
    };
    let message = Message::user("what the picker is choosing between");

    storage.save_info(&info).expect("the info stores");
    storage
        .save_message(&info.id, &message)
        .expect("the envelope stores");
    for part in &message.parts {
        storage
            .save_part(&info.id, &message.id, part)
            .expect("the part stores");
    }
}

/// Stores one session under `id` that a task call on `parent` spawned.
fn store_child(directory: &TempDir, id: &str, parent: &str) {
    let storage = Storage::open(directory.path().join("storage"));
    let info = SessionInfo {
        effort: None,
        id: SessionId::from(id.to_owned()),
        version: VERSION,
        title: Some("find the parser (@explore subagent)".to_owned()),
        created: 1_000,
        updated: 1_000,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent: Some("explore".to_owned()),
        model: None,
        activated_tools: std::collections::BTreeSet::new(),
        parent: Some(SessionId::from(parent.to_owned())),
        revert: None,
    };

    storage.save_info(&info).expect("the info stores");
}

/// The three sessions the picker snapshots render: one titled and recent,
/// one that never earned a title, and one old enough to be listed in
/// hours.
///
/// Ages are written as offsets from the clock the picker itself reads when
/// it opens, because a row says how long ago a session was touched. Fixed
/// timestamps would re-age every day and rot the snapshot; an offset
/// renders the same interval forever.
fn store_pickable_sessions(directory: &TempDir) {
    const MINUTE: u64 = 60 * 1_000;
    const HOUR: u64 = 60 * MINUTE;

    let now = sessions::now();

    store_session(
        directory,
        "0198f2c4-a1b0-7000-8000-000000000011",
        Some("porting the session store"),
        now,
        30 * 1_000,
        12_400,
    );
    store_session(
        directory,
        "0198f2c4-a1b0-7000-8000-000000000012",
        None,
        now,
        5 * MINUTE,
        1_234,
    );
    store_session(
        directory,
        "0198f2c4-a1b0-7000-8000-000000000013",
        Some("a first look at the tool registry"),
        now,
        3 * HOUR,
        42,
    );
}

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// An app plus the engine stream its own loop would read, for the tests
/// that need a prompt to travel the whole way and come back.
async fn wired() -> (App, BoxStream<'static, CoreEvent>) {
    let engine = engine();
    let events = engine.subscribe().await.expect("the test subscribes first");

    (App::new(engine, None, Themes::builtin()), events)
}

/// An app whose real turn is stopped inside the `question` tool, plus the
/// stream the frontend would be reading while the dialog is open.
async fn questioning() -> (TempDir, App, BoxStream<'static, CoreEvent>) {
    let directory = temporary();
    let script = directory.path().join("question.json");
    fs::write(
        &script,
        r#"{
                "cadence_ms": 0,
                "turns": [
                    {
                        "tool_calls": [{
                            "name": "question",
                            "args": {
                                "questions": [{
                                    "question": "Which database should the service use?",
                                    "header": "Database",
                                    "options": [
                                        {"label": "Postgres", "description": "Relational database"},
                                        {"label": "SQLite", "description": "One local file"}
                                    ]
                                }]
                            }
                        }]
                    },
                    {"text": "Thanks."}
                ]
            }"#,
    )
    .expect("the fake-provider script writes");

    let engine = Engine::new(
        Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script)),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(vec![Arc::new(
            ganja_tool::question::QuestionTool,
        )])),
        ganja_permission::Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin());
    app.engine
        .send(ganja_protocol::Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts the prompt");

    for _ in 0..64 {
        let event = next_event(&mut events).await;
        let asked = matches!(event, CoreEvent::QuestionAsked { .. });
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
        if asked {
            return (directory, app, events);
        }
    }

    panic!("the scripted turn did not ask its question");
}

/// The next engine event, bounded so a broken dialog test fails rather
/// than hanging the whole suite in the condition it is meant to prevent.
async fn next_event(events: &mut BoxStream<'static, CoreEvent>) -> CoreEvent {
    tokio::time::timeout(Duration::from_secs(2), events.next())
        .await
        .expect("the engine reports before the dialog timeout")
        .expect("the engine keeps reporting")
}

/// Feeds the app the next `count` engine events.
async fn pump(app: &mut App, events: &mut BoxStream<'static, CoreEvent>, count: usize) {
    for _ in 0..count {
        let event = events.next().await.expect("the engine keeps reporting");
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
    }
}

fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(width, height)).expect("a test terminal is buildable")
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area = buffer.area();

    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A screen dump carrying style as well as text.
///
/// [`screen`] reads `.symbol()` only, which is what lets every layout
/// snapshot survive a change of palette — and what makes it useless for
/// pinning one. This one emits each row as the runs of cells that share a
/// style, with the colors and modifiers the backend was actually handed.
fn styled_screen(terminal: &Terminal<TestBackend>) -> String {
    /// One run of same-styled cells.
    fn run(text: &str, (fg, bg, modifier): (Color, Color, Modifier)) -> String {
        let mut described = format!("{text:?} {fg:?} on {bg:?}");
        if !modifier.is_empty() {
            described.push_str(&format!(" {modifier:?}"));
        }

        format!("[{described}]")
    }

    let buffer = terminal.backend().buffer();
    let area = buffer.area();

    (0..area.height)
        .map(|row| {
            let mut runs = Vec::new();
            let mut text = String::new();
            let mut style: Option<(Color, Color, Modifier)> = None;

            for column in 0..area.width {
                let cell = &buffer[(column, row)];
                let cell_style = (cell.fg, cell.bg, cell.modifier);

                if style != Some(cell_style) {
                    if let Some(previous) = style {
                        runs.push(run(&text, previous));
                    }
                    text.clear();
                    style = Some(cell_style);
                }
                text.push_str(cell.symbol());
            }
            if let Some(previous) = style {
                runs.push(run(&text, previous));
            }

            format!("{row:>2} {}", runs.join(" "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An app drawing in the builtin theme `name`.
fn themed_app(name: &str) -> App {
    let mut themes = Themes::builtin();
    themes
        .select(name)
        .unwrap_or_else(|| panic!("{name} should be a builtin theme"));

    App::new(engine(), None, themes)
}

/// The transcript the per-theme snapshots are taken over: a prompt, a
/// reply, an edit carrying a diff and a call that was refused — between
/// them every role the transcript paints.
fn palette_transcript(app: &mut App) {
    app.chat
        .start_message(Message::user("show me every color you have"));

    let mut reply = Message::assistant("canned");
    reply
        .parts
        .push(Part::text("One edit applied, one command refused."));
    reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "edit".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "theme.rs"}),
                    output: "edited theme.rs".to_owned(),
                    title: "theme.rs".to_owned(),
                    metadata: serde_json::json!({
                        "diff": "@@ -1,2 +1,2 @@\n-let theme = Theme::default();\n+let theme = themes.theme();\n context"
                    }),
                    started: 0,
                    completed: 1,
                },
            },
        });
    reply.parts.push(Part {
        id: PartId::from("prt_2".to_owned()),
        body: PartBody::Tool {
            call_id: "call_2".to_owned(),
            tool: "bash".to_owned(),
            state: ToolState::Error {
                input: serde_json::json!({"command": "rm -rf /"}),
                error: "refused: destructive command".to_owned(),
                started: 0,
                completed: 1,
            },
        },
    });
    app.chat.start_message(reply);
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
    AppEvent::Term(TermEvent::Key(KeyEvent::new(code, modifiers)))
}

fn typing(text: &str) -> impl Iterator<Item = AppEvent> + use<'_> {
    text.chars()
        .map(|character| key(KeyCode::Char(character), KeyModifiers::NONE))
}

/// **D516.** The repaired stream drives the same editor a direct key
/// would: a split Left arrow lands as cursor movement, never as `[D`
/// text, and the phantom Esc never reaches the key handler at all.
#[tokio::test]
async fn a_split_arrow_repaired_by_the_machine_edits_the_composer() {
    let mut app = app();
    for event in typing("ab") {
        app.handle(event).await.expect("typing is handled");
    }

    let mut machine = EscRepair::active();
    let base = std::time::Instant::now();
    let split = [
        (KeyCode::Esc, KeyModifiers::NONE),
        (KeyCode::Char('['), KeyModifiers::NONE),
        // crossterm marks uppercase ASCII with SHIFT, so the final does.
        (KeyCode::Char('D'), KeyModifiers::SHIFT),
    ];
    let mut repaired = Vec::new();
    for (at, (code, modifiers)) in split.into_iter().enumerate() {
        repaired.extend(machine.accept(
            TermEvent::Key(KeyEvent::new(code, modifiers)),
            base + std::time::Duration::from_millis(at as u64),
        ));
    }

    for event in repaired {
        app.handle(AppEvent::Term(event))
            .await
            .expect("the repaired key is handled");
    }
    for event in typing("X") {
        app.handle(event).await.expect("typing is handled");
    }

    assert_eq!(app.editor.text(), "aXb");
}

/// The prompt reaches the transcript through the engine, not through the
/// editor: the frontend never invents an entry.
#[tokio::test]
async fn enter_submits_the_prompt_and_the_engine_puts_it_in_the_transcript() {
    let (mut app, mut events) = wired().await;
    for event in typing("hello") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.editor.prompt().is_none(), "the editor should be empty");

    let mut terminal = terminal(40, 12);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        !screen(&terminal).contains("hello"),
        "the transcript should wait for the engine:\n{}",
        screen(&terminal)
    );

    pump(&mut app, &mut events, 1).await;

    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("hello"),
        "the prompt should be in the transcript:\n{}",
        screen(&terminal)
    );
}

#[tokio::test]
async fn modified_enter_inserts_a_newline_instead_of_submitting() {
    for modifier in [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
    ] {
        let mut app = app();
        for event in typing("one") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, modifier))
            .await
            .expect("modified enter is handled");
        for event in typing("two") {
            app.handle(event).await.expect("typing is handled");
        }

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("one\ntwo"),
            "{modifier:?}+Enter should break the line"
        );
    }
}

/// Ctrl+J is the universal line break even when a terminal cannot report modified Enter.
#[tokio::test]
async fn ctrl_j_inserts_a_newline_and_submits_nothing() {
    let mut app = app();
    for event in typing("one") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+j is handled");
    for event in typing("two") {
        app.handle(event).await.expect("typing is handled");
    }

    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("one\ntwo"),
        "ctrl+j should break the line, not submit; a submit would have cleared the buffer"
    );
}

#[tokio::test]
async fn a_bare_q_types_instead_of_quitting() {
    let mut app = app();

    app.handle(key(KeyCode::Char('q'), KeyModifiers::NONE))
        .await
        .expect("typing is handled");

    assert!(!app.quit);
    assert_eq!(app.editor.prompt().as_deref(), Some("q"));
}

#[tokio::test]
async fn control_c_and_control_q_quit() {
    for code in [KeyCode::Char('c'), KeyCode::Char('q')] {
        let mut app = app();

        app.handle(key(code, KeyModifiers::CONTROL))
            .await
            .expect("a quit key is handled");

        assert!(app.quit, "{code:?} with Control should quit");
    }
}

/// **F4.** What used to be refused is now steered: a second Enter while a
/// turn holds the engine hands the text to *that* turn and shows it
/// waiting, rather than bouncing it off the Busy contract — which the
/// engine still keeps, and which this frontend simply stops asking about.
#[tokio::test]
async fn a_second_prompt_mid_turn_is_steered_into_the_running_turn() {
    let (mut app, mut events) = wired().await;
    for event in typing("first") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    // Both message envelopes, so the turn is visibly under way.
    pump(&mut app, &mut events, 2).await;
    assert!(app.turn_running, "the fixture needs a turn in flight");

    for event in typing("second") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(
        app.editor.is_empty(),
        "a steered message leaves the composer, like any accepted one"
    );
    assert_eq!(app.queue.depth(), 1);
    assert!(
        app.queue.entries()[0].is_steered(),
        "the engine took it, so the strip waits on SteerConsumed"
    );

    let mut terminal = terminal(100, 12);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(
        !screen.contains("already streaming"),
        "nothing was refused, so nothing should say so:\n{screen}"
    );
    assert!(screen.contains("second"), "got:\n{screen}");
    assert!(screen.contains("1 queued"), "got:\n{screen}");
}

/// Drives the real engine, so this covers the whole path a keystroke takes:
/// Esc becomes a command, the engine stops the turn, and the finish event
/// turns the status bar over.
#[tokio::test]
async fn escape_stops_a_streaming_turn_inside_the_budget() {
    const CANCEL_BUDGET: Duration = Duration::from_millis(100);

    let (mut app, mut events) = wired().await;

    for event in typing("hello") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    // Both envelopes, the reply's part, and a fragment in it: the turn is
    // actually streaming before it gets interrupted.
    pump(&mut app, &mut events, 4).await;
    assert!(app.status.is_streaming());

    let issued = Instant::now();
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    while app.status.is_streaming() {
        let event = events.next().await.expect("the engine keeps reporting");
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
    }
    let elapsed = issued.elapsed();

    assert!(
        elapsed < CANCEL_BUDGET,
        "the turn took {elapsed:?} to stop, budget is {CANCEL_BUDGET:?}"
    );

    let mut terminal = terminal(80, 12);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("stopped"),
        "the status bar should report the cancel:\n{}",
        screen(&terminal)
    );
}

#[tokio::test]
async fn escape_while_idle_changes_nothing() {
    let mut app = app();

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    assert!(!app.status.is_streaming());
    assert!(!app.quit);
    assert!(app.editor.prompt().is_none());
}

#[tokio::test]
async fn streamed_fragments_land_in_one_entry() {
    let mut app = app();
    let reply = Message::assistant("canned");
    let part = Part::text("");

    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");
    app.handle(AppEvent::core(CoreEvent::PartStarted {
        session_id: session(),
        message_id: reply.id.clone(),
        part: part.clone(),
    }))
    .await
    .expect("a part start is handled");
    for fragment in ["stream", "ed ", "reply"] {
        app.handle(AppEvent::core(CoreEvent::PartDelta {
            session_id: session(),
            message_id: reply.id.clone(),
            part_id: part.id.clone(),
            delta: fragment.to_owned(),
        }))
        .await
        .expect("a fragment is handled");
    }

    assert!(app.status.is_streaming());

    let mut terminal = terminal(40, 12);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("streamed reply"),
        "fragments should join:\n{}",
        screen(&terminal)
    );

    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: reply.id,
        reason: FinishReason::Cancelled,
        usage: None,
        error: None,
        completed: 0,
    }))
    .await
    .expect("a turn end is handled");
    assert!(!app.status.is_streaming());
}

/// The provider's dying words land in the transcript, where the person is
/// looking, and not in the status bar — whose one line keeps only the
/// failed state. A failure whose reply never started still lands
/// somewhere: the notice.
#[tokio::test]
async fn a_provider_error_lands_in_the_transcript_and_not_the_status_bar() {
    let mut app = app();
    let reply = Message::assistant("canned");
    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");
    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: reply.id,
        reason: FinishReason::Failed,
        usage: None,
        error: Some("Our servers are currently overloaded.".to_owned()),
        completed: 0,
    }))
    .await
    .expect("a turn end is handled");

    let mut terminal = terminal(120, 12);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("[error] Our servers are currently overloaded."),
        "the transcript carries the provider's words, got:\n{}",
        screen(&terminal)
    );
    assert!(
        !status_line(&mut app).contains("overloaded"),
        "the status bar does not repeat them: {}",
        status_line(&mut app)
    );

    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: MessageId::from("msg_ghost".to_owned()),
        reason: FinishReason::Failed,
        usage: None,
        error: Some("dead before a word".to_owned()),
        completed: 0,
    }))
    .await
    .expect("a turn end is handled");
    assert!(
        status_line(&mut app).contains("dead before a word"),
        "a failure with no reply entry still lands somewhere: {}",
        status_line(&mut app)
    );
}

/// A turn the provider refused ends the same way any other does, with the
/// reason on screen.
#[tokio::test]
async fn a_failed_turn_reports_why_in_the_status_bar() {
    let mut app = app();

    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: Message::assistant("canned").id,
        reason: FinishReason::Failed,
        usage: None,
        error: Some("no usable credentials".to_owned()),
        completed: 0,
    }))
    .await
    .expect("a turn end is handled");

    let mut terminal = terminal(80, 12);
    app.draw(&mut terminal).expect("a frame draws");

    assert!(!app.status.is_streaming());
    assert!(
        screen(&terminal).contains("no usable credentials"),
        "the failure should be explained:\n{}",
        screen(&terminal)
    );
}

/// Builds the finish event a provider that reported its usage produces.
fn finished(model: &str, usage: Usage) -> AppEvent {
    AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: Message::assistant(model).id,
        reason: FinishReason::Completed,
        usage: Some(usage),
        error: None,
        completed: 0,
    })
}

/// Spend is per session, not per turn, so two turns add up — and the sum
/// counts cache traffic, which is billed and is otherwise invisible.
#[tokio::test]
async fn usage_accumulates_across_turns_and_reaches_the_status_bar() {
    const MODEL: &str = "claude-sonnet-5";

    let mut app = App::new(engine_asking(MODEL), None, Themes::builtin());
    let usage = Usage {
        input_tokens: 6_000,
        output_tokens: 400,
        reasoning_tokens: 200,
        cache_read_tokens: 6_000,
        cache_write_tokens: 300,
    };

    for _ in 0..2 {
        app.handle(finished(MODEL, usage))
            .await
            .expect("a turn end is handled");
    }

    assert_eq!(
        app.totals.input_tokens, 24_600,
        "6,000 + 6,000 + 300, twice"
    );
    assert_eq!(app.totals.output_tokens, 800);

    let mut terminal = terminal(100, 12);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);

    assert!(screen.contains("24.6k in"), "got:\n{screen}");
    assert!(screen.contains("800 out"), "got:\n{screen}");
    // Two turns of $0.01795: input 6k at $2, 6k cached at $0.20, 300
    // written at $2.50, output 400 at $10, all per million tokens.
    assert!(screen.contains("$0.0359"), "got:\n{screen}");
}

/// **Non-vacuity target for asking the engine which model it is asking.**
/// The default agent names a model of its own, so the engine has already
/// left the one the process was launched with by the time the screen is
/// built. Handing the launch model to the app instead — what the startup
/// path used to do — prices this turn against a model nothing asked for,
/// and the fake one has no price at all, so the dollars vanish.
#[tokio::test]
async fn a_default_agents_own_model_is_what_the_first_turn_is_priced_against() {
    const MODEL: &str = "claude-sonnet-5";

    let config: ganja_core::config::Config = serde_json::from_value(serde_json::json!({
        "default_agent": "review",
        "agent": { "review": { "mode": "primary", "model": format!("anthropic/{MODEL}") } }
    }))
    .expect("the fixture is a config");
    let engine = engine().with_agents(Arc::new(
        ganja_core::AgentRegistry::from_config(&config).expect("the fixture resolves an agent"),
    ));
    assert_ne!(
        engine.model(),
        fake::MODEL,
        "the fixture only proves anything while the agent moves the engine off the launch model"
    );

    let mut app = App::new(engine, None, Themes::builtin());
    app.handle(finished(
        MODEL,
        Usage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            ..Usage::default()
        },
    ))
    .await
    .expect("a turn end is handled");

    assert_eq!(app.model, MODEL);
    // A million input tokens at $2 per million.
    assert_eq!(app.totals.cost_usd, Some(2.0));
}

/// The fake provider reports usage against a model with no price, which is
/// exactly the case that must show counts and no dollars.
#[tokio::test]
async fn an_unpriced_model_reports_its_tokens_and_invents_no_price() {
    let mut app = app();

    app.handle(finished(
        fake::MODEL,
        Usage {
            input_tokens: 40,
            output_tokens: 7,
            ..Usage::default()
        },
    ))
    .await
    .expect("a turn end is handled");

    assert_eq!(app.totals.cost_usd, None);

    let mut terminal = terminal(100, 12);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);

    assert!(screen.contains("40 in"), "got:\n{screen}");
    assert!(screen.contains("7 out"), "got:\n{screen}");
    assert!(!screen.contains('$'), "got:\n{screen}");
}

/// A turn that died part-way through still spent what it spent. Since a
/// provider failure became a terminal `Failed` event, a finish can carry
/// both an error and the usage reported before the stream broke, and the
/// bill has to survive the failure rather than being written off with it.
#[tokio::test]
async fn a_failed_turn_still_bills_for_what_it_spent() {
    const MODEL: &str = "claude-sonnet-5";

    let mut app = App::new(engine_asking(MODEL), None, Themes::builtin());

    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: Message::assistant(MODEL).id,
        reason: FinishReason::Failed,
        usage: Some(Usage {
            input_tokens: 2_000,
            output_tokens: 150,
            ..Usage::default()
        }),
        error: Some("the provider answered 500: overloaded".to_owned()),
        completed: 0,
    }))
    .await
    .expect("a failed turn is handled");

    assert_eq!(app.totals.input_tokens, 2_000);
    assert_eq!(app.totals.output_tokens, 150);

    let mut terminal = terminal(120, 12);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);

    assert!(screen.contains("failed"), "got:\n{screen}");
    assert!(screen.contains("2.0k in"), "got:\n{screen}");
    // 2,000 in at $2 plus 150 out at $10, per million tokens.
    assert!(screen.contains("$0.0055"), "got:\n{screen}");
    assert!(
        screen.contains("overloaded"),
        "the reason must survive beside the bill:\n{screen}"
    );
}

/// A turn that ends without usage — a cancel, or a provider that reports
/// none — leaves the totals alone rather than resetting them.
#[tokio::test]
async fn a_turn_without_usage_does_not_disturb_the_totals() {
    const MODEL: &str = "claude-sonnet-5";

    let mut app = App::new(engine_asking(MODEL), None, Themes::builtin());
    app.handle(finished(
        MODEL,
        Usage {
            input_tokens: 1_000,
            output_tokens: 100,
            ..Usage::default()
        },
    ))
    .await
    .expect("a turn end is handled");

    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: Message::assistant(MODEL).id,
        reason: FinishReason::Cancelled,
        usage: None,
        error: None,
        completed: 0,
    }))
    .await
    .expect("a cancel is handled");

    assert_eq!(app.totals.input_tokens, 1_000);
    assert_eq!(app.totals.output_tokens, 100);
}

/// The wire's reasoning and cache splits survive into the Ctrl+T
/// inspector's own per-turn row (**F2**), where the running totals above
/// collapse them into two numbers.
#[tokio::test]
async fn message_finished_retains_a_turn_usage_row_with_its_full_split() {
    const MODEL: &str = "claude-sonnet-5";

    let mut app = App::new(engine_asking(MODEL), None, Themes::builtin());
    let reply_id = Message::assistant(MODEL).id;
    let usage = Usage {
        input_tokens: 3,
        output_tokens: 4,
        reasoning_tokens: 5,
        cache_read_tokens: 6,
        cache_write_tokens: 7,
    };

    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: reply_id.clone(),
        reason: FinishReason::Completed,
        usage: Some(usage),
        error: None,
        completed: 0,
    }))
    .await
    .expect("a turn end is handled");

    let row = app.turn_usages.back().expect("a row should have been kept");
    assert_eq!(row.message_id, reply_id);
    assert_eq!(row.model, MODEL);
    assert_eq!(row.usage, usage);
}

/// The complement of [`a_turn_without_usage_does_not_disturb_the_totals`]:
/// a turn that reported nothing has nothing to add a row about either.
#[tokio::test]
async fn a_turn_without_usage_adds_no_turn_usage_row() {
    let mut app = app();

    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: Message::assistant(fake::MODEL).id,
        reason: FinishReason::Cancelled,
        usage: None,
        error: None,
        completed: 0,
    }))
    .await
    .expect("a cancel is handled");

    assert!(
        app.turn_usages.is_empty(),
        "a turn that reported no usage should add no row"
    );
}

/// The raw-log ring buffer is capped rather than growing without bound
/// (**F2**) — a synchronous unit test on `App::tee_event` directly,
/// rather than routing thousands of events through `handle`, which would
/// pay for a `replay_queued` call this behavior has nothing to do with.
#[test]
fn the_raw_log_caps_rather_than_growing_without_bound() {
    let mut app = app();
    let event = CoreEvent::AgentChanged {
        session_id: session(),
        agent: "build".to_owned(),
        model: fake::MODEL.to_owned(),
    };

    for _ in 0..(MAX_EVENT_LOG + 10) {
        app.tee_event(&event);
    }

    assert_eq!(app.event_log.len(), MAX_EVENT_LOG);
}

#[tokio::test]
async fn the_wheel_and_the_page_keys_move_the_viewport() {
    let mut app = app();
    for index in 0..60 {
        app.chat
            .start_message(Message::user(format!("entry {index}")));
    }
    let mut terminal = terminal(40, 12);
    app.draw(&mut terminal).expect("a frame draws");

    app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })))
    .await
    .expect("a wheel event is handled");
    assert!(!app.chat.is_following_tail());

    app.handle(key(KeyCode::End, KeyModifiers::NONE))
        .await
        .expect("End is handled");
    assert!(app.chat.is_following_tail());

    app.handle(key(KeyCode::PageUp, KeyModifiers::NONE))
        .await
        .expect("PageUp is handled");
    assert!(!app.chat.is_following_tail());

    app.handle(key(KeyCode::PageDown, KeyModifiers::NONE))
        .await
        .expect("PageDown is handled");
    assert!(app.chat.is_following_tail());
}

#[tokio::test]
async fn unrelated_events_do_not_disturb_the_editor() {
    let mut app = app();
    for event in typing("draft") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })))
    .await
    .expect("a click is handled");
    app.handle(AppEvent::Term(TermEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ))))
    .await
    .expect("a key release is handled");
    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert_eq!(app.editor.prompt().as_deref(), Some("draft"));
}

#[tokio::test]
async fn engine_bursts_are_coalesced_but_keystrokes_are_not() {
    let mut app = app();
    let reply = Message::assistant("canned");
    let part = Part::text("");
    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");
    app.handle(AppEvent::core(CoreEvent::PartStarted {
        session_id: session(),
        message_id: reply.id.clone(),
        part: part.clone(),
    }))
    .await
    .expect("a part start is handled");

    let mut terminal = terminal(40, 12);
    app.draw(&mut terminal).expect("a frame draws");

    app.handle(AppEvent::core(CoreEvent::PartDelta {
        session_id: session(),
        message_id: reply.id,
        part_id: part.id,
        delta: "burst".to_owned(),
    }))
    .await
    .expect("a fragment is handled");
    assert!(
        !app.needs_draw(),
        "a fragment arriving inside the frame budget should wait"
    );
    assert!(app.wants_wakeup(), "the pending frame must be woken up for");
    assert!(app.until_next_frame() <= FRAME);

    app.handle(key(KeyCode::Char('a'), KeyModifiers::NONE))
        .await
        .expect("typing is handled");
    assert!(app.needs_draw(), "a keystroke should redraw immediately");
}

#[test]
fn a_resize_storm_rewraps_without_panicking() {
    let mut app = app();
    for index in 0..40 {
        app.chat.start_message(Message::user(format!(
            "entry {index} carries enough words to wrap at every width tried here"
        )));
    }

    let mut terminal = terminal(80, 24);
    for width in [80_u16, 12, 200, 1, 61, 3, 120, 40] {
        terminal.backend_mut().resize(width, 24);
        app.draw(&mut terminal)
            .expect("a frame draws after a resize");

        assert!(
            app.chat
                .cached_widths()
                .iter()
                .all(|cached| *cached == Some(width)),
            "the wrap cache should be invalidated by a resize to {width}"
        );
    }

    terminal.backend_mut().resize(1, 1);
    app.draw(&mut terminal)
        .expect("a frame draws into a one-cell terminal");
}

#[test]
#[ignore = "timing-sensitive: run at phase gates with `cargo test -- --ignored`"]
fn a_five_thousand_line_transcript_draws_inside_the_frame_budget() {
    const ENTRIES: usize = 250;
    const LINES_PER_ENTRY: usize = 20;
    const FRAMES: usize = 200;

    let mut app = app();
    for entry in 0..ENTRIES {
        let body = (0..LINES_PER_ENTRY)
            .map(|line| format!("entry {entry} line {line} of plain streamed transcript text"))
            .collect::<Vec<_>>()
            .join("\n");
        app.chat.start_message(Message::user(body));
    }

    let mut terminal = terminal(120, 40);
    // The first frame wraps everything; the budget is about the rest.
    app.draw(&mut terminal).expect("the warm-up frame draws");
    assert!(
        app.chat.line_count() >= 5_000,
        "the transcript should be at least 5,000 lines, got {}",
        app.chat.line_count()
    );

    let mut samples = Vec::with_capacity(FRAMES);
    for frame in 0..FRAMES {
        app.chat
            .scroll_lines(if frame.is_multiple_of(2) { -7 } else { 5 });

        let started = Instant::now();
        app.draw(&mut terminal).expect("a frame draws");
        samples.push(started.elapsed());
    }

    samples.sort_unstable();
    let p50 = samples[FRAMES / 2];
    let p95 = samples[FRAMES * 95 / 100];
    let worst = samples.last().copied().unwrap_or_default();

    // The gate runner reads these numbers off the test output.
    println!(
        "{} lines, {FRAMES} frames: p50 {p50:?}, p95 {p95:?}, worst {worst:?}",
        app.chat.line_count()
    );

    assert!(
        p95 < FRAME,
        "p95 frame time was {p95:?} (worst {worst:?}), budget is {FRAME:?}"
    );
}

/// A stored conversation of `count` messages with the shape a real one
/// has: prompts of a few lines, replies of a paragraph or two, and every
/// fifth reply carrying a finished tool call between its text parts.
///
/// Deliberately not made of one-word messages. The cost the frontend pays
/// on a resume is wrapping every line of every entry, so a transcript of
/// stubs would measure nothing.
fn stored_transcript(count: usize) -> Vec<Message> {
    (0..count)
            .map(|index| {
                if index.is_multiple_of(2) {
                    return Message::user(format!(
                        "turn {index}: each part of a message is written to its own file, so a\n\
                         streaming text part can be rewritten as it grows without rewriting the\n\
                         whole envelope. Walk me through what a resume has to rebuild from that."
                    ));
                }

                let mut reply = Message::assistant(fake::MODEL);
                reply.parts.push(Part::text(format!(
                    "turn {index}: a resume reads the info file first, because it names the\n\
                     summary the live window starts from. Everything from that message onward\n\
                     is read back envelope by envelope, and each envelope's parts are read from\n\
                     the part directory keyed by the message id.\n\
                     \n\
                     Assistant messages that carry no content stay in the transcript but are\n\
                     left out of the request window, since some providers reject an empty\n\
                     message; they render as interrupted rather than as complete."
                )));

                if index.is_multiple_of(5) {
                    reply.parts.push(Part {
                        id: PartId::from(format!("prt_{index}")),
                        body: PartBody::Tool {
                            call_id: format!("call_{index}"),
                            tool: "read".to_owned(),
                            state: ToolState::Completed {
                                input: serde_json::json!({"filePath": "crates/ganja-core/src/storage.rs"}),
                                output: "read 412 lines".to_owned(),
                                title: "storage.rs".to_owned(),
                                metadata: serde_json::json!({}),
                                started: 0,
                                completed: 1,
                            },
                        },
                    });
                    reply.parts.push(Part::text(
                        "The tool call above is closed as an error on resume when the previous\n\
                         process died before it finished, because the next request has to answer\n\
                         every call the model opened.",
                    ));
                }

                reply.complete();
                reply
            })
            .collect()
}

/// P4 acceptance: a 200-message session reaches its first frame inside
/// 150ms.
///
/// The measured window is [`App::seed`] plus the first [`App::draw`], over
/// messages already in memory. Reading them off the disk is the engine's
/// cost and is timed where it happens; what the frontend owes is turning a
/// resumed transcript into a screen. The first frame is the expensive one
/// because it wraps every entry — later frames reuse the cache.
#[test]
fn a_two_hundred_message_session_reaches_its_first_frame_inside_the_budget() {
    const BUDGET: Duration = Duration::from_millis(150);
    const MESSAGES: usize = 200;

    // Built outside the window: composing fixtures is not what a resume
    // pays for.
    let transcript = stored_transcript(MESSAGES);
    let mut app = app();
    let mut terminal = terminal(100, 30);

    let started = Instant::now();
    app.seed(transcript);
    app.draw(&mut terminal).expect("the first frame draws");
    let elapsed = started.elapsed();

    // The gate runner reads this number off the test log rather than
    // trusting a green assertion to mean it was measured.
    eprintln!(
        "{MESSAGES} messages, {} wrapped lines: first frame in {elapsed:?} (budget {BUDGET:?})",
        app.chat.line_count()
    );

    assert!(
        app.chat.line_count() >= 1_000,
        "a transcript this budget is worth measuring should be at least 1,000 lines, got {}",
        app.chat.line_count()
    );
    assert!(
        elapsed < BUDGET,
        "the first frame took {elapsed:?}, budget is {BUDGET:?}"
    );
}

/// One reply of the frame-time fixture, **markdown-heavy by construction**.
///
/// Every construct the renderer has a code path for is in here on purpose:
/// a heading, a paragraph long enough to wrap at 120 columns, a fenced
/// **Rust** block (so syntect actually parses and the nine `syntax*` slots
/// actually resolve), a grid table, a nested list carrying inline emphasis
/// and code, and a blockquote. A plain-text transcript would measure the
/// wrap and call it the renderer (R16(4)).
fn markdown_reply(index: usize) -> String {
    format!(
        "## Section {index}: what the step decided\n\
             \n\
             The engine hands the frontend an ordered event stream and nothing else, so a \
             transcript that applied every event holds exactly what the next request will \
             carry — which is the property a remote client will lean on, and the reason \
             every message type is serde-derived from the first day rather than the day \
             a socket appears.\n\
             \n\
             ```rust\n\
             /// Counts the bytes that matter in `input`.\n\
             fn weigh_{index}(input: &str) -> Result<usize, Error> {{\n\
             \x20   let total = input.chars().filter(|c| !c.is_whitespace()).count();\n\
             \x20   if total == 0 {{\n\
             \x20       return Err(Error::Empty);\n\
             \x20   }}\n\
             \x20   Ok(total * {index})\n\
             }}\n\
             ```\n\
             \n\
             | field | kind | what it carries |\n\
             | --- | --- | --- |\n\
             | id | PartId | the part this body was written under |\n\
             | body | PartBody | text, a tool call, a file, or a step marker |\n\
             \n\
             - the outer claim for step {index}\n\
             \x20 - a nested one with **emphasis** and `inline_code()`\n\
             \x20 - and another, so the marker column is exercised twice\n\
             - a second outer claim, long enough that it has to wrap at a hundred and \
             twenty columns and hang under its own marker\n\
             \n\
             > What the step above settled, said once more so the quote path runs.\n"
    )
}

/// The transcript the frame-time accept measures.
fn markdown_transcript(replies: usize) -> Vec<Message> {
    (0..replies)
        .flat_map(|index| {
            let mut reply = Message::assistant(fake::MODEL);
            reply.parts.push(Part::text(markdown_reply(index)));
            reply.complete();

            [
                Message::user(format!("walk me through step {index}")),
                reply,
            ]
        })
        .collect()
}

/// P6 acceptance (R16(4)): a 10,000-line markdown transcript scrolls at
/// 30 FPS or better on a 120×40 screen.
///
/// `#[ignore]`d because it is a stopwatch, not a claim about behavior, and
/// a loaded CI box would make it flap. Run it with
/// `cargo nextest run -p ganja-tui --run-ignored all -E 'test(scrolls_a_ten_thousand_line)'`
/// and read the numbers off the log rather than off the green tick.
///
/// What is measured is a frame: the transcript's cached-wrap walk, the
/// viewport blit and the backend's diff, over a transcript whose assistant
/// text is markdown (see [`markdown_reply`]). The first frame is called out
/// separately because it is the one that parses every block and runs
/// syntect over every fence; the ones after it are what scrolling costs.
#[test]
#[ignore = "a timing measurement, not a behavior"]
fn scrolls_a_ten_thousand_line_markdown_transcript_at_thirty_frames_a_second() {
    const BUDGET: Duration = Duration::from_millis(33);
    const LINES: usize = 10_000;
    const REPLIES: usize = 320;
    const STEPS: usize = 200;

    let transcript = markdown_transcript(REPLIES);
    let mut app = app();
    let mut terminal = terminal(120, 40);
    let mut frames = Vec::with_capacity(STEPS + 1);

    app.seed(transcript);
    let started = Instant::now();
    app.draw(&mut terminal).expect("the first frame draws");
    let first = started.elapsed();
    frames.push(first);

    let lines = app.chat.line_count();
    assert!(
        lines >= LINES,
        "the accept is over {LINES} lines, and this fixture wrapped to {lines}"
    );

    // Top to bottom in even steps, so the walk crosses every entry rather
    // than re-drawing one screenful that is already warm.
    app.chat.scroll_to_top();
    let step = isize::try_from(lines / STEPS).unwrap_or(1).max(1);
    for _ in 0..STEPS {
        app.chat.scroll_lines(step);
        let started = Instant::now();
        app.draw(&mut terminal).expect("a frame draws");
        frames.push(started.elapsed());
    }

    frames.sort_unstable();
    let at = |percent: usize| frames[(frames.len() - 1) * percent / 100];
    let (p50, p95, worst) = (at(50), at(95), at(100));

    eprintln!(
        "{lines} markdown lines over {REPLIES} replies at 120x40, \
             {frames} frames: first {first:?}, p50 {p50:?}, p95 {p95:?}, max {worst:?} \
             (budget p95 {BUDGET:?})",
        frames = frames.len()
    );

    assert!(
        p95 <= BUDGET,
        "p95 frame time was {p95:?}, budget is {BUDGET:?}"
    );
}

#[test]
fn a_fresh_app_wants_its_first_frame() {
    let app = app();

    assert!(app.needs_draw());
    assert!(app.until_next_frame() <= FRAME);
}

fn permission_event(id: &str) -> CoreEvent {
    CoreEvent::PermissionRequested {
        session_id: session(),
        id: PermissionId::from(id.to_owned()),
        call_id: "call_1".to_owned(),
        tool: "shell".to_owned(),
        title: "cargo test".to_owned(),
        args: serde_json::json!({"command": "cargo test"}),
        directories: Vec::new(),
    }
}

/// The wiring's own mapping (**D469**): only `completed` counts as done,
/// and the in-progress entry's title is what the element shows working.
#[test]
fn the_bar_counts_only_completed_todos_as_done() {
    let progress = super::todo_progress(&serde_json::json!({"todos": [
        {"content": "landed", "status": "completed", "priority": "high"},
        {"content": "underway", "status": "in_progress", "priority": "medium"},
        {"content": "waiting", "status": "pending", "priority": "low"},
        {"content": "dropped", "status": "cancelled", "priority": "low"},
    ]}))
    .expect("a well-formed list maps");
    assert_eq!(progress.done, 1);
    assert_eq!(progress.total, 4);
    assert_eq!(progress.current.as_deref(), Some("underway"));

    assert!(
        super::todo_progress(&serde_json::json!({})).is_none(),
        "metadata without a list clears the element rather than inventing one"
    );
}

/// The lead wiring for the roster's `todos` element (**D469**): a
/// finished `todowrite` fills it from the metadata the tool published.
#[tokio::test]
async fn a_finished_todowrite_fills_the_bars_todo_element() {
    let statusline: ganja_core::config::StatuslineConfig =
        serde_json::from_value(serde_json::json!({"elements": ["todos"]}))
            .expect("the fixture is a statusline table");
    let mut app = app().with_statusline(Some(&statusline));

    app.handle(AppEvent::core(CoreEvent::PartUpdated {
        session_id: session(),
        message_id: MessageId::from("msg_1".to_owned()),
        part: Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "todowrite".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({}),
                    output: "[]".to_owned(),
                    title: "1 todos".to_owned(),
                    metadata: serde_json::json!({"todos": [
                        {"content": "landed", "status": "completed", "priority": "high"},
                        {"content": "underway", "status": "in_progress", "priority": "medium"},
                    ]}),
                    started: 0,
                    completed: 1,
                },
            },
        },
    }))
    .await
    .expect("the event applies");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("todos:1/2"),
        "got:\n{}",
        screen(&terminal)
    );
}

#[tokio::test]
async fn a_tool_call_moves_through_its_lifecycle_on_screen() {
    let mut app = app();
    let reply = Message::assistant("canned");
    let part = Part::tool("call_1", "shell");

    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");
    app.handle(AppEvent::core(CoreEvent::PartStarted {
        session_id: session(),
        message_id: reply.id.clone(),
        part: part.clone(),
    }))
    .await
    .expect("a part start is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("\u{25cf} Shell"),
        "got:\n{}",
        screen(&terminal)
    );

    app.handle(AppEvent::core(CoreEvent::PartUpdated {
        session_id: session(),
        message_id: reply.id.clone(),
        part: Part {
            id: part.id.clone(),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Running {
                    input: serde_json::json!({"command": "cargo test"}),
                    metadata: serde_json::Value::Null,
                    started: 0,
                },
            },
        },
    }))
    .await
    .expect("a running update is handled");
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("cargo test"),
        "got:\n{}",
        screen(&terminal)
    );

    app.handle(AppEvent::core(CoreEvent::PartUpdated {
        session_id: session(),
        message_id: reply.id.clone(),
        part: Part {
            id: part.id.clone(),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"command": "cargo test"}),
                    output: "ok".to_owned(),
                    title: "cargo test".to_owned(),
                    metadata: serde_json::json!({}),
                    started: 0,
                    completed: 1,
                },
            },
        },
    }))
    .await
    .expect("a completed update is handled");
    app.draw(&mut terminal).expect("a frame draws");
    let screen_text = screen(&terminal);
    assert!(
        screen_text.contains("\u{25cf} Shell(command: \"cargo test\")"),
        "got:\n{screen_text}"
    );
    assert!(
        screen_text.contains("\u{23bf} cargo test"),
        "got:\n{screen_text}"
    );
    assert!(screen_text.contains("ok"), "got:\n{screen_text}");
}

/// **AC4.** The tail says a turn is under way for exactly as long as the
/// engine holds one, and the loop keeps waking itself while it does — the
/// elapsed figure is a clock, and a clock nobody redraws is a wrong one.
#[tokio::test]
async fn the_working_line_lives_exactly_as_long_as_the_turn_does() {
    let mut app = app();
    let reply = Message::assistant("canned");

    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("\u{2026} (0s"),
        "got:\n{}",
        screen(&terminal)
    );
    assert!(app.animating(), "the loop has a reason to wake itself");

    app.handle(AppEvent::core(CoreEvent::MessageFinished {
        session_id: session(),
        message_id: reply.id.clone(),
        reason: FinishReason::Completed,
        usage: None,
        error: None,
        completed: 0,
    }))
    .await
    .expect("a finish is handled");
    app.draw(&mut terminal).expect("a frame draws");

    assert!(
        !screen(&terminal).contains("\u{2026} (0s"),
        "a settled turn leaves no working line:\n{}",
        screen(&terminal)
    );
    assert!(
        !app.animating(),
        "and nothing left on screen moves on its own"
    );
}

#[tokio::test]
async fn a_part_updated_for_an_unseen_id_is_appended_not_dropped() {
    let mut app = app();
    let reply = Message::assistant("canned");
    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");

    // No PartStarted for this id: a frontend that joined mid-stream still
    // has to converge on the same transcript.
    app.handle(AppEvent::core(CoreEvent::PartUpdated {
        session_id: session(),
        message_id: reply.id.clone(),
        part: Part {
            id: PartId::from("prt_orphan".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Running {
                    input: serde_json::json!({"filePath": "a.rs"}),
                    metadata: serde_json::Value::Null,
                    started: 0,
                },
            },
        },
    }))
    .await
    .expect("an update for an unseen id is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("a.rs"),
        "an update for an id the transcript never saw start should still append, got:\n{}",
        screen(&terminal)
    );
}

#[test]
fn permission_keys_map_to_the_right_reply() {
    assert_eq!(
        permission_reply(KeyCode::Char('y')),
        Some(PermissionReply::Once)
    );
    assert_eq!(
        permission_reply(KeyCode::Char('a')),
        Some(PermissionReply::Always)
    );
    assert_eq!(
        permission_reply(KeyCode::Char('n')),
        Some(PermissionReply::Reject)
    );
    assert_eq!(
        permission_reply(KeyCode::Esc),
        Some(PermissionReply::Reject)
    );
    assert_eq!(permission_reply(KeyCode::Char('x')), None);
}

#[tokio::test]
async fn keys_while_the_dialog_is_open_are_sent_as_replies_not_typed() {
    let mut app = app();
    app.handle(AppEvent::core(permission_event("perm_1")))
        .await
        .expect("a permission request is handled");

    app.handle(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .expect("y is handled");

    assert!(
        app.permission.is_some(),
        "the dialog waits for PermissionReplied before closing"
    );
    assert!(
        app.editor.prompt().is_none(),
        "the keystroke must not reach the editor"
    );
}

#[tokio::test]
async fn a_permission_request_opens_the_dialog() {
    let mut app = app();
    app.handle(AppEvent::core(permission_event("perm_1")))
        .await
        .expect("a permission request is handled");

    assert!(app.permission.is_some());

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("shell"), "got:\n{screen}");
    assert!(screen.contains("cargo test"), "got:\n{screen}");
}

#[tokio::test]
async fn a_matching_reply_closes_the_dialog_but_a_stray_one_does_not() {
    let mut app = app();
    app.handle(AppEvent::core(permission_event("perm_1")))
        .await
        .expect("a permission request is handled");
    assert!(app.permission.is_some());

    app.handle(AppEvent::core(CoreEvent::PermissionReplied {
        session_id: session(),
        id: PermissionId::from("perm_other".to_owned()),
        reply: PermissionReply::Reject,
    }))
    .await
    .expect("a stray reply is handled");
    assert!(
        app.permission.is_some(),
        "a reply naming a different request must not close this dialog"
    );

    app.handle(AppEvent::core(CoreEvent::PermissionReplied {
        session_id: session(),
        id: PermissionId::from("perm_1".to_owned()),
        reply: PermissionReply::Once,
    }))
    .await
    .expect("the matching reply is handled");
    assert!(app.permission.is_none());
}

/// Two children asking at once are two dialogs, shown one at a time
/// (**D462**).
///
/// The engine holds both open and routes each reply by id, so the frontend
/// may not drop the first when the second arrives — nor stack them, which
/// is not a design. It queues, says how many are behind, and asks the next
/// one the moment this one is answered.
#[tokio::test]
async fn a_second_request_queues_behind_the_open_dialog_and_is_asked_next() {
    let mut app = app();
    app.handle(AppEvent::core(permission_event("perm_1")))
        .await
        .expect("the first request is handled");
    app.handle(AppEvent::core(permission_event("perm_2")))
        .await
        .expect("the second request is handled");

    assert_eq!(
        app.permission
            .as_ref()
            .and_then(PendingDialog::permission_id)
            .map(|id| id.as_str()),
        Some("perm_1"),
        "the dialog on screen is still the one that was asked first"
    );
    assert_eq!(app.queued_permissions.len(), 1);

    let mut terminal = terminal(100, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(
        screen.contains("1 dialog queued"),
        "the bar says how many are behind it:\n{screen}"
    );

    app.handle(AppEvent::core(CoreEvent::PermissionReplied {
        session_id: session(),
        id: PermissionId::from("perm_1".to_owned()),
        reply: PermissionReply::Once,
    }))
    .await
    .expect("the first reply is handled");

    assert_eq!(
        app.permission
            .as_ref()
            .and_then(PendingDialog::permission_id)
            .map(|id| id.as_str()),
        Some("perm_2"),
        "answering one asks the next rather than leaving the queue stranded"
    );
    assert!(app.queued_permissions.is_empty());

    app.handle(AppEvent::core(CoreEvent::PermissionReplied {
        session_id: session(),
        id: PermissionId::from("perm_2".to_owned()),
        reply: PermissionReply::Once,
    }))
    .await
    .expect("the second reply is handled");
    assert!(app.permission.is_none(), "and then nobody is being asked");
}

/// A cancel answers every open request, including the ones nobody was
/// shown. Those retire from the queue rather than being put in front of
/// somebody after the turn they belonged to has ended.
#[tokio::test]
async fn a_reply_to_a_queued_request_retires_it_without_ever_showing_it() {
    let mut app = app();
    for id in ["perm_1", "perm_2"] {
        app.handle(AppEvent::core(permission_event(id)))
            .await
            .expect("a request is handled");
    }

    app.handle(AppEvent::core(CoreEvent::PermissionReplied {
        session_id: session(),
        id: PermissionId::from("perm_2".to_owned()),
        reply: PermissionReply::Reject,
    }))
    .await
    .expect("the queued request's own refusal is handled");

    assert!(
        app.queued_permissions.is_empty(),
        "the refused request left the queue"
    );
    assert_eq!(
        app.permission
            .as_ref()
            .and_then(PendingDialog::permission_id)
            .map(|id| id.as_str()),
        Some("perm_1"),
        "and the one on screen is untouched by it"
    );
}

// ---- D524: the admission gate's review surfaces ----

/// A `PeerHeld` as the engine's forwarder stamps one, with the parity
/// deadline where the cause carries a timer.
fn held_event(id: &str, cause: HoldCause) -> CoreEvent {
    let parity = matches!(cause, HoldCause::ModeMismatch | HoldCause::NoModeAsserted);
    CoreEvent::PeerHeld {
        session_id: session(),
        id: HeldId::from(id.to_owned()),
        from: "w1@ganja-team".to_owned(),
        cause,
        summary: Some(RedactedText::from("a finding worth a look".to_owned())),
        preview: RedactedText::from("the full body of the finding".to_owned()),
        expires_in_ms: parity.then_some(300_000),
    }
}

/// The parity causes put a person in front of the message now (AC-27):
/// the modal rides the same one-on-screen queue every dialog does, and
/// its own settlement — whoever settled it — is what closes it.
#[tokio::test]
async fn a_parity_hold_raises_the_approval_dialog_and_its_settlement_closes_it() {
    let mut app = app();
    app.handle(AppEvent::core(held_event(
        "held_1",
        HoldCause::NoModeAsserted,
    )))
    .await
    .expect("the hold is handled");

    let open = app.permission.as_ref().expect("the modal is on screen");
    assert_eq!(
        open.held_id().map(HeldId::as_str),
        Some("held_1"),
        "and it is the hold's own dialog, not a permission's"
    );
    assert!(
        open.permission_id().is_none(),
        "a hold has no permission id"
    );

    app.handle(AppEvent::core(CoreEvent::PeerHoldSettled {
        session_id: session(),
        id: HeldId::from("held_1".to_owned()),
        outcome: HeldOutcome::Expired,
    }))
    .await
    .expect("the settlement is handled");

    assert!(
        app.permission.is_none(),
        "the settlement retires the modal, whatever settled it"
    );
}

/// An explicit hold — and a mode-unknown one — raises **no** modal
/// (AC-27): no deadline races anybody, and their review surface is the
/// `/held` listing alone.
#[tokio::test]
async fn an_explicit_or_mode_unknown_hold_raises_no_dialog() {
    let mut app = app();
    for (id, cause) in [
        (
            "held_1",
            HoldCause::Explicit {
                source: ganja_protocol::PolicySource::Global,
            },
        ),
        ("held_2", HoldCause::ModeUnknown),
    ] {
        app.handle(AppEvent::core(held_event(id, cause)))
            .await
            .expect("the hold is handled");
    }

    assert!(
        app.permission.is_none(),
        "an explicit hold is /held's to review, not a modal's"
    );
    assert!(app.queued_permissions.is_empty());
}

/// A hold that settles while its modal is still queued behind another
/// dialog retires from the queue without ever being shown — the
/// permission queue's own rule, on the other variant.
#[tokio::test]
async fn a_hold_settled_while_queued_retires_without_being_shown() {
    let mut app = app();
    app.handle(AppEvent::core(permission_event("perm_1")))
        .await
        .expect("the permission is handled");
    app.handle(AppEvent::core(held_event(
        "held_1",
        HoldCause::ModeMismatch,
    )))
    .await
    .expect("the hold is handled");
    assert_eq!(app.queued_permissions.len(), 1, "the hold queued behind");

    app.handle(AppEvent::core(CoreEvent::PeerHoldSettled {
        session_id: session(),
        id: HeldId::from("held_1".to_owned()),
        outcome: HeldOutcome::Denied,
    }))
    .await
    .expect("the settlement is handled");

    assert!(app.queued_permissions.is_empty(), "the hold left the queue");
    assert_eq!(
        app.permission
            .as_ref()
            .and_then(PendingDialog::permission_id)
            .map(|id| id.as_str()),
        Some("perm_1"),
        "and the permission on screen is untouched by it"
    );
}

/// The modal's keys send the settle and nothing closes locally: the
/// engine's `PeerHoldSettled` is the one closer, so a keypress that
/// raced the deadline cannot double-decide (**D524**).
#[tokio::test]
async fn approval_keys_settle_by_event_never_by_keypress() {
    let mut app = app();
    app.handle(AppEvent::core(held_event(
        "held_1",
        HoldCause::NoModeAsserted,
    )))
    .await
    .expect("the hold is handled");

    app.handle(AppEvent::Term(TermEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    ))))
    .await
    .expect("the dismiss is handled");

    assert!(
        app.permission.is_some(),
        "Esc sent the deny; the settlement event is what closes"
    );

    app.handle(AppEvent::core(CoreEvent::PeerHoldSettled {
        session_id: session(),
        id: HeldId::from("held_1".to_owned()),
        outcome: HeldOutcome::Denied,
    }))
    .await
    .expect("the settlement is handled");
    assert!(app.permission.is_none());
}

/// **AC-32.** Under the D479 trio — the TUI's yolo drain active *and*
/// the engine seeded bypass — a parity hold still raises the dialog and
/// the drain does not answer it: the hold item survives the drain pass,
/// nothing sent `SettleHeld`, and the entry stays held in the engine.
/// The guarantee is B1's type shape, exercised here at runtime: the
/// drain answers `PermissionId`s, and [`PendingDialog::Held`] carries
/// none.
#[tokio::test]
async fn a_yolo_session_never_answers_a_hold_and_the_entry_stays_held() {
    use ganja_core::teammate::inbound::SocketAdmission;

    let mut app =
        App::new(engine().with_inbound_bypass(true), None, Themes::builtin()).with_yolo(true);

    // A real hold, through the engine's own socket door: a bypass-classed
    // receiver with no explicit policy holds every inbound
    // (`no_mode_asserted`), which is exactly the state AC-32 is about.
    let admission = app.engine.inbound().admit_socket(
        app.engine.receiver_class(),
        "w1@ganja-team",
        "the body of the message",
        None,
    );
    assert!(
        matches!(
            admission,
            SocketAdmission::Held {
                cause: HoldCause::NoModeAsserted,
                ..
            }
        ),
        "a bypassed receiver's unset policy holds: {admission:?}"
    );
    let held = app.engine.held_messages();
    assert_eq!(held.len(), 1, "the engine really holds the entry");
    let id = held[0].id.clone();

    // The event as the forwarder would stamp it, through the full
    // `App::handle` path — which runs the yolo drain
    // (`answer_for_the_absent`) right after `handle_core`.
    app.handle(AppEvent::core(CoreEvent::PeerHeld {
        session_id: session(),
        id: id.clone(),
        from: "w1@ganja-team".to_owned(),
        cause: HoldCause::NoModeAsserted,
        summary: None,
        preview: RedactedText::from("the body of the message".to_owned()),
        expires_in_ms: Some(300_000),
    }))
    .await
    .expect("the hold is handled");

    assert!(
        matches!(&app.permission, Some(PendingDialog::Held(dialog)) if *dialog.id() == id),
        "the dialog is up — yolo answers permissions, never holds"
    );
    assert!(
        app.auto_permissions.is_empty(),
        "the drain was handed nothing to answer: a hold carries no permission id"
    );
    assert_eq!(
        app.engine.held_messages().len(),
        1,
        "and the entry stays held — nothing sent SettleHeld"
    );
}

/// The `N held` segment counts what the engine holds, appearing only
/// while that is anything (AC-27) — polled on the tick, the D462 way.
#[tokio::test]
async fn the_bar_counts_held_messages_only_while_any_are_held() {
    let mut app = App::new(engine().with_inbound_bypass(true), None, Themes::builtin());
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    assert!(!status_line(&mut app).contains("held"));

    let admission = app.engine.inbound().admit_socket(
        app.engine.receiver_class(),
        "w1@ganja-team",
        "the body",
        None,
    );
    assert!(
        matches!(
            admission,
            ganja_core::teammate::inbound::SocketAdmission::Held { .. }
        ),
        "the seed held: {admission:?}"
    );
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    assert!(
        status_line(&mut app).contains("1 held"),
        "got: {}",
        status_line(&mut app)
    );

    let id = app.engine.held_messages()[0].id.clone();
    app.engine
        .send(ganja_protocol::Command::SettleHeld {
            id,
            decision: super::HeldDecision::Deny,
        })
        .await
        .expect("the settle is accepted");
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    assert!(!status_line(&mut app).contains("held"));
}

/// The `/held` listing over the engine's own buffer: rows appear with
/// sender, cause and preview, Enter opens Release/Deny, and a deny
/// retires the row through the engine (AC-27's working Release/Deny).
#[tokio::test]
async fn the_held_listing_lists_and_its_deny_retires_the_row() {
    let mut app = App::new(engine().with_inbound_bypass(true), None, Themes::builtin());
    let admission = app.engine.inbound().admit_socket(
        app.engine.receiver_class(),
        "w1@ganja-team",
        "the body of the finding",
        Some("a finding worth a look"),
    );
    assert!(
        matches!(
            admission,
            ganja_core::teammate::inbound::SocketAdmission::Held { .. }
        ),
        "the seed held: {admission:?}"
    );

    app.run_command(command::Action::Held).await;
    let dialog = app.held_dialog.as_ref().expect("/held opens the dialog");
    assert_eq!(
        dialog.selected().map(|row| row.from.as_str()),
        Some("w1@ganja-team")
    );

    // Enter opens the actions; Down moves to Deny; Enter settles it.
    app.handle_held_key(KeyCode::Enter).await;
    app.handle_held_key(KeyCode::Down).await;
    app.handle_held_key(KeyCode::Enter).await;

    assert!(
        app.engine.held_messages().is_empty(),
        "the deny settled the entry in the engine"
    );
    let dialog = app.held_dialog.as_ref().expect("the dialog stays open");
    assert!(
        dialog.selected().is_none(),
        "and the row is gone from the listing"
    );

    app.handle_held_key(KeyCode::Esc).await;
    assert!(app.held_dialog.is_none(), "Esc closes the listing");
}

#[test]
fn snapshot_held_approval_dialog_open() {
    let mut app = app();
    app.permission = Some(PendingDialog::Held(super::held::HeldApproval::new(
        HeldId::from("held_1".to_owned()),
        "w1@ganja-team".to_owned(),
        HoldCause::NoModeAsserted,
        Some("a finding worth a look".to_owned()),
        "the full body of the finding\nwith a second line for the preview".to_owned(),
        Some(300_000),
    )));

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[test]
fn snapshot_held_listing_open() {
    let mut app = app();
    app.held_dialog = Some(super::held::HeldList::new(vec![
        super::held::Row::new(
            HeldId::from("held_1".to_owned()),
            "w1@ganja-team".to_owned(),
            HoldCause::NoModeAsserted,
            Duration::from_secs(65),
            Some("a finding worth a look"),
            "the full body",
        ),
        super::held::Row::new(
            HeldId::from("held_2".to_owned()),
            "scribbler@nowhere".to_owned(),
            HoldCause::Explicit {
                source: ganja_protocol::PolicySource::Global,
            },
            Duration::from_secs(12),
            None,
            "an unsummarized body",
        ),
    ]));

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// What the scripted shell call echoes, so "the tool ran" is a question
/// about the tool's own output rather than about the screen.
const ECHOED: &str = "yolo-ran-zarquon";

/// The scripted turn after the tool call. Its arrival is what says every
/// call before it was resolved without anything still waiting on a person.
const CLOSING: &str = "script-finished-zarquon";

/// An app whose scripted turn makes one `bash` call — a tool the builtin
/// defaults ask about (`ganja_permission::permission::ASK_BY_DEFAULT`) —
/// and then says [`CLOSING`].
///
/// A real engine and a real tool rather than hand-built events, because
/// what **D479** is about is a turn that runs to its end with nobody
/// answering anything: a fixture that only replays a `PermissionRequested`
/// could not tell an answered request from a wedged one.
async fn shelling(
    yolo: bool,
    permissions: ganja_permission::Permissions,
) -> (TempDir, App, BoxStream<'static, CoreEvent>) {
    let directory = temporary();
    let script = directory.path().join("shell.json");
    fs::write(
        &script,
        format!(
            r#"{{
                    "cadence_ms": 0,
                    "turns": [
                        {{"tool_calls": [{{
                            "name": "bash",
                            "args": {{"command": "echo {ECHOED}"}}
                        }}]}},
                        {{"text": "{CLOSING}"}}
                    ]
                }}"#
        ),
    )
    .expect("the fake-provider script writes");

    let engine = Engine::new(
        Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script)),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(vec![Arc::new(
            ganja_tool::shell::ShellTool::new(),
        )])),
        permissions,
    );
    let events = engine.subscribe().await.expect("the test subscribes first");
    let app = App::new(engine, None, Themes::builtin()).with_yolo(yolo);
    app.engine
        .send(ganja_protocol::Command::SendPrompt {
            text: "run it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts the prompt");

    (directory, app, events)
}

/// Feeds the app every event of a two-turn script and hands back what it
/// was fed, asserting on the way that no dialog was ever on screen.
///
/// The assertion is *inside* the loop on purpose: a dialog that opened and
/// closed again between two events would be invisible to a check made
/// after the turn, and a dialog nobody can see is exactly the failure a
/// bypass has to be checked for.
async fn pump_turn(app: &mut App, events: &mut BoxStream<'static, CoreEvent>) -> Vec<CoreEvent> {
    let mut seen = Vec::new();

    for _ in 0..128 {
        let event = next_event(events).await;
        let finished = matches!(event, CoreEvent::MessageFinished { .. });
        seen.push(event.clone());
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
        assert!(
            app.permission.is_none() && app.queued_permissions.is_empty(),
            "no dialog may be raised in a bypassed session"
        );
        // One assistant message carries the whole turn — the step that
        // made the call and the step that said the closing word — so its
        // close is the end of everything this script had to play.
        if finished {
            return seen;
        }
    }

    panic!("the scripted turn never ran to its end: {seen:#?}");
}

/// What a completed `bash` part carries, or [`None`] while nothing has
/// completed one.
fn completed_shell(events: &[CoreEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        CoreEvent::PartUpdated { part, .. } => match &part.body {
            PartBody::Tool {
                tool,
                state: ToolState::Completed { output, .. },
                ..
            } if tool == "bash" => Some(output.clone()),
            _ => None,
        },
        _ => None,
    })
}

/// The lead behavior of **D479**: the dialog the rules raised is answered
/// for the person who asked not to be asked, and the call runs.
#[tokio::test]
async fn a_yolo_session_answers_a_raised_dialog_once_and_never_draws_it() {
    let (_directory, mut app, mut events) =
        shelling(true, ganja_permission::Permissions::default()).await;

    let seen = pump_turn(&mut app, &mut events).await;

    let asked = seen
        .iter()
        .filter(|event| matches!(event, CoreEvent::PermissionRequested { .. }))
        .count();
    assert_eq!(asked, 1, "the rules still raised the request: {seen:#?}");

    let replies: Vec<_> = seen
        .iter()
        .filter_map(|event| match event {
            CoreEvent::PermissionReplied { reply, .. } => Some(*reply),
            _ => None,
        })
        .collect();
    assert_eq!(
        replies,
        vec![PermissionReply::Once],
        "answered once, and never `Always` — nothing may be written to \
             this project's rules on the strength of a flag"
    );

    assert!(
        completed_shell(&seen).is_some_and(|output| output.contains(ECHOED)),
        "the call the dialog was about actually ran: {seen:#?}"
    );
}

/// The pre-mortem's first scenario, pinned: a bypass answers the dialogs
/// the rules *raise*, and a denial raises none.
#[tokio::test]
async fn a_denied_call_stays_denied_in_a_yolo_session() {
    let mut permissions = ganja_permission::Permissions::default();
    permissions.set_baseline(vec![ganja_permission::permission::Rule {
        permission: "bash".to_owned(),
        pattern: "*".to_owned(),
        action: ganja_permission::Action::Deny,
    }]);
    let (_directory, mut app, mut events) = shelling(true, permissions).await;

    let seen = pump_turn(&mut app, &mut events).await;

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, CoreEvent::PermissionRequested { .. })),
        "a denial is decided inside the engine and asks nobody: {seen:#?}"
    );
    assert!(
        completed_shell(&seen).is_none(),
        "the denied command never ran: {seen:#?}"
    );
    // And the turn carried on regardless: a refusal is information the
    // model reads, never a turn this frontend ended.
    assert!(
        seen.iter().any(|event| matches!(
            event,
            CoreEvent::PartUpdated { part, .. }
                if matches!(&part.body, PartBody::Tool { state: ToolState::Error { .. }, .. })
        )),
        "the refusal reached the model as an error result: {seen:#?}"
    );
}

/// The other half of the bypass's boundary: what a *person* is asked is
/// not what a *rule* asks. `question` — and the two plan doors, which ride
/// the same seam (`ganja_tool::plan`, `ctx.ask`) — arrive as
/// [`CoreEvent::QuestionAsked`] and keep their dialog.
#[tokio::test]
async fn a_yolo_session_still_raises_its_questions() {
    let mut app = app().with_yolo(true);

    app.handle(AppEvent::core(CoreEvent::QuestionAsked {
        session_id: session(),
        id: QuestionId::from("qst_1".to_owned()),
        questions: vec![QuestionInfo {
            question: "Which database should the service use?".to_owned(),
            header: "Database".to_owned(),
            options: vec![QuestionOption {
                label: "Postgres".to_owned(),
                description: "Relational database".to_owned(),
            }],
            multiple: None,
            custom: None,
        }],
        source: None,
    }))
    .await
    .expect("the question is handled");

    assert!(
        app.question.is_some(),
        "a bypass covers permission, never conversation"
    );
}

#[tokio::test]
async fn a_question_dialog_offers_the_options_and_enter_replies_the_selected_label() {
    let (_directory, mut app, mut events) = questioning().await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    for expected in [
        "Database",
        "Which database should the service use?",
        "Postgres",
        "Relational database",
        "SQLite",
        "One local file",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
    }

    app.handle(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .expect("down is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert!(
        app.question.is_some(),
        "the dialog waits for QuestionReplied before closing"
    );

    let mut answered = None;
    for _ in 0..64 {
        let event = next_event(&mut events).await;
        if let CoreEvent::QuestionReplied { answers, .. } = &event {
            answered = Some(answers.clone());
        }
        let finished = matches!(event, CoreEvent::MessageFinished { .. });
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
        if finished {
            break;
        }
    }

    assert_eq!(answered, Some(vec![vec!["SQLite".to_owned()]]));
    assert!(app.question.is_none());
}

#[tokio::test]
async fn esc_rejects_the_question_and_the_turn_reads_it_as_dismissal() {
    let (_directory, mut app, mut events) = questioning().await;

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(
        app.question.is_some(),
        "the dialog waits for QuestionRejected before closing"
    );

    let mut rejected = false;
    let mut dismissed = false;
    for _ in 0..64 {
        let event = next_event(&mut events).await;
        match &event {
            CoreEvent::QuestionRejected { .. } => rejected = true,
            CoreEvent::PartUpdated { part, .. } => {
                if let PartBody::Tool { tool, state, .. } = &part.body
                    && tool == "question"
                    && matches!(state, ToolState::Error { error, .. } if error == ganja_tool::question::DISMISSED)
                {
                    dismissed = true;
                }
            }
            _ => {}
        }
        let finished = matches!(event, CoreEvent::MessageFinished { .. });
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
        if finished {
            break;
        }
    }

    assert!(rejected, "the engine should announce the dismissal");
    assert!(
        dismissed,
        "the question tool should read its dismissal text"
    );
    assert!(app.question.is_none());
}

#[tokio::test]
async fn control_c_still_quits_while_the_dialog_is_open() {
    let mut app = app();
    app.handle(AppEvent::core(permission_event("perm_1")))
        .await
        .expect("a permission request is handled");

    app.handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .expect("control-c is handled");

    assert!(app.quit);
}

/// Drives a real turn to a streaming state, opens the dialog by hand
/// (nothing in this build yet gates a real tool call on one), presses
/// Esc, and proves the turn was never cancelled: it runs to a natural
/// `Completed` finish rather than the `Cancelled` the sibling escape test
/// gets when Esc is allowed to reach `CancelTurn`.
#[tokio::test]
async fn escape_with_the_dialog_open_does_not_cancel_the_turn() {
    let engine = Engine::new(
        Arc::new(FakeProvider::new("one two three", Duration::from_millis(2))),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin());

    for event in typing("hello") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    pump(&mut app, &mut events, 4).await;
    assert!(app.status.is_streaming());

    app.handle(AppEvent::core(permission_event("perm_1")))
        .await
        .expect("a permission request is handled");
    assert!(app.permission.is_some());

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(
        app.permission.is_some(),
        "Esc should reject the dialog, not close it before PermissionReplied"
    );

    let mut finished = None;
    while finished.is_none() {
        let event = events.next().await.expect("the engine keeps reporting");
        if let CoreEvent::MessageFinished { reason, .. } = &event {
            finished = Some(*reason);
        }
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
    }

    assert_eq!(
        finished,
        Some(FinishReason::Completed),
        "Esc must not have cancelled the turn"
    );
    assert!(
        app.permission.is_some(),
        "the dialog should not self-close on an unrelated event"
    );
}

/// **AC3.** A reply that thought before it answered, as the pane draws it:
/// the thinking behind its own marker and dimmed into italics, the answer
/// behind the bullet every reply block has.
#[test]
fn snapshot_thinking() {
    let mut app = app();
    app.chat.start_message(Message::user("say hello"));
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::reasoning_text(
        "The user wants a greeting and nothing more, so a short one is \
             the whole of the job here.",
    ));
    reply.parts.push(Part::text("Hello, world!"));
    app.chat.start_message(reply);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// The read row the user pinned by screenshot: the path and the range it
/// asked for on the header, a count as the whole of the result, and none
/// of the envelope the tool writes for the model.
#[test]
fn snapshot_read_row() {
    let mut app = app();
    let mut message = Message::assistant("canned");
    message.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({
                    "filePath": "/repo/crates/ganja-tui/src/component/chat.rs",
                    "offset": 1158,
                    "limit": 60,
                }),
                output: "<path>/repo/crates/ganja-tui/src/component/chat.rs</path>\n\
                             <type>file</type>\n<content>\n1158: fn wrap() {}\n</content>"
                    .to_owned(),
                title: "crates/ganja-tui/src/component/chat.rs".to_owned(),
                metadata: serde_json::json!({
                    "display": {
                        "type": "file",
                        "path": "/repo/crates/ganja-tui/src/component/chat.rs",
                        "lineStart": 1158,
                        "lineEnd": 1217,
                        "totalLines": 2500,
                    },
                }),
                started: 0,
                completed: 1,
            },
        },
    });
    app.chat.start_message(message);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[test]
fn snapshot_tool_pending() {
    let mut app = app();
    let mut message = Message::assistant("canned");
    message.parts.push(Part::tool("call_1", "shell"));
    app.chat.start_message(message);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[test]
fn snapshot_tool_running() {
    let mut app = app();
    let mut message = Message::assistant("canned");
    message.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "shell".to_owned(),
            state: ToolState::Running {
                input: serde_json::json!({"command": "cargo test"}),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        },
    });
    app.chat.start_message(message);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[test]
fn snapshot_tool_completed_with_a_diff() {
    let mut app = app();
    let mut message = Message::assistant("canned");
    message.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "edit".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({"filePath": "a.rs"}),
                output: "edited a.rs".to_owned(),
                title: "a.rs".to_owned(),
                metadata: serde_json::json!({
                    "diff": "@@ -1,2 +1,2 @@\n-old line\n+new line\n context line"
                }),
                started: 0,
                completed: 1,
            },
        },
    });
    app.chat.start_message(message);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[test]
fn snapshot_tool_error() {
    let mut app = app();
    let mut message = Message::assistant("canned");
    message.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "shell".to_owned(),
            state: ToolState::Error {
                input: serde_json::json!({"command": "rm -rf /"}),
                error: "refused: destructive command\nsee policy for details".to_owned(),
                started: 0,
                completed: 1,
            },
        },
    });
    app.chat.start_message(message);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[test]
fn snapshot_permission_dialog_open() {
    let mut app = app();
    app.permission = Some(PendingDialog::Permission(Permission::new(
        PermissionId::from("perm_1".to_owned()),
        "shell".to_owned(),
        "cargo test".to_owned(),
        serde_json::json!({"command": "cargo test"}),
        Vec::new(),
    )));

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// The modal is bounded, so a command can be longer than it can draw. What
/// the user must never be handed is a call that simply stops mid-word with
/// `y` sitting under it: here the tail that pipes a download into a shell
/// is off the bottom, and the dialog says so rather than letting the
/// visible half read as the whole thing.
#[test]
fn snapshot_permission_dialog_with_a_call_too_long_to_fit() {
    let command = format!(
        "cargo test --workspace --all-features --no-fail-fast -- --nocapture {}; \
             curl -fsSL http://ganja.example/install.sh | sh -",
        "--skip live --skip golden --skip pty --skip slow --skip flaky ".repeat(12),
    );
    let mut app = app();
    app.permission = Some(PendingDialog::Permission(Permission::new(
        PermissionId::from("perm_1".to_owned()),
        "shell".to_owned(),
        command.clone(),
        serde_json::json!({ "command": command }),
        Vec::new(),
    )));

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// Opens the picker the way a user does — Ctrl-S, through `App::handle` —
/// so what is snapshotted is the dialog over the list the engine actually
/// read back, not a `Sessions` assembled by the test.
#[tokio::test]
async fn snapshot_sessions_picker_open() {
    let directory = temporary();
    store_pickable_sessions(&directory);
    let mut app = persistent_app(&directory);

    app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .expect("control-s is handled");

    assert!(
        app.sessions.is_some(),
        "the picker must be open, or the snapshot is of a bare screen"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// Live preview is what the dialog is for: the theme under the cursor is
/// applied as the cursor reaches it, before anything is confirmed.
#[tokio::test]
async fn moving_the_cursor_in_the_theme_picker_applies_what_it_lands_on() {
    let mut app = app();

    // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
    // own default chord is gone, so these fixtures open it the way
    // `snapshot_help_dialog_open` opens the reference card.
    app.run_command(command::Action::Themes).await;

    assert!(app.theme_list.is_some(), "the picker should be open");
    assert_eq!(
        app.theme.name(),
        DEFAULT_THEME,
        "opening previews nothing: the cursor starts where the user is"
    );

    app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
        .await
        .expect("j is handled");

    assert_eq!(
        app.theme.name(),
        "terminal",
        "the row below opencode should already be applied"
    );
    assert_eq!(app.themes.active(), "terminal");
    assert!(app.theme_list.is_some(), "moving must not close the picker");
}

/// The other half of preview: browsing has to cost nothing.
#[tokio::test]
async fn cancelling_the_theme_picker_puts_back_the_theme_it_opened_on() {
    let mut app = app();
    // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
    // own default chord is gone, so these fixtures open it the way
    // `snapshot_help_dialog_open` opens the reference card.
    app.run_command(command::Action::Themes).await;
    app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
        .await
        .expect("j is handled");
    assert_ne!(app.theme.name(), DEFAULT_THEME, "a preview must have run");

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    assert!(app.theme_list.is_none(), "escape closes the picker");
    assert_eq!(app.theme.name(), DEFAULT_THEME);
    assert_eq!(app.themes.active(), DEFAULT_THEME);
}

#[tokio::test]
async fn keeping_a_theme_closes_the_picker_and_leaves_it_applied() {
    let mut app = app();
    // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
    // own default chord is gone, so these fixtures open it the way
    // `snapshot_help_dialog_open` opens the reference card.
    app.run_command(command::Action::Themes).await;
    app.handle(key(KeyCode::Char('k'), KeyModifiers::NONE))
        .await
        .expect("k is handled");
    let previewed = app.theme.name().to_owned();

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.theme_list.is_none());
    assert_eq!(app.theme.name(), previewed);
}

/// The picker owns every key while it is open, exactly as the sessions one
/// does — otherwise `j` would be typed into the prompt behind it.
#[tokio::test]
async fn keys_while_the_theme_picker_is_open_do_not_reach_the_editor() {
    let mut app = app();
    // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
    // own default chord is gone, so these fixtures open it the way
    // `snapshot_help_dialog_open` opens the reference card.
    app.run_command(command::Action::Themes).await;

    for event in typing("jkx") {
        app.handle(event).await.expect("typing is handled");
    }

    assert!(app.editor.prompt().is_none());
    assert!(app.theme_list.is_some());
}

/// The wheel belongs to whatever modal is up, like it does for the other
/// two: scrolling the transcript out from under a dialog is never what the
/// notch meant.
#[tokio::test]
async fn the_wheel_does_not_reach_the_transcript_while_the_theme_picker_is_open() {
    let mut app = app();
    for index in 0..60 {
        app.chat
            .start_message(Message::user(format!("entry {index}")));
    }
    app.draw(&mut terminal(40, 12)).expect("a frame draws");

    // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
    // own default chord is gone, so these fixtures open it the way
    // `snapshot_help_dialog_open` opens the reference card.
    app.run_command(command::Action::Themes).await;
    app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })))
    .await
    .expect("a wheel event is handled");

    assert!(
        app.chat.is_following_tail(),
        "the wheel should be swallowed"
    );
}

/// An app whose prompt history already holds `entries`, submitted in the
/// order given — the last one named is the newest.
fn app_with_history(directory: &TempDir, entries: &[&str]) -> App {
    let mut history = history::History::load_from(directory.path().join("prompt-history.jsonl"));
    for entry in entries {
        history.append(history::PromptInfo::text(*entry));
    }

    app().with_history(history)
}

/// Ctrl+R opens the search over whatever the store holds.
#[tokio::test]
async fn control_r_opens_the_history_search() {
    let directory = temporary();
    let mut app = app_with_history(&directory, &["first prompt", "second prompt"]);

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");

    assert!(app.history_search.is_some(), "the search should be open");
}

/// Fuzzy narrowing survives the whole way from a keystroke down to what
/// is under the cursor.
#[tokio::test]
async fn typing_into_the_history_search_narrows_to_a_fuzzy_match() {
    let directory = temporary();
    let mut app = app_with_history(&directory, &["commit the fix", "git status"]);

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");
    for event in typing("ommi") {
        app.handle(event).await.expect("typing narrows the query");
    }

    assert_eq!(
        app.history_search
            .as_ref()
            .and_then(component::search::HistorySearch::selected)
            .map(|prompt| prompt.input.as_str()),
        Some("commit the fix")
    );
}

/// Enter fills the composer with the entry under the cursor and closes
/// the search — an Enter here is a fill, never a submit, so no engine
/// event should ever arrive.
#[tokio::test]
async fn enter_in_history_search_fills_the_composer_and_sends_nothing() {
    let (mut app, mut events) = wired().await;
    let directory = temporary();
    let mut history = history::History::load_from(directory.path().join("prompt-history.jsonl"));
    history.append(history::PromptInfo::text("what does this crate do"));
    app.history = history;

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.history_search.is_none(), "enter closes the search");
    assert_eq!(app.editor.text(), "what does this crate do");

    let arrived = tokio::time::timeout(Duration::from_millis(50), events.next()).await;
    assert!(
        arrived.is_err(),
        "no engine event should have fired — a fill is not a submit"
    );
}

/// Esc restores exactly the buffer the search opened over — text and
/// cursor both — even after the query narrowed the list and the
/// selection moved.
#[tokio::test]
async fn esc_restores_the_pre_search_buffer_byte_for_byte() {
    let directory = temporary();
    let mut app = app_with_history(&directory, &["remembered prompt"]);
    for event in typing("draft in progress") {
        app.handle(event).await.expect("typing is handled");
    }
    let text_before = app.editor.text();
    let cursor_before = app.editor.cursor();

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");
    for event in typing("remem") {
        app.handle(event).await.expect("typing narrows the query");
    }
    app.handle(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .expect("down is handled");
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    assert!(app.history_search.is_none(), "escape closes the search");
    assert_eq!(
        app.editor.text(),
        text_before,
        "the buffer is restored byte for byte"
    );
    assert_eq!(app.editor.cursor(), cursor_before, "and the cursor too");
}

/// The chord is data like the six existing ones: rebinding it opens the
/// search from its new key, and the old default stops working.
#[tokio::test]
async fn a_rebound_history_search_reaches_its_new_key_and_the_default_stops_working() {
    let directory = temporary();
    let mut history = history::History::load_from(directory.path().join("prompt-history.jsonl"));
    history.append(history::PromptInfo::text("remembered"));

    let configured: std::collections::BTreeMap<String, String> =
        [("history_search".to_owned(), "f7".to_owned())].into();
    let keys = crate::keybind::Keybinds::from_config(&configured).expect("a legible binding");
    let mut app = App::new(engine(), None, Themes::builtin())
        .with_history(history)
        .with_keybinds(keys);

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");
    assert!(
        app.history_search.is_none(),
        "the replaced default should be inert"
    );

    app.handle(key(KeyCode::F(7), KeyModifiers::NONE))
        .await
        .expect("f7 is handled");
    assert!(app.history_search.is_some(), "and f7 should open it");
}

/// The picker owns every key while it is open, exactly as the sessions
/// and theme pickers do — otherwise `j` would be typed into the query
/// and nowhere else, or worse, leak past it into the editor.
#[tokio::test]
async fn keys_while_the_history_search_is_open_do_not_reach_the_editor() {
    let directory = temporary();
    let mut app = app_with_history(&directory, &["remembered prompt"]);

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");
    for event in typing("jkx") {
        app.handle(event).await.expect("typing is handled");
    }

    assert!(app.editor.prompt().is_none());
    assert!(app.history_search.is_some());
}

/// The wheel belongs to the search modal too, like every other one.
#[tokio::test]
async fn the_wheel_does_not_reach_the_transcript_while_the_history_search_is_open() {
    let directory = temporary();
    let mut app = app_with_history(&directory, &["remembered prompt"]);
    for index in 0..60 {
        app.chat
            .start_message(Message::user(format!("entry {index}")));
    }
    app.draw(&mut terminal(40, 12)).expect("a frame draws");

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");
    app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })))
    .await
    .expect("a wheel event is handled");

    assert!(
        app.chat.is_following_tail(),
        "the wheel should be swallowed"
    );
}

/// An empty store still opens — the modal says so honestly rather than
/// refusing to open at all.
#[tokio::test]
async fn control_r_over_an_empty_store_still_opens() {
    let mut app = app();

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");

    assert!(app.history_search.is_some());
}

#[tokio::test]
async fn snapshot_history_search_open() {
    let directory = temporary();
    let mut app = app_with_history(
        &directory,
        &["what does this crate do", "commit the fix", "git status"],
    );

    app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .expect("control-r is handled");

    assert!(
        app.history_search.is_some(),
        "the search must be open, or the snapshot is of a bare screen"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// A pick has to outlive the run that made it, and the theme it names has
/// to be the one the next run opens on.
#[tokio::test]
async fn a_kept_theme_is_stored_and_reopened_next_run() {
    let directory = temporary();
    let store = directory.path().join("tui.json");

    let mut themes = Themes::builtin();
    themes.adopt_store(store.clone());
    let mut app = App::new(engine(), None, themes);

    // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
    // own default chord is gone, so these fixtures open it the way
    // `snapshot_help_dialog_open` opens the reference card.
    app.run_command(command::Action::Themes).await;
    app.handle(key(KeyCode::Char('k'), KeyModifiers::NONE))
        .await
        .expect("k is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    let kept = app.theme.name().to_owned();

    let mut reopened = Themes::builtin();
    reopened.adopt_store(store);
    let next_run = App::new(engine(), None, reopened);

    assert_eq!(next_run.theme.name(), kept);
    assert_ne!(kept, DEFAULT_THEME, "the fixture must have changed it");
}

/// A cancel must not leave the previewed name behind in the store.
#[tokio::test]
async fn a_cancelled_preview_is_never_written_down() {
    let directory = temporary();
    let store = directory.path().join("tui.json");

    let mut themes = Themes::builtin();
    themes.adopt_store(store.clone());
    let mut app = App::new(engine(), None, themes);

    // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
    // own default chord is gone, so these fixtures open it the way
    // `snapshot_help_dialog_open` opens the reference card.
    app.run_command(command::Action::Themes).await;
    app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
        .await
        .expect("j is handled");
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    assert!(
        !store.exists(),
        "nothing was confirmed, so nothing is stored"
    );
}

/// A theme switch has to reach the editor and the cached transcript, not
/// just the panes redrawn from scratch every frame.
#[tokio::test]
async fn a_theme_switch_repaints_the_whole_screen() {
    let mut app = themed_app("aura");
    palette_transcript(&mut app);

    let mut screen_buffer = terminal(80, 24);
    app.draw(&mut screen_buffer).expect("a frame draws");
    let (glyphs_before, styles_before) = (screen(&screen_buffer), styled_screen(&screen_buffer));

    app.apply_theme("gruvbox");
    app.draw(&mut screen_buffer).expect("a frame draws");

    assert_eq!(
        glyphs_before,
        screen(&screen_buffer),
        "a theme switch must not move a single glyph"
    );
    assert_ne!(
        styles_before,
        styled_screen(&screen_buffer),
        "nothing was repainted"
    );
}

#[test]
fn snapshot_theme_opencode() {
    let mut app = themed_app(DEFAULT_THEME);
    palette_transcript(&mut app);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(styled_screen(&terminal));
}

#[test]
fn snapshot_theme_tokyonight() {
    let mut app = themed_app("tokyonight");
    palette_transcript(&mut app);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(styled_screen(&terminal));
}

#[test]
fn snapshot_theme_gruvbox() {
    let mut app = themed_app("gruvbox");
    palette_transcript(&mut app);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(styled_screen(&terminal));
}

#[test]
fn snapshot_theme_aura() {
    let mut app = themed_app("aura");
    palette_transcript(&mut app);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(styled_screen(&terminal));
}

/// Opened the way a user opens it, and dumped with styles: this is what
/// pins the selected row's fill and the panel surface behind the list.
#[tokio::test]
async fn snapshot_themes_dialog_open() {
    let mut app = app();
    palette_transcript(&mut app);

    // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
    // own default chord is gone, so these fixtures open it the way
    // `snapshot_help_dialog_open` opens the reference card.
    app.run_command(command::Action::Themes).await;

    assert!(
        app.theme_list.is_some(),
        "the picker must be open, or the snapshot is of a bare screen"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(styled_screen(&terminal));
}

#[tokio::test]
async fn snapshot_sessions_picker_after_moving_the_selection() {
    let directory = temporary();
    store_pickable_sessions(&directory);
    let mut app = persistent_app(&directory);

    app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .expect("control-s is handled");
    app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
        .await
        .expect("j is handled");

    assert_eq!(
        app.sessions
            .as_ref()
            .and_then(|sessions| sessions.selected())
            .map(|info| info.id.as_str()),
        Some("0198f2c4-a1b0-7000-8000-000000000012"),
        "j should move down one row rather than reaching the editor"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

// ---- D505: the session socket follows the slot through the app's doors ----

/// The socket a lead serves follows the engine's session slot through
/// this app's own two doors (**D505**): bound on the first pass under the
/// id the engine was minted with, moved to the stored session the picker
/// resumes — the old one shut down first — moved again by `/new`, and
/// shut down on the exit path. The fake binder records the ids; the real
/// one is proved end to end in `ganja-cli/tests/session_socket.rs`.
#[tokio::test]
async fn the_session_socket_follows_the_picker_and_new_through_the_apps_doors() {
    let directory = temporary();
    store_pickable_sessions(&directory);
    let recording = Arc::new(crate::binder::fake::Recording::default());
    let mut app = persistent_app(&directory).with_socket(
        Box::new(Arc::clone(&recording)),
        crate::binder::fake::served(),
    );
    let minted = app.engine.session_id();

    // The first pass, whatever event carries it, binds under the minted id.
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    assert_eq!(
        recording.bound.lock().expect("not poisoned").as_slice(),
        std::slice::from_ref(&minted),
        "bound once, under the id the engine started on"
    );

    // The picker: Ctrl-S opens it and Enter resumes the row under the
    // cursor, which is a stored session with an id of its own.
    app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .expect("control-s is handled");
    let chosen = app
        .sessions
        .as_ref()
        .and_then(|sessions| sessions.selected())
        .map(|info| info.id.clone())
        .expect("the picker has a row under the cursor");
    assert_ne!(chosen, minted);
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert_eq!(app.engine.session_id(), chosen, "the resume moved the slot");
    assert_eq!(
        recording.bound.lock().expect("not poisoned").as_slice(),
        &[minted.clone(), chosen.clone()],
        "the resume rebound under the stored session's id"
    );
    assert_eq!(
        recording.closed.lock().expect("not poisoned").as_slice(),
        &[crate::binder::fake::Recording::path_for(&minted)],
        "and the minted session's socket was shut down first"
    );

    // `/new` through the composer, as a person types it.
    for event in typing("/new") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    let fresh = app.engine.session_id();
    assert_ne!(fresh, chosen, "/new re-minted the id");
    assert_eq!(
        recording.bound.lock().expect("not poisoned").as_slice(),
        &[minted.clone(), chosen.clone(), fresh.clone()],
        "and the socket followed it"
    );
    assert_eq!(
        recording.closed.lock().expect("not poisoned").len(),
        2,
        "the resumed session's socket was shut down before the fresh bind"
    );

    // The exit path: what `App::run` does after the loop.
    app.socket
        .as_mut()
        .expect("this app serves a socket")
        .shutdown()
        .await;
    assert_eq!(
        recording.closed.lock().expect("not poisoned").last(),
        Some(&crate::binder::fake::Recording::path_for(&fresh)),
        "exit shuts the bound socket down"
    );
}

// ---- D527: registration lifecycle ----

/// A binder that mirrors `binder::fake::Recording`'s recording and
/// refusal behavior, but — unlike it — names a bound path after the
/// session id's **compact hex** form, the real binder's own naming rule
/// (`ganja-serve/src/socket.rs:87-105`): a registration record's stem
/// is read off the bound path, and only a compact-hex path can stand
/// in for one in a test. `binder.rs` itself stays byte-untouched
/// (AC-29) — this is a second, local fixture, not an edit to the
/// shared one.
#[derive(Default)]
struct CompactRecording {
    bound: std::sync::Mutex<Vec<SessionId>>,
    closed: Arc<std::sync::Mutex<Vec<PathBuf>>>,
    refuse: std::sync::atomic::AtomicBool,
}

impl CompactRecording {
    fn path_for(id: &SessionId) -> PathBuf {
        PathBuf::from(format!("/nowhere/{}.sock", compact(id)))
    }
}

/// `id`'s compact hex form — dashes stripped, the real binder's own
/// naming rule — for a test to compute the stem a bound path (real or
/// [`CompactRecording`]'s) will actually carry.
fn compact(id: &SessionId) -> String {
    id.as_str()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect()
}

struct CompactBound {
    path: PathBuf,
    closed: Arc<std::sync::Mutex<Vec<PathBuf>>>,
}

impl binder::Bound for CompactBound {
    fn path(&self) -> &Path {
        &self.path
    }

    fn shutdown(self: Box<Self>) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
        self.closed
            .lock()
            .expect("not poisoned")
            .push(self.path.clone());
        Box::pin(async { Ok(()) })
    }
}

impl binder::Binder for Arc<CompactRecording> {
    fn bind(
        &self,
        _engine: Arc<Engine>,
        id: SessionId,
        _served: binder::Served,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Box<dyn binder::Bound>>> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Box::pin(async { Err(anyhow::anyhow!("the directory is not ours")) });
        }
        self.bound.lock().expect("not poisoned").push(id.clone());
        let bound: Box<dyn binder::Bound> = Box::new(CompactBound {
            path: CompactRecording::path_for(&id),
            closed: Arc::clone(&self.closed),
        });

        Box::pin(async move { Ok(bound) })
    }
}

/// A lead app over [`CompactRecording`], registering into
/// `registry_dir` instead of a real person's `/tmp/ganja-<uid>/`.
fn registering_app(directory: &TempDir, registry_dir: &TempDir) -> (App, Arc<CompactRecording>) {
    let recording = Arc::new(CompactRecording::default());
    let app = persistent_app(directory)
        .with_socket(
            Box::new(Arc::clone(&recording)),
            crate::binder::fake::served(),
        )
        .with_registry_directory(registry_dir.path());

    (app, recording)
}

/// AC-2: a lead's own record appears beside its bound socket on the
/// first pass, a rebind moves it — the old one removed the moment the
/// slot is observed to move (P3), before the new bind's own outcome is
/// known — and teardown removes it before the socket is asked to close.
#[tokio::test]
async fn a_lead_session_registers_beside_its_socket_and_unregisters_on_teardown() {
    let directory = temporary();
    store_pickable_sessions(&directory);
    let registry_dir = temporary();
    let (mut app, _recording) = registering_app(&directory, &registry_dir);
    let minted = app.engine.session_id();

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    let minted_record = registry::record_path(registry_dir.path(), &compact(&minted));
    assert!(
        minted_record.exists(),
        "a record appears beside the bound socket"
    );
    let read: registry::Record =
        serde_json::from_slice(&fs::read(&minted_record).expect("the record reads"))
            .expect("the record is JSON");
    assert_eq!(read.session_id, minted.as_str());

    // The rebind: the picker moves the slot.
    app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .expect("control-s is handled");
    let chosen = app
        .sessions
        .as_ref()
        .and_then(|sessions| sessions.selected())
        .map(|info| info.id.clone())
        .expect("the picker has a row under the cursor");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert_eq!(app.engine.session_id(), chosen, "the resume moved the slot");

    assert!(
        !minted_record.exists(),
        "the old record is gone the moment the slot moved"
    );
    let chosen_record = registry::record_path(registry_dir.path(), &compact(&chosen));
    assert!(
        chosen_record.exists(),
        "a fresh record appears beside the rebound socket"
    );

    // `App::run`'s own teardown order: the record goes before the
    // socket is asked to close.
    app.unregister_self();
    assert!(!chosen_record.exists(), "teardown removes the record");
}

/// AC-3: a refused **first** bind writes nothing at all, and a refused
/// **rebind** writes no new record — the old one's removal is the
/// previous test's outcome, not a contradiction of this one.
#[tokio::test]
async fn a_refused_bind_writes_no_record() {
    let directory = temporary();
    let registry_dir = temporary();
    let (mut app, recording) = registering_app(&directory, &registry_dir);
    recording
        .refuse
        .store(true, std::sync::atomic::Ordering::SeqCst);

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert!(
        fs::read_dir(registry_dir.path())
            .expect("the directory reads")
            .next()
            .is_none(),
        "a refused first bind writes nothing"
    );
}

#[tokio::test]
async fn a_refused_rebind_writes_no_new_record_but_the_old_one_still_goes() {
    let directory = temporary();
    store_pickable_sessions(&directory);
    let registry_dir = temporary();
    let (mut app, recording) = registering_app(&directory, &registry_dir);
    let minted = app.engine.session_id();
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    let minted_record = registry::record_path(registry_dir.path(), &compact(&minted));
    assert!(minted_record.exists());

    recording
        .refuse
        .store(true, std::sync::atomic::Ordering::SeqCst);
    app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .expect("control-s is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(
        !minted_record.exists(),
        "the old record is still removed on observing the slot move"
    );
    assert!(
        fs::read_dir(registry_dir.path())
            .expect("the directory reads")
            .next()
            .is_none(),
        "a refused rebind writes no new record"
    );
}

/// AC-6: registration never refuses a collision — it notices instead,
/// naming the live holder's stem and cwd, and still registers.
#[tokio::test]
async fn a_name_collision_is_a_notice_never_a_refusal_at_registration() {
    let directory = temporary();
    let registry_dir = temporary();
    let holder_stem = "0298c1a2";
    registry::write(
        registry_dir.path(),
        holder_stem,
        &registry::Record {
            format: registry::FORMAT,
            session_id: "0298c1a2-0000-7000-8000-000000000002".to_owned(),
            name: "worker".to_owned(),
            name_source: registry::NameSource::User,
            cwd: "/work/holder".into(),
            root: "/work/holder".into(),
            pid: 1,
            started_at: 0,
        },
    )
    .expect("the fixture writes");
    let held =
        ganja_tool::socket::open_lock(&registry_dir.path().join(format!("{holder_stem}.sock")))
            .expect("the lock file opens");
    held.try_lock().expect("nothing else holds a fresh lock");

    let (mut app, _recording) = registering_app(&directory, &registry_dir);
    app.engine.set_self_name("worker");

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    let line = status_line(&mut app);
    assert!(line.contains("worker"), "{line}");
    assert!(line.contains(holder_stem), "{line}");

    let record = registry::record_path(registry_dir.path(), &compact(&app.engine.session_id()));
    assert!(
        record.exists(),
        "registration succeeds despite the collision"
    );
}

/// AC-7: `/rename` rewrites a lead's own record in place — same stem,
/// old name gone from the file — surfacing the collision notice when the
/// new name is held, and refusing a grammar violation with AC-5's own
/// sentence.
#[tokio::test]
async fn rename_rewrites_a_leads_record_in_place() {
    let directory = temporary();
    let registry_dir = temporary();
    let (mut app, _recording) = registering_app(&directory, &registry_dir);
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    let stem = compact(&app.engine.session_id());
    let record_path = registry::record_path(registry_dir.path(), &stem);
    assert!(record_path.exists(), "the record exists before the rename");

    for event in typing("/rename fresh") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.engine.self_name(), "fresh");
    let read: registry::Record =
        serde_json::from_slice(&fs::read(&record_path).expect("the record still reads"))
            .expect("json");
    assert_eq!(read.name, "fresh", "same stem, name rewritten in place");
    assert_eq!(read.name_source, registry::NameSource::User);

    // A grammar violation refuses with AC-5's own sentence, and renames
    // nothing.
    for event in typing("/rename a@b") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert_eq!(
        app.engine.self_name(),
        "fresh",
        "the refused rename changed nothing"
    );
    let line = status_line(&mut app);
    assert!(
        line.contains("scopes an address"),
        "the grammar's own sentence: {line}"
    );
}

/// AC-40 TUI half: in a **teamless** session (no socket, no record),
/// `/rename` still updates the engine's self-name cell and still
/// surfaces the collision notice against a live record (**F9**).
#[tokio::test]
async fn teamless_rename_updates_the_cell_and_still_warns_of_collisions() {
    let registry_dir = temporary();
    let holder_stem = "0398d3c4";
    registry::write(
        registry_dir.path(),
        holder_stem,
        &registry::Record {
            format: registry::FORMAT,
            session_id: "0398d3c4-0000-7000-8000-000000000003".to_owned(),
            name: "fresh".to_owned(),
            name_source: registry::NameSource::User,
            cwd: "/work/holder".into(),
            root: "/work/holder".into(),
            pid: 1,
            started_at: 0,
        },
    )
    .expect("the fixture writes");
    let held =
        ganja_tool::socket::open_lock(&registry_dir.path().join(format!("{holder_stem}.sock")))
            .expect("the lock file opens");
    held.try_lock().expect("nothing else holds a fresh lock");

    // No `with_socket`: a teamless session binds no socket and has no
    // record of its own to rewrite.
    let mut app = app().with_registry_directory(registry_dir.path());
    assert!(app.registered.is_none(), "a teamless session has no record");

    for event in typing("/rename fresh") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.engine.self_name(), "fresh");
    let line = status_line(&mut app);
    assert!(line.contains(holder_stem), "{line}");
}

/// **S1 / AC-39**: `register_self`'s own collision notice seeds the
/// incumbent's throttle, so a holder that predated registration is never
/// re-reported under the other notice's wording — asserted here on the
/// **un-primed** path, with nothing manually setting
/// [`App::collision_scanned`], because that is what a real first pass
/// runs on. Once that is settled, the incumbent's own re-scan, throttled
/// to once per [`COLLISION_RESCAN_INTERVAL`], surfaces "another session
/// registered your name" once per newly seen collider — never refusing
/// anything.
#[tokio::test]
async fn the_incumbents_collision_scan_warns_once_per_newly_seen_collider() {
    let directory = temporary();
    let registry_dir = temporary();

    // A holder already answers to this session's name *before* it ever
    // registers — the scenario AC-6 covers for the registering side's
    // own notice; what is new here is the *incumbent's* side of it.
    let preexisting_stem = "0598f5e6";
    registry::write(
        registry_dir.path(),
        preexisting_stem,
        &registry::Record {
            format: registry::FORMAT,
            session_id: "0598f5e6-0000-7000-8000-000000000005".to_owned(),
            name: "worker".to_owned(),
            name_source: registry::NameSource::User,
            cwd: "/work/preexisting".into(),
            root: "/work/preexisting".into(),
            pid: 3,
            started_at: 0,
        },
    )
    .expect("the fixture writes");
    let preexisting_held = ganja_tool::socket::open_lock(
        &registry_dir.path().join(format!("{preexisting_stem}.sock")),
    )
    .expect("the lock file opens");
    preexisting_held
        .try_lock()
        .expect("nothing else holds a fresh lock");

    let (mut app, _recording) = registering_app(&directory, &registry_dir);
    app.engine.set_self_name("worker");
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    let name = app.engine.self_name();
    assert_eq!(name, "worker");

    // The un-primed path (**S1**): nothing here manually sets
    // `collision_scanned`, so this is genuinely the first pass, due
    // immediately because registration never sets it either. It must
    // not re-report the holder that predated registration — that
    // holder was already named by `register_self`'s own notice.
    app.status.set_notice(None);
    app.poll_collision_scan();
    assert!(
        !status_line(&mut app).contains("registered your name"),
        "a holder seen at registration is not reported as newly arrived: {}",
        status_line(&mut app)
    );

    // A collider registers under the same name, after this session
    // already holds it.
    let collider_stem = "0498e4d5";
    registry::write(
        registry_dir.path(),
        collider_stem,
        &registry::Record {
            format: registry::FORMAT,
            session_id: "0498e4d5-0000-7000-8000-000000000004".to_owned(),
            name: name.clone(),
            name_source: registry::NameSource::User,
            cwd: "/work/collider".into(),
            root: "/work/collider".into(),
            pid: 2,
            started_at: 0,
        },
    )
    .expect("the fixture writes");
    let held =
        ganja_tool::socket::open_lock(&registry_dir.path().join(format!("{collider_stem}.sock")))
            .expect("the lock file opens");
    held.try_lock().expect("nothing else holds a fresh lock");

    // Not due yet: the throttle has not elapsed.
    app.poll_collision_scan();
    assert!(
        !status_line(&mut app).contains("registered your name"),
        "the scan has not run yet"
    );

    // Force the throttle open, as a real thirty seconds would.
    app.collision_scanned =
        Some(Instant::now() - super::COLLISION_RESCAN_INTERVAL - Duration::from_secs(1));
    app.poll_collision_scan();
    let line = status_line(&mut app);
    assert!(line.contains("registered your name"), "{line}");
    assert!(line.contains(collider_stem), "{line}");

    // A second scan, immediately due again, warns nobody twice.
    app.status.set_notice(None);
    app.collision_scanned =
        Some(Instant::now() - super::COLLISION_RESCAN_INTERVAL - Duration::from_secs(1));
    app.poll_collision_scan();
    assert!(
        !status_line(&mut app).contains("registered your name"),
        "the same collider is not warned about twice"
    );

    drop(held);
}

// ---- D529: the `@` menu's roster and live-session rows ----

fn live_session(name: &str, stem: &str, cwd: &str) -> lister::LiveSession {
    lister::LiveSession {
        name: name.to_owned(),
        name_source: registry::NameSource::Derived,
        session_id: format!("{stem}-0000-7000-8000-000000000009"),
        stem: stem.to_owned(),
        socket: format!("/tmp/ganja-0/{stem}.sock").into(),
        cwd: cwd.into(),
        health: lister::Health::Answered,
    }
}

/// AC-27: with no lister, the menu offers files and roster only —
/// nothing else regresses.
#[tokio::test]
async fn with_no_lister_the_at_menu_offers_files_and_roster_only() {
    let directory = project();
    let mut app = app_in(&directory);

    typed(&mut app, "compare @lib").await;

    let files = app.files.as_ref().expect("the menu is open");
    assert!(
        !files
            .rows()
            .iter()
            .any(|row| matches!(row, MenuRow::Session { .. })),
        "no session rows without a lister"
    );
}

/// AC-37 (D530): a **teamless** interactive session — no member, no
/// team — still shows live-session rows once a lister is injected, the
/// gate wider than the socket's.
#[tokio::test]
async fn a_teamless_session_shows_live_session_rows_when_a_lister_is_injected() {
    let directory = project();
    let recording = Arc::new(crate::lister::fake::Recording::default());
    recording.set(lister::Listing::Complete(vec![live_session(
        "backend",
        "0298c1a2",
        "/work/backend",
    )]));
    let mut app = app_in(&directory).with_lister(Box::new(recording));

    typed(&mut app, "ping @back").await;

    let files = app.files.as_ref().expect("the menu is open");
    assert!(
        files
            .rows()
            .iter()
            .any(|row| matches!(row, MenuRow::Session { name, .. } if name == "backend")),
        "a teamless session sees live-session rows"
    );
}

/// AC-37: a pane **member** shows roster rows only — no lister is ever
/// handed to a member (`lib.rs`'s own gate), so its menu offers files
/// and its roster and nothing else, however many live sessions exist.
#[tokio::test]
async fn a_member_shows_roster_rows_only_never_live_sessions() {
    let directory = temporary();
    let (mut app, _events) = membered(&directory).await;
    app.cwd = directory.path().to_path_buf();
    // A member is never handed a lister at all (`lib.rs`'s own gate) —
    // this app simply has none, which is the production shape.
    assert!(app.lister.is_none());

    for event in typing("ping @anything") {
        app.handle(event).await.expect("typing is handled");
    }
    settle_file_menu(&mut app).await;

    if let Some(files) = &app.files {
        assert!(
            !files
                .rows()
                .iter()
                .any(|row| matches!(row, MenuRow::Session { .. })),
            "a member's menu never carries session rows"
        );
    }
}

/// AC-23 + AC-28: a live session appears in the menu, snapshot-pinned;
/// a partial listing marks the menu incomplete and still completes.
#[tokio::test]
async fn snapshot_at_menu_shows_a_live_session_row() {
    let directory = project();
    let recording = Arc::new(crate::lister::fake::Recording::default());
    recording.set(lister::Listing::Complete(vec![live_session(
        "backend",
        "0298c1a2",
        "/work/backend",
    )]));
    let mut app = app_in(&directory).with_lister(Box::new(recording));

    typed(&mut app, "ping @back").await;
    assert!(app.files.is_some());

    let mut terminal = terminal(80, 16);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn a_partial_listing_marks_the_menu_incomplete_and_still_completes() {
    let directory = project();
    let recording = Arc::new(crate::lister::fake::Recording::default());
    recording.set(lister::Listing::Partial {
        rows: vec![live_session("backend", "0298c1a2", "/work/backend")],
        error: "the directory could not be fully read".to_owned(),
    });
    let mut app = app_in(&directory).with_lister(Box::new(recording));

    typed(&mut app, "ping @back").await;

    let files = app.files.as_ref().expect("the menu is open");
    assert!(
        files
            .selected()
            .is_some_and(|row| matches!(row, MenuRow::Session { name, .. } if name == "backend")),
        "the partial listing's own row still shows"
    );

    let mut terminal = terminal(80, 16);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("partial"),
        "the incomplete marker shows"
    );
}

/// AC-23 (ADJ-3): completing a duplicate-named session row splices the
/// `@`-prefixed `uds:` spelling, byte for byte; a unique row splices the
/// bare name.
#[tokio::test]
async fn a_colliding_session_completion_splices_its_uds_address() {
    let directory = project();
    let recording = Arc::new(crate::lister::fake::Recording::default());
    recording.set(lister::Listing::Complete(vec![
        live_session("worker", "0298c1a2", "/work/a"),
        live_session("worker", "0398d3c4", "/work/b"),
    ]));
    let mut app = app_in(&directory).with_lister(Box::new(recording));

    typed(&mut app, "ping @work").await;
    // Both rows are files-then-roster-then-sessions; select the first
    // session row (index 0, since there are no files or roster rows for
    // this fragment).
    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");

    let prompt = app.editor.text();
    assert!(
        prompt.contains("@uds:/tmp/ganja-0/0298c1a2.sock"),
        "a colliding row splices its exact uds: address: {prompt}"
    );
}

// ---- D529: submit-time classification (AC-22) ----

/// AC-22: a token resolving to a real file rides `mentions` even when a
/// live session shares the name (file wins, first); a non-file token
/// matching the roster or the listed live sessions rides
/// `session_mentions`, as does a `uds:`-prefixed token whether or not it
/// matches a listing; anything else stays literal.
#[tokio::test]
async fn a_token_that_is_neither_file_nor_name_stays_literal() {
    let root = project();
    std::fs::write(root.path().join("backend"), "shadowing file\n")
        .expect("the fixture file writes");
    let mut app = App::new(engine(), None, Themes::builtin())
        .with_cwd(root.path())
        .with_root(root.path());
    app.session_listing = vec![live_session("worker", "0298c1a2", "/work/a")];

    let mentions = mention::attachable(
        "@backend @worker @uds:/tmp/ganja-0/x.sock @nobody",
        &app.root,
    );
    let session_mentions = app.session_mention_tokens(
        "@backend @worker @uds:/tmp/ganja-0/x.sock @nobody",
        &mentions,
    );

    assert_eq!(
        mentions.iter().map(|m| m.path.as_str()).collect::<Vec<_>>(),
        vec!["backend"],
        "the real file wins, even though a live session shares its name"
    );
    assert_eq!(
        session_mentions,
        vec!["worker".to_owned(), "uds:/tmp/ganja-0/x.sock".to_owned()],
        "the roster/session name and the uds: token both classify; @nobody stays literal"
    );
}

// ---- D495: peer text never reaches session_mentions (AC-26 TUI half) ----

/// AC-26 TUI half: `deliver_peers` (via `start_peer_turn`, the not-yet-
/// running-turn arm) sends a peer's `@`- and `$`-laden body through
/// with `mentions`, `skills` **and** `session_mentions` all empty — the
/// engine never resolves a peer's own words as a mention (**D495**),
/// confirmed here by the resulting message carrying no
/// `session_mention`-tagged reminder part despite the body naming a
/// live-looking session.
#[tokio::test]
async fn deliver_peers_sends_no_session_mentions() {
    let directory = temporary();
    let (mut app, _registry, mut events) = leading(&directory).await;

    assert!(
        app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
            "w1",
            "2026-08-17T00:00:00.000Z",
            "check in with @backend and run $porting",
            ganja_core::teammate::Delivery::FireAndForget,
        )])
        .await,
        "the not-yet-running-turn arm sends a prompt"
    );

    let CoreEvent::MessageStarted {
        session_id: _,
        message,
    } = events.next().await.expect("the engine reports the prompt")
    else {
        panic!("the first event should be the prompt starting");
    };

    assert!(
        !message
            .parts
            .iter()
            .filter_map(ganja_protocol::Part::as_text)
            .any(|text| text.contains(ganja_core::teammate::identity::TAG)),
        "no session-mention reminder part is appended for a peer's own words"
    );
}

/// An engine carrying the four builtin agents, which is what the agent
/// list and Tab both read.
fn agentic_app() -> App {
    let registry = Arc::new(
        ganja_core::AgentRegistry::from_config(&ganja_core::config::Config::default())
            .expect("the builtin agents resolve"),
    );
    let engine = Engine::new(
        Arc::new(FakeProvider::default()),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
    )
    .with_agents(registry);

    App::new(engine, None, Themes::builtin())
}

/// The whole point of `ctrl+p`: it reaches the list of everything else.
#[tokio::test]
async fn control_p_opens_the_palette_and_escape_closes_it() {
    let mut app = app();

    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    assert!(app.palette.is_some(), "the palette should be open");

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.palette.is_none(), "escape should close it");
}

#[tokio::test]
async fn typing_in_the_palette_filters_it_rather_than_reaching_the_editor() {
    let mut app = app();
    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");

    // `j` and `k` are movement in the dialogs with no filter line; here
    // they have to be text, or half the alphabet cannot be searched for.
    for event in typing("jk") {
        app.handle(event).await.expect("typing is handled");
    }

    assert_eq!(
        app.palette.as_ref().map(Palette::filter),
        Some("jk"),
        "the keys should have reached the filter"
    );
    assert!(
        app.editor.prompt().is_none(),
        "and not the editor underneath"
    );
}

#[tokio::test]
async fn the_palette_runs_the_command_under_its_cursor() {
    let mut app = app();
    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    for event in typing("themes") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(
        app.palette.is_none(),
        "running a command closes the palette"
    );
    assert!(app.theme_list.is_some(), "and opens what it named");
}

/// Closing is not forgetting: a glance at the screen mid-search should not
/// cost the search.
#[tokio::test]
async fn a_reopened_palette_still_holds_what_was_typed_into_it() {
    let mut app = app();
    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    for event in typing("mo") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");

    assert_eq!(app.palette.as_ref().map(Palette::filter), Some("mo"));
}

#[tokio::test]
async fn the_palette_reaches_every_command_it_lists() {
    let cases = [
        ("help", (|app: &App| app.help.is_some()) as fn(&App) -> bool),
        ("themes", |app: &App| app.theme_list.is_some()),
        ("models", |app: &App| app.chooser.is_some()),
        ("agents", |app: &App| app.chooser.is_some()),
        ("exit", |app: &App| app.quit),
    ];

    for (typed, opened) in cases {
        let mut app = agentic_app().with_provider("anthropic");
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing(typed) {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(opened(&app), "/{typed} should have done something");
    }
}

/// **D519.** A `--backend` slot raises the values menu over the parser's
/// own surfaces, and Tab puts the chosen one in place of the partial word
/// with the space that ends the slot — the line waits, nothing runs.
#[tokio::test]
async fn tab_completes_a_backend_from_the_parsers_own_list() {
    let mut app = app();
    for event in typing("/team spawn foo --backend g") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(
        app.dropdown.is_some(),
        "the backend slot should raise the menu"
    );
    assert!(app.completion.is_some());

    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");

    assert_eq!(app.editor.text(), "/team spawn foo --backend ganja ");
    assert!(app.dropdown.is_none(), "choosing closes the menu");
    assert!(app.completion.is_none());
}

/// **D519.** Once the value is fully typed the menu is gone, so the
/// Enter that follows reaches the line — the spawn drills depend on it.
#[tokio::test]
async fn a_fully_typed_backend_closes_the_menu_before_enter() {
    let mut app = app();
    for event in typing("/team spawn w1 --backend ganj") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(app.dropdown.is_some());
    for event in typing("a") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(app.dropdown.is_none(), "nothing left to complete");
    assert_eq!(app.editor.text(), "/team spawn w1 --backend ganja");
}

/// **D519.** The slot after `/team` is the subcommand, and Enter fills it
/// the way Tab does — a subcommand is not a thing to run by itself.
#[tokio::test]
async fn enter_fills_a_team_subcommand_without_submitting() {
    let mut app = app();
    for event in typing("/team sh") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(app.dropdown.is_some());

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.editor.text(), "/team shutdown ");
    assert!(app.dropdown.is_none());
}

/// **D519.** Where the grammar has no slot — a name, a prompt word — no
/// menu opens, and Esc on an open one keeps what was typed (**D11**).
#[tokio::test]
async fn free_words_raise_no_values_menu_and_esc_keeps_the_text() {
    let mut app = app();
    for event in typing("/team spawn fo") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(
        app.dropdown.is_none(),
        "a member name is anyone's to choose"
    );

    for event in typing(" --agent ") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(
        app.dropdown.is_some(),
        "the agent slot should raise the menu"
    );

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("esc is handled");
    assert!(app.dropdown.is_none());
    assert_eq!(app.editor.text(), "/team spawn fo --agent ");
}

/// The trigger, at the level the user meets it: a slash that starts the
/// buffer raises the menu and a slash anywhere else does not.
#[tokio::test]
async fn the_command_menu_opens_on_a_leading_slash_and_on_nothing_else() {
    let mut leading = app();
    for event in typing("/") {
        leading.handle(event).await.expect("typing is handled");
    }
    assert!(leading.dropdown.is_some(), "a leading slash should open it");

    let mut midway = app();
    for event in typing("what about /tmp") {
        midway.handle(event).await.expect("typing is handled");
    }
    assert!(
        midway.dropdown.is_none(),
        "a slash mid-sentence is a path, not a command"
    );
}

#[tokio::test]
async fn the_command_menu_closes_once_the_slash_is_backspaced_away() {
    let mut app = app();
    for event in typing("/mo") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(app.dropdown.is_some());

    for _ in 0..3 {
        app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
            .await
            .expect("backspace is handled");
    }

    assert!(app.dropdown.is_none(), "an empty buffer is not a command");
}

/// **D11**: upstream deletes the typed `/xyz` whenever the menu closes.
/// Reverting that divergence — wiping the buffer in `handle_dropdown_key`'s
/// escape arm — fails this test.
#[tokio::test]
async fn escape_closes_the_command_menu_and_keeps_what_was_typed() {
    let mut app = app();
    for event in typing("/models") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    assert!(app.dropdown.is_none(), "the menu should have closed");
    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("/models"),
        "the text must survive, where upstream deletes it"
    );
}

#[tokio::test]
async fn enter_on_the_command_menu_runs_the_command_and_empties_the_editor() {
    let mut app = app();
    for event in typing("/themes") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.theme_list.is_some(), "the command should have run");
    assert!(
        app.editor.prompt().is_none(),
        "the text that named it has done its job"
    );
}

/// **F1**, **D446**: Tab completes the buffer and closes the menu without
/// running anything, for a UI command — the one place Tab and Enter part
/// ways, since upstream's own Tab binding dispatches a UI command exactly
/// as Enter does.
#[tokio::test]
async fn tab_on_the_command_menu_completes_the_buffer_without_running_it() {
    let mut app = app();
    for event in typing("/the") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");

    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("/themes "),
        "the selected row's name, not the fragment that narrowed to it"
    );
    assert!(app.dropdown.is_none(), "choosing closes the menu");
    assert!(
        app.theme_list.is_none(),
        "Tab must not run the command the way Enter does"
    );
}

/// The same claim from the engine's side of the wire: nothing reaches it.
#[tokio::test]
async fn tab_on_a_ui_command_never_reaches_the_engine() {
    let (mut app, mut events) = wired().await;
    for event in typing("/new") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");

    assert_eq!(app.editor.prompt().as_deref(), Some("/new "));
    assert!(
        events.next().now_or_never().is_none(),
        "nothing should have been sent to the engine"
    );
}

#[tokio::test]
async fn the_arrow_keys_steer_the_command_menu_instead_of_the_transcript() {
    let mut app = app();
    for event in typing("/") {
        app.handle(event).await.expect("typing is handled");
    }
    let first = app
        .dropdown
        .as_ref()
        .and_then(Dropdown::selected)
        .expect("a menu with rows");

    app.handle(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .expect("down is handled");

    assert_ne!(
        app.dropdown.as_ref().and_then(Dropdown::selected),
        Some(first),
        "down should have moved the cursor"
    );
}

/// **F6**, **D445**. The flag is the whole feature: `App::draw` already
/// turns it into a `terminal.clear()` for the external-editor return
/// path, so pinning that Ctrl+L sets it is what pins the key to the
/// behavior without re-testing `draw` itself.
#[tokio::test]
async fn ctrl_l_marks_the_next_frame_stale() {
    let mut app = app();
    assert!(!app.stale, "nothing has asked for a repaint yet");

    app.handle(key(KeyCode::Char('l'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl-l is handled");

    assert!(app.stale, "the next draw should force a full repaint");

    let mut terminal = terminal(40, 12);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(!app.stale, "the flag is consumed by the draw it triggered");
}

/// The exact bug `stale` exists to prevent: something else — not this
/// `Terminal` — wrote over the screen, and an ordinary draw diffs against
/// what it still thinks is there, so it writes nothing and the
/// corruption survives. Ctrl+L is the hint that forces the real
/// `terminal.clear()` a plain `draw` has no reason to call on its own.
#[tokio::test]
async fn ctrl_l_forces_a_full_repaint_instead_of_a_diff_against_a_stale_screen() {
    let mut app = app();
    let mut terminal = terminal(40, 12);
    app.draw(&mut terminal).expect("the first frame draws");
    assert!(
        !screen(&terminal).trim().is_empty(),
        "the fixture app should have drawn something to corrupt"
    );

    // Something else wrote over the screen without this `Terminal`
    // knowing, the way an external editor does.
    terminal
        .backend_mut()
        .clear_region(ClearType::All)
        .expect("the backend clears");
    assert!(screen(&terminal).trim().is_empty(), "the corruption landed");

    // Nothing changed from this `Terminal`'s point of view, so an
    // ordinary draw trusts its own cache and never touches the
    // corrupted backend.
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).trim().is_empty(),
        "an undirected draw should not have repainted over the corruption"
    );

    app.handle(key(KeyCode::Char('l'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl-l is handled");
    app.draw(&mut terminal).expect("the redraw repaints");

    assert!(
        !screen(&terminal).trim().is_empty(),
        "ctrl+l should have forced a real repaint over the corruption"
    );
}

#[tokio::test]
async fn the_model_list_holds_the_running_providers_models_and_marks_the_active_one() {
    let mut served = app().with_provider("anthropic");
    served.open_models();

    let dialog = served.chooser.as_ref().expect("the list should be open");
    assert_eq!(dialog.0, Chooser::Models);
    assert!(
        !dialog.1.is_empty(),
        "anthropic has models in the compiled-in catalog"
    );
    assert!(
        served.wire_fetch.is_none(),
        "a cataloged provider never consults the wire"
    );
}

/// The fake model has no catalog row, so `/effort` has nothing to list
/// — and says ganja's sentence instead of opening an empty dialog.
#[tokio::test]
async fn the_effort_picker_refuses_a_model_with_nothing_to_offer() {
    let mut app = app();
    app.run_command(command::Action::Effort).await;

    assert!(app.chooser.is_none(), "there is nothing to choose from");
    assert!(
        status_line(&mut app).contains(NO_EFFORTS),
        "got {:?}",
        status_line(&mut app)
    );
}

/// Enter on the picker routes through [`Command::SwitchEffort`]; the
/// engine's refusal — the fake provider has no catalog rows — lands in
/// the status bar and leaves the list open, exactly as a refused model
/// switch does.
#[tokio::test]
async fn choosing_an_effort_sends_the_switch_and_surfaces_the_refusal() {
    let mut app = app();
    app.chooser = Some((
        Chooser::Effort,
        ListDialog::new(" effort ", effort::rows(["max"], None)),
    ));
    app.move_chooser(1);

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(
        status_line(&mut app).contains("not in the catalog"),
        "the engine's refusal should be readable: {:?}",
        status_line(&mut app)
    );
    assert!(
        app.chooser.is_some(),
        "a refused switch keeps the list the user was choosing from"
    );
}

/// The announcement is what moves the status line — whichever frontend
/// issued the switch — and clearing takes the segment away whole, so a
/// session back on Default renders the bar it always had.
#[tokio::test]
async fn an_effort_change_event_moves_the_status_line_and_a_clear_empties_it() {
    let mut app = app();
    assert!(!status_line(&mut app).contains("(max)"));

    app.handle(AppEvent::core(CoreEvent::EffortChanged {
        session_id: session(),
        effort: Some("max".to_owned()),
    }))
    .await
    .expect("the announcement is handled");
    assert_eq!(app.effort.as_deref(), Some("max"));
    assert!(
        status_line(&mut app).contains("canned (max)"),
        "the model and its effort belong together: {:?}",
        status_line(&mut app)
    );

    app.handle(AppEvent::core(CoreEvent::EffortChanged {
        session_id: session(),
        effort: None,
    }))
    .await
    .expect("the clear is handled");
    assert!(
        !status_line(&mut app).contains("canned (max)"),
        "Default has no segment: {:?}",
        status_line(&mut app)
    );
}

/// A wire-listed model row, in the shape the seam hands back.
fn listed(id: &str, name: &str) -> ganja_core::provider::ListedModel {
    ganja_core::provider::ListedModel {
        id: id.to_owned(),
        name: name.to_owned(),
    }
}

/// A whole seam answer around `models`. The notice is the seam's to write
/// and nothing in this crate renders it — the chooser shows rows — so a
/// fixture supplies any of them.
fn served(models: Vec<ganja_core::provider::ListedModel>) -> ganja_core::provider::WireModels {
    ganja_core::provider::WireModels {
        models,
        notice: "a listing fixture",
    }
}

/// Ticks until the in-flight listing fetch has been reaped.
///
/// The loop the real select loop runs, minus the frame budget: the fetch
/// finishes on its own schedule and the tick is what notices.
async fn reap_wire_fetch(app: &mut App) {
    for _ in 0..400 {
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        if app.wire_fetch.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    panic!("the listing fetch never finished");
}

/// A provider with no catalog rows goes to the wire's listing, and one
/// the seam does not serve either ends where it always did: the empty
/// list, one tick later.
#[tokio::test]
async fn a_provider_no_listing_serves_still_opens_the_empty_list_it_always_did() {
    let mut unknown = app().with_provider("a-provider-nothing-ships");
    unknown.open_models();
    assert!(
        unknown.chooser.is_none(),
        "nothing opens until the fetch lands"
    );

    reap_wire_fetch(&mut unknown).await;
    assert!(
        unknown
            .chooser
            .as_ref()
            .is_some_and(|(_, list)| list.is_empty()),
        "a provider with no catalog entries has nothing to offer"
    );
}

/// The wire path's row shape, from the cache: value and label are the id,
/// the display name rides beside it, and the session's model is the
/// active row the cursor starts on.
#[tokio::test]
async fn a_cached_wire_listing_opens_the_chooser_with_its_rows_and_marks_the_active_model() {
    let mut app = app().with_provider("cursor");
    app.model = "claude-4.5-opus".to_owned();
    app.wire_models = Some(vec![
        listed("gpt-5.3-codex", "Codex 5.3"),
        listed("claude-4.5-opus", "Claude 4.5 Opus"),
    ]);

    app.open_models();

    let (kind, dialog) = app.chooser.as_ref().expect("the cache opens the list");
    assert_eq!(*kind, Chooser::Models);
    assert_eq!(
        dialog.selected(),
        Some("claude-4.5-opus"),
        "the cursor starts on the model the session is on"
    );
    assert!(app.wire_fetch.is_none(), "a cache hit fetches nothing");

    let mut terminal = terminal(80, 14);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("gpt-5.3-codex"), "{screen}");
    assert!(
        screen.contains("Codex 5.3"),
        "the display name rides beside the id: {screen}"
    );
}

/// While the fetch is out, the status bar says so instead of the frame
/// freezing on the RPC or an empty dialog opening early.
#[tokio::test]
async fn an_uncataloged_model_list_says_it_is_fetching_while_the_wire_answers() {
    let mut app = app().with_provider("a-provider-nothing-ships");
    app.open_models();

    assert!(app.wire_fetch.is_some(), "the fetch is in flight");
    assert!(
        status_line(&mut app).contains("fetching a-provider-nothing-ships models"),
        "got: {}",
        status_line(&mut app)
    );
}

/// The guard the fetch slot doubles as: after a second `/model`, the
/// planted fetch is still the one the tick reaps, so completing it fills
/// the cache — a second spawn would have replaced it and answered
/// differently.
#[tokio::test]
async fn a_second_model_list_while_the_fetch_is_in_flight_does_not_spawn_another() {
    let mut app = app().with_provider("cursor");
    let (landing, planted) = tokio::sync::oneshot::channel();
    app.wire_fetch = Some(tokio::spawn(async move {
        planted.await.expect("the test completes the fetch")
    }));

    app.open_models();
    assert!(app.chooser.is_none(), "nothing to show yet");

    landing
        .send(Some(Ok(served(vec![listed("planted-one", "Planted One")]))))
        .expect("the fetch is still listening");
    reap_wire_fetch(&mut app).await;

    assert!(
        app.wire_models
            .as_ref()
            .is_some_and(|models| models[0].id == "planted-one"),
        "the planted fetch is the one that landed"
    );
    assert!(
        app.chooser
            .as_ref()
            .is_some_and(|(kind, list)| *kind == Chooser::Models && !list.is_empty()),
        "and its rows are what opened"
    );
}

/// Without this arm the tick never fires on an idle app and the finished
/// fetch waits for an unrelated keypress — the same reason the MCP dial
/// keeps the loop waking.
#[tokio::test]
async fn an_in_flight_wire_fetch_keeps_the_loop_waking_up() {
    let mut app = app().with_provider("cursor");
    let (_landing, planted) = tokio::sync::oneshot::channel::<WireListing>();
    app.wire_fetch = Some(tokio::spawn(async move { planted.await.unwrap_or(None) }));
    app.draw(&mut terminal(80, 24)).expect("a frame draws");

    assert!(!app.dirty, "the frame above cleared it");
    assert!(app.wants_wakeup(), "the fetch is what keeps it awake");
}

/// Rows landing under an open modal wait in the cache rather than
/// stealing the keys the modal claimed, and the next `/model` opens
/// instantly from what was kept.
#[tokio::test]
async fn a_listing_that_lands_under_a_modal_waits_in_the_cache() {
    let mut app = app().with_provider("cursor");
    app.help = Some(Help::new(app.keys.clone()));
    app.wire_fetch = Some(tokio::spawn(async {
        Some(Ok(served(vec![listed("planted-one", "Planted One")])))
    }));

    reap_wire_fetch(&mut app).await;
    assert!(app.chooser.is_none(), "the modal keeps its keys");
    assert!(app.wire_models.is_some(), "but the rows are kept");

    app.help = None;
    app.open_models();
    assert!(
        app.chooser.is_some(),
        "the next `/model` opens instantly from the cache"
    );
}

/// An empty roster is an answer, and an empty dialog is not a way to
/// show it.
#[tokio::test]
async fn a_wire_that_serves_no_models_says_so_instead_of_opening_an_empty_dialog() {
    let mut app = app().with_provider("cursor");
    app.wire_fetch = Some(tokio::spawn(async { Some(Ok(served(Vec::new()))) }));

    reap_wire_fetch(&mut app).await;
    assert!(app.chooser.is_none(), "no dialog opens over nothing");
    assert!(
        status_line(&mut app).contains("the cursor wire served no models"),
        "got: {}",
        status_line(&mut app)
    );
}

/// A failure is a status line, never a dialog — and the cleared slot is
/// what makes the next `/model` a retry rather than a dead end.
#[tokio::test]
async fn a_failed_wire_listing_lands_in_the_status_bar_and_clears_the_slot_for_a_retry() {
    let mut app = app().with_provider("cursor");
    app.wire_fetch = Some(tokio::spawn(async {
        Some(Err(ganja_core::provider::ProviderError::Auth(
            "no cursor credential is stored; run `ganja auth login cursor`".to_owned(),
        )))
    }));

    reap_wire_fetch(&mut app).await;
    assert!(app.chooser.is_none(), "a failure opens nothing");
    assert!(app.wire_models.is_none(), "and caches nothing");
    assert!(
        status_line(&mut app).contains("ganja auth login cursor"),
        "the wire's own words reach the status bar: {}",
        status_line(&mut app)
    );
}

#[tokio::test]
async fn choosing_a_model_switches_the_session_to_it() {
    let mut app = app().with_provider("anthropic");
    app.open_models();

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.chooser.is_none(), "a switch that took closes the list");
    assert_eq!(
        app.model,
        app.engine.model(),
        "the frontend prices what the engine will ask for"
    );
    assert_ne!(app.model, fake::MODEL, "and it is no longer the old model");
}

/// A switch mid-turn is exactly what the engine refuses, and a refusal has
/// one place to be seen.
#[tokio::test]
async fn a_refused_model_switch_reaches_the_status_bar_and_leaves_the_list_up() {
    let (mut app, mut events) = wired().await;
    app = app.with_provider("anthropic");
    for event in typing("a turn to be busy with") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    app.open_models();
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(
        app.chooser.is_some(),
        "a refused switch keeps the list the user was choosing from"
    );
    let mut terminal = terminal(120, 12);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("already streaming"),
        "the refusal should be readable:\n{}",
        screen(&terminal)
    );

    // Drain the turn so the test does not leave one streaming.
    pump(&mut app, &mut events, 1).await;
}

/// Subagents are the task tool's to spawn; a picker offering them would be
/// offering a switch the engine refuses.
#[tokio::test]
async fn the_agent_list_holds_only_the_agents_a_user_may_switch_to() {
    let mut app = agentic_app();
    app.open_agents();

    let (kind, dialog) = app.chooser.as_ref().expect("the list should be open");
    assert_eq!(*kind, Chooser::Agents);

    let mut listed = Vec::new();
    let mut cursor = dialog.clone();
    cursor.move_selection(-99);
    for _ in 0..8 {
        if let Some(value) = cursor.selected() {
            listed.push(value.to_owned());
        }
        cursor.move_selection(1);
    }
    listed.dedup();

    assert_eq!(
        listed,
        vec!["build".to_owned(), "plan".to_owned()],
        "general and explore are subagents"
    );
}

#[tokio::test]
async fn choosing_an_agent_switches_the_session_and_the_status_bar_says_so() {
    let mut app = agentic_app();
    assert_eq!(
        app.agent.as_deref(),
        Some("build"),
        "sessions start on build"
    );

    app.open_agents();
    app.handle(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .expect("down is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.agent.as_deref(), Some("plan"));
    assert_eq!(app.engine.agent().as_deref(), Some("plan"));

    let mut terminal = terminal(80, 8);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("plan"),
        "the bar should name the agent:\n{}",
        screen(&terminal)
    );
}

#[tokio::test]
async fn an_agent_change_the_engine_announces_moves_the_indicator() {
    let mut app = app();

    app.handle(AppEvent::core(CoreEvent::AgentChanged {
        session_id: session(),
        agent: "plan".to_owned(),
        model: "provider/planner".to_owned(),
    }))
    .await
    .expect("an agent change is handled");

    assert_eq!(app.agent.as_deref(), Some("plan"));
    assert_eq!(app.model, "provider/planner");

    let mut terminal = terminal(80, 8);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("plan"),
        "the status bar should name the announced agent:\n{}",
        screen(&terminal)
    );
}

#[tokio::test]
async fn tab_on_an_empty_editor_cycles_the_agents_and_wraps() {
    let mut app = agentic_app();
    let mut seen = vec![app.agent.clone()];

    for _ in 0..3 {
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");
        seen.push(app.agent.clone());
    }

    assert_eq!(
        seen,
        vec![
            Some("build".to_owned()),
            Some("plan".to_owned()),
            Some("build".to_owned()),
            Some("plan".to_owned()),
        ],
        "two selectable agents, cycled and wrapped"
    );
}

#[tokio::test]
async fn tab_with_something_typed_reaches_the_editor_instead_of_the_agents() {
    let mut app = agentic_app();
    for event in typing("half a thought") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");

    assert_eq!(
        app.agent.as_deref(),
        Some("build"),
        "a Tab inside a sentence is not a request to change agent"
    );
    assert_ne!(
        app.editor.prompt().as_deref(),
        Some("half a thought"),
        "it should have reached the editor"
    );
}

/// The one key that means two things, and the gate that decides which.
#[tokio::test]
async fn control_d_exits_on_an_empty_editor_and_deletes_forward_otherwise() {
    let mut bare = app();
    bare.handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .await
        .expect("control-d is handled");
    assert!(bare.quit, "an empty editor has nothing to delete");

    let mut typed = app();
    for event in typing("abc") {
        typed.handle(event).await.expect("typing is handled");
    }
    typed
        .handle(key(KeyCode::Home, KeyModifiers::NONE))
        .await
        .expect("home is handled");
    typed
        .handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .await
        .expect("control-d is handled");

    assert!(!typed.quit, "there was something to delete");
    assert_eq!(
        typed.editor.prompt().as_deref(),
        Some("bc"),
        "and it was deleted"
    );
}

/// Ganja's own interrupts, which stay unconditional where Ctrl-D is gated.
#[tokio::test]
async fn control_c_and_control_q_quit_whatever_is_typed() {
    for code in [KeyCode::Char('c'), KeyCode::Char('q')] {
        let mut app = app();
        for event in typing("a draft nobody asked to keep") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(code, KeyModifiers::CONTROL))
            .await
            .expect("a quit key is handled");

        assert!(app.quit, "{code:?} with Control should quit");
    }
}

#[tokio::test]
async fn home_and_end_move_in_the_buffer_while_there_is_one_and_in_the_transcript_otherwise() {
    let mut written = app();
    for event in typing("a line to move around in") {
        written.handle(event).await.expect("typing is handled");
    }

    written
        .handle(key(KeyCode::Home, KeyModifiers::NONE))
        .await
        .expect("home is handled");
    assert_eq!(
        written.editor.cursor(),
        (0, 0),
        "home reached the line's start"
    );

    written
        .handle(key(KeyCode::End, KeyModifiers::NONE))
        .await
        .expect("end is handled");
    assert_eq!(
        written.editor.cursor(),
        (0, "a line to move around in".chars().count()),
        "end reached the line's end"
    );

    // Empty editor: the same keys are the transcript's.
    let mut empty = app();
    empty.seed(stored_transcript(80));
    let mut terminal = terminal(40, 12);
    empty.draw(&mut terminal).expect("a frame draws");

    empty
        .handle(key(KeyCode::Home, KeyModifiers::NONE))
        .await
        .expect("home is handled");
    assert!(
        !empty.chat.is_following_tail(),
        "home should have gone to the oldest message"
    );

    empty
        .handle(key(KeyCode::End, KeyModifiers::NONE))
        .await
        .expect("end is handled");
    assert!(
        empty.chat.is_following_tail(),
        "end should have come back to the newest"
    );
}

#[tokio::test]
async fn a_bare_exit_word_submitted_on_its_own_quits() {
    for word in ["exit", "quit", ":q"] {
        let (mut app, _events) = wired().await;
        for event in typing(word) {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.quit, "{word:?} on its own should quit");
    }
}

#[tokio::test]
async fn an_exit_word_inside_a_sentence_is_a_prompt_like_any_other() {
    let (mut app, mut events) = wired().await;
    for event in typing("does exit mean anything here") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(!app.quit, "the word only quits when it is the whole prompt");
    pump(&mut app, &mut events, 1).await;
}

/// The keys the config file moved are the keys that work; the ones it
/// replaced are not.
#[tokio::test]
async fn a_rebound_key_opens_what_it_was_bound_to_and_the_default_stops_working() {
    let configured: std::collections::BTreeMap<String, String> =
        [("palette_open".to_owned(), "f5".to_owned())].into();
    let keys = crate::keybind::Keybinds::from_config(&configured).expect("a legible binding");
    let mut app = app().with_keybinds(keys);

    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    assert!(
        app.palette.is_none(),
        "the replaced default should be inert"
    );

    app.handle(key(KeyCode::F(5), KeyModifiers::NONE))
        .await
        .expect("f5 is handled");
    assert!(app.palette.is_some(), "and f5 should open it");
}

#[tokio::test]
async fn the_help_card_opens_from_the_palette_and_closes_on_escape() {
    let mut app = app();
    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    for event in typing("help") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert!(app.help.is_some());

    // Every other key is swallowed, like any modal here.
    for event in typing("x") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(app.help.is_some(), "typing should not close it");
    assert!(app.editor.prompt().is_none(), "nor reach the editor");

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.help.is_none());
}

/// Ctrl+T opens the inspector overlay (**F2**, **D453**) and both Esc and
/// Ctrl+T itself close it again — a toggle, not a one-way door.
#[tokio::test]
async fn ctrl_t_opens_the_inspector_and_either_esc_or_ctrl_t_closes_it() {
    let mut app = app();
    assert!(app.inspector.is_none());

    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t is handled");
    assert!(app.inspector.is_some());

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.inspector.is_none(), "escape should close it");

    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t is handled");
    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t is handled again");
    assert!(app.inspector.is_none(), "ctrl+t itself should close it too");
}

/// `q` closes the overlay too — Codex's own binding for its transcript
/// overlay, screenshot-sourced (see `component/inspector.rs`'s module
/// doc) — beside Esc and Ctrl+T rather than instead of them.
/// vim's half-page pair reaches the overlay: with the log tab holding
/// more than a screen, Ctrl+U scrolls it up by half the rows the last
/// frame showed and Ctrl+D brings the tail back.
#[tokio::test]
async fn ctrl_u_and_ctrl_d_scroll_the_inspector_by_half_a_page() {
    let mut app = app();
    let reply = Message::assistant("canned");
    let part = Part::text("");
    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");
    app.handle(AppEvent::core(CoreEvent::PartStarted {
        session_id: session(),
        message_id: reply.id.clone(),
        part: part.clone(),
    }))
    .await
    .expect("a part start is handled");
    for index in 0..30 {
        app.handle(AppEvent::core(CoreEvent::PartDelta {
            session_id: session(),
            message_id: reply.id.clone(),
            part_id: part.id.clone(),
            delta: format!("fragment {index}\n"),
        }))
        .await
        .expect("a fragment is handled");
    }

    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t is handled");
    app.handle(key(KeyCode::Char('2'), KeyModifiers::NONE))
        .await
        .expect("the log tab is selected");
    // Wide enough that a `PartDelta`'s `{event:?}` line reaches its
    // `delta` before the clip; eight rows less the chrome is five of
    // content, so a half page is two.
    let mut terminal = terminal(220, 8);
    app.draw(&mut terminal).expect("a frame draws");
    let pinned = screen(&terminal);
    assert!(
        pinned.contains("fragment 29"),
        "the log tab opens on its tail:\n{pinned}"
    );

    app.handle(key(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+u is handled");
    app.draw(&mut terminal).expect("a frame draws");
    let up = screen(&terminal);
    assert!(
        !up.contains("fragment 29") && up.contains("fragment 27"),
        "ctrl+u moved half of the five content rows up:\n{up}"
    );

    app.handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+d is handled");
    app.draw(&mut terminal).expect("a frame draws");
    assert_eq!(screen(&terminal), pinned, "ctrl+d brought the tail back");
    assert!(
        !app.quit,
        "inside the overlay ctrl+d is vim's, not the exit chord it is elsewhere"
    );
}

#[tokio::test]
async fn q_closes_the_inspector_too() {
    let mut app = app();

    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t is handled");
    assert!(app.inspector.is_some());

    app.handle(key(KeyCode::Char('q'), KeyModifiers::NONE))
        .await
        .expect("q is handled");
    assert!(app.inspector.is_none(), "q should close it");
}

/// Left/Right cycle the tabs and the digit keys jump straight to one, all
/// reflected in what actually renders rather than just in which key was
/// pressed.
#[tokio::test]
async fn digit_keys_and_arrows_switch_the_inspectors_tab() {
    let mut app = app();
    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t opens the overlay");

    let mut terminal = terminal(120, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("no session yet"),
        "the overlay opens on the transcript tab:\n{}",
        screen(&terminal)
    );

    app.handle(key(KeyCode::Char('2'), KeyModifiers::NONE))
        .await
        .expect("2 jumps to the log tab");
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("no events yet"),
        "got:\n{}",
        screen(&terminal)
    );

    app.handle(key(KeyCode::Left, KeyModifiers::NONE))
        .await
        .expect("left cycles back a tab");
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("no session yet"),
        "left from the log tab should land back on the transcript tab:\n{}",
        screen(&terminal)
    );
}

/// A view, not a mode (**F2**): opening the overlay does not pause a
/// streaming turn, and the transcript underneath keeps growing exactly as
/// it would with nothing open.
#[tokio::test]
async fn the_inspector_does_not_pause_a_streaming_turn() {
    let mut app = app();
    let reply = Message::assistant("canned");
    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("the reply starts");
    assert!(app.status.is_streaming());

    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t opens the overlay");

    let part = Part::text("");
    app.handle(AppEvent::core(CoreEvent::PartStarted {
        session_id: session(),
        message_id: reply.id.clone(),
        part: part.clone(),
    }))
    .await
    .expect("a part starts");
    app.handle(AppEvent::core(CoreEvent::PartDelta {
        session_id: session(),
        message_id: reply.id.clone(),
        part_id: part.id.clone(),
        delta: "still streaming".to_owned(),
    }))
    .await
    .expect("a fragment is handled");

    assert!(
        app.status.is_streaming(),
        "the overlay must not pause the turn"
    );
    let grew = app.chat.messages().iter().any(|(_, parts)| {
        parts
            .iter()
            .any(|part| part.as_text() == Some("still streaming"))
    });
    assert!(
        grew,
        "the transcript should keep growing while the overlay is open"
    );
}

/// Opened the way a user opens it, over a real transcript — this is what
/// pins the overlay's banner, its tab strip and its footer, full-terminal
/// (screenshot-sourced, see `component/inspector.rs`'s module doc) rather
/// than boxed inside the three-pane frame the way it used to be.
#[tokio::test]
async fn snapshot_inspector_dialog_open() {
    let mut app = app();
    palette_transcript(&mut app);

    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t opens the overlay");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// Full-terminal takeover means what it says: the composer and the
/// status bar — both still drawn beneath every other dialog — disappear
/// under the inspector while it is open, and come back the moment it
/// closes (screenshot-sourced, see `component/inspector.rs`'s module
/// doc).
#[tokio::test]
async fn the_inspector_covers_the_composer_and_the_status_bar() {
    let mut app = app();
    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let closed = screen(&terminal);
    assert!(closed.contains("Ask ganja something"), "{closed}");
    // The idle footer carries no key reminders, so the state label is what
    // says the status bar is drawn at all.
    assert!(closed.contains("ready"), "{closed}");

    app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .expect("ctrl+t opens the overlay");
    app.draw(&mut terminal).expect("a frame draws");
    let open = screen(&terminal);
    assert!(
        !open.contains("Ask ganja something"),
        "the composer should be covered while the overlay is open:\n{open}"
    );
    assert!(
        !open.contains("ready"),
        "the status bar should be covered too:\n{open}"
    );

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape closes the overlay");
    app.draw(&mut terminal).expect("a frame draws");
    let reclosed = screen(&terminal);
    assert!(
        reclosed.contains("Ask ganja something"),
        "closing the overlay should bring the composer back:\n{reclosed}"
    );
}

#[tokio::test]
async fn the_wheel_does_not_reach_the_transcript_while_the_palette_is_open() {
    let mut app = app();
    app.seed(stored_transcript(80));
    let mut terminal = terminal(40, 12);
    app.draw(&mut terminal).expect("a frame draws");

    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })))
    .await
    .expect("the wheel is handled");

    assert!(
        app.chat.is_following_tail(),
        "a modal claims the wheel as well as the keys"
    );
}

#[tokio::test]
async fn snapshot_palette_open() {
    let mut app = app();
    palette_transcript(&mut app);

    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");

    assert!(
        app.palette.is_some(),
        "the palette must be open, or the snapshot is of a bare screen"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_palette_filtered() {
    let mut app = app();
    palette_transcript(&mut app);

    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    for event in typing("s") {
        app.handle(event).await.expect("typing is handled");
    }

    assert_eq!(
        app.palette.as_ref().map(Palette::filter),
        Some("s"),
        "the fragment must have landed, or the snapshot is of an unfiltered list"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// The selected row is filled rather than tinted, which is the one part of
/// the palette a symbol-only dump cannot pin.
#[tokio::test]
async fn snapshot_palette_selection_styling() {
    let mut app = themed_app("tokyonight");
    palette_transcript(&mut app);

    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    app.handle(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .expect("down is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(styled_screen(&terminal));
}

#[tokio::test]
async fn snapshot_command_menu_open() {
    let mut app = app();
    palette_transcript(&mut app);

    for event in typing("/s") {
        app.handle(event).await.expect("typing is handled");
    }

    assert!(
        app.dropdown.is_some(),
        "the menu must be open, or the snapshot is of a bare screen"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_agents_dialog_open() {
    let mut app = agentic_app();
    palette_transcript(&mut app);
    app.open_agents();

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_help_dialog_open() {
    let mut app = app();
    palette_transcript(&mut app);
    app.run_command(crate::command::Action::Help).await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// A project on disk for the `@` menu to walk, with one file in a
/// subdirectory so a mention has a path to complete rather than a name.
fn project() -> TempDir {
    let directory = temporary();
    std::fs::create_dir_all(directory.path().join("src")).expect("the fixture tree is made");
    for path in ["README.md", "src/lib.rs", "src/app.rs"] {
        std::fs::write(directory.path().join(path), "// a file worth mentioning\n")
            .expect("the fixture file writes");
    }

    directory
}

/// A skills root holding two named skills, for the `$` menu to offer.
fn skill_root() -> TempDir {
    let directory = temporary();
    for (name, description) in [("porting", "How to port a module."), ("tdd", "Red, green.")] {
        let dir = directory.path().join(name);
        std::fs::create_dir_all(&dir).expect("the fixture tree is made");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
        )
        .expect("the fixture file writes");
    }

    directory
}

/// An app whose engine holds `root` as its skill roots — the same seam
/// the real assembly wires (`with_skill_roots`).
fn app_with_skills(root: &TempDir) -> App {
    let app = app();
    app.engine.replace_skill_roots(
        ganja_tool::skill::Roots::none().with_paths([root.path().to_path_buf()]),
    );

    app
}

#[tokio::test]
async fn a_dollar_raises_the_skill_menu_and_the_fragment_narrows_it() {
    let root = skill_root();
    let mut app = app_with_skills(&root);

    for event in typing("use $port") {
        app.handle(event).await.expect("typing is handled");
    }

    assert!(app.skill_menu.is_some(), "the skill menu should be open");
    assert_eq!(
        app.skill_menu.as_ref().and_then(|menu| menu.selected()),
        Some("porting"),
        "the fragment narrowed the list to the matching name"
    );
}

/// The exact trigger, at the level a person meets it: prose dollars are
/// prose, and only a token a skill answers to keeps a menu up.
#[tokio::test]
async fn prose_dollars_raise_no_skill_menu() {
    let cases = [
        ("costs $5 each", false),
        ("$ cargo build", false),
        ("mail me$porting", false),
        ("$port", true),
        ("$", true),
    ];

    for (text, expected) in cases {
        let root = skill_root();
        let mut app = app_with_skills(&root);
        for event in typing(text) {
            app.handle(event).await.expect("typing is handled");
        }

        assert_eq!(
            app.skill_menu.is_some(),
            expected,
            "{text:?} should {}have raised the menu",
            if expected { "" } else { "not " }
        );
    }
}

#[tokio::test]
async fn tab_completes_the_invocation_without_submitting() {
    let root = skill_root();
    let mut app = app_with_skills(&root);
    for event in typing("use $port") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");

    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("use $porting "),
        "the fragment became the whole name, with room after it"
    );
    assert!(app.skill_menu.is_none(), "completing closes the menu");
    assert_eq!(
        app.editor.cursor(),
        (0, "use $porting ".chars().count()),
        "the cursor follows what was inserted"
    );
}

#[tokio::test]
async fn enter_on_the_skill_menu_completes_the_same_as_tab() {
    let root = skill_root();
    let mut app = app_with_skills(&root);
    for event in typing("$td") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("$tdd "),
        "enter completes rather than submitting"
    );
    assert!(app.skill_menu.is_none());
}

/// Escape closes the menu and keeps the text (**D11**), like the other
/// two inline menus.
#[tokio::test]
async fn esc_closes_the_skill_menu_and_keeps_the_text() {
    let root = skill_root();
    let mut app = app_with_skills(&root);
    for event in typing("$port") {
        app.handle(event).await.expect("typing is handled");
    }
    assert!(app.skill_menu.is_some());

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("esc is handled");

    assert!(app.skill_menu.is_none());
    assert_eq!(app.editor.prompt().as_deref(), Some("$port"));
}

/// The submit half: the names a buffer's tokens invoke are exactly what
/// rides `Command::SendPrompt::skills`, validated against the same
/// discovery the menu offered from.
#[tokio::test]
async fn requested_skills_reads_the_tokens_a_submit_would_send() {
    let root = skill_root();
    let app = app_with_skills(&root);

    assert_eq!(
        app.requested_skills("use $porting then $tdd, not $PATH or $5"),
        vec!["porting".to_owned(), "tdd".to_owned()]
    );
    assert!(app.requested_skills("nothing invoked").is_empty());
}

/// The `/skills` dialog is a listing with an insertion, not a switch:
/// Enter puts `$name ` at the cursor and the session changes nothing.
#[tokio::test]
async fn the_skills_dialog_lists_and_enter_inserts_the_token() {
    let root = skill_root();
    let mut app = app_with_skills(&root);
    app.run_command(command::Action::Skills).await;

    let (kind, dialog) = app.chooser.as_ref().expect("the dialog is open");
    assert!(matches!(kind, Chooser::Skills));
    assert_eq!(dialog.selected(), Some("porting"));

    // The row names the description; the origin root rides beside it but
    // is a temporary path too wide for this terminal, so the sentence is
    // the part a screen this size proves.
    let mut terminal = terminal(120, 20);
    app.draw(&mut terminal).expect("a frame draws");
    let drawn = screen(&terminal);
    assert!(
        drawn.contains("How to port a module."),
        "the dialog shows the skill's sentence: {drawn}"
    );

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.editor.prompt().as_deref(), Some("$porting "));
    assert!(app.chooser.is_none(), "choosing closes the dialog");
}

#[tokio::test]
async fn snapshot_skill_menu() {
    let root = skill_root();
    let mut app = app_with_skills(&root);
    for event in typing("use $") {
        app.handle(event).await.expect("typing is handled");
    }

    let mut terminal = terminal(80, 16);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));
}

/// An app whose `@` menu walks `directory`.
fn app_in(directory: &TempDir) -> App {
    app().with_cwd(directory.path())
}

/// The file paths the `@` menu is currently offering — roster and
/// live-session rows, which callers pinned before D529 landed never
/// asked about, are skipped here rather than changing what those tests
/// assert.
fn offered(app: &App) -> Vec<String> {
    let mut listed = Vec::new();
    let Some(files) = &app.files else {
        return listed;
    };
    let mut cursor = files.clone();
    cursor.move_selection(-99);
    for _ in 0..16 {
        if let Some(MenuRow::File(path)) = cursor.selected() {
            listed.push(path.clone());
        }
        cursor.move_selection(1);
    }
    listed.dedup();

    listed
}

/// Types `text` into `app`, one key at a time, the way a person does —
/// then settles the `@` menu's background walk, which a real run reaps
/// on its next ticks (2026-08-15: the walk left the keystroke's own
/// handling).
async fn typed(app: &mut App, text: &str) {
    for event in typing(text) {
        app.handle(event).await.expect("typing is handled");
    }
    settle_file_menu(app).await;
}

/// Waits for an in-flight `@` walk to finish and installs it, standing in
/// for the tick loop.
async fn settle_file_menu(app: &mut App) {
    for _ in 0..500 {
        if app.file_walk.is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        app.poll_file_walk().await;
    }
    panic!("the file walk never settled");
}

#[tokio::test]
async fn an_at_raises_the_file_menu_over_what_the_project_holds() {
    let directory = project();
    let mut app = app_in(&directory);

    typed(&mut app, "look at @lib").await;

    assert!(app.files.is_some(), "the file menu should be open");
    assert_eq!(offered(&app), vec!["src/lib.rs".to_owned()]);
}

/// A fragment naming directories anchors on them, which is what makes a
/// path typed from the root reach what is under it.
#[tokio::test]
async fn a_fragment_naming_a_directory_offers_what_is_inside_it() {
    let directory = project();
    let mut app = app_in(&directory);

    typed(&mut app, "@src/").await;

    let mut offered = offered(&app);
    offered.sort();
    assert_eq!(
        offered,
        vec!["src/app.rs".to_owned(), "src/lib.rs".to_owned()]
    );
}

/// The exact trigger, at the level a person meets it. The shapes
/// themselves are pinned in `mention`; what this covers is that the app
/// asks the same question the scan will ask on submit.
#[tokio::test]
async fn the_file_menu_opens_on_exactly_the_mentions_a_submit_would_read() {
    let cases = [
        // Typed, and whether the menu should be up at the end of it.
        ("@lib", true),
        ("look at @lib", true),
        ("mail me@example.com", false),
        ("@lib and then", false),
        ("nothing at all", false),
        ("@", true),
    ];

    for (text, expected) in cases {
        let directory = project();
        let mut app = app_in(&directory);
        typed(&mut app, text).await;

        assert_eq!(
            app.files.is_some(),
            expected,
            "{text:?} should {}have raised the menu",
            if expected { "" } else { "not " }
        );
    }
}

/// Moving the cursor back out of a mention closes the menu, and moving it
/// back in opens it again — the trigger is about where the cursor is, not
/// about what was typed.
#[tokio::test]
async fn moving_the_cursor_out_of_a_mention_closes_the_menu() {
    let directory = project();
    let mut app = app_in(&directory);
    typed(&mut app, "@lib").await;
    assert!(app.files.is_some());

    app.handle(key(KeyCode::Home, KeyModifiers::NONE))
        .await
        .expect("home is handled");

    assert!(app.files.is_none(), "the cursor is in front of the `@` now");
}

#[tokio::test]
async fn choosing_a_file_writes_the_mention_into_the_buffer() {
    let directory = project();
    let mut app = app_in(&directory);
    typed(&mut app, "compare @lib").await;

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("compare @src/lib.rs "),
        "the fragment should have become the whole path, with room after it"
    );
    assert!(app.files.is_none(), "choosing closes the menu");
    assert_eq!(
        app.editor.cursor(),
        (0, "compare @src/lib.rs ".chars().count()),
        "the cursor follows what was inserted"
    );
}

/// **F1**: Tab reaches the identical outcome as Enter in the file menu —
/// upstream's own binding falls through to the same selection unless the
/// row is a directory, and this build's `@` walker never yields one
/// (`glob.rs` filters to `is_file()`).
#[tokio::test]
async fn tab_on_the_file_menu_completes_the_mention_the_same_as_enter() {
    let directory = project();
    let mut app = app_in(&directory);
    typed(&mut app, "compare @lib").await;

    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");

    assert_eq!(app.editor.prompt().as_deref(), Some("compare @src/lib.rs "),);
    assert!(app.files.is_none(), "choosing closes the menu");
}

/// A mention in the middle of a sentence is replaced in place, and the
/// cursor stays where the user was writing rather than jumping to the end.
#[tokio::test]
async fn a_mention_mid_sentence_is_replaced_without_moving_the_rest() {
    let directory = project();
    let mut app = app_in(&directory);
    typed(&mut app, "look at @lib and say why").await;
    // Back into the mention: nine characters from the end.
    for _ in 0.."and say why".chars().count() + 1 {
        app.handle(key(KeyCode::Left, KeyModifiers::NONE))
            .await
            .expect("left is handled");
    }
    settle_file_menu(&mut app).await;
    assert!(
        app.files.is_some(),
        "the cursor is inside the mention again"
    );

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("look at @src/lib.rs and say why"),
        "the space already after the mention is not doubled"
    );
    assert_eq!(
        app.editor.cursor(),
        (0, "look at @src/lib.rs".chars().count())
    );
}

/// **Non-vacuity target for the submit scan.** Dropping `mention::scan`
/// from `submit` — sending `Vec::new()` the way the composer did before
/// mentions existed — fails this test on the `File` part.
///
/// The fixture project is what both mentions name, because the scan is now
/// filtered by whether the file is there (**D113**): before that, this
/// test read whichever directory the runner happened to start in.
#[tokio::test]
async fn a_submitted_prompt_carries_its_mentions_and_keeps_their_text() {
    let directory = project();
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin())
        .with_cwd(directory.path())
        .with_root(directory.path());

    typed(&mut app, "compare @src/lib.rs with @README.md").await;
    // The menu is still up over the mention the cursor is in, and it owns
    // Enter — so the way to send this is the way a person sends it.
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let CoreEvent::MessageStarted {
        session_id: _,
        message,
    } = events.next().await.expect("the engine reports the prompt")
    else {
        panic!("the first event of a turn is the user's message");
    };

    let attached: Vec<&str> = message
        .parts
        .iter()
        .filter_map(|part| match &part.body {
            PartBody::File { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        attached,
        vec!["src/lib.rs", "README.md"],
        "both mentions should have reached the engine"
    );

    let text: String = message
        .parts
        .iter()
        .filter_map(ganja_protocol::Part::as_text)
        .collect();
    assert_eq!(
        text, "compare @src/lib.rs with @README.md",
        "the literal tokens stay in the prompt, as upstream leaves them"
    );
}

/// The range rides the fragment while it is typed, and the walk sees only
/// the path half — so the menu keeps offering the file the range is for.
#[tokio::test]
async fn a_range_being_typed_does_not_narrow_the_file_menu() {
    let directory = project();
    let mut app = app_in(&directory);

    typed(&mut app, "@lib#10-20").await;

    assert_eq!(offered(&app), vec!["src/lib.rs".to_owned()]);
}

/// Choosing a file keeps the typed range, normalized: upstream re-appends
/// `#start` or `#start-end` to the chosen path (`autocomplete.tsx:250`),
/// dropping an empty end and a reversed one.
#[tokio::test]
async fn choosing_a_file_keeps_the_normalized_typed_range() {
    let cases = [
        ("compare @lib#10-20", "compare @src/lib.rs#10-20 "),
        ("compare @lib#5-", "compare @src/lib.rs#5 "),
        ("compare @lib#20-10", "compare @src/lib.rs#20 "),
    ];

    for (text, expected) in cases {
        let directory = project();
        let mut app = app_in(&directory);
        typed(&mut app, text).await;

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.editor.prompt().as_deref(), Some(expected), "{text:?}");
    }
}

/// The submit-time half of graceful degradation: a binary mention the
/// selected wire cannot carry is named in the status line before the
/// turn, so the engine-side text block is never the first the user hears
/// of it.
#[tokio::test]
async fn submitting_a_binary_mention_the_wire_refuses_warns_in_the_status_line() {
    let directory = project();
    fs::write(directory.path().join("shot.png"), b"png-bytes").expect("the fixture writes");

    let (provider, _requests) = ganja_testkit::ScriptedProvider::text_only(Vec::new());
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
    );
    let mut app = App::new(engine, None, Themes::builtin())
        .with_cwd(directory.path())
        .with_root(directory.path());

    typed(&mut app, "look at @shot.png").await;
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let bar = status_line(&mut app);
    assert!(
        bar.contains("@shot.png (image/png)"),
        "the warning names the file and its mime: {bar}"
    );
    assert!(
        bar.contains("does not carry"),
        "and says why the bytes will not travel: {bar}"
    );
}

/// The other half: a wire that carries the mime warns about nothing.
#[tokio::test]
async fn submitting_a_binary_mention_the_wire_carries_warns_about_nothing() {
    let directory = project();
    fs::write(directory.path().join("shot.png"), b"png-bytes").expect("the fixture writes");

    let mut app = App::new(engine(), None, Themes::builtin())
        .with_cwd(directory.path())
        .with_root(directory.path());

    typed(&mut app, "look at @shot.png").await;
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let bar = status_line(&mut app);
    assert!(
        !bar.contains("attached by name only"),
        "the fake carries every mime, so there is nothing to warn about: {bar}"
    );
}

/// **D11** again, on the other menu: closing is not deleting.
#[tokio::test]
async fn escape_closes_the_file_menu_and_keeps_what_was_typed() {
    let directory = project();
    let mut app = app_in(&directory);
    typed(&mut app, "look at @lib").await;

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    assert!(app.files.is_none());
    assert_eq!(app.editor.prompt().as_deref(), Some("look at @lib"));
}

/// **Non-vacuity target for the `!` consume.** Letting the keystroke fall
/// through to the editor — dropping the flip arm — leaves a `!` in the
/// buffer and fails the second assertion here.
#[tokio::test]
async fn an_exclamation_at_the_start_flips_to_shell_mode_and_is_never_typed() {
    let mut app = app();

    typed(&mut app, "!").await;

    assert_eq!(app.editor.mode(), Mode::Shell);
    assert!(
        app.editor.is_empty(),
        "the `!` is the mode switch, not a character: {:?}",
        app.editor.text()
    );
}

#[tokio::test]
async fn an_exclamation_anywhere_else_is_a_character_like_any_other() {
    let mut app = app();

    typed(&mut app, "wow!").await;

    assert_eq!(app.editor.mode(), Mode::Prompt);
    assert_eq!(app.editor.prompt().as_deref(), Some("wow!"));
}

/// Upstream's gate is the cursor, not the buffer: a `!` typed in front of
/// existing text flips the mode and keeps the text as the command.
#[tokio::test]
async fn an_exclamation_in_front_of_existing_text_still_flips() {
    let mut app = app();
    typed(&mut app, "ls -la").await;
    app.handle(key(KeyCode::Home, KeyModifiers::NONE))
        .await
        .expect("home is handled");

    typed(&mut app, "!").await;

    assert_eq!(app.editor.mode(), Mode::Shell);
    assert_eq!(app.editor.prompt().as_deref(), Some("ls -la"));
}

#[tokio::test]
async fn escape_and_backspace_at_the_start_are_the_two_ways_out_of_shell_mode() {
    for leaving in [KeyCode::Esc, KeyCode::Backspace] {
        let mut app = app();
        typed(&mut app, "!").await;
        typed(&mut app, "ls").await;
        app.handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");

        app.handle(key(leaving, KeyModifiers::NONE))
            .await
            .expect("the way out is handled");

        assert_eq!(app.editor.mode(), Mode::Prompt, "{leaving:?}");
        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("ls"),
            "{leaving:?} should leave the mode, not eat the command"
        );
    }
}

/// Backspace anywhere but the front is still backspace.
#[tokio::test]
async fn backspace_inside_a_shell_command_deletes_rather_than_leaving() {
    let mut app = app();
    typed(&mut app, "!").await;
    typed(&mut app, "lsx").await;

    app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
        .await
        .expect("backspace is handled");

    assert_eq!(app.editor.mode(), Mode::Shell);
    assert_eq!(app.editor.prompt().as_deref(), Some("ls"));
}

/// The whole passthrough, through the real engine: the command runs, the
/// synthetic user message upstream writes lands in the transcript, and the
/// composer comes back for the next prompt.
#[tokio::test]
async fn submitting_in_shell_mode_runs_the_command_and_comes_back() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "!").await;
    typed(&mut app, "echo hello").await;

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(
        app.editor.mode(),
        Mode::Prompt,
        "a command that was accepted leaves the composer ready for a prompt"
    );
    assert!(app.editor.is_empty());

    let CoreEvent::MessageStarted {
        session_id: _,
        message,
    } = events.next().await.expect("the engine reports the command")
    else {
        panic!("the first event of a passthrough is the synthetic user message");
    };
    let text: String = message
        .parts
        .iter()
        .filter_map(ganja_protocol::Part::as_text)
        .collect();
    assert_eq!(text, "The following tool was executed by the user");

    // Drain the rest so the test does not leave a turn streaming.
    while let Some(event) = events.next().await {
        if matches!(event, CoreEvent::MessageFinished { .. }) {
            break;
        }
    }
}

/// A refusal has to leave both halves alone: the text so the command can
/// be tried again, and the mode so trying it again does not send it to the
/// model instead.
#[tokio::test]
async fn a_refused_shell_command_keeps_the_text_and_the_mode() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "a turn to be busy with").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    pump(&mut app, &mut events, 2).await;

    typed(&mut app, "!").await;
    typed(&mut app, "echo hello").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.editor.mode(), Mode::Shell);
    assert_eq!(app.editor.prompt().as_deref(), Some("echo hello"));

    let mut terminal = terminal(120, 12);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("already streaming"),
        "the refusal should be readable:\n{}",
        screen(&terminal)
    );
}

/// Neither menu belongs in shell mode: a `/` there starts a path and an
/// `@` is whatever the shell makes of it.
#[tokio::test]
async fn shell_mode_offers_neither_commands_nor_files() {
    let directory = project();
    let mut app = app_in(&directory);
    typed(&mut app, "!").await;

    typed(&mut app, "/usr").await;
    assert!(app.dropdown.is_none());

    app.editor.clear();
    typed(&mut app, "cat @lib").await;
    assert!(app.files.is_none());
}

/// An engine command expects arguments, so choosing it types its name and
/// waits rather than running something with none.
#[tokio::test]
async fn choosing_an_engine_command_types_its_name_instead_of_running_it() {
    let (mut app, _events) = wired().await;
    typed(&mut app, "/init").await;

    assert_eq!(
        app.dropdown.as_ref().and_then(Dropdown::selected),
        Some(crate::command::Choice::Engine(
            crate::command::EngineCommand {
                name: "init".to_owned(),
                description: Some("guided AGENTS.md setup".to_owned()),
                hint: None,
            }
        )),
        "the engine's own command should be under the cursor"
    );

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(
        app.editor.text(),
        "/init ",
        "the name is typed, with room for the arguments it takes"
    );
    assert!(app.dropdown.is_none());
}

/// Tab reaches the identical outcome as Enter for an engine command:
/// Tab's own "complete without running" only changes anything for the UI
/// half of the roster, which already types-and-waits on Enter.
#[tokio::test]
async fn tab_on_an_engine_command_types_its_name_the_same_as_enter() {
    let (mut app, _events) = wired().await;
    typed(&mut app, "/init").await;

    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");

    assert_eq!(app.editor.text(), "/init ");
    assert!(app.dropdown.is_none());
}

/// And the second Enter runs it, arguments and all.
#[tokio::test]
async fn submitting_an_engine_command_runs_it_with_what_was_typed_after_it() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin());

    typed(&mut app, "/init focus on the test suite").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let CoreEvent::MessageStarted {
        session_id: _,
        message,
    } = events.next().await.expect("the engine reports the prompt")
    else {
        panic!("the first event of a turn is the user's message");
    };
    let text: String = message
        .parts
        .iter()
        .filter_map(ganja_protocol::Part::as_text)
        .collect();

    assert!(
        text.contains("AGENTS.md"),
        "the template should have been expanded, got: {text}"
    );
    assert!(
        text.contains("focus on the test suite"),
        "and the arguments should be in it, got: {text}"
    );
    assert!(
        app.editor.is_empty(),
        "a command that ran clears the composer"
    );
}

/// A slash this build does not know is not a command, so it is text. The
/// engine has its own answer for an unknown command on the wire; the UI
/// simply does not intercept what it cannot name.
#[tokio::test]
async fn an_unknown_slash_command_is_sent_as_the_text_it_is() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin());

    // Trailing space, so the menu is closed and Enter reaches the submit.
    typed(&mut app, "/nonesuch please ").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let CoreEvent::MessageStarted {
        session_id: _,
        message,
    } = events.next().await.expect("the engine reports the prompt")
    else {
        panic!("the first event of a turn is the user's message");
    };
    let text: String = message
        .parts
        .iter()
        .filter_map(ganja_protocol::Part::as_text)
        .collect();

    assert_eq!(text, "/nonesuch please ");
}

/// The dropdown is not the only door to a built-in: the menu closes the
/// moment a space follows the name, so the Enter that follows must read
/// the text itself — Claude Code and Codex both dispatch on the submitted
/// line, not on the menu's state.
#[tokio::test]
async fn a_ui_command_with_a_trailing_space_still_runs_on_enter() {
    let (mut app, _events) = wired().await;

    typed(&mut app, "/exit ").await;
    assert!(app.dropdown.is_none(), "the space closed the menu");

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.quit, "the command should have run, not been sent");
    assert!(
        app.editor.is_empty(),
        "a command that ran clears the composer"
    );
}

/// The sharpest spelling of the same edge: Tab fills `/exit ` without
/// running it (**D446**), so the Enter that follows has to mean the
/// command that was just completed.
#[tokio::test]
async fn tab_completion_then_enter_runs_the_command_it_completed() {
    let (mut app, _events) = wired().await;
    typed(&mut app, "/exi").await;

    app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .expect("tab is handled");
    assert_eq!(app.editor.text(), "/exit ");

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.quit, "tab completed the command and enter ran it");
}

/// A built-in takes no arguments, so a name followed by more words is
/// prose — the same ruling that sends an unknown slash command to the
/// model rather than intercepting it.
#[tokio::test]
async fn a_ui_command_followed_by_arguments_is_sent_as_the_text_it_is() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin());

    typed(&mut app, "/exit now ").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(!app.quit, "words after the name make it prose");
    let CoreEvent::MessageStarted {
        session_id: _,
        message,
    } = events.next().await.expect("the engine reports the prompt")
    else {
        panic!("the first event of a turn is the user's message");
    };
    let text: String = message
        .parts
        .iter()
        .filter_map(ganja_protocol::Part::as_text)
        .collect();

    assert_eq!(text, "/exit now ");
}

/// A UI command acts on the frontend, not on the running turn, so it runs
/// now rather than steering its own name into the conversation — the
/// palette already dispatches any of these mid-turn.
#[tokio::test]
async fn a_ui_command_submitted_mid_turn_runs_instead_of_steering() {
    let (mut app, _events) = wired().await;
    app.turn_running = true;

    typed(&mut app, "/exit ").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.quit, "the command should have run instead of queueing");
}

#[tokio::test]
async fn new_session_empties_the_screen_the_old_one_filled() {
    let mut app = app();
    palette_transcript(&mut app);
    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(screen(&terminal).contains("show me every color"));

    app.run_command(crate::command::Action::New).await;

    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        !screen(&terminal).contains("show me every color"),
        "the previous conversation should be off the screen:\n{}",
        screen(&terminal)
    );
}

/// A refused reset leaves the user looking at the conversation they are
/// still in, rather than at a blank screen the engine never agreed to.
#[tokio::test]
async fn a_refused_new_session_leaves_the_transcript_alone() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "a turn to be busy with").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    pump(&mut app, &mut events, 2).await;

    app.run_command(crate::command::Action::New).await;

    let mut terminal = terminal(120, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("a turn to be busy with"), "got:\n{screen}");
    assert!(screen.contains("already streaming"), "got:\n{screen}");
}

/// A compaction is a turn like any other from out here, so what proves the
/// command reached the engine is that the engine ran one. This session has
/// nothing to summarize yet, which is why the turn it runs is one that
/// simply ends.
#[tokio::test]
async fn compact_reaches_the_engine_as_a_turn_of_its_own() {
    let (mut app, mut events) = wired().await;

    app.run_command(crate::command::Action::Compact).await;

    let event = events.next().await.expect("the engine runs the turn");
    assert!(
        matches!(event, CoreEvent::MessageFinished { .. }),
        "got {event:?}"
    );
    assert!(!app.status.is_streaming());
}

/// Both of the commands the palette gained reach something.
#[tokio::test]
async fn the_palette_reaches_the_commands_this_wave_added() {
    // `/new` empties the screen the old conversation filled.
    let (mut app, _events) = wired().await;
    palette_transcript(&mut app);
    app.draw(&mut terminal(80, 24)).expect("a frame draws");
    assert_ne!(
        app.chat.line_count(),
        0,
        "there has to be something to clear"
    );

    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    typed(&mut app, "new").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert_eq!(app.chat.line_count(), 0, "/new should have cleared it");

    // `/compact` reaches the engine, which answers with a turn.
    let (mut app, mut events) = wired().await;
    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");
    typed(&mut app, "compact").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(
        events.next().await.is_some(),
        "/compact should have reached the engine"
    );
}

/// A child session belongs to the task call that spawned it, and is
/// rendered on that call's row; offering it here would be offering a
/// resume into the middle of a delegated turn.
#[tokio::test]
async fn the_picker_lists_roots_only() {
    let directory = temporary();
    store_session(
        &directory,
        "0198f2c4-a1b0-7000-8000-000000000014",
        Some("the conversation"),
        1_000,
        0,
        10,
    );
    store_child(
        &directory,
        "0198f2c4-a1b0-7000-8000-000000000015",
        "0198f2c4-a1b0-7000-8000-000000000014",
    );
    let mut app = persistent_app(&directory);

    app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .expect("control-s is handled");

    let listed: Vec<String> = app
        .sessions
        .as_ref()
        .map(|sessions| {
            let mut cursor = sessions.clone();
            let mut seen = Vec::new();
            cursor.move_selection(-99);
            for _ in 0..8 {
                if let Some(info) = cursor.selected() {
                    seen.push(info.id.as_str().to_owned());
                }
                cursor.move_selection(1);
            }
            seen.dedup();
            seen
        })
        .unwrap_or_default();

    assert_eq!(
        listed,
        vec!["0198f2c4-a1b0-7000-8000-000000000014".to_owned()],
        "a delegated turn's session is not one to resume into"
    );

    // And the row it drew is the root's, by the title a person picks by.
    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("the conversation"), "got:\n{screen}");
    assert!(
        !screen.contains("@explore subagent"),
        "the child's own title has no business in the picker:\n{screen}"
    );
}

/// A task tool part, in whichever state, on the message the engine
/// streamed it on.
async fn task_part(app: &mut App, state: ToolState) {
    let reply = Message::assistant("canned");
    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");
    app.handle(AppEvent::core(CoreEvent::PartUpdated {
        session_id: session(),
        message_id: reply.id,
        part: Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "task".to_owned(),
                state,
            },
        },
    }))
    .await
    .expect("a task update is handled");
}

/// The progress the task tool republishes on its own part is the only
/// window a frontend has into a child, so it has to reach the screen.
#[tokio::test]
async fn a_delegated_turns_progress_reaches_the_transcript() {
    let mut app = app();
    task_part(
        &mut app,
        ToolState::Running {
            input: serde_json::json!({
                "description": "find the parser",
                "subagent_type": "explore",
            }),
            metadata: serde_json::json!({"current_tool": "grep parser", "toolcalls": 3}),
            started: 0,
        },
    )
    .await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);

    assert!(
        screen.contains("\u{25cf} Task(agent: \"explore\""),
        "got:\n{screen}"
    );
    assert!(screen.contains("find the parser"), "got:\n{screen}");
    assert!(screen.contains("\u{23bf} grep parser"), "got:\n{screen}");
}

#[tokio::test]
async fn snapshot_file_menu_open() {
    let directory = project();
    let mut app = app_in(&directory);
    palette_transcript(&mut app);

    typed(&mut app, "compare @lib").await;

    assert!(
        app.files.is_some(),
        "the menu must be open, or the snapshot is of a bare screen"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_shell_mode() {
    let mut app = app();
    palette_transcript(&mut app);

    typed(&mut app, "!").await;
    typed(&mut app, "cargo nextest run --workspace").await;

    assert_eq!(app.editor.mode(), Mode::Shell, "the mode must have flipped");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_shell_output_streaming() {
    let mut app = app();
    let reply = Message::assistant("canned");
    app.handle(AppEvent::core(CoreEvent::MessageStarted {
        session_id: session(),
        message: reply.clone(),
    }))
    .await
    .expect("a message start is handled");
    app.handle(AppEvent::core(CoreEvent::PartUpdated {
        session_id: session(),
        message_id: reply.id,
        part: Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "bash".to_owned(),
                state: ToolState::Running {
                    input: serde_json::json!({"command": "cargo nextest run"}),
                    metadata: serde_json::json!({
                        "output": "    Starting 517 tests\n\
                                   PASS [   0.004s] ganja-core permission\n\
                                   PASS [   0.006s] ganja-core storage\n\
                                   PASS [   0.011s] ganja-tui app\n\
                                   PASS [   0.012s] ganja-tui chat\n\
                                   PASS [   0.019s] ganja-cli import"
                    }),
                    started: 0,
                },
            },
        },
    }))
    .await
    .expect("a running update is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_task_running() {
    let mut app = app();
    task_part(
        &mut app,
        ToolState::Running {
            input: serde_json::json!({
                "description": "find every caller of resolve",
                "subagent_type": "explore",
            }),
            metadata: serde_json::json!({"current_tool": "grep resolve", "toolcalls": 4}),
            started: 0,
        },
    )
    .await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_task_completed() {
    let mut app = app();
    task_part(
        &mut app,
        ToolState::Completed {
            input: serde_json::json!({
                "description": "find every caller of resolve",
                "subagent_type": "explore",
            }),
            output: "<task id=\"tsk_1\" state=\"completed\"><task_result>\
                         four callers, all in session.rs</task_result></task>"
                .to_owned(),
            title: "find every caller of resolve".to_owned(),
            metadata: serde_json::json!({
                "session": "ses_child",
                "agent": "explore",
                "model": fake::MODEL,
                "toolcalls": 9,
            }),
            started: 1_000,
            completed: 24_500,
        },
    )
    .await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_permission_dialog_with_directories() {
    let mut app = app();
    app.handle(AppEvent::core(CoreEvent::PermissionRequested {
        session_id: session(),
        id: PermissionId::from("perm_1".to_owned()),
        call_id: "call_1".to_owned(),
        tool: "bash".to_owned(),
        title: "cp report.md /var/www/html".to_owned(),
        args: serde_json::json!({"command": "cp report.md /var/www/html"}),
        directories: vec!["/var/www/html".to_owned(), "/etc/nginx".to_owned()],
    }))
    .await
    .expect("a permission request is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[test]
fn a_mention_fragment_anchors_on_whatever_directories_it_names() {
    let cases = [
        ("", "**/*"),
        ("lib", "**/*lib*"),
        ("src/", "**/src/**"),
        ("src/li", "**/src/**/*li*"),
        ("crates/ganja-tui/src/", "**/crates/ganja-tui/src/**"),
        // A leading slash names no directory to anchor on.
        ("/lib", "**/*lib*"),
        ("/", "**/*"),
    ];

    for (fragment, expected) in cases {
        assert_eq!(super::pattern(fragment), expected, "{fragment:?}");
    }
}

#[test]
fn only_the_paths_under_the_walk_are_offered_and_only_the_first_ten() {
    let cwd = std::path::Path::new("/project");
    let listed = (0..14)
        .map(|index| format!("/project/src/file_{index:02}.rs"))
        .collect::<Vec<_>>()
        .join("\n");
    // What `glob` appends when it capped its own result, and what it says
    // when it found nothing — neither is a path.
    let output = format!(
        "{listed}\n\n(Results are truncated: showing first 100 results. \
             Consider using a more specific path or pattern.)"
    );

    let offered = super::relative_paths(cwd, &output);

    assert_eq!(offered.len(), 10, "got {offered:?}");
    assert_eq!(offered[0], "src/file_00.rs");
    assert!(
        offered.iter().all(|path| path.starts_with("src/")),
        "the sentence is not a path: {offered:?}"
    );
    assert!(super::relative_paths(cwd, "No files found").is_empty());
}

// ---- clipboard, paste, and the mention filter ----

/// A project root holding `files`, each written with its own name.
fn project_holding(files: &[&str]) -> TempDir {
    let root = temporary();

    for file in files {
        let path = root.path().join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory is creatable");
        }
        std::fs::write(&path, file).expect("the fixture file is writable");
    }

    root
}

/// Everything the status bar is currently saying.
fn status_line(app: &mut App) -> String {
    let mut terminal = terminal(120, 12);
    app.draw(&mut terminal).expect("a frame draws");

    screen(&terminal)
        .lines()
        .next_back()
        .unwrap_or_default()
        .to_owned()
}

/// Submits `text` and hands back every `File` part the engine put on the
/// user message it made of it.
async fn submitted_files(root: &TempDir, text: &str) -> Vec<String> {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin())
        .with_cwd(root.path())
        .with_root(root.path());

    for event in typing(text) {
        app.handle(event).await.expect("typing is handled");
    }
    // A menu the last token raised owns Enter until it is closed; Esc
    // means "cancel the turn" when there is no menu, so it is only sent
    // when there is one.
    if app.files.is_some() {
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let CoreEvent::MessageStarted {
        session_id: _,
        message,
    } = events.next().await.expect("the engine reports the prompt")
    else {
        panic!("the first event of a turn is the user's message");
    };

    message
        .parts
        .iter()
        .filter_map(|part| match &part.body {
            PartBody::File { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// **D113 / R15(a)**, at the seam a person actually types into: `@alice`
/// is a person, and attaching her would put an attachment-error block in
/// front of the model in place of the sentence that was written.
#[tokio::test]
async fn a_word_that_names_no_file_is_submitted_as_text_and_attaches_nothing() {
    let root = project_holding(&["notes.md"]);

    assert!(
        submitted_files(&root, "ask @alice about it")
            .await
            .is_empty(),
        "a name should attach nothing"
    );
}

#[tokio::test]
async fn a_mention_that_names_a_real_file_still_attaches_it() {
    let root = project_holding(&["notes.md"]);

    assert_eq!(
        submitted_files(&root, "read @notes.md please").await,
        vec!["notes.md".to_owned()]
    );
}

/// The degradation the ruling asks for: the typo rides into the prompt as
/// text the model can see and act on, beside the mention that resolved.
#[tokio::test]
async fn a_mistyped_path_rides_as_visible_text_beside_the_one_that_resolved() {
    let root = project_holding(&["notes.md"]);
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin())
        .with_cwd(root.path())
        .with_root(root.path());

    for event in typing("compare @notes.md with @notez.md") {
        app.handle(event).await.expect("typing is handled");
    }
    // The cursor is still inside the second mention, so the file menu owns
    // Enter — closing it is how a person sends this.
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let CoreEvent::MessageStarted {
        session_id: _,
        message,
    } = events.next().await.expect("the engine reports the prompt")
    else {
        panic!("the first event of a turn is the user's message");
    };

    let attached: Vec<&str> = message
        .parts
        .iter()
        .filter_map(|part| match &part.body {
            PartBody::File { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    let text: String = message.parts.iter().filter_map(Part::as_text).collect();

    assert_eq!(attached, vec!["notes.md"], "only the file that exists");
    assert!(
        text.contains("@notez.md"),
        "the typo has to reach the model as text: {text}"
    );
}

/// A pasted paragraph is content, not keystrokes — which is the whole
/// reason bracketed paste is enabled. Both line endings a terminal may
/// send become the line breaks they mean.
#[tokio::test]
async fn a_bracketed_paste_lands_at_the_cursor_with_its_line_breaks_intact() {
    let mut app = app();
    for event in typing("see: ") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(AppEvent::Term(TermEvent::Paste(
        "one\r\ntwo\rthree".to_owned(),
    )))
    .await
    .expect("a paste is handled");

    assert_eq!(
        app.editor.text(),
        "see: one\ntwo\nthree",
        "CRLF and a lone CR both mean a line break"
    );
}

/// The failure this replaces: fed through the key handler, the newline in
/// the middle of a paste is an Enter, and Enter here sends the prompt.
#[tokio::test]
async fn a_multi_line_paste_does_not_submit_the_prompt() {
    let (mut app, mut events) = wired().await;

    app.handle(AppEvent::Term(TermEvent::Paste(
        "first line\nsecond line".to_owned(),
    )))
    .await
    .expect("a paste is handled");

    assert_eq!(app.editor.text(), "first line\nsecond line");
    assert!(
        events.next().now_or_never().is_none(),
        "nothing should have been sent to the engine"
    );
}

/// **F5**: a dropped path becomes a mention instead of landing as raw
/// text — the same insertion the `@` menu's own Enter uses.
#[tokio::test]
async fn a_dropped_path_pastes_as_a_mention() {
    let directory = project();
    let mut app = app_in(&directory);

    app.handle(AppEvent::Term(TermEvent::Paste("src/lib.rs".to_owned())))
        .await
        .expect("a paste is handled");

    assert_eq!(app.editor.text(), "@src/lib.rs ");
}

/// A dropped **image** is the picture, not its path (2026-08-15): the
/// drop inserts the same `[Image #N]` token every other door does, so
/// the strip previews it and the composer never carries the filesystem
/// spelling the user asked to be rid of.
#[tokio::test]
async fn a_dropped_image_path_becomes_an_image_token() {
    let directory = project();
    fs::write(directory.path().join("shot.png"), b"png-ish").expect("the image exists");
    let mut app = app_in(&directory);

    app.handle(AppEvent::Term(TermEvent::Paste("shot.png".to_owned())))
        .await
        .expect("a paste is handled");

    assert_eq!(
        app.editor.text(),
        "[Image #1] ",
        "the picture's token, never its path"
    );
    assert_eq!(
        app.pasted_images_in("[Image #1]")
            .into_iter()
            .map(|mention| mention.path)
            .collect::<Vec<_>>(),
        vec!["shot.png".to_owned()],
        "and the token names the dropped file"
    );
}

/// Several paths pasted at once — the way a terminal hands over a
/// multi-file drag — become several mentions, in the order they arrived.
#[tokio::test]
async fn a_multi_file_drop_pastes_as_mentions_in_order() {
    let directory = project();
    let mut app = app_in(&directory);

    app.handle(AppEvent::Term(TermEvent::Paste(
        "README.md src/lib.rs".to_owned(),
    )))
    .await
    .expect("a paste is handled");

    assert_eq!(app.editor.text(), "@README.md @src/lib.rs ");
}

/// A pasted shell one-liner that happens to name a real path stays
/// ordinary text: not every token in it is a path, and the classifier
/// refuses to half-transform the line.
#[tokio::test]
async fn a_pasted_shell_one_liner_stays_raw_text() {
    let directory = project();
    let mut app = app_in(&directory);

    app.handle(AppEvent::Term(TermEvent::Paste(
        "cat src/lib.rs | grep x".to_owned(),
    )))
    .await
    .expect("a paste is handled");

    assert_eq!(app.editor.text(), "cat src/lib.rs | grep x");
}

#[tokio::test]
async fn a_paste_while_a_dialog_is_up_does_not_reach_the_composer_behind_it() {
    let mut app = app();
    app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await
        .expect("control-p is handled");

    app.handle(AppEvent::Term(TermEvent::Paste("pasted".to_owned())))
        .await
        .expect("a paste is handled");

    assert!(app.palette.is_some(), "the palette is still up");
    assert!(app.editor.is_empty(), "the composer behind it is untouched");
}

#[tokio::test]
async fn control_v_pastes_what_the_clipboard_holds() {
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::holding(
        "pasted\nfrom the clipboard",
    )));

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    assert_eq!(app.editor.text(), "pasted\nfrom the clipboard");
}

/// A clipboard holding neither text nor an image (**F3**, D111's image
/// half) says so and eats no keystroke.
#[tokio::test]
async fn a_clipboard_holding_neither_text_nor_an_image_says_so() {
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::default()));

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    assert!(
        status_line(&mut app).contains("the clipboard holds neither text nor an image"),
        "got: {}",
        status_line(&mut app)
    );
    assert!(app.editor.is_empty(), "and nothing was inserted");
}

/// **F3**, lifting D111's image half — respelled 2026-08-15 to Claude
/// Code's own composer token: a scripted clipboard image pastes as
/// `[Image #N]` inline, and the bytes on disk are a real, decodable PNG
/// of the scripted dimensions.
#[tokio::test]
async fn pasting_a_clipboard_image_shows_an_inline_image_token() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255];
    let mut app = app()
        .with_clipboard(Box::new(clipboard::Recording::holding_image(
            3,
            1,
            rgba.clone(),
        )))
        .with_clipboard_scratch_dir(scratch.path());

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    assert_eq!(
        app.editor.text(),
        "[Image #1] ",
        "the composer carries the token, not the scratch path"
    );

    let bytes = fs::read(scratch.path().join("clipboard-1.png")).expect("the image was saved");
    let (width, height, decoded) = decode_png(&bytes);
    assert_eq!((width, height), (3, 1), "the scripted dimensions survive");
    assert_eq!(decoded, rgba, "and so do the pixels");
}

/// The cursor lights the token it sits on — inside it or right after
/// its bracket, where a fresh paste leaves it — and nothing else.
#[test]
fn the_token_under_the_cursor_is_found_and_prose_is_not() {
    let text = "see [Image #12] here";

    assert_eq!(
        super::image_token_at(text, 4),
        Some((4, 14, 12)),
        "its first char"
    );
    assert_eq!(super::image_token_at(text, 9), Some((4, 14, 12)), "inside");
    assert_eq!(
        super::image_token_at(text, 15),
        Some((4, 14, 12)),
        "right after the bracket"
    );
    assert_eq!(super::image_token_at(text, 3), None, "before it");
    assert_eq!(super::image_token_at(text, 16), None, "past it");
    assert_eq!(super::image_token_at("no tokens at all", 5), None);
    assert_eq!(
        super::image_token_at("[Image #] empty", 3),
        None,
        "digits are required"
    );
}

/// The offset math the highlight rides: rows rejoined on newlines,
/// counted in characters, multibyte included.
#[test]
fn the_cursor_offset_counts_characters_across_lines() {
    assert_eq!(super::char_offset("abc\ndef", 1, 2), 6);
    assert_eq!(super::char_offset("見て\n[Image #1]", 1, 0), 3);
    assert_eq!(super::char_offset("abc", 0, 3), 3);
}

/// Backspace on a token the cursor lights takes the whole token —
/// Claude Code's own composer rule, widened to
/// anywhere inside it. At the token's very front it stays an ordinary
/// one-character delete, because backspace there is about what sits
/// before the token.
#[tokio::test]
async fn backspace_on_an_image_token_deletes_the_whole_token() {
    {
        let mut app = app();
        typed(&mut app, "see [Image #12] x").await;
        for _ in 0..3 {
            app.handle(key(KeyCode::Left, KeyModifiers::NONE))
                .await
                .expect("left is handled");
        }
        app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
            .await
            .expect("backspace is handled");
        assert_eq!(
            app.editor.text(),
            "see  x",
            "mid-token backspace takes the token whole"
        );
    }

    let mut app = app();
    typed(&mut app, "ab[Image #5]").await;
    for _ in 0..10 {
        app.handle(key(KeyCode::Left, KeyModifiers::NONE))
            .await
            .expect("left is handled");
    }
    app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
        .await
        .expect("backspace is handled");
    assert_eq!(
        app.editor.text(),
        "a[Image #5]",
        "at the token's front, backspace is about the character before it"
    );
}

/// Claude Code's focus-time nudge (observed behaviour): coming back
/// to a terminal with an image on the clipboard says so once, and the
/// thirty-second limit keeps a window-switching flurry to one hint.
#[tokio::test]
async fn regaining_focus_over_a_clipboard_image_hints_once() {
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::holding_image(
        1,
        1,
        vec![1, 2, 3, 4],
    )));

    app.handle(AppEvent::Term(TermEvent::FocusGained))
        .await
        .expect("focus is handled");
    assert!(
        status_line(&mut app).contains("Image in clipboard"),
        "the hint names the ctrl+v door"
    );

    app.status.set_notice(None);
    app.handle(AppEvent::Term(TermEvent::FocusGained))
        .await
        .expect("focus is handled again");
    assert!(
        !status_line(&mut app).contains("Image in clipboard"),
        "the rate limit holds the second hint"
    );
}

/// Cmd+V over an image-only clipboard: the terminal has no text and
/// sends the bracketed-paste envelope **empty**, and that emptiness
/// routes to the clipboard chain — Claude Code's own mechanism
/// (observed 2026-08-15) — so the system paste chord
/// attaches the image exactly as Ctrl+V does.
#[tokio::test]
async fn an_empty_terminal_paste_reads_the_image_off_the_clipboard() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let mut app = app()
        .with_clipboard(Box::new(clipboard::Recording::holding_image(
            1,
            1,
            vec![1, 2, 3, 4],
        )))
        .with_clipboard_scratch_dir(scratch.path());

    app.handle(AppEvent::Term(TermEvent::Paste(String::new())))
        .await
        .expect("an empty paste is handled");

    assert_eq!(
        app.editor.text(),
        "[Image #1] ",
        "the empty envelope reads the clipboard image instead"
    );
    assert!(scratch.path().join("clipboard-1.png").exists());
}

/// The pinned 2026-08-15 screenshot: a file copied in Finder rides the
/// pasteboard as its URL *and* its bare name as text, and pasting it must
/// consult the files first — asked for text, the paste inserted
/// `Screenshot ….png`, a basename that resolves nowhere. An image file
/// tokenizes as `[Image #N]` mapped to the copied file itself, no scratch
/// copy made.
#[tokio::test]
async fn pasting_a_copied_image_file_tokenizes_the_file_itself() {
    let dir = tempfile::tempdir().expect("a directory for the copied file");
    let copied = dir.path().join("Screenshot 2026-08-15 at 10.29.02.png");
    fs::write(&copied, b"not-really-a-png").expect("the copied file exists");
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::holding_files(vec![
        copied.clone(),
    ])));

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    assert_eq!(
        app.editor.text(),
        "[Image #1] ",
        "the copied image is a token, not its basename as text"
    );
    assert_eq!(
        app.pasted_images_in("[Image #1]")
            .into_iter()
            .map(|mention| mention.path)
            .collect::<Vec<_>>(),
        vec![copied.display().to_string()],
        "and the token names the copied file itself"
    );
}

/// A copied file that is not an image goes through the same classifier a
/// typed or dropped path does — an existing file becomes an `@` mention.
#[tokio::test]
async fn pasting_a_copied_non_image_file_becomes_a_mention() {
    let dir = tempfile::tempdir().expect("a directory for the copied file");
    let copied = dir.path().join("notes.rs");
    fs::write(&copied, b"fn main() {}").expect("the copied file exists");
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::holding_files(vec![
        copied.clone(),
    ])));

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    assert_eq!(
        app.editor.text(),
        format!("@{} ", copied.display()),
        "a non-image file pastes as the mention its drop would"
    );
}

/// The token is the composer's face for the saved file: submitting a
/// prompt that still carries it attaches the PNG exactly as an `@`
/// mention would, while the text the model reads keeps the token.
#[tokio::test]
async fn a_submitted_image_token_attaches_the_saved_png() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin())
        .with_clipboard(Box::new(clipboard::Recording::holding_image(
            1,
            1,
            vec![1, 2, 3, 4],
        )))
        .with_clipboard_scratch_dir(scratch.path());

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");
    for event in typing("what is this") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let CoreEvent::MessageStarted { message, .. } =
        events.next().await.expect("the engine reports the prompt")
    else {
        panic!("the first event of a turn is the user's message");
    };
    let files: Vec<&str> = message
        .parts
        .iter()
        .filter_map(|part| match &part.body {
            PartBody::File { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(
        files,
        vec![scratch.path().join("clipboard-1.png").display().to_string()],
        "the token's file rides the mentions"
    );
    assert!(
        message
            .parts
            .iter()
            .any(|part| part.as_text() == Some("[Image #1] what is this")),
        "and the text keeps the token: {:?}",
        message.parts
    );
}

/// A second paste in the same session earns its own name rather than
/// overwriting the first.
#[tokio::test]
async fn a_second_clipboard_image_paste_gets_its_own_number() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let mut app = app()
        .with_clipboard(Box::new(clipboard::Recording::holding_image(
            1,
            1,
            vec![1, 2, 3, 4],
        )))
        .with_clipboard_scratch_dir(scratch.path());

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("the first control-v is handled");
    app.editor.clear();
    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("the second control-v is handled");

    assert_eq!(app.editor.text(), "[Image #2] ");
    assert!(scratch.path().join("clipboard-2.png").exists());
    assert!(scratch.path().join("clipboard-1.png").exists());
    assert!(scratch.path().join("clipboard-2.png").exists());
}

/// The submit-time degradation warning (`App::degraded`) fires for a
/// pasted image exactly as it does for an `@file` mention: the pasted
/// image reaches the same pipeline, and a wire without image support is
/// told so before the turn.
#[tokio::test]
async fn a_clipboard_image_on_a_wire_without_image_support_warns_at_submit() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let (provider, _requests) = ganja_testkit::ScriptedProvider::text_only(Vec::new());
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
    );
    let mut app = App::new(engine, None, Themes::builtin())
        .with_clipboard(Box::new(clipboard::Recording::holding_image(
            1,
            1,
            vec![9, 8, 7, 6],
        )))
        .with_clipboard_scratch_dir(scratch.path());

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    // Asserted at the data level, not through the rendered status line:
    // a real `<XDG data>/ganja/clipboard` path is long enough that the
    // rendered bar can truncate it before the mime reaches the visible
    // width — a pre-existing, unrelated property of a one-line status
    // bar, not something this test should depend on. The mention comes
    // off the `[Image #N]` token's own map (2026-08-15), since the
    // composer no longer carries the path.
    let pasted = app.editor.text();
    let mentions = app.pasted_images_in(&pasted);
    assert_eq!(mentions.len(), 1, "the pasted image resolved as a mention");
    assert_eq!(
        app.degraded(&mentions),
        vec![format!(
            "@{} (image/png)",
            scratch.path().join("clipboard-1.png").display()
        )],
        "the pasted image is exactly what the degradation warning names"
    );

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let bar = status_line(&mut app);
    assert!(
        bar.contains("attached by name only") && bar.contains("does not carry"),
        "the degradation warning fires at submit: {bar}"
    );
}

/// A scratch directory that cannot be created — a plain file sitting
/// where the directory would need to be — degrades to a notice naming
/// why, the same posture `ganja-tool`'s own spill directory takes on a
/// write it cannot make.
#[tokio::test]
async fn a_clipboard_image_that_cannot_be_saved_reports_why() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let blocked = scratch.path().join("blocked");
    fs::write(&blocked, "not a directory").expect("the fixture writes");
    let mut app = app()
        .with_clipboard(Box::new(clipboard::Recording::holding_image(
            1,
            1,
            vec![1, 2, 3, 4],
        )))
        .with_clipboard_scratch_dir(&blocked);

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    assert!(app.editor.is_empty(), "nothing was inserted");
    assert!(
        !status_line(&mut app).is_empty(),
        "the failure is reported rather than swallowed"
    );
}

/// Decodes `bytes` as a PNG, answering its declared width, height and raw
/// RGBA8 pixels.
fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("a valid png header");
    let mut buffer = vec![0; reader.output_buffer_size().expect("a sized frame")];
    let info = reader.next_frame(&mut buffer).expect("a valid png frame");
    buffer.truncate(info.buffer_size());

    (info.width, info.height, buffer)
}

/// [`App::encode_clipboard_png`] round-trips through [`decode_png`] for
/// the two edge shapes real screenshots stress: a single pixel, and
/// dimensions with no shared factor to hide a stride bug behind.
#[test]
fn encoding_a_clipboard_image_round_trips_at_1x1() {
    let image = clipboard::Image {
        width: 1,
        height: 1,
        rgba: vec![10, 20, 30, 40],
    };

    let bytes = App::encode_clipboard_png(&image).expect("the encode succeeds");
    let (width, height, decoded) = decode_png(&bytes);

    assert_eq!((width, height), (1, 1));
    assert_eq!(decoded, image.rgba);
}

#[test]
fn encoding_a_clipboard_image_round_trips_at_odd_dimensions() {
    let rgba: Vec<u8> = (0..(3 * 5 * 4)).map(|byte| byte as u8).collect();
    let image = clipboard::Image {
        width: 3,
        height: 5,
        rgba: rgba.clone(),
    };

    let bytes = App::encode_clipboard_png(&image).expect("the encode succeeds");
    let (width, height, decoded) = decode_png(&bytes);

    assert_eq!((width, height), (3, 5));
    assert_eq!(decoded, rgba);
}

/// A machine with no clipboard costs a notice, never the keystroke.
#[tokio::test]
async fn a_clipboard_that_cannot_be_reached_is_a_notice_and_not_a_lost_prompt() {
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::refusing_reads(
        clipboard::Error::Unavailable("no display".to_owned()),
    )));
    for event in typing("half a thought") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    assert!(status_line(&mut app).contains("no display"));
    assert_eq!(
        app.editor.text(),
        "half a thought",
        "what was being typed survives"
    );
}

/// A user who bound `ctrl+v` to something else gets what they asked for:
/// the paste fallback is checked after the bindings, not before them.
#[tokio::test]
async fn a_rebound_control_v_reaches_its_binding_rather_than_the_clipboard() {
    let keys = crate::keybind::Keybinds::from_config(
        &[("app_exit".to_owned(), "ctrl+v".to_owned())]
            .into_iter()
            .collect(),
    )
    .expect("the binding parses");
    let mut app = app()
        .with_keybinds(keys)
        .with_clipboard(Box::new(clipboard::Recording::holding("not this")));

    app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
        .await
        .expect("control-v is handled");

    assert!(app.quit, "the binding wins");
    assert!(app.editor.is_empty());
}

/// An assistant message carrying `texts`, as the transcript holds one.
fn replied(texts: &[&str]) -> Message {
    Message {
        id: ganja_protocol::MessageId::ascending(),
        role: ganja_protocol::Role::Assistant,
        parts: texts.iter().map(|text| Part::text(*text)).collect(),
        time: ganja_protocol::MessageTime {
            created: 1,
            completed: Some(2),
        },
        model: Some(fake::MODEL.to_owned()),
        usage: None,
    }
}

#[tokio::test]
async fn copying_a_message_hands_over_the_last_reply_alone() {
    let clipboard = clipboard::Recording::default();
    let log = clipboard.log();
    let mut app = app().with_clipboard(Box::new(clipboard));
    app.seed(vec![
        replied(&["an older answer"]),
        Message::user("and then?"),
        replied(&["  the newest answer", "in two parts  "]),
    ]);

    app.run_command(command::Action::CopyMessage).await;

    assert_eq!(
        *log.lock().expect("the lock holds"),
        vec!["the newest answer\nin two parts".to_owned()],
        "the parts join on a newline and the whole is trimmed"
    );
    assert!(
        status_line(&mut app).contains("Message copied to clipboard!"),
        "got: {}",
        status_line(&mut app)
    );
}

#[tokio::test]
async fn a_copy_queues_the_osc52_escape_even_when_the_system_clipboard_fails() {
    // Upstream writes the terminal escape before it tries a system
    // method: on a headless or SSH box the escape is the only channel
    // that still delivers, so a desktop refusal must not suppress it.
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::refusing_writes(
        clipboard::Error::Unavailable("no display".to_owned()),
    )));
    app.seed(vec![Message::user("and then?"), replied(&["the answer"])]);

    app.run_command(command::Action::CopyMessage).await;

    assert_eq!(
        app.pending_osc,
        vec![clipboard::osc52::sequence("the answer")],
        "one escape, queued regardless of the system half"
    );
    assert!(
        status_line(&mut app).contains("Failed to copy to clipboard"),
        "got: {}",
        status_line(&mut app)
    );
}

/// An app announcing through a capture buffer under the `tui` table
/// `tui` parses to, plus the handle the assertions read (**D468**).
fn notifying_app(tui: serde_json::Value) -> (App, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    let config: ganja_core::config::TuiConfig =
        serde_json::from_value(tui).expect("the fixture is a tui table");
    let capture = crate::notify::Capture::default();
    let log = capture.log();
    let app = app().with_notifier(crate::notify::Notifier::over(config, Box::new(capture)));

    (app, log)
}

/// Every byte the notifier has written so far.
fn notified(log: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<u8> {
    log.lock().expect("the capture lock holds").clone()
}

/// The composer's cursor is the terminal's own: the frame places it on
/// the composer's cursor cell — before the placeholder, after typed text
/// — and paints nothing there; while a modal has the keys the frame
/// places none.
#[tokio::test]
async fn the_terminal_cursor_sits_in_the_composer_and_no_cell_is_painted_for_it() {
    let mut app = app();
    let mut terminal = terminal(80, 24);

    app.draw(&mut terminal).expect("a frame draws");
    let text = screen(&terminal);
    let row = text
        .lines()
        .position(|line| line.contains("Ask ganja something"))
        .expect("the composer is on screen");
    let row = u16::try_from(row).expect("a row fits");
    let placed = terminal
        .get_cursor_position()
        .expect("the test backend reports the cursor");
    assert_eq!((placed.x, placed.y), (1, row), "before the placeholder");
    assert!(
        !terminal.backend().buffer()[(1, row)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED),
        "and no cell is painted for it"
    );

    for character in ['o', 'k'] {
        app.handle(key(KeyCode::Char(character), KeyModifiers::NONE))
            .await
            .expect("typing is handled");
    }
    app.draw(&mut terminal).expect("a frame draws");
    let placed = terminal
        .get_cursor_position()
        .expect("the test backend reports the cursor");
    assert_eq!((placed.x, placed.y), (3, row), "after the typed text");
}

#[tokio::test]
async fn an_unfocused_finished_turn_writes_exactly_one_osc9_notification() {
    let (mut app, log) = notifying_app(serde_json::json!({"notifications": true}));

    app.handle(AppEvent::Term(TermEvent::FocusLost))
        .await
        .expect("a focus event is handled");
    app.handle(finished(fake::MODEL, Usage::default()))
        .await
        .expect("a finish is handled");

    let bytes = String::from_utf8(notified(&log)).expect("the escape is utf-8");
    assert_eq!(
        bytes, "\x1b]9;turn complete\x07",
        "one sequence, whole, and nothing beside it"
    );
}

#[tokio::test]
async fn a_focused_finished_turn_writes_nothing() {
    let (mut app, log) = notifying_app(serde_json::json!({"notifications": true}));

    app.handle(AppEvent::Term(TermEvent::FocusLost))
        .await
        .expect("a focus event is handled");
    app.handle(AppEvent::Term(TermEvent::FocusGained))
        .await
        .expect("a focus event is handled");
    app.handle(finished(fake::MODEL, Usage::default()))
        .await
        .expect("a finish is handled");

    assert!(
        notified(&log).is_empty(),
        "a watched terminal hears nothing"
    );
}

/// Crossterm only learns the state from the first focus event, so a turn
/// finishing before one arrived runs on the assumption that somebody is
/// watching — quiet-by-default (**D468**).
#[tokio::test]
async fn a_turn_finishing_before_any_focus_event_is_assumed_watched() {
    let (mut app, log) = notifying_app(serde_json::json!({"notifications": true}));

    app.handle(finished(fake::MODEL, Usage::default()))
        .await
        .expect("a finish is handled");

    assert!(notified(&log).is_empty());
}

#[tokio::test]
async fn the_bel_method_writes_exactly_the_bell_byte() {
    let (mut app, log) =
        notifying_app(serde_json::json!({"notifications": true, "notification_method": "bel"}));

    app.handle(AppEvent::Term(TermEvent::FocusLost))
        .await
        .expect("a focus event is handled");
    app.handle(finished(fake::MODEL, Usage::default()))
        .await
        .expect("a finish is handled");

    assert_eq!(notified(&log), b"\x07");
}

#[tokio::test]
async fn an_approval_only_config_notifies_on_a_permission_dialog_and_not_at_turn_end() {
    let (mut app, log) =
        notifying_app(serde_json::json!({"notifications": ["approval-requested"]}));
    app.handle(AppEvent::Term(TermEvent::FocusLost))
        .await
        .expect("a focus event is handled");

    app.handle(finished(fake::MODEL, Usage::default()))
        .await
        .expect("a finish is handled");
    assert!(
        notified(&log).is_empty(),
        "turn end is a moment this config did not ask for"
    );

    app.handle(AppEvent::core(permission_event("perm_1")))
        .await
        .expect("a permission request is handled");
    let bytes = String::from_utf8(notified(&log)).expect("the escape is utf-8");
    assert_eq!(
        bytes, "\x1b]9;approval requested: cargo test\x07",
        "the one asked-for moment announces, once"
    );
}

#[tokio::test]
async fn copying_a_message_with_nothing_to_copy_says_which_kind_of_nothing() {
    let clipboard = clipboard::Recording::default();
    let log = clipboard.log();
    let mut app = app().with_clipboard(Box::new(clipboard));

    app.run_command(command::Action::CopyMessage).await;

    assert!(
        status_line(&mut app).contains("No assistant messages found"),
        "got: {}",
        status_line(&mut app)
    );
    assert!(
        log.lock().expect("the lock holds").is_empty(),
        "nothing was copied"
    );
}

#[tokio::test]
async fn copying_the_transcript_hands_over_the_whole_conversation() {
    let directory = temporary();
    store_session(
        &directory,
        "0198f2c4-a1b0-7000-8000-000000000016",
        Some("a stored talk"),
        10_000,
        0,
        0,
    );
    let clipboard = clipboard::Recording::default();
    let log = clipboard.log();
    let mut app = persistent_app(&directory).with_clipboard(Box::new(clipboard));
    let stored = app
        .engine
        .resume(&SessionId::from(
            "0198f2c4-a1b0-7000-8000-000000000016".to_owned(),
        ))
        .await
        .expect("the stored session resumes");
    app.seed(stored);

    app.run_command(command::Action::Copy).await;

    let copied = log.lock().expect("the lock holds").join("");
    assert!(copied.starts_with("# a stored talk\n\n"), "got: {copied}");
    assert!(
        copied.contains("**Session ID:** 0198f2c4-a1b0-7000-8000-000000000016\n"),
        "got: {copied}"
    );
    assert!(
        copied.contains("## User\n\nwhat the picker is choosing between\n\n---\n\n"),
        "the conversation itself has to be in it: {copied}"
    );
    assert!(
        status_line(&mut app).contains("Session transcript copied to clipboard!"),
        "got: {}",
        status_line(&mut app)
    );
}

/// Upstream returns silently here; a person who asked for a copy is owed
/// an answer (deviation: copy-with-no-session-says-so).
#[tokio::test]
async fn copying_the_transcript_before_there_is_one_says_so() {
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::default()));

    app.run_command(command::Action::Copy).await;

    assert!(
        status_line(&mut app).contains("there is no session to copy yet"),
        "got: {}",
        status_line(&mut app)
    );
}

#[tokio::test]
async fn a_clipboard_that_refuses_a_copy_says_so_in_upstreams_words() {
    let mut app = app().with_clipboard(Box::new(clipboard::Recording::refusing_writes(
        clipboard::Error::Unavailable("no display".to_owned()),
    )));
    app.seed(vec![replied(&["an answer"])]);

    app.run_command(command::Action::CopyMessage).await;

    let line = status_line(&mut app);
    assert!(line.contains("Failed to copy to clipboard"), "got: {line}");
    assert!(
        line.contains("no display"),
        "and it names the reason: {line}"
    );
}

// ---- the MCP status notice ----

/// An engine holding one MCP server named `broken`, whose command is a
/// path nothing can spawn.
fn engine_dialling_a_missing_server(root: &TempDir) -> Engine {
    let config: std::collections::BTreeMap<String, ganja_core::config::McpServer> =
        serde_json::from_value(serde_json::json!({
            "broken": { "type": "local", "command": ["/nonexistent-ganja-fixture"] }
        }))
        .expect("the fixture is a config");

    engine().with_mcp(ganja_core::McpServers::new(config, root.path()))
}

/// **R3's fake-provider-notice pattern.** A server that cannot be reached
/// costs its tools and one line of the status bar, and never the session.
#[tokio::test]
async fn a_server_that_cannot_be_reached_is_named_in_the_status_bar() {
    let root = temporary();
    let engine = engine_dialling_a_missing_server(&root);
    engine.connect_mcp();
    let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);

    // Nothing is said while it is still being dialled: a server with no
    // status yet is one nothing has finished trying.
    let mut line = status_line(&mut app);
    for _ in 0..400 {
        if line.contains("mcp broken") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        line = status_line(&mut app);
    }

    assert!(
        line.contains("mcp broken"),
        "the failed server should be named: {line}"
    );
    assert!(
        !app.pending_mcp(),
        "and once it has answered there is nothing left to wait for"
    );
}

#[tokio::test]
async fn a_session_with_no_mcp_servers_says_nothing_about_them() {
    let mut app = app();

    for _ in 0..8 {
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
    }

    assert!(
        !status_line(&mut app).contains("mcp"),
        "got: {}",
        status_line(&mut app)
    );
    assert!(
        !app.pending_mcp(),
        "and nothing keeps the loop awake looking for one"
    );
}

/// Without this, an idle app never wakes: `wants_wakeup` is what schedules
/// the tick the poll rides on, and a failed server would sit unreported
/// until the user's next keystroke.
#[tokio::test]
async fn a_session_still_dialling_keeps_waking_up_to_look() {
    let root = temporary();
    let mut app = App::new(
        engine_dialling_a_missing_server(&root),
        None,
        Themes::builtin(),
    )
    .watching_mcp(1);
    app.draw(&mut terminal(80, 24)).expect("a frame draws");

    assert!(!app.dirty, "the frame above cleared it");
    assert!(app.wants_wakeup(), "the dial is what keeps it awake");
}

// ---- F5: the `/mcp` dialog ----

/// Runs `app` a few ticks, until at least one MCP server has answered.
async fn wait_for_mcp_to_settle(app: &mut App) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && app.engine.mcp_status().is_empty() {
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `/mcp` lists every configured server with its status; a failed one
/// offers Reconnect, and one that never lent a tool names no count.
#[tokio::test]
async fn slash_mcp_lists_every_configured_server_with_its_status_and_actions() {
    let root = temporary();
    let engine = engine_dialling_a_missing_server(&root);
    engine.connect_mcp();
    let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);
    wait_for_mcp_to_settle(&mut app).await;

    app.run_command(command::Action::Mcp).await;

    let dialog = app.mcp_dialog.as_ref().expect("/mcp opens the dialog");
    let row = dialog
        .selected()
        .expect("the one configured server has a row");
    assert_eq!(row.name, "broken");
    assert_eq!(row.status, "Failed");
    assert_eq!(row.tools, None, "a failed server lends nothing to count");
    assert!(row.detail.is_some(), "a failed row names why");
    assert_eq!(row.actions, vec![mcp::Action::Reconnect]);
}

/// Login belongs on a remote server configured with `oauth` whatever its
/// status — unlike Reconnect above, gated on `Failed` alone — and running
/// it shows the URL to open in the row until the browser finishes it.
///
/// Deliberately never connects the server (no `connect_mcp()`,
/// `wait_for_mcp_to_settle` unused): a real connect attempt would read
/// this machine's own credential store, which is exactly what this test
/// must not touch — see `crates/ganja-core/tests/mcp_oauth.rs` for the
/// isolated-process test that does exercise storage. `mcp_has_oauth` and
/// `mcp_login_url`, which this test is about, read the config and the
/// in-memory login map only.
#[tokio::test]
async fn login_appears_for_an_oauth_server_and_shows_the_url_while_it_runs() {
    let address = oauth_discovery_fixture().await;
    let root = temporary();
    let config: std::collections::BTreeMap<String, ganja_core::config::McpServer> =
        serde_json::from_value(serde_json::json!({
            "hub": {
                "type": "remote",
                "url": format!("http://{address}/mcp"),
                "oauth": {},
            }
        }))
        .expect("the fixture is a config");
    let engine = engine().with_mcp(ganja_core::McpServers::new(config, root.path()));
    let mut app = App::new(engine, None, Themes::builtin());

    app.run_command(command::Action::Mcp).await;
    let dialog = app.mcp_dialog.as_ref().expect("/mcp opens the dialog");
    let row = dialog
        .selected()
        .expect("the one configured server has a row");
    assert_eq!(
        row.status, "dialling",
        "nothing has attempted a connect yet"
    );
    assert_eq!(
        row.actions,
        vec![mcp::Action::Login],
        "an oauth-configured server offers Login whatever its status"
    );

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter opens the row's one action");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter runs the chosen action");

    let dialog = app.mcp_dialog.as_ref().expect("the dialog stays open");
    let row = dialog.selected().expect("still the one row");
    assert_eq!(row.status, "Logging in", "{row:?}");
    assert!(
        row.detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with("go to: http")),
        "the URL a person has to open should be shown: {row:?}"
    );
}

/// A loopback RFC 8414 authorization-server endpoint, answering only
/// `/.well-known/oauth-authorization-server` — enough for
/// `Servers::start_login`'s discovery step, which is as far as the test
/// above runs it (no `/register`, since a server naming no registration
/// endpoint is itself a real, exercised path — the fixed fallback client
/// id — and one this test does not need to distinguish).
async fn oauth_discovery_fixture() -> std::net::SocketAddr {
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the socket has an address");

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = [0_u8; 4096];
                if stream.read(&mut buffer).await.is_err() {
                    return;
                }
                let body = serde_json::json!({
                    "authorization_endpoint": format!("http://{address}/authorize"),
                    "token_endpoint": format!("http://{address}/token"),
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    address
}

/// Esc closes the dialog from either of its two steps rather than
/// stepping back to the first — [`App::handle_rewind_key`]'s own rule,
/// answered the same way here.
#[tokio::test]
async fn esc_closes_the_mcp_dialog_from_either_step() {
    let root = temporary();
    let engine = engine_dialling_a_missing_server(&root);
    engine.connect_mcp();
    let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);
    wait_for_mcp_to_settle(&mut app).await;

    app.run_command(command::Action::Mcp).await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter opens the failed row's actions");
    assert!(
        app.mcp_dialog
            .as_ref()
            .is_some_and(mcp::Mcp::is_choosing_action),
        "the one configured server is failed, so enter must open its actions"
    );

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.mcp_dialog.is_none(), "escape closes the whole dialog");
}

/// A row with nothing to choose leaves the dialog exactly as it was: no
/// close, no action step, unlike the rewind picker's `(Current)` row.
#[tokio::test]
async fn enter_on_a_row_with_no_actions_leaves_the_dialog_open() {
    let root = temporary();
    let config: std::collections::BTreeMap<String, ganja_core::config::McpServer> =
        serde_json::from_value(serde_json::json!({
            "off": { "type": "local", "command": ["never-run"], "enabled": false }
        }))
        .expect("the fixture is a config");
    let engine = engine().with_mcp(ganja_core::McpServers::new(config, root.path()));
    let mut app = App::new(engine, None, Themes::builtin());

    app.run_command(command::Action::Mcp).await;
    let dialog = app.mcp_dialog.as_ref().expect("/mcp opens the dialog");
    assert_eq!(
        dialog.selected().map(|row| row.status.as_str()),
        Some("Disabled")
    );

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert!(app.mcp_dialog.is_some(), "a disabled row does not close it");
    assert!(
        !app.mcp_dialog
            .as_ref()
            .is_some_and(mcp::Mcp::is_choosing_action),
        "and it has nothing to choose, so no action step opens"
    );
}

/// Reconnect run from the dialog is not a UI-only gesture: it drives the
/// engine's own `reconnect_mcp`, which spawns a fresh dial — proven by
/// counting real invocations of the fixture command rather than trusting
/// a status string.
#[cfg(unix)]
#[tokio::test]
async fn reconnect_from_the_dialog_spawns_a_fresh_dial() {
    let root = temporary();
    let counter = root.path().join("attempts");
    let config: std::collections::BTreeMap<String, ganja_core::config::McpServer> =
        serde_json::from_value(serde_json::json!({
            "flaky": {
                "type": "local",
                "command": ["sh", "-c", format!("echo x >> {} ; exit 1", counter.display())],
            }
        }))
        .expect("the fixture is a config");
    let engine = engine().with_mcp(ganja_core::McpServers::new(config, root.path()));
    engine.connect_mcp();
    let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);
    wait_for_mcp_to_settle(&mut app).await;
    let attempts = |path: &std::path::Path| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert_eq!(
        attempts(&counter),
        1,
        "the startup dial is the first attempt"
    );

    app.run_command(command::Action::Mcp).await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter opens the failed row's actions");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter on Reconnect runs it");

    assert!(
        app.mcp_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.is_choosing_action()),
        "running the action returns to the server list"
    );
    assert_eq!(
        attempts(&counter),
        2,
        "reconnect must have spawned a real second dial"
    );
}

/// The dialog, over one connected and one failed server (screenshot: no
/// reference available, house `ListDialog`/`Rewind` chrome).
#[tokio::test]
async fn snapshot_mcp_dialog_open() {
    let root = temporary();
    let engine = engine_dialling_a_missing_server(&root);
    engine.connect_mcp();
    let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);
    wait_for_mcp_to_settle(&mut app).await;

    app.run_command(command::Action::Mcp).await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

// ---- D470/D471: the `/context` and `/usage` panels ----

/// A breakdown with something in every category over a small round
/// window, shared by the panel tests below.
fn breakdown_fixture() -> ganja_core::engine::ContextBreakdown {
    ganja_core::engine::ContextBreakdown {
        model: "claude-sonnet-5".to_owned(),
        system_prompt: 3_000,
        instructions: 2_000,
        tools_builtin: 11_000,
        tools_mcp: 1_000,
        tools_builtin_count: 12,
        tools_mcp_count: 3,
        skills: 500,
        conversation_user: 4_000,
        conversation_assistant: 8_500,
        window: Some(100_000),
        reserve: Some(10_000),
    }
}

/// `/context` opens the panel from the command roster — computed on the
/// spot, so a fresh session with zero turns still gets one — and Esc
/// closes it.
#[tokio::test]
async fn slash_context_opens_the_panel_and_esc_closes_it() {
    let mut app = app();
    assert_eq!(
        command::lookup("context").map(|entry| entry.action),
        Some(command::Action::Context),
        "/context is on the roster"
    );

    app.run_command(command::Action::Context).await;
    assert!(app.context_dialog.is_some(), "/context opens the panel");

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.context_dialog.is_none(), "escape closes it");
}

/// `/usage` opens its panel from the roster, and Esc closes it.
#[tokio::test]
async fn slash_usage_opens_the_panel_and_esc_closes_it() {
    let mut app = app();
    assert_eq!(
        command::lookup("usage").map(|entry| entry.action),
        Some(command::Action::Usage),
        "/usage is on the roster"
    );

    app.run_command(command::Action::Usage).await;
    assert!(app.usage_dialog.is_some(), "/usage opens the panel");

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.usage_dialog.is_none(), "escape closes it");
}

/// AC5: the `/usage` session line is the status bar's own `Totals`
/// string — the same formatter, asserted against the same value the bar
/// itself renders — never a second formatting of the same numbers.
#[tokio::test]
async fn the_usage_panel_shows_the_status_bars_own_totals_string() {
    let mut app = app();
    app.record(
        &MessageId::from("msg_1".to_owned()),
        &Usage {
            input_tokens: 1_200,
            output_tokens: 34,
            reasoning_tokens: 5,
            cache_read_tokens: 600,
            cache_write_tokens: 100,
        },
    );

    app.run_command(command::Action::Usage).await;
    let mut terminal = terminal(80, 30);
    app.draw(&mut terminal).expect("a frame draws");

    let segment = app.totals.segment();
    assert!(
        screen(&terminal).contains(&segment),
        "want the bar's own {segment:?} in:\n{}",
        screen(&terminal)
    );
}

/// The plan-limit meters Claude Code leads with ride a vendor usage API
/// ganja does not speak: the panel carries the one honest line naming
/// why, and no such meter is ever drawn (D471, plan Open question 2).
#[tokio::test]
async fn the_usage_panel_carries_no_plan_limit_meter_and_says_why() {
    let mut app = app();
    app.run_command(command::Action::Usage).await;
    let mut terminal = terminal(80, 30);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);

    assert!(
        screen.contains("plan limits unavailable on this credential (probed 2026-08-14):"),
        "got:\n{screen}"
    );
    // P17 (**D485**): the fake provider serves no plan header, so the
    // panel says which credentials are silent and why — and still draws
    // no meter over nothing.
    assert!(
        screen.contains("platform.claude.com/docs/en/manage-claude/usage-cost-api"),
        "the reason names the vendor's own page, whole:\n{screen}"
    );
    assert!(
        !screen.contains("Plan limits"),
        "no plan-limit meter may be drawn:\n{screen}"
    );
}

/// The `/context` grid over a sized fixture breakdown (screenshot:
/// Claude Code's grid-and-legend panel; house dialog chrome). The
/// breakdown is injected rather than computed so the cells and legend
/// are stable whatever machine renders them.
#[tokio::test]
async fn snapshot_context_dialog_open() {
    let mut app = app();
    app.context_dialog = Some(component::context::Context::new(
        Some("Claude Sonnet 5".to_owned()),
        breakdown_fixture(),
    ));

    let mut terminal = terminal(80, 36);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// The same panel degraded: no window, so totals alone and the honest
/// sentence — no invented denominator.
#[tokio::test]
async fn snapshot_context_dialog_degraded() {
    let mut app = app();
    app.context_dialog = Some(component::context::Context::new(
        None,
        ganja_core::engine::ContextBreakdown {
            model: fake::MODEL.to_owned(),
            window: None,
            reserve: None,
            ..breakdown_fixture()
        },
    ));

    let mut terminal = terminal(80, 36);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// The `/usage` panel over a session with one recorded turn (screenshot:
/// Claude Code's sectioned panel; house dialog chrome, HUD meter shape).
#[tokio::test]
async fn snapshot_usage_dialog_open() {
    let mut app = app();
    app.record(
        &MessageId::from("msg_fixture".to_owned()),
        &Usage {
            input_tokens: 1_200,
            output_tokens: 34,
            reasoning_tokens: 5,
            cache_read_tokens: 600,
            cache_write_tokens: 100,
        },
    );

    app.run_command(command::Action::Usage).await;
    let mut terminal = terminal(80, 30);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

#[test]
fn only_a_failed_server_earns_a_notice_and_it_is_one_line() {
    let cases = [
        (vec![("fs", ganja_core::McpStatus::Connected)], None),
        (vec![("fs", ganja_core::McpStatus::Disabled)], None),
        (
            vec![(
                "fs",
                ganja_core::McpStatus::Failed {
                    error: "no such file\n  while spawning".to_owned(),
                },
            )],
            Some("mcp fs: no such file"),
        ),
        (
            vec![
                ("fs", ganja_core::McpStatus::Connected),
                (
                    "hub",
                    ganja_core::McpStatus::Failed {
                        error: "connection refused".to_owned(),
                    },
                ),
            ],
            Some("mcp hub: connection refused"),
        ),
    ];

    for (status, expected) in cases {
        let status: std::collections::BTreeMap<String, ganja_core::McpStatus> = status
            .into_iter()
            .map(|(name, status)| (name.to_owned(), status))
            .collect();

        assert_eq!(super::mcp_notice(&status).as_deref(), expected);
    }
    assert_eq!(super::mcp_notice(&std::collections::BTreeMap::new()), None);
}

/// A conversation of two exchanges, and the id of the prompt an undo of
/// the last one anchors on.
fn two_exchanges(app: &mut App) -> ganja_protocol::MessageId {
    app.chat
        .start_message(Message::user("the question that stands"));
    let mut first = Message::assistant("canned");
    first.parts.push(Part::text("the answer that stands"));
    app.chat.start_message(first);

    let taken_back = Message::user("the question that is taken back");
    let anchor = taken_back.id.clone();
    app.chat.start_message(taken_back);
    let mut second = Message::assistant("canned");
    second
        .parts
        .push(Part::text("the answer that goes with it"));
    app.chat.start_message(second);

    anchor
}

/// The event the engine sends after an undo.
fn reverted(anchor: &ganja_protocol::MessageId, prompt: Option<&str>) -> AppEvent {
    AppEvent::core(CoreEvent::RevertChanged {
        session_id: session(),
        revert: Some(ganja_protocol::RevertInfo {
            message_id: anchor.clone(),
            files: vec!["src/lib.rs".to_owned()],
        }),
        prompt: prompt.map(str::to_owned),
    })
}

/// The event the engine sends when a revert ends, whichever way it ended.
fn unreverted() -> AppEvent {
    AppEvent::core(CoreEvent::RevertChanged {
        session_id: session(),
        revert: None,
        prompt: None,
    })
}

/// The transcript pane alone: everything above the composer's box.
///
/// A revert puts the prompt it took back into the composer, so a whole
/// screen holds that text whether or not the transcript still shows the
/// message — which is exactly the thing under test.
fn transcript_pane(terminal: &Terminal<TestBackend>) -> String {
    let whole = screen(terminal);

    whole
        .split_once("\u{250c} message")
        .map_or(whole.clone(), |(above, _)| above.to_owned())
}

/// **R10**, the whole of the TUI half in one pass: what an undo hid stops
/// being drawn, one row says how much and which files moved, and the
/// prompt it took back is offered again for editing.
#[tokio::test]
async fn an_undo_hides_what_it_took_back_shows_one_marker_row_and_refills_the_editor() {
    let mut app = app();
    let anchor = two_exchanges(&mut app);

    app.handle(reverted(&anchor, Some("the question that is taken back")))
        .await
        .expect("a revert is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let pane = transcript_pane(&terminal);

    assert!(pane.contains("the question that stands"), "{pane}");
    assert!(
        !pane.contains("the question that is taken back"),
        "the anchor is hidden with everything after it:\n{pane}"
    );
    assert!(!pane.contains("the answer that goes with it"), "{pane}");
    assert!(
        pane.contains("2 messages reverted \u{2014} /redo to restore"),
        "{pane}"
    );
    assert!(pane.contains("src/lib.rs"), "{pane}");
    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("the question that is taken back"),
        "undoing and retyping a prompt is editing it"
    );
}

/// The disambiguation the engine deliberately leaves to the frontend: the
/// same `revert: None` means two different things, and which one is
/// decided by the command this side last sent (**R10**).
#[tokio::test]
async fn a_redo_past_the_newest_reverted_prompt_puts_those_messages_back() {
    let mut app = app();
    let anchor = two_exchanges(&mut app);
    app.handle(reverted(&anchor, Some("the question that is taken back")))
        .await
        .expect("a revert is handled");

    app.run_command(command::Action::Redo).await;
    app.handle(unreverted())
        .await
        .expect("a cleared revert is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let pane = transcript_pane(&terminal);

    assert!(
        pane.contains("the question that is taken back"),
        "a redo past the newest one restores them:\n{pane}"
    );
    assert!(pane.contains("the answer that goes with it"), "{pane}");
    assert!(!pane.contains("reverted"), "{pane}");
}

/// The other reading of the same event: the engine has just deleted those
/// messages from history and from storage, so nothing is coming back and
/// leaving them on screen would be showing a conversation that no longer
/// exists.
#[tokio::test]
async fn a_prompt_after_an_undo_drops_the_messages_it_hid_for_good() {
    let (mut app, _events) = wired().await;
    let anchor = two_exchanges(&mut app);
    app.handle(reverted(&anchor, Some("the question that is taken back")))
        .await
        .expect("a revert is handled");

    // Through the editor, so what marks the clear as permanent is the same
    // submit path a person takes.
    app.editor.set_text("a different question");
    app.submit().await;
    app.handle(unreverted())
        .await
        .expect("a cleared revert is handled");
    // A second `/redo` has nothing left to show.
    app.run_command(command::Action::Redo).await;
    app.handle(unreverted())
        .await
        .expect("a cleared revert is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let pane = transcript_pane(&terminal);

    assert!(pane.contains("the question that stands"), "{pane}");
    assert!(
        !pane.contains("the question that is taken back"),
        "a prompt after an undo makes it permanent:\n{pane}"
    );
}

/// A refused prompt truncated nothing, so it must not leave this side
/// believing the next cleared revert is a deletion — which would drop
/// messages the engine still holds, and no later event could put back.
#[tokio::test]
async fn a_refused_prompt_leaves_the_revert_still_undoable() {
    let (mut app, mut events) = wired().await;
    let anchor = two_exchanges(&mut app);
    app.handle(reverted(&anchor, None))
        .await
        .expect("a revert is handled");
    assert_eq!(app.cleared, Cleared::Unhide);

    // A turn already streaming, which is what refuses the prompt below —
    // and, being accepted itself, what makes this undo permanent.
    app.editor.set_text("the turn that is already running");
    app.submit().await;
    pump(&mut app, &mut events, 2).await;
    assert_eq!(
        app.cleared,
        Cleared::Drop,
        "an accepted prompt is the user keeping what the undo did"
    );

    // A redo puts the reading back...
    app.run_command(command::Action::Redo).await;
    assert_eq!(app.cleared, Cleared::Unhide);

    // ...and a prompt the engine refuses must not take it away again. The
    // refusal a *prompt* can still meet since steering landed is `Busy`:
    // the turn above is running while this side has not yet seen the event
    // that says so, which is exactly the race the fallback lane exists for
    // (**F4**).
    app.turn_running = false;
    app.editor.set_text("the prompt the engine will refuse");
    app.submit().await;

    assert!(
        app.editor.is_empty() && app.queue.depth() == 1,
        "the fixture only proves anything while the prompt really is refused"
    );
    assert_eq!(
        app.cleared,
        Cleared::Unhide,
        "a refusal must put the reading back"
    );
}

/// **Resume.** A frontend that has just started learns the hidden range
/// from this event and from nowhere else — and learns it without anything
/// arriving in the editor, because reopening a conversation is not the
/// moment to put words in somebody's.
#[tokio::test]
async fn a_resumed_session_reconstructs_the_hidden_range_without_refilling_the_editor() {
    let mut app = app();
    let anchor = two_exchanges(&mut app);
    app.editor.set_text("what was already being typed");

    app.handle(reverted(&anchor, None))
        .await
        .expect("a seeded revert is handled");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let pane = transcript_pane(&terminal);

    assert!(
        pane.contains("2 messages reverted \u{2014} /redo to restore"),
        "the marker is reconstructed from the event alone:\n{pane}"
    );
    assert!(!pane.contains("the question that is taken back"), "{pane}");
    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("what was already being typed"),
        "a resume leaves the composer alone"
    );
}

/// Both halves are engine commands with no key of their own (**D4**), so
/// the palette row *is* the way to them: this asserts the row reaches the
/// engine, by the refusal only `Command::Undo` can earn here.
#[tokio::test]
async fn the_palette_rows_send_undo_and_redo_to_the_engine() {
    for typed in ["undo", "redo"] {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing(typed) {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let mut terminal = terminal(80, 12);
        app.draw(&mut terminal).expect("a frame draws");

        assert!(
            screen(&terminal).contains("takes no snapshots"),
            "/{typed} should have reached an engine that cannot do it:\n{}",
            screen(&terminal)
        );
    }
}

/// The `/` menu is the second view of the same command set, and a person
/// who types `/undo` and presses Enter has named the same row.
#[tokio::test]
async fn the_command_menu_reaches_undo_too() {
    let mut app = app();
    for event in typing("/undo") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let mut terminal = terminal(80, 12);
    app.draw(&mut terminal).expect("a frame draws");

    assert!(
        screen(&terminal).contains("takes no snapshots"),
        "{}",
        screen(&terminal)
    );
    assert!(
        app.editor.is_empty(),
        "a UI command runs rather than being typed"
    );
}

#[test]
fn snapshot_reverted_transcript() {
    let mut app = app();
    let anchor = two_exchanges(&mut app);
    app.chat.revert(
        anchor,
        vec![
            "crates/ganja-tui/src/app.rs".to_owned(),
            "README.md".to_owned(),
        ],
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");

    insta::assert_snapshot!(screen(&terminal));
}

/// The W2 follow-up, at the size it was reported at: two command rows had
/// already pushed the `keys` section off a stock terminal and `/undo` and
/// `/redo` push it further, so the card scrolls (deviation:
/// help-card-scrolls) and every row is reachable with the arrow keys.
#[tokio::test]
async fn the_help_card_reaches_all_of_itself_on_a_stock_terminal() {
    let mut app = app();
    app.run_command(command::Action::Help).await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let opening = screen(&terminal);
    assert!(
        opening.contains("[up/down] scroll"),
        "the card does not fit, so it must say how to see the rest:\n{opening}"
    );
    assert!(
        !opening.contains("agent_cycle"),
        "the fixture only proves anything while the tail really is off screen:\n{opening}"
    );

    let mut seen = opening;
    for _ in 0..20 {
        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");
        app.draw(&mut terminal).expect("a frame draws");
        seen.push('\n');
        seen.push_str(&screen(&terminal));
    }

    assert!(app.help.is_some(), "scrolling must not close the card");
    for row in ["/undo", "/redo", "keys", "agent_cycle"] {
        assert!(seen.contains(row), "{row} should be reachable:\n{seen}");
    }
}

/// A modal owns the keyboard: the keys that scroll the card must not also
/// reach the composer beneath it.
#[tokio::test]
async fn scrolling_the_help_card_does_not_type_into_the_editor() {
    let mut app = app();
    app.run_command(command::Action::Help).await;

    for code in [KeyCode::Down, KeyCode::Char('j'), KeyCode::Char('k')] {
        app.handle(key(code, KeyModifiers::NONE))
            .await
            .expect("a key is handled");
    }

    assert!(app.help.is_some());
    assert!(app.editor.is_empty(), "nothing should have been typed");
}

/// A window with room for the whole card is still a window with room for
/// the whole card: the scrolling is what a clip costs, not a permanent
/// change of shape.
#[tokio::test]
async fn a_tall_terminal_shows_the_whole_help_card_at_once() {
    let mut app = app();
    app.run_command(command::Action::Help).await;

    // Taller than it once was, because the roster this card lists gained
    // `/team` (**D504**), then `/held` (**D524**), then `/rename`
    // (**D527**) — the card grows with the commands, which is what "the
    // whole card" means.
    let mut terminal = terminal(90, 43);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);

    for row in ["/undo", "/redo", "keys", "agent_cycle"] {
        assert!(screen.contains(row), "{row} should be listed:\n{screen}");
    }
    assert!(!screen.contains("[up/down] scroll"), "{screen}");
}

// ---- F4: steering and the queue behind it -----------------------------

/// Drives a real turn to a streaming state and hands back the stream, so a
/// steering test acts while the engine really is busy rather than while a
/// flag says it is.
async fn streaming() -> (App, BoxStream<'static, CoreEvent>) {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "the turn to steer").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    // Both envelopes: the second is the assistant's, which is what tells
    // this side a turn holds the engine.
    pump(&mut app, &mut events, 2).await;
    assert!(app.turn_running, "the fixture needs a turn in flight");

    (app, events)
}

/// Runs the rest of a turn's events through the app, so a test never
/// leaves one streaming behind it.
async fn finish(app: &mut App, events: &mut BoxStream<'static, CoreEvent>) {
    while let Some(event) = events.next().await {
        let finished = matches!(event, CoreEvent::MessageFinished { .. });
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
        if finished {
            return;
        }
    }
}

/// The strip exists to be emptied by the engine's own word, and by nothing
/// else: `SteerConsumed` naming an id is what retires that entry.
#[tokio::test]
async fn a_queued_entry_leaves_the_strip_when_the_engine_says_it_consumed_it() {
    let (mut app, mut events) = streaming().await;
    typed(&mut app, "one more thing").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert_eq!(app.queue.depth(), 1);

    let id = app.queue.entries()[0].id.clone();
    app.handle(AppEvent::core(CoreEvent::SteerConsumed {
        session_id: app.engine.session_id(),
        id,
    }))
    .await
    .expect("the event is handled");

    assert!(
        app.queue.is_empty(),
        "the engine took it, so the strip lets go"
    );
    assert!(!status_line(&mut app).contains("queued"));

    finish(&mut app, &mut events).await;
}

/// The whole path with nothing scripted in the middle: a real engine takes
/// the steer, drains it before the turn it would otherwise have ended,
/// announces it, and the strip empties on the engine's own word.
#[tokio::test]
async fn a_real_turn_takes_the_steer_and_the_strip_empties_on_its_own() {
    let (mut app, mut events) = streaming().await;
    typed(&mut app, "and one more thing").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert_eq!(app.queue.depth(), 1);

    let mut consumed = false;
    loop {
        let event = events.next().await.expect("the turn keeps reporting");
        let finished = matches!(event, CoreEvent::MessageFinished { .. });
        consumed |= matches!(event, CoreEvent::SteerConsumed { .. });
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
        if finished {
            break;
        }
    }

    assert!(consumed, "the running turn took the message");
    assert!(app.queue.is_empty(), "so the strip let go of it");

    let mut terminal = terminal(80, 20);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(
        screen.contains("and one more thing"),
        "and it is in the transcript, not on the strip:\n{screen}"
    );
    assert!(
        !screen.contains("press up to edit queued messages"),
        "the strip is gone:\n{screen}"
    );
}

/// An id nothing answers to is the withdrawal race, and is not an error:
/// the entry was already taken back and the message lands exactly once.
#[tokio::test]
async fn a_consumed_id_that_names_nothing_changes_nothing() {
    let (mut app, mut events) = streaming().await;
    typed(&mut app, "one more thing").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    app.handle(AppEvent::core(CoreEvent::SteerConsumed {
        session_id: app.engine.session_id(),
        id: "steer-nobody".to_owned(),
    }))
    .await
    .expect("the event is handled");

    assert_eq!(app.queue.depth(), 1, "an unrelated id retires nothing");

    finish(&mut app, &mut events).await;
}

/// **Acceptance 4, Up.** With something waiting and nothing typed, Up
/// takes the newest queued message back into the composer — and takes it
/// off the strip, which is the whole of "withdraw".
#[tokio::test]
async fn up_recalls_and_withdraws_the_newest_queued_message() {
    let (mut app, mut events) = streaming().await;
    for text in ["first correction", "second correction"] {
        typed(&mut app, text).await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
    }
    assert_eq!(app.queue.depth(), 2);

    app.handle(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .expect("up is handled");

    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("second correction"),
        "the newest entry comes back for editing"
    );
    assert_eq!(app.queue.depth(), 1, "and leaves the strip");

    finish(&mut app, &mut events).await;
}

/// The queue sits *in front of* the history: once the strip is empty the
/// same key walks remembered prompts exactly as it always did.
#[tokio::test]
async fn up_walks_the_history_again_once_the_strip_is_empty() {
    let directory = temporary();
    let mut app = app_with_history(&directory, &["an older prompt"]);

    app.handle(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .expect("up is handled");

    assert_eq!(app.editor.prompt().as_deref(), Some("an older prompt"));
}

/// **Acceptance 4, cancel.** The engine drains nothing on a cancel, so
/// every entry still on the strip becomes the fallback lane's — and the
/// lane sends it once the engine is idle.
#[tokio::test]
async fn an_unconsumed_steer_survives_a_cancelled_turn_into_the_fallback_lane() {
    let (mut app, mut events) = streaming().await;
    typed(&mut app, "the correction nobody took").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert!(app.queue.entries()[0].is_steered());

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    finish(&mut app, &mut events).await;

    // The finish stranded it and the same handler replayed it, so the
    // engine has a turn of its own for the message now.
    assert!(
        app.queue.is_empty(),
        "the lane owns it and has sent it: {:?}",
        app.queue.entries()
    );
    let started = events
        .next()
        .await
        .expect("the replay starts a turn of its own");
    let CoreEvent::MessageStarted { message, .. } = started else {
        panic!("a turn opens with the user's message");
    };
    assert_eq!(
        message
            .parts
            .iter()
            .filter_map(ganja_protocol::Part::as_text)
            .collect::<String>(),
        "the correction nobody took"
    );

    finish(&mut app, &mut events).await;
}

/// **Acceptance 4, the revert pause.** A replayed prompt would commit the
/// undo the user just made, so the lane holds while one is outstanding —
/// and the entry is still there, on screen, rather than quietly gone.
#[tokio::test]
async fn the_fallback_lane_pauses_while_a_revert_is_outstanding() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "the first turn").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    finish(&mut app, &mut events).await;

    // Refused `NotStreaming`, so the entry is the fallback lane's — the
    // one kind a replay could send, and the kind this test holds back.
    app.turn_running = true;
    typed(&mut app, "the queued correction").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    app.turn_running = false;
    assert_eq!(app.queue.depth(), 1);

    app.handle(AppEvent::core(CoreEvent::RevertChanged {
        session_id: app.engine.session_id(),
        revert: Some(ganja_protocol::RevertInfo {
            message_id: ganja_protocol::MessageId::from("msg_1".to_owned()),
            files: Vec::new(),
        }),
        prompt: None,
    }))
    .await
    .expect("the revert is handled");
    assert!(app.revert_pending);

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert_eq!(
        app.queue.depth(),
        1,
        "the entry waits for the person to decide about the revert"
    );
    assert!(status_line(&mut app).contains("1 queued"));

    // And once the revert is over, the same entry goes.
    app.handle(AppEvent::core(CoreEvent::RevertChanged {
        session_id: app.engine.session_id(),
        revert: None,
        prompt: None,
    }))
    .await
    .expect("the cleared revert is handled");
    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert!(app.queue.is_empty(), "the pause was a pause, not a drop");
    finish(&mut app, &mut events).await;
}

/// **Acceptance 4, the race towards idle.** A steer that arrives after the
/// turn ended is refused `NotStreaming`, joins the fallback lane, and is
/// replayed exactly once — no loss, no duplicate.
#[tokio::test]
async fn a_steer_that_loses_the_turn_is_refused_and_replayed_exactly_once() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "the first turn").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    finish(&mut app, &mut events).await;

    // The turn is over; this side has not noticed, which is the race.
    app.turn_running = true;
    typed(&mut app, "the message that lost the race").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.queue.depth(), 1);
    assert!(
        !app.queue.entries()[0].is_steered(),
        "a refused steer belongs to the fallback lane"
    );

    // The lane sends it on the next tick, and sends it once.
    app.turn_running = false;
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    assert!(app.queue.is_empty());

    let mut prompts = 0;
    loop {
        let event = events.next().await.expect("the replay runs a turn");
        let finished = matches!(event, CoreEvent::MessageFinished { .. });
        if let CoreEvent::MessageStarted { message, .. } = &event
            && message.role == ganja_protocol::Role::User
        {
            prompts += 1;
            assert_eq!(
                message
                    .parts
                    .iter()
                    .filter_map(ganja_protocol::Part::as_text)
                    .collect::<String>(),
                "the message that lost the race"
            );
        }
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
        if finished {
            break;
        }
    }
    assert_eq!(prompts, 1, "exactly once");

    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    assert!(
        app.queue.is_empty(),
        "and nothing is left to send a second time"
    );
}

/// **Acceptance 4, the Busy retry.** A replay that loses a race to a turn
/// starting underneath it keeps its place at the front and is tried again.
#[tokio::test]
async fn a_replay_that_meets_busy_keeps_its_place_and_is_retried() {
    let (mut app, mut events) = streaming().await;

    // Queued while a turn really is running, but with this side believing
    // it is idle: the send below meets `Busy` and the entry is what
    // survives it.
    app.turn_running = false;
    typed(&mut app, "the message that met busy").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.queue.depth(), 1, "the refusal cost nothing");
    assert!(app.editor.is_empty());

    // Another tick while the turn is still running: still refused, still
    // first in the queue.
    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    assert_eq!(app.queue.depth(), 1);

    // And once the turn ends, the same entry goes.
    finish(&mut app, &mut events).await;
    assert!(app.queue.is_empty(), "the retry took");

    finish(&mut app, &mut events).await;
}

/// **Acceptance 4, the slash split.** An engine command acts on the engine
/// between turns, so it never steers: it waits for the end of the turn.
#[tokio::test]
async fn an_engine_command_typed_mid_turn_waits_for_the_end_of_the_turn() {
    let (mut app, mut events) = streaming().await;

    // With an argument, so the inline command menu is closed and Enter is
    // the submit rather than the menu's own selection.
    typed(&mut app, "/init focus on the tests").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.queue.depth(), 1);
    assert!(
        !app.queue.entries()[0].is_steered(),
        "a command is not a message the model reads"
    );
    assert!(app.editor.is_empty());

    finish(&mut app, &mut events).await;
    assert!(app.queue.is_empty(), "and it runs when the turn is over");

    finish(&mut app, &mut events).await;
}

/// Shell mode keeps the refusal it always had: upstream's server refuses a
/// shell command while busy too, and steering does not change what a `!`
/// line is.
#[tokio::test]
async fn a_shell_submission_mid_turn_is_still_refused_and_never_queued() {
    let (mut app, mut events) = streaming().await;

    typed(&mut app, "!").await;
    typed(&mut app, "echo hello").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert_eq!(app.editor.prompt().as_deref(), Some("echo hello"));
    assert_eq!(app.editor.mode(), Mode::Shell);
    assert!(app.queue.is_empty(), "nothing steers a shell line");

    app.set_shell(false);
    app.editor.clear();
    finish(&mut app, &mut events).await;
}

/// Esc is the cancel and nothing else: a person stopping a turn has not
/// said anything about the messages they queued for it.
#[tokio::test]
async fn escape_does_not_clear_the_strip() {
    let (mut app, mut events) = streaming().await;
    typed(&mut app, "still wanted").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");

    assert_eq!(app.queue.depth(), 1, "Esc says nothing about the queue");

    finish(&mut app, &mut events).await;
}

/// The whole strip, as a person sees it: the waiting message, the hint
/// line under it, and the depth on the bar.
#[tokio::test]
async fn snapshot_queued_messages_strip() {
    let (mut app, mut events) = streaming().await;
    for text in ["run the tests too", "and then commit"] {
        typed(&mut app, text).await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
    }

    let mut terminal = terminal(80, 16);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));

    finish(&mut app, &mut events).await;
}

// ---- F7: the rewind picker --------------------------------------------

/// An app whose transcript holds two exchanges, which is two checkpoints
/// as far as the picker is concerned.
fn with_checkpoints() -> App {
    let mut app = app();
    two_exchanges(&mut app);

    app
}

/// Presses Esc `times` in a row, fast enough that the gesture's window
/// never closes between them.
async fn escapes(app: &mut App, times: usize) {
    for _ in 0..times {
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
    }
}

/// **Acceptance 7, the list.** `/rewind` lists exactly the session's user
/// messages plus the row for where it already stands.
#[tokio::test]
async fn slash_rewind_lists_the_prompts_the_transcript_holds_and_current() {
    let mut app = with_checkpoints();
    app.run_command(command::Action::Rewind).await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);

    assert!(screen.contains("(Current)"), "got:\n{screen}");
    assert!(
        screen.contains("the question that stands"),
        "got:\n{screen}"
    );
    assert!(
        screen.contains("the question that is taken back"),
        "got:\n{screen}"
    );
    assert!(
        !screen.contains("the answer that stands"),
        "a reply is not a checkpoint:\n{screen}"
    );
}

/// A prompt whose turn recorded patches says how many files it moved; one
/// whose turn recorded none says there is no code to put back.
#[tokio::test]
async fn a_checkpoint_says_whether_its_turn_changed_any_code() {
    let mut app = app();
    app.chat.start_message(Message::user("touch two files"));
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: ganja_protocol::PartId::from("prt_patch".to_owned()),
        body: PartBody::Patch {
            hash: "4b825dc".to_owned(),
            files: vec!["src/lib.rs".to_owned(), "src/app.rs".to_owned()],
        },
    });
    app.chat.start_message(reply);
    app.chat
        .start_message(Message::user("just tell me about it"));
    app.chat.start_message(Message::assistant("canned"));

    app.run_command(command::Action::Rewind).await;
    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);

    assert!(screen.contains("2 files changed"), "got:\n{screen}");
    assert!(
        screen.contains("\u{26a0} No code restore"),
        "a span with no patches says so:\n{screen}"
    );
}

/// **The gesture (D467).** Two Escs at an idle composer enter the
/// backtrack walk on the newest user message, and each further Esc steps
/// one older, holding at the oldest rather than wrapping.
#[tokio::test]
async fn esc_esc_highlights_the_newest_prompt_and_each_esc_steps_one_older() {
    let mut app = with_checkpoints();
    let newest = app.chat.checkpoints()[0].message_id.clone();
    let oldest = app.chat.checkpoints()[1].message_id.clone();

    escapes(&mut app, 1).await;
    assert!(app.backtrack.is_none(), "one Esc is still just a cancel");

    escapes(&mut app, 1).await;
    assert_eq!(
        app.chat.backtrack_anchor(),
        Some(&newest),
        "the second lands on the newest prompt"
    );
    assert!(
        app.rewind.is_none(),
        "the picker is /rewind's, not the gesture's"
    );

    escapes(&mut app, 1).await;
    assert_eq!(app.chat.backtrack_anchor(), Some(&oldest), "one step older");

    escapes(&mut app, 1).await;
    assert_eq!(
        app.chat.backtrack_anchor(),
        Some(&oldest),
        "the walk holds at the oldest"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains(BACKTRACK_HINT),
        "the status bar says what the walk's keys do"
    );
}

/// **The gesture's guard.** While a turn streams Esc is the cancel and
/// nothing else — and it forgets any first press, so a double-press racing
/// a turn's end cancels and then does nothing.
#[tokio::test]
async fn esc_esc_while_a_turn_streams_cancels_and_never_backtracks() {
    let (mut app, mut events) = streaming().await;

    escapes(&mut app, 2).await;
    assert!(
        app.backtrack.is_none(),
        "no walk starts over a turn the user is watching"
    );

    finish(&mut app, &mut events).await;
    assert!(!app.turn_running, "and the Esc really did cancel it");

    // The turn is over, so the gesture is armed again — and the press that
    // happened while it was streaming does not count towards it.
    escapes(&mut app, 1).await;
    assert!(app.backtrack.is_none(), "the streaming press was forgotten");
    escapes(&mut app, 1).await;
    assert!(app.backtrack.is_some());
}

/// Two Escs far enough apart are two cancels, not a gesture.
#[tokio::test]
async fn a_second_esc_after_the_window_has_closed_opens_nothing() {
    let mut app = with_checkpoints();

    escapes(&mut app, 1).await;
    app.last_esc = Instant::now().checked_sub(ESC_CHORD * 2);
    assert!(app.last_esc.is_some(), "the fixture needs a stale press");

    escapes(&mut app, 1).await;
    assert!(app.backtrack.is_none(), "the window had closed");
}

/// The gesture is a sequence: anything typed between the two presses ends
/// it, however fast the second one follows.
#[tokio::test]
async fn a_keystroke_between_the_two_escs_ends_the_gesture() {
    let mut app = with_checkpoints();

    escapes(&mut app, 1).await;
    typed(&mut app, "n").await;
    escapes(&mut app, 1).await;

    assert!(
        app.backtrack.is_none(),
        "that was a cancel, a letter, a cancel"
    );
}

/// A transcript with no user message gives the gesture nothing to walk.
#[tokio::test]
async fn esc_esc_over_an_empty_transcript_enters_nothing() {
    let mut app = app();

    escapes(&mut app, 2).await;

    assert!(app.backtrack.is_none(), "there is nothing to step through");
}

/// A key that is neither Esc nor Enter leaves the walk silently — nothing
/// reverted, highlight and hint down — and then lands where it would have
/// without it.
#[tokio::test]
async fn any_other_key_exits_the_walk_without_reverting_and_is_then_handled() {
    let mut app = with_checkpoints();
    escapes(&mut app, 2).await;
    assert!(app.backtrack.is_some(), "the fixture needs a live walk");

    typed(&mut app, "n").await;

    assert!(app.backtrack.is_none(), "the walk exits silently");
    assert!(
        app.chat.backtrack_anchor().is_none(),
        "the highlight is down"
    );
    assert!(!app.chat.is_reverted(), "and nothing was reverted");
    assert_eq!(app.editor.text(), "n", "the key was then handled as typing");

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        !screen(&terminal).contains(BACKTRACK_HINT),
        "the hint left with the walk"
    );
}

/// **Acceptance 1, the whole gesture.** Enter reverts the conversation to
/// before the highlighted prompt and the composer holds the *whole*
/// multi-line prompt — a prefill fed from the checkpoint roster's
/// render-clipped titles would drop the second line, which is the point.
#[tokio::test]
async fn backtrack_enter_reverts_and_hands_back_the_whole_multi_line_prompt() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "the first line").await;
    app.handle(key(KeyCode::Char('j'), KeyModifiers::CONTROL))
        .await
        .expect("the newline chord is handled");
    typed(&mut app, "and the second").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    finish(&mut app, &mut events).await;

    escapes(&mut app, 2).await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    pump(&mut app, &mut events, 1).await;

    assert!(app.backtrack.is_none(), "confirming closes the walk");
    assert!(app.chat.is_reverted(), "the engine hid the checkpoint");
    assert_eq!(
        app.editor.text(),
        "the first line\nand the second",
        "the whole prompt comes back, not its clipped first line"
    );
    assert!(
        !app.code_only_rewind,
        "a backtrack is conversation-only and never reads as a code rewind"
    );
}

/// The engine's answer to a backtrack names the prompt it took back:
/// `RevertChanged.prompt` is `Some`, which is what makes the existing
/// composer-prefill path the walk's one prefill mechanism.
#[tokio::test]
async fn a_backtrack_revert_announces_the_prompt_it_took_back() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "the prompt to step back to").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    finish(&mut app, &mut events).await;

    escapes(&mut app, 2).await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    let event = events.next().await.expect("the engine answers the revert");
    let CoreEvent::RevertChanged { prompt, .. } = &event else {
        panic!("expected RevertChanged, got {event:?}");
    };
    assert_eq!(
        prompt.as_deref(),
        Some("the prompt to step back to"),
        "the event itself carries the prompt for the composer"
    );
    app.handle(AppEvent::core(event))
        .await
        .expect("the revert is handled");
}

/// `/rewind` is untouched by the gesture's retargeting: it still opens the
/// Claude-style two-step picker, never the walk (**D467**'s split).
#[tokio::test]
async fn slash_rewind_still_opens_the_picker_and_never_the_walk() {
    let mut app = with_checkpoints();

    app.run_command(command::Action::Rewind).await;

    assert!(
        app.rewind.is_some(),
        "the two-step picker is /rewind's door"
    );
    assert!(app.backtrack.is_none(), "the walk is the gesture's alone");
}

/// A transcript already showing a revert offers the walk only what is
/// still visible — the roster it steps is the checkpoint roster, which
/// already leaves out what a standing revert hides.
#[tokio::test]
async fn a_walk_over_a_standing_revert_offers_only_visible_prompts() {
    let mut app = app();
    let anchor = two_exchanges(&mut app);
    app.handle(reverted(&anchor, None))
        .await
        .expect("a revert is handled");
    assert!(
        app.chat.is_reverted(),
        "the fixture needs a standing revert"
    );

    escapes(&mut app, 2).await;

    let backtrack = app.backtrack.as_ref().expect("the gesture still works");
    assert_eq!(
        backtrack.candidates.len(),
        1,
        "the hidden prompt is not on offer"
    );
    let visible = app.chat.checkpoints()[0].message_id.clone();
    assert_eq!(app.chat.backtrack_anchor(), Some(&visible));
}

/// **Acceptance 7, Esc.** Esc at either step changes nothing: no command
/// reaches the engine and the transcript is where it was.
#[tokio::test]
async fn esc_closes_the_picker_from_either_step_and_changes_nothing() {
    let mut app = with_checkpoints();

    app.run_command(command::Action::Rewind).await;
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.rewind.is_none());
    assert!(!app.chat.is_reverted());

    app.run_command(command::Action::Rewind).await;
    app.handle(key(KeyCode::Down, KeyModifiers::NONE))
        .await
        .expect("down is handled");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert!(
        app.rewind.as_ref().is_some_and(Rewind::is_choosing_scope),
        "the scope step should be showing"
    );

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.rewind.is_none(), "Esc leaves the scope step too");
    assert!(!app.chat.is_reverted(), "and nothing was reverted");
}

/// Enter on `(Current)` is a person deciding not to rewind: the picker
/// closes and nothing is sent.
#[tokio::test]
async fn enter_on_current_closes_the_picker_and_reverts_nothing() {
    let mut app = with_checkpoints();
    app.run_command(command::Action::Rewind).await;

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");

    assert!(app.rewind.is_none());
    assert!(!app.chat.is_reverted());
}

/// **Acceptance 7, the whole path.** A real turn, then the picker taken to
/// *Conversation only*: the engine hides the message and hands the prompt
/// back for editing, with the working tree never mentioned.
#[tokio::test]
async fn choosing_conversation_only_takes_the_prompt_back_through_the_engine() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "the prompt to rewind past").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    finish(&mut app, &mut events).await;

    app.run_command(command::Action::Rewind).await;
    // Off `(Current)`, onto the one checkpoint, into the scope step, then
    // down one row to "Conversation only".
    for code in [KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
        app.handle(key(code, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    pump(&mut app, &mut events, 1).await;

    assert!(app.rewind.is_none(), "choosing closes the picker");
    assert!(app.chat.is_reverted(), "the engine hid the checkpoint");
    assert_eq!(
        app.editor.prompt().as_deref(),
        Some("the prompt to rewind past"),
        "rewinding and retyping a prompt is editing it"
    );
    assert_eq!(
        app.cleared,
        Cleared::Unhide,
        "what this hid can still be stepped back through"
    );
}

/// **Acceptance 7, code only.** The engine announces the files it put back
/// while recording no revert, so this side names them and hides nothing —
/// and the fallback lane is not paused by a revert nobody is holding.
#[tokio::test]
async fn a_code_only_rewind_names_the_files_and_hides_nothing() {
    let mut app = with_checkpoints();
    let anchor = two_exchanges(&mut app);
    // What `App::rewind_to` sets when it sends `RevertScope::Files`; the
    // engine's answer is indistinguishable from any other revert's, and
    // this is the flag that tells them apart (**R10**).
    app.code_only_rewind = true;

    app.handle(reverted(&anchor, None))
        .await
        .expect("a revert is handled");

    assert!(
        !app.chat.is_reverted(),
        "a code-only rewind hides no message"
    );
    assert!(
        !app.revert_pending,
        "and holds nothing the fallback lane has to wait on"
    );
    assert!(
        app.editor.is_empty(),
        "nothing was taken back, so nothing is offered again"
    );

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("restored 1 file"), "got:\n{screen}");
    assert!(screen.contains("src/lib.rs"), "got:\n{screen}");
}

/// The flag is consumed by the one event it answers: a code-only rewind
/// followed by an ordinary undo must not leave the undo unrendered.
#[tokio::test]
async fn the_code_only_reading_lasts_exactly_one_event() {
    let mut app = with_checkpoints();
    let anchor = two_exchanges(&mut app);
    app.code_only_rewind = true;
    app.handle(reverted(&anchor, None))
        .await
        .expect("the code-only answer is handled");

    app.handle(reverted(&anchor, None))
        .await
        .expect("an ordinary revert is handled");

    assert!(app.chat.is_reverted(), "the second one is an ordinary undo");
    assert!(app.revert_pending, "and the lane pauses for it");
}

/// A session that takes no snapshots cannot put files back, and says so
/// rather than half-rewinding.
#[tokio::test]
async fn a_rewind_a_session_cannot_serve_says_so_in_the_status_bar() {
    let mut app = with_checkpoints();
    let anchor = two_exchanges(&mut app);

    app.rewind_to(anchor, RevertScope::Files).await;

    assert!(
        !app.code_only_rewind,
        "a refused rewind leaves no reading behind for an event that will not come"
    );

    let mut terminal = terminal(100, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("takes no snapshots"), "got:\n{screen}");
}

/// **Acceptance 7, the refusal.** An id that is not a checkpoint is named
/// back rather than resolved to the nearest thing that is.
#[tokio::test]
async fn a_rewind_to_something_that_is_not_a_checkpoint_is_refused_by_name() {
    let (mut app, mut events) = wired().await;
    typed(&mut app, "the only prompt there is").await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    finish(&mut app, &mut events).await;

    app.rewind_to(
        MessageId::from("msg_nobody".to_owned()),
        RevertScope::Conversation,
    )
    .await;

    let mut terminal = terminal(100, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("no checkpoint named"), "got:\n{screen}");
    assert!(screen.contains("msg_nobody"), "got:\n{screen}");
    assert!(!app.chat.is_reverted(), "and nothing moved");
}

#[tokio::test]
async fn snapshot_rewind_picker_open() {
    let mut app = with_checkpoints();
    app.run_command(command::Action::Rewind).await;

    let mut terminal = terminal(80, 20);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));
}

#[tokio::test]
async fn snapshot_rewind_scope_choice() {
    let mut app = with_checkpoints();
    app.run_command(command::Action::Rewind).await;
    for code in [KeyCode::Down, KeyCode::Enter] {
        app.handle(key(code, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }

    let mut terminal = terminal(80, 20);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));
}

// ---- D474: the `/plugin` dialog ----

/// Writes `text` to `root/relative`, creating directories as needed.
fn plant(root: &std::path::Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

/// A store at its own temporary root, holding one installed plugin —
/// `formatter` from `company-tools`, carrying one hook and a skills
/// directory — built through the store's own doors, never by hand.
fn plugin_store_fixture(directory: &TempDir) -> ganja_core::plugin::Store {
    let market = directory.path().join("market");
    plant(
        &market,
        ".claude-plugin/marketplace.json",
        r#"{
              "name": "company-tools",
              "owner": { "name": "DevTools" },
              "plugins": [{ "name": "formatter", "source": "./plugins/formatter" }]
            }"#,
    );
    plant(
        &market,
        "plugins/formatter/hooks/hooks.json",
        r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "true"}]}]}}"#,
    );
    plant(&market, "plugins/formatter/skills/fmt/SKILL.md", "# fmt\n");

    let store = ganja_core::plugin::Store::at(directory.path().join("plugin-store"));
    store
        .add_marketplace(market.to_str().expect("the fixture path is unicode"))
        .expect("the fixture marketplace adds");
    store
        .install("formatter", "company-tools")
        .expect("the fixture plugin installs");

    store
}

/// Ticks until the `/plugin` store action running off the loop has been
/// reaped and its outcome is on the dialog.
///
/// What the real loop does on its own — [`App::wants_wakeup`] keeps it
/// ticking while the slot is full — driven by hand, because a test owns
/// its own clock. Every store action is one tick later than the keypress
/// that asked for it now (`zus`), which is the whole point: the loop
/// draws while a `git clone` runs.
async fn settle_plugin(app: &mut App) {
    for _ in 0..600 {
        if app.plugin_task.is_none() {
            return;
        }
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("the store action never finished");
}

/// A blocking task in the shape of a slow clone: it parks for `millis`
/// and answers the way [`super::run_store_effect`] would.
///
/// A real `git clone` finishes when the network says so, and a test that
/// raced one would pin timing rather than behavior. This is a real
/// blocking task in the real slot, which is what the loop's arrangement
/// is actually about.
fn parked_add(millis: u64) -> JoinHandle<String> {
    tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(millis));
        "added marketplace slow-market from /slow".to_owned()
    })
}

/// `/plugin` opens from the roster and Esc walks back out — from the
/// list, and from the per-plugin action step, the `/mcp` dialog's own
/// two-step Esc.
#[tokio::test]
async fn slash_plugin_opens_the_dialog_and_esc_walks_back_out() {
    let directory = temporary();
    let store = plugin_store_fixture(&directory);
    let mut app = app().with_plugin_store(store);
    assert_eq!(
        command::lookup("plugin").map(|entry| entry.action),
        Some(command::Action::Plugin),
        "/plugin is on the roster"
    );

    app.run_command(command::Action::Plugin).await;
    assert!(app.plugin_dialog.is_some(), "/plugin opens the dialog");

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.plugin_dialog.is_none(), "escape closes the list step");

    app.run_command(command::Action::Plugin).await;
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    assert!(
        app.plugin_dialog
            .as_ref()
            .is_some_and(component::plugin::Plugin::is_choosing_action),
        "enter on a plugin row opens its actions"
    );
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("escape is handled");
    assert!(app.plugin_dialog.is_none(), "escape closes the action step");
}

/// **AC6's agreement half**: the dialog's rows are the store's own
/// listing — the same `Store::list` the `ganja plugin` CLI prints —
/// field for field, summary included.
#[tokio::test]
async fn the_plugin_dialog_lists_what_the_store_holds_and_agrees_with_the_collector() {
    let directory = temporary();
    let store = plugin_store_fixture(&directory);
    let listings = store.list().expect("the fixture store lists");
    let mut app = app().with_plugin_store(store);

    let (rows, complaint) = app.plugin_rows();
    assert_eq!(complaint, None);
    assert_eq!(rows.len(), listings.len());
    for (row, listing) in rows.iter().zip(&listings) {
        assert_eq!(row.name, listing.name);
        assert_eq!(row.enabled, listing.enabled);
        assert_eq!(row.marketplace, listing.marketplace);
        assert_eq!(
            row.summary,
            component::plugin::summarize(&listing.components),
            "the summary is computed from the collector's own component lines"
        );
    }

    app.run_command(command::Action::Plugin).await;
    let mut terminal = terminal(90, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("formatter"), "got:\n{screen}");
    assert!(screen.contains("Enabled"), "got:\n{screen}");
    assert!(screen.contains("company-tools"), "got:\n{screen}");
    assert!(
        screen.contains("1 hook \u{b7} skills"),
        "the hook and the skills directory both count:\n{screen}"
    );
}

/// Disable, enable and remove round-trip through the dialog: each lands
/// in `plugins.json`, and the rows repaint from the store rather than
/// from a tally of their own. A disabled plugin's row shows Disabled.
#[tokio::test]
async fn enable_disable_and_remove_round_trip_through_the_dialog() {
    let directory = temporary();
    let store = plugin_store_fixture(&directory);
    let mut app = app().with_plugin_store(store.clone());
    app.run_command(command::Action::Plugin).await;

    // Enter opens the row's actions; Enter again runs the toggle, which
    // reads Disable on an enabled row.
    for _ in 0..2 {
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    settle_plugin(&mut app).await;
    assert!(
        !store.state().expect("the state reads").plugins["formatter"].enabled,
        "the dialog's Disable landed in plugins.json"
    );
    let mut terminal = terminal(90, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("Disabled"),
        "the row repaints disabled:\n{}",
        screen(&terminal)
    );

    // The same toggle now reads Enable.
    for _ in 0..2 {
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    settle_plugin(&mut app).await;
    assert!(
        store.state().expect("the state reads").plugins["formatter"].enabled,
        "the dialog's Enable landed too"
    );

    // Remove is the action after the toggle.
    for code in [KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
        app.handle(key(code, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    settle_plugin(&mut app).await;
    assert!(
        store.state().expect("the state reads").plugins.is_empty(),
        "remove deletes the state entry"
    );
    assert!(
        !store.plugin_root("formatter").exists(),
        "and the installed directory with it"
    );
}

/// The free-text step is the frontend's own: Enter submits to the store,
/// Esc cancels the edit, and the engine hears nothing either way — no
/// event, no question, nothing in the composer.
#[tokio::test]
async fn the_add_input_submits_on_enter_and_cancels_on_esc_without_an_engine_command() {
    let directory = temporary();
    let market = directory.path().join("market");
    plant(
        &market,
        ".claude-plugin/marketplace.json",
        r#"{"name": "m", "owner": {"name": "o"}, "plugins": []}"#,
    );
    let store = ganja_core::plugin::Store::at(directory.path().join("plugin-store"));
    let mut app = app().with_plugin_store(store.clone());
    let mut events = app.engine.subscribe().await.expect("the first subscriber");

    app.run_command(command::Action::Plugin).await;
    // An empty store starts the cursor on "Add marketplace".
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    assert!(
        app.plugin_dialog
            .as_ref()
            .is_some_and(component::plugin::Plugin::is_typing),
        "add opens the free-text step"
    );

    // Esc cancels the edit and keeps the dialog open.
    app.handle(key(KeyCode::Char('x'), KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    assert!(
        app.plugin_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.is_typing()),
        "esc cancels the edit without closing the dialog"
    );

    // Enter with a real path submits to the store.
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    for character in market.to_str().expect("unicode").chars() {
        app.handle(key(KeyCode::Char(character), KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    settle_plugin(&mut app).await;
    assert!(
        store
            .state()
            .expect("the state reads")
            .marketplaces
            .contains_key("m"),
        "the typed marketplace was added"
    );

    assert!(
        app.editor.is_empty(),
        "nothing typed at the dialog reaches the composer"
    );
    assert!(app.question.is_none(), "and no question round trip exists");
    assert!(
        events.next().now_or_never().is_none(),
        "the engine heard nothing from any of it"
    );
}

/// A marketplace add that fails surfaces the refusal in the dialog —
/// including a clone's, whose message carries git's own captured stderr.
#[tokio::test]
async fn a_failed_marketplace_add_surfaces_the_captured_error_in_the_dialog() {
    let directory = temporary();
    let store = ganja_core::plugin::Store::at(directory.path().join("plugin-store"));
    let mut app = app().with_plugin_store(store);
    app.run_command(command::Action::Plugin).await;

    // `.git` routes through a real `git clone`, which fails and is
    // captured; the notice is the error's own Display, stderr included.
    let missing = directory.path().join("nowhere.git");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    for character in missing.to_str().expect("unicode").chars() {
        app.handle(key(KeyCode::Char(character), KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    settle_plugin(&mut app).await;

    assert!(app.plugin_dialog.is_some(), "the dialog stays open");
    let mut terminal = terminal(100, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("git clone failed"),
        "the captured failure is on the notice line:\n{}",
        screen(&terminal)
    );
}

/// **`zus`**: the event loop answers keys and draws frames *while* a
/// marketplace add runs, rather than freezing for the clone's duration.
///
/// The proof is the parked task itself: the keys and the frame are
/// handled, and only then is the task asked whether it has finished. If
/// the loop had waited for it — the shape this test replaced — it could
/// not have answered them before the task was done.
#[tokio::test]
async fn the_loop_answers_keys_while_a_marketplace_add_is_running() {
    let directory = temporary();
    let store = plugin_store_fixture(&directory);
    let mut app = app().with_plugin_store(store);
    app.run_command(command::Action::Plugin).await;

    app.plugin_task = Some(parked_add(500));
    app.plugin_dialog
        .as_mut()
        .expect("the dialog is open")
        .set_busy(true);

    for code in [KeyCode::Down, KeyCode::Up, KeyCode::Down] {
        app.handle(key(code, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    let mut terminal = terminal(90, 24);
    app.draw(&mut terminal).expect("a frame draws");

    assert!(
        !app.plugin_task
            .as_ref()
            .expect("the action still owns the lane")
            .is_finished(),
        "the keys and the frame were answered while the add ran, not after it"
    );
    assert!(
        app.plugin_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.selected_plugin().is_none()),
        "and the keys really moved the cursor: off the one plugin row"
    );

    settle_plugin(&mut app).await;
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("added marketplace slow-market"),
        "and the outcome lands on the same line one tick later:\n{}",
        screen(&terminal)
    );
    assert!(
        app.plugin_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.is_busy()),
        "the reap clears the running flag"
    );
}

/// One lane, so a second store action while one runs is refused rather
/// than queued or raced — two writers over the same `plugins.json` is
/// the outcome nobody asked for. The dialog refuses its own two
/// store-writing actions before they open; the app catches the rest.
#[tokio::test]
async fn a_second_store_action_during_a_clone_is_refused_with_a_notice() {
    let directory = temporary();
    let store = plugin_store_fixture(&directory);
    let mut app = app().with_plugin_store(store.clone());
    app.run_command(command::Action::Plugin).await;

    app.plugin_task = Some(parked_add(400));
    app.plugin_dialog
        .as_mut()
        .expect("the dialog is open")
        .set_busy(true);

    // Down off the one plugin row lands on Add marketplace; Enter would
    // open its input step were an add not already running.
    for code in [KeyCode::Down, KeyCode::Enter] {
        app.handle(key(code, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    assert!(
        app.plugin_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.is_typing()),
        "the second add never opens its input"
    );
    let mut terminal = terminal(100, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("already running"),
        "and the refusal is on the notice line:\n{}",
        screen(&terminal)
    );

    // The app's own backstop, for what the dialog does not refuse itself:
    // a row's Remove chosen while the add runs.
    app.run_plugin_effect(component::plugin::Effect::Remove("formatter".to_owned()));
    assert!(
        store.plugin_root("formatter").exists(),
        "the refused remove ran nothing"
    );
    assert!(
        !app.plugin_task
            .as_ref()
            .expect("the first action still owns the lane")
            .is_finished(),
        "and nothing was spawned beside it"
    );

    // A dialog closed and reopened while the same action runs is told the
    // lane is still busy, rather than offering an add it would refuse.
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    app.run_command(command::Action::Plugin).await;
    assert!(
        app.plugin_dialog
            .as_ref()
            .is_some_and(component::plugin::Plugin::is_busy),
        "the reopened dialog inherits the running action"
    );

    settle_plugin(&mut app).await;
    assert!(
        app.plugin_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.is_busy()),
        "the lane frees when the first action lands"
    );
}

/// P21 pre-mortem 3: closing the dialog while a marketplace add runs
/// leaves no panic and nothing half-added.
///
/// The reap only happens on a tick, and the only ticks here come after
/// the Esc — so the answer provably arrives with no dialog to put it on,
/// which is the case the delivery had to survive. What keeps the store
/// clean is its own stage-validate-move, unchanged: a failed clone leaves
/// no marketplace, and no staging directory either.
#[tokio::test]
async fn closing_the_dialog_mid_clone_leaves_no_panic_and_no_half_add() {
    let directory = temporary();
    let root = directory.path().join("plugin-store");
    let store = ganja_core::plugin::Store::at(root.clone());
    let mut app = app().with_plugin_store(store.clone());
    app.run_command(command::Action::Plugin).await;

    // An empty store starts the cursor on "Add marketplace"; `.git`
    // routes through a real `git clone`, which is the slow one.
    let missing = directory.path().join("nowhere.git");
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    for character in missing.to_str().expect("unicode").chars() {
        app.handle(key(KeyCode::Char(character), KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    assert!(app.plugin_task.is_some(), "the clone runs off the loop");
    // Nothing has ticked yet, so this frame is the one drawn *during* the
    // clone: it says what is running rather than showing a stale list.
    let mut terminal = terminal(100, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("adding marketplace from"),
        "the dialog says what is running:\n{}",
        screen(&terminal)
    );

    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    assert!(
        app.plugin_dialog.is_none(),
        "esc closes the dialog with the clone still in flight"
    );

    settle_plugin(&mut app).await;
    assert!(
        app.plugin_dialog.is_none(),
        "the landed answer reopens nothing"
    );
    assert!(
        store
            .state()
            .expect("the state reads")
            .marketplaces
            .is_empty(),
        "the failed clone added no marketplace"
    );
    let leftovers: Vec<String> = fs::read_dir(&root)
        .map(|entries| {
            entries
                .filter_map(|entry| {
                    let name = entry.ok()?.file_name().to_string_lossy().into_owned();
                    name.starts_with(".staging-").then_some(name)
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "and left no staging directory behind: {leftovers:?}"
    );
}

/// **D474 pinned**: the reload notice names exactly what rebuilt
/// in-session and what needs a restart — the honest split, verbatim.
#[test]
fn the_reload_notice_pins_the_honest_split() {
    assert_eq!(
        super::RELOAD_SPLIT,
        "reloaded now: hooks, skills \u{b7} restart required: agents, mcp, lsp"
    );
}

/// The dialog's list step, over two fixed rows and the three store
/// actions (screenshot: no reference available; house `/mcp` chrome).
#[tokio::test]
async fn snapshot_plugin_dialog_open() {
    let mut app = app();
    app.plugin_dialog = Some(component::plugin::Plugin::new(vec![
        component::plugin::Row {
            name: "formatter".to_owned(),
            enabled: true,
            marketplace: "company-tools".to_owned(),
            summary: "1 hook \u{b7} skills".to_owned(),
        },
        component::plugin::Row {
            name: "deployer".to_owned(),
            enabled: false,
            marketplace: "company-tools".to_owned(),
            summary: "1 mcp \u{b7} 1 agent".to_owned(),
        },
    ]));

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));
}

/// The same dialog one Enter later: the per-plugin action menu.
#[tokio::test]
async fn snapshot_plugin_action_menu() {
    let mut app = app();
    let mut dialog = component::plugin::Plugin::new(vec![component::plugin::Row {
        name: "formatter".to_owned(),
        enabled: true,
        marketplace: "company-tools".to_owned(),
        summary: "1 hook \u{b7} skills".to_owned(),
    }]);
    assert_eq!(dialog.submit(), None, "enter opens the action step");
    app.plugin_dialog = Some(dialog);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));
}

/// The lead side (**D503**): an app leading a real team over a store and a
/// teams root under `directory`, plus the stream its own loop would read.
///
/// The session id is §2.1's own example, so the team on disk is
/// `session-224cbeab` and a reader can find it by hand.
async fn leading(
    directory: &TempDir,
) -> (
    App,
    Arc<ganja_core::teammate::TeammateRegistry>,
    BoxStream<'static, CoreEvent>,
) {
    let registry = Arc::new(ganja_core::teammate::TeammateRegistry::for_session(
        directory.path(),
        "224cbeab-4e62-497c-aa8f-d05cc33ce7ba",
        directory.path(),
    ));
    let engine = Engine::persistent(
        Arc::new(FakeProvider::default()),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_teammates(Arc::clone(&registry));
    let events = engine.subscribe().await.expect("the test subscribes first");

    (App::new(engine, None, Themes::builtin()), registry, events)
}

/// Writes one plain message into the lead's own inbox, as a peer would.
fn peer_writes(registry: &ganja_core::teammate::TeammateRegistry, from: &str, text: &str) {
    crate::member::write(&registry.lead_inbox(), from, text);
}

/// Everything the stream will answer right now, without waiting.
fn drained(events: &mut BoxStream<'static, CoreEvent>) -> Vec<CoreEvent> {
    std::iter::from_fn(|| events.next().now_or_never().flatten()).collect()
}

/// Leaves `app` with a turn really streaming, so a delivery takes the
/// steer lane rather than being refused as `NotStreaming`.
async fn turn_in_flight(app: &mut App, events: &mut BoxStream<'static, CoreEvent>) {
    for event in typing("what is left") {
        app.handle(event).await.expect("typing is handled");
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("enter is handled");
    pump(app, events, 2).await;
    assert!(app.turn_running, "the fixture needs a turn in flight");
}

/// What is left in the lead's inbox.
fn still_owed(registry: &ganja_core::teammate::TeammateRegistry) -> usize {
    ganja_core::team::mailbox::read(&registry.lead_inbox())
        .expect("the inbox reads")
        .valid
        .len()
}

/// **AC-10's lead leg**, idle half: a teammate's message reaches the
/// conversation on the very next tick, and only then leaves the mailbox.
#[tokio::test]
async fn a_teammates_message_reaches_an_idle_conversation_and_only_then_leaves_the_mailbox() {
    let directory = temporary();
    let (mut app, registry, mut events) = leading(&directory).await;
    peer_writes(&registry, "w1", "the parser is done");
    assert_eq!(still_owed(&registry), 1);

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    // A **peer part**, not text (**D495**): the words are attributed on the
    // user message the turn starts from, and the request assembly is what
    // wraps them in §5.3's envelope. `as_text` is deliberately blind to it,
    // which is what keeps a teammate's sentence out of `/copy` and out of a
    // checkpoint title — so a test looking for text here would find nothing
    // even when delivery worked.
    let mut attributed = None;
    for event in drained(&mut events) {
        if let CoreEvent::MessageStarted { message, .. } = event {
            for part in &message.parts {
                if let PartBody::Peer { from, body, .. } = &part.body {
                    attributed = Some((from.clone(), body.clone()));
                }
                assert_eq!(
                    part.as_text(),
                    None,
                    "a peer's words are never this conversation's text"
                );
            }
        }
    }
    assert_eq!(
        attributed,
        Some(("w1".to_owned(), "the parser is done".to_owned())),
        "the message became a turn of the lead's own, and says who wrote it"
    );
    assert_eq!(
        still_owed(&registry),
        0,
        "a delivered message does not remain"
    );
}

/// **AC-10's lead leg**, the other half of the same rule: a control frame
/// is acted on and **never** queued, so nothing about it reaches the model
/// or the strip.
#[tokio::test]
async fn a_control_frame_from_a_teammate_never_reaches_the_strip_or_the_model() {
    let directory = temporary();
    let (mut app, registry, mut events) = leading(&directory).await;
    let approved =
        ganja_protocol::team::Frame::ShutdownApproved(ganja_protocol::team::ShutdownApproved {
            request_id: "req-1".to_owned(),
            from: "w1".to_owned(),
            timestamp: ganja_core::team::record::now_iso8601(),
            pane_id: None,
            backend_type: None,
        });
    crate::member::write_frame(&registry.lead_inbox(), "w1", &approved);

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert_eq!(app.queue.depth(), 0, "a control frame is not a message");
    assert!(
        !drained(&mut events)
            .iter()
            .any(|event| matches!(event, CoreEvent::MessageStarted { .. })),
        "and nothing about it is put to the model"
    );
    assert_eq!(still_owed(&registry), 0, "it was acted on and pruned");
}

/// **D503's split.** An `Acknowledged` peer's message is rendered pending
/// until the engine says it took it; a `FireAndForget` peer's is retired at
/// write time, because the acknowledgement it would wait for never comes.
#[tokio::test]
async fn the_strip_holds_an_acknowledged_peers_message_and_never_a_fire_and_forget_one() {
    let directory = temporary();
    let (mut app, _registry, mut events) = leading(&directory).await;
    turn_in_flight(&mut app, &mut events).await;

    assert!(
        app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
            "w1",
            "2026-08-17T00:00:00.000Z",
            "have a look at the parser",
            ganja_core::teammate::Delivery::Acknowledged,
        )])
        .await,
        "the engine took the steer"
    );
    assert_eq!(app.queue.depth(), 1, "pending until the turn consumes it");
    assert!(app.queue.entries()[0].is_steered());

    assert!(
        app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
            "claude-peer",
            "2026-08-17T00:00:01.000Z",
            "and one from a pane",
            ganja_core::teammate::Delivery::FireAndForget,
        )])
        .await
    );
    assert_eq!(
        app.queue.depth(),
        1,
        "sent at write time, so nothing new is pending"
    );
}

/// **§7-5, and the door it closes.** A teammate's message waits on the
/// same strip a typed one does, and Up must not lift it into the composer:
/// Enter there resolves `@` mentions, loads `$` skills and runs `/`
/// commands, and the person at this terminal consented to none of it. The
/// body is exactly the three tokens that would be acted on.
#[tokio::test]
async fn a_peers_message_cannot_be_recalled_into_the_composer() {
    let directory = temporary();
    let (mut app, _registry, mut events) = leading(&directory).await;
    turn_in_flight(&mut app, &mut events).await;
    app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
        "w1",
        "2026-08-17T00:00:00.000Z",
        "@Cargo.toml /init $skill",
        ganja_core::teammate::Delivery::Acknowledged,
    )])
    .await;
    assert_eq!(app.queue.depth(), 1);

    app.handle(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .expect("the arrow is handled");

    assert_eq!(
        app.editor.text(),
        "what is left",
        "the strip holds nothing this person wrote, so Up falls through to \
             the history walk exactly as an empty strip does — and what it \
             finds there is their own last prompt"
    );
    assert_eq!(app.queue.depth(), 1, "the peer's row is still waiting");

    // The person's own entry is still theirs to take back, from under the
    // peer's row.
    app.editor.set_text("");
    app.queue
        .push_steered("steer-99".to_owned(), "mine".to_owned());
    app.handle(key(KeyCode::Up, KeyModifiers::NONE))
        .await
        .expect("the arrow is handled");

    assert_eq!(app.editor.text(), "mine");
    assert_eq!(app.queue.depth(), 1, "and the peer's row stayed put");
}

/// **Delivery is not idempotent; only pruning is.** An `Acknowledged`
/// sender's message stays in the inbox until the turn provably took it, so
/// every pass in between offers it again — and a second pass that
/// *delivered* it again would put the same words to the model over and
/// over for the whole length of a long step.
///
/// The message is written into the mailbox and handed over by hand, since
/// only a member this registry really started reports `Acknowledged`, and
/// the identity §2.3 composes has to be the same one on both sides for the
/// pass to recognise what it is already holding.
#[tokio::test]
async fn a_message_still_awaiting_its_consumption_is_not_delivered_twice() {
    let directory = temporary();
    let (mut app, registry, mut events) = leading(&directory).await;
    turn_in_flight(&mut app, &mut events).await;
    let when = "2026-08-17T00:00:00.000Z";
    ganja_core::team::mailbox::write(
        &registry.lead_inbox(),
        ganja_core::team::MailboxMessage::new("w1", "the parser is done", when),
    )
    .expect("the lead's inbox takes a message");
    assert!(
        app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
            "w1",
            when,
            "the parser is done",
            ganja_core::teammate::Delivery::Acknowledged,
        )])
        .await
    );
    assert_eq!(app.steers, 1);
    assert_eq!(app.queue.depth(), 1);
    assert_eq!(
        still_owed(&registry),
        1,
        "the mailbox keeps it until the turn says it took it, which is \
             exactly what makes the next pass offer it again"
    );

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert_eq!(app.steers, 1, "the same message is not steered twice");
    assert_eq!(app.queue.depth(), 1, "and the strip did not grow a copy");
}

/// **One command per pass.** The engine takes a batch of peers on one
/// `Steer`, and sending them one at a time made the second refuse: `Busy`
/// is what an engine answers between accepting a prompt and running the
/// turn it becomes, so the rest of a pass waited out another cadence for
/// nothing.
#[tokio::test]
async fn a_whole_pass_of_messages_crosses_as_one_command() {
    let directory = temporary();
    let (mut app, registry, mut events) = leading(&directory).await;
    turn_in_flight(&mut app, &mut events).await;
    peer_writes(&registry, "w1", "the parser is done");
    peer_writes(&registry, "w2", "and the lexer");
    peer_writes(&registry, "w1", "one more thing");

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert_eq!(app.steers, 1, "three messages, one command");
    assert_eq!(
        still_owed(&registry),
        0,
        "and one prune for the whole batch"
    );

    // The strip still renders *messages*, one row each, all standing for
    // the single steer that carried them — shown here with senders whose
    // backend acknowledges, since those are the rows that wait.
    app.deliver_peers(vec![
        ganja_core::teammate::lead_inbox::Delivered::new(
            "w1",
            "2026-08-17T00:00:00.000Z",
            "have a look at the parser",
            ganja_core::teammate::Delivery::Acknowledged,
        ),
        ganja_core::teammate::lead_inbox::Delivered::new(
            "w2",
            "2026-08-17T00:00:01.000Z",
            "and at the lexer",
            ganja_core::teammate::Delivery::Acknowledged,
        ),
    ])
    .await;

    assert_eq!(app.steers, 2, "one more command");
    assert_eq!(app.queue.depth(), 2, "and one row per message");
    assert_eq!(
        app.queue
            .entries()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "all under the one id that will retire them together"
    );
}

/// **D526**: one drain hands the model at most [`super::PEER_BATCH_CAP`]
/// messages; the remainder was never delivered, so the mailbox still
/// holds it and the next pass — the same `in_flight` filter the tick
/// runs — offers exactly the messages the cap cut off, oldest first.
#[tokio::test]
async fn a_backlog_past_the_cap_delivers_the_cap_now_and_the_rest_on_the_next_drain() {
    let backlog = || -> Vec<ganja_core::teammate::lead_inbox::Delivered> {
        (0..super::PEER_BATCH_CAP + 2)
            .map(|n| {
                ganja_core::teammate::lead_inbox::Delivered::new(
                    "w1",
                    format!("2026-08-17T00:00:{n:02}.000Z"),
                    format!("message {n}"),
                    ganja_core::teammate::Delivery::Acknowledged,
                )
            })
            .collect()
    };
    let directory = temporary();
    let (mut app, _registry, mut events) = leading(&directory).await;
    turn_in_flight(&mut app, &mut events).await;

    assert!(app.deliver_peers(backlog()).await);
    assert_eq!(
        app.queue.depth(),
        super::PEER_BATCH_CAP,
        "one drain hands over the cap and no more"
    );

    // The next pass, as the tick builds it: the same backlog read back
    // from the durable mailbox, minus what is already in flight.
    let leftover: Vec<ganja_core::teammate::lead_inbox::Delivered> = backlog()
        .into_iter()
        .filter(|message| !app.in_flight(message))
        .collect();
    assert_eq!(
        leftover
            .iter()
            .map(|message| message.body.as_str())
            .collect::<Vec<_>>(),
        ["message 8", "message 9"],
        "what the cap cut off is exactly what the next pass is offered"
    );

    assert!(app.deliver_peers(leftover).await);
    assert_eq!(
        app.queue.depth(),
        super::PEER_BATCH_CAP + 2,
        "and the second drain delivers the remainder"
    );
    assert_eq!(app.steers, 2, "each drain crossed as one command");
}

/// An idle lead is not a busy one: the only thing it is waiting for is a
/// file at §6's own thousand-millisecond cadence, so that is how long it
/// sleeps rather than sixty times a second forever.
#[tokio::test]
async fn an_idle_lead_sleeps_until_its_next_mailbox_pass_rather_than_at_frame_rate() {
    let directory = temporary();
    let (mut app, _registry, _events) = leading(&directory).await;

    app.handle(AppEvent::Tick).await.expect("a tick is handled");
    app.dirty = false;

    assert!(app.wants_wakeup(), "a lead still has to keep waking");
    assert!(
        app.until_next_wakeup() > FRAME,
        "but for the mailbox rather than for a frame: {:?}",
        app.until_next_wakeup()
    );

    // Anything that is really about to be drawn takes the frame clock back.
    app.open_team();
    assert!(app.until_next_wakeup() <= FRAME);
}

/// A steer the turn never took is **not** handed to the replay lane: that
/// lane resolves mentions, loads skills and matches command names, and a
/// peer's words consent to none of it (§7-5). It goes back to the mailbox
/// it was never pruned from.
#[tokio::test]
async fn a_peers_unconsumed_message_leaves_the_strip_rather_than_becoming_a_prompt() {
    let directory = temporary();
    let (mut app, _registry, mut events) = leading(&directory).await;
    turn_in_flight(&mut app, &mut events).await;
    app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
        "w1",
        "2026-08-17T00:00:00.000Z",
        "@Cargo.toml /init $skill",
        ganja_core::teammate::Delivery::Acknowledged,
    )])
    .await;
    assert_eq!(app.queue.depth(), 1);

    app.strand_peers();

    assert_eq!(app.queue.depth(), 0, "the strip entry is given back");
    assert!(
        !app.queue.has_fallback(),
        "and it is emphatically not the replay lane's to interpret"
    );
}

/// **D-5**: a teammate's dialog is shown through the machinery the
/// engine's own goes through, and answered back down the channel it
/// arrived on rather than as a command to an engine that holds nothing by
/// that id.
#[tokio::test]
async fn a_teammates_permission_dialog_is_shown_and_answered_on_its_own_channel() {
    let mut app = app();
    let (reply, answered) = tokio::sync::oneshot::channel();
    let id = PermissionId::ascending();
    app.raise_teammate_dialog(ganja_core::teammate::posture::Forwarded {
        teammate: "w1".to_owned(),
        request: CoreEvent::PermissionRequested {
            session_id: session(),
            id: id.clone(),
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            title: "rm -rf build".to_owned(),
            args: serde_json::json!({"command": "rm -rf build"}),
            directories: Vec::new(),
        },
        reply,
    });

    let dialog = app.permission.as_ref().expect("the dialog is on screen");
    assert_eq!(dialog.permission_id(), Some(&id));
    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(
        screen.contains("w1"),
        "whose call it is, is the thing this dialog has that the engine's own has not:\n{screen}"
    );

    app.handle(AppEvent::Term(TermEvent::Key(KeyEvent::new(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
    ))))
    .await
    .expect("the key is handled");

    assert_eq!(
        answered.await,
        Ok(PermissionReply::Once),
        "the answer goes back to the engine that is waiting on it"
    );
    assert!(app.permission.is_none(), "and the dialog is retired");
}

/// A yolo session stands in for the person on a teammate's dialog exactly
/// as it does for its own — and answers `Once`, never `Always`, because a
/// teammate's stored rules are not the lead's to write (**D479**).
#[tokio::test]
async fn a_yolo_lead_answers_its_teammates_dialogs_without_drawing_one() {
    let mut app = app().with_yolo(true);
    let (reply, answered) = tokio::sync::oneshot::channel();
    app.raise_teammate_dialog(ganja_core::teammate::posture::Forwarded {
        teammate: "w1".to_owned(),
        request: CoreEvent::PermissionRequested {
            session_id: session(),
            id: PermissionId::ascending(),
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            title: "cargo build".to_owned(),
            args: serde_json::json!({"command": "cargo build"}),
            directories: Vec::new(),
        },
        reply,
    });

    assert!(app.permission.is_none(), "nobody was going to be asked");
    assert_eq!(answered.await, Ok(PermissionReply::Once));
}

/// A session leading no team does none of this and pays nothing for it,
/// which is what keeps every other test in this file unchanged.
#[tokio::test]
async fn a_session_leading_no_team_polls_nothing_and_counts_nobody() {
    let mut app = app();

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert!(app.lead_inbox.is_none());
    assert!(app.teammate_dialogs.is_none());
    assert_eq!(app.teammates, 0);
    assert!(
        app.team_polled.is_none(),
        "there is no mailbox to have polled"
    );
    assert!(
        app.engine.teammates().is_none(),
        "leading no team is a different answer from leading an empty one"
    );
}

/// The frontend's own claim about the roster (**D504**): `open_team`
/// renders the lead as the dialog's first row, with an empty ring, and a
/// session with no teammates yet still has a team to show. The registry
/// invariant underneath — lead first, the only `is_lead` row — is core's
/// own to pin.
#[tokio::test]
async fn the_roster_a_team_dialog_renders_starts_as_the_lead_and_nobody_else() {
    let directory = temporary();
    let (mut app, _registry, _events) = leading(&directory).await;

    app.open_team();

    let dialog = app.team_dialog.as_ref().expect("the dialog opened");
    let lead = dialog.selected_member().expect("the cursor opens on a row");
    assert_eq!(lead.name, "team-lead");
    assert!(lead.is_lead);
    assert!(lead.recent.is_empty());
}

/// **Asking for the roster is what raises the dialog**, and nothing else
/// is. The palette's data-free action asks for it and gets it; a line that
/// asked for something else is answered where the person who typed it is
/// looking, with no overlay in front of the composer they are still at.
#[tokio::test]
async fn only_asking_for_the_roster_raises_the_team_dialog() {
    let directory = temporary();
    let (mut app, _registry, _events) = leading(&directory).await;

    app.run_command(command::Action::Team).await;
    assert!(
        app.team_dialog.is_some(),
        "the palette's door asks for the roster"
    );
    app.team_dialog = None;

    app.editor.set_text("/team wat");
    app.submit().await;

    assert!(
        app.team_dialog.is_none(),
        "a line that did not ask for the roster does not raise it"
    );
    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(
        screen.contains("wat"),
        "and the refusal is still said, on the bar instead:\n{screen}"
    );
    assert!(
        app.editor.prompt().is_none(),
        "and the line it came from is out of the composer"
    );

    // `/team list` is the typed spelling of the very same ask, so it
    // raises the dialog exactly as the palette's row does.
    app.editor.set_text("/team list");
    app.submit().await;
    assert!(app.team_dialog.is_some(), "the typed door onto the roster");
}

/// A session with nowhere to keep a team says so once, rather than opening
/// a dialog about nothing.
#[tokio::test]
async fn team_on_a_session_leading_none_refuses_readably_instead_of_opening() {
    let mut app = app();

    app.run_command(command::Action::Team).await;

    assert!(app.team_dialog.is_none());
    let mut terminal = terminal(100, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("leads no team"), "{screen}");
}

/// Every `/team` line with arguments joins the prompt history — accepted
/// or refused by the grammar — because the words leave the composer either
/// way and the history is where Up finds them again. A bare `/team` is the
/// palette's own door, like `/help`, and is remembered no more than those
/// are.
#[tokio::test]
async fn a_team_line_is_remembered_whatever_it_turned_out_to_mean() {
    let directory = temporary();
    let mut app = app_with_history(&directory, &[]);

    for line in [
        "/team spawn w1 --backend in-process explain this crate",
        "/team wat",
        "/team",
    ] {
        app.editor.set_text(line);
        app.submit().await;
        app.team_dialog = None;
        assert!(
            app.editor.prompt().is_none(),
            "{line}: the line left the composer"
        );
    }

    let remembered: Vec<String> = app
        .history
        .entries()
        .into_iter()
        .map(|recalled| recalled.prompt.input)
        .collect();
    assert_eq!(
        remembered,
        [
            "/team wat",
            "/team spawn w1 --backend in-process explain this crate",
        ],
        "newest first, every argument-bearing line as typed, the bare ask not"
    );
    assert_eq!(
        app.history
            .step(history::Direction::Older, "")
            .map(|recalled| recalled.input)
            .as_deref(),
        Some("/team wat"),
        "and Up brings the newest one back to fix"
    );
}

/// A spawn decided in the `/team` dialog is remembered as the composer
/// line it is equivalent to, so the two doors leave the same thing behind.
#[tokio::test]
async fn a_spawn_from_the_team_dialog_is_remembered_as_its_team_spawn_line() {
    let directory = temporary();
    let mut app = app_with_history(&directory, &[]);
    let Some(command::Team::Spawn(line)) =
        command::team("/team spawn w2 --backend in-process hold the fort")
    else {
        panic!("a /team spawn line parses");
    };

    app.run_team_effect(component::team::Effect::Spawn {
        request: component::team::spawn_request(&line),
        typed: "w2 --backend in-process hold the fort".to_owned(),
    })
    .await;

    assert_eq!(
        app.history
            .entries()
            .first()
            .map(|recalled| recalled.prompt.input.as_str()),
        Some("/team spawn w2 --backend in-process hold the fort")
    );
}

/// One `/team` dialog row, as the fixture below hand-builds them.
fn team_row(
    name: &str,
    backend: ganja_protocol::MemberBackend,
    is_lead: bool,
    recent: &[&str],
) -> component::team::Row {
    component::team::Row {
        name: name.to_owned(),
        backend,
        is_lead,
        color: None,
        recent: recent.iter().map(|call| (*call).to_owned()).collect(),
    }
}

/// The `/team` dialog the tests below open: a lead and two members, one
/// with a ring — hand-built, the way the plugin snapshots build theirs, so
/// what is pinned is layout and key routing rather than a registry's
/// timing.
fn team_dialog() -> component::team::Team {
    component::team::Team::new(vec![
        team_row(
            "team-lead",
            ganja_protocol::MemberBackend::InProcess,
            true,
            &[],
        ),
        team_row(
            "w1",
            ganja_protocol::MemberBackend::InProcess,
            false,
            &["read(src/lib.rs)", "grep(fn spawn)"],
        ),
        team_row("w2", ganja_protocol::MemberBackend::Claude, false, &[]),
    ])
}

/// The `/team` dialog's members step: every member with its backend and
/// its ring, the lead marked, the Spawn row after them.
#[tokio::test]
async fn snapshot_team_dialog_open() {
    let mut app = app();
    app.team_dialog = Some(team_dialog());

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));
}

/// The `/team` dialog's per-member action step: whose actions these are,
/// then Message and Shutdown.
#[tokio::test]
async fn snapshot_team_action_menu() {
    let mut app = app();
    let mut dialog = team_dialog();
    dialog.move_selection(1);
    assert_eq!(dialog.submit(), None, "enter opens the action step");
    app.team_dialog = Some(dialog);

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    insta::assert_snapshot!(screen(&terminal));
}

/// Esc walks the `/team` dialog back out from every step: the free-text
/// step consumes the first press as "cancel the edit", and the other two
/// close the dialog — the `/mcp` and `/plugin` dialogs' own Esc.
#[tokio::test]
async fn esc_closes_the_team_dialog_from_either_step() {
    let mut app = app();

    app.team_dialog = Some(team_dialog());
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    assert!(app.team_dialog.is_none(), "Esc on the members step closes");

    let mut dialog = team_dialog();
    dialog.move_selection(1);
    dialog.submit();
    assert!(dialog.is_choosing_action());
    app.team_dialog = Some(dialog);
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    assert!(app.team_dialog.is_none(), "Esc on the action step closes");

    let mut dialog = team_dialog();
    dialog.move_selection(9);
    dialog.submit();
    assert!(dialog.is_typing());
    app.team_dialog = Some(dialog);
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    let open = app.team_dialog.as_ref().expect("the dialog stays open");
    assert!(!open.is_typing(), "the first Esc only abandons the edit");
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    assert!(app.team_dialog.is_none(), "and the second closes");
}

/// While the `/team` dialog is open it owns every key: a list-step press
/// moves the cursor or is swallowed, and the free-text step's characters
/// land in the dialog's own buffer — none of it reaches the composer.
#[tokio::test]
async fn keys_while_the_team_dialog_is_open_do_not_reach_the_editor() {
    let mut app = app();
    app.team_dialog = Some(team_dialog());

    for code in [
        KeyCode::Char('x'),
        KeyCode::Char('j'),
        KeyCode::Char('k'),
        KeyCode::Char('q'),
    ] {
        app.handle(key(code, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
    }
    assert_eq!(app.editor.text(), "", "nothing leaked past the dialog");
    assert!(
        app.team_dialog.is_some(),
        "and none of it closed the dialog"
    );

    let dialog = app.team_dialog.as_mut().expect("the dialog is open");
    dialog.move_selection(9);
    dialog.submit();
    for event in typing("w3") {
        app.handle(event).await.expect("typing is handled");
    }
    assert_eq!(
        app.team_dialog.as_ref().and_then(|dialog| dialog.input()),
        Some("w3"),
        "the characters landed in the dialog's buffer"
    );
    assert_eq!(app.editor.text(), "", "the composer never saw a character");
}

/// The free-text step's Enter runs the dialog's effect: a message typed at
/// a member goes through `run_team_effect`, and what the mailbox answered
/// lands on the dialog's own notice line.
#[tokio::test]
async fn enter_on_the_team_dialogs_input_step_runs_the_effect() {
    let directory = temporary();
    let (mut app, _registry, _events) = leading(&directory).await;
    // w1 is a row of the hand-built dialog and nobody in the registry's
    // roster, so the delivery below is refused by the roster — the answer
    // that proves the effect really ran.
    let mut dialog = team_dialog();
    dialog.move_selection(1);
    dialog.submit();
    app.team_dialog = Some(dialog);
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");
    assert!(
        app.team_dialog
            .as_ref()
            .is_some_and(component::team::Team::is_typing),
        "Message opens the free-text step"
    );
    for event in typing("status?") {
        app.handle(event).await.expect("typing is handled");
    }

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .expect("the key is handled");

    let mut terminal = terminal(90, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(
        screen.contains("nobody on this team answers to that name"),
        "{screen}"
    );
}

/// A spawn's own dialog is raised by the tick and answered on the channel
/// the asker is waiting on — the same machinery a teammate's call dialog
/// uses, on the other question (**D-5**).
#[tokio::test]
async fn a_spawns_own_dialog_is_raised_on_the_tick_and_answered_back_to_the_asker() {
    let directory = temporary();
    let (mut app, _registry, _events) = leading(&directory).await;
    let (reply, answered) = tokio::sync::oneshot::channel();
    app.spawn_asker
        .send((
            ganja_core::SpawnAsk {
                title: "start teammate w1 on the in-process backend".to_owned(),
                args: serde_json::json!({"name": "w1"}),
                directories: Vec::new(),
            },
            reply,
        ))
        .await
        .expect("the queue takes the question");

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert!(app.permission.is_some(), "somebody is being asked");
    app.handle(AppEvent::Term(TermEvent::Key(KeyEvent::new(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
    ))))
    .await
    .expect("the key is handled");

    assert_eq!(answered.await, Ok(PermissionReply::Once));
    assert!(app.permission.is_none());
}

/// Starts `w1` through the typed door and waits, off the loop, for the
/// registry to hold it — without ticking the app, so a test can watch what
/// the next tick does with the moved roster.
///
/// **The backend is named**, and naming it is Dv-1's own audit clause: an
/// absent `--backend` means `ganja` since that deviation, so this line
/// without one splits a real pane in whatever tmux session the developer
/// happens to be sitting in. What these tests mean is in-process semantics
/// — a member the registry holds, so the tick and the dialog have a roster
/// that moved — and that is now said rather than inherited from a default.
async fn registry_holds_w1(app: &mut App, registry: &ganja_core::teammate::TeammateRegistry) {
    app.run_team_line(command::team("/team spawn w1 --backend in-process").expect("a /team line"))
        .await;
    assert!(app.team_spawn.is_some(), "the spawn runs off the loop");
    for _ in 0..500 {
        if registry.view().members.len() == 2 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the registry never recorded the spawn");
}

/// An open `/team` dialog repaints when the roster really moved, and
/// leaves the frame alone when it did not — `poll_team_dialog`'s
/// changed-only rule.
#[tokio::test]
async fn an_open_team_dialog_repaints_only_when_the_roster_moved() {
    let directory = temporary();
    let (mut app, registry, _events) = leading(&directory).await;
    app.open_team();

    app.dirty = false;
    app.poll_team_dialog();
    assert!(!app.dirty, "a roster nobody touched is no reason to redraw");

    registry_holds_w1(&mut app, &registry).await;

    app.dirty = false;
    app.poll_team_dialog();
    assert!(app.dirty, "a roster that grew is");
    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    assert!(
        screen(&terminal).contains("w1"),
        "and the refreshed dialog shows the new row"
    );

    app.dirty = false;
    app.poll_team_dialog();
    assert!(!app.dirty, "a dialog already caught up stays quiet");
}

/// The in-process reap (**D504**): the tick reaps the finished spawn, the
/// outcome is said with Resolution 4's cleartext-path sentence, and the
/// bar counts the teammate.
///
/// Driven through the **typed** door, which raises no dialog, so what this
/// pins is the half of that change that had to hold: the sentence is the
/// same sentence, and the status bar carries the path the dialog used to.
#[tokio::test]
async fn a_team_spawn_is_reaped_by_the_tick_and_says_where_the_prompt_landed() {
    let directory = temporary();
    let (mut app, registry, _events) = leading(&directory).await;
    registry_holds_w1(&mut app, &registry).await;

    for _ in 0..500 {
        if app.team_spawn.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
    }

    assert!(app.team_spawn.is_none(), "the tick reaped the spawn");
    let mut terminal = terminal(90, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("w1 started"), "{screen}");
    assert!(
        screen.contains("prompt persisted in cleartext at"),
        "{screen}"
    );
    assert_eq!(app.teammates, 1, "the bar counts the started teammate");
}

/// A teammate that stopped waiting — its turn cancelled, its process gone
/// — does not wedge the queue: the answer is dropped, the next dialog is
/// asked, and nothing panics.
#[tokio::test]
async fn answering_a_dialog_whose_teammate_stopped_waiting_still_advances_the_queue() {
    let mut app = app();
    let asked = |name: &str, id: &PermissionId| ganja_core::teammate::posture::Forwarded {
        teammate: name.to_owned(),
        request: CoreEvent::PermissionRequested {
            session_id: session(),
            id: id.clone(),
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            title: "rm -rf build".to_owned(),
            args: serde_json::json!({"command": "rm -rf build"}),
            directories: Vec::new(),
        },
        reply: tokio::sync::oneshot::channel().0,
    };
    let first = PermissionId::ascending();
    let second = PermissionId::ascending();
    app.raise_teammate_dialog(asked("w1", &first));
    app.raise_teammate_dialog(asked("w2", &second));
    assert_eq!(app.queued_permissions.len(), 1, "the second is queued");

    assert!(
        app.answer_teammate_dialog(&first, PermissionReply::Once),
        "the id was known even though nobody is listening"
    );

    let on_screen = app.permission.as_ref().expect("the queue advanced");
    assert_eq!(on_screen.permission_id(), Some(&second));
    assert!(app.answer_teammate_dialog(&second, PermissionReply::Once));
    assert!(app.permission.is_none());
}

/// `/team shutdown` with nobody named is the whole team, and a team that is
/// only its lead is told so rather than silently doing nothing.
#[tokio::test]
async fn shutting_down_a_team_of_nobody_says_so_rather_than_writing_to_the_lead() {
    let directory = temporary();
    let (mut app, registry, _events) = leading(&directory).await;
    app.open_team();

    app.ask_whole_team_to_stop().await;

    let mut terminal = terminal(80, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("no teammates to stop"), "{screen}");
    assert_eq!(
        still_owed(&registry),
        0,
        "and the lead did not write a shutdown request to itself"
    );
}

/// A message typed at a member goes through the lead's own postbox, so an
/// unknown name is refused by the roster rather than written anywhere.
#[tokio::test]
async fn a_message_to_a_name_nobody_answers_to_is_refused_on_the_dialog() {
    let directory = temporary();
    let (mut app, _registry, _events) = leading(&directory).await;
    app.open_team();

    app.run_team_effect(component::team::Effect::Message {
        to: "nobody".to_owned(),
        text: "hello".to_owned(),
    })
    .await;

    let mut terminal = terminal(90, 24);
    app.draw(&mut terminal).expect("a frame draws");
    let screen = screen(&terminal);
    assert!(screen.contains("nobody"), "{screen}");
}

/// The member side (§10.3): an app running **as** a pane teammate `w1` of
/// the team a lead of §2.1's example session would lead, over a teams root
/// under `directory`, plus the stream its own loop would read.
///
/// No registry is installed and no team is led — a pane teammate is a
/// member of the one that launched it — so what this app has is exactly a
/// bare engine plus the member's inbox.
async fn membered(directory: &TempDir) -> (App, BoxStream<'static, CoreEvent>) {
    let membership = crate::member::membership(directory.path(), Some("%5"));
    let engine = Engine::persistent(
        Arc::new(FakeProvider::default()),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
        Storage::open(directory.path().join("storage")),
    );
    let events = engine.subscribe().await.expect("the test subscribes first");
    let app = App::new(engine, None, Themes::builtin())
        .with_member(crate::member::Inbox::new(membership));

    (app, events)
}

/// The member's own inbox and the lead's, as the app resolved them.
fn member_paths(app: &App) -> (std::path::PathBuf, std::path::PathBuf) {
    let membership = app
        .member
        .as_ref()
        .expect("the app is a member")
        .membership();

    (membership.inbox(), membership.lead_inbox())
}

/// Writes one plain message into the member's inbox, as the lead would —
/// which is also exactly what the spawn's inbox seed is.
fn lead_writes(inbox: &std::path::Path, text: &str) {
    crate::member::write(inbox, "team-lead", text);
}

/// What is left in a member-side inbox.
fn owed(inbox: &std::path::Path) -> usize {
    crate::member::held(inbox).len()
}

/// Every frame the lead's inbox holds, by kind, oldest first.
fn lead_heard(lead_inbox: &std::path::Path) -> Vec<ganja_protocol::team::Frame> {
    ganja_core::team::mailbox::read(lead_inbox)
        .expect("the lead's inbox reads")
        .valid
        .iter()
        .filter_map(ganja_core::team::MailboxMessage::frame)
        .collect()
}

/// The Peer part the turn's `MessageStarted` carried, if it carried one.
fn attributed_start(seen: &[CoreEvent]) -> Option<(String, String)> {
    seen.iter().find_map(|event| match event {
        CoreEvent::MessageStarted { message, .. } => {
            message.parts.iter().find_map(|part| match &part.body {
                PartBody::Peer { from, body, .. } => Some((from.clone(), body.clone())),
                _ => None,
            })
        }
        _ => None,
    })
}

/// **AC-10's pane leg, the drain and idle seams** (§10.3-1, -2, -3): the
/// prompt the lead seeded is the first turn, delivered as the lead's own
/// attributed words; the turn's end reaches the lead as
/// `idle_notification{available}` stamped with this member's name; and the
/// seed leaves the inbox only once the turn provably took it.
#[tokio::test]
async fn a_pane_teammates_seeded_prompt_is_its_first_turn_and_its_end_tells_the_lead() {
    let directory = temporary();
    let (mut app, mut events) = membered(&directory).await;
    let (inbox, lead_inbox) = member_paths(&app);
    lead_writes(&inbox, "start on the parser");

    // The first tick is due at once — nothing has polled yet.
    app.dirty = false;
    assert_eq!(app.until_next_wakeup(), Duration::ZERO);
    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert_eq!(
        owed(&inbox),
        0,
        "an accepted prompt is the turn, so the seed is delivered and gone"
    );
    assert!(
        lead_heard(&lead_inbox).is_empty(),
        "nothing said until the turn ends"
    );

    let seen = pump_turn(&mut app, &mut events).await;

    assert_eq!(
        attributed_start(&seen),
        Some(("team-lead".to_owned(), "start on the parser".to_owned())),
        "the seed became a turn of the member's own, attributed to the lead"
    );
    assert!(!app.turn_running);
    let heard = lead_heard(&lead_inbox);
    assert_eq!(heard.len(), 1, "one turn, one idle notification: {heard:?}");
    match &heard[0] {
        ganja_protocol::team::Frame::IdleNotification(idle) => {
            assert_eq!(idle.from, "w1");
            assert_eq!(
                idle.idle_reason,
                Some(ganja_protocol::team::IdleReason::Available)
            );
        }
        other => panic!("an idle_notification was expected, got {other:?}"),
    }
}

/// The same lane mid-turn (D-3, §10.3-1): a message the lead writes while
/// the member is working steers the running turn, waits pending on the
/// strip until the engine says it took it, and only then leaves the inbox.
#[tokio::test]
async fn a_leads_message_steers_a_pane_teammates_running_turn() {
    let directory = temporary();
    let (mut app, mut events) = membered(&directory).await;
    let (inbox, _lead_inbox) = member_paths(&app);
    turn_in_flight(&mut app, &mut events).await;
    lead_writes(&inbox, "and the lexer too");

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert_eq!(app.queue.depth(), 1, "pending until the turn consumes it");
    assert!(app.queue.entries()[0].is_steered());
    assert_eq!(owed(&inbox), 1, "durable until consumed");
    let id = app
        .peer_steers
        .keys()
        .next()
        .cloned()
        .expect("one batch in flight");

    app.handle(AppEvent::core(CoreEvent::SteerConsumed {
        session_id: session(),
        id,
    }))
    .await
    .expect("the event is handled");

    assert_eq!(app.queue.depth(), 0);
    assert_eq!(owed(&inbox), 0, "consumed means gone");
}

/// **AC-10's pane leg, the shutdown seam** (§10.3-4, §6.2): an idle member
/// answers a `shutdown_request` on the tick that reads it, quoting the
/// request id, and leaves through the loop's own exit. The pane and
/// backend fields the approval carries are `member.rs`'s own pin.
#[tokio::test]
async fn an_idle_pane_teammate_answers_a_shutdown_request_and_quits() {
    let directory = temporary();
    let (mut app, _events) = membered(&directory).await;
    let (inbox, lead_inbox) = member_paths(&app);
    crate::member::write_frame(
        &inbox,
        "team-lead",
        &crate::member::shutdown_request("req-1"),
    );

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert!(app.quit, "the request is the last thing this member reads");
    let heard = lead_heard(&lead_inbox);
    match heard.as_slice() {
        [ganja_protocol::team::Frame::ShutdownApproved(approved)] => {
            assert_eq!(approved.request_id, "req-1");
        }
        other => panic!("one shutdown_approved was expected, got {other:?}"),
    }
}

/// A shutdown that arrives mid-turn waits for the turn's end rather than
/// cutting it short (`Teammate::shutdown`'s courtesy), and then answers in
/// the order the lead expects: the turn's idle notification, then the
/// approval.
#[tokio::test]
async fn a_shutdown_request_during_a_turn_waits_for_the_turn_to_end() {
    let directory = temporary();
    let (mut app, mut events) = membered(&directory).await;
    let (inbox, lead_inbox) = member_paths(&app);
    turn_in_flight(&mut app, &mut events).await;
    crate::member::write_frame(
        &inbox,
        "team-lead",
        &crate::member::shutdown_request("req-2"),
    );

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    assert!(!app.quit, "the turn is still running");
    assert!(app.member_shutdown.is_some(), "and the request is held");
    assert!(lead_heard(&lead_inbox).is_empty(), "nothing answered yet");
    assert!(
        app.wants_frame(),
        "a held shutdown keeps the loop ticking so its bound is read"
    );

    pump_turn(&mut app, &mut events).await;

    assert!(
        app.quit,
        "the turn ended, so the request is answered and the app leaves"
    );
    let kinds: Vec<&str> = lead_heard(&lead_inbox)
        .iter()
        .map(ganja_protocol::team::Frame::kind)
        .collect();
    assert_eq!(kinds, ["idle_notification", "shutdown_approved"]);
}

/// The lead's `mode_set_request` reaches the engine as
/// `Command::SetPermissionMode` (D-15, AC-19's pane half): the engine
/// answers with `PermissionModeChanged`, and the frame is gone.
#[tokio::test]
async fn a_leads_mode_set_request_reaches_the_pane_teammates_engine() {
    let directory = temporary();
    let (mut app, mut events) = membered(&directory).await;
    let (inbox, _lead_inbox) = member_paths(&app);
    crate::member::write_frame(
        &inbox,
        "team-lead",
        &ganja_protocol::team::Frame::ModeSetRequest(ganja_protocol::team::ModeSetRequest {
            mode: "bypassPermissions".to_owned(),
            from: "team-lead".to_owned(),
        }),
    );

    app.handle(AppEvent::Tick).await.expect("a tick is handled");

    let changed = drained(&mut events)
        .into_iter()
        .find_map(|event| match event {
            CoreEvent::PermissionModeChanged { mode, .. } => Some(mode),
            _ => None,
        });
    assert_eq!(changed, Some(ganja_protocol::PermissionMode::Bypass));
    assert_eq!(
        owed(&inbox),
        0,
        "a frame acted on leaves the inbox in the same pass"
    );
}

/// A member wakes at its own cadence, which is the teammate's rather than
/// the lead's, and a session that is nobody's teammate wakes for neither.
#[test]
fn a_member_wakes_at_the_teammates_cadence() {
    let membership = crate::member::membership(std::path::Path::new("/nowhere"), None);
    let mut member = app().with_member(crate::member::Inbox::new(membership));
    // A fresh app wants its first frame; what is under test is the clock
    // an idle member falls back to once nothing is left to draw.
    member.dirty = false;

    assert!(member.wants_wakeup());
    assert_eq!(
        member.until_next_wakeup(),
        Duration::ZERO,
        "the first pass is due at once"
    );
    member.member_polled = Some(Instant::now());
    assert!(
        member.until_next_wakeup() > FRAME && member.until_next_wakeup() <= crate::member::POLL,
        "then the member's own clock, not the frame's: {:?}",
        member.until_next_wakeup()
    );
    let mut plain = app();
    plain.dirty = false;
    assert!(!plain.wants_wakeup(), "a plain session sleeps");
}

/// **AC-8's member half** (D-5): a pane teammate under `ForwardToLead`
/// raises no dialog of its own. The ask its rules raise travels to the
/// lead as §5's `permission_request`, the lead's `permission_response`
/// comes back through the member's inbox as `ReplyPermission::Once`, and
/// the call the ask was about actually runs.
///
/// A real engine and a real tool, for `shelling`'s reason: what has to be
/// shown is a turn that ran to its end with nobody at this terminal
/// answering anything.
#[tokio::test]
async fn a_pane_teammates_ask_travels_to_the_lead_and_the_answer_lets_the_call_run() {
    let directory = temporary();
    let script = directory.path().join("shell.json");
    fs::write(
        &script,
        format!(
            r#"{{
                    "cadence_ms": 0,
                    "turns": [
                        {{"tool_calls": [{{
                            "name": "bash",
                            "args": {{"command": "echo {ECHOED}"}}
                        }}]}},
                        {{"text": "{CLOSING}"}}
                    ]
                }}"#
        ),
    )
    .expect("the fake-provider script writes");
    let membership = crate::member::membership(directory.path(), None);
    let engine = Engine::new(
        Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script)),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(vec![Arc::new(
            ganja_tool::shell::ShellTool::new(),
        )])),
        ganja_permission::Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the test subscribes first");
    let mut app = App::new(engine, None, Themes::builtin())
        .with_member(crate::member::Inbox::new(membership));
    let (inbox, lead_inbox) = member_paths(&app);
    app.engine
        .send(ganja_protocol::Command::SendPrompt {
            text: "run it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts the prompt");

    let mut seen = Vec::new();
    let mut answered = false;
    for _ in 0..128 {
        let event = next_event(&mut events).await;
        let finished = matches!(event, CoreEvent::MessageFinished { .. });
        seen.push(event.clone());
        app.handle(AppEvent::core(event))
            .await
            .expect("an engine event is handled");
        assert!(
            app.permission.is_none() && app.queued_permissions.is_empty(),
            "a forwarding pane draws no dialog of its own"
        );
        if !answered
            && app
                .member
                .as_ref()
                .is_some_and(|inbox| inbox.asks().waiting() > 0)
        {
            // The ask reached the lead as a frame naming the engine's own
            // request id, and this is the lead answering it.
            let request = lead_heard(&lead_inbox)
                .into_iter()
                .find_map(|frame| match frame {
                    ganja_protocol::team::Frame::PermissionRequest(request) => Some(request),
                    _ => None,
                })
                .expect("the ask reached the lead");
            assert_eq!(request.tool_name, "bash");
            assert_eq!(request.agent_id, "w1@session-224cbeab");
            assert_eq!(
                request.request_id,
                seen.iter()
                    .find_map(|event| match event {
                        CoreEvent::PermissionRequested { id, .. } => Some(id.as_str().to_owned()),
                        _ => None,
                    })
                    .expect("the engine asked"),
                "the frame names the engine's own id"
            );
            // The lead's own encoder rather than a `PermissionResponse`
            // built here: a hand-built success body exercises the `Once`
            // spelling only, where `response_of` is the function a real
            // lead calls for all three replies and whose round trip with
            // `reply_of` core already pins.
            crate::member::write_frame(
                &inbox,
                "team-lead",
                &ganja_protocol::team::Frame::PermissionResponse(
                    ganja_core::teammate::member::response_of(
                        &request.request_id,
                        &request.tool_name,
                        &request.input,
                        ganja_protocol::PermissionReply::Once,
                    ),
                ),
            );
            app.handle(AppEvent::Tick).await.expect("a tick is handled");
            assert_eq!(
                app.member.as_ref().map(|inbox| inbox.asks().waiting()),
                Some(0),
                "the answer closed the ask"
            );
            answered = true;
        }
        if finished {
            break;
        }
    }

    assert!(answered, "the turn never asked: {seen:#?}");
    let replies: Vec<_> = seen
        .iter()
        .filter_map(|event| match event {
            CoreEvent::PermissionReplied { reply, .. } => Some(*reply),
            _ => None,
        })
        .collect();
    assert_eq!(
        replies,
        vec![PermissionReply::Once],
        "the lead's success is Once, never Always"
    );
    assert!(
        completed_shell(&seen).is_some_and(|output| output.contains(ECHOED)),
        "the call the ask was about actually ran: {seen:#?}"
    );
}
