//! The unmodified `claude` CLI, driven as a provider (**D556**).
//!
//! Spec: the recording, `tests/fixtures/claude-code-sdk-mcp-probe.txt` —
//! nineteen invocations, twenty-one paid turns, **ten of which the vendor's
//! own safeguard refused**. No upstream file is cited here as specification:
//! the opencode phase is over, and this wire has no upstream at all. Where
//! this module's prose and a frame in that file disagree, the frame wins.
//!
//! # What is held, and what is never resumed
//!
//! A `claude` process is **held open** across turns, keyed by
//! `ids::derived(messages[0].id)`. That is the one continuity the recording
//! served every time it was tried. `--resume` is the one it did not: served
//! on 2 of 8 turns, refused on 6, on records the recording cannot tell apart.
//! So the wire **never passes `--resume`** — the token is on
//! `argv::NEVER_ANYWHERE`, forbidden rather than merely unbuilt — and every
//! divergence that needs a new process opens a **fresh record**.
//!
//! # The ten refused turns, and the shape they drew
//!
//! Runs 9a, 9b and 9c carried ganja's transcript rendered **with**
//! `[Assistant]` lines and were refused three of three; run 9d carried the
//! same conversation without them and was served. The refusal's own
//! explanation names "duplicating model outputs". So a fresh record's
//! preamble carries the user's asks and the tool trail and never the model's
//! own words, and the cost is stated rather than hidden: **across every fresh
//! record the model loses the words of its own earlier replies**, a
//! compaction summary is assistant text and so renders as nothing at all, and
//! an ask the model answered in text alone reads as unanswered and may be
//! redone.
//!
//! That is a measured tendency and not a rule the frames prove: run 2e-1 and
//! run 2e-2 are the same record under the same argv one turn apart, one
//! served and one refused. A wire that must never see a refusal cannot be
//! built on this recording, which is why a refusal is answered with a
//! **bound** rather than with a cleverer shape: a refused record's
//! replacement opens with no preamble at all, and a second refusal in a row
//! spends nothing — the turn is failed locally, naming both attempts and the
//! two doors a person has.
//!
//! # What a turn is worth, measured
//!
//! Under the CLI's own preset prompt a process finds a warm prefix across
//! processes (runs 4, 5 and 2e-2 read 3 364, 712 and 3 817). Under ganja's
//! **replaced** prompt every cold process — fresh, resumed or replayed —
//! reads **0** and pays its whole prefix. A turn carrying a tool call is two
//! API requests whose usage is summed, so such a turn's cache read is largely
//! its own second request reading its first request's write. What follows is
//! that a held process's cache is warm within the TTL and a fresh record's is
//! not, which is the whole of why a process is worth keeping alive.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use tokio_util::sync::CancellationToken;

use crate::protocol::{Message, PartBody, Role};
use crate::provider::{ChatRequest, Provider, ProviderError, ProviderEvent};
use crate::tool::ToolDefinition;

pub mod argv;
pub mod binding;
pub mod bridge;
pub mod frame;
pub mod held;
pub mod preamble;
pub mod process;
pub mod rpc;

/// What [`crate::provider::PROVIDER_ENV`] accepts for this wire.
pub const ID: &str = "claude-code";

/// The model a session runs as when it names none.
///
/// The CLI's own word for "whatever you would choose", which is why it is
/// never passed as `--model`: doing so would replace the vendor's choice with
/// a literal. What the vendor actually served comes back on `system/init` and
/// is surfaced through [`Provider::served_model`].
pub const DEFAULT_MODEL: &str = "default";

/// Names an absolute path to the `claude` binary, overriding the default.
///
/// Absolute, and never a `PATH` search: on this machine a `claude` on `PATH`
/// may be a wrapper that exports `CLAUDE_CODE_COORDINATOR_MODE=1`, and a wire
/// that found one would be driving something other than the CLI.
pub const BIN_ENV: &str = "GANJA_CLAUDE_BIN";

/// Where the binary lives under the user's home when nothing names one.
pub const BINARY_UNDER_HOME: &str = ".local/bin/claude";

/// The oldest build this wire will drive.
///
/// 2.1.263 — the build every bundle cite and the flag survey were read on.
/// The recording itself was made on 2.1.266 and run 0b proved the same argv
/// parses on the floor, so the two are consistent and the floor is the
/// surveyed one rather than the recorded one.
pub const VERSION_FLOOR: (u64, u64, u64) = (2, 1, 263);

