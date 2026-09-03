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

/// The context an expansion here runs under.
///
/// The template names no `@file` and runs no ``!`command` ``, so expansion is
/// pure text substitution: every field below is what "somewhere real, holding
/// nothing" spells, and no test in this binary varies one of them.
fn ctx() -> ToolCtx {
    ToolCtx {
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
    }
}

/// The same, from a session named `session`.
async fn expand_as(arguments: &str, session: &str) -> String {
    let registry = roster();
    let team = registry.get("team").expect("`/team` is a builtin");

    team.expand(arguments, command::Fills { session, members: None }, &ctx()).await.prompt
}

/// What `/team` sends for `arguments` **with its spec resolved**, the way
/// `Engine::run_command` resolves it (**D549**).
///
/// The three steps that engine takes, in its order and against this build's
/// real agent roster: read the head token and the flag, render the roster the
/// grammar found, and expand what is left of the line with that roster filled
/// in. Spelled here rather than driven through an engine because what these
/// tests are about is the *template* — that the block lands where the spawn
/// step reads it, and that nothing is left unfilled — and an engine would cost
/// this binary a provider and a store to prove a substitution. That the engine
/// really wires it this way is `tests/team_spec.rs`'s.
///
/// The roster predicate is `AgentRegistry::roster_answer`, which is the one the
/// engine hands `parse_team` rather than a copy of it: a copy would go on
/// passing the day that mapping changed, and it has changed once already
/// (**Dv-1** widened the bare-name arm).
async fn expand_spec(arguments: &str) -> String {
    let agents = ganja_testkit::agent_registry(&Config::default());
    let invocation = command::parse_team(arguments, &|name: &str| agents.roster_answer(name))
        .expect("these arguments are a valid spec");
    let members = command::render_members(&invocation);

    let registry = roster();
    let team = registry.get("team").expect("`/team` is a builtin");

    team.expand(
        &invocation.task,
        command::Fills { session: SESSION, members: Some(&members) },
        &ctx(),
    )
    .await
    .prompt
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
        Some("[[N:]agent[@surface],\u{2026}] [--backend <surface>,\u{2026}] <task>"),
        "the composer draws the grammar the code really reads (**D549**)",
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
        expanded.contains("[[N:]agent[@surface],\u{2026}] [--backend <surface>,\u{2026}] <task>"),
        "and shows the grammar: {expanded}",
    );
    assert!(expanded.contains("--backend"), "including what a backend is chosen from: {expanded}",);
}

/// **AC-26.** The grammar is spelled once in this build: the hint the composer
/// draws and the usage the template prints are one string.
///
/// A drift pin rather than a measurement — both halves already agree today —
/// and the drift it is pinned against is the expensive one: D549 made the hint
/// a description of what `parse_team` really accepts, so a template that went
/// on advertising an older spelling would be teaching a grammar the code
/// refuses, and nothing would redden.
#[tokio::test]
async fn the_hint_and_the_template_spell_the_grammar_the_same_way() {
    let registry = roster();
    let team = registry.get("team").expect("`/team` is a builtin");
    let hint = team.argument_hint.clone().expect("`/team` carries a hint");

    assert!(
        expand("").await.contains(&hint),
        "the usage quotes the hint verbatim, so the two cannot come apart: {hint}",
    );
}

/// **Bead 2m46.** The three exact spellings never reach the model at all: they
/// are refused where the expansion would have begun, with the line that was
/// meant. A round trip to be told what three fixed words already say is a round
/// trip nobody should pay for a typo.
///
/// Asked of [`command::misdirected`] since **D549** moved the door out of the
/// expansion and into `Engine::run_command`, ahead of the grammar reading the
/// same line. That it is still consulted, and still first, is
/// `tests/team_spec.rs`'s to prove; what these rows are about is what it
/// decides.
#[test]
fn legacy_roster_arguments_are_refused_before_a_turn_starts() {
    for (legacy, meant) in [
        ("spawn worker-1", "/teammate spawn worker-1"),
        ("shutdown worker-1", "/teammate shutdown worker-1"),
        ("list", "/teammate list"),
    ] {
        let refused = command::misdirected(legacy).expect("a roster line is refused");
        assert_eq!(refused.meant, meant, "`/team {legacy}` names its own answer");
    }
}

