//! The agent loop: one turn, as many model requests as its tool calls demand.
//!
//! Spec: upstream `packages/opencode/src/session/processor.ts` and
//! `prompt.ts`. A turn repeats — mark a step, ask the model, execute whatever
//! tools it called — until a request ends without tool calls, and everything
//! that happens lands in one assistant message: step markers, streamed text,
//! and each tool call from `Pending` through `Completed` or `Error`.
//!
//! Tool results are information, never control flow. A refused permission, an
//! unknown tool, unparseable arguments, a tool that failed — each becomes the
//! error text the model reads on the next request, and the loop continues.
//! Upstream stops the turn on a refusal unless
//! `experimental.continue_loop_on_deny` is set; this port adopts the continue
//! behaviour outright, because a refusal answers one call, not the question
//! the user asked. The two ways a turn ends early are the two that mean it:
//! the user cancelled, or the provider died.
//!
//! There is no step cap. Upstream's processor has none either — the bound in
//! `prompt.ts` is `agent.steps ?? Infinity`, off by default — so the escape
//! hatch is [`CancelTurn`](crate::protocol::Command::CancelTurn), exactly as
//! it is upstream.
//!
//! P4 makes the loop write itself through. On a persistent engine every
//! envelope and part reaches [`crate::storage`] as it happens, an over-budget
//! session is summarized before the turn begins (spec: upstream
//! `packages/core/src/session/compaction.ts`, which wins over
//! `packages/opencode/src/session/compaction.ts` where the two diverge), and
//! a completed turn on an untitled session earns one (spec: upstream
//! `packages/opencode/src/session/prompt.ts`, `ensureTitle`). None of this
//! exists on an in-memory engine — no store, no write-through, no title, no
//! compaction — which is what keeps golden, scripted and PTY runs
//! deterministic. A persistence failure is a warning, never a dead turn:
//! losing the disk must not kill the conversation.

use std::{collections::HashMap, ops::ControlFlow, path::PathBuf, sync::Arc, time::Duration};

use futures::StreamExt as _;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::{
    agent, catalog,
    engine::{Fanout, PendingSwitch, SwitchToBuild},
    permission::{Decision, Permissions},
    protocol::{
        Event, FinishReason, Message, Part, PartBody, PartId, PermissionId, PermissionReply,
        QuestionAnswer, QuestionId, QuestionInfo, QuestionOption, QuestionSource, Role, ToolState,
        Usage, now,
    },
    provider::{ChatRequest, Provider, ProviderEvent},
    storage::{SessionId, SessionInfo, Storage, StorageError},
    tool::{
        Credentials, FileTimes, Registry, Tool, ToolCtx, ToolError, ToolOutput, question, shell,
    },
};

/// What the model reads when the user refuses a call, ported verbatim from
/// upstream `packages/core/src/v1/permission.ts` (`RejectedError`).
const REJECTED: &str = "The user rejected permission to use this specific tool call.";

/// What the model reads when a rule refuses a call before anyone is asked,
/// ported from upstream `packages/core/src/v1/permission.ts` (`DeniedError`).
///
/// The rules travel with the message, as upstream's do: a model told only that
/// it may not do something tries the same thing spelled differently, where one
/// told *which rule* stopped it can work out what else the rule covers.
fn denied(rules: &[crate::permission::Rule]) -> String {
    let rendered = serde_json::to_string(rules).unwrap_or_else(|_| "[]".to_owned());

    format!(
        "The user has specified a rule which prevents you from using this specific tool call. \
         Here are some of the relevant rules {rendered}"
    )
}

/// What a buffered call reads when the provider died before it could run.
///
/// No upstream analogue: the AI SDK executes tools mid-stream, so a provider
/// that dies never leaves a parsed-but-unstarted call behind. Here calls run
/// after the stream ends, and a part that opened `Pending` has to close.
const STRANDED: &str = "the provider failed before this call could run";

/// Upstream `tool/invalid.ts`: a call that cannot run is answered through the
/// `invalid` tool, and this is the shape of its output.
fn invalid_call(detail: &str) -> String {
    format!("The arguments provided to the tool are invalid: {detail}")
}

/// What the model reads when a `PreToolUse` hook refused a call.
///
/// `reason` is the hook's own words — its stderr on an exit 2, or its
/// `permissionDecisionReason` — and it is quoted whole, because the point of
/// blocking with a message is that the model is told what to do differently.
/// The sentence around it names the hook so the model does not read a refusal
/// somebody's script wrote as a refusal the person just gave.
fn blocked_by_hook(reason: &str) -> String {
    format!("A PreToolUse hook refused this tool call: {reason}")
}

/// Environment variable that opts the fake provider into a real title
/// request. Unset — the default — a fake session takes the fallback title
/// without any provider call, because scripted demos and PTY tests count
/// every request and a hidden one would desynchronize their scripts.
pub const FAKE_TITLE_ENV: &str = "GANJA_FAKE_TITLE";

/// How long a streamed text fragment may sit unwritten. Each delta rewrites
/// the part's whole file, so writing every fragment would turn a fast stream
/// into a disk benchmark; a quarter second of loss is what a `kill -9` is
/// allowed to cost.
const TEXT_FLUSH: Duration = Duration::from_millis(250);

/// How long a cancelled tool is still polled before its future is abandoned.
///
/// A tool cleans up *inside* the future it returned: the shell tool kills its
/// command's process group there (`tool/shell.rs`, `kill_tree`). Dropping
/// that future the instant a cancel lands — which is what losing a plain race
/// does — never reaches that code, so the handle's own `kill_on_drop` takes
/// the direct child and everything the command forked keeps running; a
/// cancelled `sleep 300` outlives the turn. So the cancel signals the token
/// the tool watches and then keeps polling the same future for this long.
///
/// It is a ceiling, not a wait: the tool returning ends it, so the slowest
/// builtin cleanup (the shell tool's 200ms `SIGTERM` grace plus its 100ms
/// output drain) fits inside it with room to spare, a tool that ignores the
/// cancel costs the turn no more than this, and a turn with no tool running
/// never reaches it at all.
const TOOL_CANCEL_GRACE: Duration = Duration::from_millis(500);

/// Characters a fallback title keeps of the first prompt.
const FALLBACK_TITLE_CHARS: usize = 50;

/// The whole of the synthetic user message a `!` passthrough writes, ported
/// verbatim from upstream `packages/opencode/src/session/prompt.ts`
/// (`shellImpl`). The model reads it as the reason a `bash` call it never made
/// is sitting in the transcript.
const SHELL_NOTICE: &str = "The following tool was executed by the user";

// What an `@`-mentioned file is labelled as moved to `crate::attachment`:
// the mime is the extension's now, `text/plain` for everything outside the
// allowlist. Upstream also mints `application/x-directory` for a directory,
// which ganja does not attach at all (deviation: mentions-resolve-files-only).

/// Characters of a tool's output the summarize request is shown, upstream
/// core's `TOOL_OUTPUT_MAX_CHARS`. Counted in characters where upstream
/// counts UTF-16 units, because a byte slice at 2000 could split a code
/// point.
const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;

/// Tokens reserved for the summary reply, upstream core's
/// `SUMMARY_OUTPUT_TOKENS`: a summarize prompt estimated past
/// `context_window` minus this is not sent at all.
const SUMMARY_OUTPUT_TOKENS: u64 = 4_096;

/// The system prompt of a title request, ported verbatim from upstream
/// `packages/opencode/src/agent/prompt/title.txt` (MIT; see
/// `THIRD_PARTY_NOTICES.md`).
const TITLE_PROMPT: &str = r#"You are a title generator. You output ONLY a thread title. Nothing else.

<task>
Generate a brief title that would help the user find this conversation later.

Follow all rules in <rules>
Use the <examples> so you know what a good title looks like.
Your output must be:
- A single line
- ≤50 characters
- No explanations
</task>

<rules>
- you MUST use the same language as the user message you are summarizing
- Title must be grammatically correct and read naturally - no word salad
- Never include tool names in the title (e.g. "read tool", "bash tool", "edit tool")
- Focus on the main topic or question the user needs to retrieve
- Vary your phrasing - avoid repetitive patterns like always starting with "Analyzing"
- When a file is mentioned, focus on WHAT the user wants to do WITH the file, not just that they shared it
- Keep exact: technical terms, numbers, filenames, HTTP codes
- Remove: the, this, my, a, an
- Never assume tech stack
- Never use tools
- NEVER respond to questions, just generate a title for the conversation
- The title should NEVER include "summarizing" or "generating" when generating a title
- DO NOT SAY YOU CANNOT GENERATE A TITLE OR COMPLAIN ABOUT THE INPUT
- Always output something meaningful, even if the input is minimal.
- If the user message is short or conversational (e.g. "hello", "lol", "what's up", "hey"):
  → create a title that reflects the user's tone or intent (such as Greeting, Quick check-in, Light chat, Intro message, etc.)
</rules>

<examples>
"debug 500 errors in production" → Debugging production 500 errors
"refactor user service" → Refactoring user service
"why is app.js failing" → app.js failure investigation
"implement rate limiting" → Rate limiting implementation
"how do I connect postgres to my API" → Postgres API connection
"best practices for React hooks" → React hooks best practices
"@src/auth.ts can you add refresh token support" → Auth refresh token support
"@utils/parser.ts this is broken" → Parser bug fix
"look at @config.json" → Config review
"@App.tsx add dark mode toggle" → Dark mode toggle in App
</examples>"#;

/// What the title request asks, ported verbatim from upstream
/// `packages/opencode/src/session/prompt.ts` (`ensureTitle`).
const TITLE_INSTRUCTION: &str = "Generate a title for this conversation:\n";

/// The summary shape a compaction request demands, ported verbatim from
/// upstream `packages/core/src/session/compaction.ts` (`SUMMARY_TEMPLATE`,
/// MIT; see `THIRD_PARTY_NOTICES.md`).
const SUMMARY_TEMPLATE: &str = r#"Output exactly the Markdown structure shown inside <template> and keep the section order unchanged. Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Important Details
- [constraints/preferences, decisions and why, important facts/assumptions, exact context needed to continue, or "(none)"]

## Work State
### Completed
- [finished work, verified facts, or changes made; otherwise "(none)"]

### Active
- [current work, partial changes, or investigation state; otherwise "(none)"]

### Blocked
- [blockers, failing commands, or unknowns; otherwise "(none)"]

## Next Move
1. [immediate concrete action, or "(none)"]
2. [next action if known, or "(none)"]

## Relevant Files
- [file or directory path: why it matters, or "(none)"]
</template>

Rules:
- Keep every section, even when empty.
- Use terse bullets, not prose paragraphs.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers when known.
- Do not mention the summary process or that context was compacted."#;

/// What the engine keeps about the turn in flight; holding one is what makes
/// the engine busy.
pub(crate) struct TurnHandle {
    /// Stops the turn: the provider stream, a running tool, a permission wait.
    pub(crate) cancel: CancellationToken,
    /// The waits a reply command answers, by request id. Shared with the turn
    /// task, which opens and closes them; several can be open at once, because
    /// a step's batched `task` calls each run a child that may ask.
    pub(crate) permission: Arc<std::sync::Mutex<PendingReplies>>,
    /// Messages a [`Command::Steer`] left for this turn to take on, and the
    /// ones it already took. Shared with the turn task, which is the only
    /// thing that drains it — the same per-turn-cell shape as the permission
    /// wait above, and for the same reason: the engine's command paths never
    /// reach into a running turn except through a cell the turn owns.
    ///
    /// [`Command::Steer`]: crate::protocol::Command::Steer
    pub(crate) steer: Arc<std::sync::Mutex<Steering>>,
}

/// One mid-turn message, as a [`Command::Steer`] handed it over.
///
/// [`Command::Steer`]: crate::protocol::Command::Steer
pub(crate) struct SteerInput {
    /// The frontend's correlation id, echoed back as
    /// [`Event::SteerConsumed`] the moment this becomes a message.
    pub(crate) id: String,
    pub(crate) text: String,
    /// Read when the request carrying this message is built, never here: a
    /// steer's mentions obey the same read-at-send rule a prompt's do.
    pub(crate) mentions: Vec<crate::protocol::Mention>,
}

/// The running turn's mailbox: what has been handed to it, and what it has
/// already taken.
///
/// Both halves live in one cell because they are two views of the same queue
/// and are only ever moved together — a drain pops from [`waiting`] and pushes
/// to [`consumed`] under one lock, so no reader can see a message that has
/// left the first and not yet reached the second.
///
/// [`waiting`]: Steering::waiting
/// [`consumed`]: Steering::consumed
#[derive(Default)]
pub(crate) struct Steering {
    /// Messages that arrived and have not been drained. A cancelled turn
    /// leaves them here untouched: nobody consumed them, no
    /// [`Event::SteerConsumed`] claimed they were, and the frontend's own
    /// fallback lane owns them from the [`Event::MessageFinished`] onward.
    ///
    /// The same is true of a turn that has no step loop to drain them with —
    /// a `!` passthrough or a compaction, neither of which asks the model
    /// anything. A steer sent into one of those is accepted (the slot *is*
    /// occupied) and simply never consumed, which is the honest answer: there
    /// was no model request for it to join.
    waiting: Vec<SteerInput>,
    /// The user messages already drained, in drain order.
    ///
    /// They are *appended after* the assistant message in both the requests
    /// this turn builds and the history it leaves behind, which is the order
    /// their stored ids already sort in — the assistant's id was minted when
    /// the turn opened, before any of these existed. A resumed session
    /// therefore replays exactly what the live turn asked.
    consumed: Vec<Message>,
}

impl Steering {
    /// Hands `input` to the turn. Called from the engine's command path, never
    /// from the turn task.
    pub(crate) fn push(&mut self, input: SteerInput) {
        self.waiting.push(input);
    }

    /// Everything waiting, taken in arrival order and left for the caller to
    /// turn into messages.
    fn take_waiting(&mut self) -> Vec<SteerInput> {
        std::mem::take(&mut self.waiting)
    }
}

/// Every open request the person owes an answer to, by the id a reply names.
///
/// **A registry, where there used to be one cell.** The single slot was correct
/// for exactly as long as a turn could only ever be blocked inside one call:
/// the tool that raised the request had not returned, so nothing else could
/// have raised another. A step that fans several `task` calls out at once
/// retires that premise — two children can be sitting in two dialogs, and a
/// second request must not evict the first's channel (**D462**).
///
/// Two maps rather than one keyed by a discriminated id, which keeps the
/// property the single cell got from its enum: a `ReplyQuestion` naming a
/// permission id finds nothing, rather than finding a permission wait that
/// expected a decision. Nothing here is ever iterated to answer a reply —
/// every route is a lookup by the exact id the wire carried.
///
/// Entries are removed by **their own id**, never by taking whatever is there.
/// That distinction is the whole migration: with one cell "clear the slot" and
/// "clear my request" were the same sentence, and with several they are not.
#[derive(Default)]
pub(crate) struct PendingReplies {
    /// Open permission dialogs, answered by [`Command::ReplyPermission`].
    ///
    /// [`Command::ReplyPermission`]: crate::protocol::Command::ReplyPermission
    permissions: HashMap<PermissionId, oneshot::Sender<PermissionReply>>,
    /// Open questions, answered by [`Command::ReplyQuestion`] or dismissed by
    /// [`Command::RejectQuestion`].
    ///
    /// [`Command::ReplyQuestion`]: crate::protocol::Command::ReplyQuestion
    /// [`Command::RejectQuestion`]: crate::protocol::Command::RejectQuestion
    questions: HashMap<QuestionId, oneshot::Sender<Answered>>,
}

impl PendingReplies {
    /// Registers a permission dialog nobody has answered yet.
    fn open_permission(&mut self, id: PermissionId, sender: oneshot::Sender<PermissionReply>) {
        self.permissions.insert(id, sender);
    }

    /// Registers a question nobody has answered yet.
    fn open_question(&mut self, id: QuestionId, sender: oneshot::Sender<Answered>) {
        self.questions.insert(id, sender);
    }

    /// Forgets one request, by its own id. Called by the wait that opened it,
    /// on every path that leaves without an answer.
    fn close_permission(&mut self, id: &PermissionId) {
        self.permissions.remove(id);
    }

    /// The same for a question.
    fn close_question(&mut self, id: &QuestionId) {
        self.questions.remove(id);
    }

    /// Hands `reply` to the permission wait that named `id`, and reports
    /// whether one was there to take it.
    pub(crate) fn answer_permission(&mut self, id: &PermissionId, reply: PermissionReply) -> bool {
        // A closed receiver means the turn is already tearing down, which is
        // the same race as replying after the turn ended.
        self.permissions
            .remove(id)
            .is_some_and(|sender| sender.send(reply).is_ok())
    }

    /// The same for a question, which one command answers and another
    /// dismisses.
    pub(crate) fn answer_question(&mut self, id: &QuestionId, answered: Answered) -> bool {
        self.questions
            .remove(id)
            .is_some_and(|sender| sender.send(answered).is_ok())
    }

    /// How many requests are open right now — what a status line counts.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.permissions.len() + self.questions.len()
    }
}

/// What answered a question.
///
/// A dismissal is its own value rather than an empty answer list, because
/// upstream draws the same line: `question.rejected` carries its own payload,
/// the waiting call fails instead of completing, and a consumer that read a
/// dismissal as "answered nothing" would have to invent answers nobody gave.
pub(crate) enum Answered {
    /// The person's picks, one list per question, in the order asked.
    Replied(Vec<QuestionAnswer>),
    /// The person dismissed the question.
    Rejected,
}

/// What a persistent engine keeps beside the transcript: the store, and which
/// of its sessions is live. Shared between the engine, the turn task and the
/// detached title task, which is why the live half sits behind its own lock —
/// the one lock every session-info write goes through.
pub(crate) struct SessionState {
    pub(crate) storage: Storage,
    pub(crate) live: std::sync::Mutex<LiveSession>,
}

/// The session under the engine right now, plus bookkeeping that belongs to
/// this run of it rather than to the disk.
#[derive(Default)]
pub(crate) struct LiveSession {
    /// The current session's record; [`None`] until a prompt creates one or a
    /// resume installs one.
    pub(crate) info: Option<SessionInfo>,
    /// Whether this session has already logged that its model is not in the
    /// catalog and therefore will never auto-compact. Once per session, not
    /// once per turn: the warning is advice, not a metronome.
    pub(crate) warned_uncataloged: bool,
}

/// Write-through state for one turn of a persistent engine.
///
/// Every write is synchronous and happens on the turn task deliberately: the
/// files are small, `save_part` replaces whole files so two writes racing out
/// of order would persist stale content, and the turn task is the lane built
/// to absorb backpressure — the render loop sits on the other side of the
/// bounded event channel either way. A failed write is a warning and the turn
/// continues; losing the disk must not kill the conversation.
pub(crate) struct Persist {
    pub(crate) state: Arc<SessionState>,
    /// Session this turn writes into, pinned at start; resume is refused
    /// while a turn is in flight, so it cannot be retargeted mid-turn.
    pub(crate) session: SessionId,
    inner: std::sync::Mutex<PersistInner>,
}

/// The mutable half of [`Persist`], behind a lock only because the turn task
/// reaches it through `&Turn`; nothing else ever holds it.
#[derive(Default)]
struct PersistInner {
    /// Text part with unwritten deltas, and when the oldest of them arrived.
    dirty: Option<(PartId, Instant)>,
    /// Whether the assistant envelope reached the disk at `MessageStarted`;
    /// the finish rewrite only makes sense for a message the disk has met.
    assistant_saved: bool,
    /// Input tokens of the most recent request that reported usage, which is
    /// what [`SessionInfo::context_tokens`] becomes at finish.
    last_input: Option<u64>,
    /// What this turn's summarize request spent, when compaction ran.
    summary_usage: Option<Usage>,
}

