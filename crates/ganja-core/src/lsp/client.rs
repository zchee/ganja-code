//! One language server process, and the conversation with it.
//!
//! Spec: upstream `packages/opencode/src/lsp/client.ts` — the handshake, the
//! document table, the push-diagnostics cache and the freshness wait.
//!
//! The transport is hand-rolled, the way `provider/sse.rs` is: LSP framing is
//! a `Content-Length` header and a JSON body, and a crate that reads that for
//! us would also bring a runtime, an error taxonomy and a version to track.
//!
//! # Both diagnostic channels, because one of them is not enough
//!
//! Diagnostics arrive two ways: pushed over `textDocument/publishDiagnostics`,
//! and pulled with `textDocument/diagnostic`. Upstream implements both and so
//! does this, and the reason is worth recording, because "push is enough" is
//! the obvious guess and it is measurably wrong.
//!
//! rust-analyzer pushes freely while it is loading a workspace, and then
//! effectively stops: after the initial analysis it answers an edited buffer by
//! sending `workspace/diagnostic/refresh` — *ask me* — and publishes nothing
//! further for that file. Measured against rust-analyzer 1.99.0-nightly on a
//! two-file fixture crate, an edit that introduced a type error produced no
//! publish at all in thirty seconds, while a pull issued at the same moment
//! returned the `E0308` immediately. A push-only client does not see a slower
//! diagnostic; it sees none (deviation: lsp-document-pull).
//!
//! What stays unported is the rest of upstream's pull machinery: **workspace**
//! diagnostics, and the `client/registerCapability` tracking that re-enters the
//! wait when a server registers pull support mid-session. Registrations are
//! answered — refusing a request a server is entitled to make is a different
//! thing from ignoring what it announces — but not acted on, because the
//! document pull is issued unconditionally anyway and a server without one
//! simply says so.
//!
//! There is no `shutdown`/`exit` handshake either: a client that is done with
//! is a process that is killed (upstream `util/process.ts:149-163`). A
//! language server holds no state worth a graceful goodbye, and one that hangs
//! on shutdown would hang the frontend's exit.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use lsp_types::{Diagnostic, PublishDiagnosticsParams, TextDocumentSyncKind};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, Command},
    sync::{Notify, oneshot},
    // The runtime's clock rather than the standard library's, so that the
    // freshness window and the debounce below are the same code under a test
    // that drives time and under a session that lives in it.
    time::Instant,
};

use super::{language::language_id, server::Spec};

/// How long a publish must stay the newest one before the wait is satisfied
/// (`client.ts:13`).
///
/// A server that is still analysing publishes repeatedly; answering the first
/// of those would hand the model a half-finished picture. Each further publish
/// restarts this.
pub const DIAGNOSTICS_DEBOUNCE: Duration = Duration::from_millis(150);

/// The whole budget for waiting on one document's diagnostics
/// (`client.ts:14`), debounce included.
pub const DOCUMENT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one `textDocument/diagnostic` request may take
/// (`client.ts:16`).
pub const DIAGNOSTICS_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the `initialize` handshake may take (`client.ts:17`).
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);

/// How long a server's process group is given to end itself after `SIGTERM`
/// before `SIGKILL` follows — the same grace `tool/shell.rs`'s own kill
/// sequence gives a command tree, and for the same reason: only the unix path
/// signals a group at all.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(200);

/// What went wrong starting or speaking to a server.
///
/// Every one of these ends the same way — the `(root, id)` pair is marked
/// broken and nothing is retried — so the variants exist to be *logged*
/// precisely, not to be matched on.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No binary to run: not on `PATH`, or a configured command that is empty.
    #[error("no executable for language server \"{id}\"")]
    NoExecutable {
        /// The server that has none.
        id: String,
    },
    /// The process would not start.
    #[error("could not start language server \"{id}\": {source}")]
    Spawn {
        /// The server that would not start.
        id: String,
        /// Why the operating system refused.
        source: std::io::Error,
    },
    /// The pipes this client speaks over were not there.
    #[error("language server \"{id}\" was started without the pipes to speak over")]
    NoPipes {
        /// The server that came up without stdio.
        id: String,
    },
    /// `initialize` did not answer inside [`INITIALIZE_TIMEOUT`].
    #[error("language server \"{id}\" did not finish initializing within {timeout:?}")]
    InitializeTimeout {
        /// The server that did not answer.
        id: String,
        /// The budget it overran.
        timeout: Duration,
    },
    /// The server answered `initialize` with an error, or died during it.
    #[error("language server \"{id}\" refused to initialize: {message}")]
    Initialize {
        /// The server that refused.
        id: String,
        /// What it said.
        message: String,
    },
    /// The connection is gone: the process exited, or its pipes closed.
    #[error("language server \"{id}\" is no longer running")]
    Disconnected {
        /// The server that went away.
        id: String,
    },
    /// A file this client was asked to open could not be read.
    #[error("could not read {path}: {source}")]
    Read {
        /// The file that could not be read.
        path: PathBuf,
        /// Why not.
        source: std::io::Error,
    },
}

