//! `ganja run` — one turn, headless, and then the process is over.
//!
//! Spec: upstream `packages/opencode/src/cli/cmd/run.ts`, its non-interactive
//! branch (`run.ts:828-872`). Upstream's `--mini` split-footer interactive
//! branch is out of scope — ganja's interactive mode *is* the TUI (deviation:
//! run-drives-the-engine-directly). What is left is exactly what a script
//! wants: a message in, an ordered account of the turn out, and an exit code
//! that says whether it worked.
//!
//! The engine here is the same [`Engine`] the TUI drives, assembled the same
//! way and in the same order — upstream reaches its own engine through a
//! loopback HTTP client, and this build reaches it through the call it already
//! had. **`--attach` is the other half of that sentence**: given the address of
//! a running `ganja serve`, this command drives that engine instead, over the
//! four routes `ganja-client` wraps (`run.ts:938-941`, which swaps its SDK for
//! one bound to the remote base URL and otherwise runs the same loop). The
//! account of the turn is written by the same [`Reporter`] either way, which is
//! how the two transcripts stay identical rather than merely similar —
//! `ganja-cli/tests/attach.rs` holds them against each other frame for frame.
//!
//! Every observable rule of the loop below is upstream's:
//!
//! * **Subscribe before prompting.** Upstream's `client.event.subscribe()`
//!   precedes `client.session.prompt()` (`run.ts:829,859`); so does this. In
//!   ganja the queue exists from construction and is lossless, so a late
//!   subscriber loses nothing — it *wedges*, once the turn fills the queue
//!   nobody is draining. The ordering is therefore a liveness rule here rather
//!   than a correctness one, and it is kept for the same reason either way.
//! * **The session id is a local** (`run.ts:676`, read at `:684`), captured
//!   once and stamped on every emitted object. Upstream additionally filters
//!   other sessions out before emitting (`run.ts:717,790,798`); this build has
//!   nothing to filter, because a subagent's events never reach the subscribed
//!   stream at all — they go to a private channel, which
//!   `ganja-core/tests/task.rs` pins (deviation:
//!   run-needs-no-session-filter).
//! * **Nothing here waits on a person.** A permission request is answered the
//!   moment it arrives: `once` under `--auto`, and otherwise a warning and a
//!   `reject` (`run.ts:800-815`). A headless run that opened a dialog would
//!   hang until it was killed. An attached run answers the same way over
//!   `POST /permission/{id}/reply`, and refuses [`REFUSED`] there itself: a
//!   `serve` engine deliberately keeps its dialogs interactive, so the rules
//!   that make a headless turn safe have to be applied where the person is
//!   absent — here — rather than on a server that is not this run's to
//!   reconfigure (deviation: an-attached-run-refuses-at-the-client).
//! * **Payload on stdout, diagnostics on stderr**, as every other subcommand
//!   here does it. Upstream mixes its warnings into stdout; doing that would
//!   corrupt `--format json`'s stream, so a warning and an error both go to
//!   stderr and an nd-JSON `error` object is emitted *beside* the stderr line
//!   rather than instead of it (deviation: run-reports-errors-on-both-channels).
//! * **An attached run does not invoke skills.** A local run scans its prompt
//!   for `$name` tokens (**D491**); `--attach` sends the text literal, because
//!   `ganja_client::Prompt` carries no `skills` field and the roster to
//!   validate a token against lives on the server this client did not
//!   assemble. The wire is ready — serve's prompt route accepts `skills` —
//!   so this is a recorded gap, not a stance; `ganja-code-80g` tracks it
//!   (deviation: an-attached-run-sends-dollar-tokens-literal).

use std::{
    io::{self, IsTerminal as _, Read as _, Write},
    path::PathBuf,
};

use anyhow::{Context as _, Result, bail};
use clap::{Args, ValueEnum};
use futures::StreamExt as _;
use ganja_client::Prompt;
use ganja_core::{Engine, EngineError, SessionId, config::Overrides};
use ganja_permission::permission;
use ganja_protocol::{
    Command as EngineCommand, Event, FinishReason, Message, Part, PartBody, PartId,
    PermissionReply, Role, ToolState,
};
use secrecy::ExposeSecret as _;
use serde_json::Value;

use crate::{
    assemble::{Assembled, assemble},
    millis_now, printable,
};

/// Every `type` an nd-JSON object may carry: upstream's six and no seventh
/// (`run.ts:720`, `:741`, `:745`, `:749`, `:762`, `:784`).
///
/// All six have a source. `reasoning` was the one that did not: this build had
/// no readable-thinking part, and the slot was held open against the day it
/// grew one, "a build that later grows the part must fill this slot rather
/// than invent a seventh". `PartBody::ReasoningText` is that part and this is
/// that filling, so the `run-emits-no-reasoning` deviation is retired.
///
/// **`--format json` only.** The readable stream is the answer a person asked
/// for; thinking is the model's way to it and stays out of that stream, where
/// it would run together with the reply it precedes. A consumer parsing types
/// is told the difference and a reader watching stdout is not, so only the one
/// that can tell them apart is given both.
const TYPES: [&str; 6] = [
    "tool_use",
    "step_start",
    "step_finish",
    "text",
    "reasoning",
    "error",
];

/// The entries of [`TYPES`] this build emits, discriminants being indexes into
/// it. There is no gap left: every name in the set has a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    ToolUse = 0,
    StepStart = 1,
    StepFinish = 2,
    Text = 3,
    Reasoning = 4,
    Error = 5,
}

impl Kind {
    /// What this kind is called on the wire.
    fn as_str(self) -> &'static str {
        TYPES[self as usize]
    }
}

/// The permissions a non-interactive turn refuses outright, at every pattern.
///
/// Upstream's non-interactive ruleset (`run.ts:430-448`), ported verbatim —
/// originally ahead of the tools it names, so that `question` would be safe
/// in `run` by construction rather than by whoever added it remembering. All
/// three have since landed — `question`, then `plan_exit`, and now
/// `plan_enter` (**D477**) — and the construction held for every one of them:
/// this list needed no edit as each arrived, which is the whole argument for
/// writing a refusal ahead of the thing it refuses. The set is still
/// upstream's own, `plan_enter` included: `run.ts:439` denies it headless
/// there too, so ganja's synthesized door inherits the same posture as the
/// ported one.
///
/// Two consumers, because a run reaches its engine two ways:
/// [`refuse_interactive_permissions`] installs them as standing rules on an
/// engine this process owns, and the attached loop applies the same set to the
/// dialogs a remote engine sends. Both refuse them **even under `--auto`** —
/// `--auto` answers the dialogs a person would have answered, and these are
/// the ones no answer from a script can mean anything for.
const REFUSED: [&str; 3] = ["question", "plan_enter", "plan_exit"];

