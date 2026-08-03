//! Drives the real binary through a pty to prove the TUI starts and exits.
#![cfg(unix)]

use std::{process::Command, thread, time::Duration};

use expectrl::{Eof, Expect as _, Session, process::unix::WaitStatus};

const EXIT_DEADLINE: Duration = Duration::from_secs(10);

/// Time for the app to enable raw mode and start reading events. A keystroke
/// sent before that can be discarded by the line discipline.
const STARTUP_GRACE: Duration = Duration::from_millis(500);

#[test]
fn q_quits_the_tui_cleanly() {
    let mut session = Session::spawn(Command::new(env!("CARGO_BIN_EXE_ganja")))
        .expect("failed to spawn `ganja` in a pty");
    session.set_expect_timeout(Some(EXIT_DEADLINE));
    session
        .get_process_mut()
        .set_window_size(80, 24)
        .expect("failed to size the pty");

    thread::sleep(STARTUP_GRACE);

    session.send("q").expect("failed to send the quit key");
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