/// Requests that have gone out and not yet been answered, by id.
///
/// Shared between the client that sends and the reader task that answers, and
/// dropped wholesale when the connection dies — which is what turns a server
/// that went away into a `Disconnected` at each call site rather than a task
/// parked for the life of the process.
type Pending = Arc<std::sync::Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

/// Removes one id from [`Pending`] when dropped.
///
/// An answer removes its own entry in [`Reader::dispatch`], and that removal
/// races nothing this drops afterward — `HashMap::remove` on an already-gone
/// key is a no-op. What this exists for is every path that is *not* an
/// answer: `pull`'s `tokio::time::timeout` drops [`Client::request`]'s future
/// outright once a request runs long, and a plain `async fn` has no way to
/// run cleanup when it is dropped mid-`await` rather than returned from.
/// Without this, a request the caller stopped waiting on leaves its sender in
/// the map for the rest of the client's life — one leaked entry per timed-out
/// pull, for a session that may pull diagnostics thousands of times.
struct PendingGuard {
    pending: Pending,
    id: i64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending
            .lock()
            .expect("the pending requests are never poisoned")
            .remove(&self.id);
    }
}

/// When a publish for one path arrived, and which document version it claimed.
#[derive(Clone, Copy, Debug)]
struct Publish {
    at: Instant,
    /// [`None`] where the server did not say, which most do not.
    version: Option<i32>,
}

impl Publish {
    /// Whether this publish answers a touch made at `after` for `version`.
    ///
    /// Upstream's two guards (`client.ts:481-484`), which between them say: a
    /// publish that names a *different* version is never fresh, and one that
    /// arrived before the touch is fresh only if it names the touch's version
    /// exactly.
    fn is_fresh(self, version: i32, after: Instant) -> bool {
        if self.version.is_some_and(|claimed| claimed != version) {
            return false;
        }

        self.at >= after || self.version == Some(version)
    }
}

/// What one server has said about the files it was shown.
///
/// Separated from the connection deliberately: it is the half of a client with
/// interesting behavior — freshness, debounce, the timeout that returns
/// silently — and keeping it apart means those can be tested by publishing
/// into it rather than by starting a language server and hoping.
#[derive(Default)]
pub struct Store {
    state: std::sync::Mutex<State>,
    /// Woken on every publish, so a waiter re-reads instead of polling.
    published: Notify,
}

#[derive(Default)]
struct State {
    /// What arrived over `textDocument/publishDiagnostics`.
    pushed: HashMap<PathBuf, Vec<Diagnostic>>,
    /// What came back from a `textDocument/diagnostic` request, including the
    /// related documents a report carried.
    pulled: HashMap<PathBuf, Vec<Diagnostic>>,
    publishes: HashMap<PathBuf, Publish>,
}

impl State {
    /// Both caches for one path, concatenated and deduped.
    ///
    /// Upstream's `mergedDiagnostics` + `dedupeDiagnostics` (`client.ts:91-105`,
    /// `:145-146`): the same error usually arrives through both channels, and a
    /// model shown it twice is a model told there are two problems.
    fn merged(&self, path: &Path) -> Vec<Diagnostic> {
        let mut seen = std::collections::HashSet::new();

        self.pushed
            .get(path)
            .into_iter()
            .chain(self.pulled.get(path))
            .flatten()
            .filter(|issue| seen.insert(identity(issue)))
            .cloned()
            .collect()
    }

    /// Every path either cache knows about.
    fn paths(&self) -> std::collections::HashSet<PathBuf> {
        self.pushed
            .keys()
            .chain(self.pulled.keys())
            .cloned()
            .collect()
    }
}

