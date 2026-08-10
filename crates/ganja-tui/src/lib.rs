//! ratatui frontend for ganja.
//!
//! The crate owns every pixel and no engine logic: it turns terminal events
//! into [`Command`](ganja_protocol::Command)s and
//! [`Event`](ganja_protocol::Event)s into frames.

pub mod app;
pub mod clipboard;
pub mod command;
pub mod component;
pub mod event;
pub mod external;
pub mod history;
pub mod keybind;
pub(crate) mod markdown;
pub mod mention;
pub mod theme;
pub mod transcript;

use std::{
    io::stdout,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use ganja_core::{
    AgentRegistry, Engine, SessionId, Storage, catalog,
    config::{Config, Overrides, ThemeMode},
    instruction, provider,
};
use ganja_permission::Project;
use ganja_protocol::Message;
use ratatui::crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
};
use tokio_util::sync::CancellationToken;

use crate::{
    app::App,
    history::History,
    keybind::Keybinds,
    theme::{Mode, Themes},
};

/// Directory the session store lives in, under the project's data directory.
const STORAGE: &str = "storage";

/// Separates the things the status bar shows on its left.
pub(crate) const NOTICE_SEPARATOR: &str = " \u{b7} ";

/// Which stored session a run opens, when it opens one.
///
/// Naming a session is the caller's way of saying it wants *that*
/// conversation; nothing here quietly substitutes another one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resume {
    /// The most recently updated session in this project's store.
    Latest,
    /// The session with this stored id.
    Session(String),
}

