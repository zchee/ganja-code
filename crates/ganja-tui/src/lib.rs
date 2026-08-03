//! ratatui frontend for ganja.
//!
//! P0 ships the shell only: an alternate-screen app whose single
//! [`tokio::select!`] loop owns all UI state, renders a bordered
//! [`ratatui_textarea::TextArea`] plus a hint bar, and exits cleanly. The chat
//! viewport and the engine it talks to arrive with P1.

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::Stylize as _,
    text::Line,
    widgets::Block,
};
use ratatui_textarea::TextArea;

const HINT: &str = " q or Ctrl-C quits · every other key edits the message ";

/// Runs the interactive terminal UI until the user quits.
///
/// The terminal is restored on every exit path, including a panic: the hook
/// installed by [`ratatui::try_init`] runs ahead of any other panic handler.
///
/// # Errors
///
/// Returns an error if the terminal cannot be initialized, drawn to, read from,
/// or restored.
pub async fn run() -> Result<()> {
    let terminal = ratatui::try_init().context("failed to initialize the terminal")?;
    let outcome = event_loop(terminal).await;
    let restored = ratatui::try_restore().context("failed to restore the terminal");

    outcome.and(restored)
}

async fn event_loop(mut terminal: DefaultTerminal) -> Result<()> {
    let mut editor = TextArea::default();
    editor.set_block(Block::bordered().title(" message "));
    editor.set_placeholder_text("Ask ganja something…");

    let mut terminal_events = EventStream::new();

    loop {
        terminal
            .draw(|frame| draw(frame, &editor))
            .context("failed to draw a frame")?;

        tokio::select! {
            event = terminal_events.next() => {
                let Some(event) = event else {
                    // The event source closed; nothing left to react to.
                    break;
                };
                let event = event.context("failed to read a terminal event")?;

                if let Event::Key(key) = event
                    && key.kind != KeyEventKind::Release
                {
                    if is_quit(key) {
                        break;
                    }
                    editor.input(key);
                }
            }
            // Raw mode swallows Ctrl-C, so this arm only fires for a signal
            // raised from outside the terminal, such as `kill -INT`.
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    Ok(())
}

fn draw(frame: &mut Frame, editor: &TextArea) {
    let [editor_area, hint_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    frame.render_widget(editor, editor_area);
    frame.render_widget(Line::from(HINT).dim(), hint_area);
}

fn is_quit(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') => true,
        KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}
