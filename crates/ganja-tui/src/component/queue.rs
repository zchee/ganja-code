//! The strip of messages waiting above the composer.
//!
//! What lands here is everything typed while a turn already held the engine
//! (**F4**). Most of it is *steered*: handed to the running turn as a
//! `Command::Steer` that the engine will take on at its next step boundary,
//! and rendered until an `Event::SteerConsumed` names its id. The rest is the
//! fallback lane — a slash command, which never steers because it is not a
//! message the model reads; a steer the engine refused because the turn had
//! already ended; a steer still unconsumed when the turn finished — and those
//! are replayed as ordinary prompts once the engine is idle again.
//!
//! Spec: Codex `codex-rs` TUI `chatwidget/input_queue.rs` for the queue half of
//! the same design (its `queued_user_messages` holds exactly what cannot be
//! injected), and Claude Code's own footer for the presentation — a dimmed
//! list above the composer with one hint line under it.
//!
//! **A queued entry is never silent.** Its text is on screen for as long as
//! anything owns it, and the status bar carries the depth: an entry that got
//! stuck would be a visible bug rather than a message that vanished.
//!
//! # A teammate's words are a third kind of row
//!
//! A peer's message waits here too (**D503**), and it is not the person's to
//! take back: recalling one into the composer would put words nobody typed
//! where Enter reads them as consent — for the `@` mentions, `$` skills and
//! `/` commands in them (§7-5). So the lane a row belongs to is a **field**
//! rather than something a caller remembers, and [`Queue::withdraw_newest`]
//! answers with the newest row this person really wrote.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Clear, Paragraph, Widget as _};

use crate::component::chat::clip;
use crate::theme::Theme;

/// The line under the list, in Claude Code's own words.
const HINT: &str = "press up to edit queued messages";

/// Rows of queued text the strip shows at once. Beyond this the strip stops
/// growing and the status bar's depth is what says how many there really are:
/// the composer must not be pushed off a short terminal by its own queue.
const MAX_ROWS: usize = 5;

/// What each row is marked with.
const MARKER: &str = "\u{2502} ";

/// Which lane a row belongs to, which is to say **who wrote it**.
///
/// A field rather than a caller's memory, because the one thing that must
/// never happen to a peer's words is that they end up in the composer as
/// though this person had typed them (§7-5): a `String` walking around with no
/// mark on it is exactly how that happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    /// The person at this terminal typed it.
    Typed,
    /// A teammate wrote it, and it reached here through the lead's mailbox
    /// (**D503**).
    Peer,
}

/// One message waiting to reach the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The correlation id minted for it. A steered entry is retired when
    /// `Event::SteerConsumed` names this; a fallback entry keeps it only so
    /// every row on screen has one identity for its whole life.
    ///
    /// Not unique per row: one `Command::Steer` carries a whole pass of a
    /// teammate's messages under one id, and the event that names it retires
    /// every row it stood for at once.
    pub id: String,
    /// What was written, verbatim — including its `@` and `/` tokens, which
    /// are resolved when a *typed* entry actually reaches the engine and never
    /// for a peer's.
    pub text: String,
    /// Whether the engine took this as a steer and has yet to say it consumed
    /// it. `false` is the fallback lane: nothing is waiting on the engine, and
    /// this entry is replayed as a prompt at the end of the turn.
    steered: bool,
    /// Who wrote it.
    lane: Lane,
}

impl Entry {
    /// Whether this entry is waiting on the engine rather than on the next
    /// idle moment.
    #[cfg(test)]
    #[must_use]
    pub fn is_steered(&self) -> bool {
        self.steered
    }
}

/// Everything typed while a turn was running, oldest first.
#[derive(Debug, Default)]
pub struct Queue {
    entries: Vec<Entry>,
}

impl Queue {
    /// Records a message the engine accepted as a steer.
    pub fn push_steered(&mut self, id: String, text: String) {
        self.entries.push(Entry { id, text, steered: true, lane: Lane::Typed });
    }

    /// Records a teammate's message the engine accepted as a steer (**D503**).
    ///
    /// The same row as [`Queue::push_steered`]'s in every way a person can
    /// see — the words are what they are looking for — and a different one in
    /// the only way that matters: Up will not hand it back to them.
    pub fn push_peer(&mut self, id: String, text: String) {
        self.entries.push(Entry { id, text, steered: true, lane: Lane::Peer });
    }

    /// Records a message nothing is going to steer, to be replayed as a prompt
    /// once the engine is idle.
    ///
    /// Typed by construction: the replay lane resolves mentions and matches
    /// command names, which a peer's words consent to none of, so a peer's
    /// message is given back to its mailbox instead (§7-5).
    pub fn push_fallback(&mut self, id: String, text: String) {
        self.entries.push(Entry { id, text, steered: false, lane: Lane::Typed });
    }

    /// Puts `entry` back at the front, for a replay the engine refused because
    /// a turn started underneath it. Front rather than back so the queue keeps
    /// the order the user typed in.
    pub fn requeue_front(&mut self, entry: Entry) {
        self.entries.insert(0, entry);
    }

