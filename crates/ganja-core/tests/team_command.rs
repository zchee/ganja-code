//! The `/team` builtin: what it is in the roster, and what it expands to.
//!
//! The template is prose the model acts on, so nothing here asserts a
//! rendering. What it pins is the five things the *code* is responsible for:
//! the command exists as a builtin with its grammar as a hint; the two
//! directory placeholders are filled at roster build with this project's real
//! absolute paths, so two sessions in one checkout cannot disagree about where
//! the state lives; the third placeholder — the session's own id — is filled
//! at expansion instead, because one roster serves every session a process
//! opens and no model can read that id for itself; whatever the user typed
//! reaches the text intact, however the grammar spelled it, because the model
//! is what parses it; and the three branches the text has to carry — usage,
//! the `/teammate` redirect, and the pipeline — are all in front of the model
//! on every run, since which one applies is the model's decision rather than a
//! dispatch here.
//!
//! Its own test binary because it writes `XDG_DATA_HOME` and
//! `GANJA_CONFIG_HOME` process-wide: the resolved paths are read off the first
//! and the file command tier off the second, and a suite that inherited either
//! from the developer running it would be describing that machine.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use ganja_core::config::CONFIG_HOME_ENV;
use ganja_core::project::Project;
use ganja_core::tool::{Credentials, FileTimes, ToolCtx};
use ganja_core::{Config, command};

/// The worktree every roster here is built for. A path this binary never
/// creates, so the project command tier is empty and the slug is stable.
const WORKTREE: &str = "/repo";

/// Points both homes at directories this binary owns.
///
/// `XDG_DATA_HOME` decides where [`Project::data_dir`] answers, which is what
/// the two `/team` placeholders are filled from; `GANJA_CONFIG_HOME` decides
/// the global command tier (**D481**), which would otherwise be whatever
/// `*.md` files the developer running the suite keeps in their own home
/// (`ganja-code-qh1`). Neither directory is created: an absent one is the
/// empty tier these tests want and the path arithmetic does not care.
///
/// Forced through a `LazyLock` rather than written per test because this
/// binary's tests share one process and run on parallel threads under a plain
/// `cargo test`: the one write happens before the first read, with any other
/// builder parked on the lock while it does.
fn pin_homes() -> &'static Path {
    static DATA: LazyLock<PathBuf> = LazyLock::new(|| {
        let base = std::env::temp_dir().join(format!("ganja-team-command-{}", std::process::id()));
        // SAFETY: this binary's only writes to the environment, run exactly
        // once, under the lock every reader here goes through.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", base.join("data"));
            std::env::set_var(CONFIG_HOME_ENV, base.join("config"));
        }
        base.join("data")
    });

    &DATA
}

/// The builtin roster, with both homes pinned first.
fn roster() -> command::Registry {
    pin_homes();

    command::Registry::build(&Config::default(), Path::new(WORKTREE))
}

/// A session id shaped like the ones this build mints, so the tests that are
/// not about the id read as a real run does.
const SESSION: &str = "01997c4b-1d2e-7a10-9f3c-6b2e5d8a4c71";

/// What `/team` sends for `arguments`, from a session that could be any of
/// them.
async fn expand(arguments: &str) -> String {
    expand_as(arguments, SESSION).await
}

