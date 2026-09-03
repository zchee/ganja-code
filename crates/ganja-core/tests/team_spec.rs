//! `/team`'s grammar as an engine really runs it (**D549**).
//!
//! `command_tests.rs` walks the grammar itself — a pure function over a roster
//! closure, thirty-odd rows and no fixture at all. What only an engine can say
//! is the other three things:
//!
//! - the refusals are **the engine's**: a spec segment naming a primary, or a
//!   name nobody holds, or a count of nothing, is refused with the roster this
//!   session really has rather than with a closure a test wrote;
//! - a refused spec **starts no turn** — the door is before `start_turn`, so
//!   the session is still idle and the transcript never grew a message;
//! - both gates are on [`Definition::builtin`](ganja_core::command::Definition)
//!   rather than on the name, which is D547's own ruling and, after D549 moved
//!   the misdirection door out of the expansion, provable nowhere else.
//!
//! Its own test binary because it writes `XDG_DATA_HOME` and
//! `GANJA_CONFIG_HOME` process-wide, the way `team_command.rs` does and for the
//! same two reasons: the `/team` template's directory placeholders are resolved
//! off the first, and the file command tier off the second, so a suite
//! inheriting either from the developer running it would be describing that
//! machine. No `[[test]]` entry is needed or wanted — `ganja-core` declares
//! none and sets no `autotests = false`, so this file is auto-discovered.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use futures::StreamExt as _;
use ganja_core::config::CONFIG_HOME_ENV;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event};
use ganja_core::tool::Registry;
use ganja_core::{Config, Engine, EngineError, command};
use ganja_testkit::ScriptedProvider;

/// How long a turn started here is given to end. Generous against a loaded
/// machine: the scripted provider answers nothing at all, so what is being
/// waited on is the loop noticing that.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(20);

/// The worktree the config-owned roster is built for. A path this binary never
/// creates, so the project command tier is empty and the slug is stable.
const WORKTREE: &str = "/repo";

/// Points both homes at directories this binary owns.
///
/// `XDG_DATA_HOME` decides where the `/team` template's two directory
/// placeholders resolve; `GANJA_CONFIG_HOME` decides the global command tier
/// (**D481**), which would otherwise be whatever `*.md` files the developer
/// running the suite keeps in their own home. Neither directory is created: an
/// absent one is the empty tier these tests want.
///
/// Forced through a `LazyLock` for `team_command.rs`'s reason — this binary's
/// tests share one process and run on parallel threads under a plain `cargo
/// test`, so the one write happens before the first read with any other builder
/// parked on the lock while it does.
fn pin_homes() -> &'static Path {
    static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
        let base = std::env::temp_dir().join(format!("ganja-team-spec-{}", std::process::id()));
        // SAFETY: this binary's only writes to the environment, run exactly
        // once, under the lock every reader here goes through.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", base.join("data"));
            std::env::set_var(CONFIG_HOME_ENV, base.join("config"));
        }
        base.join("config")
    });

    &HOME
}

/// An engine holding this build's real agent roster and nothing else of
/// consequence.
///
/// In-memory and scripted with no steps at all: every test here is about a
/// refusal that lands *before* a turn starts, so a provider that would answer
/// one is a provider that never gets asked. The roster is
/// [`Config::default`]'s, which is the ten builtins — `critic` spawnable,
/// `build` and `plan` primary — so what the grammar is judged against is what a
/// real session judges it against.
fn engine_with_agents() -> Engine {
    pin_homes();
    let (provider, _) = ScriptedProvider::new(Vec::new());

    Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()))
}

/// The same, with no agent registry at all — **R8**'s case.
///
/// Reachable from no shipped binary (`ganja-cli/src/assemble.rs` and
/// `ganja-tui/src/lib.rs` both call `with_agents`), which is exactly why it
/// needs a fixture: the asymmetry it produces is an accepted consequence, and
/// an accepted consequence nobody has written down is one somebody later
/// "fixes" into a guess.
fn engine_without_agents() -> Engine {
    pin_homes();
    let (provider, _) = ScriptedProvider::new(Vec::new());

    Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
}

/// Runs `/team` with `arguments` and hands back whatever the engine said.
async fn run(engine: &Engine, arguments: &str) -> Result<(), EngineError> {
    engine.send(Command::RunCommand { name: "team".to_owned(), args: arguments.to_owned() }).await
}

/// What `/team` refused `arguments` with.
async fn refused(engine: &Engine, arguments: &str) -> EngineError {
    run(engine, arguments).await.expect_err("this line is refused rather than run")
}

