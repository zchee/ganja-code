//! Background shell jobs: `bash` calls started with `run_in_background:
//! true` that return immediately and keep running beside the turn that
//! started them.
//!
//! Spec: Claude Code's `run_in_background`/`BashOutput`/`KillShell` (2.1.x;
//! docs at code.claude.com/docs/en/interactive-mode, wire behavior verified
//! via gemini-search 2026-08-11). Upstream opencode has no equivalent —
//! **D454** (`background-execution-is-a-claude-port`) — so nothing here
//! ports a TypeScript file; the shape below is this port's own reading of
//! the observed contract: a call returns a job id immediately, `bash_output`
//! answers with whatever is new since the caller's last poll (there is
//! exactly one caller per job in this port's shape, so one cursor per job is
//! the whole of "per consumer"), and `kill_shell` ends the tree outright.
//!
//! # Lifetime
//!
//! A job outlives the turn that started it. [`crate::job::JobRegistry`] owns one root
//! [`tokio_util::sync::CancellationToken`]; every job's own token is a child of it, so a
//! turn's cancel — which fires the *turn's* token, never this one — leaves a
//! background job running, and [`crate::job::JobRegistry::shutdown`] (the engine's own
//! exit path, called wherever a frontend already calls
//! [`crate::engine::Engine::shutdown_mcp`]/`shutdown_lsp`) takes every job
//! down at once through the same `SIGTERM`-then-`SIGKILL` sequence a
//! foreground command's timeout uses
//! ([`ganja_tool::shell::kill_tree`], widened to `pub` for exactly this
//! reuse — see that function's doc comment).
//!
//! A job publishes no events through any turn's fanout: `Engine::subscribe`
//! is the frontier every root event crosses, and a background job has no
//! turn to ride along with by the time anybody reads its output.
//!
//! **D455**: a background job ignores the `timeout` argument entirely — see
//! [`ganja_tool::shell::ShellTool`]'s background branch, which is where that
//! decision is made and documented; nothing here ever sees a deadline to
//! apply.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use ganja_tool::{
    job::{JobRead, JobStatus, JobsError, State},
    shell::kill_tree,
    truncate,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::Child,
    sync::Notify,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// Bytes kept in memory, undelivered, before the oldest are dropped. Mirrors
/// `ganja_tool::shell`'s `KEEP` discipline — the same budget, for the same
/// reason: a job nobody has polled in a while is free to write more than a
/// machine has memory for.
const KEEP: usize = truncate::MAX_CHARS * 2;

/// Bytes written before a job's output starts spilling to disk. Mirrors
/// `ganja_tool::shell`'s `SPILL_THRESHOLD`.
const SPILL_THRESHOLD: usize = truncate::MAX_CHARS;

/// How long a killed job's pump is given to drain what is already in the
/// pipe before it is aborted outright. Mirrors `ganja_tool::shell`'s
/// `DRAIN_GRACE`.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

/// Tracks and runs every background job this session has started.
///
/// One per engine, always present: unlike MCP or LSP, which a config
/// switches on, there is no engine that cannot run a background job, so
/// there is nothing here for an engine to opt into.
pub struct JobRegistry {
    /// Every job's own token is a child of this one, so
    /// [`JobRegistry::shutdown`] ends them all with a single cancel.
    root: CancellationToken,
    next: AtomicU64,
    jobs: Mutex<BTreeMap<String, Arc<Job>>>,
    /// The pump-and-wait task each [`JobRegistry::start`] spawns, kept so
    /// [`JobRegistry::shutdown`] can wait for every one of them to actually
    /// finish winding down — not merely ask them to.
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for JobRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JobRegistry")
            .field(
                "jobs",
                &self
                    .jobs
                    .lock()
                    .expect("the job map is never poisoned")
                    .len(),
            )
            .finish()
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    /// Builds an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: CancellationToken::new(),
            next: AtomicU64::new(1),
            jobs: Mutex::new(BTreeMap::new()),
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Ends every job's process tree, and waits for each one's own cleanup
    /// task to finish — so a caller that awaits this knows the processes are
    /// gone before it returns, not only asked to go. Idempotent: a registry
    /// with nothing running has nothing to wait for.
    pub async fn shutdown(&self) {
        self.root.cancel();
        let handles =
            std::mem::take(&mut *self.tasks.lock().expect("the task list is never poisoned"));
        for handle in handles {
            let _ = handle.await;
        }
    }

