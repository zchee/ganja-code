//! The acceptance drill sessions owe: kill the process outright while a turn
//! is in flight, start it again with `--continue`, and find the conversation
//! where it was left — a reply the store never saw finish, said to be
//! interrupted rather than shown as one that simply stopped talking.
//!
//! # Why the window is a sleeping shell command
//!
//! "Mid-flight" has to be a state the drill can *hold*, not one it has to
//! catch. A kill aimed at streaming text would race the stream and pass or
//! fail on how fast the machine is. A scripted `bash` call that sleeps puts
//! the turn in a state it stays in for as long as the sleep lasts, and the
//! call's own part file — written before the command is even spawned — is
//! what says the window is open. Nothing here is timed; everything is waited
//! for.
//!
//! # What is asserted where
//!
//! Per the rules `pty_smoke.rs` sets out, the screen is used for
//! synchronization and for the one question only the frontend can answer: that
//! a resumed transcript carries the marker. What the crash actually left
//! behind is read back off the store, because a terminal is only sent the
//! cells that changed and a store assertion is the honest one wherever there
//! is one to make.
//!
//! The store is a SQLite database, so it is read through `ganja_core::Storage`
//! rather than by opening files: the same reader the binary under test uses,
//! which is what keeps this drill an assertion about stored state rather than
//! about a layout it would have to be taught again on every schema change. A
//! poll of a database a live process is writing into needs no tolerance for a
//! half-written record — a reader under WAL sees committed rows and nothing
//! else — which is a property this drill now depends on and used to have to
//! work around.
//!
//! # Killing a process that owns a terminal
//!
//! A process in a pty is a session leader, and a session leader finishes
//! exiting only once its terminal's output queue has drained. So the kill has
//! two halves that cannot be separated: send the signal, then read the pty
//! out. Waiting for the exit status without reading deadlocks outright — the
//! process is waiting for a reader and the reader is waiting for the process —
//! and it deadlocks in the kernel, where no timeout in this file can reach it.
//!
//! For the same family of reasons the kill is aimed at a moment when the run's
//! own child is idle: the drill waits for the command to say it started rather
//! than for the store to say it was started, and the scripted command keeps
//! work after the sleep so that the shell forks the sleep off instead of
//! replacing itself with it. A kill that landed while that child was still
//! starting up was seen to wedge the run in its exit with the child stuck in
//! `execve`.
//!
//! # The command the kill leaves behind
//!
//! `SIGKILL` runs no cleanup, so the sleeping command outlives the agent that
//! spawned it. Nothing here waits on it — the run is reaped by pid and the
//! store is read off disk — and it is reparented rather than left a zombie of
//! this test's, so it costs nothing but its own exit, well inside a `cargo
//! test` run.
#![cfg(unix)]