impl Persist {
    pub(crate) fn new(state: Arc<SessionState>, session: SessionId) -> Self {
        Self {
            state,
            session,
            inner: std::sync::Mutex::new(PersistInner::default()),
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, PersistInner> {
        self.inner
            .lock()
            .expect("the write-through state is never poisoned")
    }

    /// One warning per failed write, naming the session so a log reader can
    /// tell which conversation is running on memory alone.
    fn complain(&self, what: &str, error: &StorageError) {
        tracing::warn!(
            session = self.session.as_str(),
            what,
            %error,
            "a write-through failed; the conversation continues in memory"
        );
    }

    /// The user's message, envelope and parts, before the provider hears the
    /// prompt: a `kill -9` mid-stream must still preserve what was asked.
    fn user(&self, message: &Message) {
        if let Err(error) = self.state.storage.save_message(&self.session, message) {
            self.complain("the user envelope", &error);
        }
        for part in &message.parts {
            self.write(message, part);
        }
    }

    /// The assistant envelope as the turn opens it. `time.completed` is
    /// absent on disk until the finish rewrite, and that absence is the
    /// crash marker resume reads.
    fn assistant_open(&self, message: &Message) {
        if let Err(error) = self.state.storage.save_message(&self.session, message) {
            self.complain("the assistant envelope", &error);
        }
        self.locked().assistant_saved = true;
    }

    /// One part, immediately. A dirty text part is flushed first so the disk
    /// never sees parts in an order the stream did not produce.
    fn part_now(&self, owner: &Message, part: &Part) {
        let stale = match self.locked().dirty.take() {
            Some((id, _)) if id != part.id => Some(id),
            // Writing the dirty part itself is the flush.
            Some(_) | None => None,
        };
        if let Some(id) = stale {
            self.write_by_id(owner, &id);
        }

        self.write(owner, part);
    }

    /// Notes that a text part grew, writing it through only when the oldest
    /// unwritten delta is [`TEXT_FLUSH`] old.
    fn text_delta(&self, owner: &Message, part_id: &PartId) {
        let flush = {
            let mut inner = self.locked();
            match &inner.dirty {
                None => {
                    inner.dirty = Some((part_id.clone(), Instant::now()));
                    None
                }
                Some((id, since)) if id == part_id => {
                    if since.elapsed() >= TEXT_FLUSH {
                        inner.dirty = None;
                        Some(part_id.clone())
                    } else {
                        None
                    }
                }
                // A different part is dirty — a step boundary already flushes
                // between text parts, but the invariant is cheap to keep
                // locally: flush the old one, start the clock on the new one.
                Some((id, _)) => {
                    let stale = id.clone();
                    inner.dirty = Some((part_id.clone(), Instant::now()));
                    Some(stale)
                }
            }
        };
        if let Some(id) = flush {
            self.write_by_id(owner, &id);
        }
    }

    /// Writes the dirty text part, if any: part close, step end and turn
    /// finish all owe the disk whatever text is still pending.
    fn flush_text(&self, owner: &Message) {
        if let Some((id, _)) = self.locked().dirty.take() {
            self.write_by_id(owner, &id);
        }
    }

    /// When the dirty text part must reach the disk, for the stream loop to
    /// sleep against; [`None`] when nothing is pending.
    fn flush_deadline(&self) -> Option<Instant> {
        self.locked()
            .dirty
            .as_ref()
            .map(|(_, since)| *since + TEXT_FLUSH)
    }

    fn note_input_tokens(&self, tokens: u64) {
        self.locked().last_input = Some(tokens);
    }

    fn note_summary_usage(&self, usage: Option<Usage>) {
        self.locked().summary_usage = usage;
    }

    /// The turn's closing writes: the assistant envelope again — now carrying
    /// its usage and completion stamp — then the session record with the
    /// turn's spend summed in, `context_tokens` moved to what the last
    /// request reported, and the fallback title installed when one applies.
    fn finish(&self, assistant: &Message, fallback_title: Option<String>) {
        self.flush_text(assistant);

        let (assistant_saved, last_input, summary_usage) = {
            let inner = self.locked();
            (inner.assistant_saved, inner.last_input, inner.summary_usage)
        };

        if assistant_saved
            && let Err(error) = self.state.storage.save_message(&self.session, assistant)
        {
            self.complain("the finished assistant envelope", &error);
        }

        let mut live = self
            .state
            .live
            .lock()
            .expect("the live session is never poisoned");
        let Some(info) = live.info.as_mut() else {
            return;
        };
        if info.id != self.session {
            return;
        }

        if let Some(spent) = summary_usage {
            info.usage = add_usage(info.usage, spent);
        }
        if let Some(spent) = assistant.usage {
            info.usage = add_usage(info.usage, spent);
        }
        if let Some(tokens) = last_input {
            info.context_tokens = tokens;
        }
        if info.title.is_none()
            && let Some(title) = fallback_title.filter(|title| !title.is_empty())
        {
            info.title = Some(title);
        }
        info.updated = now();

        if let Err(error) = self.state.storage.save_info(info) {
            self.complain("the session record", &error);
        }
    }

    /// The boundary's durable half of a plan approval: the same row write
    /// `remember_selection` produces, made from what the turn already holds —
    /// `run_turn` has no engine to ask. The model rides along because the
    /// row's two selection fields are written together everywhere else, and
    /// a mid-turn model switch is refused Busy, so the turn's model *is* the
    /// active one.
    fn remember_agent(&self, agent: &str, model: &str) {
        let mut live = self
            .state
            .live
            .lock()
            .expect("the live session is never poisoned");
        let Some(info) = live.info.as_mut() else {
            return;
        };
        if info.id != self.session {
            return;
        }

        info.agent = Some(agent.to_owned());
        info.model = Some(model.to_owned());
        info.updated = now();
        if let Err(error) = self.state.storage.save_info(info) {
            self.complain("the approved agent switch", &error);
        }
    }

    /// The one call that touches `save_part`, so every write shares the
    /// warning path.
    fn write(&self, owner: &Message, part: &Part) {
        if let Err(error) = self.state.storage.save_part(&self.session, &owner.id, part) {
            self.complain("a part", &error);
        }
    }

    fn write_by_id(&self, owner: &Message, part_id: &PartId) {
        match owner.parts.iter().find(|part| part.id == *part_id) {
            Some(part) => self.write(owner, part),
            None => tracing::debug!(
                part = part_id.as_str(),
                "a dirty text part left its message before it was flushed"
            ),
        }
    }
}

/// What a turn is for.
///
/// All three end the same way — one [`Event::MessageFinished`], the busy slot
/// released — because a frontend's idle/busy state is the slot, and a turn that
/// ended without saying so would leave it stuck.
pub(crate) enum TurnKind {
    /// The ordinary one: answer what the user said.
    Prompt {
        /// Files the user attached, which become [`PartBody::File`] parts on
        /// their message and are read when a request is built.
        mentions: Vec<crate::protocol::Mention>,
    },
    /// Run a command the *user* typed and put it and its output in the
    /// transcript, without asking the model anything. Upstream's `!`
    /// passthrough.
    Shell {
        /// What to run, verbatim.
        command: String,
    },
    /// Summarize the conversation and continue from the summary, without
    /// asking the model anything else.
    Compact,
}

/// Everything one turn needs, gathered so the spawned task takes one argument.
///
/// A root turn — what a person's prompt, `!` command or compaction starts —
/// is built as a plain literal by the engine: it is the session's turn, holds
/// the busy slot a frontend reads as idle, carries the read log and the
/// snapshots the whole session shares, and is the only kind that may spawn a
/// subagent at all. All of that is the engine's to vary; the fixed points of
/// a child's turn are [`Turn::child`]'s.
pub(crate) struct Turn {
    pub(crate) provider: Arc<dyn Provider>,
    /// The session every event this turn emits names: the engine's current
    /// one for a root turn, and the child's own stored session for a
    /// subagent's — whose private stream is addressed honestly even though
    /// only the watcher ever reads it.
    pub(crate) session_id: SessionId,
    pub(crate) model: String,
    /// The option map of the catalog effort this turn runs under, resolved
    /// by the engine against [`Turn::model`] before the turn started. Rides
    /// the step and summarize requests — the two that ask this turn's model —
    /// and never the title request, which asks a stablemate the name was
    /// never validated against. Empty means no effort.
    pub(crate) effort_options: serde_json::Map<String, serde_json::Value>,
    /// What the model is told before it is told anything else. Carried by
    /// every request this turn makes except the title one, which asks a
    /// different question and brings its own prompt.
    pub(crate) system: Option<String>,
    /// Synthetic user text this turn's requests carry, appended to the last
    /// user message; see [`stream_step`].
    pub(crate) reminders: Vec<String>,
    /// What this turn is for; see [`TurnKind`].
    pub(crate) kind: TurnKind,
    /// Tools the model is offered, and this loop executes.
    pub(crate) tools: Arc<Registry>,
    /// Rules deciding which calls wait for the user.
    pub(crate) permissions: Arc<std::sync::Mutex<Permissions>>,
    /// Directory tool calls resolve relative paths against.
    pub(crate) cwd: PathBuf,
    /// Where the project starts. A mentioned file is named relative to it, and
    /// a `!` command runs in it.
    pub(crate) root: PathBuf,
    /// Which files this session has read, shared by every call in it.
    pub(crate) files: Arc<FileTimes>,
    /// Where this build keeps its credentials, handed to every call this turn
    /// makes so that `read` and `grep` can refuse the file. [`None`] is a turn
    /// nobody named one to, which every scripted and golden run is.
    pub(crate) credentials: Credentials,
    /// Language servers this session may run. [`None`] is a session whose
    /// config asked for none, and every tool call then completes exactly as it
    /// did before this existed.
    pub(crate) lsp: Option<Arc<crate::lsp::Lsp>>,
    /// What this turn's file changes are recorded against, so `/undo` can put
    /// them back. [`None`] on a turn nobody gave snapshots — every scripted and
    /// golden run — and on every turn a subagent runs: the parent's own patch
    /// covers whatever a child changed, because a patch is a diff of the
    /// working tree rather than a record of who wrote to it (deviation:
    /// subagents-take-no-snapshots-of-their-own).
    pub(crate) snapshots: Option<Arc<crate::snapshot::Snapshots>>,
    pub(crate) prompt: String,
    pub(crate) cancel: CancellationToken,
    /// Where open permission requests and questions wait for their replies; the
    /// same registry the engine's [`TurnHandle`] routes replies into.
    pub(crate) pending: Arc<std::sync::Mutex<PendingReplies>>,
    /// Where a [`Command::Steer`] leaves a message for this turn; the same
    /// cell the engine's [`TurnHandle`] pushes into. A child turn gets one of
    /// its own that nothing can reach — no handle of a child's is ever put in
    /// the slot — so the drain below is a uniform no-op there.
    ///
    /// [`Command::Steer`]: crate::protocol::Command::Steer
    pub(crate) steer: Arc<std::sync::Mutex<Steering>>,
    /// Every subscriber's queue, which this turn publishes into. A root turn
    /// shares the engine's; a child turn gets one seeded with its private
    /// channel, so the send sites below never know the difference.
    pub(crate) events: Arc<Fanout>,
    pub(crate) slot: Arc<Mutex<Option<TurnHandle>>>,
    pub(crate) history: Arc<Mutex<Vec<Message>>>,
    /// What a `task` call needs to run a whole second agent loop. [`None`] on a
    /// turn with no agents to spawn, and on every turn a subagent runs — which
    /// is the depth limit, stated where the loop can see it.
    pub(crate) spawn: Option<Arc<crate::subagent::Host>>,
    /// How many of one step's `task` calls may run at the same time.
    ///
    /// Carried by every turn, including a subagent's, where it is simply never
    /// read: a child's registry has no `task` tool, so it never assembles a
    /// batch to cap.
    pub(crate) concurrency: usize,
    /// The engine's plan-approval cell, when this turn could write or
    /// announce it: a `plan_exit` Yes records `Requested` here through
    /// [`ToolCtx::switch`], and this turn's boundary moves it to `Announced`.
    /// [`None`] on an engine whose registry holds no build agent — nothing to
    /// switch to — and on every turn a subagent runs, with the same
    /// discipline as `spawn: None`: a child's tail must never run phase one
    /// against its private fanout and its own persist.
    pub(crate) pending_switch: Option<Arc<std::sync::Mutex<PendingSwitch>>>,
    /// What a call tracks a background job through — engine-owned and shared
    /// by every turn this session runs, root or subagent alike, because a
    /// job outlives whichever turn started it. [`None`] on every scripted
    /// and golden run that built a [`Turn`] without one.
    pub(crate) jobs: Option<Arc<dyn crate::tool::job::Jobs>>,
    /// What a config asked to be run around this turn's tool calls and at its
    /// end — engine-owned and shared with every subagent it spawns, because a
    /// `PreToolUse` hook that stopped applying inside a delegated turn would be
    /// a gate with a hole in it. [`None`] is a session whose config asked for
    /// none, where every seam below does nothing at all.
    pub(crate) hooks: Option<Arc<crate::hook::Hooks>>,
    /// Whether this turn is a subagent's.
    ///
    /// One question, one field, and it decides exactly one thing: which of the
    /// two stop hooks this turn's end fires. `Stop` is the *session's* — a root
    /// turn finished — and `SubagentStop` belongs to the caller in
    /// [`crate::subagent`], which is the only place that knows which agent ran
    /// and how it ended. Nothing else here varies on it.
    pub(crate) delegated: bool,
    /// Write-through and session bookkeeping, when the engine persists.
    /// [`None`] is an in-memory engine, and every hook below is a no-op.
    pub(crate) persist: Option<Persist>,
}

/// What a subagent's turn varies, which is everything [`Turn::child`] does not
/// fix.
pub(crate) struct ChildParts {
    /// The child's own stored session, which its private events name.
    pub(crate) session_id: SessionId,
    /// The subagent's own model when it named one, and the parent's otherwise.
    pub(crate) model: String,
    pub(crate) system: Option<String>,
    pub(crate) kind: TurnKind,
    pub(crate) prompt: String,
    /// The ruleset the caller derived, which becomes the child's own: a
    /// subagent inherits the refusals and never the allows.
    pub(crate) permissions: Permissions,
    /// The child's **private** channel, not the sender the parent's turn
    /// reports on. Every event on the frontend's stream is understood to
    /// belong to the engine's one current session, so a child that published
    /// there would be a second conversation on the same wire.
    pub(crate) events: mpsc::Sender<Event>,
    /// The transcript a resumed child continues from; empty for a fresh one.
    pub(crate) history: Vec<Message>,
    pub(crate) cancel: CancellationToken,
    pub(crate) persist: Option<Persist>,
}

impl Turn {
    /// The turn a subagent runs, spawned by a `task` call rather than by
    /// anything a person did.
    ///
    /// What a child turn is *not* free to vary is the point of this
    /// constructor: the caller cannot supply the fields below, so the reasons
    /// they hold are stated once, here, instead of at each place a child is
    /// started. Two of them are shared with the parent on purpose — the
    /// pending-reply cell, because the parent is blocked inside this call and
    /// its slot is the only route a dialog's answer can travel (see
    /// [`Spawn::pending`]), and the language servers, because a client is
    /// identified by `(root, server)` and a child in the same project should
    /// reuse what the parent already has warm (see [`Host::lsp`]).
    ///
    /// [`Spawn::pending`]: crate::subagent::Spawn::pending
    /// [`Host::lsp`]: crate::subagent::Host::lsp
    pub(crate) fn child(spawn: &crate::subagent::Spawn, parts: ChildParts) -> Self {
        let host = &spawn.host;

        Self {
            provider: Arc::clone(&host.provider),
            session_id: parts.session_id,
            model: parts.model,
            // No effort: the selection is the session's and was validated
            // against the session's model, while a child may run a model of
            // the subagent's own choosing that the name was never checked
            // against.
            effort_options: serde_json::Map::new(),
            system: parts.system,
            // Upstream's plan/build reminders are about the agent a *person*
            // switched to; a subagent runs the prompt it was built with.
            reminders: Vec::new(),
            kind: parts.kind,
            tools: Arc::clone(&host.tools),
            permissions: Arc::new(std::sync::Mutex::new(parts.permissions)),
            cwd: host.cwd.clone(),
            root: host.root.clone(),
            // A fresh read log: what the parent read is not what the child may
            // write over, and the read-before-write rule is per conversation.
            files: Arc::new(FileTimes::default()),
            // The same store the parent refuses, for the same reason and one
            // more: nobody is watching a subagent's turn.
            credentials: host.credentials.clone(),
            lsp: host.lsp.clone(),
            // No snapshots of its own: a patch is a diff of the working tree
            // rather than a record of who wrote to it, so the step of the
            // *parent* that made this call already covers everything the child
            // changed — and covers it in the session an `/undo` can reach.
            snapshots: None,
            prompt: parts.prompt,
            cancel: parts.cancel,
            pending: Arc::clone(&spawn.pending),
            // A mailbox of its own, and one nothing can post to: a child's
            // turn handle never reaches the engine's slot, so no `Steer` can
            // name it. Sharing the parent's would be worse than useless — it
            // would let a person's correction land in a subagent's private
            // conversation instead of in the one they are watching.
            steer: Arc::default(),
            // A fanout of one, so the loop publishes the same way whoever is
            // listening; the watcher on the other end stays the only reader.
            events: Arc::new(Fanout::new(parts.events)),
            // The child's turn handle is nobody else's: the busy slot a
            // frontend reads belongs to the parent, which is busy running this.
            slot: Arc::new(Mutex::new(None)),
            history: Arc::new(Mutex::new(parts.history)),
            // The child's task tool is absent from its registry, so nothing
            // below it can spawn anything.
            spawn: None,
            // Carried forward and never read, for the reason `spawn: None`
            // states: a child assembles no batch, because it is offered no
            // tool that would make one.
            concurrency: host.concurrency,
            // The approval cell is the parent engine's, and a child never
            // holds it — same discipline as `spawn: None`. A child that did
            // would run phase one at its own tail, announcing on a private
            // fanout nobody subscribes to and persisting through a child
            // session's row; and its rules already deny `plan_exit`, so
            // the belt has braces.
            pending_switch: None,
            // The parent's own registry, shared rather than withheld: a
            // background job outlives whichever turn started it, and the
            // depth guard `spawn: None` states is about delegating *more*
            // work, not about a subagent's own `bash` calls losing a
            // capability its parent has.
            jobs: host.jobs.clone(),
            // The session's, shared for the reason the rules are: a hook that
            // watches every `edit` must watch a subagent's too, or a delegated
            // turn is the way around it. What the child does *not* do is fire
            // the session's `Stop` — see `delegated` just below.
            hooks: host.hooks.clone(),
            delegated: true,
            persist: parts.persist,
        }
    }

    /// Runs whatever this session configured for `payload`'s event, and puts
    /// whatever failed in the log.
    ///
    /// The default outcome on a session with no hooks, which is what lets every
    /// fire site below read as one statement rather than as a branch. Nothing
    /// here can fail: a hook that could end a turn would be a hook that could
    /// take a session down by exiting badly.
    async fn fire_hook(&self, payload: crate::hook::Payload) -> crate::hook::Outcome {
        let Some(hooks) = &self.hooks else {
            return crate::hook::Outcome::default();
        };
        let event = payload.event();
        let outcome = hooks.fire(self.session_id.as_str(), &payload).await;
        outcome.report(event);

        outcome
    }

    /// Writes `part` through immediately, when the engine persists.
    fn persist_part(&self, owner: &Message, part: &Part) {
        if let Some(persist) = &self.persist {
            persist.part_now(owner, part);
        }
    }

    /// Notes a grown text part for the debounced write, when the engine
    /// persists.
    fn persist_text_delta(&self, owner: &Message, part_id: &PartId) {
        if let Some(persist) = &self.persist {
            persist.text_delta(owner, part_id);
        }
    }

    /// Flushes the debounced text write, when the engine persists.
    fn persist_flush(&self, owner: &Message) {
        if let Some(persist) = &self.persist {
            persist.flush_text(owner);
        }
    }

    /// When the debounced text write is due, if one is pending at all.
    fn flush_deadline(&self) -> Option<Instant> {
        self.persist.as_ref().and_then(Persist::flush_deadline)
    }

    /// The mid-turn messages this turn has already taken on, in drain order.
    ///
    /// Cloned rather than borrowed because every caller composes them into a
    /// request or a history it then owns, and holding the lock across either
    /// would hold it across an await.
    fn steered(&self) -> Vec<Message> {
        self.steer
            .lock()
            .expect("the steer mailbox is never poisoned")
            .consumed
            .clone()
    }
}

/// The parts a mention becomes on the message that carried it.
///
/// A reference and nothing more: what the file says is read when a request is
/// built, which is what makes the model see the file as it is *then* rather
/// than as it was when somebody typed the `@`. Shared by the prompt that opens
/// a turn and by every steer the turn takes on afterwards, so the two cannot
/// drift into attaching differently.
fn mention_parts(mentions: &[crate::protocol::Mention]) -> impl Iterator<Item = Part> + '_ {
    mentions.iter().map(|mention| {
        Part::file_range(
            mention.path.clone(),
            crate::attachment::mime(&mention.path),
            mention.start,
            mention.end,
        )
    })
}

/// Takes whatever a [`Command::Steer`] left for this turn and turns each one
/// into a real user message: announced, persisted, and appended to what the
/// next request carries.
///
/// Returns whether anything was drained — which the finish path reads as "do
/// not end the turn yet" — or breaks when the turn is over.
///
/// **A cancelled turn drains nothing.** The check is first and deliberate: a
/// turn that is stopping must not consume a message it will never answer, and
/// leaving it in the mailbox is what lets the frontend's fallback lane own it.
/// Nothing else here interacts with the permission cell, so a steer that
/// arrived while a dialog was open simply drains at the boundary after the
/// dialog resolved, like any other.
///
/// [`Command::Steer`]: crate::protocol::Command::Steer
async fn drain_steers(turn: &Turn) -> ControlFlow<Option<Outcome>, bool> {
    if turn.cancel.is_cancelled() {
        return ControlFlow::Continue(false);
    }

    let waiting = turn
        .steer
        .lock()
        .expect("the steer mailbox is never poisoned")
        .take_waiting();
    if waiting.is_empty() {
        return ControlFlow::Continue(false);
    }

    for input in waiting {
        // The id goes out first: a frontend retires its queue entry in the
        // same breath the message appears, and never before the engine has
        // committed to taking it.
        if let ControlFlow::Break(stop) = deliver(
            turn,
            Event::SteerConsumed {
                session_id: turn.session_id.clone(),
                id: input.id,
            },
        )
        .await
        {
            return ControlFlow::Break(stop);
        }

        let mut user = Message::user(input.text);
        user.parts.extend(mention_parts(&input.mentions));

        // Disk before the provider hears it, exactly as the opening prompt
        // reaches it: a `kill -9` mid-stream must still preserve what was
        // asked, whenever in the turn it was asked.
        if let Some(persist) = &turn.persist {
            persist.user(&user);
        }
        // Recorded before the announcement for the same reason: the request
        // this turn builds next is composed from here, and a frontend that
        // applies the event holds what that request will carry.
        turn.steer
            .lock()
            .expect("the steer mailbox is never poisoned")
            .consumed
            .push(user.clone());

        if let ControlFlow::Break(stop) = deliver(
            turn,
            Event::MessageStarted {
                session_id: turn.session_id.clone(),
                message: user,
            },
        )
        .await
        {
            return ControlFlow::Break(stop);
        }
    }

    ControlFlow::Continue(true)
}

/// Why a turn ended, and what to say about it.
struct Outcome {
    reason: FinishReason,
    error: Option<String>,
}

impl Outcome {
    fn finished(reason: FinishReason) -> Self {
        Self {
            reason,
            error: None,
        }
    }

