//! What the child is spawned with: two argv builders, two never-lists, and
//! the environment the CLI inherits.
//!
//! Spec: the recording, `tests/fixtures/claude-code-sdk-mcp-probe.txt` — its
//! header's `argv:` block is [`BASE`] verbatim, its `env:` block is [`STRIP`]
//! by name and the first nine of [`SET`] by name — [`BETAS`] was added after
//! the recording and is not in it — and both preflights (runs 0 and 0b)
//! prove the whole line parses on the recorded build and on the floor.
//!
//! # Two builders, and why the never-lists are two
//!
//! A conversation's process is held open across turns; a one-shot's — a
//! title, a compaction summary — lives for one turn and is told so with
//! `--no-session-persistence`. That flag is therefore forbidden on one
//! builder and required on the other, which is a distinction a single
//! never-list cannot draw. So [`NEVER_ANYWHERE`] holds what **no** argv of
//! this wire may carry and is checked against every builder, and
//! [`NEVER_ON_CONVERSATION`] holds the one flag only a one-shot may carry,
//! checked in addition to it against the held-process builder alone.
//!
//! `--resume` is on the wider list, not merely unbuilt (**D556**): the
//! recording served it on 2 of 8 turns and refused it on 6, so a builder that
//! grew it back is a defect this crate refuses to be able to express.
//! `--include-partial-messages` is there for a different reason — M4 read
//! that under it every completed block arrives twice — and is forbidden
//! rather than merely unbuilt so that both dropped flags are enforced the
//! same way.
//!
//! # The environment is built by name and never read
//!
//! Nothing here calls `env::var`. [`STRIP`] is `env_remove` and [`SET`] is
//! `env`, so a test can assert the whole posture off `Command::get_envs()`
//! without a value ever being read, logged or rendered — which is the point:
//! an `ANTHROPIC_API_KEY` left in a shell would outrank the CLI's own login
//! and bill a platform key in silence.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::provider::ProviderError;

/// The tokens every argv of this wire opens with, in this order.
///
/// **Twenty-one**, and the count is pinned by `argv_tests.rs` because this is
/// the one place it is checkable: a flag silently dropped from a builder is a
/// behaviour change no other test would name.
///
/// What each is for, in the order they appear: `-p` is print mode, the two
/// `stream-json` pairs make stdin and stdout the frame protocol this wire
/// speaks, `--verbose` is what makes the CLI emit every frame rather than a
/// summary (without it the process refuses the combination outright),
/// `--permission-prompt-tool stdio` routes a tool ask to this side as a
/// `can_use_tool` control request, `--permission-mode manual` is the spelling
/// `--help` lists for "ask me" (Deviation 10, user-confirmed), `--tools ""`
/// leaves the CLI none of its own so every tool the model sees is ganja's,
/// `--setting-sources ""` drops user, project and local settings,
/// `--strict-mcp-config` keeps a discovered `.mcp.json` out, and of the last
/// five, three turn off the CLI's own extras — slash commands, the browser,
/// the system-prompt snapshot — and two add frames: an `auth_status` at dial,
/// and every user message echoed back so a wire can see what the CLI
/// recorded.
pub const BASE: &[&str] = &[
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--permission-prompt-tool",
    "stdio",
    "--permission-mode",
    PERMISSION_MODE,
    "--tools",
    "",
    "--setting-sources",
    "",
    "--strict-mcp-config",
    "--disable-slash-commands",
    "--no-chrome",
    "--system-prompt-snapshot",
    "off",
    "--enable-auth-status",
    "--replay-user-messages",
];

/// The pair a conversation's argv carries when its turn runs under an effort.
///
/// `summarized` because it is the one display the recording measured making a
/// `thinking` block's text arrive (run 4); run 1 passed none and read an empty
/// string under a 1 148-character signature. Never on a one-shot or a
/// listing: a title and a summary read text and never reasoning, so the text
/// would be paid for and dropped.
pub const THINKING_DISPLAY: [&str; 2] = ["--thinking-display", "summarized"];

/// The value the token after `--permission-mode` must always be.
///
/// Asserted rather than merely built, and the assertion is stronger than
/// forbidding the three dangerous spellings: `auto` and `acceptEdits` are
/// choices somebody could make too, and this wire makes none of them — every
/// tool ask crosses ganja's own dialog.
pub const PERMISSION_MODE: &str = "manual";