    fn find(&self, bash_id: &str) -> Result<Arc<Job>, JobsError> {
        self.jobs
            .lock()
            .expect("the job map is never poisoned")
            .get(bash_id)
            .cloned()
            .ok_or_else(|| JobsError::NotFound(bash_id.to_owned()))
    }
}

#[async_trait]
impl ganja_tool::job::Jobs for JobRegistry {
    async fn start(&self, command: String, mut child: Child) -> JobStatus {
        let id = format!("bash_{}", self.next.fetch_add(1, Ordering::Relaxed));
        let stdout = child.stdout.take().expect("the child's stdout was piped");
        let stderr = child.stderr.take().expect("the child's stderr was piped");

        let job = Arc::new(Job {
            id: id.clone(),
            command: command.clone(),
            state: Mutex::new(State::Running),
            buffer: Mutex::new(Buffer::default()),
            cancel: self.root.child_token(),
            done: Notify::new(),
        });
        self.jobs
            .lock()
            .expect("the job map is never poisoned")
            .insert(id.clone(), Arc::clone(&job));

        let watched = Arc::clone(&job);
        let handle = tokio::spawn(async move {
            run_job(watched, child, stdout, stderr).await;
        });
        self.tasks
            .lock()
            .expect("the task list is never poisoned")
            .push(handle);

        job.status()
    }

    async fn output(&self, bash_id: &str) -> Result<JobRead, JobsError> {
        let job = self.find(bash_id)?;
        let (bytes, dropped, spill) = job
            .buffer
            .lock()
            .expect("a job's buffer is never poisoned")
            .drain();

        Ok(JobRead {
            chunk: render_chunk(&bytes, dropped, spill.as_deref()),
            status: job.status(),
        })
    }

    async fn kill(&self, bash_id: &str) -> Result<JobStatus, JobsError> {
        let job = self.find(bash_id)?;
        let running = matches!(
            *job.state.lock().expect("a job's state is never poisoned"),
            State::Running
        );
        if running {
            job.cancel.cancel();
            job.done.notified().await;
        }

        Ok(job.status())
    }

    fn list(&self) -> Vec<JobStatus> {
        self.jobs
            .lock()
            .expect("the job map is never poisoned")
            .values()
            .map(|job| job.status())
            .collect()
    }
}

/// One background job: its identity, where it stands, and what it has
/// written that nobody has read yet.
struct Job {
    id: String,
    command: String,
    state: Mutex<State>,
    buffer: Mutex<Buffer>,
    /// This job's own kill switch — a child of [`JobRegistry::root`], so
    /// [`JobRegistry::shutdown`] fires it too, and never the token of
    /// whichever turn's `bash` call registered it.
    cancel: CancellationToken,
    /// Fired exactly once, by [`run_job`], the moment [`Job::state`] becomes
    /// terminal — what [`ganja_tool::job::Jobs::kill`] waits on so it answers
    /// with the job's *actual* terminal status rather than a still-`Running`
    /// one raced against the kill it just asked for.
    done: Notify,
}

impl Job {
    fn status(&self) -> JobStatus {
        JobStatus {
            id: self.id.clone(),
            command: self.command.clone(),
            state: self
                .state
                .lock()
                .expect("a job's state is never poisoned")
                .clone(),
        }
    }
}

/// How a job's process ended.
enum Ended {
    /// It exited on its own.
    Exit(Option<i32>),
    /// It was killed — by an explicit `kill_shell`, or by
    /// [`JobRegistry::shutdown`].
    Killed,
}