    fn cancelled() -> Self {
        Self {
            reason: FinishReason::Cancelled,
            error: None,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            reason: FinishReason::Failed,
            error: Some(error),
        }
    }
}

/// Whether `call` delegates — the one call kind a step batches.
fn delegates(call: &BufferedCall) -> bool {
    call.name == crate::tool::task::ID
}

/// A tool call as the provider streamed it, waiting for the step to end.
#[derive(Clone)]
struct BufferedCall {
    /// The provider's id for the call, echoed back beside its result.
    id: String,
    /// Tool the model asked for, by name.
    name: String,
    /// The call's arguments, accumulated fragment by fragment; only valid
    /// JSON once assembled.
    json: String,
    /// The `Pending` part opened for the call when it started.
    part_id: PartId,
}

/// Runs one turn to its finish event.
pub(crate) async fn run_turn(turn: Turn) {
    let (mut assistant, outcome) = match &turn.kind {
        TurnKind::Prompt { .. } => drive(&turn).await,
        TurnKind::Shell { command } => drive_shell(&turn, command.clone()).await,
        TurnKind::Compact => drive_compact(&turn).await,
    };
    let completed = assistant.complete();

    // A turn that died before its first fragment leaves nothing worth sending
    // back as context — and some providers reject an empty assistant message.
    // Step markers alone do not count; text or a tool call does.
    //
    // A compacting turn is the exception: what it produced is the summary, and
    // the summary is already the whole history by the time it returns.
    if !matches!(turn.kind, TurnKind::Compact) && assistant.has_content() {
        turn.history.lock().await.push(assistant.clone());
    }

    // Then the messages a `Steer` added mid-turn, in the order they were
    // taken. After the reply, never before it: their ids sort there, a
    // resumed session reads them back there, and every request this turn built
    // already carried them there. Unconditional on the assistant above — a
    // steer that was consumed is a message the person really sent and the disk
    // really holds, whether or not the reply it interrupted came to anything.
    {
        let steered = turn.steered();
        if !steered.is_empty() {
            turn.history.lock().await.extend(steered);
        }
    }

    let finished_clean = matches!(
        &outcome,
        Some(outcome) if outcome.reason == FinishReason::Completed
    );

    if let Some(persist) = &turn.persist {
        // The fake provider's fallback title is decided inline rather than in
        // the detached title task, because a fake session must not spend a
        // provider request on a title — scripted demos and PTY tests count
        // every request — and an inline write means the title is already on
        // disk when the finish event reaches the frontend.
        let fallback = if finished_clean && fake_title_skips_the_request(&turn) {
            Some(fallback_title(&turn).await)
        } else {
            None
        };
        persist.finish(&assistant, fallback);
    }

    // The finish event is queued BEFORE the slot is released: MessageFinished
    // is defined to be the last event of a turn — save for the one
    // `AgentChanged` a plan approval announces immediately after it — and an
    // engine that goes idle first opens a window where the next turn's
    // opening events overtake the finish on a multi-thread runtime. The
    // pinned order of this tail is **MessageFinished (when the turn produced
    // one) → AgentChanged (when a switch is pending) → slot release**;
    // draining the approval after the release instead would let a raced
    // prompt observe the events out of order. The Busy a frontend can still
    // see at the boundary is bounded by these channel sends, and a refused
    // prompt keeps its text. Everything persisted above happens earlier for
    // the same reason: nothing that could admit a new turn may precede this
    // send.
    if let Some(outcome) = outcome {
        let _ = turn
            .events
            .send(Event::MessageFinished {
                session_id: turn.session_id.clone(),
                message_id: assistant.id,
                reason: outcome.reason,
                usage: assistant.usage,
                error: outcome.error,
                completed,
            })
            .await;
    }

    // Phase one of a plan approval. Positional, not conditional on the finish
    // above — `outcome` is `None` on the fanout-dead bails, and the durable
    // half must land even with nobody left to tell. Cancel converges through
    // this same tail: there is no second site to drift.
    announce_pending_switch(&turn).await;

    // Inside the pinned tail, and deliberately: a `Stop` hook is defined to run
    // at the end of a turn, and releasing the slot first would let the next
    // prompt's turn start while it was still running — so a hook that formats
    // the tree would be racing the edits of the turn after the one it was
    // written for. The cost is that the busy window now includes the hook's own
    // budget, which is the point of the budget. A subagent's turn fires nothing
    // here: `SubagentStop` is its end, and only its caller knows which agent
    // ran (**D461**).
    if !turn.delegated {
        turn.fire_hook(crate::hook::Payload::Stop {
            // Forced continuation is a recorded follow-up, so no turn in this
            // build is ever running because a Stop hook asked for one.
            stop_hook_active: false,
        })
        .await;
    }

    *turn.slot.lock().await = None;

    if finished_clean {
        spawn_title_if_untitled(&turn).await;
    }
}

/// Phase one of a plan approval, at the one boundary every turn ends
/// through: if this turn's Yes is still `Requested`, persist the switch
/// durably, announce it, and hand the in-memory half to the next engine
/// entry by moving the cell to `Announced`.
///
/// The durable write goes first — disk, then the announcement, like every
/// other write-through point — and produces the same row `remember_selection`
/// would: a restart between here and the next prompt resumes as build
/// (deviation: approval-persists-at-the-boundary — upstream's window is
/// zero, ganja's is the turn that just ended). On an engine with no
/// persistence the write degrades to announce-only, correct by construction:
/// there is no row for a restart to read. The event's model is exact because
/// build prefers no model of its own, so the turn's model is the model the
/// session keeps.
async fn announce_pending_switch(turn: &Turn) {
    let Some(cell) = &turn.pending_switch else {
        return;
    };
    {
        let mut pending = cell.lock().expect("the pending switch is never poisoned");
        if *pending != PendingSwitch::Requested {
            return;
        }
        *pending = PendingSwitch::Announced;
    }

    if let Some(persist) = &turn.persist {
        persist.remember_agent(agent::BUILD, &turn.model);
    }
    let _ = turn
        .events
        .send(Event::AgentChanged {
            session_id: turn.session_id.clone(),
            agent: agent::BUILD.to_owned(),
            model: turn.model.clone(),
        })
        .await;
}

/// Whether this turn's provider is the fake one running without
/// [`FAKE_TITLE_ENV`], in which case a title is never requested — the
/// fallback is written instead.
fn fake_title_skips_the_request(turn: &Turn) -> bool {
    turn.provider.id() == crate::provider::fake::ID
        && std::env::var(FAKE_TITLE_ENV).as_deref() != Ok("1")
}

/// The title a session falls back to: the first [`FALLBACK_TITLE_CHARS`]
/// characters of its first prompt, cut on a character boundary by
/// construction.
async fn fallback_title(turn: &Turn) -> String {
    let history = turn.history.lock().await;
    let prompt = history
        .iter()
        .find(|message| message.role == Role::User)
        .and_then(|message| message.parts.iter().find_map(Part::as_text))
        .unwrap_or(turn.prompt.as_str());

    clip_title(prompt)
}

fn clip_title(prompt: &str) -> String {
    prompt.trim().chars().take(FALLBACK_TITLE_CHARS).collect()
}

/// Starts the detached task that titles an untitled session, unless nothing
/// needs doing. Spec: upstream `packages/opencode/src/session/prompt.ts`
/// (`ensureTitle`) — a toolless request to the provider's cheapest catalog
/// model, falling back to the session model, and any failure falls back to
/// the clipped first prompt.
///
/// Detached on purpose: the task never holds the turn slot, so the next
/// prompt is never waiting on bookkeeping. The title is storage-only in P4 —
/// no event announces it; the session picker reads it from disk.
async fn spawn_title_if_untitled(turn: &Turn) {
    let Some(persist) = &turn.persist else {
        return;
    };
    if fake_title_skips_the_request(turn) {
        // The fallback already handled it inline.
        return;
    }

    let untitled = {
        let live = persist
            .state
            .live
            .lock()
            .expect("the live session is never poisoned");
        live.info
            .as_ref()
            .is_some_and(|info| info.id == persist.session && info.title.is_none())
    };
    if !untitled {
        return;
    }

    // Upstream titles from the history up to and including the first real
    // user message, which for the turn that earns a title is that message
    // alone.
    let first_user = {
        let history = turn.history.lock().await;
        history
            .iter()
            .find(|message| message.role == Role::User)
            .cloned()
    };
    let Some(mut first_user) = first_user else {
        return;
    };
    // A title request is not a working request: it wants what the user *asked*,
    // and a mentioned file is part of that as the `@path` they typed — which is
    // exactly the shape upstream's title prompt is written against — rather
    // than as the file's whole contents.
    for part in &mut first_user.parts {
        if let PartBody::File { path, .. } = &part.body {
            part.body = PartBody::Text {
                text: format!("@{path}"),
            };
        }
    }
    let fallback = clip_title(
        first_user
            .parts
            .iter()
            .find_map(Part::as_text)
            .unwrap_or_default(),
    );

    let provider = Arc::clone(&turn.provider);
    let model = turn.model.clone();
    let state = Arc::clone(&persist.state);
    let session = persist.session.clone();

    tokio::spawn(async move {
        let title = match request_title(provider.as_ref(), &model, first_user).await {
            Some(title) => title,
            None => fallback,
        };
        if title.is_empty() {
            return;
        }
        store_title(&state, &session, title);
    });
}

/// Asks the provider's cheapest stablemate for a title, returning [`None`]
/// for any failure — the caller owns the fallback.
async fn request_title(
    provider: &dyn Provider,
    session_model: &str,
    first_user: Message,
) -> Option<String> {
    // Cheapest by fresh-input price, upstream's "small model"; a provider the
    // catalog does not know keeps the session's own model.
    let model = catalog::models()
        .filter(|info| info.provider_id == provider.id())
        .min_by(|a, b| a.pricing.input.total_cmp(&b.pricing.input))
        .map_or_else(|| session_model.to_owned(), |info| info.id.to_owned());

    let request = ChatRequest {
        model,
        system: Some(TITLE_PROMPT.to_owned()),
        messages: vec![Message::user(TITLE_INSTRUCTION), first_user],
        tools: Vec::new(),
        // No effort: this request may ask a cheaper stablemate the selected
        // name was never validated against.
        effort_options: serde_json::Map::new(),
    };

    let mut events = match provider.stream(request, CancellationToken::new()).await {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(%error, "the title request was refused; falling back to the prompt");
            return None;
        }
    };

    let mut text = String::new();
    while let Some(event) = events.next().await {
        match event {
            ProviderEvent::TextDelta(delta) => text.push_str(&delta),
            ProviderEvent::Failed(error) => {
                tracing::warn!(%error, "the title request died; falling back to the prompt");
                return None;
            }
            ProviderEvent::Finish(_) => break,
            // A toolless request has no business calling tools, and thinking
            // is stripped below either way. Sealed thinking has nowhere to go
            // at all: this request is not part of the conversation, so there
            // is no next request of its own to hand it back on.
            ProviderEvent::ReasoningDelta(_)
            | ProviderEvent::ReasoningState { .. }
            | ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallEnd { .. }
            | ProviderEvent::Usage(_) => {}
        }
    }

    clean_title(&text)
}

/// Upstream's title cleaning: `<think>` blocks stripped, the first non-empty
/// line kept, anything past 100 characters clipped to 97 plus an ellipsis.
fn clean_title(text: &str) -> Option<String> {
    let stripped = strip_think(text);
    let line = stripped
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;

    Some(if line.chars().count() > 100 {
        let clipped: String = line.chars().take(97).collect();
        format!("{clipped}...")
    } else {
        line.to_owned()
    })
}