/// Flags **no** argv this wire builds may carry, on either builder.
///
/// Twenty-eight tokens. Most are capabilities this wire has no use for and
/// would be answering for if it opened them — a second messaging socket, a
/// teammate identity, a remote control channel, a system prompt appended
/// behind ganja's back. Two are there because the recording measured them
/// and they lost:
///
/// - **`--resume`**: served on 2 of 8 turns, refused on 6 (**D556**). The
///   wire never resumes a record; every divergence opens a fresh one.
/// - **`--include-partial-messages`**: under it a completed block arrives
///   twice, as `stream_event` deltas and again as an `assistant` frame (M4),
///   so the reader decodes the `assistant` frame and the flag is not passed.
///
/// Both are forbidden rather than absent so that a builder growing one back
/// is refused here, at every builder's own never-list check, in every profile
/// — and again at the fake, which refuses `--resume` with the CLI's own
/// `unknown option`.
pub const NEVER_ANYWHERE: &[&str] = &[
    "--bare",
    "--continue",
    "--betas",
    "--max-turns",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--permission-prompts",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--exclude-dynamic-system-prompt-sections",
    "--sdk-url",
    "--ide",
    "--bg",
    "--cloud",
    "--remote",
    "--environment",
    "--remote-control",
    "--messaging-socket-path",
    "--team-name",
    "--agent-id",
    "--agent-name",
    "--teammate-mode",
    "--init-only",
    "--plan-mode-required",
    "--workload",
    "--fork-session",
    "--resume",
    "--include-partial-messages",
];

/// The one flag only a one-shot may carry, checked in addition to
/// [`NEVER_ANYWHERE`] against a held conversation's argv.
///
/// A held process is the continuity this wire has; telling it not to persist
/// its record would throw that away at the first turn.
pub const NEVER_ON_CONVERSATION: &[&str] = &["--no-session-persistence"];

/// Names removed from the child's environment, by name, without reading one.
///
/// **Eighty-seven**: the CLI's own 76-name `nSe` list — what a `claude`
/// strips from a child `claude`, and ganja's parent is often a `claude`
/// session — plus eleven the list does not carry and this wire will not
/// inherit. The whole `nSe` list rather than a curated subset because the
/// posture is "be the child `claude` expects", and a subset would be this
/// side deciding which of the vendor's own isolation the vendor did not mean.
///
/// The eleven: the two `ANTHROPIC_*` credentials, which would outrank the
/// CLI's login and bill a platform key silently; `NODE_OPTIONS`
/// and `DEBUG`, which change what the runtime does before any of ours runs;
/// and the seven `CLAUDE_CODE_*` names a ganja run under a `claude` session
/// inherits and that would tell the child it is something it is not.
///
/// `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_OAUTH_TOKEN` are on neither this list
/// nor [`SET`], and a test asserts it: the first is where the person's own
/// `claude` keeps its records and the second is the login this wire is
/// deliberately not holding — inheriting both is what makes the child the
/// user's own CLI rather than a second, differently-configured one.
pub const STRIP: &[&str] = &[
    // The eleven the CLI's own list does not carry.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "NODE_OPTIONS",
    "DEBUG",
    "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS",
    "CLAUDE_CODE_REMOTE",
    "CLAUDE_CODE_TEE_SDK_STDOUT",
    "CLAUDE_CODE_SESSION_KIND",
    "CLAUDE_CODE_SESSION_NAME",
    "CLAUDE_CODE_SESSION_LOG",
    "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
    // `nSe`, verbatim and in the bundle's own order (`w1a-trace.md` §7.1,
    // L575782 on 2.1.263). Its order carries no meaning and is kept anyway,
    // so a future re-read of the bundle diffs against this line by line.
    "CLAUDE_CODE_SAFE_MODE",
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_CODE_RESTRICTED",
    "CLAUDE_BG_POST_CLEAR_RESPAWN",
    "CLAUDE_CODE_RESUME_INTERRUPTED_TURN",
    "CLAUDE_CODE_RESUME_INTERRUPTED_TURN_MAX_AGE_MS",
    "CLAUDE_CODE_RESUME_PROMPT",
    "CLAUDE_CODE_QUESTION_PREVIEW_FORMAT",
    "CLAUDE_CODE_QUESTION_EXTENDED",
    "GITHUB_ACTIONS",
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_CHROME_MCP_ORG_DENIED",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_COWORK_FRAME_ARTIFACTS",
    "CLAUDE_CODE_SKILL_PROPOSALS",
    "CLAUDE_CODE_EVAL_INTERVIEW_SESSION",
    "CLAUDE_CODE_EVAL_ARTIFACT_STUB_DIR",
    "CLAUDE_CODE_EVAL_ALLOW_ARTIFACT_PUBLISH",
    "CLAUDE_CODE_EVAL_ALLOW_FLAG_OVERRIDES",
    "CLAUDE_CODE_EVAL_CONFINED",
    "CLAUDE_BG_RV_AUTH",
    "CLAUDE_BG_PTY_AUTH",
    "CLAUDE_BG_SOCKET_TOKENS_PATH",
    "CLAUDE_BG_ISOLATION",
    "CLAUDE_CODE_RESUME_SOURCE_ALIVE",
    "CLAUDE_CODE_COORDINATOR_MODE",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_AX_SCREEN_READER",
    "CLAUDE_CODE_SKIP_PROMPT_HISTORY",
    "ANTHROPIC_MODEL",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "__CFBundleIdentifier",
    "KITTY_WINDOW_ID",
    "WT_SESSION",
    "KONSOLE_VERSION",
    "VTE_VERSION",
    "ZED_TERM",
    "ZELLIJ",
    "TMUX",
    "TMUX_PANE",
    "CLAUDE_CODE_TMUX_SESSION",
    "CLAUDE_CODE_TMUX_PREFIX",
    "CLAUDE_CODE_TMUX_PREFIX_CONFLICTS",
    "STY",
    "CLAUDE_CODE_RELAUNCH_TERMINAL_SIZE",
    "LC_TERMINAL",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "COLORFGBG",
    "CURSOR_TRACE_ID",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "VSCODE_GIT_ASKPASS_MAIN",
    "VSCODE_GIT_ASKPASS_NODE",
    "VSCODE_GIT_ASKPASS_EXTRA_ARGS",
    "VSCODE_GIT_IPC_HANDLE",
    "TERMINAL_EMULATOR",
    "ITERM_SESSION_ID",
    "GNOME_TERMINAL_SERVICE",
    "XTERM_VERSION",
    "ALACRITTY_LOG",
    "TILIX_ID",
    "TERMINATOR_UUID",
    "ConEmuANSI",
    "ConEmuPID",
    "ConEmuTask",
    "MSYSTEM",
    "CLAUDE_CODE_SSE_PORT",
    "FORCE_CODE_TERMINAL",
];

