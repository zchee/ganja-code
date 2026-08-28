//! Commands a config asks this build to run at nine named moments of a
//! session, and what their answers are allowed to change.
//!
//! Spec: Claude Code's hooks (2.1.x; docs at code.claude.com/docs/en/hooks,
//! wire protocol verified 2026-08-11 via gemini-search — the stdin envelope,
//! the exit-code table, `hookSpecificOutput`, and the matcher vocabularies).
//! Upstream opencode has **none** of this, so nothing here ports a TypeScript
//! file: **D456** (`hooks-are-a-claude-port`) names the whole family, and the
//! shape below is this port's reading of the observed contract.
//!
//! # The contract, in one place
//!
//! - A hook is spawned under the same POSIX shell the `bash` tool uses,
//!   `sh -c <command>`, with the project root as its working directory and the
//!   event's JSON envelope on its **standard input** (which is why this module
//!   owns its spawn rather than reusing `ShellTool`'s: that one hard-codes
//!   `Stdio::null()` for stdin, on purpose, because nothing is ever typed at a
//!   tool call).
//! - Every hook matching one event runs **concurrently**, and every one of
//!   them is awaited, each under a timeout of its own ([`crate::hook::DEFAULT_TIMEOUT`], or
//!   whatever the entry asked for).
//! - Exit **0** passes, and its stdout is read: a JSON object is the documented
//!   envelope, anything else is plain text — which is context for the two
//!   events that take it and nothing at all for the rest. Unparseable stdout
//!   never fails a hook.
//! - Exit **2** blocks, where blocking means something ([`crate::hook::HookEvent::blocking`]
//!   is the whole list), and its **stderr** is the sentence the model or the
//!   person then reads.
//! - Any other exit, a spawn that failed, and a hook killed for running too
//!   long are all the same thing: a **non-blocking failure**. What was about to
//!   happen still happens, and the failure is reported. A killed hook can never
//!   be read as approval — the only route to `allowed` is an explicit
//!   `permissionDecision` on a clean exit, which a dead process cannot have
//!   written.
//!
//! # Whose authority a hook runs with
//!
//! The user's, deliberately, and **no permission dialog gates one**. A hook is
//! not a tool call: the model neither chose it nor knows it ran. What makes
//! that safe is the same thing that makes a `command` template safe — it is in
//! a config file somebody wrote, so *authorship of the config is the trust
//! boundary*, and a build that asked permission to run the user's own hook
//! would be asking them to approve their own decision. The risk this leaves
//! standing is recorded rather than mitigated away: a hook can write files no
//! dialog approved. It is bounded by a timeout and by these documented
//! channels, and by nothing else.
//!
//! # Recursion
//!
//! Hooks never fire for a hook. Not by a guard but by construction: every fire
//! site is inside the engine — the agent loop, the turn boundary, the session
//! entries — and a hook's process reaches none of them. Nothing in this module
//! calls a fire site, so a hook running `rm -rf` fires no `PreToolUse`, and one
//! that spawns a shell fires no second round of itself.
//!
//! # Divergences beyond the family
//!
//! - **D457** (`hook-stdin-omits-transcript-path`): Claude's envelope carries a
//!   `transcript_path` naming a JSONL file. ganja's transcripts live in SQLite,
//!   so there is no such path to name and the field is **omitted** rather than
//!   filled with something that is not a transcript.
//! - **D458** (`hook-allow-skips-the-ask-never-a-deny`): a
//!   `permissionDecision: "allow"` skips the *dialog* for an ask-gated call.
//!   It does not overturn a `deny` rule. Claude documents allow as bypassing
//!   the permission system; here a deny is a standing decision the user already
//!   made in the same config file, and letting one key in it silently repeal
//!   another is not a thing this build will do.
//! - **D459** (`hook-failures-travel-the-log`): a non-blocking failure is
//!   reported through `tracing` and through the [`crate::hook::Outcome`] a fire site reads,
//!   not through a new protocol event — P13 defers `Event` growth by name, and
//!   a hook that failed must not be able to fail a turn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use ganja_tool::shell::{NoPosixShell, kill_tree, posix_shell};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use tokio::task::JoinSet;