/// Removes `<think>…</think>` blocks and the whitespace trailing them, the
/// way upstream's non-greedy regex does; an unclosed block is kept verbatim,
/// because the regex would not have matched it either.
fn strip_think(text: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";

    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        kept.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = rest[start + end + CLOSE.len()..].trim_start(),
            None => {
                kept.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    kept.push_str(rest);

    kept
}

/// Writes a freshly generated title onto its session, wherever that session
/// now is: still live under the engine, or already back on disk because the
/// user resumed another one mid-request. Every path holds the live lock, so
/// a concurrent finish write cannot be torn.
fn store_title(state: &SessionState, session: &SessionId, title: String) {
    let mut live = state
        .live
        .lock()
        .expect("the live session is never poisoned");

    if let Some(info) = live.info.as_mut()
        && info.id == *session
    {
        if info.title.is_none() {
            info.title = Some(title);
            info.updated = now();
            if let Err(error) = state.storage.save_info(info) {
                tracing::warn!(session = session.as_str(), %error, "could not persist the title");
            }
        }
        return;
    }

    match state.storage.load_info(session) {
        Ok(Some(mut info)) if info.title.is_none() => {
            info.title = Some(title);
            info.updated = now();
            if let Err(error) = state.storage.save_info(&info) {
                tracing::warn!(session = session.as_str(), %error, "could not persist the title");
            }
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(session = session.as_str(), %error, "could not reload the session to title it");
        }
    }
}

/// Runs the step loop and hands back the assistant message it accumulated,
/// together with why the turn ended — or [`None`] once every subscriber is
/// gone and there is nobody left to tell.
///
/// The assistant message is minted here, *after* the user's message, because
/// stored ids double as transcript order: the reply has to sort after the
/// prompt it answers. The early returns before that point mint one anyway —
/// never announced and never persisted, it exists so the finish event has an
/// id to name.
async fn drive(turn: &Turn) -> (Message, Option<Outcome>) {
    // Compaction runs before the user's message so the summary's id sorts
    // before it: a resumed window is "messages from the summary onward", and
    // the prompt that follows a compaction must land inside it.
    if let ControlFlow::Break(stop) = compact_if_needed(turn, false).await {
        return (Message::assistant(turn.model.clone()), stop);
    }

    let mut user = Message::user(turn.prompt.clone());
    // A mention is a reference and nothing more; what the file says is read
    // when a request is built. See [`PartBody::File`].
    if let TurnKind::Prompt { mentions } = &turn.kind {
        user.parts.extend(mention_parts(mentions));
    }
    // The prompt reaches the disk before the provider hears it: a `kill -9`
    // mid-stream must still preserve what was asked.
    if let Some(persist) = &turn.persist {
        persist.user(&user);
    }
    turn.history.lock().await.push(user.clone());

    if turn
        .events
        .send(Event::MessageStarted {
            session_id: turn.session_id.clone(),
            message: user,
        })
        .await
        .is_err()
    {
        return (Message::assistant(turn.model.clone()), None);
    }

    let mut assistant = Message::assistant(turn.model.clone());
    // The envelope is on disk with `completed` absent before the frontend
    // sees the message; that absence is the crash marker resume reads.
    if let Some(persist) = &turn.persist {
        persist.assistant_open(&assistant);
    }
    if turn
        .events
        .send(Event::MessageStarted {
            session_id: turn.session_id.clone(),
            message: assistant.clone(),
        })
        .await
        .is_err()
    {
        return (assistant, None);
    }

    // The provider-reported spend of the steps so far. [`None`] until a step
    // reports one, so a provider that says nothing yields a message that says
    // nothing, rather than a fabricated zero.
    let mut total: Option<Usage> = None;
    // The working tree as this turn found it, taken before the first provider
    // byte and replaced after every step that ran tools. What `/undo` puts the
    // files back to.
    let mut before = track(turn).await;

    loop {
        let step = stream_step(turn, &mut assistant).await;

        if let Some(usage) = step.usage {
            total = Some(add_usage(total.unwrap_or_default(), usage));
            assistant.usage = total;
            // Each request replaces the measure: `context_tokens` is what the
            // most recent request carried, not a running sum.
            if let Some(persist) = &turn.persist {
                persist.note_input_tokens(usage.input_tokens);
            }
        }

        match step.end {
            StepEnd::Interrupted(stop) => {
                // A turn that was cancelled or died having already written
                // files is still a turn worth being able to undo, which is
                // what upstream's own cleanup does with a snapshot the step
                // loop never closed. The cost is the git call, paid once,
                // after the cancel has already stopped everything else.
                record_patch(turn, &mut assistant, before.take()).await;
                return (assistant, stop);
            }
            StepEnd::Finished { reason, calls } => {
                if calls.is_empty() {
                    // A request that ended without calling anything is the
                    // model done talking — unless somebody typed while it was.
                    // The mailbox is checked **before** the turn ends, which
                    // is the whole of "a queued message keeps the turn going":
                    // a steer waiting here continues the loop and the finish
                    // tail is simply not reached. Without this check a steer
                    // that landed during the model's last request would be
                    // announced by nothing and answered by nobody.
                    match drain_steers(turn).await {
                        ControlFlow::Break(stop) => {
                            record_patch(turn, &mut assistant, before.take()).await;
                            return (assistant, stop);
                        }
                        ControlFlow::Continue(false) => {
                            record_patch(turn, &mut assistant, before.take()).await;
                            return (assistant, Some(Outcome::finished(reason)));
                        }
                        // The same bookkeeping a tool step's boundary does, so
                        // the continued turn measures its next step against
                        // the tree as it stands rather than against the one
                        // this step opened on.
                        ControlFlow::Continue(true) => {
                            record_patch(turn, &mut assistant, before.take()).await;
                            before = track(turn).await;
                            continue;
                        }
                    }
                }

                // Calls resolve sequentially in arrival order: a later call
                // is allowed to depend on an earlier one's effect, and
                // interleaved permission dialogs would be unreadable anyway.
                //
                // **With one exception, and it is the whole of D462**: a run of
                // consecutive `task` calls is one batch, run at the same time.
                // Delegation is the one call whose effect nothing in the same
                // step can depend on — a child works in a conversation of its
                // own and hands back a summary — and it is also the one whose
                // sequential cost is measured in whole agent loops rather than
                // in a file read. Everything else, including a `task` call with
                // an ordinary call between it and the next one, keeps the
                // promise above word for word.
                let mut queued: std::collections::VecDeque<BufferedCall> = calls.into();
                while let Some(first) = queued.pop_front() {
                    let mut batch = vec![first];
                    if delegates(&batch[0]) {
                        while queued.front().is_some_and(delegates) {
                            batch.push(queued.pop_front().expect("the front was just seen"));
                        }
                    }

                    let flow = if batch.len() == 1 {
                        resolve(turn, &mut assistant, &batch[0]).await
                    } else {
                        resolve_batch(turn, &mut assistant, &batch).await
                    };

                    if let ControlFlow::Break(stop) = flow {
                        let error = ToolError::Cancelled.to_string();
                        // The interrupted calls themselves first — a no-op for
                        // each one already closed — then everything queued
                        // behind them.
                        for call in &batch {
                            close_unresolved(turn, &mut assistant, call, &error).await;
                        }
                        let mut rest: Vec<BufferedCall> = queued.into();
                        fail_buffered(turn, &mut assistant, &mut rest, &error).await;
                        record_patch(turn, &mut assistant, before.take()).await;
                        return (assistant, stop);
                    }
                }

                // The step is over once its calls have run — which is where
                // this loop differs from upstream's, whose tools run inside
                // the stream and whose patch is therefore computed at the
                // `step-finish` event. Both put the part in the same place:
                // after the step's tool parts, naming the tree the step
                // started from.
                record_patch(turn, &mut assistant, before.take()).await;
                before = track(turn).await;

                // The step boundary: the tool results are in and the request
                // that carries them has not been built yet, which is the one
                // moment a new user message can join this turn without
                // reordering anything the model has already been told.
                if let ControlFlow::Break(stop) = drain_steers(turn).await {
                    return (assistant, stop);
                }
            }
        }
    }
}

/// The working tree as it stands, when this turn takes snapshots.
async fn track(turn: &Turn) -> Option<String> {
    match &turn.snapshots {
        Some(snapshots) => snapshots.track().await,
        None => None,
    }
}

/// Records what changed since `before` as a [`PartBody::Patch`] on the
/// assistant message, when anything did.
///
/// Nothing here can end a turn: a snapshot that failed reports no files, and a
/// step with no files reports no part. The part is delivered as a
/// [`Event::PartStarted`] like every other, so a frontend that has been
/// applying events holds what `/undo` will act on.
async fn record_patch(turn: &Turn, assistant: &mut Message, before: Option<String>) {
    let (Some(snapshots), Some(before)) = (&turn.snapshots, before) else {
        return;
    };

    let patch = snapshots.patch(&before).await;
    if patch.files.is_empty() {
        return;
    }

    let part = Part {
        id: PartId::ascending(),
        body: PartBody::Patch {
            hash: patch.hash,
            files: patch.files,
        },
    };
    assistant.parts.push(part.clone());
    turn.persist_part(assistant, &part);
    // Delivered without minding a refusal: this runs on the paths that are
    // already returning, and a subscriber that has gone away has nothing left
    // to be told.
    let _ = turn
        .events
        .send(Event::PartStarted {
            session_id: turn.session_id.clone(),
            message_id: assistant.id.clone(),
            part,
        })
        .await;
}

/// Runs the `!` passthrough: the user's own command, its output, and both of
/// them in the transcript where the next model request will read them.
///
/// Spec: upstream `packages/opencode/src/session/prompt.ts` (`shellImpl`). Two
/// messages go in — a synthetic user message saying what happened, and an
/// assistant message carrying a `bash` tool part that never came from a model —
/// and the output streams into that part as it arrives. **The exit code is
/// awaited and discarded**, exactly as upstream discards it: what the model
/// needs is what the command printed.
///
/// **No permission is checked, deliberately** (**D13**). Every other route to
/// the shell is the model asking to run something; this one is the person at
/// the terminal running it themselves, and putting a dialog in front of their
/// own keystrokes would be asking them to approve their own intent. The command
/// runs in the project root under ganja's own `sh -c` shape rather than
/// upstream's login shell (**D14**).
async fn drive_shell(turn: &Turn, command: String) -> (Message, Option<Outcome>) {
    let user = Message::user(SHELL_NOTICE);
    if let Some(persist) = &turn.persist {
        persist.user(&user);
    }
    turn.history.lock().await.push(user.clone());
    if turn
        .events
        .send(Event::MessageStarted {
            session_id: turn.session_id.clone(),
            message: user,
        })
        .await
        .is_err()
    {
        return (Message::assistant(turn.model.clone()), None);
    }

    let mut assistant = Message::assistant(turn.model.clone());
    if let Some(persist) = &turn.persist {
        persist.assistant_open(&assistant);
    }
    if turn
        .events
        .send(Event::MessageStarted {
            session_id: turn.session_id.clone(),
            message: assistant.clone(),
        })
        .await
        .is_err()
    {
        return (assistant, None);
    }

    let input = serde_json::json!({ "command": command });
    let started = now();
    let part = Part {
        id: PartId::ascending(),
        body: PartBody::Tool {
            // No model streamed this call, so there is no provider id to echo.
            // The part's own id is the only identifier there is, and it is
            // unique for the same reason a call id would be.
            call_id: String::new(),
            tool: shell::ShellTool::ID.to_owned(),
            state: ToolState::Running {
                input: input.clone(),
                metadata: serde_json::Value::Null,
                started,
            },
        },
    };
    let part_id = part.id.clone();
    assistant.parts.push(part.clone());
    turn.persist_part(&assistant, &part);
    if let ControlFlow::Break(stop) = deliver(
        turn,
        Event::PartStarted {
            session_id: turn.session_id.clone(),
            message_id: assistant.id.clone(),
            part,
        },
    )
    .await
    {
        return (assistant, stop);
    }

    let (progress, mut chunks) = mpsc::unbounded_channel();
    let ctx = ToolCtx {
        // Upstream runs the user's command where the instance is; ganja runs it
        // where the project is, which is what a person typing `!git status`
        // means by "here".
        cwd: turn.root.clone(),
        cancel: turn.cancel.child_token(),
        call_id: part_id.as_str().to_owned(),
        files: Arc::clone(&turn.files),
        credentials: turn.credentials.clone(),
        spawn: None,
        // A `!` passthrough is the person at the terminal running a command,
        // not the model calling a tool. There is no call to ask about and
        // nothing that could ask — and nothing that could approve a plan, so
        // the switch seam stays empty too.
        ask: None,
        switch: None,
        jobs: None,
    };
    let tool = shell::ShellTool::new();
    let running = tool.run_reporting(input.clone(), &ctx, Some(progress));
    tokio::pin!(running);

    let mut seen: Vec<u8> = Vec::new();
    let mut streaming = true;
    let result = loop {
        let chunk = tokio::select! {
            biased;
            result = &mut running => break result,
            // Disabled once the pumps have dropped their senders, so a command
            // that has stopped writing but not yet exited does not spin here.
            chunk = chunks.recv(), if streaming => chunk,
        };
        let Some(chunk) = chunk else {
            streaming = false;
            continue;
        };

        seen.extend_from_slice(&chunk);
        // Whatever else already arrived goes into the same redraw: one event
        // per burst rather than one per pipe read.
        while let Ok(more) = chunks.try_recv() {
            seen.extend_from_slice(&more);
        }

        // Progress only — deliberately not written through. What reaches the
        // disk is the completed call, and rewriting the part's file for every
        // burst would turn a chatty command into a disk benchmark.
        if let Some(part) = set_tool_state(
            &mut assistant,
            &part_id,
            ToolState::Running {
                input: input.clone(),
                metadata: serde_json::json!({ "output": String::from_utf8_lossy(&seen) }),
                started,
            },
        ) && let ControlFlow::Break(stop) = deliver(
            turn,
            Event::PartUpdated {
                session_id: turn.session_id.clone(),
                message_id: assistant.id.clone(),
                part,
            },
        )
        .await
        {
            return (assistant, stop);
        }
    };

    let completed = now();
    let (state, outcome) = match result {
        Ok(output) => (
            ToolState::Completed {
                input,
                // Upstream's completion metadata is the output and nothing
                // else. The exit code was awaited and dropped on the way here.
                metadata: serde_json::json!({ "output": output.output }),
                output: output.output,
                // Upstream leaves this empty; the command is what a transcript
                // row has to show, and it is what ganja's own `bash` tool
                // titles a call with
                // (deviation: passthrough-titles-the-command).
                title: command,
                started,
                completed,
            },
            Some(Outcome::finished(FinishReason::Completed)),
        ),
        Err(error @ ToolError::Cancelled) => (
            ToolState::Error {
                input,
                error: error.to_string(),
                started,
                completed,
            },
            Some(Outcome::cancelled()),
        ),
        // A command that could not be started is information like any other:
        // the transcript says so and the next turn reads it.
        Err(error) => (
            ToolState::Error {
                input,
                error: error.to_string(),
                started,
                completed,
            },
            Some(Outcome::finished(FinishReason::Completed)),
        ),
    };

    if let Some(part) = set_tool_state(&mut assistant, &part_id, state) {
        turn.persist_part(&assistant, &part);
        let _ = turn
            .events
            .send(Event::PartUpdated {
                session_id: turn.session_id.clone(),
                message_id: assistant.id.clone(),
                part,
            })
            .await;
    }

    (assistant, outcome)
}

/// Runs a compaction the user asked for.
///
/// The same path the automatic one takes, with the fill-level question skipped:
/// the user asked, and how full the window was is what they were judging. The
/// summary the compaction announced is what the finish event names, so the one
/// message this turn put on the stream is the one it closes.
async fn drive_compact(turn: &Turn) -> (Message, Option<Outcome>) {
    match compact_if_needed(turn, true).await {
        ControlFlow::Break(stop) => (Message::assistant(turn.model.clone()), stop),
        ControlFlow::Continue(Some(summary)) => {
            (summary, Some(Outcome::finished(FinishReason::Completed)))
        }
        // Nothing to summarize — an in-memory engine, a session with no
        // history, or a model the catalog cannot size. The turn still ends
        // with a finish event, because the busy slot is released by that event
        // reaching the frontend and by nothing else.
        ControlFlow::Continue(None) => (
            Message::assistant(turn.model.clone()),
            Some(Outcome::finished(FinishReason::Completed)),
        ),
    }
}

/// Summarizes the live window into a fresh assistant message when the last
/// request already filled 90% of the model's context window, then resets the
/// window to that summary so the user's turn proceeds inside budget.
///
/// Spec: upstream `packages/core/src/session/compaction.ts` — the
/// conversation is serialized into a single toolless user message built by
/// `buildPrompt`, and an empty or failed summary leaves the session
/// uncompacted rather than dead. The trigger diverges deliberately: upstream
/// core estimates the assembled request, this port compares the stored
/// `context_tokens` against the catalog window at turn start, which is the
/// contract P4 froze (the measure survives resume, and a model the catalog
/// does not know never compacts).
///
/// The summary is announced as one complete [`Event::MessageStarted`] rather
/// than streamed: the frozen protocol closes a message only through the
/// turn's single `MessageFinished`, and a summary left open would render as
/// aborted forever. Breaks only when the turn itself is over — the user
/// cancelled, the provider died, or the subscriber is gone — and in every
/// break path `SessionInfo::summary` is untouched: there is no half-installed
/// window.
async fn compact_if_needed(
    turn: &Turn,
    forced: bool,
) -> ControlFlow<Option<Outcome>, Option<Message>> {
    let Some(persist) = &turn.persist else {
        return ControlFlow::Continue(None);
    };

    let (summary_id, context_window) = {
        let mut live = persist
            .state
            .live
            .lock()
            .expect("the live session is never poisoned");
        let Some(info) = live.info.as_ref() else {
            return ControlFlow::Continue(None);
        };
        // A subagent's turn writes through a `Persist` of its own while the
        // engine stays live on the parent. Without this the child would read
        // the parent's fill level, compact its own window against it, and
        // stamp the parent's record with a summary belonging to a
        // conversation nobody was having.
        if info.id != persist.session {
            return ControlFlow::Continue(None);
        }
        let Some(model) = catalog::model(&turn.model) else {
            if !live.warned_uncataloged {
                live.warned_uncataloged = true;
                tracing::warn!(
                    model = turn.model.as_str(),
                    "not in the catalog, so its context window is unknown; \
                     this session will never auto-compact"
                );
            }
            // A manual compaction still has to know what fits, and only the
            // catalog can say. Nothing to do but say so.
            return ControlFlow::Continue(None);
        };
        // tokens × 10 ≥ window × 9 is "at least 90% full" without leaving
        // the integers; a saturated multiply only ever fails toward
        // compacting sooner. A manual compaction skips the question: the user
        // asked, and how full the window is was their business to judge.
        if !forced
            && info.context_tokens.saturating_mul(10) < model.context_window.saturating_mul(9)
        {
            return ControlFlow::Continue(None);
        }

        (info.summary.clone(), model.context_window)
    };

    // Past every return above, so this fires when a compaction is really about
    // to happen and never for a turn that merely looked at the fill level and
    // walked away. `forced` is the whole vocabulary: the two trigger sites are
    // the automatic one at the top of a turn and the `/compact` a person typed,
    // which is exactly Claude's `auto`/`manual`.
    turn.fire_hook(crate::hook::Payload::PreCompact {
        trigger: if forced {
            crate::hook::Trigger::Manual
        } else {
            crate::hook::Trigger::Auto
        },
    })
    .await;

    let window = turn.history.lock().await.clone();
    let (previous, context) = match (&summary_id, window.first()) {
        // The window already opens with an earlier summary: upstream folds it
        // into the prompt as <previous-summary> and summarizes what follows.
        (Some(id), Some(first)) if first.id == *id && first.role == Role::Assistant => {
            (summary_text(first), &window[1..])
        }
        _ => (None, &window[..]),
    };
    let serialized = serialize_conversation(context);
    if previous.is_none() && serialized.is_empty() {
        // Nothing to summarize — a fresh window under an inherited
        // `context_tokens`. Upstream returns false here too.
        return ControlFlow::Continue(None);
    }

    let prompt = build_summary_prompt(previous.as_deref(), &serialized);
    // Upstream core's fit guard: a summarize prompt the model cannot hold —
    // estimated at four characters per token — is not sent at all. Skipping
    // keeps the turn alive; the alternative is a summarize request that
    // fails or loops on the very overflow it exists to relieve.
    let estimated = u64::try_from(prompt.chars().count()).unwrap_or(u64::MAX) / 4;
    if estimated > context_window.saturating_sub(SUMMARY_OUTPUT_TOKENS) {
        tracing::warn!(
            estimated,
            context_window,
            "the history is too large to summarize; continuing uncompacted"
        );
        return ControlFlow::Continue(None);
    }

    // The same system prompt the conversation was held under. Summarizing
    // without it would judge what mattered by different instructions than the
    // ones that produced the transcript, and the summary is what the rest of
    // the session is built on.
    let request = ChatRequest {
        model: turn.model.clone(),
        system: turn.system.clone(),
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
        // The same model as the steps, so the same effort: a session that
        // thinks harder should not summarize with a different mind.
        effort_options: turn.effort_options.clone(),
    };

    let (text, usage) = summarize(turn, request).await?;
    if text.trim().is_empty() {
        tracing::warn!("the summarize request said nothing; continuing uncompacted");
        return ControlFlow::Continue(None);
    }

    let mut summary = Message::assistant(turn.model.clone());
    summary.parts.push(Part::text(text));
    summary.usage = usage;
    summary.complete();

    // Disk first, then the announcement, like every other write-through
    // point. A cancel between the two leaves a complete assistant message in
    // the transcript that no window pointer names — clutter, not corruption.
    if let Err(error) = persist
        .state
        .storage
        .save_message(&persist.session, &summary)
    {
        persist.complain("the summary envelope", &error);
    }
    for part in &summary.parts {
        persist.part_now(&summary, part);
    }
    persist.note_summary_usage(usage);

    deliver(
        turn,
        Event::MessageStarted {
            session_id: turn.session_id.clone(),
            message: summary.clone(),
        },
    )
    .await?;

    *turn.history.lock().await = vec![summary.clone()];

    {
        let mut live = persist
            .state
            .live
            .lock()
            .expect("the live session is never poisoned");
        if let Some(info) = live.info.as_mut()
            && info.id == persist.session
        {
            info.summary = Some(summary.id.clone());
            info.updated = now();
            if let Err(error) = persist.state.storage.save_info(info) {
                persist.complain("the compacted session record", &error);
            }
        }
    }

    ControlFlow::Continue(Some(summary))
}

/// Runs the summarize request to completion, returning its text and reported
/// usage, or the interruption that ends the turn instead. Cancel during
/// compaction behaves exactly like cancel during a turn.
async fn summarize(
    turn: &Turn,
    request: ChatRequest,
) -> ControlFlow<Option<Outcome>, (String, Option<Usage>)> {
    let mut events = match turn.provider.stream(request, turn.cancel.clone()).await {
        Ok(events) => events,
        Err(error) => return ControlFlow::Break(Some(Outcome::failed(error.to_string()))),
    };

    let mut text = String::new();
    let mut usage: Option<Usage> = None;
    loop {
        // Biased for the same reason the step loop is: a cancel already in
        // hand beats a fragment that happens to be ready.
        let event = tokio::select! {
            biased;
            () = turn.cancel.cancelled() => {
                return ControlFlow::Break(Some(Outcome::cancelled()));
            }
            event = events.next() => event,
        };

        let Some(event) = event else {
            if turn.cancel.is_cancelled() {
                return ControlFlow::Break(Some(Outcome::cancelled()));
            }
            break;
        };

        match event {
            ProviderEvent::TextDelta(delta) => text.push_str(&delta),
            ProviderEvent::Usage(reported) => usage = Some(reported),
            ProviderEvent::Failed(error) => {
                return ControlFlow::Break(Some(Outcome::failed(error.to_string())));
            }
            ProviderEvent::Finish(_) => break,
            // The request offered no tools and thinking has no part; both are
            // dropped the way the step loop drops orphan reasoning. A summary
            // request is a question about the conversation rather than a step
            // of it, so state sealed while answering it belongs to nothing.
            ProviderEvent::ReasoningDelta(_) | ProviderEvent::ReasoningState { .. } => {}
            ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallEnd { .. } => {
                tracing::debug!("the summarize request offered no tools; dropping a call");
            }
        }
    }

    ControlFlow::Continue((text, usage))
}

/// A summary message's own text, upstream `summaryText`: text parts trimmed,
/// blanks dropped, joined by blank lines.
fn summary_text(message: &Message) -> Option<String> {
    let text = message
        .parts
        .iter()
        .filter_map(Part::as_text)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    (!text.is_empty()).then_some(text)
}

/// Serializes the window the way upstream core's `serialize` does, one block
/// per message, blank-line separated, empty renderings dropped.
fn serialize_conversation(messages: &[Message]) -> String {
    messages
        .iter()
        .map(serialize_message)
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// One message as the summarize prompt shows it: `[User]:` lines for the
/// person, `[Assistant]:` / `[Assistant tool call]:` / `[Tool result]:` /
/// `[Tool error]:` lines for the model, step markers silent.
fn serialize_message(message: &Message) -> String {
    let lines: Vec<String> = match message.role {
        Role::User => message
            .parts
            .iter()
            .filter_map(|part| match &part.body {
                PartBody::Text { text } => Some(format!("[User]: {text}")),
                // What the user attached, named rather than pasted: the
                // summary is about what the conversation was for, and a file
                // the model can read again is not what it needs to remember.
                PartBody::File { path, .. } => Some(format!("[User]: @{path}")),
                PartBody::Tool { .. }
                | PartBody::StepStart
                | PartBody::StepFinish { .. }
                | PartBody::Patch { .. }
                | PartBody::Reasoning { .. } => None,
            })
            .collect(),
        Role::Assistant => message
            .parts
            .iter()
            .flat_map(|part| match &part.body {
                PartBody::Text { text } => vec![format!("[Assistant]: {text}")],
                PartBody::Tool { tool, state, .. } => {
                    let (input, outcome) = match state {
                        ToolState::Pending => (serde_json::json!({}).to_string(), None),
                        ToolState::Running { input, .. } => (input.to_string(), None),
                        ToolState::Completed { input, output, .. } => (
                            input.to_string(),
                            Some(format!("[Tool result]: {}", truncate_output(output))),
                        ),
                        ToolState::Error { input, error, .. } => {
                            (input.to_string(), Some(format!("[Tool error]: {error}")))
                        }
                    };
                    let call = format!("[Assistant tool call]: {tool}({input})");
                    match outcome {
                        Some(outcome) => vec![call, outcome],
                        None => vec![call],
                    }
                }
                // Sealed thinking says nothing a summary could carry: it is
                // bytes for the provider, and the conversation it summarizes
                // is what the model said and did.
                PartBody::File { .. }
                | PartBody::StepStart
                | PartBody::StepFinish { .. }
                | PartBody::Patch { .. }
                | PartBody::Reasoning { .. } => Vec::new(),
            })
            .collect(),
    };

    lines.join("\n")
}

/// Upstream core's `truncate`: a tool output past the cap keeps its head and
/// says so.
fn truncate_output(output: &str) -> String {
    if output.chars().count() <= TOOL_OUTPUT_MAX_CHARS {
        return output.to_owned();
    }
    let kept: String = output.chars().take(TOOL_OUTPUT_MAX_CHARS).collect();

    format!("{kept}\n[truncated]")
}

/// Upstream core's `buildPrompt`: the instruction — updating when a previous
/// summary exists, creating otherwise — then the template, then the
/// serialized history, blank-line separated with empties dropped.
fn build_summary_prompt(previous: Option<&str>, context: &str) -> String {
    let opening = match previous {
        Some(previous) => format!(
            "Update the anchored summary below using the conversation history above.\n\
             Preserve still-true details, remove stale details, and merge in the new facts.\n\
             <previous-summary>\n{previous}\n</previous-summary>"
        ),
        None => "Create a new anchored summary from the conversation history.".to_owned(),
    };

    [opening.as_str(), SUMMARY_TEMPLATE, context]
        .iter()
        .filter(|block| !block.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// How one model request ended.
enum StepEnd {
    /// The stream ran out, and these calls now want to run.
    Finished {
        reason: FinishReason,
        calls: Vec<BufferedCall>,
    },
    /// The turn is over — cancelled, failed, or abandoned — and any buffered
    /// call has already been closed.
    Interrupted(Option<Outcome>),
}

/// What one model request produced.
struct Step {
    end: StepEnd,
    /// What the request spent, when the provider said.
    usage: Option<Usage>,
}

/// Runs one model request: step marker, stream, step-finish marker.
async fn stream_step(turn: &Turn, assistant: &mut Message) -> Step {
    // Every request opens with a step marker, upstream's `step-start` part,
    // so a transcript shows where one request ended and the next began.
    let marker = Part {
        id: PartId::ascending(),
        body: PartBody::StepStart,
    };
    assistant.parts.push(marker.clone());
    turn.persist_part(assistant, &marker);
    if let ControlFlow::Break(stop) = deliver(
        turn,
        Event::PartStarted {
            session_id: turn.session_id.clone(),
            message_id: assistant.id.clone(),
            part: marker,
        },
    )
    .await
    {
        return Step {
            end: StepEnd::Interrupted(stop),
            usage: None,
        };
    }

    let request = {
        let history = turn.history.lock().await;
        let mut messages = history.clone();
        // Later steps carry the reply so far — its text, its tool calls and
        // their results — which is how the model reads what its calls
        // returned. The first step has nothing to add.
        if assistant.has_content() {
            messages.push(assistant.clone());
        }
        // And after it, whatever a `Steer` added while it was being written.
        // They sort **after** the reply they interrupted because that is the
        // order their stored ids give them — the assistant's id was minted
        // when the turn opened — so what this request carries is exactly what
        // a resumed session would replay.
        messages.extend(turn.steered());

        // Upstream's `session/reminders.ts` appends these to the last user
        // message, and — on the path this port takes — never writes them
        // through, so they belong to the REQUEST and not to the transcript.
        // That is deliberate on both sides: the notice is about the state the
        // session is in right now, and a stored copy would still be telling a
        // later turn about a mode it left. This clone is the request's alone,
        // so nothing here can reach the history it was copied from.
        if !turn.reminders.is_empty()
            && let Some(user) = messages
                .iter_mut()
                .rev()
                .find(|message| message.role == Role::User)
        {
            user.parts
                .extend(turn.reminders.iter().cloned().map(Part::text));
        }

        // The one place a mention becomes content. Doing it here rather than
        // when the user attached the file is what makes the model read the
        // file as it is *now*: a mention is a reference, and a reference
        // resolved at attach time would go stale the moment the user saved.
        resolve_mentions(&mut messages, &turn.root, &|mime| {
            turn.provider.accepts_attachment(mime)
        });

        ChatRequest {
            model: turn.model.clone(),
            system: turn.system.clone(),
            messages,
            tools: turn.tools.definitions(),
            effort_options: turn.effort_options.clone(),
        }
    };

    let mut events = match turn.provider.stream(request, turn.cancel.clone()).await {
        Ok(events) => events,
        Err(error) => {
            return Step {
                end: StepEnd::Interrupted(Some(Outcome::failed(error.to_string()))),
                usage: None,
            };
        }
    };

    // The text part this step's fragments accumulate into, once one is open.
    // Steps do not share one: text after a tool round is a new thought, and
    // upstream gives it a new part.
    let mut open: Option<PartId> = None;
    // What this one request cost. The last report wins, which is also how the
    // engine treated usage before the loop existed: providers accumulate
    // internally and report complete counts.
    let mut usage: Option<Usage> = None;
    let mut calls: Vec<BufferedCall> = Vec::new();

    /// Closes the buffered calls and hands back the interruption, so every
    /// early exit below stays one expression.
    macro_rules! interrupt {
        ($stop:expr, $error:expr) => {{
            fail_buffered(turn, assistant, &mut calls, $error).await;
            return Step {
                end: StepEnd::Interrupted($stop),
                usage,
            };
        }};
    }

    let reason = loop {
        // Recomputed each pass: a flush clears it, a first delta sets it.
        let flush_due = turn.flush_deadline();

        // Biased so that a cancel already in hand always wins the race
        // against a fragment that happens to be ready, which is what bounds
        // how long a cancelled turn can keep streaming. The flush arm sits
        // last: with fragments arriving it never fires, and the delta path
        // enforces the same deadline inline — this arm exists for a stream
        // that goes quiet with text still unwritten.
        let event = tokio::select! {
            biased;
            () = turn.cancel.cancelled() => {
                interrupt!(Some(Outcome::cancelled()), &ToolError::Cancelled.to_string());
            }
            event = events.next() => event,
            () = flush_after(flush_due), if flush_due.is_some() => {
                turn.persist_flush(assistant);
                continue;
            }
        };

        let Some(event) = event else {
            // A stream that ends after a cancel ended because of it; one that
            // ends without a finish has said all it is going to.
            if turn.cancel.is_cancelled() {
                interrupt!(
                    Some(Outcome::cancelled()),
                    &ToolError::Cancelled.to_string()
                );
            }
            break FinishReason::Completed;
        };

        match event {
            ProviderEvent::TextDelta(delta) => {
                let part_id = match &open {
                    Some(part_id) => part_id.clone(),
                    None => {
                        let part = Part::text(String::new());
                        let part_id = part.id.clone();
                        assistant.parts.push(part.clone());
                        open = Some(part_id.clone());
                        turn.persist_part(assistant, &part);

                        if let ControlFlow::Break(stop) = deliver(
                            turn,
                            Event::PartStarted {
                                session_id: turn.session_id.clone(),
                                message_id: assistant.id.clone(),
                                part,
                            },
                        )
                        .await
                        {
                            interrupt!(stop, &ToolError::Cancelled.to_string());
                        }

                        part_id
                    }
                };

                // Addressed by id, not by position: a tool part opened
                // mid-step means the text part is no longer the newest one.
                if let Some(text) = assistant
                    .parts
                    .iter_mut()
                    .find(|part| part.id == part_id)
                    .and_then(Part::as_text_mut)
                {
                    text.push_str(&delta);
                }
                turn.persist_text_delta(assistant, &part_id);

                if let ControlFlow::Break(stop) = deliver(
                    turn,
                    Event::PartDelta {
                        session_id: turn.session_id.clone(),
                        message_id: assistant.id.clone(),
                        part_id,
                        delta,
                    },
                )
                .await
                {
                    interrupt!(stop, &ToolError::Cancelled.to_string());
                }
            }
            ProviderEvent::ToolCallStart { id, name } => {
                if calls.iter().any(|call| call.id == id) {
                    tracing::debug!(id, "the provider started the same call twice");
                    continue;
                }

                let part = Part::tool(id.clone(), name.clone());
                calls.push(BufferedCall {
                    id,
                    name,
                    json: String::new(),
                    part_id: part.id.clone(),
                });
                assistant.parts.push(part.clone());
                turn.persist_part(assistant, &part);

                if let ControlFlow::Break(stop) = deliver(
                    turn,
                    Event::PartStarted {
                        session_id: turn.session_id.clone(),
                        message_id: assistant.id.clone(),
                        part,
                    },
                )
                .await
                {
                    interrupt!(stop, &ToolError::Cancelled.to_string());
                }
            }
            // Argument fragments accumulate silently: the part stays
            // `Pending` until the call resolves, and frontends read the
            // parsed input off the `Running` state rather than assembling raw
            // JSON themselves. An orphan fragment is dropped the way upstream
            // drops orphan reasoning deltas.
            ProviderEvent::ToolCallDelta { id, json } => {
                match calls.iter_mut().find(|call| call.id == id) {
                    Some(call) => call.json.push_str(&json),
                    None => tracing::debug!(id, "argument fragment for an unknown call"),
                }
            }
            // Arrival order already decides execution order, and the
            // arguments are only read once the stream ends, so the completion
            // marker carries nothing the loop needs.
            ProviderEvent::ToolCallEnd { .. } => {}
            ProviderEvent::Usage(reported) => usage = Some(reported),
            // A provider that died mid-stream keeps whatever it already
            // streamed — the transcript is honest about how far it got — but
            // the turn is reported as failed, and any call the model asked
            // for before dying is closed unrun: the next request needs every
            // opened call to carry a result.
            ProviderEvent::Failed(error) => {
                interrupt!(Some(Outcome::failed(error.to_string())), STRANDED);
            }
            ProviderEvent::Finish(reason) => break reason,
            // Reasoning a person could read has no protocol part yet. Dropping
            // it keeps the transcript honest instead of pasting thinking into
            // the reply.
            ProviderEvent::ReasoningDelta(_) => {
                tracing::debug!("reasoning has no rendered part yet");
            }
            // Reasoning the *provider* will read does have one, and it is
            // written through like any other part for one reason: the next
            // request is built from the transcript, so state that only lived
            // in this loop would be state the turn's second step no longer
            // has. It reaches the event stream for the same reason — a
            // frontend that applies every event holds exactly what the next
            // request will carry, and this is now part of that.
            ProviderEvent::ReasoningState { item, encrypted } => {
                let part = Part::reasoning(turn.provider.id(), item, Some(encrypted));
                assistant.parts.push(part.clone());
                turn.persist_part(assistant, &part);

                if let ControlFlow::Break(stop) = deliver(
                    turn,
                    Event::PartStarted {
                        session_id: turn.session_id.clone(),
                        message_id: assistant.id.clone(),
                        part,
                    },
                )
                .await
                {
                    interrupt!(stop, &ToolError::Cancelled.to_string());
                }
            }
        }
    };

    // The request is over: mark what it spent, upstream's `step-finish` part.
    // The marker is born complete, so this is the one append whose
    // `PartStarted` already carries content. Writing it also flushes the
    // step's text part, which is the step-end flush the debounce promises.
    let marker = Part {
        id: PartId::ascending(),
        body: PartBody::StepFinish {
            usage: usage.unwrap_or_default(),
        },
    };
    assistant.parts.push(marker.clone());
    turn.persist_part(assistant, &marker);
    if let ControlFlow::Break(stop) = deliver(
        turn,
        Event::PartStarted {
            session_id: turn.session_id.clone(),
            message_id: assistant.id.clone(),
            part: marker,
        },
    )
    .await
    {
        interrupt!(stop, &ToolError::Cancelled.to_string());
    }

    Step {
        end: StepEnd::Finished { reason, calls },
        usage,
    }
}

/// Turns every [`PartBody::File`] in `messages` into what the selected wire
/// can carry, by reading the file it names **now**.
///
/// Three outcomes, decided by the part's mime and by `carries` — the selected
/// provider's [`accepts_attachment`](crate::provider::Provider::accepts_attachment)
/// answer:
///
/// - a text mime (and SVG, upstream's one image that reads as text) becomes
///   the tagged text block it always did, sliced to the mention's line range
///   when one was named;
/// - a binary mime the wire carries stays a file part, its `content` filled
///   with the base64 the wire's encoder will spend — the read happens here so
///   `ganja-protocol` never needs a base64 dependency;
/// - a binary mime the wire does **not** carry degrades to a text block naming
///   the file and its kind. Never a dropped part and never a failed turn: the
///   model learns what was attached even when the wire cannot show it
///   (graceful degradation, the LSP precedent). The frontend warned at submit
///   time via `Engine::accepts_attachment`, so the status line already said
///   why.
///
/// Called on the request's own copy of the history, never on the history
/// itself: the transcript keeps the reference, so the next request reads the
/// file again rather than replaying whatever it said the first time. That is
/// upstream's shape — its file part carries a URL the server resolves per
/// request — and it is what the `@`-mention promise actually is.
///
/// **A mention is not a read.** Nothing here touches
/// [`FileTimes`](crate::tool::FileTimes), so a file the user attached is still
/// a file the model has not opened, and `edit` still refuses it (R9).
///
/// The read is synchronous on the turn task, like every
/// [`Persist`](Persist) write: these are small files, and the lane that absorbs
/// backpressure is this one.
fn resolve_mentions(
    messages: &mut [Message],
    root: &std::path::Path,
    carries: &dyn Fn(&str) -> bool,
) {
    for message in messages {
        for part in &mut message.parts {
            let PartBody::File {
                path,
                mime,
                start,
                end,
                ..
            } = &part.body
            else {
                continue;
            };
            let (path, mime, start, end) = (path.clone(), mime.clone(), *start, *end);

            part.body = if !crate::attachment::is_binary(&mime) {
                PartBody::Text {
                    text: attached(root, &path, start, end),
                }
            } else if carries(&mime) {
                match std::fs::read(root.join(&path)) {
                    Ok(bytes) => {
                        use base64::{Engine as _, engine::general_purpose::STANDARD};
                        PartBody::File {
                            path,
                            mime,
                            start,
                            end,
                            content: Some(STANDARD.encode(bytes)),
                        }
                    }
                    // The same failure block a text mention earns: the user
                    // attached it deliberately, and a silently missing
                    // attachment reads as a user who never mentioned anything.
                    Err(error) => PartBody::Text {
                        text: format!(
                            "<attached-file path=\"{path}\">\n(could not be read: {error})\n</attached-file>"
                        ),
                    },
                }
            } else {
                PartBody::Text {
                    text: format!(
                        "<attached-file path=\"{path}\" mime=\"{mime}\">\n(attached by name only: \
                         this provider's wire does not carry {mime} content)\n</attached-file>"
                    ),
                }
            };
        }
    }
}

/// One mentioned file as the model reads it: a tag naming where it came from
/// — and, for an `@path#12-40` mention, which lines — then whatever it says.
///
/// A path that cannot be read becomes a block saying so rather than nothing:
/// the user attached it deliberately, and a silently missing attachment reads
/// to the model as a user who never mentioned anything.
fn attached(root: &std::path::Path, path: &str, start: Option<u32>, end: Option<u32>) -> String {
    let resolved = root.join(path);
    let body = if resolved.is_dir() {
        // Upstream attaches a directory as a part of its own; this build
        // resolves files only (deviation: mentions-resolve-files-only), and
        // says so where the model can act on it.
        "(this is a directory; name a file inside it, or use glob and grep)".to_owned()
    } else {
        match std::fs::read_to_string(&resolved) {
            Ok(text) => {
                let text = match start {
                    Some(start) => sliced(&text, start, end),
                    None => text,
                };
                crate::tool::truncate::clamp(&text).text
            }
            Err(error) => format!("(could not be read: {error})"),
        }
    };

    // Upstream hands the provider a file part and lets it decide; ganja has no
    // such part, so the file arrives as text that says where it came from
    // (deviation: mention-renders-as-a-tagged-block). The range rides the tag
    // so two slices of one file stay distinguishable to the model.
    match (start, end) {
        (Some(start), Some(end)) => {
            format!(
                "<attached-file path=\"{path}\" lines=\"{start}-{end}\">\n{body}\n</attached-file>"
            )
        }
        (Some(start), None) => {
            format!("<attached-file path=\"{path}\" lines=\"{start}-\">\n{body}\n</attached-file>")
        }
        (None, _) => format!("<attached-file path=\"{path}\">\n{body}\n</attached-file>"),
    }
}

/// Lines `start` through `end` of `text`, 1-indexed and inclusive; to the end
/// of the file when `end` is absent.
///
/// An `end` at or before `start` is treated as absent, which is upstream's
/// keep-the-end-only-when-`start < end` rule applied at the read as well as at
/// the scan — the scan already normalizes what a person types, so this arm
/// only matters for a range a wire client sent by hand.
fn sliced(text: &str, start: u32, end: Option<u32>) -> String {
    let lines = text.lines().skip(start.saturating_sub(1) as usize);

    match end.filter(|end| *end > start) {
        Some(end) => lines
            .take((end - start) as usize + 1)
            .collect::<Vec<_>>()
            .join("\n"),
        None => lines.collect::<Vec<_>>().join("\n"),
    }
}

/// A call that has been parsed, admitted by its hooks and let through the
/// permission gate, and has not run yet.
///
/// Owns its call rather than borrowing it, because a batched one is handed to
/// a task of its own and a borrow of the step's buffer could not travel.
struct Prepared {
    call: BufferedCall,
    tool: Arc<dyn Tool>,
    args: serde_json::Value,
    /// What this call's `PreToolUse` hook asked to add to the result the model
    /// reads, kept until [`finish`] appends it. **Per call, not per turn**:
    /// several calls can be in flight, and each one's hook conversation is
    /// its own.
    hook: crate::hook::Outcome,
}

/// A prepared call whose part already says `Running`, with everything its body
/// needs and nothing of the turn.
///
/// The split point of the whole executor: everything above this is the turn's
/// own thread holding `&mut Message`, and everything in [`body`] is work a
/// batch may put on a task of its own.
struct Started {
    prepared: Prepared,
    /// When the part started running, which its terminal state repeats.
    at: u64,
    ctx: ToolCtx,
    /// The turn's token, cloned: what ends the wait for a result, and what the
    /// grace below is measured against.
    cancel: CancellationToken,
}

/// A call that has run, and whatever it produced.
struct Finished {
    prepared: Prepared,
    at: u64,
    result: Result<ToolOutput, ToolError>,
}

/// Resolves one buffered call: parse, gate, run, and put the result — or the
/// reason there is none — where the model reads it next.
///
/// Breaks only when the turn itself is over: the user cancelled, or the
/// subscriber is gone. Everything else, including a refusal and a tool that
/// failed, continues the loop.
///
/// Four phases, and only one of them is separable: [`prepare`], [`start`] and
/// [`finish`] hold the assistant message and run on the turn's own thread,
/// while [`body`] holds nothing of the turn at all. A single call runs all four
/// end to end here; [`resolve_batch`] runs the same four for several calls,
/// which is what makes "a batched call behaves like an unbatched one" a
/// property of the code rather than of anyone's discipline.
async fn resolve(
    turn: &Turn,
    assistant: &mut Message,
    call: &BufferedCall,
) -> ControlFlow<Option<Outcome>> {
    let Some(prepared) = prepare(turn, assistant, call).await? else {
        // The call was refused, unknown or unparseable, and `prepare` already
        // put the reason where the model reads it.
        return ControlFlow::Continue(());
    };
    let started = start(turn, assistant, prepared).await?;

    finish(turn, assistant, body(started).await).await
}

/// Runs a step's consecutive `task` calls at the same time, applying each
/// child's result the moment it comes home (**D462**).
///
/// Three orders live here and only two of them are the same:
///
/// - **Call order** decides who is prepared, who is asked about, and who
///   starts — so a dialog the batch's own calls raise is still one dialog at a
///   time, in the order the model made them, and the parts were opened in that
///   order while the step streamed.
/// - **Completion order** decides who is applied. A child that finishes first
///   is written into the transcript first, whichever call it was.
/// - **Part order** is call order and stays that way: each result rewrites the
///   part its own call opened, in place. That is what makes a resumed session
///   read back the turn the model itself would have built.
///
/// The cap is [`Turn::concurrency`]: at most that many bodies are ever polled,
/// and the rest wait their turn to *start* rather than being started and
/// throttled — a call that has not begun has an honest `Pending` part, where
/// one marked `Running` for a minute before it ran would not.
async fn resolve_batch(
    turn: &Turn,
    assistant: &mut Message,
    calls: &[BufferedCall],
) -> ControlFlow<Option<Outcome>> {
    let mut waiting: std::collections::VecDeque<Prepared> =
        std::collections::VecDeque::with_capacity(calls.len());
    // The first reason the turn ended, kept rather than returned on the spot:
    // whatever is already running has to be drained before this function may
    // hand the assistant message back.
    let mut stop: Option<Option<Outcome>> = None;

    for call in calls {
        match prepare(turn, assistant, call).await {
            ControlFlow::Continue(Some(prepared)) => waiting.push_back(prepared),
            // Refused, unknown or unparseable: already closed, and the batch
            // carries on without it exactly as a sequential step would.
            ControlFlow::Continue(None) => {}
            ControlFlow::Break(reason) => {
                stop = Some(reason);
                break;
            }
        }
    }

    let mut running = tokio::task::JoinSet::new();
    loop {
        while stop.is_none()
            && running.len() < turn.concurrency.max(1)
            && let Some(prepared) = waiting.pop_front()
        {
            match start(turn, assistant, prepared).await {
                ControlFlow::Continue(started) => {
                    running.spawn(body(started));
                }
                ControlFlow::Break(reason) => stop = Some(reason),
            }
        }

        let Some(joined) = running.join_next().await else {
            break;
        };
        match joined {
            // Applied even when the turn is already over: the tool really did
            // run, and what it produced belongs in the transcript whether or
            // not anybody is still listening. `finish` refuses to publish on a
            // cancelled turn by itself.
            Ok(finished) => {
                if let ControlFlow::Break(reason) = finish(turn, assistant, finished).await {
                    stop = stop.or(Some(reason));
                }
            }
            // A tool that panicked kills the turn task, which is exactly what
            // it did before any of this ran on a task of its own. Reporting it
            // as a failed call instead would be a nicer build and a different
            // one, and this wave is not where that gets decided.
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            // Only `abort` produces this and nothing here aborts; if it ever
            // happens the batch is over, and the loop's caller closes every
            // call it left unresolved.
            Err(error) => {
                tracing::error!(%error, "a batched tool call ended without a result");
                stop = stop.or(Some(Some(Outcome::cancelled())));
            }
        }
    }

    match stop {
        Some(reason) => ControlFlow::Break(reason),
        None => ControlFlow::Continue(()),
    }
}

/// Parses, hooks and gates one call, and reports what is left to run.
///
/// [`None`] is a call that is already over — unparseable, naming a tool that
/// is not there, refused by a rule, blocked by a hook, or turned down at the
/// dialog — with the reason already where the model reads it.
async fn prepare(
    turn: &Turn,
    assistant: &mut Message,
    call: &BufferedCall,
) -> ControlFlow<Option<Outcome>, Option<Prepared>> {
    let args = match parse_args(&call.json) {
        Ok(args) => args,
        Err(error) => {
            let message = invalid_call(&error);
            return closed(fail_call(turn, assistant, call, serde_json::json!({}), &message).await);
        }
    };

    let Some(tool) = turn.tools.get(&call.name) else {
        // Upstream reroutes an unavailable tool through `invalid.ts` carrying
        // the AI SDK's `NoSuchToolError` message, and this is that message.
        let names: Vec<String> = turn
            .tools
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        let available = if names.is_empty() {
            "No tools are available.".to_owned()
        } else {
            format!("Available tools: {}.", names.join(", "))
        };
        let message = invalid_call(&format!(
            "Model tried to call unavailable tool '{}'. {available}",
            call.name
        ));

        return closed(fail_call(turn, assistant, call, args, &message).await);
    };

    // Before the gate, because a `PreToolUse` hook is defined to run before the
    // call and its answer can change what the gate does: a refusal ends the
    // call here, and an "allow" skips the dialog an ask-gated call would
    // otherwise raise (**D458** — it skips the *ask*, and never overturns a
    // `deny` rule the same person wrote in the same file). Whatever context it
    // asked to add rides with the call to its result below, which is where the
    // model reads it.
    let hook = turn
        .fire_hook(crate::hook::Payload::PreToolUse {
            tool_name: call.name.clone(),
            tool_input: args.clone(),
        })
        .await;
    if let Some(reason) = &hook.blocked {
        return closed(fail_call(turn, assistant, call, args, &blocked_by_hook(reason)).await);
    }

    // The one consultation this call gets. What a refusal quotes, what the
    // dialog discloses and what an "always" writes down are all read off the
    // same call in the same moment — three separate derivations, the last of
    // them after a person has answered, are three chances to disagree about
    // which call was being decided.
    let decision = turn
        .permissions
        .lock()
        .expect("the permission rules are never poisoned")
        .gate(&call.name, &args);

    match decision.action {
        Decision::Allow => {}
        // A rule already answered this one, so there is nothing to put in
        // front of anybody. Like a refusal, it is information: the model reads
        // why, and the turn carries on.
        Decision::Deny => {
            let message = denied(&decision.rules);

            return closed(fail_call(turn, assistant, call, args, &message).await);
        }
        // A hook already approved it, so nobody is asked. The rules are not
        // rewritten and nothing is remembered: this is one call's answer, given
        // by the user's own code, for this call alone.
        Decision::Ask if hook.allowed => {}
        Decision::Ask => {
            match wait_permission(
                turn,
                call,
                tool.describe(&args),
                &args,
                &decision.directories,
            )
            .await?
            {
                PermissionReply::Once => {}
                PermissionReply::Always => turn
                    .permissions
                    .lock()
                    .expect("the permission rules are never poisoned")
                    .remember(&decision),
                // A refusal is information, not a turn abort: the model reads
                // it as the call's result and decides what to do next.
                PermissionReply::Reject => {
                    return closed(fail_call(turn, assistant, call, args, REJECTED).await);
                }
            }
        }
    }

    ControlFlow::Continue(Some(Prepared {
        call: call.clone(),
        tool: Arc::clone(tool),
        args,
        hook,
    }))
}

/// A call that ended inside [`prepare`], as that phase's own answer.
fn closed(flow: ControlFlow<Option<Outcome>>) -> ControlFlow<Option<Outcome>, Option<Prepared>> {
    match flow {
        ControlFlow::Continue(()) => ControlFlow::Continue(None),
        ControlFlow::Break(stop) => ControlFlow::Break(stop),
    }
}

/// Moves a prepared call's part to `Running`, announces it, and builds the
/// context its body runs under.
///
/// The last phase that touches the assistant message before the tool runs, and
/// the reason the batch starts its calls one at a time on this thread rather
/// than all at once: `Running` is announced when the work actually begins, so a
/// call waiting for a slot under the cap still reads as `Pending`.
async fn start(
    turn: &Turn,
    assistant: &mut Message,
    prepared: Prepared,
) -> ControlFlow<Option<Outcome>, Started> {
    let at = now();
    if let Some(part) = set_tool_state(
        assistant,
        &prepared.call.part_id,
        ToolState::Running {
            input: prepared.args.clone(),
            // Nothing to report yet. A tool that reports progress — the task
            // tool is the one that does — rewrites this as it goes.
            metadata: serde_json::Value::Null,
            started: at,
        },
    ) {
        turn.persist_part(assistant, &part);
        deliver(
            turn,
            Event::PartUpdated {
                session_id: turn.session_id.clone(),
                message_id: assistant.id.clone(),
                part,
            },
        )
        .await?;
    }

    let ctx = ToolCtx {
        cwd: turn.cwd.clone(),
        cancel: turn.cancel.child_token(),
        call_id: prepared.call.id.clone(),
        files: Arc::clone(&turn.files),
        credentials: turn.credentials.clone(),
        // Built per call because a subagent reports its progress on *this*
        // part, and the part is what a frontend renders the child's one
        // inline row from. With several calls batched, that is also what keeps
        // one child's progress off another child's row.
        spawn: turn.spawn.as_ref().map(|host| {
            Arc::new(crate::subagent::Spawn {
                host: Arc::clone(host),
                events: Arc::clone(&turn.events),
                session_id: turn.session_id.clone(),
                // Shared with this turn: the parent is inside the call for as
                // long as the child runs, so the registry is the child's to ask
                // through and a reply addressed to the parent reaches it. Keyed
                // by request id, so a sibling child asking at the same moment
                // is a second entry rather than an eviction (**D462**).
                pending: Arc::clone(&turn.pending),
                message_id: assistant.id.clone(),
                part_id: prepared.call.part_id.clone(),
            }) as Arc<dyn crate::tool::task::Subagents>
        }),
        // Built per call for the same reason, and out of the same three
        // pieces the permission wait uses: a dialog names the call it came
        // from, and a reply has to reach the turn that is blocked in it.
        // Present on every turn — including a subagent's, whose questions
        // cross to the parent exactly as its permission dialogs do. What
        // keeps a headless run from being asked is a standing rule
        // refusing `question`, not the absence of this.
        ask: Some(Arc::new(Ask {
            events: Arc::clone(&turn.events),
            session_id: turn.session_id.clone(),
            pending: Arc::clone(&turn.pending),
            cancel: turn.cancel.clone(),
            source: QuestionSource {
                message_id: assistant.id.clone(),
                call_id: prepared.call.id.clone(),
            },
            hooks: turn.hooks.clone(),
        }) as Arc<dyn question::Asker>),
        // Present exactly where the approval cell was threaded: a parent
        // turn of an engine whose registry holds build. A child turn
        // carries no cell, so a subagent's call reads the no-build
        // refusal even before its rules refuse it.
        switch: turn.pending_switch.as_ref().map(|cell| {
            Arc::new(SwitchToBuild {
                pending: Arc::clone(cell),
            }) as Arc<dyn crate::tool::plan::Switcher>
        }),
        // Shared with every call in this session, root or subagent alike
        // (`Turn::child` carries it forward too): a background job
        // outlives the turn that started it, so which turn started it is
        // not a reason to track it differently.
        jobs: turn.jobs.clone(),
    };

    ControlFlow::Continue(Started {
        prepared,
        at,
        ctx,
        cancel: turn.cancel.clone(),
    })
}

/// Runs one started call's tool and reports what it produced.
///
/// **Holds nothing of the turn**, which is the whole point: everything it
/// needs was cloned into [`Started`], so a batch can put several of these on
/// tasks of their own while the turn's thread keeps sole ownership of the
/// assistant message.
async fn body(started: Started) -> Finished {
    let Started {
        prepared,
        at,
        ctx,
        cancel,
    } = started;

    // The tool gets a child of the turn's token so a cancel reaches it, and
    // the race below is what stops waiting for the tool's *result*. What it
    // must not do is drop the tool's future along with the wait: the shell
    // tool `killpg`s its command's process group from inside that future, so
    // a dropped one leaves the group alive — see [`TOOL_CANCEL_GRACE`]. The
    // cancelled future is therefore polled on until it winds itself up, or
    // until the grace says it never will.
    // A cancel that arrived before the tool was ever polled must not start it.
    // The grace below is for a tool that is already *running*, and a future's
    // first poll is precisely where its body begins — so entering the grace
    // holding an unpolled future would start the work the cancel refused, and
    // then discard its result. `write` and `read` never look at their token,
    // so one first polled there would run to completion: the file changed, the
    // transcript saying the call was cancelled. Checking here rather than
    // trusting the race below is what keeps those two facts the same fact.
    let result = if cancel.is_cancelled() {
        Err(ToolError::Cancelled)
    } else {
        let running = prepared.tool.run(prepared.args.clone(), &ctx);
        tokio::pin!(running);
        let finished = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = &mut running => Some(result),
        };
        match finished {
            Some(result) => result,
            None => {
                // The token the tool watches is already cancelled, so this
                // waits on cleanup rather than on the work. Reached only with
                // a future that has been polled at least once, which is what
                // makes "cleanup" the right word for what is being waited on.
                if tokio::time::timeout(TOOL_CANCEL_GRACE, running)
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        tool = prepared.call.name.as_str(),
                        grace = ?TOOL_CANCEL_GRACE,
                        "the tool did not finish cancelling in time; abandoning it, \
                         which may leave what it started running"
                    );
                }

                // Whatever the tool managed to return inside the grace, the
                // turn ended because it was cancelled and says exactly what it
                // said before: the cancel is the outcome, and the grace bought
                // the tool its cleanup, not a second chance at a result.
                Err(ToolError::Cancelled)
            }
        }
    };

    Finished {
        prepared,
        at,
        result,
    }
}

/// Applies one finished call to the assistant message: the language server's
/// opinion, the `PostToolUse` hook's, and the terminal state the model reads.
///
/// Back on the turn's own thread, which is what makes the `&mut Message` here
/// safe however many bodies were in flight. A batch calls this in **completion**
/// order; the part it rewrites is the one its own call opened, so the message
/// itself stays in call order.
async fn finish(
    turn: &Turn,
    assistant: &mut Message,
    finished: Finished,
) -> ControlFlow<Option<Outcome>> {
    let Finished {
        prepared,
        at: started,
        result,
    } = finished;
    let Prepared {
        call, args, hook, ..
    } = prepared;

    match result {
        Ok(mut output) => {
            // The single seam where a language server's opinion reaches the
            // model. It is here rather than inside `edit`, `write` and `read`
            // because the observable output is identical either way, and one
            // place that knows which tools care beats three tools each
            // remembering to ask. Everything inside swallows its own failures,
            // so a language server can cost this call some advice and can
            // never cost it its result.
            if let Some(lsp) = &turn.lsp {
                output
                    .output
                    .push_str(&lsp.annotate(&call.name, &args, &turn.cwd).await);
            }

            // After the annotation, so a `PostToolUse` hook is shown what the
            // model will actually read rather than a draft of it, and so its
            // own context lands last. Observational: nothing it says can turn a
            // call that succeeded into one that failed — an exit 2 here is
            // reported like any other failure (`HookEvent::blocking`).
            let after = turn
                .fire_hook(crate::hook::Payload::PostToolUse {
                    tool_name: call.name.clone(),
                    tool_input: args.clone(),
                    tool_response: serde_json::json!({
                        "output": output.output,
                        "title": output.title,
                        "metadata": output.metadata,
                    }),
                })
                .await;
            // Both halves of one call's hook conversation reach the model at
            // the one place it reads a call's result: what the `PreToolUse`
            // hook asked to add, then what the `PostToolUse` one did.
            for context in hook.context.iter().chain(&after.context) {
                output.output.push_str("\n\n");
                output.output.push_str(context);
            }

            emit_tool_state(
                turn,
                assistant,
                &call.part_id,
                ToolState::Completed {
                    input: args,
                    output: output.output,
                    title: output.title,
                    metadata: output.metadata,
                    started,
                    completed: now(),
                },
            )
            .await?;

            ControlFlow::Continue(())
        }
        // A cancelled call ends the turn. The part update travels the
        // terminal path — a plain send, like the finish event — because
        // racing it against the cancel that caused it would drop it.
        Err(error @ ToolError::Cancelled) => {
            if let Some(part) = set_tool_state(
                assistant,
                &call.part_id,
                ToolState::Error {
                    input: args,
                    error: error.to_string(),
                    started,
                    completed: now(),
                },
            ) {
                turn.persist_part(assistant, &part);
                let _ = turn
                    .events
                    .send(Event::PartUpdated {
                        session_id: turn.session_id.clone(),
                        message_id: assistant.id.clone(),
                        part,
                    })
                    .await;
            }

            ControlFlow::Break(Some(Outcome::cancelled()))
        }
        // The message is what the model sees next, so it is the error's own
        // words — `ToolError` promises they say what went wrong in terms the
        // model can act on.
        Err(error) => {
            emit_tool_state(
                turn,
                assistant,
                &call.part_id,
                ToolState::Error {
                    input: args,
                    error: error.to_string(),
                    started,
                    completed: now(),
                },
            )
            .await?;

            ControlFlow::Continue(())
        }
    }
}

/// Asks the user about one call and waits for the answer.
///
/// Every `PermissionRequested` that reaches the subscriber is answered by
/// exactly one `PermissionReplied` — the user's, or the refusal a cancel is —
/// so a frontend can retire its dialog unconditionally.
async fn wait_permission(
    turn: &Turn,
    call: &BufferedCall,
    title: String,
    args: &serde_json::Value,
    outside: &[PathBuf],
) -> ControlFlow<Option<Outcome>, PermissionReply> {
    // Fire-and-forget, unlike every other seam in this file: a `Notification`
    // hook exists to tell somebody their attention is wanted — a desktop
    // notification, a terminal bell — and awaiting one would make the dialog
    // itself wait for a program whose whole job is to run beside it. Each hook
    // still has its own budget, and the detached task holds nothing the turn
    // needs.
    notify_hook(
        turn,
        format!("ganja needs your permission to use {}", call.name),
    );

    let (sender, receiver) = oneshot::channel();
    let id = PermissionId::ascending();
    turn.pending
        .lock()
        .expect("the pending replies are never poisoned")
        .open_permission(id.clone(), sender);

    // The directories outside the project this call would work in, disclosed
    // with the request so the dialog can say what it is really asking about.
    // They arrive from the decision that judged the call rather than being
    // read off the rules again here: what the person is shown has to be what
    // the judgement was made on, and what an "always" would then remember.
    let directories = outside
        .iter()
        .map(|directory| directory.to_string_lossy().into_owned())
        .collect();

    if let ControlFlow::Break(stop) = deliver(
        turn,
        Event::PermissionRequested {
            session_id: turn.session_id.clone(),
            id: id.clone(),
            call_id: call.id.clone(),
            tool: call.name.clone(),
            title,
            args: args.clone(),
            directories,
        },
    )
    .await
    {
        // The request never reached the subscriber, so no reply is owed.
        retract_permission(turn, &id);
        return ControlFlow::Break(stop);
    }

    let received = tokio::select! {
        biased;
        () = turn.cancel.cancelled() => None,
        reply = receiver => reply.ok(),
    };

    let Some(reply) = received else {
        // Cancelled while waiting. The reply below travels the terminal path
        // unconditionally: it is the answer this request was promised.
        retract_permission(turn, &id);
        let _ = turn
            .events
            .send(Event::PermissionReplied {
                session_id: turn.session_id.clone(),
                id,
                reply: PermissionReply::Reject,
            })
            .await;
        return ControlFlow::Break(Some(Outcome::cancelled()));
    };

    match deliver(
        turn,
        Event::PermissionReplied {
            session_id: turn.session_id.clone(),
            id: id.clone(),
            reply,
        },
    )
    .await
    {
        ControlFlow::Continue(()) => ControlFlow::Continue(reply),
        ControlFlow::Break(stop) => {
            // The user's answer lost its race against a cancel and was never
            // queued; what the request gets instead is the refusal the cancel
            // means, and the call does not run either way.
            if stop.is_some() {
                let _ = turn
                    .events
                    .send(Event::PermissionReplied {
                        session_id: turn.session_id.clone(),
                        id,
                        reply: PermissionReply::Reject,
                    })
                    .await;
            }

            ControlFlow::Break(stop)
        }
    }
}

/// One question as the wire carries it, from one as the model asked it.
///
/// One half of the round-trip pin that holds `ganja-tool`'s copy of this shape
/// against `ganja-protocol`'s — `ganja-core` is the only crate that sees both,
/// because a tool may not name a wire type. See [`question_prompt`] for the
/// other half, and `tests/question_shape.rs` for the test that compares their
/// serde representations.
///
/// Both destructure **exhaustively, and never with `..`**: a field added to
/// either copy fails to compile here until somebody decides what it means on
/// the other side.
#[must_use]
pub fn question_info(prompt: &question::Prompt) -> QuestionInfo {
    let question::Prompt {
        question,
        header,
        options,
        multiple,
    } = prompt;

    QuestionInfo {
        question: question.clone(),
        header: header.clone(),
        options: options.iter().map(question_option).collect(),
        multiple: *multiple,
        // Upstream's `Prompt` carries no `custom`: it is the asking service's
        // field rather than the model's, and its absence reads as the
        // documented default — a custom answer is allowed.
        custom: None,
    }
}

/// One question as the model asks it, from one as the wire carries it.
///
/// The other half of the pin described on [`question_info`].
#[must_use]
pub fn question_prompt(info: &QuestionInfo) -> question::Prompt {
    let QuestionInfo {
        question,
        header,
        options,
        multiple,
        // Dropped on the way back, which is upstream's own asymmetry: the
        // model's shape has no such field. Bound rather than skipped with
        // `..` so a *new* protocol field cannot be quietly lost here — it
        // would fail to compile until somebody decides what it means to the
        // model.
        custom: _,
    } = info;

    question::Prompt {
        question: question.clone(),
        header: header.clone(),
        options: options.iter().map(question_choice).collect(),
        multiple: *multiple,
    }
}

/// One choice as the wire carries it. See [`question_info`].
#[must_use]
pub fn question_option(choice: &question::Choice) -> QuestionOption {
    let question::Choice { label, description } = choice;

    QuestionOption {
        label: label.clone(),
        description: description.clone(),
    }
}

/// One choice as the model offers it. See [`question_info`].
#[must_use]
pub fn question_choice(option: &QuestionOption) -> question::Choice {
    let QuestionOption { label, description } = option;

    question::Choice {
        label: label.clone(),
        description: description.clone(),
    }
}

/// What one `question` call asks the person through.
///
/// Built per call for the same reason [`crate::subagent::Spawn`] is: what a
/// dialog has to name is *this* call's part, and a value that outlived the
/// call would name the wrong one. Everything it holds belongs to the turn —
/// the fanout the request is published on, the session it is addressed as, the
/// slot a reply lands in, and the cancel that ends the wait.
pub(crate) struct Ask {
    /// The turn's fanout. A child turn's is its private channel, and the
    /// crossing watcher re-addresses what it finds there, exactly as it does
    /// for a permission dialog.
    pub(crate) events: Arc<Fanout>,
    /// The session the request is addressed as.
    pub(crate) session_id: SessionId,
    /// Where the reply lands, shared with the turn handle the engine routes
    /// commands into. A subagent shares the **parent's**, because the parent
    /// is blocked inside the call the child is running — and several of those
    /// calls can be in flight at once, which is why the registry is keyed.
    pub(crate) pending: Arc<std::sync::Mutex<PendingReplies>>,
    /// Ends the wait, and the turn with it.
    pub(crate) cancel: CancellationToken,
    /// Where in the transcript the question was asked from.
    pub(crate) source: QuestionSource,
    /// The session's hooks, for the `Notification` one this seam fires. Carried
    /// rather than reached through the turn for the reason everything else here
    /// is: this holds the turn's pieces without holding the turn.
    pub(crate) hooks: Option<Arc<crate::hook::Hooks>>,
}

impl std::fmt::Debug for Ask {
    /// Hand-written because the reply slot is a channel end with no [`Debug`]
    /// of its own, and because what is worth reading here is where the call
    /// sits rather than the machinery behind it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Ask")
            .field("session_id", &self.session_id)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl question::Asker for Ask {
    /// Publishes the request and waits, under the same discipline the
    /// permission wait keeps and for the same reason: **every
    /// `QuestionAsked` that reaches a subscriber is followed by exactly one
    /// terminal event**, so a frontend can retire its dialog unconditionally.
    ///
    /// The three races and their one terminal each:
    ///
    /// | the winner | what the subscriber sees | what the call reads |
    /// |---|---|---|
    /// | a reply | `QuestionReplied` | the answers |
    /// | a dismissal | `QuestionRejected` | upstream's dismissal sentence |
    /// | a cancel | `QuestionRejected` | a cancelled call, which ends the turn |
    ///
    /// A cancel wins even against an answer already in the channel: the
    /// permission wait resolves that race the same way, and for the same
    /// reason — the person stopped the turn, so the work the answer would
    /// have unblocked must not happen.
    async fn ask(
        &self,
        questions: Vec<question::Prompt>,
    ) -> Result<Vec<question::Answer>, question::Unanswered> {
        // The session's other "somebody is wanted" moment, notified the same
        // detached way the permission dialog is and for the same reason.
        if let Some(hooks) = self.hooks.clone() {
            let session = self.session_id.as_str().to_owned();
            tokio::spawn(async move {
                let outcome = hooks
                    .fire(
                        &session,
                        &crate::hook::Payload::Notification {
                            message: "ganja is waiting for an answer to a question".to_owned(),
                        },
                    )
                    .await;
                outcome.report(crate::hook::HookEvent::Notification);
            });
        }

        let (sender, receiver) = oneshot::channel();
        let id = QuestionId::ascending();
        self.pending
            .lock()
            .expect("the pending replies are never poisoned")
            .open_question(id.clone(), sender);

        let asked = Event::QuestionAsked {
            session_id: self.session_id.clone(),
            id: id.clone(),
            questions: questions.iter().map(question_info).collect(),
            source: Some(self.source.clone()),
        };
        if self.publish(asked).await.is_break() {
            // The request never reached the subscriber, so no reply is owed
            // and no terminal event is either.
            self.retract(&id);
            return Err(question::Unanswered::Cancelled);
        }

        let received = tokio::select! {
            biased;
            () = self.cancel.cancelled() => None,
            answered = receiver => answered.ok(),
        };

        let Some(answered) = received else {
            // Cancelled while waiting. The rejection below travels the
            // terminal path unconditionally: it is the answer this request was
            // promised.
            self.retract(&id);
            self.terminate(&id).await;
            return Err(question::Unanswered::Cancelled);
        };

        let (event, answer) = match answered {
            Answered::Replied(answers) => (
                Event::QuestionReplied {
                    session_id: self.session_id.clone(),
                    id: id.clone(),
                    answers: answers.clone(),
                },
                Ok(answers),
            ),
            Answered::Rejected => (
                Event::QuestionRejected {
                    session_id: self.session_id.clone(),
                    id: id.clone(),
                },
                Err(question::Unanswered::Dismissed),
            ),
        };

        if self.publish(event).await.is_break() {
            // The answer lost its race against a cancel and was never queued;
            // what the request gets instead is the dismissal the cancel means,
            // and the call does not complete either way.
            self.terminate(&id).await;
            return Err(question::Unanswered::Cancelled);
        }

        answer
    }
}

impl Ask {
    /// Queues `event`, or reports that the turn is over — the question seam's
    /// [`deliver`], which cannot use that one because it holds the turn's
    /// pieces rather than the turn.
    async fn publish(&self, event: Event) -> ControlFlow<()> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => ControlFlow::Break(()),
            queued = self.events.send(event) => match queued {
                Ok(()) => ControlFlow::Continue(()),
                Err(_) => ControlFlow::Break(()),
            },
        }
    }

    /// Forgets this question, by its own id — [`retract_permission`]'s twin for
    /// the seam that carries the registry without carrying the turn.
    fn retract(&self, id: &QuestionId) {
        self.pending
            .lock()
            .expect("the pending replies are never poisoned")
            .close_question(id);
    }

    /// Sends the terminal event a published request is owed, on the plain path
    /// that is never raced against the cancel that caused it.
    async fn terminate(&self, id: &QuestionId) {
        let _ = self
            .events
            .send(Event::QuestionRejected {
                session_id: self.session_id.clone(),
                id: id.clone(),
            })
            .await;
    }
}

