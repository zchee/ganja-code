//! Spec: pandaemonium `pkg/tmux/client.go`, `pkg/tmux/transport.go`,
//! `pkg/tmux/process_unix.go`, `pkg/tmux/process_other.go`.
//!
//! [`Client`] owns one persistent `tmux -C` subprocess. [`Client::new`]
//! spawns it, registers the configured initial command as the client's
//! first pending response, and waits for tmux's handshake reply before
//! returning. Callers then send further commands with [`Client::exec`],
//! [`Client::exec_line`], or [`Client::exec_raw`], read asynchronous `%`
//! notifications with [`Client::recv`] or [`Client::events`], and end the
//! session with [`Client::close`].
//!
//! # No public `Transport` trait (divergence)
//!
//! Go's `transport` interface exists so `client_test.go` can inject a
//! scripted double in place of the real `stdioTransport`. This port instead
//! gives every I/O half a common trait-object type
//! (`Box<dyn AsyncWrite + Unpin + Send>` / a [`tokio::io::BufReader`] over
//! `Box<dyn AsyncRead + Unpin + Send>`), so the real spawn path
//! ([`Client::new`]) and the scripted test path
//! (`Client::from_duplex`, `pub(crate)`, built on a [`tokio::io::duplex`]
//! pair) both construct the same [`Client`] shape without a public trait a
//! downstream crate would otherwise be tempted to implement against. An
//! external `-CC`/PTY-backed transport still has a first-class entry point:
//! [`crate::control_mode::Parser`] directly, exactly as in Go (see the crate
//! doc).
//!
//! # `Error` is `Clone` (divergence)
//!
//! See `error.rs`'s module doc. It is what lets [`Client`] hand every
//! subsequent caller of an already-closed client an owned copy of the one
//! abort cause, rather than only the first caller to observe it.
//!
//! # Cancellation: ctx-cancel becomes future-drop poisoning
//!
//! Go serializes commands by holding `writeMu` for the whole of `ExecRaw`,
//! including the `select` that waits for either the response or
//! `ctx.Done()`; a `ctx` cancellation while waiting aborts the whole client,
//! because a response that arrives after cancellation can no longer be
//! safely matched to a future command. This port has no `ctx` parameter —
//! [`Client::exec_raw`] holds an async mutex for its own entire body the
//! same way, so a caller who wants a timeout wraps the call in
//! [`tokio::time::timeout`] (or races it in a [`tokio::select!`]), and
//! **dropping that outer future mid-flight is the cancellation signal**. A
//! `PendingDropGuard` local to `exec_raw` poisons the client — marks it
//! closed with [`Error::Closed`] and fails the pending registration — but
//! only when *both* of these hold at the moment it is dropped: the write to
//! tmux has already been *attempted* — the guard arms the instant
//! `exec_raw` commits to writing, immediately before the first byte goes
//! out, not only once the write has finished. Go's own `WriteLine` is one
//! uninterruptible call with no cancellation point inside it, so a drop
//! while genuinely suspended partway through this port's own write is the
//! faithful translation of "from commitment onward, abandonment aborts";
//! only a drop *before* the write begins clears silently, since nothing was
//! sent that a late reply could misassociate. The second condition is this
//! call's own pending registration still being the one installed (a
//! response delivered concurrently with the drop clears the registration
//! first, in the same critical section as sending the result, so whichever
//! of the two — delivery or drop — wins the race, the other observes
//! nothing to do). A normal completion (successful or a `%error`-flagged
//! [`Error::Command`]) disarms the guard entirely. The write itself is
//! raced against a concurrent [`Client::close`] too, so a call genuinely
//! blocked inside it does not hang forever behind one — see
//! [`Client::close`]'s doc.
//!
//! ```no_run
//! # async fn run() -> Result<(), tmux::Error> {
//! use std::time::Duration;
//!
//! use tmux::control_mode::{Client, Options};
//!
//! let mut client = Client::new(Options::new().with_session_name("work")).await?;
//! if tokio::time::timeout(Duration::from_secs(2), client.exec_raw("list-panes")).await.is_err() {
//!     // The timeout dropped the exec future while it was waiting on tmux's
//!     // reply; per the rule above, the client is now poisoned. Every
//!     // further call returns `Error::Closed` — reconnect instead of retrying.
//!     let _ = client.close().await;
//!     client = Client::new(Options::new().with_session_name("work")).await?;
//! }
//! # let _ = client;
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::Stream;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Notify, oneshot};