/// How long a finished run waits for the turn's own tail before it ends the
/// session and exits.
///
/// Comfortably past the default hook budget, because what is being waited for
/// is at most one `Stop` hook of somebody's own, and each of those is already
/// bounded by [`ganja_core::hook::DEFAULT_TIMEOUT`] or by whatever its entry
/// asked for. A run that waits this long and still finds a turn in flight
/// leaves it: an exit code is what a script is here for.
const SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(90);

/// What a completed tool call is marked with in the default format, matching
/// upstream's fallback glyph (`run.ts:103`).
const RAN: char = '\u{2699}';

/// What a failed one is marked with (`run.ts:113`).
const FAILED: char = '\u{2717}';

/// How the account of a turn is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// One readable line per thing that happened.
    #[default]
    Default,
    /// Newline-delimited JSON, one object per event, for a script to parse.
    Json,
}

/// `ganja run`'s flags.
///
/// A subset of upstream's, and deliberately so: `--mini`, `--interactive`,
/// `--demo` and `--replay*` are about the interactive branch this build does
/// not have, and `--share`, `--file`, `--title` and `--thinking` name features
/// ganja has no surface for — a session is titled by its first completed turn,
/// there is no share endpoint, and `--thinking` is a *request-side* knob this
/// build does not have: what a model was asked to think is the provider's to
/// decide, and what it did think this run already emits as
/// `PartBody::ReasoningText` under `--format json` (deviation:
/// run-carries-the-flags-ganja-can-honor). A flag that parsed and then did
/// nothing would be worse than one that is absent. `--effort` left that list
/// when catalog efforts landed: it now selects one for the turn, validated
/// against the same catalog row the interactive `/effort` picker reads.
///
/// `--attach` is here now; the three upstream flags that travel with it are
/// not, and each for its own reason. `--port` is upstream's *other* attach
/// spelling (a port on localhost) and says nothing an address does not.
/// `--password` and `--username` are read from the environment instead —
/// `GANJA_SERVER_PASSWORD` and `GANJA_SERVER_USERNAME`, the same two variables
/// the server was started with — because a password in `argv` is readable by
/// every process on the machine, and a flag that leaks the credential it
/// protects is worse than no flag (deviation:
/// attach-reads-the-password-from-the-environment).
#[derive(Debug, Args)]
pub struct RunArgs {
    /// What to ask. Every argument is one word of it; they are joined with
    /// spaces, and anything after `--` counts too.
    #[arg(value_name = "MESSAGE")]
    message: Vec<String>,
    /// Run this slash command instead, with the message as its arguments.
    #[arg(long, value_name = "NAME")]
    command: Option<String>,
    /// Continue this project's most recent root session.
    // Refused together with `--session` for the reason the UI's pair is: the
    // two name different sessions, and picking a winner would be inventing an
    // answer. Upstream lets `--session` quietly win.
    #[arg(long, short = 'c', conflicts_with = "session")]
    r#continue: bool,
    /// Continue the session with this id, as `ganja sessions` lists it.
    #[arg(long, short = 's', value_name = "ID")]
    session: Option<String>,
    /// Fork the session before continuing (requires --continue or --session).
    #[arg(long)]
    fork: bool,
    /// Ask this model, spelled "provider/model" the way the config's `model` key is.
    #[arg(long, value_name = "PROVIDER/MODEL")]
    model: Option<String>,
    /// Run as this agent instead of the roster's default.
    #[arg(long, value_name = "NAME")]
    agent: Option<String>,
    /// Run the model under this catalog effort, as `/effort` lists them.
    // Refused together with `--attach`: the client's surface carries no
    // effort route, and a flag that parsed and then decided nothing is the
    // one thing this table refuses to hold.
    #[arg(long, value_name = "NAME", conflicts_with = "attach")]
    effort: Option<String>,
    /// Merge exactly this config file, outranking `GANJA_CONFIG` and discovery.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Drive a running `ganja serve` at this address instead of an engine in
    /// this process, e.g. http://127.0.0.1:4096.
    // Refused together with `--config`, which configures an engine this
    // process no longer builds, and with `--command`, whose route is not in
    // the attached client's surface: either would parse and then decide
    // nothing, which is the one thing this flag table refuses to do.
    #[arg(long, value_name = "URL", conflicts_with_all = ["config", "command"])]
    attach: Option<String>,
    /// How to write the account of the turn.
    #[arg(long, value_enum, default_value_t = Format::Default)]
    format: Format,
    /// Allow every permission request this turn instead of refusing it.
    ///
    /// Dangerous, and the reason it is spelled out: nobody is watching, so an
    /// "allow" here is an allow for whatever the model decided to run.
    #[arg(long)]
    auto: bool,
    // Two hidden spellings of `--auto`, carried because upstream carries them
    // and scripts written against it pass them (`run.ts:247-256`).
    #[arg(long, hide = true)]
    yolo: bool,
    #[arg(long = "dangerously-skip-permissions", hide = true)]
    dangerously_skip_permissions: bool,
}