/// **AC-9.** Every refusal the engine words itself, against a real roster.
///
/// One row per shape rather than one test each, because what is being asserted
/// is a mapping: the parser's variant and the session's roster decide which
/// `EngineError` a person reads, and a table is what makes a wrong arm visible
/// as a wrong row.
#[tokio::test]
async fn a_spec_the_roster_refuses_never_reaches_the_model() {
    let engine = engine_with_agents();

    // A primary is a real agent, and refusing it as a typo would send somebody
    // looking for a spelling mistake in a word they spelled right.
    let primary = refused(&engine, "3:build port the loader").await;
    assert!(matches!(primary, EngineError::TeamSpec(..)), "{primary:?}");
    let said = primary.to_string();
    assert!(said.contains("build"), "the refusal names the agent: {said}");
    assert!(said.contains("not one a teammate can be"), "and why it cannot be one: {said}");

    // A name nobody holds is the one refusal that reuses `UnknownAgent`, which
    // `SwitchAgent` already words — one sentence for one problem (**B1**).
    let unknown = refused(&engine, "2:nosuch port the loader").await;
    assert!(
        matches!(&unknown, EngineError::UnknownAgent { name } if name == "nosuch"),
        "{unknown:?}",
    );

    // The two count bounds, both by name.
    let zero = refused(&engine, "0:critic port the loader").await.to_string();
    assert!(zero.contains("no members at all"), "a count of zero is refused by name: {zero}");
    let too_many = refused(&engine, "17:critic port the loader").await.to_string();
    assert!(too_many.contains("17"), "and one over the cap names what was asked: {too_many}");
    assert!(too_many.contains("16"), "and the cap it is over: {too_many}");

    // And every refusal the parser words names the way back to plain task text
    // (**PM-1**), which is what makes a false positive on prose survivable.
    for typed in ["3:build x", "0:critic x", "17:critic x"] {
        let said = refused(&engine, typed).await.to_string();
        assert!(said.contains("rather than a team spec"), "`/team {typed}`: {said}");
    }
    // The one that does not, and the cost of **B1** stated as one: an unknown
    // agent is answered in `EngineError::UnknownAgent`'s own words — the
    // sentence `SwitchAgent` already had — so it carries no escape tail. The
    // reuse is the ruling's, and one sentence for one problem is what it buys;
    // what it costs is exactly this row, and a person who typed
    // `/team notes:read this` is told the name is unknown without being told
    // how to say it as prose. Pinned rather than left to be noticed, so that
    // wording this refusal separately is a decision somebody takes on purpose.
    assert!(
        !refused(&engine, "2:nosuch x").await.to_string().contains("rather than a team spec"),
        "the reused variant carries the engine's sentence, tail and all",
    );
}

/// **AC-10.** A refused spec starts no turn.
///
/// Two halves, because either alone is weak: `send` returns `Err`, and the
/// event stream carries no `MessageStarted` for the session — so the transcript
/// never grew a message the person did not get an answer to. `Engine::settle`
/// is deliberately **not** asserted: it polls whether the slot is free *now*,
/// so it cannot tell "never taken" from "taken and finished" and would be green
/// on every path including the one this is about.
#[tokio::test]
async fn a_refused_spec_starts_no_turn() {
    let engine = engine_with_agents();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    for typed in [
        "3:build port the loader",
        "2:nosuch port the loader",
        "0:critic port the loader",
        "17:critic port the loader",
        "2:critic --backend claude,codex,grok port the loader",
        "2:critic@claude --backend codex port the loader",
        "critic@nope port the loader",
        "3:critic --backend x --backend y port the loader",
        "3:critic --backend",
        "list",
    ] {
        assert!(run(&engine, typed).await.is_err(), "`/team {typed}` is refused");
    }

    // Read after the last refusal rather than between them: a turn that started
    // would have announced its user message by now, and nothing else in this
    // engine publishes anything at all.
    drop(engine);
    let mut started = Vec::new();
    while let Some(event) = events.next().await {
        if let Event::MessageStarted { message, .. } = event {
            started.push(message);
        }
    }

    assert!(started.is_empty(), "no refusal reached a turn: {started:?}");
}

/// **AC-24.** On an engine with no agent registry, the bare-name arm never
/// fires and a spec naming an agent is [`EngineError::NoAgents`].
///
/// The asymmetry **R8** accepts, pinned as an asymmetry: one intent spelled two
/// ways is answered two ways here, because "is this a spec?" and "is this agent
/// real?" have the same answer when nothing is known. `critic --backend claude
/// x` stays task text — the head token is a bare word, and a bare word is a
/// spec only when the roster claims it — while `critic@claude x` carries an
/// `@`, is spec-shaped whatever the roster says, and is refused.
#[tokio::test]
async fn a_session_with_no_roster_answers_a_spec_with_no_agents() {
    let engine = engine_without_agents();

    let refused = refused(&engine, "critic@claude port the loader").await;
    assert!(matches!(refused, EngineError::NoAgents), "{refused:?}");

    // And the bare-name arm, on the same engine, does not fire: this is task
    // text, so nothing refuses it and the turn is started rather than gated.
    // The scripted provider answers nothing, which is the turn ending at once.
    run(&engine, "critic --backend claude port the loader")
        .await
        .expect("a bare word the roster does not claim is the first word of the task");
}