/// How long `--version` may take before the wire gives up on the binary.
const VERSION_BOUND: Duration = Duration::from_secs(10);

/// The `claude` CLI as a provider.
pub struct ClaudeCodeProvider {
    bin: PathBuf,
    version: String,
    spawner: Arc<dyn process::Spawner>,
    held: Arc<held::HeldProcesses>,
    slots: held::Slots,
    paths: binding::Paths,
    /// One lock per key, held for the life of that key's process.
    locks: Mutex<std::collections::HashMap<held::Key, binding::Lock>>,
}

impl std::fmt::Debug for ClaudeCodeProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaudeCodeProvider")
            .field("bin", &self.bin)
            .field("version", &self.version)
            .field("held_entries", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl ClaudeCodeProvider {
    /// Resolves the binary, checks its version, and builds the wire.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError::Transport`] naming the path when no binary
    /// is there, when `GANJA_CLAUDE_BIN` is relative, when `--version` did not
    /// answer, or when the build is below [`VERSION_FLOOR`] — the last naming
    /// both versions and the reason.
    pub async fn from_env() -> Result<Self, ProviderError> {
        let bin = resolve_binary()?;
        let version = read_version(&bin).await?;
        let paths = binding::Paths::resolved().map_err(ProviderError::Transport)?;

        Ok(Self {
            bin,
            version,
            spawner: Arc::new(process::Real),
            held: Arc::new(held::HeldProcesses::new(held::DEFAULT_IDLE_BOUND)),
            slots: held::Slots::default(),
            paths,
            locks: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// The same wire against a spawner of the caller's choosing.
    ///
    /// The door every test in this suite comes through: a live turn spends
    /// money on a real account and can add to the recording's ten refusals,
    /// which is the one thing a test must never be able to do.
    #[must_use]
    pub fn with_parts(
        bin: PathBuf,
        version: String,
        spawner: Arc<dyn process::Spawner>,
        paths: binding::Paths,
    ) -> Self {
        Self {
            bin,
            version,
            spawner,
            held: Arc::new(held::HeldProcesses::new(held::DEFAULT_IDLE_BOUND)),
            slots: held::Slots::default(),
            paths,
            locks: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The same wire whose entries go idle after `idle_bound`.
    ///
    /// W4's `select` calls this with the curated `claude_code.idle_bound`
    /// key; [`held::DEFAULT_IDLE_BOUND`] is what nothing configured means.
    #[must_use]
    pub fn with_idle_bound(mut self, idle_bound: Duration) -> Self {
        self.held = Arc::new(held::HeldProcesses::new(idle_bound));

        self
    }

    /// How many processes this wire is holding right now.
    #[must_use]
    pub fn held_entries(&self) -> usize {
        self.held.len()
    }
}

/// The absolute path this wire will spawn.
fn resolve_binary() -> Result<PathBuf, ProviderError> {
    if let Ok(named) = std::env::var(BIN_ENV) {
        let named = PathBuf::from(named);
        if !named.is_absolute() {
            return Err(ProviderError::Transport(format!(
                "{BIN_ENV} must be an absolute path, not `{}` — this wire never searches PATH",
                named.display()
            )));
        }
        if !named.exists() {
            return Err(ProviderError::Transport(format!(
                "{BIN_ENV} names `{}`, which does not exist",
                named.display()
            )));
        }

        return Ok(named);
    }

    let home = etcetera::home_dir()
        .map_err(|error| ProviderError::Transport(format!("no home directory: {error}")))?;
    let under_home = home.join(BINARY_UNDER_HOME);
    if !under_home.exists() {
        return Err(ProviderError::Transport(format!(
            "no claude CLI at {} — install it, or name one with {BIN_ENV}",
            under_home.display()
        )));
    }

    Ok(under_home)
}

/// `<bin> --version`, floored.
async fn read_version(bin: &Path) -> Result<String, ProviderError> {
    let mut command = tokio::process::Command::new(bin);
    command.arg("--version");
    // The same posture the turn-taking children run under, so a version read
    // under a stray `ANTHROPIC_API_KEY` cannot differ from what a turn sees.
    argv::ChildEnv { cwd: std::env::temp_dir() }.apply(&mut command);

    let output = tokio::time::timeout(VERSION_BOUND, command.output())
        .await
        .map_err(|_| {
            ProviderError::Transport(format!(
                "{} --version did not answer within {}s",
                bin.display(),
                VERSION_BOUND.as_secs()
            ))
        })?
        .map_err(|error| {
            ProviderError::Transport(format!("could not run {} --version: {error}", bin.display()))
        })?;

    let said = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let numbers = parse_version(&said).ok_or_else(|| {
        ProviderError::Transport(format!(
            "{} --version said `{said}`, which is not `<major>.<minor>.<patch> (Claude Code)`",
            bin.display()
        ))
    })?;

    if numbers < VERSION_FLOOR {
        let (major, minor, patch) = VERSION_FLOOR;

        return Err(ProviderError::Transport(format!(
            "the claude CLI at {} is {said}, below the {major}.{minor}.{patch} this wire was \
             read from — its frame names and its argv were surveyed on that build, and an older \
             one may parse neither",
            bin.display()
        )));
    }

    tracing::info!(provider = ID, bin = %bin.display(), version = %said, "claude CLI");

    Ok(said)
}

/// `2.1.263 (Claude Code)` as three numbers.
fn parse_version(said: &str) -> Option<(u64, u64, u64)> {
    let numbers = said.strip_suffix(" (Claude Code)")?;
    let mut parts = numbers.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;

    parts.next().is_none().then_some((major, minor, patch))
}

// ------------------------------------------------- what the wire has written

/// `request.turn_start`, clamped to an index its message list actually has.
///
/// The field is `pub`, so a value past the end is a bound to bring back in
/// range and never an index to slice on.
#[must_use]
pub fn turn_start(request: &ChatRequest) -> usize {
    request.turn_start.min(request.messages.len().saturating_sub(1))
}

/// The ids of a request's user messages, in order, over the **whole** of
/// `messages`.
///
/// Before and after `turn_start` alike: which side a message is on is the
/// engine's taxonomy, and this wire deliberately does not ask.
#[must_use]
pub fn user_ids(request: &ChatRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.id.as_str().to_owned())
        .collect()
}

/// Whether the record has read a prefix of what ganja holds, and nothing
/// ganja no longer holds.
///
/// False means an id in `sent` is gone from the request — a `/rewind` — which
/// is what tells that arm from every other reason a process might be missing.
#[must_use]
pub fn honest(sent: &[String], request: &ChatRequest) -> bool {
    let ids = user_ids(request);

    ids.len() >= sent.len() && ids.iter().zip(sent).all(|(id, written)| id == written)
}

/// Every user message of the request whose id is not in `sent`, in request
/// order.
///
/// The whole contract: a message class the engine adds later — a reminder
/// that becomes a message, a new guard block — is owed by this rule and never
/// by a new arm.
#[must_use]
pub fn owed<'a>(sent: &[String], request: &'a ChatRequest) -> Vec<&'a Message> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .filter(|message| !sent.iter().any(|written| written == message.id.as_str()))
        .collect()
}

/// Several owed messages as **one** `user` frame's text.
///
/// One frame, because the CLI enqueues each `user` frame as a new prompt, so
/// two frames would be two turns.
#[must_use]
pub fn owed_text(owed: &[&Message]) -> String {
    owed.iter()
        .map(|message| preamble::message_text(message))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A stable hash of what a process opened under, for the `stale_<what>` log
/// line.
fn hash_of(value: &impl std::hash::Hash) -> u64 {
    use std::hash::{BuildHasher as _, RandomState};

    // A `RandomState` is per-process, which is all this needs: the two values
    // being compared are always this process's own, and nothing is persisted.
    static SEED: std::sync::LazyLock<RandomState> = std::sync::LazyLock::new(RandomState::new);

    SEED.hash_one(value)
}

/// The roster's identity: names and schemas, in the order advertised.
fn tools_hash(tools: &[ToolDefinition]) -> u64 {
    let spelled: Vec<String> = tools
        .iter()
        .map(|tool| format!("{}\u{1}{}\u{1}{}", tool.name, tool.description, tool.schema))
        .collect();

    hash_of(&spelled)
}

/// Whether this turn's own slice carries a `Tool` part.
///
/// The discriminator for the recover arm: a next-turn request holds no `Tool`
/// part after `turn_start`, so a request that does is one whose tools have
/// been run and whose process should have been waiting for them.
fn carries_tool_parts(request: &ChatRequest) -> bool {
    request.messages[turn_start(request)..]
        .iter()
        .flat_map(|message| &message.parts)
        .any(|part| matches!(part.body, PartBody::Tool { .. }))
}

#[async_trait]
impl Provider for ClaudeCodeProvider {
    fn id(&self) -> &str {
        ID
    }

    /// Attachments are not carried in v1: a `File` part degrades to its name
    /// in the text a frame carries, which is what the base trait's default
    /// already means.
    fn accepts_attachment(&self, mime: &str) -> bool {
        let _ = mime;
        false
    }

    fn served_model(&self) -> Option<crate::provider::ServedModel> {
        self.slots
            .served_model
            .lock()
            .expect("the served-model slot is never poisoned")
            .clone()
            .map(|served| crate::provider::ServedModel {
                requested: served.requested,
                served: served.served,
            })
    }

    /// Closes every held process: stdin EOF, then the two signal bounds, and
    /// the per-key scratch directory removed with each entry.
    ///
    /// The **only** override of this method in the workspace, because this is
    /// the only wire that holds anything between turns.
    async fn shutdown(&self) {
        self.held.close_all().await;
    }

    fn last_eviction(&self) -> Option<crate::provider::Eviction> {
        self.slots
            .eviction
            .lock()
            .expect("the eviction slot is never poisoned")
            .clone()
            .map(|eviction| crate::provider::Eviction { key: eviction.key, at: eviction.at })
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.route(request, cancel).await
    }
}

// --------------------------------------------------------------- the arms

/// Which arm a request took, and why — one `info!` line per decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    /// A title or a compaction summary: one process, one turn, no entry.
    OneShot,
    /// Answers the asks a live process parked. Writes no `user` frame.
    Resolve,
    /// Reopens a turn whose process is gone, on a fresh record.
    Recover,
    /// Rides a live process. Writes the owed set as one `user` frame.
    Continue,
    /// Opens a fresh record.
    Spawn,
}

impl Arm {
    fn word(self) -> &'static str {
        match self {
            Self::OneShot => "one-shot",
            Self::Resolve => "resolve",
            Self::Recover => "recover",
            Self::Continue => "continue",
            Self::Spawn => "spawn",
        }
    }
}

/// The catalog effort this turn runs under, if any.
#[must_use]
fn effort_of(request: &ChatRequest) -> Option<String> {
    request.effort_options.get("effort").and_then(serde_json::Value::as_str).map(str::to_owned)
}

/// A stream carrying one terminal failure and nothing else.
fn failed(message: String) -> BoxStream<'static, ProviderEvent> {
    stream::iter([ProviderEvent::Failed(ProviderError::Transport(message))]).boxed()
}

impl ClaudeCodeProvider {
    /// Picks the arm and takes it.
    ///
    /// The four are tried in this order, and the order is the whole of the
    /// dispatch: a one-shot never enters the table, a live process with
    /// parked asks is always a resolve, a live process without them is a
    /// continue when nothing a person chose has moved, and everything else
    /// opens a fresh record.
    async fn route(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        if request.messages.is_empty() {
            return Err(ProviderError::Transport(
                "a request with no messages is a turn about nothing".to_owned(),
            ));
        }

        // `turn_start == 0` alone is **not** a one-shot marker: a
        // conversation's own first turn has it too. The empty roster is the
        // other half, and it is what `title_stream` and the compaction
        // summary both set — so a first turn offering tools falls through to
        // Spawn, enters the table and writes a binding.
        if request.turn_start == 0 && request.tools.is_empty() {
            return self.one_shot(request).await;
        }

        let key = crate::provider::ids::derived(&request.messages[0].id);
        let effort = effort_of(&request);

        if let Some(meta) = self.held.meta(&key) {
            if !meta.pending.is_empty() {
                return self.resolve(&key, meta.pending, request, cancel).await;
            }

            // The two values a person **chose**. A `system` or `tools`
            // difference closes nothing — the process keeps what it opened
            // with — because those are machinery nobody picked, where keeping
            // a chosen model stale bills the opening model under a status bar
            // that says otherwise.
            if meta.model != request.model {
                self.held.close(&key, held::Reason::Divergence).await;

                return self.spawn(&key, "model", request, cancel).await;
            }
            if meta.effort != effort {
                self.held.close(&key, held::Reason::Divergence).await;

                return self.spawn(&key, "effort", request, cancel).await;
            }

            if honest(&meta.sent, &request) {
                return self.continue_turn(&key, &meta, request, cancel).await;
            }

            // An id in `sent` is gone: a `/rewind`. The live process holds a
            // conversation ganja no longer has, so it is closed.
            self.held.close(&key, held::Reason::Divergence).await;

            return self.spawn(&key, "rewind", request, cancel).await;
        }

        // No live entry, and this turn's own slice carries `Tool` parts: the
        // process that parked those asks is gone. The engine ran the tools
        // exactly once and the CLI turn that asked is dead, so the turn is
        // reopened rather than re-run — a shape no other arm fits, since
        // Continue needs a live entry and a next-turn request holds no `Tool`
        // part after `turn_start`.
        if carries_tool_parts(&request) {
            return self.recover(&key, request, cancel).await;
        }

        let reason = self.spawn_reason(&key, &request);

        self.spawn(&key, reason, request, cancel).await
    }

    /// The reason word a Spawn logs, first that applies.
    ///
    /// The binding is read **before** the ring, so a refused record logs its
    /// own word from either source and never `exited`.
    fn spawn_reason(&self, key: &held::Key, request: &ChatRequest) -> &'static str {
        // A `try_lock` that fails means another ganja holds this
        // conversation. Nothing is read and nothing is written; this arm sits
        // outside the refusal bound by design, so a second ganja spends its
        // own two.
        if self.holds_lock(key).is_none() {
            return "locked-elsewhere";
        }

        let Some(binding) = binding::load(&self.paths.binding(key)) else {
            // No binding: a compaction's new `messages[0]`, or a first turn.
            return "new-key";
        };

        if binding.refused {
            return "refused-record";
        }
        if !honest(&binding.sent, request) {
            return "rewind";
        }
        if let Some(word) = self.held.dropped_reason(key).and_then(held::Reason::spawn_word) {
            return word;
        }

        // An honest binding whose process is gone for no reason this process
        // recorded: a record this ganja did not write.
        "unsent-history"
    }

    /// Whether this ganja holds `key`'s lock, claiming it if nobody does.
    fn holds_lock(&self, key: &held::Key) -> Option<()> {
        let mut locks = self.locks.lock().expect("the lock table is never poisoned");
        if locks.contains_key(key) {
            return Some(());
        }

        match binding::Lock::claim(&self.paths.lock(key)) {
            Ok(lock) => {
                locks.insert(key.clone(), lock);

                Some(())
            }
            Err(error) => {
                tracing::debug!(provider = ID, key, %error, "the binding is locked elsewhere");

                None
            }
        }
    }

    /// A title or a compaction summary.
    ///
    /// One process, one turn, stdin closed at the `result`. No table entry, no
    /// binding, and an `initialize` declaring **no** `sdkMcpServers`: the
    /// roster is empty, so a server would answer an empty `tools/list` to a
    /// process that lives one turn, and nothing would ever dial it.
    async fn one_shot(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let session_id = crate::protocol::uuidv7();
        let argv = argv::Argv::one_shot(&argv::OneShot {
            session_id: session_id.clone(),
            model: request.model.clone(),
            effort: effort_of(&request),
        });

        let cwd = self.paths.one_shot_cwd();
        prepare(&cwd)?;

        let io = self.spawner.spawn(&self.bin, &argv, &argv::ChildEnv { cwd })?;
        let (input, inputs) = tokio::sync::mpsc::channel(4);
        let (events, stream) = channel();

        let meta = Arc::new(Mutex::new(held::Meta::opening(
            session_id,
            request.model.clone(),
            effort_of(&request),
            hash_of(&request.system),
            tools_hash(&request.tools),
        )));

        let wiring = held::Wiring {
            key: "one-shot".to_owned(),
            meta,
            // Nothing files a one-shot, so nothing removes it either.
            table: std::sync::Weak::new(),
            slots: self.slots.clone(),
            binding: None,
            cwd: None,
            tools: Vec::new(),
            requested_model: request.model.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            opening: frame::initialize_line(
                &crate::protocol::uuidv7(),
                &frame::Initialize {
                    system_prompt: request.system.clone().map(|system| vec![system]),
                    sdk_mcp_servers: Vec::new(),
                    sdk_mcp_server_configs: serde_json::json!({}),
                },
            ),
            one_shot: true,
        };

        tokio::spawn(held::run(io, inputs, wiring, held::DEFAULT_IDLE_BOUND));

        let text = owed_text(&owed(&[], &request));
        let frame = frame::user_line(&frame::UserFrame { content: text, parent_tool_use_id: None });

        tracing::info!(
            provider = ID,
            arm = Arm::OneShot.word(),
            one_shot = true,
            held_entries = self.held.len(),
            "one process, one turn"
        );

        // The process ends with the request: nothing is recorded, so `sent`
        // afterwards is nothing at all.
        let _ = input.send(held::Input::Turn { frame, sent: Vec::new(), events }).await;

        Ok(stream)
    }

    /// Answers the asks a live process parked, on the same CLI turn.
    async fn resolve(
        &self,
        key: &held::Key,
        pendings: Vec<bridge::Pending>,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let at = turn_start(&request);
        let turn = &request.messages[at..];
        let sent = self.held.meta(key).map(|meta| meta.sent).unwrap_or_default();
        // A steer the person typed while the tool ran. It is in `messages`
        // because `steered()` is cumulative, and it cannot be a `user` frame:
        // the CLI would enqueue a second turn.
        let carry = owed(&sent, &request).into_iter().next_back().cloned();

        let resolved = match bridge::resolve(&pendings, turn, carry.as_ref()) {
            Ok(resolved) => resolved,
            // A keyed match with pendings and no results means the engine and
            // this wire disagree about what has been run. **Never a new
            // turn**: opening one on that disagreement would run something
            // twice.
            Err(id) => {
                return Ok(failed(format!(
                    "the claude CLI is waiting on tool call {id} and this request carries no \
                     result for it"
                )));
            }
        };

        let Some(input) = self.held.input(key) else {
            return Ok(failed(format!("the held claude process for {key} went away mid-resolve")));
        };
        let (events, stream) = channel();

        tracing::info!(
            provider = ID,
            key,
            arm = Arm::Resolve.word(),
            answers = resolved.answers.len(),
            owed = usize::from(carry.is_some()),
            sent = sent.len(),
            carried_on = ?resolved.carried_on,
            held_entries = self.held.len(),
            "answering parked asks"
        );

        let _ = input
            .send(held::Input::Resolve {
                answers: resolved.answers,
                carried_id: resolved.carried_id,
                events,
            })
            .await;
        self.watch_cancel(input, cancel);

        Ok(stream)
    }

    /// Rides a live process: the owed set as one `user` frame.
    async fn continue_turn(
        &self,
        key: &held::Key,
        meta: &held::Meta,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let owed = owed(&meta.sent, &request);
        if owed.is_empty() {
            // The engine sent a request with nothing new, and a `user` frame
            // with no text is a turn about nothing.
            return Ok(failed(format!(
                "the held claude process for {key} was given a turn with no new message"
            )));
        }

        // A live process runs under its opening prompt and roster. The change
        // is logged **once per change** and picked up by the next fresh
        // record, so D480's walk-ins and a `/plugin` reload cost nothing while
        // the process lives.
        self.log_stale(key, meta, &request);

        let text = owed_text(&owed);
        let mut sent = meta.sent.clone();
        sent.extend(owed.iter().map(|message| message.id.as_str().to_owned()));

        let Some(input) = self.held.input(key) else {
            return Ok(failed(format!("the held claude process for {key} went away")));
        };
        let (events, stream) = channel();

        tracing::info!(
            provider = ID,
            key,
            arm = Arm::Continue.word(),
            owed = owed.len(),
            sent = sent.len(),
            held_entries = self.held.len(),
            "riding the held process"
        );

        let frame = frame::user_line(&frame::UserFrame { content: text, parent_tool_use_id: None });
        let _ = input.send(held::Input::Turn { frame, sent, events }).await;
        self.watch_cancel(input, cancel);

        Ok(stream)
    }

    /// Says once that a live process is running under a value that has moved.
    fn log_stale(&self, key: &held::Key, meta: &held::Meta, request: &ChatRequest) {
        let system = hash_of(&request.system);
        let tools = tools_hash(&request.tools);

        if system != meta.system_hash && meta.logged_stale_system != Some(system) {
            tracing::info!(
                provider = ID,
                key,
                stale_system = true,
                "the process keeps its opening prompt"
            );
            self.held.with_meta(key, |meta| meta.logged_stale_system = Some(system));
        }
        if tools != meta.tools_hash && meta.logged_stale_tools != Some(tools) {
            tracing::info!(
                provider = ID,
                key,
                stale_tools = true,
                "the process keeps its opening roster"
            );
            self.held.with_meta(key, |meta| meta.logged_stale_tools = Some(tools));
        }
    }

    /// Reopens a turn whose process is gone, once per turn.
    async fn recover(
        &self,
        key: &held::Key,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let at = turn_start(&request);
        let turn_id = request.messages[at].id.as_str().to_owned();

        // Per **turn**, not per key and not per entry: a conversation that
        // recovered on turn 3 recovers again on turn 40, and only a second
        // stranding of *one* turn fails by name.
        if !self.held.claim_recovery(key, &turn_id) {
            // Both attempts named: the first reopened this turn on a fresh
            // record, and the second found that record gone as well. A third
            // would re-seed the same turn again for as long as something keeps
            // killing it.
            return Ok(failed(format!(
                "this turn of conversation {key} was already reopened once on a fresh record \
                 (message {turn_id}), and that record is gone too — nothing is re-run"
            )));
        }

        let word =
            self.held.dropped_reason(key).and_then(held::Reason::spawn_word).unwrap_or("unknown");

        self.open_fresh(key, "recover", Arm::Recover, word, request, cancel).await
    }

    /// Opens a fresh record.
    async fn spawn(
        &self,
        key: &held::Key,
        reason: &'static str,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // The one reading that ends the arm before anything spawns. A key
        // spends at most two consecutive refusals per ganja process: one with
        // the transcript rendered, one with the prompts alone.
        if reason == "refused-record" {
            let streak = binding::load(&self.paths.binding(key))
                .map(|binding| binding.refused_streak)
                .unwrap_or_default();

            if streak >= binding::REFUSED_STREAK_BOUND {
                tracing::info!(
                    provider = ID,
                    key,
                    arm = Arm::Spawn.word(),
                    reason = "refused-twice",
                    spawned = false,
                    refused_streak = streak,
                    "not spending a third turn on this conversation"
                );

                return Ok(failed(
                    "refused twice in a row on this conversation — once with the transcript \
                     rendered, once with the prompts alone; open a new key with /compact, or a \
                     new session"
                        .to_owned(),
                ));
            }
        }

        self.open_fresh(key, reason, Arm::Spawn, reason, request, cancel).await
    }

    /// The fresh-record procedure, shared by Spawn and Recover.
    ///
    /// `sent := []`; the preamble's render **is a write**, so its ids join
    /// `sent` before `owed` is computed; the owed text goes into the same
    /// frame. Each user message of the request is therefore in that frame
    /// **exactly once** — once as a preamble line or once as owed text, never
    /// both — and `honest` holds on the next request.
    async fn open_fresh(
        &self,
        key: &held::Key,
        reason: &str,
        arm: Arm,
        ring_word: &str,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let locked_elsewhere = reason == "locked-elsewhere";

        // Refusing a turn to keep a count is the worse failure, so a spawn
        // that would exceed the cap evicts an idle entry if it can and
        // proceeds either way.
        if self.held.len() >= held::HELD_CAP {
            match self.held.evictable() {
                Some(idle) => self.held.close(&idle, held::Reason::Cap).await,
                None => tracing::info!(
                    provider = ID,
                    key,
                    held_over_cap = true,
                    held_entries = self.held.len(),
                    "every held process is busy; spawning anyway"
                ),
            }
        }

        let at = turn_start(&request);
        let session_id = crate::protocol::uuidv7();
        let effort = effort_of(&request);
        let argv = argv::Argv::conversation(&argv::Spawn {
            session_id: session_id.clone(),
            model: request.model.clone(),
            effort: effort.clone(),
        });

        let cwd = self.paths.cwd(key);
        prepare(&cwd)?;

        let io = self.spawner.spawn(&self.bin, &argv, &argv::ChildEnv { cwd: cwd.clone() })?;
        let (input, inputs) = tokio::sync::mpsc::channel(4);
        let (events, stream) = channel();

        let meta = Arc::new(Mutex::new(held::Meta::opening(
            session_id,
            request.model.clone(),
            effort,
            hash_of(&request.system),
            tools_hash(&request.tools),
        )));

        // `sent := []`. A fresh record has read nothing.
        let mut sent: Vec<String> = Vec::new();
        let mut preamble_dropped = false;
        let mut assistant_turns_dropped = 0;
        let mut paragraphs: Vec<String> = Vec::new();

        if reason == "refused-record" {
            // The user's asks alone: no header, no tool trail. The closest
            // measured shape to the prompt alone, and one that quotes no model
            // output — the model loses the conversation, not only its own
            // words, and that degradation is what this line names.
            preamble_dropped = true;
        } else {
            let rendered = preamble::render(&request.messages[..at]);
            if !rendered.text.is_empty() {
                assistant_turns_dropped = rendered.assistant_turns_dropped;
                // The render is a write.
                sent.extend(rendered.user_ids);
                paragraphs.push(rendered.text);
            }

            if arm == Arm::Recover {
                // The turn's own prompt and its tool parts, never the reply's
                // text, closed by the line that says the calls were answered.
                let turn = preamble::render_turn(&request.messages[at..]);
                sent.extend(turn.user_ids);
                paragraphs.push(turn.text);
            }
        }

        let owed = owed(&sent, &request);
        let text = owed_text(&owed);
        if !text.trim().is_empty() {
            paragraphs.push(text);
        }
        sent.extend(owed.iter().map(|message| message.id.as_str().to_owned()));

        let wiring = held::Wiring {
            key: key.clone(),
            meta: Arc::clone(&meta),
            table: Arc::downgrade(&self.held),
            slots: self.slots.clone(),
            binding: (!locked_elsewhere).then(|| self.paths.binding(key)),
            cwd: Some(cwd),
            tools: request.tools.clone(),
            requested_model: request.model.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            opening: frame::initialize_line(
                &crate::protocol::uuidv7(),
                &frame::Initialize {
                    system_prompt: request.system.clone().map(|system| vec![system]),
                    sdk_mcp_servers: vec![rpc::SERVER.to_owned()],
                    // One hour per `tools/call`, measured honoured and
                    // unclamped at a 25 s hold.
                    sdk_mcp_server_configs: serde_json::json!({
                        rpc::SERVER: {"timeout": 3_600_000}
                    }),
                },
            ),
            one_shot: false,
        };

        let task = tokio::spawn(held::run(io, inputs, wiring, self.held.idle_bound()));
        self.held.insert(key.clone(), held::Held { input: input.clone(), meta, task });

        // The eviction notice comes down when the record that pays for it
        // opens, so the sentence shows exactly between the two — and it is
        // taken down **conditionally**, never by an unconditional clear: the
        // slot is one a frontend reads, and wiping somebody else's key would
        // hide an eviction nobody has paid for yet.
        //
        // One lock, held once. An `if let` over the guard would still be
        // holding it when the body asked for it again, which a
        // `std::sync::Mutex` answers by never returning.
        {
            let mut slot = self.slots.eviction.lock().expect("the eviction slot is never poisoned");
            if slot.as_ref().is_some_and(|eviction| &eviction.key == key) {
                *slot = None;
            }
        }

        tracing::info!(
            provider = ID,
            key,
            arm = arm.word(),
            reason,
            ring = ring_word,
            owed = owed.len(),
            sent = sent.len(),
            preamble_dropped,
            assistant_turns_dropped,
            spawned = true,
            held_entries = self.held.len(),
            "fresh record"
        );

        let frame = frame::user_line(&frame::UserFrame {
            content: paragraphs.join("\n\n"),
            parent_tool_use_id: None,
        });
        let _ = input.send(held::Input::Turn { frame, sent, events }).await;
        self.watch_cancel(input, cancel);

        Ok(stream)
    }

    /// Ends the turn when the token fires, keeping the process.
    fn watch_cancel(
        &self,
        input: tokio::sync::mpsc::Sender<held::Input>,
        cancel: CancellationToken,
    ) {
        tokio::spawn(async move {
            cancel.cancelled().await;
            let _ = input.send(held::Input::Cancel).await;
        });
    }
}

