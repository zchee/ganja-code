//! Drives the real binary through a pty: a fake turn streams into the
//! transcript, a scripted one runs a read, an edit and a shell command past
//! the permission dialog, and every exit path leaves the terminal restored.
//!
//! # What a pty test may assert on
//!
//! A terminal is only sent the cells that changed, so a string arrives whole
//! only when it was drawn over cells it differs from everywhere — in practice,
//! over blank ones. Anything drawn on top of other text comes back split
//! around the characters that happened to already match, which is why the
//! status bar is never waited for here, and why the scripted tests run in a
//! window tall enough that the centered permission dialog lands below the
//! transcript's last line rather than across it.
//!
//! So the screen is used for synchronization and for nothing else: waiting for
//! the dialog, and waiting for the turn to reach its closing word. What a
//! scripted run actually has to prove is read back off the filesystem — the
//! file the edit changed, the files the shell commands wrote, and the rules an
//! "always" answer stored.
//!
//! # Why the dialog is a safe thing to wait for
//!
//! A step's tool calls are resolved after the model's stream ends, so no
//! fragment of the reply can race the dialog open: the options line cannot be
//! drawn before the text ahead of it. The script's `cadence_ms` therefore only
//! decides how long a run takes, never what it proves — which is why these
//! wait for the options line rather than for a tool's name in the reply.
#![cfg(unix)]

use std::{
    fs,
    ops::{Deref, DerefMut},
    path::PathBuf,
    process::Command,
    thread,
    time::Duration,
};

use expectrl::{
    ControlCode, Eof, Expect as _, Session, process::unix::WaitStatus, session::OsSession,
};
use serde_json::json;
use tempfile::TempDir;

const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// The escape that opens the alternate screen. The app enables raw mode
/// before it emits this, so a pty that has seen it holds a process the line
/// discipline no longer speaks for — a keystroke sent earlier can be eaten by
/// cooked-mode buffering, and a Ctrl-C can be a SIGINT instead of a key. A
/// fixed grace period was the old guard here, and a loaded CI runner outran
/// it.
const ALT_SCREEN: &str = "\x1b[?1049h";

/// Width every pty here is opened at. Wide enough that no line these tests
/// wait for is wrapped.
const COLUMNS: u16 = 80;

/// Rows the unscripted tests run in: a stock terminal.
const SMOKE_ROWS: u16 = 24;

/// Rows the scripted tests run in.
///
/// The permission dialog is centered in the transcript pane, so the taller the
/// window, the further below the transcript's content it is drawn — and cells
/// nothing has drawn into are what let the options line reach the pty whole.
/// A stock twenty-four rows happens to work today; eighty is what stops that
/// from depending on how many lines a tool's preview or a reply's wording
/// takes up, which is not something a test about permissions should be
/// sensitive to.
const SCRIPTED_ROWS: u16 = 80;

/// The opening word of the fake provider's canned reply. It is the first thing
/// drawn for a turn, so it lands at the start of a line and is never split by
/// wrapping.
const REPLY_OPENING: &str = "Acknowledged";

/// The prompt to type. Typing draws it one cell at a time — the terminal only
/// receives the cells that changed — so this string only ever appears whole
/// when the transcript draws it into blank cells, which is what makes it an
/// assertion about the engine's user message rather than about the editor.
const PROMPT: &str = "kaleidoscope";

/// The permission dialog's options line, which is the one string on screen
/// that says a dialog — rather than a tool — is what the turn is waiting on.
/// Pinned to `ganja_tui::component::permission`.
const DIALOG_OPTIONS: &str = "[y] allow once   [a] always allow   [n]/[Esc] reject";

/// The last turn of every script: one word, so that a single streamed fragment
/// carries it, and one that appears nowhere else in the UI.
const CLOSING: &str = "script-finished-zarquon";

/// The file each scripted scenario works on.
const TARGET: &str = "target.txt";

/// Its seeded contents, minus the newline the file ends with. This is the text
/// an edit is scripted to replace, so it has to occur exactly once.
const SEEDED: &str = "seeded-original";