/// Names set in the child's environment, in [`ChildEnv::apply`]'s own order.
///
/// Ten: two that say what is driving the CLI, seven that quiet a thing it
/// would otherwise do on its own — update itself under a held process, phone
/// home, ask for feedback, or rewrite the terminal's title, which is ganja's
/// title and not this child's — and one, [`BETAS`], that names the API betas
/// the CLI opts its requests into.
///
/// [`SET`] is applied **after** [`STRIP`], which matters for exactly one
/// name: `CLAUDE_CODE_QUESTION_PREVIEW_FORMAT` is on both lists — the CLI
/// strips it from a child and this wire then chooses its value — so the child
/// receives it set. That is the recording's own posture (its header lists the
/// name under both `SET (9)` and `REMOVED (87)`), and `argv_tests.rs` pins
/// that the overlap is that one name and no other, so a later widening of
/// either list cannot quietly create a second.
pub const SET: &[(&str, &str)] = &[
    ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
    ("CLAUDE_AGENT_SDK_CLIENT_APP", crate::auth::device::GANJA_USER_AGENT),
    ("DISABLE_AUTOUPDATER", "1"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
    ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
    ("CLAUDE_CODE_QUESTION_PREVIEW_FORMAT", "markdown"),
    ("ANTHROPIC_BETAS", BETAS),
];

/// The value of `ANTHROPIC_BETAS` on the child: the comma-separated
/// `anthropic-beta` header values the CLI sends on every request.
///
/// The CLI reads the name itself and appends these to the betas it would
/// send anyway, so the list is additive and a name it already sends is not a
/// conflict. Kept as one constant so a test can assert the child's value is
/// this string and no other, and so a beta is added or retired in exactly
/// one place. Each name appears once: the header is a set, and a repeat
/// would only make a diff of this list harder to read.
pub const BETAS: &str = "files-api-2025-04-14,\
context-1m-2025-08-07,\
web-fetch-2025-09-10,\
model-context-window-exceeded-2025-08-26,\
code-execution-2025-08-25,\
advanced-tool-use-2025-11-20,\
effort-2025-11-24,\
structured-outputs-2025-12-15,\
code-execution-web-tools-2026-02-09,\
fast-mode-2026-02-01,\
adaptive-thinking-2026-01-28,\
prompt-caching-scope-2026-01-05,\
token-efficient-tools-2026-03-28,\
managed-agents-2026-04-01,\
afk-mode-2026-01-31,\
cli-internal-2026-02-09,\
dreaming-2026-04-21,\
mid-conversation-system-2026-04-07,\
advisor-tool-2026-03-01,\
computer-use-2025-11-24,\
mcp-client-2025-11-20,\
compact-2026-01-12,\
context-management-2025-06-27,\
task-budgets-2026-03-13,\
thinking-token-count-2026-05-13,\
mid-conversation-tool-changes-2026-07-01,\
server-side-fallback-2026-06-01,\
server-side-fallback-2026-07-01,\
fallback-credit-2026-06-01,\
thinking-display-updates-2026-08-18,\
auto-mode-classifier-2026-07-16,\
mcp-tunnels-2026-06-22,\
per-turn-control-2026-07-01,\
mid-conversation-system-clear-at-2026-08-21,\
message-threads-2026-08-12,\
thinking-resumption-2026-07-17,\
timing-2026-09-09,\
environments-2025-11-01,\
oidc-federation-2026-04-01";

/// What a process that takes a turn is spawned with: a held conversation's,
/// or a title's or a compaction summary's one-shot, told apart by which
/// builder it is handed to.
///
/// It carries **no resume variant**, which is the held process's never-resume
/// rule expressed as a type: there is no value of this struct that names a
/// record to continue, so the arm that would have built `--resume` cannot be
/// written by accident.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    /// The record this process opens, minted fresh every time.
    pub session_id: String,
    /// The model the request asked for, or the wire's default.
    pub model: String,
    /// The catalog effort's name, when this turn runs under one.
    pub effort: Option<String>,
}

