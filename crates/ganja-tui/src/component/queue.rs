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

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Text},
    widgets::{Clear, Paragraph, Widget as _},
};

use crate::{component::chat::clip, theme::Theme};

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
        self.entries.push(Entry {
            id,
            text,
            steered: true,
            lane: Lane::Typed,
        });
    }

    /// Records a teammate's message the engine accepted as a steer (**D503**).
    ///
    /// The same row as [`Queue::push_steered`]'s in every way a person can
    /// see — the words are what they are looking for — and a different one in
    /// the only way that matters: Up will not hand it back to them.
    pub fn push_peer(&mut self, id: String, text: String) {
        self.entries.push(Entry {
            id,
            text,
            steered: true,
            lane: Lane::Peer,
        });
    }

    /// Records a message nothing is going to steer, to be replayed as a prompt
    /// once the engine is idle.
    ///
    /// Typed by construction: the replay lane resolves mentions and matches
    /// command names, which a peer's words consent to none of, so a peer's
    /// message is given back to its mailbox instead (§7-5).
    pub fn push_fallback(&mut self, id: String, text: String) {
        self.entries.push(Entry {
            id,
            text,
            steered: false,
            lane: Lane::Typed,
        });
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
        let index = self
            .entries
            .iter()
            .rposition(|entry| entry.lane == Lane::Typed)?;

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

        let area = Rect {
            x: anchor.x,
            y: anchor.y.saturating_sub(height),
            width: anchor.width,
            height,
        };
        Clear.render(area, buffer);

        let shown = usize::from(height).saturating_sub(1);
        Paragraph::new(Text::from(self.lines(
            usize::from(area.width),
            shown,
            theme,
        )))
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
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{HINT, Queue};
    use crate::theme::Theme;

    /// Renders the strip over an editor anchored at the bottom of a `height`
    /// row screen, and hands back what it wrote.
    fn rendered(queue: &Queue, width: u16, height: u16) -> String {
        let anchor = Rect::new(0, height - 3, width, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        queue.render(anchor, &mut buffer, &Theme::default());

        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_empty_queue_draws_nothing_at_all() {
        let queue = Queue::default();

        assert_eq!(rendered(&queue, 40, 10).trim(), "");
        assert!(queue.is_empty());
        assert_eq!(queue.depth(), 0);
    }

    #[test]
    fn a_queued_message_is_on_screen_with_the_hint_under_it() {
        let mut queue = Queue::default();
        queue.push_steered("steer-1".to_owned(), "use the other file".to_owned());

        let screen = rendered(&queue, 60, 10);

        assert!(screen.contains("use the other file"), "got:\n{screen}");
        assert!(screen.contains(HINT), "got:\n{screen}");
        assert_eq!(queue.depth(), 1);
    }

    /// The strip is a reminder, not a second transcript: a multi-line entry
    /// takes one row.
    #[test]
    fn a_multi_line_entry_shows_only_its_first_line() {
        let mut queue = Queue::default();
        queue.push_steered("steer-1".to_owned(), "first line\nsecond line".to_owned());

        let screen = rendered(&queue, 60, 10);

        assert!(screen.contains("first line"), "got:\n{screen}");
        assert!(!screen.contains("second line"), "got:\n{screen}");
    }

    /// The composer must survive its own queue: past the cap the strip stops
    /// growing and the newest entries are the ones kept, because the newest is
    /// what an Up arrow acts on.
    #[test]
    fn a_long_queue_stops_growing_and_keeps_the_newest_entries() {
        let mut queue = Queue::default();
        for index in 0..9 {
            queue.push_steered(format!("steer-{index}"), format!("message {index}"));
        }

        let screen = rendered(&queue, 60, 20);

        assert_eq!(queue.depth(), 9);
        assert!(screen.contains("message 8"), "got:\n{screen}");
        assert!(!screen.contains("message 0"), "got:\n{screen}");
        assert_eq!(
            screen.lines().filter(|line| !line.is_empty()).count(),
            6,
            "five entries and the hint, and no more:\n{screen}"
        );
    }

    /// A screen with no room above the composer draws nothing rather than half
    /// a strip.
    #[test]
    fn a_screen_with_no_room_above_the_composer_draws_nothing() {
        let mut queue = Queue::default();
        queue.push_steered("steer-1".to_owned(), "waiting".to_owned());

        assert_eq!(rendered(&queue, 40, 4).trim(), "");
    }

    #[test]
    fn consuming_an_id_retires_exactly_that_entry_and_a_miss_is_ordinary() {
        let mut queue = Queue::default();
        queue.push_steered("steer-1".to_owned(), "first".to_owned());
        queue.push_steered("steer-2".to_owned(), "second".to_owned());

        assert!(queue.consume("steer-1"));
        assert_eq!(queue.depth(), 1);
        assert_eq!(queue.entries()[0].text, "second");

        assert!(
            !queue.consume("steer-1"),
            "a withdrawn entry is already gone, and saying so is not an error"
        );
    }

    /// The end of a turn is what moves a steer nobody took into the lane that
    /// replays it.
    #[test]
    fn stranding_moves_every_unconsumed_steer_into_the_fallback_lane() {
        let mut queue = Queue::default();
        queue.push_steered("steer-1".to_owned(), "first".to_owned());
        assert!(!queue.has_fallback());

        queue.strand();

        assert!(queue.has_fallback());
        let taken = queue.take_next_fallback().expect("the entry is replayable");
        assert_eq!(taken.text, "first");
        assert!(!taken.is_steered());
        assert!(queue.is_empty());
    }

    /// A steered entry is still waiting on the engine, so the replay lane must
    /// not take it: that is what would send the same message twice.
    #[test]
    fn the_replay_lane_never_takes_an_entry_the_engine_still_owes() {
        let mut queue = Queue::default();
        queue.push_steered("steer-1".to_owned(), "steered".to_owned());
        queue.push_fallback("steer-2".to_owned(), "queued".to_owned());

        let taken = queue.take_next_fallback().expect("the fallback entry");
        assert_eq!(taken.text, "queued");
        assert_eq!(queue.depth(), 1);
        assert!(queue.take_next_fallback().is_none());
    }

    #[test]
    fn a_requeued_entry_goes_back_in_front_of_what_came_after_it() {
        let mut queue = Queue::default();
        queue.push_fallback("steer-1".to_owned(), "first".to_owned());
        queue.push_fallback("steer-2".to_owned(), "second".to_owned());

        let taken = queue.take_next_fallback().expect("the first entry");
        queue.requeue_front(taken);

        assert_eq!(
            queue
                .entries()
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn withdrawing_takes_the_newest_entry_back() {
        let mut queue = Queue::default();
        queue.push_steered("steer-1".to_owned(), "first".to_owned());
        queue.push_steered("steer-2".to_owned(), "second".to_owned());

        let taken = queue.withdraw_newest().expect("the newest entry");

        assert_eq!(taken.text, "second");
        assert_eq!(queue.depth(), 1);
        assert!(taken.is_steered());
    }

    /// **§7-5.** The composer is a consent surface, so a teammate's words may
    /// not be handed to it: Up passes a peer's row over and takes the newest
    /// one this person really wrote, and a strip holding only a peer's answers
    /// nothing at all.
    #[test]
    fn withdrawing_passes_over_a_peers_row_and_takes_the_persons_own() {
        let mut queue = Queue::default();
        queue.push_steered("steer-1".to_owned(), "mine".to_owned());
        queue.push_peer("peer-1".to_owned(), "@Cargo.toml /init $skill".to_owned());

        let taken = queue.withdraw_newest().expect("the person's own entry");

        assert_eq!(taken.text, "mine");
        assert_eq!(queue.depth(), 1, "and the peer's row is still waiting");
        assert_eq!(queue.entries()[0].text, "@Cargo.toml /init $skill");

        assert!(
            queue.withdraw_newest().is_none(),
            "a strip holding only a peer's message has nothing to hand back"
        );
        assert_eq!(queue.depth(), 1);
    }

    /// The other half of the same rule: the replay lane resolves mentions and
    /// runs command names, so the end of a turn must not put a peer's row into
    /// it.
    #[test]
    fn stranding_leaves_a_peers_row_out_of_the_replay_lane() {
        let mut queue = Queue::default();
        queue.push_peer("peer-1".to_owned(), "@Cargo.toml /init".to_owned());
        queue.push_steered("steer-1".to_owned(), "mine".to_owned());

        queue.strand();

        let taken = queue.take_next_fallback().expect("the person's own entry");
        assert_eq!(taken.text, "mine");
        assert!(
            !queue.has_fallback(),
            "and the peer's row is nothing the lane may replay"
        );
        assert_eq!(queue.depth(), 1);
    }

    /// One `Command::Steer` carries a whole pass of a teammate's messages, so
    /// its id stands for several rows and the event that names it retires all
    /// of them.
    #[test]
    fn consuming_a_batchs_id_retires_every_row_it_stood_for() {
        let mut queue = Queue::default();
        queue.push_peer("steer-1".to_owned(), "the parser is done".to_owned());
        queue.push_peer("steer-1".to_owned(), "and the lexer".to_owned());
        queue.push_steered("steer-2".to_owned(), "mine".to_owned());

        assert!(queue.consume("steer-1"));

        assert_eq!(queue.depth(), 1);
        assert_eq!(queue.entries()[0].text, "mine");
    }
}