/// Pumps a job's output for as long as it runs, then waits for it to end —
/// on its own, or because [`Job::cancel`] fired — and marks it terminal.
///
/// Mirrors `ganja_tool::shell::ShellTool::run_reporting`'s shape: pumps spawned
/// alongside a `select!` between the child's own exit and a cancel, `kill_tree`
/// only on the branch that needed it, a bounded drain grace before the pumps
/// are abandoned. What differs is the ending — there is no timeout, no result
/// to return, and no turn waiting on this future — so it runs to completion in
/// its own spawned task instead of being polled by a tool call.
async fn run_job(
    job: Arc<Job>,
    mut child: Child,
    stdout: impl AsyncRead + Unpin + Send + 'static,
    stderr: impl AsyncRead + Unpin + Send + 'static,
) {
    let out_job = Arc::clone(&job);
    let err_job = Arc::clone(&job);
    let mut pumps = tokio::spawn(async move {
        tokio::join!(pump(stdout, &out_job), pump(stderr, &err_job));
    });

    let ended = tokio::select! {
        status = child.wait() => match status {
            Ok(status) => Ended::Exit(status.code()),
            Err(error) => {
                tracing::warn!(%error, job = %job.id, "a background job could not be waited on");
                Ended::Exit(None)
            }
        },
        () = job.cancel.cancelled() => Ended::Killed,
    };

    if matches!(ended, Ended::Killed) {
        kill_tree(&mut child).await;
        // Reaping is what keeps a killed job from leaving a zombie behind.
        let _ = child.wait().await;
    }

    if tokio::time::timeout(DRAIN_GRACE, &mut pumps).await.is_err() {
        pumps.abort();
    }

    let final_state = match ended {
        Ended::Exit(code) => State::Exited { code },
        Ended::Killed => State::Killed,
    };
    *job.state.lock().expect("a job's state is never poisoned") = final_state;
    job.done.notify_one();
}

/// Hands everything `reader` produces to `job`'s buffer.
async fn pump(mut reader: impl AsyncRead + Unpin, job: &Arc<Job>) {
    let mut chunk = [0_u8; 8192];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => job
                .buffer
                .lock()
                .expect("a job's buffer is never poisoned")
                .push(&chunk[..read]),
        }
    }
}

/// Where a job's output goes between polls: a bounded, undelivered queue, and
/// the whole record spilled to disk once it grows past a budget worth
/// keeping in memory.
///
/// The tail-ring-plus-spill discipline `ganja_tool::shell::Collector` uses
/// for one finished command's whole output, adapted for a job that is read
/// more than once: [`Buffer::drain`] is callable repeatedly, taking only what
/// has arrived since the last call, where `Collector::finish` is one-shot.
#[derive(Default)]
struct Buffer {
    /// Bytes produced since the last [`Buffer::drain`].
    pending: Vec<u8>,
    /// Whether some of `pending` was dropped — grown past [`KEEP`] — before
    /// ever being delivered.
    dropped: bool,
    spill: Spill,
}

/// The file a job's whole output is kept in, and how far along it is.
/// Mirrors `ganja_tool::shell::Spill`, minus the window-vs-spill distinction
/// that type draws for a one-shot `finish` — this one's `pending` already
/// serves that role.
enum Spill {
    /// No file yet; everything the job has written so far, bounded by
    /// [`SPILL_THRESHOLD`].
    Holding(Vec<u8>),
    /// Open, with everything since appended to it as it arrives.
    Open(PathBuf, std::fs::File),
    /// Nothing more can be written; the file that was open when writing
    /// stopped, or [`None`] when there never was one.
    Refused(Option<PathBuf>),
}

impl Default for Spill {
    fn default() -> Self {
        Self::Holding(Vec::new())
    }
}

impl Buffer {
    /// Takes one chunk of a job's output.
    fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        if self.pending.len() > KEEP {
            let excess = self.pending.len() - KEEP;
            self.pending.drain(..excess);
            self.dropped = true;
        }