/// The bounded channel a turn's events travel on, and the stream over it.
///
/// Bounded because a subscriber that stopped reading must not let the child's
/// frames accumulate without limit; the task drops rather than blocks, so a
/// stalled reader never stalls the process.
fn channel() -> (tokio::sync::mpsc::Sender<ProviderEvent>, BoxStream<'static, ProviderEvent>) {
    let (sender, receiver) = tokio::sync::mpsc::channel(256);
    let stream = stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|event| (event, receiver))
    });

    (sender, stream.boxed())
}

/// An empty directory, owner-only, for one child to run in.
///
/// **Never the project root and never `.`**: run 6 paid 7 348 extra prefix
/// tokens for a checkout as cwd, not itemised by any frame and not suppressed
/// by `--setting-sources ""`.
fn prepare(cwd: &Path) -> Result<(), ProviderError> {
    std::fs::create_dir_all(cwd).map_err(|error| {
        ProviderError::Transport(format!("could not make {}: {error}", cwd.display()))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(cwd, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
            ProviderError::Transport(format!("could not seal {}: {error}", cwd.display()))
        })?;
    }

    Ok(())
}

// `pub(crate)` for the reason `ganja_core`'s teammate module declares its own
// so: the fake-CLI harness lives here, and the sibling suites — `held_tests`,
// `process_tests` — drive the same wire through it rather than each building
// a second double that could drift from this one.
#[cfg(test)]
#[path = "claude_code_tests.rs"]
pub(crate) mod tests;