use crate::control_mode::flow::DETACH_CLIENT;
use crate::control_mode::notification::{Notification, NotificationKind};
use crate::control_mode::options::Options;
use crate::control_mode::protocol::{Event, Parser, Response};
use crate::error::Error;

/// A boxed, type-erased half of a duplex byte stream — see the module doc's
/// "no public `Transport` trait" section.
type BoxedRead = Box<dyn AsyncRead + Unpin + Send>;
type BoxedWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// A persistent tmux control-mode client.
///
/// See the module doc.
pub struct Client {
    shared: Arc<Shared>,
    write: tokio::sync::Mutex<WriteState>,
    child: std::sync::Mutex<Option<Child>>,
    read_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    stderr_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    close_state: tokio::sync::Mutex<Option<Result<(), Error>>>,
    shutdown_timeout: Duration,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

/// State shared between [`Client`] and its spawned read/stderr tasks.
struct Shared {
    pending: std::sync::Mutex<Option<Pending>>,
    next_id: AtomicU64,
    state: std::sync::Mutex<ClosedState>,
    /// Fires once, on the state's `closed: false` → `true` transition. Lets
    /// [`Client::exec_raw`] race a genuinely blocked write against a
    /// concurrent abort instead of hanging on an OS write that may never
    /// drain — see the module doc's cancellation section and
    /// [`Client::close`]'s doc.
    close_notify: Notify,
    events: EventQueue,
    stderr: StderrRing,
}

struct ClosedState {
    closed: bool,
    error: Option<Error>,
}

struct Pending {
    id: u64,
    line: String,
    sender: oneshot::Sender<Result<Response, Error>>,
}

struct WriteState {
    writer: Option<BoxedWrite>,
}

impl Shared {
    fn new(event_buffer: usize, stderr_line_limit: usize) -> Self {
        Self {
            pending: std::sync::Mutex::new(None),
            next_id: AtomicU64::new(0),
            state: std::sync::Mutex::new(ClosedState { closed: false, error: None }),
            close_notify: Notify::new(),
            events: EventQueue::new(event_buffer),
            stderr: StderrRing::new(stderr_line_limit),
        }
    }

    fn next_pending_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers `pending` as the sole in-flight command.
    ///
    /// Ports Go's `registerPending`. Under normal operation this can never
    /// observe an existing registration — [`Client`]'s write mutex already
    /// serializes every caller across the whole of `exec_raw` — the check
    /// stays as the same defensive backstop Go's does.
    fn register_pending(&self, pending: Pending) -> Result<(), Error> {
        let mut guard = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_some() {
            return Err(Error::AlreadyPending);
        }
        *guard = Some(pending);
        Ok(())
    }

    /// Clears the pending registration iff it is still `id`. Returns
    /// whether it did — see the module doc's cancellation section for why
    /// this identity check is what makes the delivery/drop race safe.
    fn clear_pending_by_id(&self, id: u64) -> bool {
        let mut guard = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.as_ref().is_some_and(|pending| pending.id == id) {
            *guard = None;
            true
        } else {
            false
        }
    }

    /// Delivers a completed response block to the pending caller, if any.
    ///
    /// Ports Go's `deliverResponse`.
    fn deliver_response(&self, response: Response) {
        let pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(pending) = pending {
            let result = if response.error {
                Err(Error::Command(crate::error::CommandError { line: pending.line, response }))
            } else {
                Ok(response)
            };
            let _ = pending.sender.send(result);
        }
    }

    /// Fails the pending caller, if any, with `err`.
    ///
    /// Ports Go's `failPending`.
    fn fail_pending(&self, err: Error) {
        let pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(pending) = pending {
            let _ = pending.sender.send(Err(err));
        }
    }