        self.spill = match std::mem::take(&mut self.spill) {
            Spill::Open(path, mut file) => {
                use std::io::Write as _;

                if file.write_all(chunk).is_ok() {
                    Spill::Open(path, file)
                } else {
                    self.dropped = true;
                    Spill::Refused(Some(path))
                }
            }
            Spill::Refused(path) => Spill::Refused(path),
            Spill::Holding(mut bytes) => {
                bytes.extend_from_slice(chunk);
                if bytes.len() <= SPILL_THRESHOLD {
                    Spill::Holding(bytes)
                } else {
                    match truncate::open_spill(&bytes) {
                        Some((path, file)) => Spill::Open(path, file),
                        None => {
                            self.dropped = true;
                            Spill::Refused(None)
                        }
                    }
                }
            }
        };
    }

    /// Everything produced since the last call, whether anything was
    /// dropped on the way, and the spill file if one exists.
    fn drain(&mut self) -> (Vec<u8>, bool, Option<PathBuf>) {
        let bytes = std::mem::take(&mut self.pending);
        let dropped = std::mem::take(&mut self.dropped);
        let spill = match &self.spill {
            Spill::Open(path, _) | Spill::Refused(Some(path)) => Some(path.clone()),
            Spill::Holding(_) | Spill::Refused(None) => None,
        };

        (bytes, dropped, spill)
    }
}

/// Turns what [`Buffer::drain`] took into the text a `bash_output` call
/// reads.
fn render_chunk(bytes: &[u8], dropped: bool, spill: Option<&std::path::Path>) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    if !dropped {
        return text;
    }

    match spill {
        Some(path) => format!(
            "...output produced between polls was truncated...\n\nFull output saved to: {}\n\n{text}",
            path.display()
        ),
        None => format!("...output produced between polls was truncated...\n\n{text}"),
    }
}