/// The diagnostics inside a `full` document-diagnostic report.
///
/// A report whose `items` are missing or malformed yields none rather than an
/// error: a server that answered badly has said nothing, and nothing is a
/// thing this can represent.
fn items(report: &Value) -> Vec<Diagnostic> {
    report
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// What makes two diagnostics the same diagnostic, spelled as upstream spells
/// it — the fields that identify it, and not the ones that decorate it.
fn identity(diagnostic: &Diagnostic) -> String {
    json!({
        "code": diagnostic.code,
        "severity": diagnostic.severity,
        "message": diagnostic.message,
        "source": diagnostic.source,
        "range": diagnostic.range,
    })
    .to_string()
}

impl Store {
    /// Records what a server published for `path`.
    pub fn publish(&self, path: PathBuf, version: Option<i32>, diagnostics: Vec<Diagnostic>) {
        {
            let mut state = self.locked();
            state.publishes.insert(
                path.clone(),
                Publish {
                    at: Instant::now(),
                    version,
                },
            );
            state.pushed.insert(path, diagnostics);
        }
        self.published.notify_waiters();
    }

    /// Records what a `textDocument/diagnostic` report said about `path`.
    ///
    /// Returns whether the server now has something to say about it, which is
    /// upstream's `matched`: the signal that the pull answered and the wait
    /// need not sit out the rest of its budget.
    pub fn absorb(&self, path: &Path, report: &Value) -> bool {
        let mut state = self.locked();
        // A report is either the diagnostics themselves (`full`) or a claim
        // that what the client already has is still current (`unchanged`). Only
        // the first replaces anything; the second leaves the cache standing,
        // which is what makes it an answer rather than a silence.
        if report.get("kind").and_then(Value::as_str) == Some("full") {
            state.pulled.insert(path.to_owned(), items(report));
        }

        // Related documents are how one file's report carries another file's
        // errors (`client.ts:318-324`), and how a `write` learns that what it
        // wrote broke something elsewhere.
        if let Some(related) = report.get("relatedDocuments").and_then(Value::as_object) {
            for (uri, report) in related {
                let Some(other) = file_path(uri) else {
                    continue;
                };
                if report.get("kind").and_then(Value::as_str) == Some("full") {
                    state.pulled.insert(other, items(report));
                }
            }
        }

        !state.merged(path).is_empty()
    }

    /// Forgets what was said about `path`.
    ///
    /// Called when a document is opened for the first time, and never on a
    /// change: some servers only re-publish when content actually changed, so
    /// clearing on every touch would lose errors that are still true
    /// (`client.ts:564-567`).
    fn forget(&self, path: &Path) {
        let mut state = self.locked();
        state.pushed.remove(path);
        state.pulled.remove(path);
    }

    /// Everything this server currently believes, by path.
    #[must_use]
    pub fn diagnostics(&self) -> HashMap<PathBuf, Vec<Diagnostic>> {
        let state = self.locked();

        state
            .paths()
            .into_iter()
            .map(|path| {
                let merged = state.merged(&path);

                (path, merged)
            })
            .filter(|(_, merged)| !merged.is_empty())
            .collect()
    }

    /// What this server currently believes about one path.
    #[cfg(test)]
    #[must_use]
    pub fn for_path(&self, path: &Path) -> Vec<Diagnostic> {
        self.locked().merged(path)
    }

    /// Waits for a publish about `path` that is newer than the touch which
    /// produced `version` at `after`, then for the server to stop revising it.
    ///
    /// Returns whether a fresh publish settled inside `budget`. **A `false` is
    /// not an error and has no marker**: the caller reads whatever the caches
    /// hold, which may be empty and may be stale, exactly as upstream does
    /// (`client.ts:499-519`). Telling the model "diagnostics timed out" would
    /// be telling it about ganja's plumbing instead of about its code.
    pub async fn wait_fresh(
        &self,
        path: &Path,
        version: i32,
        after: Instant,
        budget: Duration,
    ) -> bool {
        if budget.is_zero() {
            return false;
        }

        tokio::time::timeout(budget, self.settle(path, version, after))
            .await
            .unwrap_or(false)
    }

    /// The freshness-then-debounce loop [`Store::wait_fresh`] bounds.
    async fn settle(&self, path: &Path, version: i32, after: Instant) -> bool {
        loop {
            // Registered before the state is read, so a publish landing
            // between the read and the await is a wake-up and not a lost one.
            let signal = self.published.notified();
            tokio::pin!(signal);
            signal.as_mut().enable();

            let hit = self.locked().publishes.get(path).copied();
            if let Some(hit) = hit.filter(|hit| hit.is_fresh(version, after)) {
                let quiet = DIAGNOSTICS_DEBOUNCE.saturating_sub(hit.at.elapsed());
                tokio::select! {
                    () = tokio::time::sleep(quiet) => return true,
                    // A further publish means the server is still revising:
                    // start the quiet period again against the new one.
                    () = signal => continue,
                }
            }

            signal.await;
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("the diagnostics store is never poisoned")
    }
}

/// What this client last sent a server about one file.
struct Document {
    version: i32,
    text: String,
}

/// A live language server.
pub struct Client {
    /// Which server definition this is running, for log lines and for the
    /// `(root, id)` identity the owner keys clients on.
    id: String,
    /// Where the server was started.
    root: PathBuf,
    /// What it has published. Shared with the reader task.
    store: Arc<Store>,
    /// Frames waiting to go out. An unbounded queue because every producer is
    /// a notification that must not block a tool call, and the consumer is a
    /// pipe write.
    outgoing: tokio::sync::mpsc::UnboundedSender<Value>,
    /// Requests waiting for an answer, by id. Shared with the reader task.
    pending: Pending,
    next_id: AtomicI64,
    /// Files this client has opened, and what it last told the server they
    /// contain.
    documents: tokio::sync::Mutex<HashMap<PathBuf, Document>>,
    /// Whether the server asked for ranged content changes at `initialize`.
    incremental: bool,
    /// Kept so shutdown can end the process. Also `kill_on_drop`, so an engine
    /// that is dropped without a shutdown does not leak a language server.
    child: std::sync::Mutex<Option<Child>>,
}

impl Client {
    /// Starts `spec` at `root` and completes the `initialize` handshake.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when there is no binary, the process will not
    /// start, or the handshake does not finish inside [`INITIALIZE_TIMEOUT`].
    /// Every one of them is a permanently broken `(root, id)` to the caller.
    pub async fn start(spec: &Spec, root: &Path) -> Result<Self, ClientError> {
        let program = spec.program().ok_or_else(|| ClientError::NoExecutable {
            id: spec.id.clone(),
        })?;
        let (binary, arguments) =
            program
                .split_first()
                .ok_or_else(|| ClientError::NoExecutable {
                    id: spec.id.clone(),
                })?;

        let mut command = Command::new(binary);
        command
            .args(arguments)
            .current_dir(root)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A language server outlives nothing: if this handle goes, so does
            // the process.
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // The same call `mcp.rs` and `tool/shell.rs` make, for the same
            // reason: rust-analyzer forks cargo and rustc of its own, and
            // only a process group can be ended as one.
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|source| ClientError::Spawn {
            id: spec.id.clone(),
            source,
        })?;

        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(ClientError::NoPipes {
                id: spec.id.clone(),
            });
        };

        let store = Arc::new(Store::default());
        let pending = Pending::default();
        let (outgoing, queue) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(write_frames(stdin, queue));
        tokio::spawn(drain(stderr, spec.id.clone()));
        tokio::spawn(read_frames(
            stdout,
            Reader {
                id: spec.id.clone(),
                root: root.to_owned(),
                store: Arc::clone(&store),
                pending: Arc::clone(&pending),
                outgoing: outgoing.clone(),
                initialization: spec.initialization.clone(),
            },
        ));

        let client = Self {
            id: spec.id.clone(),
            root: root.to_owned(),
            store,
            outgoing,
            pending,
            next_id: AtomicI64::new(1),
            documents: tokio::sync::Mutex::default(),
            incremental: false,
            child: std::sync::Mutex::new(Some(child)),
        };

        client.initialize(spec).await
    }

    /// The `initialize`/`initialized` handshake, and the configuration push
    /// that follows it (`client.ts:207-266`).
    async fn initialize(mut self, spec: &Spec) -> Result<Self, ClientError> {
        let root_uri = uri(&self.root);
        let initialization = spec.initialization.clone().unwrap_or(Value::Null);
        let capabilities = json!({
            "window": { "workDoneProgress": true },
            "workspace": {
                "configuration": true,
                "didChangeWatchedFiles": { "dynamicRegistration": true },
                "diagnostics": { "refreshSupport": false },
            },
            "textDocument": {
                "synchronization": { "didOpen": true, "didChange": true },
                // Both halves are load-bearing. `relatedDocumentSupport` is
                // how one file's report carries another file's errors, which
                // is what fills `write`'s cross-file section.
                "diagnostic": { "dynamicRegistration": true, "relatedDocumentSupport": true },
                "publishDiagnostics": { "versionSupport": false },
            },
        });
        let params = json!({
            "rootUri": root_uri,
            "processId": self.pid(),
            "workspaceFolders": [{ "name": "workspace", "uri": root_uri }],
            "initializationOptions": initialization,
            "capabilities": capabilities,
        });

        let answered = tokio::time::timeout(INITIALIZE_TIMEOUT, self.request("initialize", params))
            .await
            .map_err(|_| ClientError::InitializeTimeout {
                id: self.id.clone(),
                timeout: INITIALIZE_TIMEOUT,
            })?;
        let result = answered
            .map_err(|error| ClientError::Initialize {
                id: self.id.clone(),
                message: error.to_string(),
            })?
            .map_err(|message| ClientError::Initialize {
                id: self.id.clone(),
                message,
            })?;

        self.incremental = sync_kind(&result) == Some(TextDocumentSyncKind::INCREMENTAL);
        self.notify("initialized", json!({}))?;
        if let Some(settings) = &spec.initialization {
            self.notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": settings }),
            )?;
        }

        Ok(self)
    }

    /// What this client has been told, by path.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Shows `path` to the server, reading it fresh from disk, and returns the
    /// document version the server now holds.
    ///
    /// Sync is always the whole file's current contents. There is no editor
    /// buffer behind this: what the model changed, it changed on disk, and
    /// what is on disk is the only thing anyone here has an opinion about.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the file cannot be read or the server has
    /// gone away.
    pub async fn open(&self, path: &Path) -> Result<i32, ClientError> {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|source| ClientError::Read {
                path: path.to_owned(),
                source,
            })?;
        let uri = uri(path);
        let mut documents = self.documents.lock().await;

        let Some(document) = documents.get(path) else {
            self.notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": [{ "uri": uri, "type": FILE_CREATED }] }),
            )?;
            // The first sight of a file is the one moment stale diagnostics
            // are certainly wrong: nothing has been said about *this* opening
            // yet.
            self.store.forget(path);
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id(path),
                        "version": 0,
                        "text": text,
                    }
                }),
            )?;
            documents.insert(path.to_owned(), Document { version: 0, text });

            return Ok(0);
        };

        self.notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": uri, "type": FILE_CHANGED }] }),
        )?;
        let version = document.version + 1;
        let changes = if self.incremental {
            // A single change spanning the whole of what the server last saw,
            // carrying the whole of what is there now. Incremental in shape
            // only — which is what the negotiated kind asks for, and all it
            // asks for (`client.ts:585-594`).
            json!([{
                "range": { "start": { "line": 0, "character": 0 }, "end": end_position(&document.text) },
                "text": text,
            }])
        } else {
            json!([{ "text": text }])
        };
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": changes,
            }),
        )?;
        documents.insert(path.to_owned(), Document { version, text });

        Ok(version)
    }

    /// Waits for this server's verdict on `path` after a touch that produced
    /// `version` at `after`.
    ///
    /// Upstream's `waitForDocumentDiagnostics` (`client.ts:499-519`): both
    /// channels at once, whichever answers first. The push wait is started
    /// before the pull is sent so that a publish landing while the request is
    /// in flight still counts.
    ///
    /// Returns nothing, and cannot fail. A budget that runs out is not an
    /// error — the caller reads whatever the caches hold, which is upstream's
    /// behavior and the only honest one: the alternative is telling the model
    /// about ganja's timers instead of about its code.
    pub async fn wait_for_diagnostics(&self, path: &Path, version: i32, after: Instant) {
        let push = self
            .store
            .wait_fresh(path, version, after, DOCUMENT_WAIT_TIMEOUT);
        tokio::pin!(push);

        let answered = {
            let pull = self.pull(path);
            tokio::pin!(pull);

            tokio::select! {
                // A fresh publish satisfies the wait on its own; the pull in
                // flight is then just a request nobody reads.
                _ = &mut push => return,
                answered = &mut pull => answered,
            }
        };
        if answered {
            return;
        }

        // The pull had nothing. Sit out the rest of the budget in case a
        // publish arrives; `wait_fresh` owns the deadline.
        push.await;
    }

    /// Asks the server for `path`'s diagnostics, and files the answer.
    ///
    /// Returns whether the server had anything to say. Every failure — no
    /// support for the request, a timeout, a dead connection, a report that
    /// does not parse — is a `false`, because a pull that did not work is a
    /// channel that did not answer and not a turn that should stop.
    async fn pull(&self, path: &Path) -> bool {
        let params = json!({ "textDocument": { "uri": uri(path) } });
        let answered = tokio::time::timeout(
            DIAGNOSTICS_REQUEST_TIMEOUT,
            self.request("textDocument/diagnostic", params),
        )
        .await;

        match answered {
            Ok(Ok(Ok(report))) => self.store.absorb(path, &report),
            Ok(Ok(Err(message))) => {
                // A server with no `diagnosticProvider` answers this way, which
                // is a fact about the server and not a problem with the turn.
                tracing::debug!(
                    server = self.id.as_str(),
                    message,
                    "the language server would not answer a diagnostics request"
                );

                false
            }
            Ok(Err(error)) => {
                tracing::debug!(server = self.id.as_str(), %error, "the diagnostics request failed");

                false
            }
            Err(_) => false,
        }
    }

    /// Ends the server's process.
    ///
    /// On unix this ends the whole **group**, not just the direct child:
    /// rust-analyzer forks cargo and rustc while it works, and signalling the
    /// leader alone would leave those running. The `SIGTERM` goes out
    /// immediately; the grace and the `SIGKILL` that follows it run on a
    /// spawned task because `shutdown` is called from `Drop::drop`, which
    /// cannot await — the same reason `mcp.rs`'s `spawn` gives for why
    /// `rmcp`'s own child cleanup is spawned rather than awaited there.
    /// `kill_on_drop` on the handle is still armed (it moves into that task
    /// and drops with it), and is the whole mechanism on a platform with no
    /// process groups to signal.
    pub fn shutdown(&self) {
        let child = self
            .child
            .lock()
            .expect("the language server handle is never poisoned")
            .take();
        let Some(mut child) = child else {
            return;
        };

        #[cfg(unix)]
        {
            let Some(pid) = child.id() else {
                // Already reaped; there is no group left to signal.
                return;
            };
            ganja_tool::shell::signal_group(pid, libc::SIGTERM);

            tokio::spawn(async move {
                tokio::time::sleep(KILL_GRACE).await;
                if matches!(child.try_wait(), Ok(None)) {
                    ganja_tool::shell::signal_group(pid, libc::SIGKILL);
                }
            });
        }
        #[cfg(not(unix))]
        if let Err(error) = child.start_kill() {
            tracing::debug!(
                server = self.id.as_str(),
                %error,
                "the language server had already gone"
            );
        }
    }

    /// The server's process id, which `initialize` carries so a server can
    /// notice its client dying.
    fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .expect("the language server handle is never poisoned")
            .as_ref()
            .and_then(Child::id)
    }

    /// Sends a notification. Nothing waits for it, and a closed connection is
    /// the only thing that can go wrong.
    fn notify(&self, method: &str, params: Value) -> Result<(), ClientError> {
        self.outgoing
            .send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .map_err(|_| ClientError::Disconnected {
                id: self.id.clone(),
            })
    }

    /// Sends a request and waits for its answer.
    ///
    /// The outer result is the connection's, the inner one the server's: a
    /// server that answers "no" is a working server.
    async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Result<Value, String>, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.locked_pending().insert(id, sender);
        // Armed the moment the entry exists, so every way out of this
        // function — the early return below, the ordinary answer, a
        // connection dying, or this future being dropped before either —
        // clears the same entry exactly once.
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id,
        };

        let sent = self.outgoing.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        if sent.is_err() {
            return Err(ClientError::Disconnected {
                id: self.id.clone(),
            });
        }

        receiver.await.map_err(|_| ClientError::Disconnected {
            id: self.id.clone(),
        })
    }

    fn locked_pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<i64, oneshot::Sender<Result<Value, String>>>> {
        self.pending
            .lock()
            .expect("the pending requests are never poisoned")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `workspace/didChangeWatchedFiles` change kinds, which LSP numbers.
const FILE_CREATED: u8 = 1;
/// See [`FILE_CREATED`].
const FILE_CHANGED: u8 = 2;

/// What the reader task needs to dispatch what arrives.
struct Reader {
    id: String,
    root: PathBuf,
    store: Arc<Store>,
    pending: Pending,
    outgoing: tokio::sync::mpsc::UnboundedSender<Value>,
    initialization: Option<Value>,
}

/// Reads frames off the server's stdout until it closes, dispatching each.
async fn read_frames(stdout: tokio::process::ChildStdout, reader: Reader) {
    let mut stream = BufReader::new(stdout);
    loop {
        match next_frame(&mut stream).await {
            Ok(Some(frame)) => reader.dispatch(frame),
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(
                    server = reader.id.as_str(),
                    %error,
                    "the language server's output stopped making sense; hanging up"
                );
                break;
            }
        }
    }

    // Everything still waiting is waiting forever: dropping the senders is
    // what turns that into a `Disconnected` at each call site rather than a
    // task parked for the life of the process.
    reader
        .pending
        .lock()
        .expect("the pending requests are never poisoned")
        .clear();
}