/// The same, from a session named `session`.
///
/// The template names no `@file` and runs no ``!`command` ``, so expansion is
/// pure text substitution and the context only has to be somewhere real.
async fn expand_as(arguments: &str, session: &str) -> String {
    let registry = roster();
    let team = registry.get("team").expect("`/team` is a builtin");
    let ctx = ToolCtx {
        cwd: std::env::temp_dir(),
        cancel: tokio_util::sync::CancellationToken::new(),
        call_id: String::new(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    team.expand(arguments, session, &ctx).await.expect("these arguments expand").prompt
}

/// What `/team` refuses `arguments` with, for the lines it never expands.
async fn refusal(arguments: &str) -> command::Misdirected {
    let registry = roster();
    let team = registry.get("team").expect("`/team` is a builtin");
    let ctx = ToolCtx {
        cwd: std::env::temp_dir(),
        cancel: tokio_util::sync::CancellationToken::new(),
        call_id: String::new(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    team.expand(arguments, SESSION, &ctx)
        .await
        .expect_err("a roster line is refused rather than expanded")
}

/// Where this checkout's pipeline state belongs, computed the way anything
/// else on the machine would compute it rather than by restating the layout.
fn team_dir(leaf: &str) -> String {
    pin_homes();
    Project::resolve(Path::new(WORKTREE))
        .data_dir()
        .expect("the pinned data home resolves")
        .join("team")
        .join(leaf)
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
async fn team_is_a_builtin_with_its_grammar_as_the_hint() {
    let registry = roster();
    let team = registry.get("team").expect("`/team` is a builtin");

    assert!(team.source.is_none(), "a builtin is not a file somebody is looking at");
    assert!(team.agent.is_none(), "the pipeline runs as whoever the session already is");
    assert!(team.model.is_none());
    assert_eq!(
        team.argument_hint.as_deref(),
        Some("[N[:agent]] [--backend <surface>] <task>"),
        "the composer draws the grammar, because nothing in this crate parses it",
    );
    assert!(
        registry.get("init").is_some(),
        "and the builtin it joined is still there: two, not one",
    );
}

#[tokio::test]
async fn both_directory_placeholders_are_filled_at_roster_build() {
    let expanded = expand("port the loader").await;

    assert!(
        !expanded.contains("${state}") && !expanded.contains("${handoffs}"),
        "an unfilled placeholder would reach the model as literal text",
    );
    assert!(
        expanded.contains(&team_dir("state")),
        "the state directory is this project's own, absolute: {expanded}",
    );
    assert!(
        expanded.contains(&team_dir("handoffs")),
        "and so is the handoffs directory: {expanded}",
    );
}

#[tokio::test]
async fn the_filled_paths_are_under_the_data_home_and_not_the_worktree() {
    let state = team_dir("state");

    assert!(Path::new(&state).is_absolute(), "{state}");
    assert!(
        state.starts_with(&*pin_homes().to_string_lossy()),
        "operational state lives in the data home (plan decision 19): {state}",
    );
    assert!(
        !state.contains("/repo/.ganja"),
        "`.ganja/` stays a committable-config namespace: {state}",
    );
}

#[tokio::test]
async fn bare_team_still_reaches_the_model_with_its_usage_and_teammate_pointer() {
    // Bare `/team` is answered by the template rather than by a dispatch here,
    // so what this pins is that the text carrying the usage is what an empty
    // argument list sends — and that the usage names the command the roster
    // muscle memory moved to.
    let expanded = expand("").await;

    assert!(
        expanded.contains("/teammate"),
        "the usage points the old spelling somewhere: {expanded}",
    );
    assert!(
        expanded.contains("[N[:agent]] [--backend <surface>] <task>"),
        "and shows the grammar: {expanded}",
    );
    assert!(expanded.contains("--backend"), "including what a backend is chosen from: {expanded}",);
}

/// **Bead 2m46.** The three exact spellings never reach the model at all: they
/// are refused where the expansion would have begun, with the line that was
/// meant. A round trip to be told what three fixed words already say is a round
/// trip nobody should pay for a typo.
#[tokio::test]
async fn legacy_roster_arguments_are_refused_before_a_turn_starts() {
    for (legacy, meant) in [
        ("spawn worker-1", "/teammate spawn worker-1"),
        ("shutdown worker-1", "/teammate shutdown worker-1"),
        ("list", "/teammate list"),
    ] {
        assert_eq!(refusal(legacy).await.meant, meant, "`/team {legacy}` names its own answer");
    }
}

/// And it is refused on behalf of **this build's** `/team`, never on behalf of
/// the name.
///
/// A `[command.team]` entry replaces the builtin outright — the config tier
/// wins a name it reuses, which `command::Registry::build` documents as
/// deliberate — so from that point the project's own template is what `/team`
/// sends. A gate that kept refusing three argument shapes there would make
/// somebody's own command unreachable for those spellings, and would answer
/// with a sentence about a command they never wrote.
#[tokio::test]
async fn a_project_that_owns_the_team_name_keeps_the_roster_spellings() {
    pin_homes();
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
    let registry =
        command::Registry::build(&Config { command, ..Config::default() }, Path::new(WORKTREE));
    let team = registry.get("team").expect("the config entry took the name");
    assert!(!team.builtin, "the builtin was replaced rather than layered under");
    let ctx = ToolCtx {
        cwd: std::env::temp_dir(),
        cancel: tokio_util::sync::CancellationToken::new(),
        call_id: String::new(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    for typed in ["spawn w1", "shutdown w2", "list"] {
        let expanded = team
            .expand(typed, SESSION, &ctx)
            .await
            .unwrap_or_else(|refused| panic!("`/team {typed}` is the project's own: {refused:?}"))
            .prompt;

        assert_eq!(
            expanded,
            format!("run the deploy playbook for {typed}"),
            "the project's template is what runs, whole",
        );
    }

    // And the builtin, on the same machine and the same worktree, still is not.
    assert_eq!(refusal("spawn w1").await.meant, "/teammate spawn w1");
}

/// And the template's redirect branch is still the fallback, because the
/// refusal above only knows three spellings: anything else that means the same
/// thing reaches the model with what was typed, whole, so it can answer with
/// the corrected line itself.
#[tokio::test]
async fn roster_management_in_other_words_reaches_the_branch_that_redirects_it() {
    for asked in ["start a teammate called worker-1", "who is on the team"] {
        let expanded = expand(asked).await;

        assert!(
            expanded.contains(asked),
            "what was typed reaches the model whole, so it can echo the corrected line: {expanded}",
        );
        assert!(
            expanded.contains("/teammate"),
            "and the branch that answers it names where those moved: {expanded}",
        );
    }
}

#[tokio::test]
async fn the_whole_grammar_reaches_the_text_however_it_was_spelled() {
    for typed in [
        "3 port the config loader",
        "3:executor port the config loader",
        "2:debugger --backend claude find the leak",
        "--backend codex read the wire and report",
        "just do the thing",
    ] {
        let expanded = expand(typed).await;

        assert!(
            expanded.contains(typed),
            "`$ARGUMENTS` carries the whole line untokenized, because the model parses it: \
             {typed:?} is missing from {expanded}",
        );
    }
}

#[tokio::test]
async fn the_text_carries_the_stage_names_the_state_file_records() {
    let expanded = expand("port the loader").await;

    for stage in ["team-plan", "team-prd", "team-exec", "team-verify", "team-fix"] {
        assert!(expanded.contains(stage), "the pipeline names its stage {stage}: {expanded}");
    }
    for field in ["current_phase", "fix_loop_count", "max_fix_loops", "stage_history"] {
        assert!(expanded.contains(field), "the state schema names {field}: {expanded}");
    }
    for heading in ["# Decided", "# Rejected", "# Risks", "# Files", "# Remaining"] {
        assert!(expanded.contains(heading), "the handoff names {heading}: {expanded}");
    }
}

#[tokio::test]
async fn the_text_carries_the_tools_the_pipeline_actually_drives() {
    let expanded = expand("port the loader").await;

    for tool in ["task_create", "task_update", "task_list", "send_message"] {
        assert!(expanded.contains(tool), "the pipeline is driven through {tool}: {expanded}");
    }
    assert!(
        expanded.contains("shutdown_request"),
        "and shut down through the frame that already exists: {expanded}",
    );
    assert!(
        expanded.contains("idle_notification"),
        "monitoring is a signal rather than a poll: {expanded}",
    );
}

#[tokio::test]
async fn the_text_states_the_backend_asymmetry_and_the_permission_cost() {
    let expanded = expand("port the loader").await;

    assert!(
        expanded.contains("claude"),
        "the surprise a `claude` member's own list is has to be said out loud: {expanded}",
    );
    for surface in ["codex", "grok", "agy"] {
        assert!(expanded.contains(surface), "and {surface} holds no ganja tools: {expanded}");
    }
    assert!(
        expanded.contains("permission"),
        "N spawns raise N dialogs, and the model is told to expect them: {expanded}",
    );
}

#[tokio::test]
async fn the_text_routes_every_stage_to_an_agent_this_build_ships() {
    let expanded = expand("port the loader").await;

    // `plan` is deliberately not among them: it is a primary agent the `task`
    // tool refuses, so the table routes team-plan to the lead session itself.
    for agent in ["analyst", "executor", "verifier", "critic", "debugger", "explore"] {
        assert!(expanded.contains(agent), "the routing table names {agent}: {expanded}");
    }
    assert!(
        expanded.contains("team-plan     the lead itself"),
        "team-plan is the lead's own stage: {expanded}",
    );
    assert!(
        !expanded.contains("team-plan     plan"),
        "and is never routed to the agent the spawn door refuses: {expanded}",
    );
    assert!(
        expanded.contains(".ganja/agents"),
        "and says a project's own definition outranks the builtin: {expanded}",
    );
}

/// The state file's name is **given**, not derived: a session cannot read its
/// own id — the `<env>` block does not carry it and `list_sessions` drops the
/// caller's own row — so a template that said "use this session's id" was
/// asking for the one thing the model on the other side of it has no way to
/// find out.
#[tokio::test]
async fn the_session_placeholder_is_filled_with_the_id_the_expansion_runs_under() {
    let expanded = expand("port the loader").await;

    assert!(
        !expanded.contains("${session}"),
        "an unfilled placeholder would reach the model as literal text: {expanded}",
    );
    assert!(
        expanded.contains(&format!("`{SESSION}.json`")),
        "the state file is named outright, id and extension: {expanded}",
    );
}

/// And it is filled at **expansion**, not at roster build, which is what makes
/// one roster serve every session a process opens: two expansions of the same
/// definition name two different files.
#[tokio::test]
async fn two_sessions_expanding_one_roster_name_two_different_state_files() {
    let registry = roster();
    let team = registry.get("team").expect("`/team` is a builtin");
    assert!(
        team.template.contains("${session}"),
        "the roster leaves it standing, because a roster has no session: {}",
        team.template,
    );

    let first = expand_as("port the loader", "session-one").await;
    let second = expand_as("port the loader", "session-two").await;

    assert!(first.contains("`session-one.json`"), "{first}");
    assert!(second.contains("`session-two.json`"), "{second}");
    assert!(!first.contains("session-two"), "neither expansion carries the other's: {first}");
}

/// The id goes in as the bytes it is — `str::replace`, one left-to-right pass,
/// and nothing reads what it wrote afterwards.
///
/// A real session id is a UUIDv7 and would survive almost any substitution,
/// which is exactly why the one here is spelled with the placeholder's own
/// text, with an argument token, and with characters a pattern language would
/// take an interest in. Each catches a different way of getting this wrong: a
/// second pass would eat the first replacement, filling this before
/// `$ARGUMENTS` would rewrite the `$1` into whatever the user typed, and a
/// regex would mangle the rest. That the fill is *last* is the reason the
/// middle one holds, and the reason worth having: every pass that reads this
/// text for something to run or attach has already finished by then.
#[tokio::test]
async fn a_session_id_is_substituted_literally_however_it_is_spelled() {
    let odd = "${session}-$1-.*-a+b";

    let expanded = expand_as("port the loader", odd).await;

    assert!(
        expanded.contains(&format!("`{odd}.json`")),
        "the id reaches the model exactly as it was spelled: {expanded}",
    );
}