/// What an edit that ran replaced [`SEEDED`] with.
const REPLACEMENT: &str = "edited-replacement";

/// Where a scenario's script is written, inside the project directory.
const SCRIPT: &str = "script.json";

/// Where permission answers are stored, relative to the project's data
/// directory. Pinned to `ganja_permission::permission::FILE`.
const PERMISSIONS: &str = "permissions.json";

/// A `ganja` process in a pty, reaped however the test that owns it ends.
struct Ganja {
    /// Taken by [`Ganja::assert_clean_exit`]. One still here when the guard
    /// drops belongs to a test that failed part-way through, and it is holding
    /// a pty and a temporary directory that nothing else will free.
    session: Option<OsSession>,
    /// The isolated homes a bare spawn was given, held so they outlive the
    /// process. [`None`] when the caller isolated the command itself, the
    /// way [`scripted`] does with directories the test owns. Underscored:
    /// held for its `Drop` alone.
    _homes: Option<TempDir>,
}

impl Ganja {
    /// Spawns `command` in a pty `rows` tall, and waits for the app to take
    /// the terminal over.
    ///
    /// Every test in this file runs the built-in fake provider, so the
    /// selection is made here rather than by each caller: none of them may
    /// reach a network or read a credential.
    fn spawn(mut command: Command, rows: u16) -> Self {
        command.env("GANJA_PROVIDER", "fake");
        // A bare spawn gets its own throwaway homes: prompt history,
        // permission answers and spilled output all land under the data
        // home, and a run that does not redirect them types its test
        // prompts into the developer's real history (how `kaleidoscope`
        // and `ok[D` came to haunt one, 2026-08-24). A caller that set
        // `XDG_DATA_HOME` already isolated everything on its own terms.
        let mut homes = None;
        let isolated = command
            .get_envs()
            .any(|(key, _)| key == std::ffi::OsStr::new("XDG_DATA_HOME"));
        if !isolated {
            let dir = TempDir::new().expect("an isolated home is creatable");
            command.env("XDG_DATA_HOME", dir.path());
            command.env("XDG_CONFIG_HOME", dir.path().join("config"));
            command.env("HOME", dir.path());
            command.env_remove("GANJA_CONFIG_HOME");
            homes = Some(dir);
        }
        // The kitty keyboard probe (D517) blocks up to two seconds when
        // nothing answers its query, and this harness never does — except in
        // the probe drills below, which set the variable themselves and
        // answer the query by hand.
        let probing = command
            .get_envs()
            .any(|(key, _)| key == std::ffi::OsStr::new("GANJA_DISABLE_TERM_PROBE"));
        if !probing {
            command.env("GANJA_DISABLE_TERM_PROBE", "1");
        }

        let mut session = Session::spawn(command).expect("failed to spawn `ganja` in a pty");
        session.set_expect_timeout(Some(EXIT_DEADLINE));
        session
            .get_process_mut()
            .set_window_size(COLUMNS, rows)
            .expect("failed to size the pty");

        session
            .expect(ALT_SCREEN)
            .expect("`ganja` never took the terminal over");

        Self {
            session: Some(session),
            _homes: homes,
        }
    }

    /// Keeps the pty drained for `ms` milliseconds so the app stays live.
    ///
    /// A bare `thread::sleep` starves the pty: the app blocks writing its
    /// next frame into the kernel buffer nobody is reading, and never gets
    /// to *read* the input a test sent — so bytes meant to arrive split are
    /// read in one batch after all. Draining while waiting is what makes a
    /// deliberate pause on this side a real pause on the app's side.
    fn breathe(&mut self, ms: u64) {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        let mut sink = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            let _ = self.try_read(&mut sink);
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Like [`Ganja::breathe`], but keeps what it drained.
    fn harvest(&mut self, ms: u64) -> Vec<u8> {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            if let Ok(n) = self.try_read(&mut buf) {
                all.extend_from_slice(&buf[..n]);
            }
            thread::sleep(Duration::from_millis(2));
        }
        all
    }