/// Runs the interactive terminal UI until the user quits.
///
/// `resume` opens a stored session instead of starting a fresh one, and
/// `overrides` carries what the command line decided — the tier above every
/// config file and above the environment between them.
///
/// Everything that can refuse does so *before* the terminal is taken over: a
/// config file that will not parse, a key binding this build cannot read, a
/// provider it does not ship, an agent roster that leaves nothing to start on,
/// a resume naming a session that is not there. All of them reach the shell as
/// a readable error rather than flashing past inside the alternate screen.
///
/// The terminal is restored on every exit path, including a panic: the hook
/// installed here undoes bracketed paste and mouse capture and then defers to
/// the one [`ratatui::try_init`] installed, which leaves raw mode and the
/// alternate screen. MCP servers are **not** part of that hook — its work has
/// to be synchronous and closing them is not — so a panic leaves a local
/// server's process group standing until it notices its stdin has closed.
///
/// # Errors
///
/// Returns an error for any of the refusals above, and if the terminal cannot
/// be initialized, drawn to, read from, or restored.
pub async fn run(resume: Option<Resume>, overrides: Overrides) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = Config::load_with(&cwd, &overrides).context("failed to read the configuration")?;
    let keys =
        Keybinds::from_config(&config.keybinds).context("failed to read the key bindings")?;
    let selection = provider::select(&config).context("failed to select a provider")?;
    let agents = Arc::new(AgentRegistry::build(&config).context("failed to resolve the agents")?);
    // Captured before the provider is handed to the engine: the model list is
    // narrowed to this provider, and `Selection` gives it up on the move.
    let provider_id = selection.provider.id().to_owned();
    // Sessions live per project, so opening `src/` and opening the repository
    // root reach the same history.
    let project = Project::resolve(&cwd);
    let storage = Storage::open(
        project
            .data_dir()
            .context("failed to locate the project's data directory")?
            .join(STORAGE),
    );
    // `/init`'s template names the worktree it is being run in, so the roster
    // is resolved against the project root rather than against whichever
    // subdirectory the terminal happened to be opened in.
    let commands = Arc::new(ganja_core::command::Registry::build(
        &config,
        project.root(),
    ));
    // Configured MCP servers, none of them dialled yet. The **project root**
    // is what a relative `cwd` in an entry resolves against, not the directory
    // this process happens to have been started in: a server configured once
    // for the project has to start in the same place whichever subdirectory
    // its owner opened the terminal in.
    let servers = ganja_core::McpServers::new(config.mcp.clone(), project.root());
    // The **project root** again, and for the same reason: a language server's
    // idea of a workspace has to be the project's, not whichever subdirectory
    // the terminal was opened in. `None` when the config asked for no LSP,
    // which leaves the engine doing no LSP work rather than inert LSP work.
    let lsp = ganja_core::Lsp::new(config.lsp.as_ref(), project.root());
    // Probed here, before the first frame: whether this project can be
    // snapshotted at all decides what the status bar has to say about `/undo`,
    // and the answer costs one synchronous `git` probe.
    let snapshots = Arc::new(ganja_core::Snapshots::new(
        &project,
        config.snapshots_enabled(),
    ));
    let snapshot_notice = snapshots.notice().map(str::to_owned);
    // The registry carries every builtin tool the agent loop can execute;
    // permission rules load for the project the terminal was opened in.
    let mut tools = ganja_tool::Registry::with_builtins();
    // `webfetch` refuses a private address unless this session said otherwise,
    // and the config is the only place that can say so — which address on a
    // private network is a legitimate one to fetch is a question only the
    // person running the session can answer.
    if config.webfetch_allows_private() {
        tools = tools.with(Arc::new(
            ganja_tool::webfetch::WebfetchTool::allowing_private(),
        ));
    }
    // Over the top of the roster's rootless one, out of the **same** value the
    // prompt's `<available_skills>` block is built from below: a session that
    // is offered a skill has to be able to load it, and only a caller holding
    // the config and the directory can resolve where either half looks.
    tools = tools.with(Arc::new(ganja_tool::skill::SkillTool::over(
        instruction::skill_roots(&config, &cwd),
    )));
    let mut engine = Engine::persistent(
        selection.provider,
        selection.model,
        Arc::new(tools),
        ganja_permission::Permissions::load(&cwd),
        storage,
    )
    .with_agents(agents)
    .with_commands(commands)
    .with_mcp(Arc::clone(&servers))
    .with_snapshots(snapshots);
    if let Some(lsp) = lsp {
        engine = engine.with_lsp(lsp);
    }
    // Composed from the engine and not from the selection, and only once the
    // agents are on it: the default agent may have named a model of another
    // family, and the prompt has to be that model's.
    let (base, suffix) = system_parts(&engine, &config, &cwd);
    let engine = engine
        .with_system_parts(base, suffix)
        .with_environment({
            // Owned copies, because this outlives the startup that composed the
            // first one: the environment block names the model, and the model
            // moves when somebody picks another one mid-session.
            let config = config.clone();
            let cwd = cwd.clone();
            move |model| instruction::suffix(&config, &cwd, model)
        })
        // And the other half moves with it: the base prompt is chosen by the
        // model's family, so a switch from a `claude` model to a `gpt` one has
        // to change this too or the new model reads the old family's
        // instructions. Nothing is handed over — the engine composes this half
        // from the model's name alone.
        .with_base_for_model();

    // Dialled from here on, in the background: the first turn is offered
    // whichever servers have answered by the time it starts, and a server
    // that never answers costs its tools and a line of the status bar rather
    // than the startup this call returns straight out of (**R3**).
    engine.connect_mcp();
    // Filesystem events for the files this session reads, so a file edited in
    // another window is refused before the model acts on what it read and is
    // named to it at the top of the next turn. A watcher that will not start
    // is one warning and nothing else.
    engine.watch_files();

    let seed = match resume {
        Some(resume) => stored_transcript(&engine, resume).await?,
        None => Vec::new(),
    };

    // The builtins, the user's own themes, and the theme they last picked —
    // then whatever the config asks for on top, because a `theme` written in a
    // file outranks a runtime pick permanently rather than until the next one.
    let mut themes = Themes::load();
    let theme_notice = configure_themes(&mut themes, &config);

    // Prices come off the disk before the first frame — adoption happens on
    // this thread — and are kept current behind the loop for as long as the app
    // runs. Deliberately not a refusal: a catalog that could not be fetched
    // leaves the compiled-in snapshot standing, which is a session that prices
    // slightly stale rather than a session that does not start.
    let background = CancellationToken::new();
    catalog::spawn_refresh_loop(background.clone());

    // Spilled tool output older than a week is nobody's context any more, and
    // nothing else on this machine ever deletes it.
    ganja_tool::truncate::spawn_sweep_loop(background.clone());

    let mut terminal = ratatui::try_init().context("failed to initialize the terminal")?;
    let outcome = match capture_input() {
        Ok(()) => {
            // The model is the engine's to answer for, not the selection's:
            // the default agent may have named one of its own, and a resumed
            // session restores the one it was left on.
            let mut app = App::new(
                engine,
                notice(&[selection.notice, theme_notice, snapshot_notice]),
                themes,
            )
            .with_provider(provider_id)
            .with_keybinds(keys)
            // The one place the prompt history reaches the disk: the default
            // store is inert, so a test that does not opt in never touches the
            // machine's own history.
            .with_history(History::load())
            .with_root(project.root())
            // The `@` file menu walks from here, so a mention resolves against
            // the directory the user opened rather than the project root: what
            // they typed is relative to where they are standing.
            .with_cwd(cwd)
            .watching_mcp(config.mcp.len());
            app.seed(seed);
            app.run(&mut terminal).await
        }
        Err(error) => Err(error),
    };
    // Nothing is waiting on the loops, but a background task that outlives the
    // screen it was feeding is a leak whichever way the run ended.
    background.cancel();
    // Every local server's process group ends here. Through this handle rather
    // than through `Engine::shutdown_mcp`, which is the same call one layer
    // down: the engine moved into the app, and `App::run` consumes it.
    servers.shutdown().await;
    let restored = restore();

    outcome.and(restored)
}