/// Starts this session's `Notification` hooks and does not wait for them.
///
/// Detached on purpose; see the call sites. Nothing is reported back — a
/// notifier that failed is in the log by the time anybody looks, and there is
/// no dialog it could have changed.
fn notify_hook(turn: &Turn, message: String) {
    let Some(hooks) = turn.hooks.clone() else {
        return;
    };
    let session = turn.session_id.as_str().to_owned();
    tokio::spawn(async move {
        let outcome = hooks
            .fire(&session, &crate::hook::Payload::Notification { message })
            .await;
        outcome.report(crate::hook::HookEvent::Notification);
    });
}

/// Forgets one permission request that will never be answered.
///
/// **By its id, never "whatever is open".** When the registry was a single cell
/// the two were the same sentence, because the turn was blocked inside the one
/// request being retracted. A step whose batched `task` calls each run a child
/// that can ask makes them different sentences, and clearing the registry here
/// would abandon a sibling's dialog — the deadlock the pre-mortem named.
fn retract_permission(turn: &Turn, id: &PermissionId) {
    turn.pending
        .lock()
        .expect("the pending replies are never poisoned")
        .close_permission(id);
}

/// Moves the call's part to [`ToolState::Error`] carrying `error`, which is
/// what the model reads as the call's result on the next request.
async fn fail_call(
    turn: &Turn,
    assistant: &mut Message,
    call: &BufferedCall,
    input: serde_json::Value,
    error: &str,
) -> ControlFlow<Option<Outcome>> {
    let stamp = now();
    let Some(part) = set_tool_state(
        assistant,
        &call.part_id,
        ToolState::Error {
            input,
            error: error.to_owned(),
            started: stamp,
            completed: stamp,
        },
    ) else {
        return ControlFlow::Continue(());
    };

    turn.persist_part(assistant, &part);
    deliver(
        turn,
        Event::PartUpdated {
            session_id: turn.session_id.clone(),
            message_id: assistant.id.clone(),
            part,
        },
    )
    .await
}