    /// Waits for the process to end and checks that it ended cleanly.
    fn assert_clean_exit(mut self) {
        let mut session = self
            .session
            .take()
            .expect("a session is only ever taken once");

        session
            .expect(Eof)
            .expect("`ganja` did not exit within the deadline");

        let status = session
            .get_process()
            .wait()
            .expect("failed to reap the `ganja` process");
        assert!(
            matches!(status, WaitStatus::Exited(_, 0)),
            "expected a clean exit, got {status:?}"
        );
    }

    /// Quits with Ctrl-C and checks that the process ended cleanly.
    fn quit_and_assert_clean_exit(mut self) {
        self.send(ControlCode::EndOfText)
            .expect("failed to send Ctrl-C");

        self.assert_clean_exit();
    }
}

impl Deref for Ganja {
    type Target = OsSession;

    fn deref(&self) -> &Self::Target {
        self.session.as_ref().expect("the session outlives its use")
    }
}

impl DerefMut for Ganja {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.session.as_mut().expect("the session outlives its use")
    }
}

impl Drop for Ganja {
    fn drop(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };

        // Escalates through SIGHUP to SIGKILL and reaps what it stops. A test
        // that panicked mid-scenario left a child in raw mode on a pty, and
        // leaving it running would outlive the whole `cargo test` run.
        let _ = session.get_process_mut().exit(true);
    }
}

fn ganja() -> Ganja {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    // A script exported in the shell that runs the tests would otherwise
    // replace the canned reply these two are about.
    command.env_remove("GANJA_FAKE_SCRIPT");

    Ganja::spawn(command, SMOKE_ROWS)
}

#[test]
fn control_c_quits_the_tui_cleanly() {
    ganja().quit_and_assert_clean_exit();
}

/// A `ganja` whose kitty keyboard probe (**D517**) runs: "0" is falsy, so
/// [`Ganja::spawn`] leaves the variable alone and the test answers the query
/// by hand.
fn ganja_probing() -> Ganja {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command.env_remove("GANJA_FAKE_SCRIPT");
    command.env("GANJA_DISABLE_TERM_PROBE", "0");
    Ganja::spawn(command, SMOKE_ROWS)
}

/// The kitty keyboard query, exactly as crossterm writes it: progressive
/// enhancement flags, then primary device attributes.
const KITTY_QUERY: &str = "\x1b[?u\x1b[c";

/// What `PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)` writes.
const KITTY_PUSH: &str = "\x1b[>1u";

/// What `PopKeyboardEnhancementFlags` writes.
const KITTY_POP: &str = "\x1b[<1u";

/// **D517.** A terminal that answers the probe positively gets the
/// disambiguate flag pushed — and popped again on the way out, so the shell
/// underneath never sees kitty-encoded keys.
#[test]
fn a_positive_kitty_probe_pushes_and_the_exit_pops() {
    let mut session = ganja_probing();

    session
        .expect(KITTY_QUERY)
        .expect("the kitty probe never asked");
    // Flags report (none set), then device attributes: a supporting terminal.
    session
        .send("\x1b[?0u\x1b[?1;2c")
        .expect("failed to answer the probe");

    session
        .expect(KITTY_PUSH)
        .expect("a positive probe should push the disambiguate flag");

    session
        .send(ControlCode::EndOfText)
        .expect("failed to send Ctrl-C");
    session
        .expect(KITTY_POP)
        .expect("the exit should pop what the start pushed");
    session.assert_clean_exit();
}

/// **D517.** A terminal that answers only the device-attributes query does
/// not speak the protocol: nothing is pushed, and the session runs on the
/// hold-off repair alone.
#[test]
fn a_negative_kitty_probe_pushes_nothing() {
    let mut session = ganja_probing();

    session
        .expect(KITTY_QUERY)
        .expect("the kitty probe never asked");
    // Device attributes alone: the flags query went unanswered.
    session
        .send("\x1b[?1;2c")
        .expect("failed to answer the probe");

    let after = session.harvest(600);
    assert!(
        !String::from_utf8_lossy(&after).contains(KITTY_PUSH),
        "an unsupporting terminal must not have flags pushed at it"
    );

    session.quit_and_assert_clean_exit();
}