use crate::config::{HookHandler, HookMatcher};

/// How long a hook may take when its entry does not say, in Claude's own
/// documented default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The `reason` every frontend gives a `SessionEnd`.
///
/// Claude's vocabulary here is four words — `clear`, `logout`,
/// `prompt_input_exit`, `other` — describing four ways its own UI ends a
/// session. This build has one: the process is stopping, whichever key or
/// signal said so. Naming it in one place is what keeps the three frontends
/// from each inventing a synonym.
pub const EXIT_REASON: &str = "exit";

/// The nine moments a hook can be attached to.
///
/// Spelled exactly as a config file spells them, because the config key **is**
/// the name: [`HookEvent::name`] and [`HookEvent::from_name`] are the two
/// halves of that, and `check_hooks` refuses anything the second one cannot
/// answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookEvent {
    /// Before a tool call runs, and early enough to refuse it.
    PreToolUse,
    /// After one has run, with what it answered.
    PostToolUse,
    /// Before the model is told what a person typed.
    UserPromptSubmit,
    /// When the session is waiting for a person — a permission dialog, a
    /// question.
    Notification,
    /// At the end of a turn the session ran.
    Stop,
    /// At the end of a turn a subagent ran.
    SubagentStop,
    /// When a session opens, either freshly or by resuming a stored one.
    SessionStart,
    /// When a frontend is shutting the session down.
    SessionEnd,
    /// Before a conversation is summarized into its own window.
    PreCompact,
}

/// Every [`HookEvent`], in the order the documentation lists them — which is
/// the order a refusal spells them back to whoever misspelled one.
pub const EVENTS: [HookEvent; 9] = [
    HookEvent::PreToolUse,
    HookEvent::PostToolUse,
    HookEvent::UserPromptSubmit,
    HookEvent::Notification,
    HookEvent::Stop,
    HookEvent::SubagentStop,
    HookEvent::SessionStart,
    HookEvent::SessionEnd,
    HookEvent::PreCompact,
];

impl HookEvent {
    /// The name a config file writes, and the `hook_event_name` the envelope
    /// carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Notification => "Notification",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::PreCompact => "PreCompact",
        }
    }

    /// The event `name` spells, or [`None`] for a name nothing fires.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        EVENTS.into_iter().find(|event| event.name() == name)
    }

    /// Whether a hook for this event can refuse what is about to happen.
    ///
    /// The v1 list, and a short one on purpose: blocking is only honest where
    /// something has not happened yet *and* there is somewhere to put the
    /// refusal. A `Stop` hook's exit 2 would have to restart a finished turn,
    /// which is Claude's forced-continuation behavior and a recorded follow-up;
    /// until then an exit 2 on a non-blocking event is reported like any other
    /// failure rather than silently doing nothing.
    #[must_use]
    pub const fn blocking(self) -> bool {
        matches!(self, Self::PreToolUse | Self::UserPromptSubmit)
    }

    /// Whether stdout that is not the JSON envelope is context for the model.
    ///
    /// Claude's own asymmetry: these two events are the ones whose whole point
    /// is adding something to what the model is about to read, so a hook that
    /// prints a line is understood there and merely logged everywhere else.
    #[must_use]
    const fn takes_plain_stdout(self) -> bool {
        matches!(self, Self::UserPromptSubmit | Self::SessionStart)
    }
}

/// Why a session is opening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A frontend just started on a fresh conversation.
    Startup,
    /// A stored session was reopened.
    Resume,
}

impl Source {
    /// The word the envelope carries, and what a `SessionStart` matcher is
    /// matched against.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
        }
    }
}

/// What asked for a compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The window filled up.
    Auto,
    /// Somebody asked for one.
    Manual,
}