/// And a **file** cannot take the name away from it, which is the other half
/// of that gate and the opposite outcome to the config tier's below.
///
/// `Registry::build` skips a command file whose name is a builtin's, with a
/// warning naming the file — so a `team.md` in either commands home leaves the
/// builtin, its `builtin` flag and therefore its refusal exactly where they
/// were. Worth its own test because the two tiers resolve the same collision
/// in opposite directions: a config entry *replaces* and the gate goes with
/// the name, a file is *dropped* and the gate stays. Only `/init` was covered
/// (`tests/command_files.rs`), and a change that let a file win would have
/// disabled bead 2m46's door silently.
///
/// Planted into the config home this binary already pins, which its tests
/// share: a skipped file changes no other roster here, and the day one stopped
/// being skipped every test in this binary reddening is the honest report.
///
/// What it can say about the gate itself ends at
/// [`command::Definition::builtin`] since **D549**: the flag is what
/// `Engine::run_command` reads, and an engine is what proves it is read —
/// `tests/team_spec.rs::a_project_that_owns_the_team_name_keeps_the_roster_spellings`
/// is where that half now lives.
#[tokio::test]
async fn a_command_file_named_after_the_builtin_leaves_its_gate_alone() {
    // Read back out of the variable `pin_homes` set, rather than rebuilt from
    // the same arithmetic, so the plant cannot land in a directory the loader
    // is not reading.
    pin_homes();
    let config_home =
        PathBuf::from(std::env::var_os(CONFIG_HOME_ENV).expect("`pin_homes` set the config home"));
    ganja_testkit::plant(&config_home, "commands/team.md", "run my own team playbook\n");

    let registry = roster();
    let team = registry.get("team").expect("the builtin survives a file wearing its name");

    assert!(team.builtin, "the file was skipped rather than layered over the builtin");
    assert!(team.source.is_none(), "and the surviving definition is not that file");
    assert!(
        !team.template.contains("run my own team playbook"),
        "the builtin's own text is what expands: {}",
        team.template,
    );
    assert!(
        command::misdirected("list").is_some(),
        "so the roster spellings the surviving builtin is gated on are still roster lines",
    );
}

/// And the template's redirect branch is still the fallback, because the
/// refusal above only knows three spellings: anything else that means the same
/// thing reaches the model with what was typed, whole, so it can answer with
/// the corrected line itself.
///
/// It survives **D549** on a vocabulary accident worth naming: `start` and
/// `who` happen to be in no agent roster, so neither line's head token is a
/// spec and both still reach `$ARGUMENTS` untouched. An agent named either
/// would turn these two rows into specs — the head token would be consumed,
/// and `expanded.contains(asked)` would redden — which is a true report about
/// this grammar rather than a fault in this test.
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

/// What reaches `$ARGUMENTS` is the **task**, whole and untokenized — and,
/// since **D549**, exactly the task and nothing else.
///
/// Asserted as the arguments block's whole content rather than as a
/// `contains`, which is the only way to pin the half that is new: the head
/// token and the flag were read by the code, so they are *gone* from what the
/// model is handed. A `contains` cannot see that — the raw typed line contains
/// the task too — so a `fill_template` that started handing the whole line
/// through again would go unnoticed by every other assertion in this binary.
/// The other half is unchanged and still load-bearing: whatever is left is the
/// person's own bytes, spacing and all, because the model reads it as prose.
#[tokio::test]
async fn the_whole_grammar_reaches_the_text_however_it_was_spelled() {
    for (typed, task) in [
        ("3 port the config loader", "port the config loader"),
        ("3:executor port the config loader", "port the config loader"),
        ("2:critic --backend claude find the leak", "find the leak"),
        ("--backend codex read the wire and report", "read the wire and report"),
        ("just do the thing", "just do the thing"),
    ] {
        assert_eq!(
            arguments_block(&expand_spec(typed).await),
            task,
            "`/team {typed}` hands the model its task and neither the head token nor the flag",
        );
    }
}