/// **D516.** A Left arrow whose `ESC [ D` bytes are split across two writes
/// lands as cursor movement, never as the literal text `[D` — and the phantom
/// Esc the split once produced reaches nothing.
///
/// The drained pause between the two writes forces the read boundary the
/// repair exists for: the app reads the bare ESC alone, then its tail inside
/// the hold-off window. Asserted on the submitted transcript line, which
/// paints in one frame — the composer's own cell-diffed echo never yields a
/// contiguous needle for the pty stream to match.
#[test]
fn a_split_arrow_key_edits_instead_of_pasting_garbage() {
    let mut session = ganja();
    session.breathe(100);

    session
        .send("hel")
        .expect("failed to type into the composer");
    session.breathe(30);
    session
        .send("\x1b")
        .expect("failed to send the bare escape");
    session.breathe(5);
    session
        .send("[D")
        .expect("failed to send the sequence tail");
    session.send("X").expect("failed to type the marker");
    session.send("\r").expect("failed to submit");

    session
        .expect("heXl")
        .expect("the split arrow was not repaired into a Left");

    session.quit_and_assert_clean_exit();
}

/// **D516.** A bare Esc followed by nothing within the hold-off was a real
/// key press: it is released at the deadline, and a `[D` typed after that
/// deadline is literal text — exactly what the person sent.
#[test]
fn a_late_continuation_stays_literal_text() {
    let mut session = ganja();
    session.breathe(100);

    session
        .send("ok")
        .expect("failed to type into the composer");
    session.breathe(30);
    session
        .send("\x1b")
        .expect("failed to send the bare escape");
    session.breathe(120);
    session.send("[D").expect("failed to send the late tail");
    session.send("\r").expect("failed to submit");

    session
        .expect("ok[D")
        .expect("a late tail should stay literal text");

    session.quit_and_assert_clean_exit();
}

#[test]
fn a_submitted_prompt_streams_a_reply_before_quitting() {
    let mut session = ganja();

    submit_prompt(&mut session);

    session
        .expect(REPLY_OPENING)
        .expect("the fake provider's reply never reached the transcript");

    session.quit_and_assert_clean_exit();
}

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// A project directory with [`TARGET`] seeded.
///
/// The checkout marker is what pins the project — and so the permission store
/// the session writes — to this directory rather than to whatever the
/// temporary directory happens to sit inside.
fn project() -> TempDir {
    let directory = temporary();
    fs::create_dir(directory.path().join(".git")).expect("the checkout marker is creatable");
    fs::write(directory.path().join(TARGET), format!("{SEEDED}\n"))
        .expect("the fixture file is writable");

    directory
}

/// One entry of a script turn's `tool_calls`.
fn call(name: &str, args: serde_json::Value) -> serde_json::Value {
    json!({ "name": name, "args": args })
}

/// The read the model has to make before it may edit anything: `edit` refuses
/// a file this session has not read.
fn read_target() -> serde_json::Value {
    call("read", json!({ "filePath": TARGET }))
}

/// The edit that turns [`SEEDED`] into [`REPLACEMENT`].
fn edit_target() -> serde_json::Value {
    call(
        "edit",
        json!({
            "filePath": TARGET,
            "oldString": SEEDED,
            "newString": REPLACEMENT,
        }),
    )
}

/// A shell call that copies [`TARGET`] to `into`, so that "the command ran"
/// is a question about the filesystem rather than about the screen.
fn copy_target(into: &str) -> serde_json::Value {
    call(
        "bash",
        json!({ "command": format!("cat {TARGET} > {into}") }),
    )
}