impl Trigger {
    /// The word the envelope carries, and what a `PreCompact` matcher is
    /// matched against.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

/// One fire, with everything its event's envelope carries beyond the three
/// common fields.
///
/// One variant per [`HookEvent`], so a fire site cannot name an event and then
/// fail to say what it is about.
#[derive(Clone, Debug)]
pub enum Payload {
    /// A tool call that has not run yet.
    PreToolUse {
        /// The registry id, `mcp__server__tool` included.
        tool_name: String,
        /// The arguments as the model wrote them.
        tool_input: Value,
    },
    /// A tool call that has.
    PostToolUse {
        /// The registry id.
        tool_name: String,
        /// The arguments as the model wrote them.
        tool_input: Value,
        /// What it answered, in this build's shape: the text the model reads,
        /// the title a frontend renders, and whatever metadata the tool
        /// attached.
        tool_response: Value,
    },
    /// Something a person typed, before the model hears it.
    UserPromptSubmit {
        /// The text itself.
        prompt: String,
    },
    /// The session is waiting for a person.
    Notification {
        /// What it is waiting for, in a sentence.
        message: String,
    },
    /// A turn of the session's own ended.
    Stop {
        /// Always `false` in this build: forced continuation is a recorded
        /// follow-up, so no turn here is ever running *because* a Stop hook
        /// asked for it. Carried anyway, because a hook written against
        /// Claude's envelope reads this field to avoid looping.
        stop_hook_active: bool,
    },
    /// A turn a subagent ran ended.
    SubagentStop {
        /// As [`Payload::Stop`].
        stop_hook_active: bool,
        /// Which agent the child ran as (**D461**).
        agent: String,
        /// How its turn ended, in a word (**D461**).
        outcome: String,
    },
    /// A session opened.
    SessionStart {
        /// Freshly, or by resuming.
        source: Source,
    },
    /// A session is being shut down.
    SessionEnd {
        /// Why, in a word.
        reason: String,
    },
    /// A conversation is about to be summarized.
    PreCompact {
        /// What asked for it.
        trigger: Trigger,
    },
}

impl Payload {
    /// Which event this is about.
    #[must_use]
    pub fn event(&self) -> HookEvent {
        match self {
            Self::PreToolUse { .. } => HookEvent::PreToolUse,
            Self::PostToolUse { .. } => HookEvent::PostToolUse,
            Self::UserPromptSubmit { .. } => HookEvent::UserPromptSubmit,
            Self::Notification { .. } => HookEvent::Notification,
            Self::Stop { .. } => HookEvent::Stop,
            Self::SubagentStop { .. } => HookEvent::SubagentStop,
            Self::SessionStart { .. } => HookEvent::SessionStart,
            Self::SessionEnd { .. } => HookEvent::SessionEnd,
            Self::PreCompact { .. } => HookEvent::PreCompact,
        }
    }

    /// The string a `matcher` is matched against, when the event has one.
    ///
    /// [`None`] is an event with nothing to match on — a turn ending, a prompt,
    /// a notification — where every group applies whatever it wrote, because a
    /// matcher there names no property of anything.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        match self {
            Self::PreToolUse { tool_name, .. } | Self::PostToolUse { tool_name, .. } => {
                Some(tool_name)
            }
            Self::SessionStart { source } => Some(source.as_str()),
            Self::PreCompact { trigger } => Some(trigger.as_str()),
            Self::UserPromptSubmit { .. }
            | Self::Notification { .. }
            | Self::Stop { .. }
            | Self::SubagentStop { .. }
            | Self::SessionEnd { .. } => None,
        }
    }

    /// Everything this event adds to the three common fields.
    fn fields(&self) -> Vec<(&'static str, Value)> {
        match self {
            Self::PreToolUse { tool_name, tool_input } => {
                vec![("tool_name", json!(tool_name)), ("tool_input", tool_input.clone())]
            }
            Self::PostToolUse { tool_name, tool_input, tool_response } => vec![
                ("tool_name", json!(tool_name)),
                ("tool_input", tool_input.clone()),
                ("tool_response", tool_response.clone()),
            ],
            Self::UserPromptSubmit { prompt } => vec![("prompt", json!(prompt))],
            Self::Notification { message } => vec![("message", json!(message))],
            Self::Stop { stop_hook_active } => vec![("stop_hook_active", json!(stop_hook_active))],
            Self::SubagentStop { stop_hook_active, agent, outcome } => vec![
                ("stop_hook_active", json!(stop_hook_active)),
                ("agent", json!(agent)),
                ("outcome", json!(outcome)),
            ],
            Self::SessionStart { source } => vec![("source", json!(source.as_str()))],
            Self::SessionEnd { reason } => vec![("reason", json!(reason))],
            Self::PreCompact { trigger } => vec![("trigger", json!(trigger.as_str()))],
        }
    }
}

