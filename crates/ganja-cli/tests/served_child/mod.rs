//! The half of a `ganja serve` fixture that `serve.rs` and `attach.rs` must
//! not spell twice: a server child that dies with the test that spawned it,
//! its diagnostics read as they are written rather than after it exits, and
//! waits that say what they were waiting for when they give up.
//!
//! # Why a child needs reaping at all
//!
//! [`std::process::Child`] deliberately does nothing when it is dropped — it
//! neither kills nor waits — so every panic between the spawn and the SIGTERM
//! left a `ganja serve` listening for as long as the machine stayed up. That
//! is bead `ganja-code-pjc`, and the orphan it was re-confirmed on had plainly
//! outlived its fixture rather than a hard kill: all three of the temporary
//! directories `Server` holds were already gone from the disk while the
//! process was still holding its port, which is the signature of a struct that
//! unwound around a `Child` that did not care.
//!
//! # What this cannot cover, and what does
//!
//! A `Drop` runs only where code runs. If the harness itself is SIGKILLed —
//! nextest's hard kill after a timeout's grace period, or a person's `kill -9`
//! — nothing here executes and the child outlives the run. The mitigation is
//! not this file's to write, and it is the reason nothing here puts the child
//! in a process group of its own: nextest runs each test in its own group and
//! signals the **group**, so a child left in the harness's group is reached by
//! that kill, where one that had called `setpgid` would escape it. A fixture
//! that reaped harder in the paths it can see, by hiding from the only
//! mechanism that covers the path it cannot, would be a worse fixture. That
//! mitigation is nextest's alone — a plain `cargo test` harness that is
//! SIGKILLed signals nothing on the way out — and that each test sits in a
//! group of its own rather than the harness's is read off nextest's own
//! account of itself, not measured here.

// Each binary over this module uses a different half of it.
#![allow(dead_code)]

use std::io::{BufRead as _, BufReader};
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long any single wait may take before the fixture is declared broken.
pub const DEADLINE: Duration = Duration::from_secs(60);

/// How often a wait for the child's exit looks again.
const POLL: Duration = Duration::from_millis(50);

/// How many of the server's last lines a failure message carries — a wait
/// that gave up, or an attached run that did not exit clean. Enough to hold a
/// startup refusal and the panic that followed it, short enough to read.
const TAIL: usize = 40;

/// A spawned `ganja serve`, killed when this value is dropped, and with its
/// standard error drained while it runs.
///
/// Draining is not only so a failed wait can quote it: standard error is a
/// pipe nobody read until the child had already exited, and a server that
/// filled that pipe's buffer would block inside a write rather than answer the
/// request the test was waiting on — a hang whose cause would be invisible at
/// both ends.
pub struct Reaped {
    child: Child,
    said: Arc<Mutex<String>>,
    draining: Option<JoinHandle<()>>,
}

impl Reaped {
    /// Takes over `child`, which must have been spawned with both its standard
    /// output and its standard error piped.
    pub fn new(child: Child) -> Self {
        // Owned by the guard before anything here can panic: a `take` on an
        // unpiped stream, or a thread that would not start, would otherwise
        // unwind around a bare `Child` — the exact orphan this type exists to
        // prevent, minted inside it.
        let mut this = Self { child, said: Arc::new(Mutex::new(String::new())), draining: None };
        let stderr = this.child.stderr.take().expect("stderr is piped");
        let into = Arc::clone(&this.said);
        this.draining = Some(std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => return,
                    // Lossily, and a whole line at a time: a fixture that
                    // refused to report a server's diagnostics because they
                    // were not UTF-8 would be withholding the one thing the
                    // failure is about.
                    Ok(_) => {
                        if let Ok(mut said) = into.lock() {
                            said.push_str(&String::from_utf8_lossy(&line));
                        }
                    }
                }
            }
        }));

        this
    }

    /// The child's process id, which is what a signal is addressed to.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Whether the child has exited, without waiting for it.
    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("the child is waitable")
    }

    /// The first line the child writes to standard output.
    ///
    /// Read on a thread so a server that never speaks fails the deadline
    /// instead of hanging the harness — and a server that closed its standard
    /// output without saying anything fails here too, rather than handing back
    /// an empty line for a caller to fail on with nothing to show. Its exit
    /// status is usually the whole answer: a refused bind exits before it ever
    /// reaches the address line.
    pub fn announcement(&mut self, what: &str) -> String {
        let stdout = self.child.stdout.take().expect("stdout is piped");
        let (line_tx, line_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = line_tx.send(line);
        });

        let started = Instant::now();
        let line = line_rx.recv_timeout(DEADLINE).unwrap_or_default();
        assert!(!line.trim().is_empty(), "{}", self.gave_up(what, started));

        line
    }

    /// Polls until the child exits, and answers how it did.
    pub fn wait_for_exit(&mut self, what: &str) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.exited() {
                return status;
            }
            assert!(started.elapsed() < DEADLINE, "{}", self.gave_up(what, started));
            std::thread::sleep(POLL);
        }
    }

    /// Everything the child has written to standard error, once it has exited.
    ///
    /// Joins the drain, so what comes back is the whole of it rather than
    /// whatever had arrived by the time the caller asked — under the same
    /// deadline as every other wait here, because a pipe some grandchild
    /// inherited stays open after the server is gone, and a join with no bound
    /// on it would be the one wait in this file that hangs without a word.
    pub fn diagnostics(&mut self) -> String {
        debug_assert!(
            self.exited().is_some(),
            "the diagnostics are read once the child has exited"
        );
        if let Some(draining) = self.draining.take() {
            let started = Instant::now();
            while !draining.is_finished() {
                assert!(
                    started.elapsed() < DEADLINE,
                    "{}",
                    self.gave_up("the server's standard error to close", started)
                );
                std::thread::sleep(POLL);
            }
            let _ = draining.join();
        }

        self.said().clone()
    }

    /// What the server is doing and what it has said, for a failure message.
    pub fn state(&mut self) -> String {
        let standing = match self.exited() {
            Some(status) => format!("had already exited ({status})"),
            None => "was still running".to_owned(),
        };

        format!("the server {standing}; its standard error so far:\n{}", self.tail())
    }

    /// A wait this fixture gave up on, named: which wait it was, how long it
    /// actually waited, and [`Reaped::state`].
    pub fn gave_up(&mut self, what: &str, started: Instant) -> String {
        format!("waited {:?} for {what} and it did not happen; {}", started.elapsed(), self.state())
    }

    /// The last [`TAIL`] lines of what the child has said so far.
    fn tail(&self) -> String {
        let said = self.said();
        let lines: Vec<&str> = said.lines().collect();
        if lines.is_empty() {
            return "(it has said nothing)".to_owned();
        }

        lines[lines.len().saturating_sub(TAIL)..].join("\n")
    }

    /// The drained diagnostics, readable through a lock a panicking test may
    /// have poisoned: what the server said is exactly what such a test needs.
    fn said(&self) -> MutexGuard<'_, String> {
        self.said.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        // Both are no-ops once the child has been waited on — the standard
        // library keeps the reaped status so a recycled pid can never be
        // signalled — so a test that stopped its server cleanly pays nothing
        // for this.
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The drain is left to end on its own: it does so as soon as the dead
        // child's pipe closes, and joining it here would hang this drop for as
        // long as anything else held that pipe open.
    }
}