/// Closes every remaining buffered call with an [`ToolState::Error`] saying
/// why it never ran.
///
/// A turn on its way out must not leave a part `Pending` or `Running`: the
/// next request has to answer every call the model opened, and a frontend has
/// to be able to stop its spinners. The updates travel the terminal path —
/// plain sends — because this only runs when the turn is already over, and
/// racing them against the cancel that caused them would drop them.
async fn fail_buffered(
    turn: &Turn,
    assistant: &mut Message,
    calls: &mut Vec<BufferedCall>,
    error: &str,
) {
    for call in std::mem::take(calls) {
        close_unresolved(turn, assistant, &call, error).await;
    }
}

/// Closes one call that will never run, unless it already has a terminal
/// state, in which case there is nothing to close and nothing is sent.
async fn close_unresolved(turn: &Turn, assistant: &mut Message, call: &BufferedCall, error: &str) {
    let input = parse_args(&call.json).unwrap_or_else(|_| serde_json::json!({}));
    let stamp = now();
    if let Some(part) = set_tool_state(
        assistant,
        &call.part_id,
        ToolState::Error {
            input,
            error: error.to_owned(),
            started: stamp,
            completed: stamp,
        },
    ) {
        turn.persist_part(assistant, &part);
        let _ = turn
            .events
            .send(Event::PartUpdated {
                session_id: turn.session_id.clone(),
                message_id: assistant.id.clone(),
                part,
            })
            .await;
    }
}

