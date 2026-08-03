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
use ganja_core::{Engine, provider};
use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};

use crate::app::App;

/// Runs the interactive terminal UI until the user quits.
///
/// The terminal is restored on every exit path, including a panic: the hook
/// installed here undoes mouse capture and then defers to the one
/// [`ratatui::try_init`] installed, which leaves raw mode and the alternate
/// screen.
///
/// # Errors
///
/// Returns an error if `GANJA_PROVIDER` names a provider this build does not
/// have, or if the terminal cannot be initialized, drawn to, read from, or
/// restored.
pub async fn run() -> Result<()> {
    let selection = provider::from_env().context("failed to select a provider")?;
    let engine = Engine::new(selection.provider, selection.model);

    let mut terminal = ratatui::try_init().context("failed to initialize the terminal")?;
    let outcome = match capture_mouse() {
        Ok(()) => App::new(engine, selection.notice).run(&mut terminal).await,
        Err(error) => Err(error),
    };
    let restored = restore();

    outcome.and(restored)
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