/// Runs one turn and reports it, returning when the turn is over.
///
/// # Errors
///
/// Exit 1 for every one of upstream's refusals — an empty message with no
/// `--command` (`run.ts:420-423`), a `--fork` with nothing to fork
/// (`:425-428`), a `--session` the store does not hold (`:465-467`), a prompt
/// the engine would not accept (`:867-869`) and a turn that streamed an error
/// (`:836-838`) — and for a configuration, provider or storage failure that
/// leaves nothing to run.
pub async fn run(args: RunArgs) -> Result<()> {
    let auto = args.auto || args.yolo || args.dangerously_skip_permissions;
    let message = resolve_input(&args.message.join(" "), &piped_stdin()?);

    // Upstream's order, and it decides which refusal a run with two problems
    // reports: the message is checked first (`run.ts:420`), the fork second
    // (`:425`).
    if message.trim().is_empty() && args.command.is_none() {
        bail!("You must provide a message or a command");
    }
    if args.fork && !args.r#continue && args.session.is_none() {
        bail!("--fork requires --continue or --session");
    }
    // The validation above is upstream's and is worth keeping whole, because
    // it is the half that says what `--fork` *means*. The fork itself is not
    // portable: nothing in `ganja-core` copies a session, so the honest thing
    // is to refuse loudly here rather than continue into a run that silently
    // wrote to the session it was asked to leave alone (deviation:
    // run-cannot-fork).
    if args.fork {
        bail!("--fork is not available in this build: nothing here copies a session");
    }

    // Everything above is about the message and is true wherever the turn
    // runs; everything below assembles an engine, which an attached run does
    // not have and does not want.
    if let Some(address) = args.attach {
        return attached(Attached {
            address,
            message,
            resume_latest: args.r#continue,
            session: args.session,
            agent: args.agent,
            model: args.model,
            auto,
            format: args.format,
        })
        .await;
    }

    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let assembled = assemble(
        &cwd,
        &Overrides {
            model: args.model,
            agent: args.agent,
            config_file: args.config,
        },
    )?;
    let Assembled {
        engine,
        servers,
        config,
        ..
    } = assembled;
    refuse_interactive_permissions(&engine);
    // Dialled in the background, exactly as the UI dials them: a server that
    // never answers costs its tools rather than the run. No file watcher
    // beside it, though — the stale-read notice it feeds is delivered at the
    // top of a *later* turn, and a one-shot run has no later turn (deviation:
    // run-does-not-watch-files).
    engine.connect_mcp();
    // The same opening a screen gives a session, because the engine owns hooks
    // and a headless run is a session too. Before `select_session`, so a
    // `--continue` fires `startup` here and `resume` from inside the resume
    // itself, in that order.
    engine.session_start().await;

    let session = select_session(&engine, args.r#continue, args.session.as_deref()).await?;
    // After the session for the same reason the flag is, and before it because
    // it yields to both: a resumed row's own effort is already on the engine
    // here, and this seeds only a session that carries none.
    engine.seed_effort(config.effort).await;
    // After the session, so the flag outranks whatever effort a resumed row
    // restored; before the turn, so a bad name is this refusal — listing the
    // model's real names — and never a request built around it.
    let outcome = match effort_switch(&engine, args.effort).await {
        Ok(()) => {
            drive(
                &engine,
                session,
                &message,
                args.command.as_deref(),
                auto,
                args.format,
            )
            .await
        }
        Err(refusal) => Err(refusal),
    };

    // Every local MCP server's process group ends here, whichever way the turn
    // went, and before the refusal below leaves the function.
    // The turn's own tail — the `Stop` hook above all — runs *after* the finish
    // event this loop stopped reading at, so a run that ended the session here
    // would end it while the turn it just watched was still finishing. Bounded:
    // a hook has its own timeout, and a script must be able to exit.
    engine.settle(SETTLE_LIMIT).await;
    engine.session_end(ganja_core::hook::EXIT_REASON).await;
    servers.shutdown().await;
    engine.shutdown_lsp();
    engine.shutdown_jobs().await;

    match outcome? {
        None => Ok(()),
        Some(error) => bail!("{error}"),
    }
}

/// Applies `--effort` to the engine before the turn, or does nothing when
/// the flag was not given.
///
/// The refusal is the engine's own — the catalog row's real names for a wrong
/// one, the no-catalog sentence for a provider without rows — surfaced
/// verbatim as this run's exit-1 message.
async fn effort_switch(engine: &Engine, effort: Option<String>) -> Result<()> {
    if effort.is_some() {
        engine.send(EngineCommand::SwitchEffort { effort }).await?;
    }

    Ok(())
}

/// Installs [`REFUSED`] as standing rules the engine re-applies itself.
///
/// Standing rules survive every baseline recomposition — an agent switch, a
/// `--command` naming its own agent, a resume, and the tool-set rebuild a
/// finishing MCP dial triggers — because the engine appends them after the
/// agent's own rules inside the one place a baseline is composed. That is
/// where last-match-wins needs them: a config that allowed `question` must
/// not outrank the refusal that makes a headless run safe, and no later
/// recomposition may quietly drop it.
fn refuse_interactive_permissions(engine: &Engine) {
    engine.append_standing_rules(
        REFUSED
            .iter()
            .map(|permission| permission::Rule {
                permission: (*permission).to_owned(),
                pattern: "*".to_owned(),
                action: permission::Action::Deny,
            })
            .collect(),
    );
}

/// What a run needs once `--attach` has taken the engine out of this process.
///
/// A struct rather than eight arguments, and a *narrow* one: what is missing
/// from it is the point. There is no config, no provider, no tool registry and
/// no storage here, because every one of those belongs to the server — this
/// process asks and renders, and nothing else.
struct Attached {
    address: String,
    message: String,
    resume_latest: bool,
    session: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    auto: bool,
    format: Format,
}

/// Runs one turn on a server somebody else is running.
///
/// Upstream's attach branch (`run.ts:938-941`) rebinds its SDK to the remote
/// base URL and re-enters the same `execute`; this does the same with
/// [`ganja_client::Client`] in place of the engine. One divergence is
/// deliberate: upstream's `finish()` returns early when attached
/// (`run.ts:835`), so an attached run does not take the turn's failure into its
/// exit code. Here it does — an exit code that means "the turn worked" locally
/// and "the request was accepted" remotely would be a trap for the one thing
/// this command exists for, which is scripts (deviation:
/// an-attached-run-exits-on-the-turn-not-the-request).
async fn attached(run: Attached) -> Result<()> {
    // The credential the *server* configured, read through the server's own
    // resolver so the two cannot disagree about which variables mean what.
    let credentials = ganja_serve::Credentials::from_env().map(|configured| {
        ganja_client::Credentials::new(configured.username, configured.password.expose_secret())
    });
    let client = ganja_client::Client::new(&run.address, credentials)?;

    // Nothing is prompted before something answers: a mistyped address, a
    // server that is not running and a password that is wrong are one sentence
    // here instead of a failure three calls later.
    client.health().await?;

    let session = remote_session(&client, run.resume_latest, run.session.as_deref()).await?;

    match drive_attached(&client, &session, &run).await? {
        None => Ok(()),
        Some(error) => bail!("{error}"),
    }
}

/// The session an attached run drives.
///
/// Upstream resolves the same three cases through the attached SDK
/// (`run.ts:456-533`): the session that was named, the newest root under
/// `--continue`, or a fresh one. A named session is checked against the
/// listing before anything is printed, so the refusal keeps both its wording
/// and its place in the order the local path refuses in.
async fn remote_session(
    client: &ganja_client::Client,
    resume_latest: bool,
    named: Option<&str>,
) -> Result<SessionId> {
    if let Some(named) = named {
        let wanted = SessionId::from(named.to_owned());
        if !client.sessions().await?.iter().any(|row| row.id == wanted) {
            // Upstream's wording, because a script that greps for it is
            // greping for upstream's (`run.ts:465`).
            bail!("Session not found");
        }

        return Ok(wanted);
    }
    // A `--continue` with no root falls through to a fresh session exactly as
    // the local path does; the server mints it.
    if resume_latest
        && let Some(root) = client
            .sessions()
            .await?
            .into_iter()
            .find(|row| row.parent.is_none())
    {
        return Ok(root.id);
    }

    Ok(client.create_session().await?)
}

