//! Drives the real binary through a pty: a fake turn streams into the
//! transcript, and every exit path leaves the terminal restored.
#![cfg(unix)]

use std::{process::Command, thread, time::Duration};

use expectrl::{
    ControlCode, Eof, Expect as _, Session, process::unix::WaitStatus, session::OsSession,
};

const EXIT_DEADLINE: Duration = Duration::from_secs(10);

/// Time for the app to enable raw mode and start reading events. A keystroke
/// sent before that can be discarded by the line discipline.
const STARTUP_GRACE: Duration = Duration::from_millis(500);

/// The opening word of the fake provider's canned reply. It is the first thing
/// drawn for a turn, so it lands at the start of a line and is never split by
/// wrapping.
const REPLY_OPENING: &str = "Acknowledged";

fn ganja() -> OsSession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command.env("GANJA_PROVIDER", "fake");

    let mut session = Session::spawn(command).expect("failed to spawn `ganja` in a pty");
    session.set_expect_timeout(Some(EXIT_DEADLINE));
    session
        .get_process_mut()
        .set_window_size(80, 24)
        .expect("failed to size the pty");

    thread::sleep(STARTUP_GRACE);

    session
}

fn assert_clean_exit(mut session: OsSession) {
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

#[test]
fn control_c_quits_the_tui_cleanly() {
    let mut session = ganja();

    session
        .send(ControlCode::EndOfText)
        .expect("failed to send Ctrl-C");

    assert_clean_exit(session);
}

#[test]
fn a_submitted_prompt_streams_a_reply_before_quitting() {
    let mut session = ganja();

    session.send("hello").expect("failed to type the prompt");
    session.send("\r").expect("failed to send Enter");

    session
        .expect(REPLY_OPENING)
        .expect("the fake provider's reply never reached the transcript");

    session
        .send(ControlCode::EndOfText)
        .expect("failed to send Ctrl-C");

    assert_clean_exit(session);
}