    /// Retires the entry `id` names, and says whether one was there.
    ///
    /// A miss is ordinary rather than exceptional: an entry withdrawn by an Up
    /// arrow is already gone by the time the engine says it consumed it, and
    /// that race is the documented one — the message lands once, and the
    /// recalled text in the composer is the user's to resend or discard.
    pub fn consume(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);

        self.entries.len() != before
    }

    /// Turns every **typed** entry still waiting on the engine into a fallback
    /// one, which is what the end of a turn means for them: no turn is left to
    /// drain a steer, so whatever it did not take is replayed as a prompt.
    ///
    /// A peer's row is left where it is, and that is the structural half of
    /// §7-5: the replay lane resolves mentions, loads skills and matches
    /// command names, so a teammate's words must never reach it. The app takes
    /// those rows back separately and lets the mailbox re-offer them — a row
    /// this missed would sit on the strip rather than be replayed, which is the
    /// safe way for that to go wrong.
    pub fn strand(&mut self) {
        for entry in &mut self.entries {
            if entry.lane == Lane::Typed {
                entry.steered = false;
            }
        }
    }

    /// Takes the oldest entry the fallback lane owns, if there is one.
    pub fn take_next_fallback(&mut self) -> Option<Entry> {
        let index = self.entries.iter().position(|entry| !entry.steered)?;

        Some(self.entries.remove(index))
    }

    /// Takes the newest entry **this person wrote** back for editing.
    ///
    /// A steered entry cannot be un-sent — there is no command that takes a
    /// steer back, and inventing one would be a second contract to race — so
    /// what a withdrawal does is drop the *rendered* claim on it. If the
    /// engine consumes it anyway it lands exactly once, in the transcript,
    /// where the person can see it.
    ///
    /// A teammate's row is passed over rather than popped (**§7-5**): the
    /// composer is a consent surface — Enter there resolves `@` mentions,
    /// loads `$` skills and runs `/` commands — and words nobody at this
    /// terminal typed may not be put in front of it. So a strip whose newest
    /// row is a peer's hands back the newest one under it, and a strip holding
    /// nothing else hands back [`None`], which is an Up arrow falling through
    /// to the history walk exactly as an empty strip does. Whoever wrote the
    /// peer row keeps its id, so a withdrawal can never orphan one either.
    pub fn withdraw_newest(&mut self) -> Option<Entry> {
        let index = self.entries.iter().rposition(|entry| entry.lane == Lane::Typed)?;

        Some(self.entries.remove(index))
    }

    /// Whether anything is waiting to be replayed as a prompt.
    #[must_use]
    pub fn has_fallback(&self) -> bool {
        self.entries.iter().any(|entry| !entry.steered)
    }

    /// How many messages are waiting, for the status bar to name.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.entries.len()
    }

    /// Whether there is nothing waiting at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, oldest first — what the strip renders and what a test
    /// reads back.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Draws the strip directly above `anchor`, which is the editor's area.
    ///
    /// Nothing is drawn while the queue is empty, so a session that never
    /// queues anything renders exactly the frame it always did.
    pub fn render(&self, anchor: Rect, buffer: &mut Buffer, theme: &Theme) {
        if self.entries.is_empty() || anchor.width == 0 {
            return;
        }

        // One row per shown entry plus the hint line under them.
        let rows = self.entries.len().min(MAX_ROWS).saturating_add(1);
        let height = u16::try_from(rows).unwrap_or(u16::MAX).min(anchor.y);
        // Below two rows there is no room for even one entry and its hint, and
        // half a strip says less than none.
        if height < 2 {
            return;
        }

        let area =
            Rect { x: anchor.x, y: anchor.y.saturating_sub(height), width: anchor.width, height };
        Clear.render(area, buffer);

        let shown = usize::from(height).saturating_sub(1);
        Paragraph::new(Text::from(self.lines(usize::from(area.width), shown, theme)))
            .style(theme.background_panel)
            .render(area, buffer);
    }

    /// The visible rows: the newest `rows` entries, then the hint.
    ///
    /// Newest rather than oldest, because the newest is what an Up arrow takes
    /// back and a list whose bottom row is not the one the key acts on would
    /// be lying about which. A peer's row is the one exception — the key skips
    /// it, since it is nobody here's to edit — and it is drawn in place anyway,
    /// because the strip's job is to say what is waiting and a message hidden
    /// to keep a hint line true would be the worse lie.
    fn lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let first = self.entries.len().saturating_sub(rows);
        let mut lines: Vec<Line<'static>> = self.entries[first..]
            .iter()
            .map(|entry| {
                // A queued message may be several lines; the strip is a
                // reminder of what is waiting rather than a second transcript,
                // so each entry is its first line, clipped.
                let head = entry.text.lines().next().unwrap_or_default();

                Line::styled(clip(&format!("{MARKER}{head}"), width), theme.dim)
            })
            .collect();
        lines.push(Line::styled(clip(HINT, width), theme.dim));

        lines
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