// The three builders are pure: they read nothing, spawn nothing and allocate a
// `Vec` from their argument. All three check their own never-list before
// returning, in **every** profile — so a forbidden token is refused at the
// builder rather than handed to the child.

/// The argv a held conversation's process is spawned with.
///
/// # Errors
///
/// Returns a [`ProviderError::Transport`] naming the token when this argv
/// would carry a forbidden flag: one of [`NEVER_ANYWHERE`] or of
/// [`NEVER_ON_CONVERSATION`], matched by its `--flag` up to `=`.
pub fn conversation(spawn: &Spawn) -> Result<Vec<OsString>, ProviderError> {
    let mut argv = base();
    argv.push("--session-id".into());
    argv.push(spawn.session_id.as_str().into());
    push_model_and_effort(&mut argv, &spawn.model, spawn.effort.as_deref());

    // Readable thinking, asked for only by a turn that asked for reasoning.
    // The one such ask a `ChatRequest` carries on this wire is the effort —
    // `effort_options` is what `/effort` splices, and this wire has no other
    // reasoning field — so an effort is the signal and nothing else is.
    // Without the flag the CLI withholds the text under a full signature (run
    // 1); with it the text arrives (run 4). Thinking is billed and this wire
    // has no catalog row to price it with, which is why it is not passed to a
    // turn that never asked (**D556**, `fd5v`).
    if spawn.effort.is_some() {
        argv.extend(THINKING_DISPLAY.iter().map(OsString::from));
    }

    checked(argv, NEVER_ON_CONVERSATION)
}

/// The argv a listing process is spawned with (**D556**, Dv-17).
///
/// A one-shot's shape without `--model` or `--effort`: the same fresh
/// `--session-id` and the same `--no-session-persistence`, because a
/// listing's record is worth nothing the moment its answer is read. It names
/// no model because a listing asks the CLI what models the seat may name,
/// and naming one would be asking the question with the answer already in
/// it; and no effort, a property of a turn this process never takes. Here
/// rather than at the caller so that every rule about what may appear on this
/// wire's command line stays in one file.
///
/// # Errors
///
/// Returns a [`ProviderError::Transport`] naming the token when this argv
/// would carry one of [`NEVER_ANYWHERE`], matched by its `--flag` up to `=`.
pub fn listing(session_id: &str) -> Result<Vec<OsString>, ProviderError> {
    let mut argv = base();
    argv.push("--session-id".into());
    argv.push(session_id.into());
    argv.push("--no-session-persistence".into());

    checked(argv, &[])
}