/// One `Content-Length`-framed JSON message, or [`None`] at end of stream.
async fn next_frame(
    stream: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<Option<Value>, std::io::Error> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if stream.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        {
            length = Some(value);
        }
    }

    let Some(length) = length else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a frame arrived with no Content-Length",
        ));
    };
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

impl Reader {
    /// Routes one message: an answer to something asked, a notification, or a
    /// request the server expects an answer to.
    fn dispatch(&self, frame: Value) {
        let method = frame.get("method").and_then(Value::as_str);
        let id = frame.get("id");

        match (method, id) {
            // An answer: an id and no method.
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else {
                    return;
                };
                let Some(waiting) = self
                    .pending
                    .lock()
                    .expect("the pending requests are never poisoned")
                    .remove(&id)
                else {
                    return;
                };
                let answer = match frame.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the language server gave no reason")
                        .to_owned()),
                    None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                };
                // The receiver may be gone if the caller timed out; that is
                // the caller's answer already given.
                let _ = waiting.send(answer);
            }
            // A request: both a method and an id.
            (Some(method), Some(id)) => self.answer(method, id.clone(), frame.get("params")),
            // A notification: a method and no id.
            (Some(method), None) => self.observe(method, frame.get("params")),
            (None, None) => {}
        }
    }

    /// Answers a request the server made of this client (`client.ts:173-206`).
    fn answer(&self, method: &str, id: Value, params: Option<&Value>) {
        let result = match method {
            "workspace/configuration" => {
                let items = params
                    .and_then(|params| params.get("items"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let answers: Vec<Value> = items
                    .iter()
                    .map(|item| {
                        configuration(
                            self.initialization.as_ref(),
                            item.get("section").and_then(Value::as_str),
                        )
                    })
                    .collect();

                Value::Array(answers)
            }
            "workspace/workspaceFolders" => {
                json!([{ "name": "workspace", "uri": uri(&self.root) }])
            }
            // Answered, not acted on: nothing here pulls diagnostics, and
            // work-done progress is reported to a UI this engine does not have.
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability"
            | "workspace/diagnostic/refresh" => Value::Null,
            _ => {
                // JSON-RPC's method-not-found. A server that asked for
                // something must hear back, or it may wait forever.
                let _ = self.outgoing.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("{method} is not implemented") },
                }));

                return;
            }
        };

        let _ = self
            .outgoing
            .send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    /// Takes note of a notification. Only one of them means anything here.
    fn observe(&self, method: &str, params: Option<&Value>) {
        if method != "textDocument/publishDiagnostics" {
            return;
        }
        let Some(params) = params else {
            return;
        };
        let Ok(published) = serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
        else {
            tracing::debug!(
                server = self.id.as_str(),
                "a publishDiagnostics arrived that could not be understood"
            );
            return;
        };
        let Some(path) = file_path(published.uri.as_str()) else {
            // Not a `file://` URI: an untitled buffer or a virtual document,
            // neither of which any tool here can have edited.
            return;
        };

        self.store
            .publish(path, published.version, published.diagnostics);
    }
}