// Unix-only as a module rather than test by test: every test but one drives
// a real `sh` through the registry, and the tree-kill tests assert
// process-group semantics that have no Windows spelling.
#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use ganja_tool::job::{Jobs as _, JobsError, State};
    use tokio::process::Command;

    use super::JobRegistry;

    /// A command this platform's shell can run, split the way
    /// [`tokio::process::Command`] wants it, mirroring the two-argument
    /// shape `ganja_tool::shell::ShellTool::spawn` builds.
    fn shell(command: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::null());
        // A group of its own, the way the real tool spawns one — required for
        // the leak test's `killpg`-reaches-the-tree claim to mean anything.
        // `pre_exec` is `tokio::process::Command`'s own unix extension
        // method; no `std::os::unix::process::CommandExt` import needed.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        cmd
    }

    #[tokio::test]
    async fn a_started_job_is_running_and_answers_to_its_own_id() {
        let registry = JobRegistry::new();
        let status = registry
            .start(
                "sleep 5".to_owned(),
                shell("sleep 5").spawn().expect("sh spawns"),
            )
            .await;

        assert_eq!(status.state, State::Running);
        assert!(status.id.starts_with("bash_"));

        registry.shutdown().await;
    }

    #[tokio::test]
    async fn ids_are_assigned_in_order_and_never_reused() {
        let registry = JobRegistry::new();
        let first = registry
            .start("true".to_owned(), shell("true").spawn().expect("sh spawns"))
            .await;
        let second = registry
            .start("true".to_owned(), shell("true").spawn().expect("sh spawns"))
            .await;

        assert_ne!(first.id, second.id);
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn output_is_delivered_once_and_then_only_whats_new() {
        let registry = JobRegistry::new();
        let status = registry
            .start(
                "echo one; sleep 0.2; echo two".to_owned(),
                shell("echo one; sleep 0.2; echo two")
                    .spawn()
                    .expect("sh spawns"),
            )
            .await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        let first = registry
            .output(&status.id)
            .await
            .expect("a known id answers");
        assert!(first.chunk.contains("one"), "got {:?}", first.chunk);
        assert!(!first.chunk.contains("two"), "got {:?}", first.chunk);

        tokio::time::sleep(Duration::from_millis(400)).await;
        let second = registry
            .output(&status.id)
            .await
            .expect("a known id answers");
        assert!(second.chunk.contains("two"), "got {:?}", second.chunk);
        assert!(
            !second.chunk.contains("one"),
            "the first poll already delivered it"
        );
        assert!(matches!(
            second.status.state,
            State::Exited { code: Some(0) }
        ));

        let third = registry
            .output(&status.id)
            .await
            .expect("a known id answers");
        assert!(third.chunk.is_empty(), "nothing new since the second poll");

        registry.shutdown().await;
    }

    #[tokio::test]
    async fn an_unknown_id_is_refused_by_name() {
        let registry = JobRegistry::new();

        let refused = registry
            .output("bash_9")
            .await
            .expect_err("nothing registered that id");
        assert_eq!(refused, JobsError::NotFound("bash_9".to_owned()));

        let refused = registry
            .kill("bash_9")
            .await
            .expect_err("nothing registered that id");
        assert_eq!(refused, JobsError::NotFound("bash_9".to_owned()));
    }

    #[tokio::test]
    async fn killing_a_job_ends_its_whole_process_tree() {
        let registry = JobRegistry::new();
        let dir = tempfile::tempdir().expect("a scratch directory");
        let forked = dir.path().join("forked");
        let survived = dir.path().join("survived");
        let command = format!(
            "( touch {forked}; sleep 3; touch {survived} ) & sleep 30",
            forked = forked.display(),
            survived = survived.display(),
        );

        let status = registry
            .start(command.clone(), shell(&command).spawn().expect("sh spawns"))
            .await;

        while !forked.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let killed = registry
            .kill(&status.id)
            .await
            .expect("a running job can be killed");
        assert_eq!(killed.state, State::Killed);

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !survived.exists(),
            "the grandchild outlived the kill; the tree was not reached"
        );
    }

    /// Killing something already dead is not an error: the second call
    /// answers with the terminal status it already had.
    #[tokio::test]
    async fn killing_an_already_exited_job_is_answered_not_refused() {
        let registry = JobRegistry::new();
        let status = registry
            .start("true".to_owned(), shell("true").spawn().expect("sh spawns"))
            .await;

        // Give the job a moment to actually exit before asking again.
        loop {
            let read = registry
                .output(&status.id)
                .await
                .expect("a known id answers");
            if !matches!(read.status.state, State::Running) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let answer = registry
            .kill(&status.id)
            .await
            .expect("an already-dead job is still answered");
        assert!(matches!(answer.state, State::Exited { code: Some(0) }));
    }

    /// A turn's cancel is never this registry's token — proven here by never
    /// touching the job's own `cancel` at all and observing it keeps running.
    #[tokio::test]
    async fn a_job_outlives_the_registry_without_being_asked_to_stop() {
        let registry = JobRegistry::new();
        let status = registry
            .start(
                "sleep 1".to_owned(),
                shell("sleep 1").spawn().expect("sh spawns"),
            )
            .await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        let mid = registry
            .output(&status.id)
            .await
            .expect("a known id answers");
        assert_eq!(
            mid.status.state,
            State::Running,
            "nothing here should have stopped it"
        );

        registry.shutdown().await;
    }

    /// The leak test: a sleeping process tree really is gone once
    /// [`JobRegistry::shutdown`] returns, not merely asked to go.
    #[tokio::test]
    async fn shutdown_kills_every_running_jobs_whole_process_tree() {
        let registry = JobRegistry::new();
        let dir = tempfile::tempdir().expect("a scratch directory");
        let forked = dir.path().join("forked");
        let survived = dir.path().join("survived");
        let command = format!(
            "( touch {forked}; sleep 3; touch {survived} ) & sleep 30",
            forked = forked.display(),
            survived = survived.display(),
        );

        registry
            .start(command.clone(), shell(&command).spawn().expect("sh spawns"))
            .await;

        while !forked.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        registry.shutdown().await;

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !survived.exists(),
            "a job outlived engine shutdown; the tree was not reached"
        );
    }
}
