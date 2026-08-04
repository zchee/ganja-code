//! ratatui frontend for ganja.
//!
//! The crate owns every pixel and no engine logic: it turns terminal events
//! into [`Command`](ganja_core::Command)s and
//! [`Event`](ganja_core::Event)s into frames.

pub mod app;
pub mod command;
pub mod component;
pub mod event;
pub mod external;
pub mod keybind;
pub mod mention;
pub mod theme;

use std::{
    io::stdout,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use ganja_core::{
    AgentRegistry, Engine, Message, Project, SessionId, Storage, catalog,
    config::{Config, Overrides, ThemeMode},
    instruction, provider,
};
use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use tokio_util::sync::CancellationToken;

use crate::{
    app::App,
    keybind::Keybinds,
    theme::{Mode, Themes},
};

/// Directory the session store lives in, under the project's data directory.
const STORAGE: &str = "storage";

/// Separates the things the status bar shows on its left.
const NOTICE_SEPARATOR: &str = " \u{b7} ";

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
/// installed here undoes mouse capture and then defers to the one
/// [`ratatui::try_init`] installed, which leaves raw mode and the alternate
/// screen.
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
    // The frontend keeps its own copy of the model so that it can price a turn
    // without reaching into the engine for the model it was built with.
    //
    // The registry carries every builtin tool the agent loop can execute;
    // permission rules load for the project the terminal was opened in.
    let engine = Engine::persistent(
        selection.provider,
        selection.model.clone(),
        Arc::new(ganja_core::Registry::with_builtins()),
        ganja_core::Permissions::load(&cwd),
        storage,
    )
    .with_agents(agents)
    .with_commands(commands);
    let (base, suffix) = system_parts(&config, &cwd, &selection.model);
    let engine = engine.with_system_parts(base, suffix);

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
    ganja_core::tool::truncate::spawn_sweep_loop(background.clone());

    let mut terminal = ratatui::try_init().context("failed to initialize the terminal")?;
    let outcome = match capture_mouse() {
        Ok(()) => {
            let mut app = App::new(
                engine,
                selection.model,
                notice(selection.notice, theme_notice),
                themes,
            )
            .with_provider(provider_id)
            .with_keybinds(keys)
            // The `@` file menu walks from here, so a mention resolves against
            // the directory the user opened rather than the project root: what
            // they typed is relative to where they are standing.
            .with_cwd(cwd);
            app.seed(seed);
            app.run(&mut terminal).await
        }
        Err(error) => Err(error),
    };
    // Nothing is waiting on the loops, but a background task that outlives the
    // screen it was feeding is a leak whichever way the run ended.
    background.cancel();
    let restored = restore();

    outcome.and(restored)
}

/// The two halves of the system prompt a session runs under.
///
/// The base half is handed over **explicitly** rather than left to [`None`].
/// `Engine::system_for` resolves an agent's own prompt *or* the base one, and
/// both agents a session can start on — `build` and `plan` — deliberately have
/// no prompt of their own: what makes `plan` plan is a reminder injected per
/// turn, not a system prompt. Passing [`None`] here would leave every one of
/// their turns carrying the environment block and nothing else.
///
/// The suffix is the half no agent replaces, so a switch swaps the first and
/// keeps this one.
fn system_parts(config: &Config, cwd: &Path, model: &str) -> (Option<String>, Option<String>) {
    (
        Some(instruction::base_prompt(model).to_owned()),
        instruction::suffix(config, cwd, model),
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
fn notice(provider: Option<String>, theme: Option<String>) -> Option<String> {
    match (provider, theme) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only),
        (Some(provider), Some(theme)) => Some(format!("{provider}{NOTICE_SEPARATOR}{theme}")),
    }
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

/// Turns on wheel reporting and extends the panic hook to turn it back off.
fn capture_mouse() -> Result<()> {
    execute!(stdout(), EnableMouseCapture).context("failed to enable mouse reporting")?;

    let installed = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        installed(info);
    }));

    Ok(())
}

fn restore() -> Result<()> {
    let mouse =
        execute!(stdout(), DisableMouseCapture).context("failed to disable mouse reporting");
    let terminal = ratatui::try_restore().context("failed to restore the terminal");

    mouse.and(terminal)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ganja_core::{
        Engine, Message, Storage,
        config::{Config, ThemeMode},
        provider::{FakeProvider, fake},
    };
    use tempfile::TempDir;

    use super::{Resume, configure_themes, notice, stored_transcript, system_parts};
    use crate::theme::{Mode, Themes};

    /// A persistent engine over an empty store in `directory`.
    fn engine(directory: &TempDir) -> Engine {
        Engine::persistent(
            Arc::new(FakeProvider::default()),
            fake::MODEL,
            Arc::new(ganja_core::Registry::new(Vec::new())),
            ganja_core::Permissions::default(),
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
            usage: ganja_core::Usage::default(),
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            parent: None,
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
                .filter_map(ganja_core::Part::as_text)
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
                .filter_map(ganja_core::Part::as_text)
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
            let (base, suffix) = system_parts(&Config::default(), directory.path(), model);

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

    #[test]
    fn the_opening_notice_carries_whatever_startup_had_to_say() {
        let cases = [
            (None, None, None),
            (Some("provider"), None, Some("provider")),
            (None, Some("theme"), Some("theme")),
            (
                Some("provider"),
                Some("theme"),
                Some("provider \u{b7} theme"),
            ),
        ];

        for (provider, theme, expected) in cases {
            assert_eq!(
                notice(provider.map(str::to_owned), theme.map(str::to_owned)).as_deref(),
                expected
            );
        }
    }
}