    /// Ports Go's `markClosed`: the first cause wins. Notifies every
    /// [`Notify::notified`] waiter registered on [`Shared::close_notify`]
    /// — but only on the actual `false` → `true` transition, since that is
    /// the only time this ever fires.
    fn mark_closed(&self, err: Error) {
        {
            let mut guard = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.closed {
                return;
            }
            guard.closed = true;
            guard.error = Some(err);
        }
        self.close_notify.notify_waiters();
    }

    /// Ports Go's `closedError`: `None` while open, else the stored cause
    /// (cloned — see the module doc) or [`Error::Closed`] as the default.
    fn closed_error(&self) -> Option<Error> {
        let guard = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.closed {
            return None;
        }
        Some(guard.error.clone().unwrap_or(Error::Closed))
    }

    /// Ports Go's `abort`: marks the client closed and fails whatever is
    /// pending, in that order.
    fn abort(&self, err: Error) {
        self.mark_closed(err.clone());
        self.fail_pending(err);
    }
}

/// A bounded, drop-oldest queue of asynchronous notifications.
///
/// Ports the `events`/`droppedNotifications` half of Go's `Client`. `push`
/// is synchronous and never awaits (R3 in the port plan): the reader task
/// that feeds it must never block behind a slow consumer.
struct EventQueue {
    queue: std::sync::Mutex<VecDeque<Notification>>,
    capacity: usize,
    notify: Notify,
    dropped: AtomicU64,
    closed: AtomicBool,
}

impl EventQueue {
    fn new(capacity: usize) -> Self {
        Self {
            queue: std::sync::Mutex::new(VecDeque::new()),
            // Clamped to at least 1: `push`'s drop-oldest check degenerates
            // at capacity 0, permanently retaining one notification rather
            // than the intended zero. `Options::validate` already requires
            // `event_buffer > 0`, so 0 is reachable only through the
            // unvalidated `Client::from_duplex` test path.
            capacity: capacity.max(1),
            notify: Notify::new(),
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }

    /// Pushes `notification`, dropping the oldest buffered one when full.
    ///
    /// Ports Go's `deliverEvent`.
    fn push(&self, notification: Notification) {
        {
            let mut queue = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if queue.len() >= self.capacity && queue.pop_front().is_some() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            queue.push_back(notification);
        }
        self.notify.notify_one();
    }

    /// Marks the queue closed: no more pushes will arrive, so [`recv`]
    /// drains whatever remains and then returns `None` forever after.
    ///
    /// [`recv`]: EventQueue::recv
    fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Waits for and returns the next notification, or `None` once the
    /// queue is closed and drained.
    ///
    /// The condvar-loop pattern over [`Notify`]: the `notified()` future is
    /// created *before* the queue is checked, so a `push`/`mark_closed`
    /// racing this call between the check and the `.await` is not lost —
    /// Tokio's `Notify` records that a permit is due to whichever
    /// `Notified` future was already registered.
    async fn recv(&self) -> Option<Notification> {
        loop {
            let notified = self.notify.notified();
            if let Some(notification) =
                self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop_front()
            {
                return Some(notification);
            }
            if self.closed.load(Ordering::Acquire) {
                return self
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front();
            }
            notified.await;
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The bounded stderr tail. Ports Go's `stderrLines`/`appendStderrLine`.
struct StderrRing {
    lines: std::sync::Mutex<VecDeque<String>>,
    limit: usize,
}

impl StderrRing {
    fn new(limit: usize) -> Self {
        Self { lines: std::sync::Mutex::new(VecDeque::new()), limit }
    }

    fn push(&self, line: String) {
        if self.limit == 0 {
            return;
        }
        let mut lines = self.lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        lines.push_back(line);
        while lines.len() > self.limit {
            lines.pop_front();
        }
    }

    fn tail(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

impl Client {
    /// Starts a new persistent `tmux -C` control-mode client.
    ///
    /// Resolves the tmux executable (`options`' explicit path, or a bare
    /// `tmux` left to the platform's own `PATH` search at spawn time — a
    /// deliberate simplification of Go's separate `exec.LookPath` probe;
    /// both surface a "not found" failure readably, just at different
    /// call frames), spawns it with piped stdio and `kill_on_drop(true)`,
    /// registers the configured initial command as pending response #0,
    /// starts the read and stderr-drain tasks, and waits for that initial
    /// response before returning.
    ///
    /// Dropping the returned future before it resolves — for instance by
    /// wrapping this call in [`tokio::time::timeout`] — still reaps the
    /// spawned process: the local `Child` this function holds until it
    /// either succeeds or is handed to [`Client::close`] is armed with
    /// `kill_on_drop(true)`, so an abandoned handshake cannot leak a tmux
    /// subprocess. `Close` afterward owns the subprocess's whole lifetime,
    /// exactly as in Go.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptions`] when `options` fails validation,
    /// [`Error::Spawn`] when the executable cannot be found or started, and
    /// [`Error::Startup`] when the initial command's response is a
    /// `%error` block or the client aborts before it arrives.
    pub async fn new(options: Options) -> Result<Client, Error> {
        options.validate()?;

        let path = options
            .path()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("tmux"));

        let mut command = tokio::process::Command::new(&path);
        command.args(options.launch_args());
        if let Some(dir) = options.dir() {
            command.current_dir(dir);
        }
        if !options.env().is_empty() {
            command.envs(options.env().iter().cloned());
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| Error::Spawn {
            path: path.display().to_string(),
            source: Arc::new(source),
        })?;

        let stdin = child.stdin.take().expect("Stdio::piped() guarantees a stdin pipe");
        let stdout = child.stdout.take().expect("Stdio::piped() guarantees a stdout pipe");
        let stderr = child.stderr.take().expect("Stdio::piped() guarantees a stderr pipe");

        let shared = Arc::new(Shared::new(options.event_buffer(), options.stderr_line_limit()));

        let (tx, rx) = oneshot::channel();
        let startup_id = shared.next_pending_id();
        shared
            .register_pending(Pending {
                id: startup_id,
                line: options.initial_command_line(),
                sender: tx,
            })
            .expect("a freshly constructed client has no pending registration yet");

        let stderr_task =
            tokio::spawn(stderr_drain(Box::new(stderr) as BoxedRead, Arc::clone(&shared)));
        let read_task = tokio::spawn(read_loop(
            BufReader::new(Box::new(stdout) as BoxedRead),
            Arc::clone(&shared),
        ));

        let client = Client {
            shared,
            write: tokio::sync::Mutex::new(WriteState { writer: Some(Box::new(stdin)) }),
            child: std::sync::Mutex::new(Some(child)),
            read_task: std::sync::Mutex::new(Some(read_task)),
            stderr_task: std::sync::Mutex::new(Some(stderr_task)),
            close_state: tokio::sync::Mutex::new(None),
            shutdown_timeout: options.shutdown_timeout(),
        };

        match rx.await {
            Ok(Ok(_response)) => Ok(client),
            Ok(Err(err)) => {
                let _ = client.close().await;
                Err(Error::Startup { source: Box::new(err) })
            }
            Err(_sender_dropped) => {
                let _ = client.close().await;
                Err(Error::Startup { source: Box::new(Error::Closed) })
            }
        }
    }

    /// Builds a client over an in-memory [`tokio::io::duplex`] pair, in
    /// place of a real `tmux -C` subprocess.
    ///
    /// The scripted-test replacement for Go's injectable `transport` — see
    /// the module doc. Unlike [`Client::new`], no handshake runs: no
    /// pending response is registered, and the read (and, when `stderr` is
    /// given, stderr-drain) task starts immediately over whichever half of
    /// `io` the caller did not keep. `options` supplies only the three
    /// runtime tunables (`event_buffer`, `stderr_line_limit`,
    /// `shutdown_timeout`); its validation, path, and session/command
    /// fields go unused.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "only #[cfg(test)] tests call it — this module's and \
                      `flow`'s; a non-test build of the lib target never does"
        )
    )]
    pub(crate) fn from_duplex(
        options: &Options,
        io: tokio::io::DuplexStream,
        stderr: Option<tokio::io::DuplexStream>,
    ) -> Client {
        let (read_half, write_half) = tokio::io::split(io);
        let shared = Arc::new(Shared::new(options.event_buffer(), options.stderr_line_limit()));

        let read_task = tokio::spawn(read_loop(
            BufReader::new(Box::new(read_half) as BoxedRead),
            Arc::clone(&shared),
        ));
        let stderr_task = stderr.map(|stderr| {
            let (stderr_read, _stderr_write) = tokio::io::split(stderr);
            tokio::spawn(stderr_drain(Box::new(stderr_read) as BoxedRead, Arc::clone(&shared)))
        });

        Client {
            shared,
            write: tokio::sync::Mutex::new(WriteState { writer: Some(Box::new(write_half)) }),
            child: std::sync::Mutex::new(None),
            read_task: std::sync::Mutex::new(Some(read_task)),
            stderr_task: std::sync::Mutex::new(stderr_task),
            close_state: tokio::sync::Mutex::new(None),
            shutdown_timeout: options.shutdown_timeout(),
        }
    }

    /// Sends `command` with `args` and waits for its response block.
    ///
    /// See the module doc's cancellation section for what dropping the
    /// returned future mid-flight does.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCommand`] when rendering fails, and whatever
    /// [`Client::exec_raw`] can return otherwise.
    pub async fn exec(
        &self,
        command: crate::control_mode::commandline::Command,
        args: impl IntoIterator<Item = crate::control_mode::commandline::Arg>,
    ) -> Result<Response, Error> {
        self.exec_line(crate::control_mode::commandline::CommandLine::new(command, args)).await
    }

    /// Sends a pre-built [`crate::control_mode::CommandLine`] and waits for
    /// its response block. See [`Client::exec`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCommand`] when rendering fails, and whatever
    /// [`Client::exec_raw`] can return otherwise.
    pub async fn exec_line(
        &self,
        line: crate::control_mode::commandline::CommandLine,
    ) -> Result<Response, Error> {
        let rendered = line.render()?;
        self.exec_raw(&rendered).await
    }

    /// Sends one newline-framed raw tmux command line and waits for its
    /// response block.
    ///
    /// Commands are serialized: only one is ever in flight. See the module
    /// doc's cancellation section for the poisoning rule this method's
    /// returned future is subject to if dropped before completion.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCommand`] when `line` is blank or contains a
    /// newline, [`Error::Closed`] once the client has aborted, an
    /// [`Error::Io`] write failure, or [`Error::Command`] when tmux answers
    /// with a `%error` block.
    pub async fn exec_raw(&self, line: &str) -> Result<Response, Error> {
        crate::control_mode::commandline::validate_raw_line(line)?;

        let mut guard = self.write.lock().await;
        if let Some(err) = self.shared.closed_error() {
            return Err(err);
        }

        let id = self.shared.next_pending_id();
        let (tx, rx) = oneshot::channel();
        self.shared.register_pending(Pending { id, line: line.to_string(), sender: tx })?;

        let mut drop_guard =
            PendingDropGuard { shared: &self.shared, id, poison: false, completed: false };

        let Some(writer) = guard.writer.as_mut() else {
            self.shared.clear_pending_by_id(id);
            drop_guard.completed = true;
            return Err(Error::Closed);
        };

        // The write is about to begin: from here on, a drop while this
        // future is suspended — whether genuinely mid-write or later,
        // awaiting the response — leaves state a late reply or a stray
        // byte on the wire could misassociate with a future command, so
        // the guard must poison the client. See the module doc.
        drop_guard.poison = true;

        let payload = format!("{line}\n");

        // CRITICAL lost-wakeup discipline (the same one `EventQueue::recv`
        // relies on): the `notified()` future is created *before* the
        // closed flag is checked, so a `close()` — or any other abort —
        // landing between this check and the `select!` below is still
        // observed. A `Notified` created before a `notify_waiters()` call
        // is guaranteed to see it, regardless of whether it has been
        // polled yet.
        let closed = self.shared.close_notify.notified();
        if let Some(err) = self.shared.closed_error() {
            drop_guard.completed = true;
            return Err(err);
        }

        tokio::select! {
            write_result = write_command(writer, payload.as_bytes()) => {
                if let Err((stage, source)) = write_result {
                    self.shared.clear_pending_by_id(id);
                    drop_guard.completed = true;
                    let context = match stage {
                        WriteStage::Write => format!("write command {line:?}"),
                        WriteStage::Flush => format!("flush command {line:?}"),
                    };
                    return Err(Error::Io {
                        context,
                        source: Arc::new(source),
                    });
                }
            }
            () = closed => {
                // An abort — most likely `Client::close` — landed while
                // this call was genuinely blocked inside the write itself.
                // `abort` fails the pending registration before `close`
                // ever attempts the write lock (see `Client::close`'s
                // doc), so there is nothing left to reconcile here beyond
                // reporting the stored cause.
                drop_guard.completed = true;
                return Err(self.shared.closed_error().unwrap_or(Error::Closed));
            }
        }

        let result = match rx.await {
            Ok(result) => result,
            Err(_sender_dropped) => Err(Error::Closed),
        };
        drop_guard.completed = true;
        result
    }

    /// Waits for and returns the next asynchronous notification, or `None`
    /// once the client has closed and every buffered notification has been
    /// drained.
    ///
    /// Ports the receive half of Go's `Events` iterator. The queue is
    /// bounded by [`Options::event_buffer`]; see [`Client::dropped_notifications`].
    pub async fn recv(&self) -> Option<Notification> {
        self.shared.events.recv().await
    }

    /// Returns a [`Stream`] of asynchronous notifications, built on
    /// [`Client::recv`].
    ///
    /// Ports Go's `Events` iterator as a `Stream` rather than an `iter.Seq`
    /// — Rust has no native async-iterator syntax equivalent to Go's
    /// `range`-over-func, so the idiomatic Rust shape is a `Stream` a
    /// caller drives with `futures::StreamExt` (`.next()`, `while let
    /// Some(_) = stream.next().await`, …).
    pub fn events(&self) -> impl Stream<Item = Notification> + '_ {
        futures::stream::unfold(
            self,
            |client| async move { client.recv().await.map(|n| (n, client)) },
        )
    }

    /// Returns the notification backpressure counter: how many buffered
    /// notifications were dropped to keep the reader from blocking behind
    /// a slow consumer.
    ///
    /// Ports Go's `DroppedNotifications`. Approximate under concurrent
    /// consumption — see the crate doc.
    #[must_use]
    pub fn dropped_notifications(&self) -> u64 {
        self.shared.events.dropped()
    }

    /// Returns the bounded stderr tail retained for diagnostics.
    ///
    /// Ports Go's `StderrTail`.
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<String> {
        self.shared.stderr.tail()
    }

    /// Detaches and releases the tmux control-mode subprocess.
    ///
    /// Idempotent: every caller after the first observes the same result,
    /// and a caller that arrives while a close is already in progress waits
    /// for it to finish rather than racing it — both ported from Go's
    /// `closeMu` guard around `cleanupDone`/`cleanupErr`.
    ///
    /// Best-effort `detach-client` is attempted only if this client is not
    /// already closed and the write lock can be acquired without waiting
    /// ([`Error::DetachSkippedWriteLocked`] otherwise — Go's `TryLock`
    /// parity). **Divergence**: Go additionally closes its transport
    /// unconditionally afterward, even when the `TryLock` failed, because
    /// its transport's OS-level close can safely race an in-flight write on
    /// the same file descriptor. This port's writer still lives inside the
    /// same mutex the write lock guards, so there is still no way to reach
    /// in and force-close it out from under an exec call that holds that
    /// lock — but that call is no longer left to hang behind one: this
    /// method's own `abort` (above) wakes a genuinely blocked
    /// [`Client::exec_raw`] write through the same close-notification the
    /// module doc's cancellation section describes, so a write stuck
    /// inside the transport unblocks with this client's own abort cause
    /// rather than waiting on an OS write that may never drain.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Close`] carrying every failure observed: a skipped
    /// detach, a write/flush failure, the subprocess exiting abnormally or
    /// failing to exit within [`Options::with_shutdown_timeout`], or the
    /// read/stderr tasks failing to join within the same budget.
    pub async fn close(&self) -> Result<(), Error> {
        let mut cache = self.close_state.lock().await;
        if let Some(outcome) = cache.as_ref() {
            return outcome.clone();
        }
        let outcome = self.close_inner().await;
        *cache = Some(outcome.clone());
        outcome
    }

    async fn close_inner(&self) -> Result<(), Error> {
        let already_closed = self.shared.closed_error().is_some();
        self.shared.abort(Error::Closed);

        let mut errors = Vec::new();

        match self.write.try_lock() {
            Ok(mut guard) => {
                if !already_closed && let Some(writer) = guard.writer.as_mut() {
                    let payload = format!("{}\n", DETACH_CLIENT.as_str());
                    match writer.write_all(payload.as_bytes()).await {
                        Ok(()) => {
                            if let Err(source) = writer.flush().await {
                                errors.push(Error::Io {
                                    context: "flush detach-client".to_string(),
                                    source: Arc::new(source),
                                });
                            }
                        }
                        Err(source) => errors.push(Error::Io {
                            context: "write detach-client".to_string(),
                            source: Arc::new(source),
                        }),
                    }
                }
                guard.writer = None;
            }
            Err(_would_block) => errors.push(Error::DetachSkippedWriteLocked),
        }

        let taken_child =
            self.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(mut child) = taken_child
            && let Err(err) = wait_process(&mut child, self.shutdown_timeout).await
        {
            errors.push(err);
        }

        for (name, task) in
            [("stdout read loop", &self.read_task), ("stderr drain", &self.stderr_task)]
        {
            let handle = task.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            if let Some(handle) = handle
                && let Err(err) = wait_task(name, handle, self.shutdown_timeout).await
            {
                errors.push(err);
            }
        }

        if errors.is_empty() { Ok(()) } else { Err(Error::Close { errors }) }
    }
}

/// Which half of [`write_command`] a write failure came from, so
/// [`Client::exec_raw`] can still label the resulting [`Error::Io`] the way
/// it always has.
enum WriteStage {
    Write,
    Flush,
}

/// Writes and flushes one framed command line, tagging any failure with
/// which of the two steps it came from.
///
/// Split out of [`Client::exec_raw`] so the write can be raced against a
/// concurrent abort inside a [`tokio::select!`] — see the module doc's
/// cancellation section and [`Client::close`]'s doc. Go's own
/// `stdioTransport.WriteLine` has no such race: it is one uninterruptible
/// `io.WriteString` call.
async fn write_command(
    writer: &mut BoxedWrite,
    payload: &[u8],
) -> Result<(), (WriteStage, std::io::Error)> {
    writer.write_all(payload).await.map_err(|source| (WriteStage::Write, source))?;
    writer.flush().await.map_err(|source| (WriteStage::Flush, source))
}

/// A local guard on `exec_raw`'s stack. See the module doc's cancellation
/// section.
struct PendingDropGuard<'a> {
    shared: &'a Shared,
    id: u64,
    poison: bool,
    completed: bool,
}

impl Drop for PendingDropGuard<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let was_ours = self.shared.clear_pending_by_id(self.id);
        if was_ours && self.poison {
            self.shared.abort(Error::Closed);
        }
    }
}

