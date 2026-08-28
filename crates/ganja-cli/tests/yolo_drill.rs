//! The `--yolo` drill (**D479**): a bypassed session through a real pty, from
//! the flag on the command line to the command that ran without anybody being
//! asked about it.
//!
//! A unit test can assert that the frontend replies `Once` to an event it was
//! handed. What it cannot assert is that a *real* session — a real terminal, a
//! real engine, the real permission rules loaded off this project — runs a
//! command the defaults ask about and never stops to ask. That is what this
//! is for, and it is the half of AC1 that only a process can prove.
//!
//! # What this waits for, and why
//!
//! Per `pty_smoke.rs`'s rules, a string reaches the pty whole only when it is
//! drawn over cells it differs from. Only one string here is read off the
//! screen: the standing marker, which is the **first** thing on the status bar
//! and is therefore drawn into blank cells on the session's very first frame.
//! Everything else this proves is read back off the filesystem — the file the
//! shell command wrote, and the permission store the session did *not* write —
//! which is exactly the division `pty_smoke.rs` draws.
//!
//! # The absence assertion
//!
//! One step asserts the permission dialog does **not** appear, which needs a
//! bound: the expect timeout is shortened for exactly that wait, and a timeout
//! is the pass. A dialog is drawn as soon as a step's calls are resolved —
//! before any of the reply after it — so a dialog that was going to open has
//! opened long before the bound expires.
//!
//! That wait is also the window the whole scripted turn runs in, which is why
//! the closing word is never waited for: a failed `expect` consumes whatever
//! it read on the way to its timeout, so a string drawn *during* the absence
//! window cannot be waited for after it. The filesystem is what says the turn
//! got where it was going.
//!
//! What makes the absence mean something is `pty_smoke.rs`, which runs the
//! same tool call through the same fake provider **without** the flag and
//! waits for that same dialog string successfully. A dialog that could never
//! be drawn at all would fail there, so "it did not open" cannot pass here by
//! being broken everywhere.
#![cfg(unix)]

use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use std::{fs, thread};

use expectrl::process::unix::WaitStatus;
use expectrl::session::OsSession;
use expectrl::{ControlCode, Eof, Expect as _, Session};
use ganja_testkit::temp_dir as temporary;
use serde_json::json;
use tempfile::TempDir;

const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// How long the absence assertion waits before calling the dialog absent, and
/// therefore also how long the scripted turn is given to run to its end.
const ABSENCE_DEADLINE: Duration = Duration::from_secs(3);

/// How long the file the command wrote is waited for, after the window above.
/// Generous: what it guards against is a loaded machine, not a slow design.
const FILE_DEADLINE: Duration = Duration::from_secs(10);

/// The escape that opens the alternate screen; see `pty_smoke.rs`.
const ALT_SCREEN: &str = "\x1b[?1049h";

const COLUMNS: u16 = 80;

/// Tall enough that a permission dialog, if one were drawn, would land on
/// blank cells below the transcript — so the absence assertion is about a
/// dialog that was never raised rather than about one that could not be read.
const ROWS: u16 = 80;

/// The standing marker, pinned to `ganja_tui::component::status`'s `YOLO`.
const MARKER: &str = "yolo";

/// The permission dialog's options line, pinned to
/// `ganja_tui::component::permission` — the same string `pty_smoke.rs` waits
/// for successfully in the un-bypassed run of this same call.
const DIALOG_OPTIONS: &str = "[y] allow once   [a] always allow   [n]/[Esc] reject";

/// A string nothing ever draws, waited for only so that the wait itself
/// reads the pty. Its timeout is the point; its match would be a bug here.
const NEVER: &str = "never-drawn-zarquon";

/// The prompt this drill submits.
const PROMPT: &str = "kaleidoscope";

/// What the scripted shell call writes. `bash` asks by default
/// (`ganja_permission::permission::ASK_BY_DEFAULT`), so this file existing is
/// the whole claim: a call the rules stop was let through by nobody.
const RAN: &str = "ran.txt";

/// Where permission answers are stored, relative to the project's data
/// directory. Pinned to `ganja_permission::permission::FILE`.
const PERMISSIONS: &str = "permissions.json";

/// Where the script is written, inside the project directory.
const SCRIPT: &str = "script.json";

/// A `ganja` process in a pty, reaped however the test that owns it ends.
struct Ganja {
    session: Option<OsSession>,
}

impl Ganja {
    fn spawn(mut command: Command) -> Self {
        command.env("GANJA_PROVIDER", "fake");
        // The kitty keyboard probe (D517) would stall 2s unanswered here.
        command.env("GANJA_DISABLE_TERM_PROBE", "1");

        let mut session = Session::spawn(command).expect("failed to spawn `ganja` in a pty");
        session.set_expect_timeout(Some(EXIT_DEADLINE));
        session.get_process_mut().set_window_size(COLUMNS, ROWS).expect("failed to size the pty");
        session.expect(ALT_SCREEN).expect("`ganja` never took the terminal over");

        Self { session: Some(session) }
    }