/// The JSON one hook reads on its standard input.
///
/// Three common fields — `session_id`, `cwd`, `hook_event_name` — and then the
/// event's own. **No `transcript_path`**: see D457 in the module docs.
#[must_use]
pub fn envelope(session_id: &str, cwd: &Path, payload: &Payload) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("session_id".to_owned(), json!(session_id));
    object.insert("cwd".to_owned(), json!(cwd.display().to_string()));
    object.insert("hook_event_name".to_owned(), json!(payload.event().name()));
    for (key, value) in payload.fields() {
        object.insert(key.to_owned(), value);
    }

    Value::Object(object)
}

/// What every hook for one event said, folded together.
///
/// Empty is the answer for an event nothing was configured for, and it is also
/// the answer for hooks that all ran cleanly and said nothing — which is the
/// point: a fire site reads this the same way either way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Text to put in front of the model: `additionalContext`, and plain
    /// stdout on the two events that take it.
    pub context: Vec<String>,
    /// What a person should be told: a hook that failed, timed out, or asked
    /// for a `systemMessage`. Reported through the log (D459), never through a
    /// failed turn.
    pub notices: Vec<String>,
    /// Why this must not proceed, on the two events where that means something.
    /// [`None`] everywhere else, always — a refusal on a non-blocking event is
    /// folded into [`Outcome::notices`] instead of quietly doing nothing.
    pub blocked: Option<String>,
    /// A hook said this call may run without asking. Skips the dialog and
    /// nothing else (D458).
    pub allowed: bool,
}

impl Outcome {
    /// Records a refusal, keeping the first reason: several hooks may refuse
    /// one call, and the one that got there first is the one whose sentence is
    /// about the decision the others then agreed with.
    fn block(&mut self, reason: String) {
        if self.blocked.is_none() {
            self.blocked = Some(reason);
        } else {
            self.notices.push(reason);
        }
    }

    /// Writes every notice to the log, which is where a hook failure is
    /// reported (D459). Called by the fire sites that have nowhere else to put
    /// one, so the reporting cannot be forgotten at a site that only cares
    /// about the blocking half.
    pub fn report(&self, event: HookEvent) {
        for notice in &self.notices {
            tracing::warn!(event = event.name(), "{notice}");
        }
    }
}

/// One configured group: whom it applies to, and what it runs.
struct Group {
    /// [`None`] matches everything — an absent matcher, an empty one, or one
    /// that would not compile (refused at load, warned about here).
    matcher: Option<regex::Regex>,
    handlers: Vec<Handler>,
}

/// One command to run.
#[derive(Clone)]
struct Handler {
    command: String,
    budget: Duration,
}