async fn wait_task(
    name: &str,
    handle: tokio::task::JoinHandle<()>,
    timeout: Duration,
) -> Result<(), Error> {
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(join_error)) => Err(Error::Io {
            context: format!("{name} task"),
            source: Arc::new(std::io::Error::other(join_error.to_string())),
        }),
        Err(_elapsed) => Err(Error::Io {
            context: format!("wait for {name}"),
            source: Arc::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out")),
        }),
    }
}

/// Waits for the subprocess to exit within `timeout`, killing it and
/// re-waiting (bounded to one further second) on expiry.
///
/// Ports Go's `waitProcess`.
async fn wait_process(child: &mut Child, timeout: Duration) -> Result<(), Error> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => accept_exit(status),
        Ok(Err(source)) => Err(Error::Io {
            context: "wait for tmux process".to_string(),
            source: Arc::new(source),
        }),
        Err(_elapsed) => {
            let _ = child.start_kill();
            match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
                Ok(Ok(status)) => accept_exit(status),
                Ok(Err(source)) => Err(Error::Io {
                    context: "wait for tmux process after kill".to_string(),
                    source: Arc::new(source),
                }),
                Err(_elapsed_again) => Err(Error::Io {
                    context: "tmux process did not exit after kill".to_string(),
                    source: Arc::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out",
                    )),
                }),
            }
        }
    }
}