/// Subscribes, prompts, and reports the turn until it ends — [`drive`] with
/// the four routes in place of the four engine calls.
///
/// The same [`Reporter`], deliberately: the account of a turn is one format,
/// and two writers of it would drift on the first change to either.
async fn drive_attached(
    client: &ganja_client::Client,
    session: &SessionId,
    run: &Attached,
) -> Result<Option<String>> {
    // Before the prompt, always — and here the ordering is a correctness rule
    // rather than the liveness one it is locally: the server's subscription is
    // registered when this returns, and a prompt sent first could stream its
    // opening events into a stream nobody had opened yet.
    let mut events = client.events().await?;

    let started = client
        .prompt(
            session,
            &Prompt::new(run.message.clone())
                .as_agent(run.agent.clone())
                .asking(run.model.clone()),
        )
        .await;

    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut reporter = Reporter::new(run.format, session.as_str().to_owned(), &mut out, &mut err);

    if let Err(error) = started {
        reporter.failed(&error.to_string());

        return reporter.finish();
    }

    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            // An eviction or a wire this build cannot read ends the stream;
            // reporting it as a failed turn is honest, because a transcript
            // that stopped early is exactly what the caller has.
            Err(error) => {
                reporter.failed(&error.to_string());
                break;
            }
        };

        if let Event::PermissionRequested {
            id, tool, title, ..
        } = &event
        {
            let reply = if run.auto && !REFUSED.contains(&tool.as_str()) {
                PermissionReply::Once
            } else {
                reporter.rejecting(tool, title);
                PermissionReply::Reject
            };
            // A reply nothing is waiting for is defined to be ignored, which
            // is what a reply racing a cancelled turn becomes.
            let _ = client.reply_permission(id, reply).await;

            continue;
        }
        if reporter.apply(&event, run.agent.as_deref()) {
            break;
        }
    }

    reporter.finish()
}

/// Installs the session this run continues, if it continues one.
///
/// Upstream's `session()` (`run.ts:456-533`) minus the fork branches. A
/// `--continue` that finds no root session falls through to a fresh one, which
/// is upstream's behaviour when its listing has no parentless entry
/// (`run.ts:492`, `:510`): the first prompt then mints the session.
async fn select_session(
    engine: &Engine,
    resume_latest: bool,
    named: Option<&str>,
) -> Result<Option<SessionId>> {
    if let Some(named) = named {
        let id = SessionId::from(named.to_owned());

        return match engine.resume(&id).await {
            Ok(_) => Ok(Some(id)),
            // Upstream's wording, because a script that greps for it is
            // greping for upstream's (`run.ts:465`).
            Err(EngineError::SessionNotFound { .. }) => bail!("Session not found"),
            Err(error) => Err(error).context("failed to resume the session"),
        };
    }
    if !resume_latest {
        return Ok(None);
    }

    let roots = engine
        .sessions()
        .await
        .context("failed to list the stored sessions")?;
    // Roots only, as `ganja sessions` lists them: a session carrying a parent
    // belongs to the `task` call that spawned it.
    let Some(root) = roots.into_iter().find(|session| session.parent.is_none()) else {
        return Ok(None);
    };
    engine
        .resume(&root.id)
        .await
        .context("failed to resume the session")?;

    Ok(Some(root.id))
}

/// Subscribes, prompts, and reports the turn until it ends.
///
/// Answers with what the turn failed with, or [`None`] when it did not — the
/// caller turns that into the exit code after it has closed everything down.
async fn drive(
    engine: &Engine,
    session: Option<SessionId>,
    message: &str,
    command: Option<&str>,
    auto: bool,
    format: Format,
) -> Result<Option<String>> {
    // Before the prompt, always. See the module documentation.
    let mut events = engine
        .subscribe()
        .await
        .context("failed to subscribe to the engine")?;

    let started = match command {
        Some(name) => {
            engine
                .send(EngineCommand::RunCommand {
                    name: name.to_owned(),
                    args: message.to_owned(),
                })
                .await
        }
        None => {
            // The composer's `$name` grammar works headless too: the scan is
            // the same `requested_in` the TUI submits through, over the same
            // roots the engine will expand from, so a token nothing answers
            // to stays literal here exactly as it does on a screen.
            let skills = {
                let roots = engine.skill_roots();
                ganja_core::tool::skill::requested_in(
                    message,
                    &ganja_core::tool::skill::discover(&roots),
                )
            };
            engine
                .send(EngineCommand::SendPrompt {
                    text: message.to_owned(),
                    mentions: Vec::new(),
                    skills,
                })
                .await
        }
    };

    // Read *after* the prompt and not off an event: a fresh session is minted
    // synchronously inside the send (`engine.rs:1649-1666`), so by the time it
    // returns the id exists, and it is the id every emitted object carries for
    // the rest of the run.
    let id = session
        .map(|id| id.as_str().to_owned())
        .or_else(|| {
            engine
                .current_session()
                .map(|info| info.id.as_str().to_owned())
        })
        .unwrap_or_default();

    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut reporter = Reporter::new(format, id, &mut out, &mut err).with_effort(engine.effort());

    if let Err(error) = started {
        // Upstream reports a refused prompt and stops without waiting on a
        // turn that never began (`run.ts:866-870`). `failed` records what it
        // reported, so `finish` answers with exactly this error — there is no
        // fallback here, because a fallback would be a branch nothing can
        // reach and a silent exit 0 if that ever stopped being true.
        reporter.failed(&error.to_string());

        return reporter.finish();
    }

    let agent = engine.agent();
    while let Some(event) = events.next().await {
        // Answered here rather than in the reporter, because answering is a
        // command and the reporter only writes.
        if let Event::PermissionRequested {
            id, tool, title, ..
        } = &event
        {
            let reply = if auto {
                PermissionReply::Once
            } else {
                reporter.rejecting(tool, title);
                PermissionReply::Reject
            };
            // A reply nothing is waiting for is defined to be ignored, which
            // is what a reply racing a cancelled turn becomes.
            let _ = engine
                .send(EngineCommand::ReplyPermission {
                    id: id.clone(),
                    reply,
                })
                .await;

            continue;
        }
        if reporter.apply(&event, agent.as_deref()) {
            break;
        }
    }

    reporter.finish()
}