/// Replaces the state of the tool part `part_id`, returning the part as it
/// now stands for the event that reports it, or [`None`] when there is
/// nothing to report.
fn set_tool_state(assistant: &mut Message, part_id: &PartId, state: ToolState) -> Option<Part> {
    let part = assistant
        .parts
        .iter_mut()
        .find(|part| part.id == *part_id)?;
    if let PartBody::Tool { state: current, .. } = &mut part.body {
        // Terminal states stick: a call the turn already closed is not
        // reopened by a late race.
        if matches!(
            current,
            ToolState::Completed { .. } | ToolState::Error { .. }
        ) {
            return None;
        }
        *current = state;
    }

    Some(part.clone())
}

/// Parses a call's accumulated argument JSON.
///
/// An empty buffer parses as `{}`: a call to a tool with no parameters
/// streams no fragments at all, and upstream opens every call with `input:
/// {}` for the same reason. A non-object parses into `{"value": …}`, which is
/// upstream's wrapping. Anything unparseable is an error the model is told
/// about.
fn parse_args(json: &str) -> Result<serde_json::Value, String> {
    if json.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }

    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(value @ serde_json::Value::Object(_)) => Ok(value),
        Ok(other) => Ok(serde_json::json!({ "value": other })),
        Err(error) => Err(error.to_string()),
    }
}

/// Sums what two requests spent.
fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
        reasoning_tokens: a.reasoning_tokens.saturating_add(b.reasoning_tokens),
        cache_read_tokens: a.cache_read_tokens.saturating_add(b.cache_read_tokens),
        cache_write_tokens: a.cache_write_tokens.saturating_add(b.cache_write_tokens),
    }
}