fn accept_exit(status: std::process::ExitStatus) -> Result<(), Error> {
    if status.success() || is_expected_kill(&status) {
        Ok(())
    } else {
        Err(Error::Io {
            context: "tmux process exited".to_string(),
            source: Arc::new(std::io::Error::other(format!("exit status: {status}"))),
        })
    }
}

/// Reports whether `status` is exactly the `SIGKILL` this client's own
/// `close` issued. Ports `process_unix.go`'s `processSignaledKilled`.
#[cfg(unix)]
fn is_expected_kill(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    // 9 is SIGKILL on every unix `close`'s `start_kill` can run on; naming
    // it as a bare constant (rather than pulling in `libc` for one signal
    // number) keeps this crate's dependency list at the three declared in
    // `Cargo.toml`.
    status.signal() == Some(9)
}

/// Reports whether `status` looks like the `SIGKILL` this client's own
/// `close` issued. Ports `process_other.go`'s string-matching fallback for
/// platforms with no `ExitStatusExt`.
#[cfg(not(unix))]
fn is_expected_kill(status: &std::process::ExitStatus) -> bool {
    format!("{status:?}").contains("signal: killed")
}

/// Owns the stdout reader for the client's lifetime: feeds every line
/// through a [`Parser`], delivers response blocks and pushes notifications,
/// and aborts the client on EOF, a protocol error, or a `%exit`
/// notification.
///
/// Ports Go's `readLoop`.
async fn read_loop(mut reader: BufReader<BoxedRead>, shared: Arc<Shared>) {
    let mut parser = Parser::default();
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => {
                let err = match parser.close() {
                    Ok(()) => Error::Io {
                        context: "read control-mode line".to_string(),
                        source: Arc::new(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
                    },
                    Err(protocol_error) => protocol_error.into(),
                };
                shared.abort(err);
                break;
            }
            Ok(_bytes) => {
                let line = trim_line_ending(&buf);
                match parser.feed(line) {
                    Ok(Some(Event::Response(response))) => shared.deliver_response(response),
                    Ok(Some(Event::Notification(notification))) => {
                        let exit = notification.exit();
                        shared.events.push(notification);
                        if let Some(exit) = exit {
                            shared.abort(Error::Exit { reason: exit.reason });
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(protocol_error) => {
                        shared.events.push(Notification {
                            kind: NotificationKind::ProtocolError,
                            raw: line.to_string(),
                            args: vec![protocol_error.message.clone()],
                        });
                        shared.abort(protocol_error.into());
                        break;
                    }
                }
            }
            Err(source) => {
                shared.abort(Error::Io {
                    context: "read control-mode line".to_string(),
                    source: Arc::new(source),
                });
                break;
            }
        }
    }
    shared.events.mark_closed();
}

/// Drains stderr into the bounded ring for as long as the pipe stays open.
///
/// Ports Go's `drainStderr`.
async fn stderr_drain(source: BoxedRead, shared: Arc<Shared>) {
    let mut reader = BufReader::new(source);
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => break,
            Ok(_bytes) => shared.stderr.push(trim_line_ending(&buf).to_string()),
            Err(err) => {
                if err.kind() != std::io::ErrorKind::BrokenPipe {
                    shared.stderr.push(format!("stderr read error: {err}"));
                }
                break;
            }
        }
    }
}

/// Strips a trailing `\n`, and then a trailing `\r`, from `line`. Ports
/// Go's `trimLineEnding`.
pub(crate) fn trim_line_ending(line: &str) -> &str {
    line.strip_suffix('\n').map_or(line, |rest| rest.strip_suffix('\r').unwrap_or(rest))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