/// Writes the account of a turn, in whichever format was asked for.
///
/// Both writers are borrowed rather than resolved here so the whole of what
/// this build emits is exercisable without a process: what an nd-JSON line
/// says, and which channel a warning lands on, are the two things `run` has to
/// get right.
struct Reporter<'a> {
    format: Format,
    /// The run's own session, stamped on every emitted object.
    session: String,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    /// Whether the `> agent · model` header has been written.
    announced: bool,
    /// The effort the turn runs under, which the readable header carries
    /// beside the model. [`None`] — every run before `--effort`, every json
    /// run — leaves the header exactly what it always was.
    effort: Option<String>,
    /// Text parts still streaming, in the order they opened. A text part has
    /// no completion event of its own — the step's `StepFinish` marker is what
    /// closes it (`session.rs`, "writing it also flushes the step's text
    /// part") — so the text is accumulated here and written when the step
    /// ends.
    open: Vec<(PartId, String, Kind)>,
    /// What the turn failed with, if it did.
    failure: Option<String>,
}

impl<'a> Reporter<'a> {
    fn new(
        format: Format,
        session: String,
        out: &'a mut dyn Write,
        err: &'a mut dyn Write,
    ) -> Self {
        Self {
            format,
            session,
            out,
            err,
            announced: false,
            effort: None,
            open: Vec::new(),
            failure: None,
        }
    }

    /// Names the effort the readable header carries beside the model.
    fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort;

        self
    }

    /// Applies one event, answering whether the turn is over.
    fn apply(&mut self, event: &Event, agent: Option<&str>) -> bool {
        match event {
            // The event's own session is deliberately unread: the id this run
            // stamps on every emitted object is the local it captured before
            // the first event, which is the contract the format hangs on.
            Event::MessageStarted {
                session_id: _,
                message,
            } => self.announce(message, agent),
            Event::PartStarted { part, .. } => match &part.body {
                // Both are opened empty and grown by the deltas below, and
                // both are written when the step closes; which of the two a
                // row is decides the name it is emitted under.
                PartBody::Text { text } => {
                    self.open.push((part.id.clone(), text.clone(), Kind::Text));
                }
                PartBody::ReasoningText { text } => {
                    self.open
                        .push((part.id.clone(), text.clone(), Kind::Reasoning));
                }
                PartBody::StepStart => self.emit(Kind::StepStart, "part", part),
                // The marker that closes the step, so the step's text is
                // written first and this second.
                PartBody::StepFinish { .. } => {
                    self.flush();
                    self.emit(Kind::StepFinish, "part", part);
                }
                // A sealed blob is not thinking a reader could be shown, so it
                // is emitted under no name at all: saying `reasoning` of it
                // would tell a consumer the model said something printable.
                // A tool the *provider* ran (**D489**) arrives finished and
                // is never updated, so it is written here rather than from
                // `updated` — under the tool kind that already exists, because
                // it is an account of a tool that ran and a seventh name would
                // say it is something else. What kind of tool it was is in the
                // part's own body, where a consumer reading the payload finds
                // the `openrouter:` name it carries.
                PartBody::ServerTool { tool, .. } => {
                    self.flush();
                    self.emit(Kind::ToolUse, "part", part);
                    if self.format == Format::Default {
                        let _ = writeln!(self.out, "{RAN} {}", printable(tool));
                    }
                }
                PartBody::Tool { .. }
                | PartBody::File { .. }
                | PartBody::Patch { .. }
                | PartBody::Reasoning { .. } => {}
            },
            Event::PartDelta { part_id, delta, .. } => {
                if let Some((_, text, _)) = self.open.iter_mut().find(|(id, ..)| id == part_id) {
                    text.push_str(delta);
                }
            }
            Event::PartUpdated { part, .. } => self.updated(part),
            Event::MessageFinished { reason, error, .. } => {
                // Whatever a step never closed still belongs to the reader.
                self.flush();
                if *reason == FinishReason::Failed {
                    let error = error
                        .clone()
                        .unwrap_or_else(|| "the turn failed".to_owned());
                    self.failed(&error);
                }

                return true;
            }
            // `run` has no mid-turn agent switch and refuses both plan doors
            // even under `--auto`, so an adoption announcement is unreachable
            // in its own turn; the arm keeps the protocol match honest without
            // inventing a seventh JSON kind for an event it cannot receive.
            Event::AgentChanged { .. } => {}
            Event::PermissionRequested { .. }
            | Event::PermissionReplied { .. }
            | Event::RevertChanged { .. }
            // Nothing steers a headless run: the one prompt is handed over
            // before the turn opens and there is no composer to type a second
            // one into, so the event exists here only to keep the match
            // honest. The steered message itself would render anyway — it
            // arrives as an ordinary `MessageStarted` above.
            | Event::SteerConsumed { .. }
            // An effort switch is session state, not an account of the turn,
            // and this run announced its own `--effort` before the turn
            // began — the same reasoning that leaves `AgentChanged` above
            // unrendered, and the "serialized generically" half of the flag:
            // the six nd-JSON type names have no room for a shape no consumer
            // was promised.
            | Event::EffortChanged { .. }
            // The quad is a dialog's lifecycle, not an account of the turn:
            // a headless run refuses every question before one can be asked,
            // and `--format json`'s six type names have no room for a shape
            // no consumer was promised.
            | Event::QuestionAsked { .. }
            | Event::QuestionReplied { .. }
            | Event::QuestionRejected { .. } => {}
        }

        false
    }

    /// Writes upstream's `> agent · model` header, once, above the first thing
    /// the model says (`run.ts:705-713`) — with the effort in parentheses
    /// beside the model when the turn runs under one, which is the readable
    /// announcement `--effort` promises.
    fn announce(&mut self, message: &Message, agent: Option<&str>) {
        if self.format == Format::Json || self.announced || message.role != Role::Assistant {
            return;
        }
        self.announced = true;

        let mut model = message.model.as_deref().unwrap_or_default().to_owned();
        if let Some(effort) = &self.effort {
            model = format!("{model} ({effort})");
        }
        let line = match agent {
            Some(agent) => format!("{agent} \u{b7} {model}"),
            None => model,
        };
        let _ = writeln!(self.out, "\n> {}\n", printable(&line));
    }

    /// Reports a tool call that has stopped running.
    ///
    /// Only the two terminal states, as upstream reports only them
    /// (`run.ts:719`): a pending or running call is still going to change.
    fn updated(&mut self, part: &Part) {
        let PartBody::Tool { tool, state, .. } = &part.body else {
            return;
        };

        match state {
            ToolState::Completed { title, .. } => {
                self.emit(Kind::ToolUse, "part", part);
                if self.format == Format::Default {
                    let _ = writeln!(self.out, "{RAN} {}", printable(described(title, tool)));
                }
            }
            ToolState::Error { error, .. } => {
                self.emit(Kind::ToolUse, "part", part);
                if self.format == Format::Default {
                    let _ = writeln!(self.out, "{FAILED} {} failed", printable(tool));
                    // The reason is a diagnostic; the account of what ran is
                    // the payload.
                    let _ = writeln!(self.err, "{}", printable(error));
                }
            }
            ToolState::Pending { .. } | ToolState::Running { .. } => {}
        }
    }

    /// Writes every text part that has finished streaming.
    fn flush(&mut self) {
        for (id, text, kind) in std::mem::take(&mut self.open) {
            if self.format == Format::Json {
                let body = if kind == Kind::Reasoning {
                    PartBody::ReasoningText { text }
                } else {
                    PartBody::Text { text }
                };
                self.emit(kind, "part", &Part { id, body });
                continue;
            }

            // The readable stream carries the answer and not the way to it;
            // see [`TYPES`].
            if kind == Kind::Reasoning {
                continue;
            }

            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = writeln!(self.out, "{trimmed}");
        }
    }

    /// Records what failed the turn, and emits the object that says so.
    ///
    /// Upstream accumulates the same way (`run.ts:783`) and reports each
    /// failure to stderr where it happened; here the caller reports it once,
    /// on its way to the exit code. The two are the same account in the same
    /// order. In `run`'s world `MessageFinished` is still last because both
    /// plan doors are refused here, so no trailing `AgentChanged` can
    /// interleave with a failure; it is one line rather than the same sentence
    /// twice.
    fn failed(&mut self, error: &str) {
        self.emit_value(Kind::Error, "error", Value::from(error));
        self.failure = Some(match self.failure.take() {
            Some(first) => format!("{first}\n{error}"),
            None => error.to_owned(),
        });
    }

    /// Says a permission request was refused because nobody is here to answer
    /// it (`run.ts:806-810`).
    fn rejecting(&mut self, tool: &str, title: &str) {
        let _ = writeln!(
            self.err,
            "! permission requested: {} ({}); auto-rejecting",
            printable(tool),
            printable(title)
        );
    }

    /// Serializes `part` under `field` and writes the line.
    fn emit(&mut self, kind: Kind, field: &str, part: &Part) {
        if self.format != Format::Json {
            return;
        }
        // A `Part` is serde-derived and holds only data the engine put there,
        // so this cannot fail for any part that reached the stream.
        let Ok(value) = serde_json::to_value(part) else {
            return;
        };
        self.emit_value(kind, field, value);
    }

    fn emit_value(&mut self, kind: Kind, field: &str, value: Value) {
        if self.format != Format::Json {
            return;
        }

        let mut object = serde_json::Map::new();
        object.insert("type".to_owned(), Value::from(kind.as_str()));
        object.insert("timestamp".to_owned(), Value::from(millis_now()));
        object.insert("sessionID".to_owned(), Value::from(self.session.clone()));
        object.insert(field.to_owned(), value);

        let _ = writeln!(self.out, "{}", Value::Object(object));
    }

    /// Flushes both channels and hands back what the turn failed with.
    fn finish(mut self) -> Result<Option<String>> {
        self.flush();
        self.out.flush().context("failed to write the turn")?;
        self.err
            .flush()
            .context("failed to write the diagnostics")?;

        Ok(self.failure)
    }
}