/// **AC-25**, moved here whole from `tests/team_command.rs`.
///
/// A `[command.team]` entry replaces the builtin outright — the config tier
/// wins a name it reuses, which `command::Registry::build` documents as
/// deliberate — so from that point the project's own template is what `/team`
/// sends. Neither gate may fire for it: a misdirection refusal would make three
/// argument shapes of somebody's own command unreachable and answer with a
/// sentence about a command they never wrote, and a spec parse would eat the
/// head token out of arguments their template is the only thing that reads.
///
/// After **D549** this is the **only** proof of D547's `builtin` gate: both
/// doors moved into `Engine::run_command`, and `command::misdirected` is now a
/// pure `&str -> Option<_>` that cannot be asked which definition it was called
/// for. So it needs an engine, and an engine needs a home to resolve a roster
/// against — which is what makes this binary's process-wide pinning load-bearing
/// rather than incidental.
#[tokio::test]
async fn a_project_that_owns_the_team_name_keeps_the_roster_spellings() {
    let mut command = std::collections::BTreeMap::new();
    command.insert(
        "team".to_owned(),
        ganja_core::config::CommandConfig {
            template: "run the deploy playbook for $ARGUMENTS".to_owned(),
            description: None,
            agent: None,
            model: None,
        },
    );
    let config = Config { command, ..Config::default() };
    let registry = {
        pin_homes();
        command::Registry::build(&config, Path::new(WORKTREE))
    };
    let team = registry.get("team").expect("the config entry took the name");
    assert!(!team.builtin, "the builtin was replaced rather than layered under");

    let engine = engine_with_agents().with_commands(Arc::new(registry));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    for typed in ["spawn w1", "shutdown w2", "list", "3:build the deploy", "0:critic the deploy"] {
        run(&engine, typed)
            .await
            .unwrap_or_else(|refused| panic!("`/team {typed}` is the project's own: {refused:?}"));
        // One turn at a time, and the scripted provider answers nothing, so
        // each of these is over before the next is sent. Without the wait the
        // second line would be refused `Busy` — which would be a green test
        // about the wrong refusal.
        assert!(engine.settle(SETTLE).await, "`/team {typed}` ran and ended");
    }
    drop(engine);

    let mut prompts = Vec::new();
    while let Some(event) = events.next().await {
        if let Event::MessageStarted { message, .. } = event {
            prompts.extend(
                message
                    .parts
                    .iter()
                    .filter_map(ganja_core::protocol::Part::as_text)
                    .map(str::to_owned),
            );
        }
    }

    assert_eq!(
        prompts,
        [
            "run the deploy playbook for spawn w1",
            "run the deploy playbook for shutdown w2",
            "run the deploy playbook for list",
            "run the deploy playbook for 3:build the deploy",
            "run the deploy playbook for 0:critic the deploy",
        ],
        "the project's template ran, whole, with its arguments untouched by either gate",
    );
}

/// And the builtin, on the same machine and the same worktree, still answers
/// both gates — which is the half that makes the test above about `builtin`
/// rather than about the config tier being unreachable.
#[tokio::test]
async fn the_builtin_on_the_same_machine_still_answers_both_gates() {
    let engine = engine_with_agents();

    let roster_line = refused(&engine, "spawn w1").await;
    assert!(
        matches!(&roster_line, EngineError::MisdirectedCommand { meant } if meant == "/teammate spawn w1"),
        "{roster_line:?}",
    );
    let spec = refused(&engine, "0:critic the deploy").await;
    assert!(matches!(spec, EngineError::TeamSpec(..)), "{spec:?}");
}

/// **AC-16**, the ordering half: bead 2m46's door runs **before** the grammar.
///
/// The two doors read the same line, so which one goes first decides two
/// sentences. `/team list` is a roster line and stays one; `/team 2:critic
/// list` is a pipeline over the task `list`, because the head token is a spec
/// and `misdirected` only ever claimed a *first word* of `spawn`, `shutdown`
/// or a bare `list`.
#[tokio::test]
async fn the_roster_door_runs_before_the_grammar() {
    let engine = engine_with_agents();

    let bare = refused(&engine, "list").await;
    assert!(matches!(bare, EngineError::MisdirectedCommand { .. }), "{bare:?}");

    run(&engine, "2:critic list").await.expect("a spec'd `list` is a pipeline over that task");
}