/// A script that plays `turns` and then says [`CLOSING`].
///
/// The cadence is as fast as the format allows: a script's fragments only
/// decide how long a run takes, because a step's calls are resolved after its
/// stream has ended.
fn script(mut turns: Vec<serde_json::Value>) -> serde_json::Value {
    turns.push(json!({ "text": CLOSING }));

    json!({ "cadence_ms": 1, "turns": turns })
}

/// Runs the binary in `project`, keeping its state under `data`, playing
/// `script`.
fn scripted(project: &TempDir, data: &TempDir, script: &serde_json::Value) -> Ganja {
    let path = project.path().join(SCRIPT);
    fs::write(
        &path,
        serde_json::to_vec_pretty(script).expect("a script serializes"),
    )
    .expect("the script is writable");

    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        // Relative paths in a call resolve against the engine's directory,
        // which is the one the binary was started in.
        .current_dir(project.path())
        // `ganja_core::provider::fake::SCRIPT_ENV` and the XDG variable are
        // spelled out rather than imported: what a pty test pins is the
        // contract a demo actually uses, which is the name of the variable.
        .env("GANJA_FAKE_SCRIPT", &path)
        // Permission answers and spilled tool output both land under the data
        // home, so a scenario with its own keeps it from reading what another
        // stored — or from writing into a developer's real one.
        .env("XDG_DATA_HOME", data.path())
        // The global config home moves with it: a developer's real
        // `ganja.jsonc` can pick a provider or a theme, either of which would
        // change what this smoke's screen holds.
        .env("HOME", data.path())
        .env("XDG_CONFIG_HOME", data.path().join("config"))
        .env_remove("GANJA_CONFIG_HOME");

    Ganja::spawn(command, SCRIPTED_ROWS)
}

/// Types `PROMPT` and submits it, then waits for the engine to put it in the
/// transcript — which is what says the turn has started.
fn submit_prompt(session: &mut Ganja) {
    session.send(PROMPT).expect("failed to type the prompt");
    session.send("\r").expect("failed to send Enter");

    session
        .expect(PROMPT)
        .expect("the engine's user message never reached the transcript");
}

/// Waits for the permission dialog to open.
fn expect_dialog(session: &mut Ganja, what: &str) {
    session
        .expect(DIALOG_OPTIONS)
        .unwrap_or_else(|error| panic!("no permission dialog for the {what}: {error}"));
}

/// Waits for the script's last turn, which is what says every call before it
/// was resolved without anything still waiting on the user.
fn expect_closing(session: &mut Ganja) {
    session
        .expect(CLOSING)
        .expect("the turn never reached the script's closing text");
}

fn contents(directory: &TempDir, name: &str) -> Vec<u8> {
    fs::read(directory.path().join(name))
        .unwrap_or_else(|error| panic!("{name} should exist in the project: {error}"))
}

/// The permission store a session left under `data`, if it left one.
///
/// The project's directory is found rather than computed, because its name is
/// `ganja-core`'s to decide; that there is at most one of them is itself worth
/// asserting, since a session that stored under two projects stored under the
/// wrong one.
fn permission_store(data: &TempDir) -> Option<PathBuf> {
    let projects = data.path().join("ganja").join("project");

    let stores: Vec<PathBuf> = fs::read_dir(&projects)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(PERMISSIONS))
        .filter(|store| store.is_file())
        .collect();

    assert!(
        stores.len() <= 1,
        "one session answers for one project, got {stores:?}"
    );

    stores.into_iter().next()
}

/// The rules stored under `data`.
fn stored_rules(data: &TempDir) -> serde_json::Value {
    let store = permission_store(data).expect("an always-allow answer should have been stored");
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(&store).expect("the store is readable"))
            .expect("the store is JSON");

    document["rules"].clone()
}