/// What the template put between its own argument markers.
///
/// The template names `$ARGUMENTS` exactly once, between a line holding
/// `<team-arguments>` and one holding `</team-arguments>`, so reading back what
/// landed there is reading back what the model is told the user typed. Named
/// because two tests ask it and because a `contains` over the whole prompt
/// cannot answer the question either of them is asking.
fn arguments_block(expanded: &str) -> &str {
    let (_, rest) = expanded.split_once("<team-arguments>\n").expect("the template opens a block");
    let (block, _) = rest.split_once("\n</team-arguments>").expect("and closes it");

    block
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
    // And through the only settlement that travels member→lead. The plan this
    // template came from told the lead to wait for `teammate_terminated` too,
    // which `teammate.rs` documents as the lead's word to a member and which
    // no path in this workspace originates in either direction: a step waiting
    // for it waits forever. Named as an absence rather than fixed once,
    // because a harness-only frame is exactly what a later edit reaches for
    // when it wants to say "and then it really went away".
    assert!(
        !expanded.contains("teammate_terminated"),
        "the lead is never told to wait for a frame nothing sends it: {expanded}",
    );
    // And the positive half of the same correction. A `ganja` or `claude`
    // pane's exit is mailed nowhere — `pane.rs` posts it on the registry
    // channel, which reaches the person's status bar and never the model — so
    // the only honest instruction is a bounded wait. Pinned on the words that
    // carry it, because a text that says only what does *not* arrive leaves
    // the lead holding the wait open.
    assert!(
        expanded.contains("stop waiting"),
        "a member that stopped answering is one to stop waiting on: {expanded}",
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

/// **AC-11.** `${members}` is filled at expansion like the session id, and
/// nothing is left standing.
///
/// The four placeholders reach the model by two different routes — two at
/// roster build, two at expansion — so the honest assertion is over the whole
/// prompt: no `${` of any kind survives a run that resolved a roster. A fifth
/// placeholder added to the template and filled nowhere would redden here,
/// which is what this is for.
#[tokio::test]
async fn no_placeholder_survives_a_spec_run() {
    let expanded = expand_spec("2:critic@claude,critic@codex port the loader").await;

    assert!(
        !expanded.contains("${"),
        "every placeholder is filled by the time the model reads this: {expanded}",
    );
    assert!(
        expanded.contains("critic-1 — critic on claude"),
        "and the roster the grammar resolved is what the block says: {expanded}",
    );
}

/// **AC-14.** The block sits inside team-exec's own spawn step, not in a
/// reference section further down.
///
/// Which is the whole of PM-2's mitigation: nothing enforces that the model
/// spawns these rows, so where the roster sits relative to the instruction that
/// spawns is the persuasion. A later edit that moved it under "Which agent runs
/// which stage" would be moving it out of the step that reads it.
#[tokio::test]
async fn the_members_block_is_named_where_the_spawn_step_reads_it() {
    let expanded = expand_spec("2:critic port the loader").await;

    let (before, _) = expanded
        .split_once("critic-1 — critic on ganja")
        .expect("the resolved roster is in the prompt");
    let (_, step) = before
        .rsplit_once("3. Spawn the teammates with the `task` tool")
        .expect("the block sits after the spawn step opens");

    assert!(
        !step.contains("\n## "),
        "and before the next section begins, so the roster is inside that step: {step:?}",
    );
}

/// **AC-13.** A spec-less `/team --backend codex <task>` still puts `codex` in
/// front of the model.
///
/// Driver 2's fifth shipped shape, and the reason `render_members` has a third
/// rendering at all: the line names nobody, so there is no roster to draw, but
/// the surface it named must not be lost between the flag and the spawn.
#[tokio::test]
async fn a_spec_less_surface_reaches_the_model_as_a_standing_surface() {
    let expanded = expand_spec("--backend codex read the wire and report").await;

    assert!(
        expanded.contains("Run every member on `codex`"),
        "the surface survives a line that named nobody: {expanded}",
    );
    assert!(
        expanded.contains("size the team yourself"),
        "and the model is still the one that sizes the team: {expanded}",
    );
    assert_eq!(
        arguments_block(&expanded),
        "read the wire and report",
        "with the flag taken out of the task",
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