/// Every hook this session may run, compiled once.
///
/// Built from the config's `hooks` block and then shared by the engine, every
/// turn it starts, and every subagent those turns spawn — one regex compiled
/// per group per session rather than per call.
pub struct Hooks {
    groups: BTreeMap<HookEvent, Vec<Group>>,
    /// Where a hook runs: the project root, so `git status` in a hook means
    /// what a person standing in the checkout would mean by it.
    cwd: PathBuf,
    /// The shell every hook is handed to, or why this machine offers none —
    /// held as the refusal for [`ganja_tool::shell::ShellTool`]'s reason: a
    /// machine with no POSIX shell says so the same way every time.
    shell: Result<PathBuf, NoPosixShell>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hooks")
            .field("events", &self.groups.keys().map(|event| event.name()).collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl Hooks {
    /// Compiles what a config asked for, or [`None`] when it asked for nothing.
    ///
    /// [`None`] rather than an empty registry, mirroring
    /// [`crate::lsp::Lsp::new`]: an engine with no hooks then does no hook work
    /// at all rather than doing inert hook work at nine seams.
    ///
    /// An event name nothing answers to is dropped with a warning rather than
    /// refused — `config::check_hooks` already refuses one at load, so reaching
    /// this arm means the map came from somewhere else, and taking down a
    /// session over it would be the wrong end of the trade.
    #[must_use]
    pub fn new(config: &BTreeMap<String, Vec<HookMatcher>>, cwd: &Path) -> Option<Arc<Self>> {
        let mut groups: BTreeMap<HookEvent, Vec<Group>> = BTreeMap::new();
        for (name, configured) in config {
            let Some(event) = HookEvent::from_name(name) else {
                tracing::warn!(event = name.as_str(), "no hook event by that name; ignoring it");
                continue;
            };
            for group in configured {
                let handlers: Vec<Handler> = group
                    .hooks
                    .iter()
                    .map(|handler| {
                        let HookHandler::Command(command) = handler;
                        Handler {
                            command: command.command.clone(),
                            budget: command.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs),
                        }
                    })
                    .collect();
                if handlers.is_empty() {
                    continue;
                }

                let matcher = group
                    .matcher
                    .as_deref()
                    .filter(|matcher| !matcher.is_empty())
                    .and_then(|matcher| match regex::Regex::new(matcher) {
                        Ok(compiled) => Some(compiled),
                        Err(error) => {
                            tracing::warn!(
                                event = event.name(),
                                %error,
                                "a hook matcher is not a regular expression; it will match nothing"
                            );
                            None
                        }
                    });
                // A matcher that would not compile must not become "matches
                // everything": the group asked to be narrow, and widening it
                // would run somebody's command against calls they excluded.
                let matcher = match (&group.matcher, matcher) {
                    (Some(asked), None) if !asked.is_empty() => Some(never()),
                    (_, compiled) => compiled,
                };

                groups.entry(event).or_default().push(Group { matcher, handlers });
            }
        }
        if groups.is_empty() {
            return None;
        }

        Some(Arc::new(Self { groups, cwd: cwd.to_owned(), shell: posix_shell() }))
    }

    /// Whether anything at all is configured for `event`, which is what lets a
    /// fire site skip building a payload nobody will read.
    #[must_use]
    pub fn fires(&self, event: HookEvent) -> bool {
        self.groups.contains_key(&event)
    }

    /// Runs every hook matching `payload`, concurrently, and folds what they
    /// said into one [`Outcome`].
    ///
    /// Never fails: a hook that could not be spawned, exited badly or ran too
    /// long is a notice, and the caller proceeds. The only thing that can stop
    /// a caller is an explicit refusal on an event where refusing means
    /// something.
    pub async fn fire(&self, session_id: &str, payload: &Payload) -> Outcome {
        let event = payload.event();
        let Some(groups) = self.groups.get(&event) else {
            return Outcome::default();
        };
        let subject = payload.subject();
        let matching: Vec<Handler> = groups
            .iter()
            .filter(|group| matches(group, subject))
            .flat_map(|group| group.handlers.iter().cloned())
            .collect();
        if matching.is_empty() {
            return Outcome::default();
        }

        let shell = match &self.shell {
            Ok(shell) => shell.clone(),
            Err(why) => {
                let mut outcome = Outcome::default();
                outcome.notices.push(why.to_string());
                return outcome;
            }
        };
        let text = envelope(session_id, &self.cwd, payload).to_string();

        let mut running = JoinSet::new();
        for (index, handler) in matching.into_iter().enumerate() {
            let shell = shell.clone();
            let cwd = self.cwd.clone();
            let text = text.clone();
            running.spawn(async move {
                let ran = run(&shell, &cwd, &handler, &text).await;

                (index, handler.command, ran)
            });
        }

        let mut reports = Vec::new();
        while let Some(joined) = running.join_next().await {
            match joined {
                Ok(report) => reports.push(report),
                // A hook task that panicked is a bug in this module rather than
                // in somebody's command, and it is still not worth a turn.
                Err(error) => tracing::error!(%error, "a hook task did not finish"),
            }
        }
        // Completion order is whatever the machine did; configuration order is
        // what somebody wrote and can reason about, so that is the order the
        // context and the notices come back in.
        reports.sort_by_key(|(index, _, _)| *index);

        let mut outcome = Outcome::default();
        for (_, command, ran) in reports {
            absorb(&mut outcome, event, &command, ran);
        }
        // Blocking is only honest where something has not happened yet. An
        // exit 2 anywhere else is reported like the failure it is, rather than
        // being carried back to a caller with nothing to do about it.
        if !event.blocking()
            && let Some(reason) = outcome.blocked.take()
        {
            outcome.notices.push(reason);
        }

        outcome
    }
}

/// A regular expression matching nothing, for a group whose own matcher would
/// not compile. `\z` cannot be followed by a character, so no input reaches an
/// accepting state.
fn never() -> regex::Regex {
    regex::Regex::new(r"\z.").expect("a literal that matches nothing compiles")
}

/// Whether `group` applies to an event whose subject is `subject`.
///
/// An event with no subject matches every group, matcher or not: a matcher
/// there names a property nothing has, and the alternative — treating it as a
/// mismatch — would make a hook silently never fire for writing a field the
/// event does not use.
fn matches(group: &Group, subject: Option<&str>) -> bool {
    match (&group.matcher, subject) {
        (None, _) | (Some(_), None) => true,
        (Some(matcher), Some(subject)) => matcher.is_match(subject),
    }
}

/// What one hook did.
#[derive(Debug)]
enum Ran {
    /// It finished. [`None`] as the code is a process a signal ended.
    Finished { code: Option<i32>, stdout: String, stderr: String },
    /// It never ran, or it ran too long and was killed. Carries the sentence
    /// the notice is built from.
    Failed(String),
}

/// Spawns one hook, feeds it the envelope, and waits for it under its own
/// budget.
///
/// The three pipes are driven **concurrently**: a hook that writes more than a
/// pipe buffer holds before reading its stdin would otherwise deadlock against
/// a writer waiting for it to read, and the deadlock would only show up for
/// somebody whose hook printed a lot.
async fn run(shell: &Path, cwd: &Path, handler: &Handler, text: &str) -> Ran {
    let mut spawner = Command::new(shell);
    spawner
        .arg("-c")
        .arg(&handler.command)
        .current_dir(cwd)
        // The whole reason this module owns its spawn: the envelope is what a
        // hook reads, and `ShellTool::spawn` nulls stdin by design.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // As every other child this build starts: the group is what makes the tree
    // killable when the budget runs out, so a hook that forked something slow
    // does not leave it behind.
    #[cfg(unix)]
    spawner.process_group(0);

    let mut child = match spawner.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ran::Failed(format!("could not be started by {}: {error}", shell.display()));
        }
    };

    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let bytes = text.as_bytes().to_vec();

    let finished = tokio::time::timeout(handler.budget, async {
        let feed = async {
            if let Some(stdin) = &mut stdin {
                // A hook that never reads its stdin, or closes it early, is
                // allowed to: the write failing is not the hook failing.
                let _ = stdin.write_all(&bytes).await;
            }
            // Dropping the pipe is the EOF a hook reading to the end waits for.
            stdin.take();
        };
        let drain_out = async {
            if let Some(stdout) = &mut stdout {
                let _ = stdout.read_to_end(&mut out).await;
            }
        };
        let drain_err = async {
            if let Some(stderr) = &mut stderr {
                let _ = stderr.read_to_end(&mut err).await;
            }
        };
        tokio::join!(feed, drain_out, drain_err);

        child.wait().await
    })
    .await;

    match finished {
        Ok(Ok(status)) => Ran::Finished {
            code: status.code(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        },
        Ok(Err(error)) => Ran::Failed(format!("could not be waited for: {error}")),
        Err(_) => {
            // Pre-mortem #2, pinned here: the kill is where a hook that hung on
            // a terminal read ends up, and this arm returns a **failure**. It
            // has no route to `allowed`, so a hook killed mid-dialog can never
            // be mistaken for one that approved something.
            kill_tree(&mut child).await;

            Ran::Failed(format!(
                "took longer than {} seconds and was killed",
                handler.budget.as_secs()
            ))
        }
    }
}