use std::{
    fs,
    ops::{Deref, DerefMut},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use expectrl::{
    ControlCode, Eof, Expect as _, Session,
    process::unix::{Signal, WaitStatus},
    session::OsSession,
};
use ganja_core::{Message, Part, Role, Storage};
use ganja_protocol::PartBody;
use serde_json::json;
use tempfile::TempDir;

/// How long the resumed run is given to draw and then to quit. Generous on
/// purpose: a timeout here should mean "hung", not "slow machine".
const EXIT_DEADLINE: Duration = Duration::from_secs(10);

/// The escape that opens the alternate screen. The app enables raw mode
/// before it emits this, so a pty that has seen it holds a process the line
/// discipline no longer speaks for — a keystroke sent earlier can be eaten by
/// cooked-mode buffering. A fixed grace period was the old guard here, and a
/// loaded CI runner outran it.
const ALT_SCREEN: &str = "\x1b[?1049h";

/// Width both ptys are opened at. Wide enough that neither string this drill
/// waits for is wrapped — [`INTERRUPTED`] is the longer of the two, and the
/// transcript pane is the full width of the window.
const COLUMNS: u16 = 80;

/// Rows both ptys are opened at.
///
/// The permission dialog is centered in the transcript pane, so the taller the
/// window, the further below the transcript's content it is drawn — and cells
/// nothing has drawn into are what let the options line reach the pty whole.
/// The resumed run has no dialog, but it runs in the same window so that the
/// two runs differ in the flag under test and in nothing else.
const ROWS: u16 = 80;

/// How long the store is polled before the run is called hung. It covers a
/// prompt, a model turn, a permission answer and a spawn, on a machine sharing
/// its cores with the rest of the suite.
const STORE_DEADLINE: Duration = Duration::from_secs(30);

/// How often the store is looked at while waiting for it to say something.
const POLL: Duration = Duration::from_millis(50);

/// How long the scripted command sleeps for.
///
/// It only has to outlast the assertions between the window opening and the
/// kill, which are a handful of file reads; the margin is because the cost of
/// too short a sleep is a flaky suite and the cost of too long a one is a
/// process that lingers after a test that already passed.
const HELD_SECONDS: u64 = 45;

/// The file the scripted command writes before it sleeps, in the project
/// directory the command runs in. It is what says a shell really started —
/// only a running one could have written it.
const HELD: &str = "held.txt";

/// The reply the killed turn gets to stream. One word, so a single fragment
/// carries it, and one that appears nowhere else in the UI — it has to be
/// recognizable both in the part file it lands in and on the resumed screen.
const PARTIAL: &str = "half-said-hierophant";

/// The prompt to type, and what the user's stored message will hold.
const PROMPT: &str = "kaleidoscope";

/// The permission dialog's options line, which is the one string on screen
/// that says a dialog — rather than a tool — is what the turn is waiting on.
/// Pinned to `ganja_tui::component::permission`.
const DIALOG_OPTIONS: &str = "[y] allow once   [a] always allow   [n]/[Esc] reject";

/// What a resumed reply the store never saw finish says about itself. Pinned
/// to `ganja_tui::component::chat`.
const INTERRUPTED: &str = "[interrupted] the session ended before this reply finished";

/// Where the drill's script is written, inside the project directory.
const SCRIPT: &str = "script.json";

/// The state a call that is executing is stored in. Pinned to
/// `ganja_protocol::ToolState`, which tags its variants `status`.
const RUNNING: &str = "running";

/// Where a project's sessions live, under its data directory. Pinned to the
/// same constant in `ganja-cli` and `ganja-tui`.
const STORAGE: &str = "storage";

/// A `ganja` process in a pty, reaped however the test that owns it ends.
struct Ganja {
    /// Taken by whichever of the two endings the test reaches. One still here
    /// when the guard drops belongs to a test that failed part-way through,
    /// and it is holding a pty and a temporary directory nothing else frees.
    session: Option<OsSession>,
}

impl Ganja {
    /// Spawns `command` in a pty and waits for the app to take the terminal
    /// over.
    fn spawn(command: Command) -> Self {
        let mut session = Session::spawn(command).expect("failed to spawn `ganja` in a pty");
        session.set_expect_timeout(Some(EXIT_DEADLINE));
        session
            .get_process_mut()
            .set_window_size(COLUMNS, ROWS)
            .expect("failed to size the pty");

        session
            .expect(ALT_SCREEN)
            .expect("`ganja` never took the terminal over");

        Self {
            session: Some(session),
        }
    }

    /// Kills the process outright and reaps it.
    ///
    /// `SIGKILL` rather than the escalation the guard uses, because what the
    /// drill is about is the state a process left behind when it was given no
    /// chance to tidy up: a signal it could have handled would let the engine
    /// close the turn on its way out, and then nothing under test would be
    /// under test.
    fn kill_outright(mut self) {
        let mut session = self
            .session
            .take()
            .expect("a session is only ever ended once");

        session
            .get_process_mut()
            .kill(Signal::SIGKILL)
            .expect("failed to SIGKILL the `ganja` process");

        // Reading the pty out is not tidying up, it is what lets the kill
        // finish. A session leader — which is what a process in a pty is —
        // only completes its exit once its terminal's output queue has
        // drained, so a test that reached for the exit status without first
        // emptying the pty would wait on a process that is itself waiting on
        // the test. Both would wait forever.
        session
            .expect(Eof)
            .expect("the killed `ganja` never let go of the pty");

        let status = session
            .get_process()
            .wait()
            .expect("failed to reap the killed `ganja` process");
        assert!(
            matches!(status, WaitStatus::Signaled(_, Signal::SIGKILL, _)),
            "expected a process killed outright, got {status:?}"
        );
    }

    /// Quits with Ctrl-C and checks that the process ended cleanly.
    fn quit_and_assert_clean_exit(mut self) {
        self.send(ControlCode::EndOfText)
            .expect("failed to send Ctrl-C");

        let mut session = self
            .session
            .take()
            .expect("a session is only ever ended once");

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
        // that panicked mid-drill left a child in raw mode on a pty, and
        // leaving it running would outlive the whole `cargo test` run.
        let _ = session.get_process_mut().exit(true);
    }
}

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// A project directory holding the drill's script.
///
/// The checkout marker is what pins the project — and so the store the runs
/// write into — to this directory rather than to whatever the temporary
/// directory happens to sit inside.
fn project() -> TempDir {
    let directory = temporary();
    fs::create_dir(directory.path().join(".git")).expect("the checkout marker is creatable");

    // The marker comes first so that a shell which reached it is a shell that
    // is past its own `execve`; the echo after the sleep is what stops the
    // shell from replacing itself with the sleep, which would put the run's
    // direct child back inside one. Both are load-bearing — see the module
    // documentation for what a kill that lands on an `execve` does.
    let script = json!({
        "cadence_ms": 1,
        "turns": [{
            "text": PARTIAL,
            "tool_calls": [{
                "name": "bash",
                "args": {
                    "command": format!("echo held > {HELD}; sleep {HELD_SECONDS}; echo released"),
                },
            }],
        }],
    });
    fs::write(
        directory.path().join(SCRIPT),
        serde_json::to_vec_pretty(&script).expect("a script serializes"),
    )
    .expect("the script is writable");

    directory
}

/// Runs the binary in `project` with `arguments`, keeping its state under
/// `data` and playing the drill's script.
///
/// `ganja_core::provider::fake::SCRIPT_ENV` and the XDG variable are spelled
/// out rather than imported: what a pty test pins is the contract a demo
/// actually uses, which is the name of the variable.
fn ganja(project: &TempDir, data: &TempDir, arguments: &[&str]) -> Ganja {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        // Relative paths in a call resolve against the engine's directory,
        // which is the one the binary was started in — and it is also what
        // decides which project's store the run opens.
        .current_dir(project.path())
        .args(arguments)
        .env("GANJA_PROVIDER", "fake")
        .env("GANJA_FAKE_SCRIPT", project.path().join(SCRIPT))
        // Sessions land under the data home, so a drill with its own keeps it
        // from reading what another stored — or from writing into a
        // developer's real one.
        .env("XDG_DATA_HOME", data.path());

    Ganja::spawn(command)
}

/// Types [`PROMPT`] and submits it, then waits for the engine to put it in the
/// transcript — which is what says the turn has started.
fn submit_prompt(session: &mut Ganja) {
    session.send(PROMPT).expect("failed to type the prompt");
    session.send("\r").expect("failed to send Enter");

    session
        .expect(PROMPT)
        .expect("the engine's user message never reached the transcript");
}

/// The session store the runs under `data` write into, once one of them has
/// created it.
///
/// The project's directory is found rather than computed, because its name is
/// `ganja-core`'s to decide; that there is at most one of them is itself worth
/// asserting, since a run that stored under two projects stored under the
/// wrong one. Opening a store does no I/O, so the database file is asked for
/// by name before anything opens it: a store this test created would answer
/// every question with an empty transcript forever.
fn store(data: &TempDir) -> Option<Storage> {
    let stores: Vec<Storage> = fs::read_dir(data.path().join("ganja").join("project"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| Storage::open(entry.path().join(STORAGE)))
        .filter(|store| store.database().is_file())
        .collect();

    assert!(
        stores.len() <= 1,
        "one run stores under one project, got {stores:?}"
    );

    stores.into_iter().next()
}

/// What one session's assistant reply looks like in the store.
struct StoredReply {
    /// The message as the store holds it, its parts attached in id order.
    reply: Message,
}

impl StoredReply {
    /// Whether the store ever saw the reply finish.
    ///
    /// A `time.completed` a turn has not reached is left absent rather than
    /// written as a stamp, and that absence is the whole crash marker: this
    /// reply has no ending.
    fn finished(&self) -> bool {
        self.reply.time.completed.is_some()
    }

    /// Everything the stored text parts carry, in the order they were written.
    fn text(&self) -> String {
        self.reply.parts.iter().filter_map(Part::as_text).collect()
    }

    /// The status of every stored tool call, in the order they were written.
    ///
    /// Read off the serialized state rather than matched on, because what the
    /// assertion is about is the tag a stored call carries — `ToolState` is
    /// what tags its variants `status`, and a rename there has to reach this
    /// test rather than pass it.
    fn call_states(&self) -> Vec<String> {
        self.reply
            .parts
            .iter()
            .filter_map(|part| match &part.body {
                PartBody::Tool { state, .. } => serde_json::to_value(state).ok(),
                _ => None,
            })
            .filter_map(|state| state["status"].as_str().map(str::to_owned))
            .collect()
    }
}

/// The assistant reply `store` holds, once it holds one.
///
/// One session, because the drill runs one conversation; the reply is the
/// assistant message in it, read back through the same transcript loader the
/// frontend seeds itself from.
fn stored_reply(store: &Storage) -> Option<StoredReply> {
    let session = store.list_sessions().ok()?.into_iter().next()?;
    let reply = store
        .load_transcript(&session.id)
        .ok()?
        .into_iter()
        .find(|message| message.role == Role::Assistant)?;

    Some(StoredReply { reply })
}

/// Whether the store says the drill's window is open: the streamed text has
/// reached its part row, and the call that holds the turn open is recorded as
/// executing.
fn mid_flight(store: &Storage) -> bool {
    stored_reply(store).is_some_and(|reply| {
        reply.text().contains(PARTIAL) && reply.call_states().iter().any(|state| state == RUNNING)
    })
}

/// Polls the store until it says `ready`, or gives up after
/// [`STORE_DEADLINE`].
///
/// Polling rather than sleeping is what keeps the drill honest on a loaded
/// machine: a fixed wait long enough to be reliable there would be long enough
/// to hide a regression that let the window close early.
fn wait_until(data: &TempDir, what: &str, ready: impl Fn(&Storage) -> bool) {
    let deadline = Instant::now() + STORE_DEADLINE;
    // Opened once and then kept: every query runs in its own read transaction,
    // so one handle still sees what the live process commits after it — and
    // reopening on every tick would run a fresh integrity check fifty times a
    // second for nothing.
    let mut opened: Option<Storage> = None;

    loop {
        if opened.is_none() {
            opened = store(data);
        }
        if opened.as_ref().is_some_and(&ready) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the store never showed {what} within {STORE_DEADLINE:?}"
        );
        thread::sleep(POLL);
    }
}

/// The whole drill: a turn is held open by a sleeping command, the process is
/// killed outright, and what it left on disk is a reply with no ending — which
/// `--continue` shows for what it is rather than as a reply that stopped
/// mid-sentence.
#[test]
fn a_reply_killed_mid_call_comes_back_under_continue_marked_interrupted() {
    let project = project();
    let data = temporary();

    let mut session = ganja(&project, &data, &[]);
    submit_prompt(&mut session);

    // Waiting for the dialog is safe: a step's calls are resolved after the
    // model's stream has ended, so no fragment of the reply can race the
    // options line onto the screen.
    session
        .expect(DIALOG_OPTIONS)
        .expect("no permission dialog for the shell command");
    session
        .send("y")
        .expect("failed to allow the sleeping shell command");

    // Both halves matter. The store saying `running` is what the drill is
    // about; the marker the command itself wrote is what says the shell is
    // past its own startup, and so that the kill below lands on a run whose
    // child is sleeping rather than on one whose child is still starting up.
    let held = project.path().join(HELD);
    wait_until(&data, "a call whose command has started", |store| {
        held.is_file() && mid_flight(store)
    });

    session.kill_outright();

    // What survived the kill is the store, so that is what is asked. Read
    // after the kill rather than reused from the wait: the claim is about what
    // a dead process left behind, not about what a live one had written.
    // A store opened after the kill rather than the one the wait held: what
    // is being asked is what a dead process left in the database, and a
    // connection that was open while it died has no business answering that.
    let survived = store(&data)
        .as_ref()
        .and_then(stored_reply)
        .expect("the killed run's session should still be on disk");

    assert!(
        !survived.finished(),
        "a reply its process died in the middle of must not be stored as finished, got {:?}",
        survived.reply.time
    );
    assert!(
        survived.text().contains(PARTIAL),
        "what was streamed before the kill must have reached a part file, got {:?}",
        survived.text()
    );
    assert_eq!(
        survived.call_states(),
        vec![RUNNING.to_owned()],
        "the call the process died inside must be left exactly as it was"
    );

    let mut resumed = ganja(&project, &data, &["--continue"]);

    resumed
        .expect(PARTIAL)
        .expect("the resumed transcript never showed what had been streamed");
    resumed
        .expect(INTERRUPTED)
        .expect("the resumed reply was not shown as one that never finished");

    resumed.quit_and_assert_clean_exit();
}
