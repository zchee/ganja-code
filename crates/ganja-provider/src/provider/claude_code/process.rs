//! The seam a `claude` process is spawned across, and the real spawner.
//!
//! Unix-only, like the whole of this wire: the signals below go through
//! `libc`, which this crate names for unix targets alone.
//!
//! # Why a trait
//!
//! Every test in this suite drives a fake CLI, and none spawns the user's
//! own `claude`. That is not convenience: a live turn spends money on a real
//! account, and eight of the recording's sixteen paid runs were refused by a
//! vendor safeguard — the one thing a test must never be able to do is add to
//! that count. So the spawn is a trait with two implementations, and the real
//! one is reached by exactly one caller.
//!
//! # `kill_on_drop` is **off**, and that is the orderly exit
//!
//! With it on, *any* drop of the child — an error return, a panic unwind —
//! SIGKILLs the process, which contradicts "the only orderly exit is stdin
//! EOF" everywhere that sentence appears. With it off, dropping [`ChildIo`]
//! closes the pipe, and a closed pipe **is** EOF to the child: the orderly
//! exit happens by construction rather than by a signal. The recording
//! measured 8–14 ms from EOF to exit on every run that reached a `result`,
//! and nothing ever needed the driver's SIGTERM fallback.

use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitStatus, Stdio};

use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};

use super::argv::ChildEnv;
use crate::provider::ProviderError;

/// A signal sent to a child that would not take EOF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Asks it to end. The bound on a process that ignored EOF.
    Term,
    /// Ends it. The bound on one that ignored both.
    Kill,
}

/// One spawned child's pipes and its ending.
pub struct ChildIo {
    /// What the wire writes frames to. Dropping it is EOF to the child.
    pub stdin: Box<dyn AsyncWrite + Send + Unpin>,
    /// What the wire reads frames from.
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    /// The vendor's own diagnostics, logged verbatim and never parsed for
    /// anything but the two sentences that decide an `Auth` from a
    /// `Transport` before `system/init`.
    pub stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    /// Resolves when the child exits.
    pub exit: BoxFuture<'static, std::io::Result<ExitStatus>>,
    /// Sends the child a signal.
    ///
    /// `Fn`, not `FnOnce`, and aimed at the child's **pid** rather than its
    /// `Child` handle, because the `SIGKILL` bound had two independent
    /// reasons it could never fire (CC-3): a driver that consumed a `FnOnce`
    /// on the first bound found nothing to call for the second, and a `Child`
    /// behind a lock the exit future held across `wait()` was a handle no
    /// other arm could reach while the child lived. Sending a signal to a
    /// child that has already exited is `ESRCH`, so calling it twice costs
    /// nothing when the first one worked.
    pub kill: Box<dyn Fn(Signal) + Send>,
}

impl std::fmt::Debug for ChildIo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChildIo")
            .field("stderr", &self.stderr.is_some())
            .finish_non_exhaustive()
    }
}

/// How a `claude` process comes into being.
///
/// The environment value carries the cwd, so the seam takes no separate
/// directory parameter and a fake records what a child was spawned in beside
/// what it was spawned with — one side file, one reading.
pub trait Spawner: Send + Sync {
    /// Spawns `bin` with `argv` under `env`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError::Transport`] naming the binary when it could
    /// not be started at all.
    fn spawn(
        &self,
        bin: &Path,
        argv: &[OsString],
        env: &ChildEnv,
    ) -> Result<ChildIo, ProviderError>;
}

/// The spawner that runs the user's own `claude`.
#[derive(Debug, Default)]
pub struct Real;

impl Spawner for Real {
    fn spawn(
        &self,
        bin: &Path,
        argv: &[OsString],
        env: &ChildEnv,
    ) -> Result<ChildIo, ProviderError> {
        let mut command = tokio::process::Command::new(bin);
        command
            .args(argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // See the module doc.
            .kill_on_drop(false);
        env.apply(&mut command);

        // The argv holds a session id and a model and no secret, so it is
        // safe to render; the environment is never rendered at all.
        tracing::info!(
            provider = super::ID,
            bin = %bin.display(),
            cwd = %env.cwd.display(),
            argv = ?argv,
            "spawning"
        );

        let mut child = command.spawn().map_err(|error| {
            ProviderError::Transport(format!("could not start {}: {error}", bin.display()))
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Transport("the claude process gave no stdin pipe".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::Transport("the claude process gave no stdout pipe".to_owned())
        })?;
        let stderr = child.stderr.take();

        // The handle goes to the exit future, which reaps; the killer takes the
        // pid, so both signals go the same way (CC-3, told on `ChildIo::kill`).
        let pid = child.id();
        let exit: BoxFuture<'static, std::io::Result<ExitStatus>> =
            Box::pin(async move { child.wait().await });

        Ok(ChildIo {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: stderr.map(|stderr| Box::new(stderr) as Box<dyn AsyncRead + Send + Unpin>),
            exit,
            kill: Box::new(move |signal| {
                // `Child::kill` is always SIGKILL, and this wire wants SIGTERM
                // first — so neither arm uses it and both go through `libc` at
                // the pid. Symmetric on purpose: the second bound exists for a
                // child that ignored the first, and an arm that could only
                // fire while the first was still working would be no bound at
                // all.
                let number = match signal {
                    Signal::Term => libc::SIGTERM,
                    Signal::Kill => libc::SIGKILL,
                };

                let Some(pid) = narrowed(pid) else {
                    // Never `kill(0, …)`, which signals every process in
                    // ganja's own group — on a tmux-pane teammate arrangement,
                    // whatever shares it. A pid this code could not narrow is
                    // precisely the situation in which broadcasting a signal
                    // is least defensible, so it sends none (CC-7).
                    tracing::warn!(
                        provider = super::ID,
                        ?pid,
                        signal = ?signal,
                        "not signalling: the child's pid does not fit a pid_t"
                    );

                    return;
                };

                // SAFETY: `kill(2)` with a pid this process owns — it spawned
                // it, and both arms are reached only while the exit future is
                // still pending, so `wait()` has not returned and the pid has
                // not been reaped into reuse — and a valid signal number. It
                // touches no memory of ours and cannot fail in a way that
                // matters here: the one failure mode is ESRCH, a child that
                // already exited, which is the outcome being asked for.
                //
                // The window the reap could open is between the driver's
                // `timeout` expiring and this call: microseconds wide, and
                // monotonic pid allocation makes reuse inside it effectively
                // impossible.
                unsafe {
                    libc::kill(pid, number);
                }
            }),
        })
    }
}

/// The `pid_t` a signal may be sent to, or [`None`] for a pid there is none.
///
/// Its own function so the refusal is testable without a child: no platform
/// this builds for allocates a `u32` pid above `i32::MAX`, so the arm is
/// unreachable in practice and a test is the only thing that can say it does
/// the right thing.
fn narrowed(pid: Option<u32>) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid?).ok()
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