/// The whole chain, through the dialogs: a read runs unasked, an edit and a
/// shell command each stop for an answer, and the file on disk is what says
/// they really ran — and ran in that order, because the command copies what
/// the edit left behind.
#[test]
fn a_scripted_read_edit_and_shell_chain_runs_once_it_is_allowed() {
    const ECHOED: &str = "echoed.txt";

    let project = project();
    let data = temporary();
    let mut session = scripted(
        &project,
        &data,
        &script(vec![json!({
            "text": "Working.",
            "tool_calls": [read_target(), edit_target(), copy_target(ECHOED)],
        })]),
    );

    submit_prompt(&mut session);

    // `read` is allowed by default, so the first dialog is the edit's; the
    // second is the shell command's.
    expect_dialog(&mut session, "edit");
    session.send("y").expect("failed to allow the edit");
    expect_dialog(&mut session, "shell command");
    session
        .send("y")
        .expect("failed to allow the shell command");

    expect_closing(&mut session);
    session.quit_and_assert_clean_exit();

    assert_eq!(
        contents(&project, TARGET),
        format!("{REPLACEMENT}\n").into_bytes(),
        "an allowed edit has to reach the file"
    );
    assert_eq!(
        contents(&project, ECHOED),
        format!("{REPLACEMENT}\n").into_bytes(),
        "the shell command ran, and ran on what the edit left behind"
    );
    assert_eq!(
        permission_store(&data),
        None,
        "an answer given once must not outlive the call it answered"
    );
}

/// A rejection is an answer, not a failure: nothing runs, the file is left
/// exactly as it was, and the turn carries on to its next step.
///
/// The read is scripted ahead of the edit even though only the edit is
/// refused. Without it the edit would be refused anyway — `edit` will not
/// touch a file this session has not read — and the file would be unchanged
/// for a reason that has nothing to do with the answer under test.
#[test]
fn a_rejected_edit_leaves_the_file_untouched_and_the_turn_running() {
    let project = project();
    let data = temporary();
    let mut session = scripted(
        &project,
        &data,
        &script(vec![json!({
            "text": "Working.",
            "tool_calls": [read_target(), edit_target()],
        })]),
    );

    submit_prompt(&mut session);

    expect_dialog(&mut session, "edit");
    session.send("n").expect("failed to reject the edit");

    expect_closing(&mut session);
    session.quit_and_assert_clean_exit();

    assert_eq!(
        contents(&project, TARGET),
        format!("{SEEDED}\n").into_bytes(),
        "a rejected edit must not touch a single byte"
    );
    assert_eq!(
        permission_store(&data),
        None,
        "a rejection is not a rule to remember"
    );
}

/// An "always" answer covers the calls that come after it.
///
/// The proof that the second command did not ask is that the turn reached its
/// closing text without another key being pressed: a dialog would have stopped
/// it there until the deadline. The rule that let it through is read back off
/// disk, and both commands left their own file behind — the second naming a
/// different output, so what was remembered is the command rather than the
/// invocation.
#[test]
fn an_always_answer_lets_the_next_shell_command_run_unasked() {
    const FIRST: &str = "first.txt";
    const SECOND: &str = "second.txt";

    let project = project();
    let data = temporary();
    let mut session = scripted(
        &project,
        &data,
        &script(vec![
            json!({ "text": "First.", "tool_calls": [copy_target(FIRST)] }),
            json!({ "text": "Second.", "tool_calls": [copy_target(SECOND)] }),
        ]),
    );

    submit_prompt(&mut session);

    expect_dialog(&mut session, "first shell command");
    session
        .send("a")
        .expect("failed to always-allow the shell command");

    expect_closing(&mut session);
    session.quit_and_assert_clean_exit();

    let seeded = format!("{SEEDED}\n").into_bytes();
    assert_eq!(contents(&project, FIRST), seeded, "the allowed command ran");
    assert_eq!(
        contents(&project, SECOND),
        seeded,
        "the command after the answer ran too, and ran without asking"
    );
    assert_eq!(
        stored_rules(&data),
        json!([{ "permission": "bash", "pattern": "cat *", "action": "allow" }]),
        "an always answer is stored as the command it named, not the whole tool"
    );
}
