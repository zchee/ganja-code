//! ratatui frontend for ganja.
//!
//! The crate owns every pixel and no engine logic: it turns terminal events
//! into [`Command`](ganja_core::Command)s and
//! [`Event`](ganja_core::Event)s into frames.

pub mod app;
pub mod component;
pub mod event;
pub mod theme;

use std::io::stdout;

use anyhow::{Context as _, Result};
use ganja_core::{Engine, Message, Project, SessionId, Storage, provider};
use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};

use crate::{app::App, theme::Themes};

/// Directory the session store lives in, under the project's data directory.
const STORAGE: &str = "storage";

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
/// `resume` opens a stored session instead of starting a fresh one. It is
/// resolved *before* the terminal is taken over, so a resume that cannot be
/// honored reaches the shell as a readable error rather than flashing past
/// inside the alternate screen.
///
/// The terminal is restored on every exit path, including a panic: the hook
/// installed here undoes mouse capture and then defers to the one
/// [`ratatui::try_init`] installed, which leaves raw mode and the alternate
/// screen.
///
/// # Errors
///
/// Returns an error if `GANJA_PROVIDER` names a provider this build does not
/// have, if `resume` names a session this project's store does not hold, or if
/// the terminal cannot be initialized, drawn to, read from, or restored.
pub async fn run(resume: Option<Resume>) -> Result<()> {
    let selection = provider::from_env().context("failed to select a provider")?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Sessions live per project, so opening `src/` and opening the repository
    // root reach the same history.
    let storage = Storage::open(
        Project::resolve(&cwd)
            .data_dir()
            .context("failed to locate the project's data directory")?
            .join(STORAGE),
    );
    // The frontend keeps its own copy of the model so that it can price a turn
    // without reaching into the engine for the model it was built with.
    //
    // The registry carries every builtin tool the agent loop can execute;
    // permission rules load for the project the terminal was opened in.
    let engine = Engine::persistent(
        selection.provider,
        selection.model.clone(),
        std::sync::Arc::new(ganja_core::Registry::with_builtins()),
        ganja_core::Permissions::load(&cwd),
        storage,
    );

    let seed = match resume {
        Some(resume) => stored_transcript(&engine, resume).await?,
        None => Vec::new(),
    };

    // The builtins, the user's own themes, and the theme they last picked.
    // Resolved before the terminal is taken over, like the resume above: a
    // warning about a theme file that would not load is worth reading, and it
    // is unreadable once the alternate screen is up.
    let themes = Themes::load();

    let mut terminal = ratatui::try_init().context("failed to initialize the terminal")?;
    let outcome = match capture_mouse() {
        Ok(()) => {
            let mut app = App::new(engine, selection.model, selection.notice, themes);
            app.seed(seed);
            app.run(&mut terminal).await
        }
        Err(error) => Err(error),
    };
    let restored = restore();

    outcome.and(restored)
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
        provider::{FakeProvider, fake},
    };
    use tempfile::TempDir;

    use super::{Resume, stored_transcript};

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
}