/// What a completed call is called, falling back to the tool's own name.
///
/// A tool's title is written for a person to read and is what upstream renders
/// (`run.ts:99`); an empty one would leave a line saying nothing at all.
fn described<'a>(title: &'a str, tool: &'a str) -> &'a str {
    if title.trim().is_empty() { tool } else { title }
}

/// Combines what was typed with what was piped, upstream's `resolveRunInput`
/// (`run.ts:40-50`).
///
/// The piped text goes last, and an empty half is no half at all — which is
/// what makes `echo hi | ganja run` and `ganja run hi` two spellings of the
/// same message rather than one of them carrying a stray newline.
fn resolve_input(typed: &str, piped: &str) -> String {
    if typed.is_empty() {
        return piped.to_owned();
    }
    if piped.is_empty() {
        return typed.to_owned();
    }

    format!("{typed}\n{piped}")
}

/// Everything on standard input, when standard input is not a terminal.
///
/// Upstream's condition exactly (`run.ts:416`): a terminal is a person, and
/// reading from one would hang the run waiting for a message that was already
/// given on the command line.
///
/// # Errors
///
/// When the pipe cannot be read, or carries something that is not text. A
/// message is text by definition, and guessing at bytes that are not would put
/// replacement characters in front of the model.
fn piped_stdin() -> Result<String> {
    let mut stdin = io::stdin();
    if stdin.is_terminal() {
        return Ok(String::new());
    }

    let mut piped = String::new();
    stdin
        .read_to_string(&mut piped)
        .context("failed to read the message from standard input")?;

    Ok(piped)
}

/// What this build emits, exercised without a process.
///
/// Everything here is about the two rules a consumer depends on — the set of
/// `type` values, and whose session id is stamped on them — plus the joining
/// rules that decide what the model is actually asked.
#[cfg(test)]
mod tests {
    use ganja_protocol::{
        Event, FinishReason, Message, MessageId, Part, PartBody, PartId, Role, SessionId,
        ToolState, Usage,
    };
    use serde_json::Value;

    use super::{Format, Kind, Reporter, TYPES, resolve_input};

    /// The session every fixture runs in, distinct from anything a part
    /// carries so that a stamp read off the wrong place would show.
    const SESSION: &str = "ses_the_runs_own";

    /// The session the fixture events themselves carry — deliberately not
    /// [`SESSION`], for the same reason that one is distinct from anything a
    /// part carries: the stamp is the run's own local, and a stamp read off
    /// the event instead would show.
    fn event_session() -> SessionId {
        SessionId::from("ses_carried_on_events".to_owned())
    }

    /// Drives `events` through a reporter and hands back both channels.
    fn report(format: Format, events: &[Event]) -> (String, String) {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        {
            let mut reporter = Reporter::new(format, SESSION.to_owned(), &mut out, &mut err);
            for event in events {
                if reporter.apply(event, Some("build")) {
                    break;
                }
            }
            reporter.finish().expect("a vector accepts every write");
        }

        (
            String::from_utf8(out).expect("the output is text"),
            String::from_utf8(err).expect("the diagnostics are text"),
        )
    }