    fn quit_and_assert_clean_exit(mut self) {
        self.send(ControlCode::EndOfText).expect("failed to send Ctrl-C");

        let mut session = self.session.take().expect("a session is only ever taken once");
        session.expect(Eof).expect("`ganja` did not exit within the deadline");

        let status = session.get_process().wait().expect("failed to reap the `ganja` process");
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

        let _ = session.get_process_mut().exit(true);
    }
}

/// A project directory the session will pin its state — and so its permission
/// store — to.
fn project() -> TempDir {
    let directory = temporary();
    fs::create_dir(directory.path().join(".git")).expect("the checkout marker is creatable");

    directory
}

/// One shell call the defaults ask about, then a closing word.
fn script() -> serde_json::Value {
    json!({
        "cadence_ms": 1,
        "turns": [
            {
                "text": "Let me run it.",
                "tool_calls": [{"name": "bash", "args": {"command": format!("touch {RAN}")}}],
            },
            {"text": "script-finished-zarquon"},
        ],
    })
}

/// Runs the binary in `project` with the flags `spelled`, keeping its state
/// under `data`.
fn scripted(project: &TempDir, data: &TempDir, spelled: &[&str]) -> Ganja {
    let path = project.path().join(SCRIPT);
    fs::write(&path, serde_json::to_vec_pretty(&script()).expect("a script serializes"))
        .expect("the script is writable");

    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        // Relative paths in a call resolve against the engine's directory,
        // which is the one the binary was started in.
        .current_dir(project.path())
        .args(spelled)
        .env("GANJA_FAKE_SCRIPT", &path)
        // Permission answers land under the data home, so this scenario keeps
        // its own — which is also what makes "the store is empty" a claim
        // about this session rather than about the machine.
        .env("XDG_DATA_HOME", data.path())
        // The global config home moves with it: a developer's real
        // `ganja.toml` could allow or deny `bash` outright, either of which
        // would decide this drill's question before the flag got to.
        .env("HOME", data.path())
        .env("XDG_CONFIG_HOME", data.path().join("config"))
        .env_remove("GANJA_CONFIG_HOME");

    Ganja::spawn(command)
}

/// Reads the pty for a bounded moment while asserting that `absent` is not
/// drawn into it. The timeout is the pass.
fn expect_absent(session: &mut Ganja, absent: &str, message: &str) {
    session.set_expect_timeout(Some(ABSENCE_DEADLINE));
    let drawn = session.expect(absent);
    session.set_expect_timeout(Some(EXIT_DEADLINE));
    assert!(drawn.is_err(), "{message}");
}

/// Waits for the file the scripted command writes, keeping the pty drained so
/// the app is never blocked on its own stdout while we wait.
fn wait_for(session: &mut Ganja, project: &TempDir, name: &str) {
    let deadline = Instant::now() + FILE_DEADLINE;
    while Instant::now() < deadline {
        if project.path().join(name).exists() {
            return;
        }
        session.set_expect_timeout(Some(Duration::from_millis(100)));
        let drawn = session.expect(NEVER);
        session.set_expect_timeout(Some(EXIT_DEADLINE));
        assert!(drawn.is_err(), "{NEVER} is never drawn by anything");
        thread::yield_now();
    }

    panic!("the scripted command never wrote {name}");
}

/// The permission store this session left under `data`, if it left one.
///
/// Found rather than computed, exactly as `pty_smoke.rs` finds it: the
/// project directory's name is `ganja-core`'s to decide.
fn permission_store(data: &TempDir) -> Option<PathBuf> {
    let projects = data.path().join("ganja").join("project");

    fs::read_dir(&projects)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(PERMISSIONS))
        .find(|store| store.is_file())
}

/// AC1, end to end: the flag parses, the marker stands, the call the rules ask
/// about runs with no dialog on screen, and the session leaves no rule behind.
#[test]
fn a_yolo_session_runs_an_asked_about_command_without_a_dialog_or_a_stored_rule() {
    let project = project();
    let data = temporary();
    let mut session = scripted(&project, &data, &["--yolo"]);

    // Before anything is asked of the model: whoever is looking at this
    // terminal is told what kind of session it is (**D479**).
    session.expect(MARKER).expect("a bypassed session says so on its status bar");

    session.send(PROMPT).expect("failed to type the prompt");
    session.send("\r").expect("failed to send Enter");

    expect_absent(&mut session, DIALOG_OPTIONS, "a bypassed session raises no permission dialog");
    wait_for(&mut session, &project, RAN);

    assert!(
        permission_store(&data).is_none(),
        "an `allow once` is answered and forgotten: nothing may be written to \
         this project's rules on the strength of a flag"
    );

    session.quit_and_assert_clean_exit();
}

/// The documented spelling reaches the same session as the hidden one, which
/// is the whole reason both exist.
#[test]
fn the_documented_spelling_bypasses_exactly_as_the_hidden_one_does() {
    let project = project();
    let data = temporary();
    let mut session = scripted(&project, &data, &["--auto"]);

    session.expect(MARKER).expect("`--auto` is the same session `--yolo` is");

    session.send(PROMPT).expect("failed to type the prompt");
    session.send("\r").expect("failed to send Enter");

    expect_absent(&mut session, DIALOG_OPTIONS, "a bypassed session raises no permission dialog");
    wait_for(&mut session, &project, RAN);

    session.quit_and_assert_clean_exit();
}
