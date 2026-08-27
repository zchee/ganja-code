//! The Esc Esc drill (**D452** amended by **D467**): the backtrack gesture
//! through a real pty, across the two states its guard is about — and the
//! split, in which `/rewind` alone still opens the Claude-style picker.
//!
//! The gesture only exists at an **idle** composer. While a turn streams, Esc
//! is the cancel and nothing else — and it forgets any press before it, so a
//! double-press racing a turn's end cancels and then does nothing rather than
//! opening a walk over a conversation somebody was still watching. That is a
//! rule about *timing*, and a unit test can only assert it against a flag; this
//! asserts it against a real terminal, a real engine and a turn that really is
//! streaming.
//!
//! # What this waits for, and why
//!
//! Per `pty_smoke.rs`'s rules, a string reaches the pty whole only when it is
//! drawn over cells it differs from. The waits here are strings that have
//! never been on screen before — the walk's status-bar hint, appended past the
//! bar's short segments onto cells that were blank; the picker's hint line,
//! which nothing else draws; and a checkpoint row's `No code restore`
//! annotation, which cannot exist until the picker has a checkpoint to
//! annotate. The window runs eighty rows so the centered dialog lands on blank
//! cells below the transcript.
//!
//! # Why the two Escs are not one write
//!
//! Two `\x1b` bytes that reach crossterm's parser in one read are **one** Esc
//! event: the parser holds a lone escape until the next byte can rule out a
//! sequence, and `ESC ESC` resolves to a single `Esc` with both bytes
//! consumed. A gesture typed as one write is therefore one press, and so is
//! one typed as two writes that the app has not had a chance to read between.
//! [`gesture`] puts a drain between them — which is also what keeps the app
//! running at all, since a process whose frames nobody reads blocks on its own
//! stdout, and a blocked app reads no keys. A person pressing Esc twice sends
//! two reads tens of milliseconds apart, which is exactly what this
//! reproduces.
//!
//! # The absence assertion
//!
//! One step asserts that something does **not** appear, which needs a bound:
//! the expect timeout is shortened for exactly that wait, and a timeout is the
//! pass. A keystroke redraws immediately — the app never coalesces one — so a
//! picker that was going to open has drawn long before the bound expires.
//!
//! The step after it is what makes the absence mean something: the same string
//! is waited for again once the turn is over, and it arrives. A picker that
//! could not open at all would fail there, so "it did not open" cannot pass by
//! being broken.
#![cfg(unix)]

use std::{
    fs,
    ops::{Deref, DerefMut},
    process::Command,
    time::Duration,
};

use expectrl::{
    ControlCode, Eof, Expect as _, Session, process::unix::WaitStatus, session::OsSession,
};
use ganja_testkit::temp_dir as temporary;
use serde_json::json;
use tempfile::TempDir;

const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// How long the absence assertion waits before calling the picker absent.
///
/// Generous next to a keystroke's own redraw, which is immediate, and short
/// enough that the turn it is racing is still streaming when it expires.
const ABSENCE_DEADLINE: Duration = Duration::from_secs(3);

/// The escape that opens the alternate screen; see `pty_smoke.rs`.
const ALT_SCREEN: &str = "\x1b[?1049h";

/// One Esc.
const ESC: &str = "\x1b";

/// How long the pty is drained between the gesture's two presses: long enough
/// that the app reads the first one, short enough that the second lands well
/// inside the half-second the chord allows (`ganja_tui::app::ESC_CHORD`).
const BETWEEN_PRESSES: Duration = Duration::from_millis(150);

/// A string nothing ever draws, waited for only so that the wait itself drains
/// the pty. Its timeout is the point; its match would be a bug in this file.
const NEVER: &str = "never-drawn-zarquon";

const COLUMNS: u16 = 80;

/// Tall enough that the centered picker is drawn below the transcript's last
/// line, on cells nothing else has written to.
const ROWS: u16 = 80;

/// The picker's hint line, which nothing else on screen draws. Pinned to
/// `ganja_tui::component::rewind`.
const PICKER_HINTS: &str = "[Enter] continue   [Esc] cancel";

/// The head of the walk's status-bar hint, shown only while the backtrack
/// walk is up. Pinned to `ganja_tui::app::BACKTRACK_HINT`; a prefix rather
/// than the whole line so a narrow bar cannot clip the wait's tail away.
const WALK_HINT: &str = "backtrack: Esc older";

/// A checkpoint row's annotation for a turn that changed no file. It cannot
/// appear until the picker has a checkpoint, which is what makes it the right
/// string to wait for *after* a prompt and the wrong one to see before.
const NO_CODE: &str = "No code restore";

/// The prompt this drill submits.
const PROMPT: &str = "kaleidoscope";

/// The first word of the scripted reply. The reply is long and the cadence
/// slow, so the turn is still streaming for many seconds after this lands —
/// which is what makes "while a turn streams" a state this drill can hold
/// rather than one it has to catch.
const OPENING: &str = "streaming-opening-zarquon";