/// The argv a one-shot's process is spawned with: this process answers one
/// request and its record is worth nothing afterwards.
///
/// # Errors
///
/// Returns a [`ProviderError::Transport`] naming the token when this argv
/// would carry one of [`NEVER_ANYWHERE`], matched by its `--flag` up to `=`.
pub fn one_shot(spawn: &Spawn) -> Result<Vec<OsString>, ProviderError> {
    let mut argv = base();
    argv.push("--session-id".into());
    argv.push(spawn.session_id.as_str().into());
    argv.push("--no-session-persistence".into());
    push_model_and_effort(&mut argv, &spawn.model, spawn.effort.as_deref());

    checked(argv, &[])
}

/// `argv`, unless it carries a flag this wire may not pass.
///
/// A refusal rather than the `debug_assert!` the three builders used to end on:
/// the macro expands to nothing in a release build, so the list was enforced
/// by the builders' own shape and by the fake CLI and **not** by the shipped
/// binary (CC-4). A real `assert!` would be the wrong direction too. The two
/// free-form values that reach this command line are `model` and `effort`, and
/// both arrive from outside this crate — a checked-in `ganja.toml`'s
/// `model` key, `GANJA_MODEL`, a `POST /session/{id}/model` that validates
/// nothing — so a panic here would be a person crashing the wire by typing a
/// flag where a model goes. A refusal naming the token is what a value from
/// outside earns.
///
/// # Errors
///
/// Returns a [`ProviderError::Transport`] naming the offending token.
fn checked(argv: Vec<OsString>, never: &[&str]) -> Result<Vec<OsString>, ProviderError> {
    match forbidden(&argv, never) {
        Some(token) => Err(ProviderError::Transport(format!(
            "this wire never passes `{token}` to the claude CLI"
        ))),
        None => Ok(argv),
    }
}

/// [`BASE`] as an owned argv.
fn base() -> Vec<OsString> {
    BASE.iter().map(OsString::from).collect()
}

/// `--model` unless the model is the wire's own default — which is the CLI's
/// own word for "whatever you would choose", so passing it would replace the
/// vendor's choice with a literal — and `--effort` when this turn runs under
/// a catalog effort.
fn push_model_and_effort(argv: &mut Vec<OsString>, model: &str, effort: Option<&str>) {
    if model != super::DEFAULT_MODEL {
        argv.push("--model".into());
        argv.push(model.into());
    }

    if let Some(effort) = effort {
        argv.push("--effort".into());
        argv.push(effort.into());
    }
}

/// The first token of `argv` that appears on `never` or on
/// [`NEVER_ANYWHERE`], or [`None`].
///
/// `never` is the *additional* list, so a caller passes only what its own
/// builder forbids and the wide list is always checked.
///
/// A token is judged by its **flag**, which is everything before the first
/// `=`: `--sdk-url=https://…` carries both flag and value in one word, and
/// comparing whole tokens put every joined spelling outside the list (CC-4).
/// The whole token is what comes back, so a refusal quotes what it saw.
#[must_use]
pub fn forbidden(argv: &[OsString], never: &[&str]) -> Option<String> {
    argv.iter().find_map(|token| {
        let token = token.to_string_lossy();
        let listed = {
            let flag = token.split('=').next().unwrap_or(token.as_ref());

            NEVER_ANYWHERE.contains(&flag) || never.contains(&flag)
        };

        listed.then(|| token.into_owned())
    })
}

/// The environment and working directory one child is spawned under.
///
/// The cwd travels here rather than as a second parameter of the spawn seam
/// so that "what this child sees of the machine" is one value: a fake records
/// it beside the argv, and a test reads both off one side file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildEnv {
    /// The empty scratch directory this child runs in.
    ///
    /// **Never the project root and never `.`**: run 6 measured a checkout as
    /// cwd at 7 348 extra prefix tokens over the identical request in an
    /// empty directory, not itemised by any frame and not suppressed by
    /// `--setting-sources ""` — the CLI derives its own project memory from
    /// its cwd. A CLI under `--tools ""` has no use for a working directory,
    /// and ganja's own tools run in ganja's own cwd.
    pub cwd: PathBuf,
}

impl ChildEnv {
    /// Puts this posture on `command`: [`STRIP`] removed by name, then
    /// [`SET`] set, then the cwd.
    ///
    /// Never `env_clear` — the child is meant to be the user's own `claude`,
    /// which needs its `PATH`, its `HOME` and its `CLAUDE_CONFIG_DIR` — and
    /// never a read of any value.
    pub fn apply(&self, command: &mut tokio::process::Command) {
        for name in STRIP {
            command.env_remove(name);
        }
        for (name, value) in SET {
            command.env(name, value);
        }

        command.current_dir(&self.cwd);
    }
}

#[cfg(test)]
#[path = "argv_tests.rs"]
mod tests;