/// The two halves of the system prompt `engine` runs under.
///
/// The model is asked of the engine rather than taken from the provider
/// selection, and that is the whole reason this takes an [`Engine`] instead of
/// a name. Both halves are written against a model id — the base half picks a
/// prompt by family (`claude` / `gpt` / neither), and the suffix's environment
/// block names the model twice — so composing them against the launch model
/// while the engine had already adopted the default agent's own would tell a
/// `gpt` session it was Claude, in a block that also states its model as fact.
/// This must therefore run **after** `with_agents`.
///
/// The base half is handed over **explicitly** rather than left to [`None`].
/// `Engine::system_for` resolves an agent's own prompt *or* the base one, and
/// both agents a session can start on — `build` and `plan` — deliberately have
/// no prompt of their own: what makes `plan` plan is a reminder injected per
/// turn, not a system prompt. Passing [`None`] here would leave every one of
/// their turns carrying the environment block and nothing else.
///
/// The suffix is the half no agent replaces, so an agent switch swaps the
/// first and keeps this one. This composes the pair the session starts on;
/// keeping each half current across a later model switch is the engine's
/// business — `Engine::with_environment` for the suffix and
/// `Engine::with_base_for_model` for the base — and the caller installs both
/// beside this.
fn system_parts(engine: &Engine, config: &Config, cwd: &Path) -> (Option<String>, Option<String>) {
    let model = engine.model();

    (
        Some(instruction::base_prompt(&model).to_owned()),
        instruction::suffix(config, cwd, &model),
    )
}

/// Applies `config`'s theme and mode, answering with anything worth saying
/// about it.
///
/// A `theme` naming something this build does not have leaves the default in
/// place and says so, rather than failing the run: a custom theme file the
/// user deleted should cost them that theme, exactly as a malformed one does,
/// and not their session (deviation: config-theme-unknown-is-a-notice).
fn configure_themes(themes: &mut Themes, config: &Config) -> Option<String> {
    // The mode goes first so that the selection below resolves in the arm the
    // config asked for rather than in the default one.
    if let Some(mode) = config.theme_mode {
        themes.set_mode(match mode {
            ThemeMode::Dark => Mode::Dark,
            ThemeMode::Light => Mode::Light,
        });
    }

    let name = config.theme.as_deref()?;
    if themes.select(name).is_none() {
        return Some(format!(
            "no theme named {name:?}; using {}",
            themes.active()
        ));
    }

    None
}

/// The status bar's opening line: whatever startup had to say, in one string.
///
/// Everything that has something to say gets a say, in the order it is passed:
/// a session can start on the fake provider, in a theme it could not find, in
/// a directory it cannot snapshot, and none of those three is a reason to
/// swallow the others.
fn notice(parts: &[Option<String>]) -> Option<String> {
    let said: Vec<&str> = parts.iter().filter_map(Option::as_deref).collect();

    (!said.is_empty()).then(|| said.join(NOTICE_SEPARATOR))
}

/// Installs the session `resume` names and hands back its transcript.
///
/// Split out of [`run`] because it is the one part of the startup path worth
/// testing on its own: everything around it needs a terminal, and what has to
/// be true here — that a resume either produces the session that was asked for
/// or fails loudly — is exactly what a silent fallback would break.
async fn stored_transcript(engine: &Engine, resume: Resume) -> Result<Vec<Message>> {
    let id = match resume {
        Resume::Latest => engine
            .sessions()
            .await
            .context("failed to list the stored sessions")?
            .into_iter()
            // `sessions()` answers newest first, so the latest is the first.
            .next()
            .map(|info| info.id)
            .context("there is no stored session to continue in this project")?,
        Resume::Session(id) => SessionId::from(id),
    };

    engine
        .resume(&id)
        .await
        .context("failed to resume the session")
}

/// Turns on wheel reporting and bracketed paste, and extends the panic hook to
/// turn both back off.
///
/// Bracketed paste is what makes a pasted paragraph one event instead of a
/// stream of keystrokes — without it, the newline in the middle of a paste is
/// an Enter, and Enter here sends the prompt. Left enabled on the way out it
/// would leave the user's shell wrapping their pastes in escapes nothing there
/// reads (**R13**).
fn capture_input() -> Result<()> {
    execute!(stdout(), EnableMouseCapture).context("failed to enable mouse reporting")?;
    execute!(stdout(), EnableBracketedPaste).context("failed to enable bracketed paste")?;

    let installed = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableBracketedPaste);
        let _ = execute!(stdout(), DisableMouseCapture);
        installed(info);
    }));

    Ok(())
}

fn restore() -> Result<()> {
    let paste =
        execute!(stdout(), DisableBracketedPaste).context("failed to disable bracketed paste");
    let mouse =
        execute!(stdout(), DisableMouseCapture).context("failed to disable mouse reporting");
    let terminal = ratatui::try_restore().context("failed to restore the terminal");

    paste.and(mouse).and(terminal)
}

#[cfg(test)]
mod tests {
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
            ganja_core::AgentRegistry::build(&config).expect("the fixture resolves an agent"),
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
            let owned: Vec<Option<String>> =
                parts.iter().map(|part| part.map(str::to_owned)).collect();

            assert_eq!(notice(&owned).as_deref(), expected, "{parts:?}");
        }
    }
}