    /// Every object of an nd-JSON stream, parsed.
    fn objects(stream: &str) -> Vec<Value> {
        stream
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("every line is one JSON object"))
            .collect()
    }

    fn assistant() -> Message {
        Message {
            id: MessageId::from("msg_1".to_owned()),
            role: Role::Assistant,
            parts: Vec::new(),
            time: ganja_protocol::MessageTime {
                created: 1,
                completed: None,
            },
            model: Some("canned".to_owned()),
            usage: None,
        }
    }

    /// One turn's worth of stream: a step that says something and calls a
    /// tool, then closes.
    fn turn() -> Vec<Event> {
        let text = Part {
            id: PartId::from("prt_text".to_owned()),
            body: PartBody::Text {
                text: String::new(),
            },
        };
        let step = Part {
            id: PartId::from("prt_step".to_owned()),
            body: PartBody::StepStart,
        };
        let finish = Part {
            id: PartId::from("prt_finish".to_owned()),
            body: PartBody::StepFinish {
                usage: Usage::default(),
            },
        };
        let call = Part {
            id: PartId::from("prt_call".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "src/main.rs"}),
                    output: "fn main() {}".to_owned(),
                    title: "Read src/main.rs".to_owned(),
                    metadata: Value::Null,
                    started: 1,
                    completed: 2,
                },
            },
        };
        let message_id = MessageId::from("msg_1".to_owned());

        vec![
            Event::MessageStarted {
                session_id: event_session(),
                message: Message::user("what is in main"),
            },
            Event::MessageStarted {
                session_id: event_session(),
                message: assistant(),
            },
            Event::PartStarted {
                session_id: event_session(),
                message_id: message_id.clone(),
                part: step,
            },
            Event::PartStarted {
                session_id: event_session(),
                message_id: message_id.clone(),
                part: text,
            },
            Event::PartDelta {
                session_id: event_session(),
                message_id: message_id.clone(),
                part_id: PartId::from("prt_text".to_owned()),
                delta: "Reading it.".to_owned(),
            },
            Event::PartStarted {
                session_id: event_session(),
                message_id: message_id.clone(),
                part: finish,
            },
            Event::PartUpdated {
                session_id: event_session(),
                message_id: message_id.clone(),
                part: call,
            },
            Event::MessageFinished {
                session_id: event_session(),
                message_id,
                reason: FinishReason::Completed,
                usage: None,
                error: None,
                completed: 3,
            },
        ]
    }

    /// The set is the contract: a consumer switches on it, and a seventh name
    /// would reach a default arm nobody wrote.
    #[test]
    fn the_wire_carries_exactly_upstreams_six_type_names() {
        assert_eq!(
            TYPES,
            [
                "tool_use",
                "step_start",
                "step_finish",
                "text",
                "reasoning",
                "error"
            ]
        );
        // Each kind names a distinct one of them, so no two objects can be
        // told apart by anything but their type. All six are listed: `as_str`
        // indexes `TYPES` by discriminant, so a kind left out of this array is
        // a kind whose index nothing checks — which is exactly how `reasoning`
        // sat here unverified while its slot was still a placeholder.
        let named = [
            Kind::ToolUse,
            Kind::StepStart,
            Kind::StepFinish,
            Kind::Text,
            Kind::Reasoning,
            Kind::Error,
        ]
        .map(Kind::as_str);
        assert!(
            named.iter().all(|name| TYPES.contains(name)),
            "a kind named something outside the set: {named:?}"
        );
        let mut sorted = named;
        sorted.sort_unstable();
        sorted.windows(2).for_each(|pair| {
            assert_ne!(pair[0], pair[1], "two kinds share a name");
        });
    }

    #[test]
    fn every_emitted_object_carries_a_type_from_the_set() {
        let (out, _) = report(Format::Json, &turn());
        let emitted = objects(&out);

        assert!(!emitted.is_empty(), "a turn has to emit something");
        for object in &emitted {
            let kind = object["type"].as_str().expect("every object has a type");
            assert!(
                TYPES.contains(&kind),
                "an object carried a type outside the set: {kind}"
            );
        }
    }

    /// The rule the whole format hangs on: the id is the run's own, captured
    /// once, and nothing about a part can move it.
    #[test]
    fn every_emitted_object_carries_the_runs_own_session_id() {
        let (out, _) = report(Format::Json, &turn());

        for object in objects(&out) {
            assert_eq!(
                object["sessionID"].as_str(),
                Some(SESSION),
                "an object carried a session that is not this run's: {object}"
            );
        }
    }

    /// A turn that said something and ran a tool emits both, in the order they
    /// happened, and the text is closed by the step rather than left behind.
    #[test]
    fn a_turn_emits_its_step_its_text_and_its_call_in_order() {
        let (out, _) = report(Format::Json, &turn());
        let kinds: Vec<String> = objects(&out)
            .iter()
            .map(|object| object["type"].as_str().unwrap_or_default().to_owned())
            .collect();

        assert_eq!(kinds, ["step_start", "text", "step_finish", "tool_use"]);
    }

    #[test]
    fn a_streamed_text_part_carries_every_fragment_that_was_appended() {
        let (out, _) = report(Format::Json, &turn());
        let text = objects(&out)
            .into_iter()
            .find(|object| object["type"] == "text")
            .expect("the turn said something");

        assert_eq!(text["part"]["text"].as_str(), Some("Reading it."));
    }

    /// Default format is for a person, so the model's words reach stdout whole
    /// and the header names what answered.
    #[test]
    fn the_default_format_writes_the_header_and_the_reply_to_stdout() {
        let (out, err) = report(Format::Default, &turn());

        assert!(
            out.contains("> build \u{b7} canned"),
            "no header in {out:?}"
        );
        assert!(out.contains("Reading it."), "no reply in {out:?}");
        assert!(out.contains("Read src/main.rs"), "no tool line in {out:?}");
        assert!(
            err.is_empty(),
            "a turn that worked said nothing on stderr: {err:?}"
        );
    }

    /// A failed turn emits an `error` object *and* is what the run answers
    /// with, which is what the caller turns into the exit code. The stderr
    /// line is the caller's, so that the same sentence is not printed twice.
    #[test]
    fn a_failed_turn_emits_an_error_object_and_is_what_the_run_returns() {
        let mut events = turn();
        events.pop();
        events.push(Event::MessageFinished {
            session_id: event_session(),
            message_id: MessageId::from("msg_1".to_owned()),
            reason: FinishReason::Failed,
            usage: None,
            error: Some("the provider hung up".to_owned()),
            completed: 3,
        });

        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let returned = {
            let mut reporter = Reporter::new(Format::Json, SESSION.to_owned(), &mut out, &mut err);
            for event in &events {
                if reporter.apply(event, Some("build")) {
                    break;
                }
            }
            reporter.finish().expect("a vector accepts every write")
        };
        let out = String::from_utf8(out).expect("the output is text");
        let err = String::from_utf8(err).expect("the diagnostics are text");

        let failure = objects(&out)
            .into_iter()
            .find(|object| object["type"] == "error")
            .expect("a failed turn emits an error object");
        assert_eq!(failure["error"].as_str(), Some("the provider hung up"));
        assert_eq!(failure["sessionID"].as_str(), Some(SESSION));
        assert_eq!(returned.as_deref(), Some("the provider hung up"));
        assert!(err.is_empty(), "the caller owns the stderr line: {err:?}");
    }

    /// A tool that failed is still a `tool_use` object — upstream emits the
    /// part for both terminal states (`run.ts:719-720`) — and its reason is a
    /// diagnostic rather than payload.
    #[test]
    fn a_failed_call_is_a_tool_use_object_and_its_reason_goes_to_stderr() {
        let call = Part {
            id: PartId::from("prt_call".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "edit".to_owned(),
                state: ToolState::Error {
                    input: serde_json::json!({}),
                    error: "the file was not read first".to_owned(),
                    started: 1,
                    completed: 2,
                },
            },
        };
        let events = [Event::PartUpdated {
            session_id: event_session(),
            message_id: MessageId::from("msg_1".to_owned()),
            part: call.clone(),
        }];

        let (out, err) = report(Format::Json, &events);
        assert_eq!(objects(&out).len(), 1);
        assert_eq!(objects(&out)[0]["type"], "tool_use");
        assert!(err.is_empty(), "json mode renders no lines: {err:?}");

        let (out, err) = report(Format::Default, &events);
        assert!(out.contains("edit failed"), "no failure line in {out:?}");
        assert!(
            err.contains("the file was not read first"),
            "the reason has to be a diagnostic: {err:?}"
        );
    }

    /// A title a tool wrote reaches a terminal that would execute an escape in
    /// it, exactly as a stored session's title does in `sessions`.
    #[test]
    fn a_tool_title_cannot_move_the_terminals_cursor() {
        let call = Part {
            id: PartId::from("prt_call".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Completed {
                    input: Value::Null,
                    output: String::new(),
                    title: "\u{1b}[2Jread \u{7}src/main.rs\r\nsecond row".to_owned(),
                    metadata: Value::Null,
                    started: 1,
                    completed: 2,
                },
            },
        };

        let (out, _) = report(
            Format::Default,
            &[Event::PartUpdated {
                session_id: event_session(),
                message_id: MessageId::from("msg_1".to_owned()),
                part: call,
            }],
        );

        let leaked: Vec<char> = out
            .chars()
            .filter(|character| character.is_control() && *character != '\n')
            .collect();
        assert!(
            leaked.is_empty(),
            "control characters reached stdout: {leaked:?}"
        );
        assert!(
            out.contains("src/main.rs"),
            "the printable half survives: {out:?}"
        );
    }

    /// The warning is the whole difference between a run that refuses and one
    /// that hangs, and it must not land in the middle of an nd-JSON stream.
    #[test]
    fn a_rejected_permission_is_warned_about_on_stderr_in_both_formats() {
        for format in [Format::Default, Format::Json] {
            let mut out: Vec<u8> = Vec::new();
            let mut err: Vec<u8> = Vec::new();
            {
                let mut reporter = Reporter::new(format, SESSION.to_owned(), &mut out, &mut err);
                reporter.rejecting("bash", "rm -rf /");
                reporter.finish().expect("a vector accepts every write");
            }

            let err = String::from_utf8(err).expect("the diagnostics are text");
            assert!(out.is_empty(), "a warning is never payload: {out:?}");
            assert!(err.contains("bash"), "the tool has to be named: {err:?}");
            assert!(
                err.contains("rm -rf /"),
                "what would run has to be named: {err:?}"
            );
            assert!(
                err.contains("auto-rejecting"),
                "the decision has to be said: {err:?}"
            );
        }
    }

    /// The `$` scan runs headless too: a run's prompt tokens reach the
    /// provider expanded through the engine's own roots, exactly as a
    /// screenful session's do. The nd-JSON stream carries only the
    /// assistant's side, so the proof reads the fake provider's request log
    /// — which *is* this run's wire.
    #[tokio::test]
    async fn a_runs_dollar_tokens_reach_the_provider_expanded() {
        let root = tempfile::tempdir().expect("a scratch directory");
        let dir = root.path().join("porting");
        std::fs::create_dir_all(&dir).expect("the fixture tree is creatable");
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: porting\n---\nRead upstream first.",
        )
        .expect("the fixture is writable");

        let provider = std::sync::Arc::new(ganja_core::provider::fake::FakeProvider::new(
            "done",
            std::time::Duration::ZERO,
        ));
        let engine = ganja_core::Engine::new(
            std::sync::Arc::clone(&provider) as _,
            "run-model",
            std::sync::Arc::new(ganja_core::tool::Registry::new(Vec::new())),
            ganja_core::permission::Permissions::default(),
        )
        .with_skill_roots(
            ganja_core::tool::skill::Roots::none().with_paths([root.path().to_path_buf()]),
        );

        let failure = super::drive(
            &engine,
            None,
            "use $porting and leave $PATH alone",
            None,
            false,
            Format::Default,
        )
        .await
        .expect("the canned turn drives to its end");
        assert_eq!(failure, None, "and ends clean");

        let requests = provider.recorded();
        let carried: Vec<&str> = requests
            .first()
            .expect("the run reached the provider")
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| message.parts.iter())
            .filter_map(ganja_protocol::Part::as_text)
            .collect();

        assert!(
            carried
                .iter()
                .any(|part| part.starts_with("<skill_content name=\"porting\">")
                    && part.contains("Read upstream first.")),
            "the invoked skill rides the request whole: {carried:?}"
        );
        assert!(
            carried.iter().any(|part| part.contains("$PATH")),
            "and the un-invoked token is still just text: {carried:?}"
        );
        assert!(
            !carried.iter().any(|part| part.contains("name=\"PATH\"")),
            "nothing answered to $PATH, so nothing was loaded for it: {carried:?}"
        );
    }

    #[test]
    fn a_typed_message_and_a_piped_one_join_with_the_pipe_last() {
        assert_eq!(
            resolve_input("explain this", "fn main() {}"),
            "explain this\nfn main() {}"
        );
        assert_eq!(resolve_input("explain this", ""), "explain this");
        assert_eq!(resolve_input("", "fn main() {}"), "fn main() {}");
        assert_eq!(resolve_input("", ""), "");
    }
}