/// Writes queued messages to the server's stdin, framed.
async fn write_frames(
    mut stdin: tokio::process::ChildStdin,
    mut queue: tokio::sync::mpsc::UnboundedReceiver<Value>,
) {
    while let Some(message) = queue.recv().await {
        let Ok(body) = serde_json::to_vec(&message) else {
            continue;
        };
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        if stdin.write_all(header.as_bytes()).await.is_err()
            || stdin.write_all(&body).await.is_err()
            || stdin.flush().await.is_err()
        {
            break;
        }
    }
}

/// Reads the server's stderr so its pipe cannot fill and stall it.
///
/// Upstream discards this outright. Logging it at debug costs nothing and is
/// the difference between "rust-analyzer did not start" and knowing why.
async fn drain(stderr: tokio::process::ChildStderr, id: String) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(server = id.as_str(), "{line}");
    }
}

/// The change kind the server negotiated, if it said (`client.ts:76-81`).
fn sync_kind(result: &Value) -> Option<TextDocumentSyncKind> {
    let sync = result.get("capabilities")?.get("textDocumentSync")?;
    let kind = sync
        .as_i64()
        .or_else(|| sync.get("change").and_then(Value::as_i64))?;

    match kind {
        0 => Some(TextDocumentSyncKind::NONE),
        1 => Some(TextDocumentSyncKind::FULL),
        2 => Some(TextDocumentSyncKind::INCREMENTAL),
        _ => None,
    }
}

