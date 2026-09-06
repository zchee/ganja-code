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
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long any single wait may take before the fixture is declared broken.
pub const DEADLINE: Duration = Duration::from_secs(60);

/// What no `ganja` this suite starts may inherit from the machine running it.
///
/// One list, because two copies of it are two different experiments wearing
/// one name: a credential left on would make a wire reachable, a provider or a
/// model would decide what answered, a config home would hand the process a
/// tier no assertion accounts for, and a server password would secure a server
/// whose unsecured warning is the thing being read. It is a `const` rather
/// than nine lines at each spawn so that a name added here reaches every one
/// of them — the servers `attach.rs` and `serve.rs` start, and the clients
/// they run against those servers.
pub const UNINHERITED: &[&str] = &[
    "GANJA_CONFIG_HOME",
    "GANJA_PROVIDER",
    "GANJA_MODEL",
    "GANJA_CONFIG",
    "GANJA_SERVER_PASSWORD",
    "GANJA_SERVER_USERNAME",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
];

/// Spawns `ganja serve --port 0` in `project`, playing `script`, with all
/// three homes pinned and [`UNINHERITED`] taken out — and hands it straight to
/// [`Reaped`], which is what the caller must not get wrong.
///
/// `home` is separate from `data` and `config` because the two callers
/// disagree about it and both are right: ganja's global config home resolves
/// against `GANJA_CONFIG_HOME`, `XDG_CONFIG_HOME` and `HOME` in that order, so
/// whichever of the two directories `HOME` names, it has to be one this
/// fixture owns rather than the developer's.
///
/// Wrapped before it is returned: every panic in a caller's own startup
/// checks — an announcement's deadline, a line that is not an address, a port
/// that is not a number — then unwinds through the kill that `Child` itself
/// would not do (bead `pjc`).
pub fn spawn(project: &Path, data: &Path, config: &Path, home: &Path, script: &Path) -> Reaped {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        .args(["serve", "--port", "0"])
        .current_dir(project)
        .env("XDG_DATA_HOME", data)
        .env("XDG_CONFIG_HOME", config)
        .env("HOME", home)
        .env("GANJA_FAKE_SCRIPT", script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in UNINHERITED {
        command.env_remove(name);
    }

    Reaped::new(command.spawn().expect("the binary starts"))
}

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
        if line.trim().is_empty() {
            // The failure this is about is a server that exited instead of
            // announcing itself, and what it exited *saying* is the whole
            // diagnosis — so the drain is waited on before the message is
            // built. [`Reaped::state`] reads whatever had arrived by now,
            // which for a child that has only just died is a refusal cut off
            // mid-sentence. Only once it has exited: waiting on a live
            // server's drain would spend the bound on a pipe that is not going
            // to close.
            //
            // On what is left of this wait's own [`DEADLINE`], and through
            // [`Reaped::drained`] rather than [`Reaped::diagnostics`]: a drain
            // that will not close is a detail missing from this failure, not
            // the failure itself, so it may neither panic in place of the
            // message below — the one that names what was being waited for —
            // nor spend a second deadline on top of the one already gone.
            // Whatever had arrived by then is what the message carries.
            if self.exited().is_some() {
                let _ = self.drained(DEADLINE.saturating_sub(started.elapsed()));
            }
            panic!("{}", self.gave_up(what, started));
        }

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
        let started = Instant::now();
        assert!(
            self.drained(DEADLINE),
            "{}",
            self.gave_up("the server's standard error to close", started)
        );

        self.said().clone()
    }

    /// Waits up to `within` for the drain to end, and answers whether it did.
    ///
    /// Nothing here panics, because the two callers disagree about what a
    /// drain that will not close means: for [`Reaped::diagnostics`] it is the
    /// failure, and for [`Reaped::announcement`] it is a detail missing from
    /// one. A drain that outlasted `within` is put back, so a later caller
    /// waits on the same thread rather than on nothing.
    fn drained(&mut self, within: Duration) -> bool {
        let Some(draining) = self.draining.take() else {
            return true;
        };

        let started = Instant::now();
        while !draining.is_finished() {
            if started.elapsed() >= within {
                self.draining = Some(draining);
                return false;
            }
            std::thread::sleep(POLL);
        }
        let _ = draining.join();

        true
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
