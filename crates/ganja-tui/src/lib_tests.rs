use std::sync::Arc;

use ganja_core::{
    Engine, Storage,
    config::{Config, ThemeMode},
    provider::{FakeProvider, fake},
};
use ganja_protocol::Message;
use tempfile::TempDir;

use super::{Resume, configure_themes, notice, stored_transcript, system_parts};
use crate::theme::{Mode, Themes};

/// A persistent engine over an empty store in `directory`.
fn engine(directory: &TempDir) -> Engine {
    engine_asking(directory, fake::MODEL)
}

/// The same, launched on a model of the caller's choosing — which is what
/// the system-prompt tests need, since a prompt is picked by model family.
fn engine_asking(directory: &TempDir, model: &str) -> Engine {
    Engine::persistent(
        Arc::new(FakeProvider::default()),
        model,
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
}

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// Stores one session carrying `prompt`, and answers with its id.
fn stored(directory: &TempDir, prompt: &str) -> String {
    let storage = Storage::open(directory.path().join("storage"));
    let info = ganja_core::SessionInfo {
        effort: None,
        id: ganja_core::SessionId::ascending(),
        version: ganja_core::storage::VERSION,
        title: None,
        created: 1,
        updated: 1,
        usage: ganja_protocol::Usage::default(),
        context_tokens: 0,
        summary: None,
        agent: None,
        model: None,
        activated_tools: std::collections::BTreeSet::new(),
        parent: None,
        revert: None,
    };
    let message = Message::user(prompt);

    storage.save_info(&info).expect("the info stores");
    storage
        .save_message(&info.id, &message)
        .expect("the envelope stores");
    for part in &message.parts {
        storage
            .save_part(&info.id, &message.id, part)
            .expect("the part stores");
    }

    info.id.as_str().to_owned()
}

/// The whole point of naming a session: getting that one, or being told.
#[tokio::test]
async fn resuming_a_session_the_store_does_not_hold_fails_instead_of_starting_a_fresh_one() {
    let directory = temporary();
    stored(&directory, "a session that does exist");
    let engine = engine(&directory);

    let refusal = stored_transcript(&engine, Resume::Session("ses_missing".to_owned()))
        .await
        .expect_err("an unknown session must not resolve");

    assert!(
        format!("{refusal:#}").contains("ses_missing"),
        "the refusal should name what was asked for, got: {refusal:#}"
    );
    assert!(
        engine.current_session().is_none(),
        "a failed resume must not leave a session installed"
    );
}

#[tokio::test]
async fn continuing_with_nothing_stored_says_so_rather_than_opening_a_blank_session() {
    let directory = temporary();
    let engine = engine(&directory);

    let refusal = stored_transcript(&engine, Resume::Latest)
        .await
        .expect_err("an empty store has nothing to continue");

    assert!(
        format!("{refusal:#}").contains("no stored session"),
        "got: {refusal:#}"
    );
}

#[tokio::test]
async fn continuing_picks_the_newest_session_and_returns_its_transcript() {
    let directory = temporary();
    stored(&directory, "the older conversation");
    let newest = stored(&directory, "the newer conversation");
    let engine = engine(&directory);

    let transcript = stored_transcript(&engine, Resume::Latest)
        .await
        .expect("the newest session resumes");

    assert_eq!(
        engine
            .current_session()
            .map(|info| info.id.as_str().to_owned()),
        Some(newest),
        "the newest session should be the one installed"
    );
    assert_eq!(
        transcript
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(ganja_protocol::Part::as_text)
            .collect::<String>(),
        "the newer conversation"
    );
}

#[tokio::test]
async fn resuming_by_id_returns_that_session_rather_than_the_newest() {
    let directory = temporary();
    let older = stored(&directory, "the older conversation");
    stored(&directory, "the newer conversation");
    let engine = engine(&directory);

    let transcript = stored_transcript(&engine, Resume::Session(older.clone()))
        .await
        .expect("a stored session resumes");

    assert_eq!(
        engine
            .current_session()
            .map(|info| info.id.as_str().to_owned()),
        Some(older)
    );
    assert_eq!(
        transcript
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(ganja_protocol::Part::as_text)
            .collect::<String>(),
        "the older conversation"
    );
}

/// A registry whose stored pick is `stored`, read back through the same
/// file a previous run would have written.
fn with_stored_pick(directory: &TempDir, stored: &str) -> Themes {
    let store = directory.path().join("tui.json");

    let mut previous = Themes::builtin();
    previous.adopt_store(store.clone());
    previous
        .select(stored)
        .unwrap_or_else(|| panic!("{stored} should be a builtin theme"));
    previous.persist().expect("the pick stores");

    let mut themes = Themes::builtin();
    themes.adopt_store(store);

    themes
}

/// The whole point of resolving the config after the store: a `theme`
/// written in a file is a standing instruction, where a pick made in the
/// dialog is what to do until told otherwise. Dropping the `select` in
/// `configure_themes` fails this test.
#[test]
fn a_theme_named_in_the_config_outranks_the_one_that_was_last_picked() {
    let directory = temporary();
    let mut themes = with_stored_pick(&directory, "gruvbox");
    assert_eq!(themes.active(), "gruvbox", "the stored pick should load");

    let complaint = configure_themes(
        &mut themes,
        &Config {
            theme: Some("tokyonight".to_owned()),
            ..Config::default()
        },
    );

    assert_eq!(complaint, None);
    assert_eq!(themes.active(), "tokyonight");
}

#[test]
fn a_stored_pick_stands_when_the_config_names_no_theme() {
    let directory = temporary();
    let mut themes = with_stored_pick(&directory, "aura");

    assert_eq!(configure_themes(&mut themes, &Config::default()), None);
    assert_eq!(themes.active(), "aura");
}

/// **D3**: ganja has no terminal auto-detection, so the config key is the
/// only thing that moves off dark.
#[test]
fn the_configured_mode_is_the_arm_themes_resolve_in_and_dark_is_the_default() {
    let mut themes = Themes::builtin();
    assert_eq!(themes.mode(), Mode::Dark);

    configure_themes(
        &mut themes,
        &Config {
            theme_mode: Some(ThemeMode::Light),
            ..Config::default()
        },
    );

    assert_eq!(themes.mode(), Mode::Light);
}

/// A custom theme the user deleted should cost them that theme, not their
/// session — the same call the loader makes for one that will not parse.
#[test]
fn a_configured_theme_this_build_does_not_have_is_reported_rather_than_fatal() {
    let mut themes = Themes::builtin();

    let complaint = configure_themes(
        &mut themes,
        &Config {
            theme: Some("a-theme-nobody-shipped".to_owned()),
            ..Config::default()
        },
    )
    .expect("an unknown theme should be worth saying something about");

    assert!(
        complaint.contains("a-theme-nobody-shipped"),
        "the complaint should name it: {complaint}"
    );
    assert_eq!(themes.active(), crate::theme::DEFAULT_THEME);
}

/// The engine resolves an agent's own prompt *or* the base one, and the
/// two agents a session can start on have no prompt of their own — so the
/// base half has to be handed over rather than left to [`None`], which
/// would leave their turns carrying the environment block alone.
#[test]
fn the_system_prompt_carries_the_base_half_a_promptless_agent_falls_back_to() {
    let directory = temporary();

    for model in ["claude-sonnet-5", "gpt-5.6", "something-else"] {
        let engine = engine_asking(&directory, model);
        let (base, suffix) = system_parts(&engine, &Config::default(), directory.path());

        assert_eq!(
            base.as_deref(),
            Some(ganja_core::instruction::base_prompt(model)),
            "{model} should carry its family's prompt"
        );
        assert!(
            base.is_some_and(|base| !base.trim().is_empty()),
            "{model}: an empty base prompt would pass the check above and say nothing"
        );
        assert!(
            suffix.is_some(),
            "{model}: the environment block always says something"
        );
    }
}

/// **Non-vacuity target for composing the prompt after the agents.** The
/// launch model is Claude's and the default agent names one of OpenAI's,
/// so the two families disagree and only one of them is the model that
/// will actually be asked. Composing against the launch model — what the
/// startup path did before — hands a GPT session Anthropic's prompt, and
/// an environment block that states the wrong model as fact twice over.
#[test]
fn the_system_prompt_is_composed_for_the_model_the_agents_left_the_engine_on() {
    const LAUNCH: &str = "claude-sonnet-5";
    const ADOPTED: &str = "gpt-5.6";

    let directory = temporary();
    let config: Config = serde_json::from_value(serde_json::json!({
        "default_agent": "review",
        "agent": { "review": { "mode": "primary", "model": format!("openai/{ADOPTED}") } }
    }))
    .expect("the fixture is a config");
    let engine = engine_asking(&directory, LAUNCH).with_agents(Arc::new(
        ganja_core::AgentRegistry::from_config(&config).expect("the fixture resolves an agent"),
    ));
    assert_eq!(
        engine.model(),
        ADOPTED,
        "the fixture only proves anything while the agent moves the engine off the launch model"
    );

    let (base, suffix) = system_parts(&engine, &config, directory.path());

    assert_eq!(
        base.as_deref(),
        Some(ganja_core::instruction::base_prompt(ADOPTED)),
        "the base half is the adopted model's family"
    );
    assert_ne!(
        ganja_core::instruction::base_prompt(ADOPTED),
        ganja_core::instruction::base_prompt(LAUNCH),
        "the two families must really differ, or the assertion above proves nothing"
    );

    let suffix = suffix.expect("the environment block always says something");
    assert!(
        suffix.contains(ADOPTED),
        "the environment block names the model that will be asked: {suffix}"
    );
    assert!(
        !suffix.contains(LAUNCH),
        "and never the one it was launched with: {suffix}"
    );
}

#[test]
fn the_opening_notice_carries_whatever_startup_had_to_say() {
    let cases: [(&[Option<&str>], Option<&str>); 6] = [
        (&[None, None, None], None),
        (&[Some("provider"), None, None], Some("provider")),
        (&[None, Some("theme"), None], Some("theme")),
        (&[None, None, Some("no git")], Some("no git")),
        (
            &[Some("provider"), Some("theme"), None],
            Some("provider \u{b7} theme"),
        ),
        (
            &[Some("provider"), Some("theme"), Some("no git")],
            Some("provider \u{b7} theme \u{b7} no git"),
        ),
    ];

    for (parts, expected) in cases {
        let owned: Vec<Option<String>> = parts.iter().map(|part| part.map(str::to_owned)).collect();

        assert_eq!(notice(&owned).as_deref(), expected, "{parts:?}");
    }
}

// ---- D533/AC-11: which assemblies ask the binder ----

/// The gate the engine assembly branches on, spelled here exactly as
/// `run` spells it — the pin's whole point (**AC-11**, narrowed by user
/// ruling 2026-08-27, OQ1 option (b)): the predicate is asserted **once, by
/// name**, so a later widening or narrowing of it reddens this test instead
/// of quietly changing which sessions become addressable.
const BIND_GATE: &str = "ganja_core::config::config_home().filter(|_| membership.is_none())";

/// **AC-11**, as the user's ruling narrowed it to a regression pin: the
/// three assemblies this build can actually reach, and the predicate that
/// decides between them.
///
/// - **(ii)** An interactive non-member assembly *with* a config home asks
///   the binder — byte-identically to what it has done since **D527**, with
///   zero teammates or a hundred, since the gate does not ask about the
///   roster. Pinned so a later change to the population cannot regress the
///   ordinary case silently.
/// - **(iii)** A **member pane** binds nothing: it is addressed through its
///   lead's team, by the same line that keeps it from leading one (**D505**).
/// - **(iv)** A **headless** turn binds nothing, for a reason no gate in this
///   file could express — it never enters this crate at all. Asserted where
///   the fact lives: `ganja-cli`'s headless driver names no binder.
///
/// The no-config-home arm — v1 of the plan's case (i) — is **not** here: the
/// user's ruling selected option (b), so the solo receiving surface was not
/// built and that arm still binds nothing.
///
/// This is a source pin rather than a behavioral drill because `run` opens a
/// real terminal: there is no headless seam in that function to drive the
/// assembly match from, the same gap `lib.rs`'s own reaper comments record.
#[test]
fn the_bind_predicate_is_interactive_non_member() {
    let source = include_str!("lib.rs");

    assert_eq!(
        source.matches(BIND_GATE).count(),
        1,
        "the bind predicate is spelled once, and this is that spelling"
    );
    // Two conditions, and no more: a config home, and not being a member. A
    // third would have to be added to that line, which is what this counts.
    let gate_line = source
        .lines()
        .find(|line| line.contains(BIND_GATE))
        .expect("the gate is on a line of its own");
    assert!(
        gate_line.trim().starts_with("match ") && gate_line.trim().ends_with('{'),
        "the gate is the match's own scrutinee, not one arm of a wider test: {gate_line}"
    );

    // (ii) The binder is consumed at exactly one place in this file, and that
    // place is inside the arm the gate's `Some(home)` opens.
    assert_eq!(
        source.matches("binder.map(|binder| {").count(),
        1,
        "the binder is asked for in one arm only"
    );
    let gate_at = source.find(BIND_GATE).expect("the gate is in this file");
    let member_at = source
        .find("None if let Some((membership, _)) = &membership =>")
        .expect("the member arm is in this file");
    let binder_at = source
        .find("binder.map(|binder| {")
        .expect("the binder is asked for in this file");
    assert!(
        gate_at < binder_at && binder_at < member_at,
        "the one hand-in sits in the config-home, non-member arm, ahead of the member arm"
    );

    // (iii) The member arm hands back no socket at all — its tuple's third
    // slot is `None`, and so is the no-config-home arm's.
    let member_arm = &source[member_at..];
    let member_arm = &member_arm[..member_arm
        .find("None => {")
        .expect("the no-config-home arm follows the member arm")];
    assert!(
        !member_arm.contains("binder"),
        "a member pane binds nothing: {member_arm}"
    );
    assert!(
        source.contains("(engine.with_solo_postbox(), None, None)"),
        "the no-config-home arm binds nothing either, and OQ1(b) left it that way"
    );

    // (iv) A headless turn never reaches this crate: `ganja-cli`'s own
    // headless driver names no binder anywhere.
    let headless = include_str!("../../ganja-cli/src/run.rs");
    assert!(
        !headless.contains("binder") && !headless.contains("Binder"),
        "a headless turn installs no teammates and binds no socket"
    );
}