/// Milliseconds between the reply's words.
const CADENCE: u64 = 200;

/// Words of filler between the opening and the end, at [`CADENCE`] each.
const FILLER: usize = 60;

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

    fn quit_and_assert_clean_exit(mut self) {
        self.send(ControlCode::EndOfText)
            .expect("failed to send Ctrl-C");

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

/// A project directory the app will pin its state to.
fn project() -> TempDir {
    let directory = temporary();
    fs::create_dir(directory.path().join(".git")).expect("the checkout marker is creatable");

    directory
}

/// One turn, streamed a word at a time slowly enough that the drill can act
/// while it is still going.
fn script() -> serde_json::Value {
    let reply = format!("{OPENING} {}", "filler ".repeat(FILLER));

    json!({ "cadence_ms": CADENCE, "turns": [{ "text": reply }] })
}

fn scripted(project: &TempDir, data: &TempDir) -> Ganja {
    let path = project.path().join("script.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&script()).expect("a script serializes"),
    )
    .expect("the script is writable");

    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        .current_dir(project.path())
        .env("GANJA_FAKE_SCRIPT", &path)
        .env("XDG_DATA_HOME", data.path())
        // The global config home moves with the data home: a developer's real
        // `ganja.toml` can pick a provider or rebind Esc, either of which
        // would change what this drill's keystrokes mean.
        .env("HOME", data.path())
        .env("XDG_CONFIG_HOME", data.path().join("config"))
        .env_remove("GANJA_CONFIG_HOME");

    Ganja::spawn(command)
}

/// Reads whatever the app has to say for a moment and throws it away.
///
/// Every Esc this file sends is followed by one. An Esc that shares a read
/// with the byte after it is not an Esc at all — crossterm folds `ESC x` into
/// Alt-x — so the pause is what makes each press its own keystroke, and the
/// reading is what lets the app draw at all.
fn drain(session: &mut Ganja) {
    session.set_expect_timeout(Some(BETWEEN_PRESSES));
    let drawn = session.expect(NEVER);
    session.set_expect_timeout(Some(EXIT_DEADLINE));
    assert!(drawn.is_err(), "{NEVER} is never drawn by anything");
}

/// Presses Esc once, and gives the app the moment it needs to see it alone.
fn escape(session: &mut Ganja) {
    session.send(ESC).expect("failed to send Esc");
    drain(session);
}

/// Sends the two presses the gesture is made of, as two reads and therefore
/// two key events. See the module doc.
fn gesture(session: &mut Ganja) {
    escape(session);
    session.send(ESC).expect("failed to send the second Esc");
}

#[test]
fn esc_esc_walks_back_only_at_an_idle_composer_and_rewind_keeps_the_picker() {
    let project = project();
    let data = temporary();
    let mut session = scripted(&project, &data);

    // ---- idle, before anything has been asked ---------------------------
    // The picker no longer rides the gesture (D467), and over an empty
    // transcript there is nothing to walk either. The same picker string is
    // waited for again at the end, through `/rewind`, which is what keeps
    // this absence honest.

    gesture(&mut session);
    session.set_expect_timeout(Some(ABSENCE_DEADLINE));
    let opened = session.expect(PICKER_HINTS);
    session.set_expect_timeout(Some(EXIT_DEADLINE));
    assert!(
        opened.is_err(),
        "Esc Esc opens no picker any more, and no walk over an empty transcript"
    );

    // ---- streaming -------------------------------------------------------

    session.send(PROMPT).expect("failed to type the prompt");
    session.send("\r").expect("failed to send Enter");
    session
        .expect(OPENING)
        .expect("the scripted reply never started streaming");

    gesture(&mut session);
    session.set_expect_timeout(Some(ABSENCE_DEADLINE));
    let walked = session.expect(WALK_HINT);
    session.set_expect_timeout(Some(EXIT_DEADLINE));
    assert!(
        walked.is_err(),
        "no walk may open over a turn the user is still watching"
    );

    // ---- idle again, which is also what says the Esc really cancelled ----

    gesture(&mut session);
    session.expect(WALK_HINT).expect(
        "with the turn cancelled the gesture is armed again, and the walk \
         highlights the prompt",
    );

    // Any key that is neither Esc nor Enter leaves the walk without
    // reverting; backspace is one that then also types nothing.
    session.send("\x7f").expect("failed to send Backspace");
    drain(&mut session);

    // ---- the picker's only door is /rewind now ---------------------------

    // The pty carries a frame's rows top to bottom, so the checkpoint row is
    // waited for before the hint line drawn beneath it.
    session.send("/rewind").expect("failed to type /rewind");
    session.send("\r").expect("failed to send Enter");
    session.expect(NO_CODE).expect(
        "the picker lists the cancelled prompt as a checkpoint that changed \
         no file",
    );
    session
        .expect(PICKER_HINTS)
        .expect("/rewind should still open the two-step picker");

    escape(&mut session);
    session.quit_and_assert_clean_exit();
}