/// Folds one hook's result into the outcome being built.
fn absorb(outcome: &mut Outcome, event: HookEvent, command: &str, ran: Ran) {
    match ran {
        Ran::Failed(why) => outcome.notices.push(format!("hook `{command}` {why}")),
        Ran::Finished { code: Some(0), stdout, .. } => {
            absorb_stdout(outcome, event, command, &stdout)
        }
        Ran::Finished { code: Some(2), stderr, .. } => {
            let reason = stderr.trim();
            outcome.block(if reason.is_empty() {
                format!("a {} hook (`{command}`) refused it", event.name())
            } else {
                reason.to_owned()
            });
        }
        Ran::Finished { code, stderr, .. } => {
            let ending = match code {
                Some(code) => format!("exited with {code}"),
                None => "was ended by a signal".to_owned(),
            };
            let detail = stderr.trim();
            outcome.notices.push(if detail.is_empty() {
                format!("hook `{command}` {ending}")
            } else {
                format!("hook `{command}` {ending}: {detail}")
            });
        }
    }
}

/// Reads what a hook that passed printed.
fn absorb_stdout(outcome: &mut Outcome, event: HookEvent, command: &str, stdout: &str) {
    let text = stdout.trim();
    if text.is_empty() {
        return;
    }

    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => absorb_envelope(outcome, event, command, &map),
        // Never a failure: a hook that echoed something is a hook that
        // succeeded, and Claude's own behavior is to read it as text.
        _ => {
            if event.takes_plain_stdout() {
                outcome.context.push(text.to_owned());
            } else {
                tracing::debug!(
                    event = event.name(),
                    command,
                    "a hook printed something this event does not read as context"
                );
            }
        }
    }
}