/// One `workspace/configuration` item answered out of `settings`
/// (`client.ts:107-114`).
///
/// The section is a dotted path; anything it does not reach is `null`, which
/// is LSP's "I have no setting for that" and not an error.
fn configuration(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(settings) = settings else {
        return Value::Null;
    };
    let Some(section) = section else {
        return settings.clone();
    };

    section
        .split('.')
        .try_fold(settings, |value, key| value.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

/// `path` as a `file://` URI.
fn uri(path: &Path) -> String {
    url::Url::from_file_path(path).map_or_else(
        |()| format!("file://{}", path.display()),
        |url| url.to_string(),
    )
}

/// The path a `file://` URI names, or [`None`] for any other scheme.
fn file_path(uri: &str) -> Option<PathBuf> {
    let parsed = url::Url::parse(uri).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }

    parsed.to_file_path().ok().map(keyed)
}

/// `path` in the single spelling this module files diagnostics under.
///
/// Diagnostics arrive keyed by whatever URI the server chose, and are looked up
/// again by a path this process built from the filesystem. On Windows those two
/// need not agree about the drive letter's case: rust-analyzer answers
/// `file:///c%3A/work/api/src/main.rs` where this port would have written
/// `file:///C:/work/api/src/main.rs`, and the percent-encoding decodes back to a
/// lower-case `c`. Two [`PathBuf`]s differing only there are two different map
/// keys, so an edit's diagnostics would be filed under a file nothing ever asks
/// about — the errors would simply never appear.
///
/// Only the drive letter is touched. The rest of a Windows path is compared
/// case-insensitively by the filesystem but is not this module's to rewrite: a
/// server that echoes back the spelling it was given keeps it.
///
/// Nothing to do on any other platform, where a path has no prefix to disagree
/// about.
fn keyed(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    let path = uppercase_drive(path);

    path
}

/// The rewrite [`keyed`] documents.
#[cfg(windows)]
fn uppercase_drive(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    if !path.is_absolute() {
        return path;
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return path;
    };
    let Prefix::Disk(letter) = prefix.kind() else {
        return path;
    };
    if letter.is_ascii_uppercase() {
        return path;
    }

    let mut rewritten = PathBuf::from(format!("{}:\\", char::from(letter.to_ascii_uppercase())));
    rewritten.extend(
        path.components()
            .skip_while(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
            .map(Component::as_os_str),
    );

    rewritten
}

/// The position one past the last character of `text`.
///
/// Line endings are LSP's three (`client.ts:83-89`), and the column is counted
/// in UTF-16 code units because that is the unit LSP positions are in unless a
/// server negotiated otherwise, and none here does.
fn end_position(text: &str) -> Value {
    let lines: Vec<&str> = split_lines(text);
    let character = lines.last().map_or(0, |line| line.encode_utf16().count());

    json!({ "line": lines.len().saturating_sub(1), "character": character })
}

/// `text` split on `\r\n`, `\r` or `\n`, keeping empty trailing lines.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    loop {
        let Some(index) = rest.find(['\r', '\n']) else {
            lines.push(rest);

            return lines;
        };
        lines.push(&rest[..index]);
        let skip = if rest[index..].starts_with("\r\n") {
            2
        } else {
            1
        };
        rest = &rest[index + skip..];
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