/// Sleeps until the debounced text write is due. Only ever polled when the
/// step loop's guard saw a deadline; the pending fallback is for the guard's
/// disabled branch, whose future is built but never awaited.
async fn flush_after(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Queues `event` for every subscriber, or breaks with the turn's report.
///
/// Waiting on a full lossless queue must not outlive a cancel, hence the
/// race. A cancel that lands mid-delivery abandons it wherever it stood, so
/// subscribers of a *cancelled* turn may differ by the one event that was in
/// flight — and by nothing else, because the terminal events that follow
/// travel plain sends that are never raced. A completed turn is delivered
/// whole to everyone.
/// Records a call's terminal state and delivers the part update — the shape
/// the completed and failed arms share. The cancelled arm stays written out
/// at its match site: its update travels the terminal path as a plain send,
/// because racing it against the cancel that caused it would drop it.
async fn emit_tool_state(
    turn: &Turn,
    assistant: &mut Message,
    part_id: &PartId,
    state: ToolState,
) -> ControlFlow<Option<Outcome>> {
    if let Some(part) = set_tool_state(assistant, part_id, state) {
        turn.persist_part(assistant, &part);
        deliver(
            turn,
            Event::PartUpdated {
                session_id: turn.session_id.clone(),
                message_id: assistant.id.clone(),
                part,
            },
        )
        .await?;
    }

    ControlFlow::Continue(())
}

async fn deliver(turn: &Turn, event: Event) -> ControlFlow<Option<Outcome>> {
    tokio::select! {
        biased;
        () = turn.cancel.cancelled() => ControlFlow::Break(Some(Outcome::cancelled())),
        queued = turn.events.send(event) => match queued {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(None),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::{
        Answered, BufferedCall, ChildParts, PendingReplies, Turn, TurnKind, add_usage, attached,
        parse_args, resolve, resolve_mentions, sliced,
    };
    use crate::{
        engine::Fanout,
        permission::Permissions,
        protocol::{
            FinishReason, Message, Part, PartBody, PermissionId, PermissionReply, QuestionId,
            SessionId, ToolState, Usage,
        },
        provider::{FakeProvider, fake},
        subagent::{Host, Spawn},
        tool::{Credentials, FileTimes, Registry, Tool, ToolCtx, ToolError, ToolOutput},
    };

    /// Two dialogs stand open together, and each reply reaches the one it
    /// names.
    ///
    /// The single cell this replaced could not hold the second without dropping
    /// the first's channel — the deadlock the pre-mortem named, in its smallest
    /// form.
    #[test]
    fn two_open_permission_requests_are_each_answered_by_their_own_id() {
        let mut pending = PendingReplies::default();
        let (first, mut first_reply) = tokio::sync::oneshot::channel();
        let (second, mut second_reply) = tokio::sync::oneshot::channel();
        let alpha = PermissionId::ascending();
        let beta = PermissionId::ascending();

        pending.open_permission(alpha.clone(), first);
        pending.open_permission(beta.clone(), second);
        assert_eq!(pending.len(), 2, "both are open at the same time");

        // Newest first: routing is by id, not by arrival.
        assert!(pending.answer_permission(&beta, PermissionReply::Reject));
        assert!(pending.answer_permission(&alpha, PermissionReply::Once));
        assert_eq!(
            first_reply.try_recv().expect("alpha was answered"),
            PermissionReply::Once
        );
        assert_eq!(
            second_reply.try_recv().expect("beta was answered"),
            PermissionReply::Reject
        );
        assert_eq!(pending.len(), 0, "an answered request is closed");
    }

    /// Closing one request is closing *that* request. When the registry was one
    /// cell the two sentences were the same one, and a sibling's dialog was
    /// what got thrown away.
    #[test]
    fn closing_one_request_leaves_its_sibling_open() {
        let mut pending = PendingReplies::default();
        let (first, _first_reply) = tokio::sync::oneshot::channel();
        let (second, mut second_reply) = tokio::sync::oneshot::channel();
        let alpha = PermissionId::ascending();
        let beta = PermissionId::ascending();
        pending.open_permission(alpha.clone(), first);
        pending.open_permission(beta.clone(), second);

        pending.close_permission(&alpha);

        assert!(
            !pending.answer_permission(&alpha, PermissionReply::Once),
            "the retracted one is gone"
        );
        assert!(
            pending.answer_permission(&beta, PermissionReply::Once),
            "and the one beside it is not"
        );
        assert_eq!(
            second_reply.try_recv().expect("beta was answered"),
            PermissionReply::Once
        );
    }

    /// The property the discriminated single cell had, kept by holding the two
    /// kinds in two maps: an id of one kind never finds a wait of the other.
    #[test]
    fn a_reply_of_one_kind_never_reaches_a_wait_of_the_other() {
        let mut pending = PendingReplies::default();
        let (permission, _permission_reply) = tokio::sync::oneshot::channel();
        let (question, _question_reply) = tokio::sync::oneshot::channel();
        let asked = PermissionId::ascending();
        let question_id = QuestionId::ascending();
        pending.open_permission(asked.clone(), permission);
        pending.open_question(question_id.clone(), question);

        assert!(
            !pending.answer_question(&QuestionId::ascending(), Answered::Rejected),
            "an id nothing answers to delivers nothing"
        );
        assert_eq!(
            pending.len(),
            2,
            "and takes neither open request with it: {}",
            pending.len()
        );
        assert!(pending.answer_question(&question_id, Answered::Rejected));
        assert!(pending.answer_permission(&asked, PermissionReply::Once));
    }

    /// A tool that marks the filesystem the moment its body runs.
    ///
    /// It never looks at its cancellation token, which is not laziness: that
    /// is `write.rs` and `read.rs` exactly as they are today, and the point of
    /// the test is that nothing inside a tool is what saves us here.
    struct Effectful {
        marker: PathBuf,
    }

    #[derive(schemars::JsonSchema)]
    struct NoArgs {}

    #[async_trait::async_trait]
    impl Tool for Effectful {
        fn id(&self) -> &str {
            "effectful"
        }

        fn description(&self) -> &str {
            "marks the filesystem when it runs"
        }

        fn schema(&self) -> schemars::Schema {
            schemars::schema_for!(NoArgs)
        }

        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            std::fs::write(&self.marker, "the body ran").expect("the marker is writable");

            Ok(ToolOutput {
                title: "effectful".to_owned(),
                output: "done".to_owned(),
                metadata: serde_json::json!({}),
            })
        }
    }

    /// A turn carrying `tool` and nothing else of consequence. The receiver
    /// comes back with it because dropping it would close the event channel
    /// and turn every `deliver` into a different kind of stop.
    fn turn_with(
        cancel: CancellationToken,
        tool: Arc<dyn Tool>,
    ) -> (Turn, mpsc::Receiver<crate::protocol::Event>) {
        let (events, received) = mpsc::channel(64);
        let turn = Turn {
            provider: Arc::new(FakeProvider::new("", Duration::ZERO)),
            concurrency: crate::config::AgentsConfig::DEFAULT_CONCURRENCY,
            session_id: SessionId::from("ses_fixture".to_owned()),
            model: fake::MODEL.to_owned(),
            effort_options: serde_json::Map::new(),
            system: None,
            reminders: Vec::new(),
            kind: TurnKind::Prompt {
                mentions: Vec::new(),
            },
            tools: Arc::new(Registry::new(vec![tool])),
            permissions: Arc::new(std::sync::Mutex::new(Permissions::default())),
            cwd: std::env::temp_dir(),
            root: std::env::temp_dir(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            lsp: None,
            snapshots: None,
            prompt: "run it".to_owned(),
            cancel,
            pending: Arc::default(),
            steer: Arc::default(),
            events: Arc::new(Fanout::new(events)),
            slot: Arc::new(Mutex::new(None)),
            history: Arc::new(Mutex::new(Vec::new())),
            spawn: None,
            pending_switch: None,
            jobs: None,
            hooks: None,
            delegated: false,
            persist: None,
        };

        (turn, received)
    }

    /// A cancel that lands before the tool is ever polled must not start it.
    ///
    /// `resolve` builds the tool's future and then races it against the turn's
    /// token. The race is `biased` on the cancel, so an already-cancelled turn
    /// takes that arm *without polling the future at all* — and the grace that
    /// follows used to be where that future got its first poll, which is where
    /// an async body begins. A tool that never checks its token then ran to
    /// completion inside the grace: the file written, the result thrown away,
    /// the transcript reporting a cancel. This pins the two back together.
    ///
    /// The part is seeded already-closed on purpose. `set_tool_state` refuses
    /// to reopen a terminal state and returns `None`, which skips the block
    /// holding the `deliver` call — and `deliver` is the last cancel
    /// checkpoint before the race. That is one of the two real ways to arrive
    /// at the race with a cancelled token, and the only one a test can reach
    /// without racing the scheduler.
    #[tokio::test]
    async fn a_call_cancelled_before_it_starts_never_runs_the_tool() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let marker = dir.path().join("ran");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let (turn, _received) = turn_with(
            cancel,
            Arc::new(Effectful {
                marker: marker.clone(),
            }),
        );

        let mut assistant = Message::assistant("canned");
        let mut part = Part::tool("call_1", "effectful");
        let part_id = part.id.clone();
        if let PartBody::Tool { state, .. } = &mut part.body {
            *state = ToolState::Error {
                input: serde_json::json!({}),
                error: "closed by an earlier race".to_owned(),
                started: 0,
                completed: 1,
            };
        }
        assistant.parts.push(part);

        let call = BufferedCall {
            id: "call_1".to_owned(),
            name: "effectful".to_owned(),
            json: "{}".to_owned(),
            part_id,
        };

        let flow = resolve(&turn, &mut assistant, &call).await;

        assert!(
            !marker.exists(),
            "the tool body ran for a call that was cancelled before it started"
        );
        match flow {
            std::ops::ControlFlow::Break(Some(outcome)) => {
                assert_eq!(outcome.reason, FinishReason::Cancelled);
            }
            other => panic!("a cancelled call ends the turn: {:?}", other.is_break()),
        }
    }

    /// The same call on a turn nobody cancelled still runs, so the guard above
    /// is a guard and not a wall.
    #[tokio::test]
    async fn a_call_on_a_live_turn_still_runs_the_tool() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let marker = dir.path().join("ran");

        let (turn, _received) = turn_with(
            CancellationToken::new(),
            Arc::new(Effectful {
                marker: marker.clone(),
            }),
        );

        let mut assistant = Message::assistant("canned");
        let part = Part::tool("call_1", "effectful");
        let part_id = part.id.clone();
        assistant.parts.push(part);

        let call = BufferedCall {
            id: "call_1".to_owned(),
            name: "effectful".to_owned(),
            json: "{}".to_owned(),
            part_id,
        };

        let flow = resolve(&turn, &mut assistant, &call).await;

        assert!(marker.exists(), "an uncancelled call has to actually run");
        assert!(
            flow.is_continue(),
            "a tool that succeeded lets the turn carry on"
        );
    }

    #[test]
    fn arguments_parse_leniently_and_fail_loudly() {
        assert_eq!(
            parse_args("").expect("no fragments is a no-argument call"),
            serde_json::json!({})
        );
        assert_eq!(
            parse_args("   \n").expect("whitespace is still empty"),
            serde_json::json!({})
        );
        assert_eq!(
            parse_args(r#"{"path":"a.rs"}"#).expect("an object passes through"),
            serde_json::json!({"path": "a.rs"})
        );
        assert_eq!(
            parse_args("[1,2]").expect("a non-object is wrapped, as upstream wraps it"),
            serde_json::json!({"value": [1, 2]})
        );
        assert!(
            parse_args("{not json").is_err(),
            "malformed JSON is an error the model must hear about"
        );
    }

    #[test]
    fn usage_sums_field_by_field_and_saturates() {
        let summed = add_usage(
            Usage {
                input_tokens: 1,
                output_tokens: 2,
                reasoning_tokens: 3,
                cache_read_tokens: 4,
                cache_write_tokens: 5,
            },
            Usage {
                input_tokens: 10,
                output_tokens: 20,
                reasoning_tokens: 30,
                cache_read_tokens: 40,
                cache_write_tokens: 50,
            },
        );

        assert_eq!(
            summed,
            Usage {
                input_tokens: 11,
                output_tokens: 22,
                reasoning_tokens: 33,
                cache_read_tokens: 44,
                cache_write_tokens: 55,
            }
        );
        assert_eq!(
            add_usage(
                Usage {
                    input_tokens: u64::MAX,
                    ..Usage::default()
                },
                Usage {
                    input_tokens: 1,
                    ..Usage::default()
                },
            )
            .input_tokens,
            u64::MAX,
            "a sum never wraps into a tiny bill"
        );
    }

    /// A mention's path is project-relative, and this is where that means
    /// something: the integration suite uses absolute paths so its fixtures can
    /// live in a temporary directory, so the join itself is pinned here.
    #[test]
    fn a_mentioned_path_resolves_against_the_project_root() {
        let root = tempfile::tempdir().expect("a scratch directory");
        let nested = root.path().join("src");
        std::fs::create_dir(&nested).expect("the fixture nests");
        std::fs::write(nested.join("main.rs"), "fn main() {}").expect("the fixture writes");

        let block = attached(root.path(), "src/main.rs", None, None);
        assert_eq!(
            block, "<attached-file path=\"src/main.rs\">\nfn main() {}\n</attached-file>",
            "the block names the path the user typed and carries what it says"
        );

        // An absolute path is already resolved, and joining leaves it alone.
        let absolute = nested.join("main.rs");
        assert!(
            attached(root.path(), &absolute.to_string_lossy(), None, None).contains("fn main() {}"),
            "an absolute mention resolves to itself"
        );
    }

    /// The `#line-range` promise at the read: 1-indexed, inclusive, sliced
    /// before anything else sees the text, and the tag says which lines so two
    /// slices of one file stay distinguishable.
    #[test]
    fn a_ranged_mention_inlines_exactly_the_lines_it_names() {
        let root = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(root.path().join("a.txt"), "one\ntwo\nthree\nfour\nfive")
            .expect("the fixture writes");

        assert_eq!(
            attached(root.path(), "a.txt", Some(2), Some(4)),
            "<attached-file path=\"a.txt\" lines=\"2-4\">\ntwo\nthree\nfour\n</attached-file>"
        );
        assert_eq!(
            attached(root.path(), "a.txt", Some(4), None),
            "<attached-file path=\"a.txt\" lines=\"4-\">\nfour\nfive\n</attached-file>",
            "no end reads from start to the end of the file"
        );
        assert_eq!(
            attached(root.path(), "a.txt", Some(99), None),
            "<attached-file path=\"a.txt\" lines=\"99-\">\n\n</attached-file>",
            "a start past the end names an empty slice rather than failing"
        );
    }

    /// The scan normalizes what a person types, but a wire client can send any
    /// numbers it likes; the read applies upstream's keep-the-end-only-when-
    /// start-is-less rule again so the two never disagree.
    #[test]
    fn a_range_a_client_sent_backwards_reads_from_start_to_the_end() {
        assert_eq!(sliced("a\nb\nc\nd", 3, Some(2)), "c\nd");
        assert_eq!(sliced("a\nb\nc\nd", 3, Some(3)), "c\nd");
        assert_eq!(
            sliced("a\nb\nc\nd", 0, None),
            "a\nb\nc\nd",
            "a zero start is the top"
        );
    }

    /// Resolution replaces the reference in the request's own copy. What it
    /// must never do is record the file as read — that is the model's act, not
    /// the user's, and `edit` depends on the difference.
    #[test]
    fn resolving_a_mention_is_not_a_read() {
        let root = tempfile::tempdir().expect("a scratch directory");
        let path = root.path().join("a.txt");
        std::fs::write(&path, "one").expect("the fixture writes");

        let mut messages = vec![Message {
            id: crate::protocol::MessageId::ascending(),
            role: crate::protocol::Role::User,
            parts: vec![Part::file("a.txt", "text/plain")],
            time: crate::protocol::MessageTime {
                created: 1,
                completed: Some(1),
            },
            model: None,
            usage: None,
        }];
        resolve_mentions(&mut messages, root.path(), &|_| false);

        assert!(
            messages[0].parts[0]
                .as_text()
                .is_some_and(|text| text.contains("one")),
            "the reference became content: {:?}",
            messages[0].parts[0]
        );

        let times = FileTimes::default();
        assert!(
            times.check_fresh(&path).is_err(),
            "and nothing recorded the file as read"
        );
    }

    /// One user message carrying one file part for `path`, for the resolution
    /// tests to work on.
    fn message_mentioning(path: &str) -> Vec<Message> {
        vec![Message {
            id: crate::protocol::MessageId::ascending(),
            role: crate::protocol::Role::User,
            parts: vec![Part::file(path, crate::attachment::mime(path))],
            time: crate::protocol::MessageTime {
                created: 1,
                completed: Some(1),
            },
            model: None,
            usage: None,
        }]
    }

    /// The attachment split at the request build: a binary mime the wire
    /// carries stays a file part with its base64 filled in, and the transcript
    /// side of the promise — the stored part — was never touched because
    /// resolution runs on the request's own copy.
    #[test]
    fn a_binary_mention_becomes_base64_when_the_wire_carries_it() {
        let root = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(root.path().join("shot.png"), b"png-bytes").expect("the fixture writes");

        let mut messages = message_mentioning("shot.png");
        resolve_mentions(&mut messages, root.path(), &|mime| mime == "image/png");

        let PartBody::File {
            path,
            mime,
            content: Some(content),
            ..
        } = &messages[0].parts[0].body
        else {
            panic!("the part stays a file part: {:?}", messages[0].parts[0]);
        };
        assert_eq!(path, "shot.png");
        assert_eq!(mime, "image/png");
        {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            assert_eq!(
                STANDARD.decode(content).expect("the payload is base64"),
                b"png-bytes"
            );
        }
    }

    /// The degradation half: a wire that answers no gets a text block naming
    /// the file and its kind — never a dropped part, never a failed turn.
    #[test]
    fn a_binary_mention_the_wire_cannot_carry_degrades_to_its_name() {
        let root = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(root.path().join("shot.png"), b"png-bytes").expect("the fixture writes");

        let mut messages = message_mentioning("shot.png");
        resolve_mentions(&mut messages, root.path(), &|_| false);

        let text = messages[0].parts[0]
            .as_text()
            .expect("the part degraded to text");
        assert!(
            text.contains("shot.png"),
            "the model learns the name: {text}"
        );
        assert!(
            text.contains("image/png") && text.contains("does not carry"),
            "and why the bytes are not there: {text}"
        );
    }

    /// SVG is upstream's one image that reads as text: it is inlined like any
    /// text mention, whatever the wire would have said about images.
    #[test]
    fn an_svg_mention_is_inlined_as_text_whatever_the_wire_accepts() {
        let root = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(root.path().join("logo.svg"), "<svg/>").expect("the fixture writes");

        let mut messages = message_mentioning("logo.svg");
        resolve_mentions(&mut messages, root.path(), &|_| false);

        assert!(
            messages[0].parts[0]
                .as_text()
                .is_some_and(|text| text.contains("<svg/>")),
            "the markup itself is inlined: {:?}",
            messages[0].parts[0]
        );
    }

    /// Where the fixture parent's credentials sit, which is nowhere: the guard
    /// compares paths, and what the child must inherit is the *answer*, not a
    /// file.
    const PARENTS_STORE: &str = "/nonexistent/ganja/auth.json";

    /// A [`Spawn`] as a `task` call hands one over. The parent is blocked
    /// inside that call, which is what makes its pending-reply cell free for
    /// the child to use and its language servers worth reusing.
    ///
    /// The receiver comes back with it because dropping it would close the
    /// parent's event channel, and a dead sender is not what a blocked parent
    /// is holding.
    fn parent_spawn(
        lsp: Option<Arc<crate::lsp::Lsp>>,
    ) -> (Spawn, mpsc::Receiver<crate::protocol::Event>) {
        let (events, received) = mpsc::channel(64);
        let host = Host {
            provider: Arc::new(FakeProvider::new("", Duration::ZERO)),
            concurrency: crate::config::AgentsConfig::DEFAULT_CONCURRENCY,
            model: fake::MODEL.to_owned(),
            agents: Arc::new(
                crate::agent::Registry::build(&crate::config::Config::default())
                    .expect("the default config resolves agents"),
            ),
            tools: Arc::new(Registry::new(Vec::new())),
            permissions: Arc::new(std::sync::Mutex::new(Permissions::default())),
            base_prompt: None,
            prompt_suffix: None,
            cwd: std::env::temp_dir(),
            root: std::env::temp_dir(),
            credentials: Credentials::Guarded(PARENTS_STORE.into()),
            lsp,
            persistence: None,
            jobs: None,
            hooks: None,
        };

        let spawn = Spawn {
            host: Arc::new(host),
            events: Arc::new(Fanout::new(events)),
            session_id: SessionId::from("ses_parent".to_owned()),
            pending: Arc::default(),
            message_id: crate::protocol::MessageId::ascending(),
            part_id: crate::protocol::PartId::ascending(),
        };

        (spawn, received)
    }

    /// The turn a `task` call builds for its subagent, with the child's own
    /// event channel held open beside it for the same reason.
    fn child_of(spawn: &Spawn) -> (Turn, mpsc::Receiver<crate::protocol::Event>) {
        let (events, received) = mpsc::channel(64);
        let turn = Turn::child(
            spawn,
            ChildParts {
                session_id: SessionId::from("ses_child".to_owned()),
                model: fake::MODEL.to_owned(),
                system: None,
                kind: TurnKind::Prompt {
                    mentions: Vec::new(),
                },
                prompt: "do the thing".to_owned(),
                permissions: Permissions::default(),
                events,
                history: Vec::new(),
                cancel: CancellationToken::new(),
                persist: None,
            },
        );

        (turn, received)
    }

    /// Upstream's plan/build reminders are about the agent a *person* switched
    /// to. Nobody switched to a subagent, so it is told nothing of the kind.
    #[test]
    fn a_child_turn_carries_no_reminders() {
        let (spawn, _parent) = parent_spawn(None);
        let (turn, _events) = child_of(&spawn);

        assert!(
            turn.reminders.is_empty(),
            "a subagent runs the prompt it was built with: {:?}",
            turn.reminders
        );
    }

    /// Read-before-write is per conversation, so a child begins having read
    /// nothing — whatever the parent read is not what the child may write over.
    #[test]
    fn a_child_turn_starts_a_fresh_read_log() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").expect("the fixture writes");

        let (spawn, _parent) = parent_spawn(None);
        let (turn, _events) = child_of(&spawn);

        assert!(
            turn.files.check_fresh(&path).is_err(),
            "the child has read nothing yet, so it may write nothing yet"
        );
        assert_eq!(
            Arc::strong_count(&turn.files),
            1,
            "and the log is its own, not a view of somebody else's"
        );
    }

    /// A patch is a diff of the working tree rather than a record of who wrote
    /// to it, so the parent's own step already covers what the child changed.
    #[test]
    fn a_child_turn_takes_no_snapshots_of_its_own() {
        let (spawn, _parent) = parent_spawn(None);
        let (turn, _events) = child_of(&spawn);

        assert!(
            turn.snapshots.is_none(),
            "the step that made the call is where an `/undo` reaches the change"
        );
    }

    /// The busy slot a frontend reads belongs to the parent, which is busy
    /// running this call. The child's own cell is nobody else's.
    #[tokio::test]
    async fn a_child_turn_gets_a_turn_handle_cell_of_its_own() {
        let (spawn, _parent) = parent_spawn(None);
        let (turn, _events) = child_of(&spawn);

        assert!(
            turn.slot.lock().await.is_none(),
            "nothing is holding the child's cell when it starts"
        );
        assert_eq!(
            Arc::strong_count(&turn.slot),
            1,
            "and nobody outside the child's turn can reach it"
        );
    }

    /// The depth limit, as the loop sees it: a child has nothing to spawn
    /// with, so nothing below it can spawn anything.
    #[test]
    fn a_child_turn_cannot_spawn_anything() {
        let (spawn, _parent) = parent_spawn(None);
        let (turn, _events) = child_of(&spawn);

        assert!(
            turn.spawn.is_none(),
            "one level, fixed — and fixed here rather than asked about later"
        );
    }

    /// A subagent runs unattended, so it is the last conversation that should
    /// be able to read a key off the disk: it refuses the same store its parent
    /// does, and refuses it because it was told which one that is.
    #[test]
    fn a_child_turn_refuses_the_same_credential_store_the_parent_does() {
        let (spawn, _parent) = parent_spawn(None);
        let (turn, _events) = child_of(&spawn);

        assert_eq!(
            turn.credentials.guarded(),
            Some(std::path::Path::new(PARENTS_STORE)),
            "a child handed no store would read one the parent refuses"
        );
    }

    /// A subagent's permission dialog is answered through the parent's cell:
    /// the engine's handle routes a reply into that one, and the parent is
    /// blocked here rather than using it.
    #[test]
    fn a_child_turn_shares_the_parents_pending_permission_cell() {
        let (spawn, _parent) = parent_spawn(None);
        let (turn, _events) = child_of(&spawn);

        assert!(
            Arc::ptr_eq(&turn.pending, &spawn.pending),
            "a child asking through a cell of its own would be a child that hangs"
        );
    }

    /// A client is identified by `(root, server)`, so a child working in the
    /// same project reuses what the parent already has warm.
    #[test]
    fn a_child_turn_shares_the_parents_language_servers() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let lsp = crate::lsp::Lsp::new(Some(&crate::config::LspConfig::Enabled(true)), dir.path())
            .expect("the builtins resolve to at least one server");

        let (spawn, _parent) = parent_spawn(Some(Arc::clone(&lsp)));
        let (turn, _events) = child_of(&spawn);

        let shared = turn
            .lsp
            .expect("a child of a session that has servers is given them");
        assert!(
            Arc::ptr_eq(&shared, &lsp),
            "the same service, not a second one started behind the parent's back"
        );
    }
}