/// Reads the documented JSON envelope a hook may answer with.
fn absorb_envelope(
    outcome: &mut Outcome,
    event: HookEvent,
    command: &str,
    map: &serde_json::Map<String, Value>,
) {
    let message = map
        .get("systemMessage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());

    if map.get("continue").and_then(Value::as_bool) == Some(false) {
        let reason = map
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .or(message)
            .map_or_else(
                || format!("a {} hook (`{command}`) asked to stop", event.name()),
                str::to_owned,
            );
        outcome.block(reason);
    } else if let Some(message) = message {
        outcome.notices.push(message.to_owned());
    }

    let Some(specific) = map.get("hookSpecificOutput").and_then(Value::as_object) else {
        return;
    };
    if let Some(context) = specific
        .get("additionalContext")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|context| !context.is_empty())
    {
        outcome.context.push(context.to_owned());
    }

    let Some(decision) = specific.get("permissionDecision").and_then(Value::as_str) else {
        return;
    };
    let reason = || {
        specific
            .get("permissionDecisionReason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_owned)
    };
    match decision {
        "allow" => outcome.allowed = true,
        "deny" => outcome.block(
            reason().unwrap_or_else(|| format!("a {} hook (`{command}`) denied it", event.name())),
        ),
        // The documented default flow: the call is judged by the rules exactly
        // as it would have been with no hook at all.
        "ask" => {}
        other => outcome
            .notices
            .push(format!("hook `{command}` asked for an unknown permissionDecision \"{other}\"")),
    }
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod tests;
