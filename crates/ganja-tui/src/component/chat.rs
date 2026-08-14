//! The transcript viewport.
//!
//! The transcript is built from engine events alone — the frontend never
//! invents an entry — so the same event stream replays into the same screen,
//! which is what P4's resumed sessions and P7's remote clients depend on.
//!
//! Each entry caches the lines it wrapped to at a given width, so a frame costs
//! one wrap per entry that actually changed plus a walk over the entries the
//! viewport crosses — never a reflow of the whole transcript. P6 fills the
//! stage this doc reserved: an assistant text part is parsed into styled,
//! width-independent markdown blocks by [`crate::markdown`] first, and only
//! then wrapped here. The two caches invalidate on different things — stage 1
//! on the part's source and the theme, stage 2 on the width and the theme —
//! which is what keeps a resize off the markdown parser and a streamed delta
//! off the blocks that already settled.
//!
//! Markdown reaches **assistant text only** (ruling R12): a user's own message,
//! a tool's output and a file chip stay plain, so nothing a person typed is
//! re-read as markup.
//!
//! # The grammar the pane draws (**D487**, `claude-transcript-grammar`)
//!
//! What the transcript looks like is Claude Code's own grammar, taken from a
//! screenshot rather than ported: a `\u{25cf}` bullet leads every block a reply
//! is made of and every tool call it makes, a `\u{23bf}` marker introduces what
//! a call answered and hangs its preview under itself, and a `>` caret marks
//! what a person said. Upstream opencode's pane renders none of that — it heads
//! each message with its author's name and brackets a call's state into the
//! heading word — and [`crate::transcript`], the `/copy` formatter, keeps
//! upstream's markdown shape on purpose: the screen and the clipboard are read
//! by different readers, and only the screen moved.
//!
//! Presentation is all that moves. Every fact the pane showed before — which
//! state a call is in, what it was called with, what it answered, why it failed
//! — is still on screen, told by a glyph and a color instead of by a bracketed
//! word.

use std::{collections::HashMap, time::Instant};

use ganja_protocol::{Message, MessageId, Part, PartBody, PartId, Role, ToolState};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::{component::rewind, markdown, mention, theme::Theme};

/// Lines one wheel notch moves the viewport.
pub const WHEEL_LINES: isize = 3;

/// What a resumed reply says when the process that was writing it died.
///
/// The engine leaves such a message exactly as it found it — `time.completed`
/// absent, whatever parts reached the disk still attached — rather than
/// inventing an ending for it. Saying nothing here would render the fragment
/// as a reply that simply stopped mid-sentence, which is the one reading that
/// is actually false.
const INTERRUPTED: &str = "[interrupted] the session ended before this reply finished";

/// What introduces each file a revert put back.
///
/// A prefix rather than an indent: [`wrap`] lays every line out from its words,
/// so leading whitespace is collapsed and an indent would be a claim the screen
/// never honors — the same reason the task row's detail line carries an arrow.
const REVERTED_FILE: &str = "\u{21b3} ";

/// What a checkpoint row calls a prompt: its first non-empty line.
///
/// The id stands in for a message with no text at all — an attachment-only
/// prompt — because a blank row is one a person cannot pick with any
/// confidence, and the id is at least the thing the engine knows it by.
fn title(entry: &Entry) -> String {
    entry
        .parts
        .iter()
        .find_map(Part::as_text)
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .map_or_else(|| entry.id.as_str().to_owned(), ToOwned::to_owned)
}

/// What leads a block of a reply: a text block the model wrote, or a call it
/// made (**D487**).
const BULLET: &str = "\u{25cf} ";

/// What leads what a call answered, one step under the header it answers.
const RESULT: &str = "  \u{23bf} ";

/// What leads a prompt, in place of the author's name the pane used to head
/// every message with.
const PROMPT: &str = "> ";

/// What leads thinking a person can read, and the line a running turn leaves
/// at the tail of the transcript.
///
/// One glyph for both because they are one thing seen twice: the tail says a
/// turn is thinking, and this is what it thought.
const THINKING: &str = "\u{273b} ";

/// See [`THINKING`].
const WORKING: &str = THINKING;

/// The words a working line runs under, one per turn in order.
///
/// **Ganja's own vocabulary.** The *shape* of the line is Claude Code's, from
/// the screenshot; the words are not ported — those are that program's voice,
/// and upstream opencode has no such line at all to take one from. They are
/// deliberately machine-plain: a loop churns and grinds, and none of these
/// claims more about what is happening inside than is true.
const WORKING_VERBS: [&str; 6] = [
    "Working", "Thinking", "Churning", "Grinding", "Whirring", "Chewing",
];

/// The line a revert leaves in place of the messages it hid.
///
/// Upstream draws a hoverable, left-bordered panel here and makes clicking it
/// the confirmed way back; ganja draws the row and points at `/redo`
/// (**D106**). What is lost with the panel is upstream's per-file
/// `+additions -deletions`, which it parses out of a unified diff the engine
/// sends and ganja's [`RevertInfo`](ganja_protocol::RevertInfo) does not carry.
fn reverted_headline(hidden: usize) -> String {
    format!(
        "{hidden} message{plural} reverted \u{2014} /redo to restore",
        plural = if hidden == 1 { "" } else { "s" },
    )
}

/// A scrollable transcript of plain-text entries.
#[derive(Debug, Default)]
pub struct Chat {
    entries: Vec<Entry>,
    /// The revert this transcript is showing, while it is showing one.
    ///
    /// Nothing is removed while it is set: the messages an undo hid are still
    /// in `entries`, because a redo brings them straight back and the engine
    /// keeps them too until the next prompt makes the choice permanent.
    revert: Option<Revert>,
    /// Where the viewport starts, or [`None`] to stay pinned to the tail.
    offset: Option<usize>,
    /// Height of the last viewport rendered; paging and clamping need it.
    height: usize,
    /// The user message the Esc Esc backtrack walk is standing on (**D467**),
    /// painted with the selection style while it is set.
    ///
    /// A rendering concern only: which messages the walk may land on, and what
    /// confirming one does, are [`crate::app::App`]'s — the same split the
    /// rewind picker draws.
    backtrack: Option<MessageId>,
    /// Whether the highlight moved since the last frame, so exactly the next
    /// render scrolls it into view — and a wheel scroll after that is left
    /// alone rather than snapped back.
    backtrack_unseen: bool,
    /// The turn that is running, while one is (**D487**).
    working: Option<Working>,
    /// The line [`Chat::working`] drew to on the last frame.
    ///
    /// Rebuilt every render rather than cached on width and theme like every
    /// other block here, because its text moves with the clock and there is
    /// nothing for such a cache to key on. It is kept rather than returned
    /// because [`Chat::visible`] hands out borrowed lines, and it costs one
    /// short format per frame — on exactly the frames the status bar's spinner
    /// is already redrawing for.
    working_line: Option<Line<'static>>,
}

/// What is hidden, and the row that says so.
#[derive(Debug)]
struct Revert {
    /// The message the revert stopped at. It, and every entry after it, is
    /// hidden — the engine hides at and after the anchor, so the hidden set is
    /// always a tail and the marker always sits at the end of the viewport.
    anchor: MessageId,
    /// Files the revert put back, project-relative, in the order the undone
    /// turns touched them.
    files: Vec<String>,
    wrapped: Option<Wrapped>,
}

#[derive(Debug)]
struct Entry {
    id: MessageId,
    role: Role,
    parts: Vec<Part>,
    /// The reply this entry holds was cut off by a crash; see [`INTERRUPTED`].
    /// Only a resume can know this — a live message is equally unfinished
    /// while it streams, and there the absence means "still arriving".
    interrupted: bool,
    /// Why the turn behind this reply died, when it did: the provider's own
    /// words, painted under the reply where the person is looking rather than
    /// squeezed into the status bar's one line.
    error: Option<String>,
    /// Stage 1 of the cache: one parsed markdown document per assistant text
    /// part. Deliberately *not* inside [`Wrapped`] — a resize and a streamed
    /// delta both clear that, and neither is a reason to parse again.
    markdown: HashMap<PartId, markdown::Document>,
    wrapped: Option<Wrapped>,
}

/// What a running turn leaves at the tail of the transcript (**D487**).
///
/// Carries the turn's own facts and no clock of its own: the elapsed figure is
/// read off `started` at every frame, exactly as `component::status`'s spinner
/// reads its phase off the moment its activity began, so nothing here has to
/// be advanced by the render loop.
#[derive(Clone, Copy, Debug)]
pub struct Working {
    /// When the turn began.
    pub started: Instant,
    /// Which turn of this session it is, so the verb rotates rather than being
    /// drawn at random — a transcript replays into the same screen, and a die
    /// roll would break that.
    pub turn: u64,
    /// Output tokens the **session** has spent, `0` for one that has spent
    /// none yet.
    ///
    /// Not what this turn has spent, which nothing on this side knows: a
    /// provider reports usage once, when the turn ends
    /// (`Event::TurnFinished`), and there is no per-step channel to read a
    /// live figure off. So the honest reading of the segment is the status
    /// bar's own `N out` — the same number, in a second place — and a session
    /// that has spent nothing draws no segment at all rather than claiming a
    /// zero the screen would contradict.
    pub output_tokens: u64,
}

impl Working {
    /// The one line this draws to.
    fn line(&self, theme: &Theme) -> Line<'static> {
        let verbs = u64::try_from(WORKING_VERBS.len()).unwrap_or(1);
        let verb = WORKING_VERBS[usize::try_from(self.turn % verbs).unwrap_or(0)];
        let mut text = format!(
            "{WORKING}{verb}\u{2026} ({elapsed}s",
            elapsed = self.started.elapsed().as_secs()
        );
        if self.output_tokens > 0 {
            text.push_str(&format!(
                " \u{b7} \u{2191} {tokens} tokens",
                tokens = self.output_tokens
            ));
        }
        text.push(')');

        Line::styled(text, theme.accent)
    }
}

/// One line of a block before the viewport lays it out: what introduces it,
/// what it says, and how it is painted.
///
/// The prefix leads the row's first visual line and hangs as blank columns
/// under every line the wrap adds, so a preview's second line still sits under
/// its own marker instead of sliding back to the margin. The columns are
/// **measured**, never counted: `\u{25cf}` and `\u{23bf}` are both East Asian
/// Ambiguous, and an indent hard-coded to what they are worth here would skew
/// every continuation the day a terminal draws one wide.
#[derive(Debug)]
struct Row {
    prefix: String,
    text: String,
    style: Style,
}

impl Row {
    fn new(prefix: &str, text: impl Into<String>, style: Style) -> Self {
        Self {
            prefix: prefix.to_owned(),
            text: text.into(),
            style,
        }
    }
}

/// Lays `rows` out at `width` columns, each behind its own prefix.
fn lay_out(rows: &[Row], width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for row in rows {
        let indent = row.prefix.width();
        let hang = " ".repeat(indent);
        // One column of body even where the prefix alone would fill the
        // viewport: a row that wrapped to nothing would take its text off the
        // screen entirely, where an overflowing one is merely clipped by the
        // buffer.
        let body = width.saturating_sub(indent).max(1);

        for (index, line) in wrap(&row.text, body).into_iter().enumerate() {
            // A blank line inside a block stays blank — a row of spaces is an
            // indent nobody can see, and one the backtrack highlight would
            // have to treat as content.
            let text = match (index, line.is_empty()) {
                (0, _) => format!("{prefix}{line}", prefix = row.prefix),
                (_, true) => String::new(),
                (_, false) => format!("{hang}{line}"),
            };
            lines.push(Line::styled(text, row.style));
        }
    }

    lines
}

#[derive(Debug)]
struct Wrapped {
    width: u16,
    /// The theme these lines carry the styles of. Cached lines hold their
    /// styles, so a theme switch has to invalidate the cache exactly as a
    /// resize does — otherwise the transcript keeps the old palette while
    /// everything drawn fresh takes the new one.
    revision: u64,
    lines: Vec<Line<'static>>,
}

impl Chat {
    /// Appends `message` and returns to following the tail.
    pub fn start_message(&mut self, message: Message) {
        self.push(message, false);
    }

    /// Appends a message read back from a resumed session's store.
    ///
    /// The same append a live `MessageStarted` performs, plus the one thing a
    /// stored message can say that a live one cannot: an assistant message the
    /// store never saw finish was cut off by a crash. Both routes end in
    /// [`Chat::push`], so a resumed transcript and a streamed one are the same
    /// entries built the same way — which is what lets the two replay
    /// identically.
    pub fn restore_message(&mut self, message: Message) {
        let interrupted = message.role == Role::Assistant && message.time.completed.is_none();

        self.push(message, interrupted);
    }

    /// The one place an entry enters the transcript.
    fn push(&mut self, message: Message, interrupted: bool) {
        self.entries.push(Entry {
            id: message.id,
            role: message.role,
            parts: message.parts,
            interrupted,
            error: None,
            markdown: HashMap::new(),
            wrapped: None,
        });
        self.follow_tail();
    }

    /// Paints `error` under the entry it ended, in the transcript the person
    /// is actually looking at when a turn dies. Answers whether the entry
    /// exists — a failure so early that no reply ever started has nowhere
    /// here to land, and the caller still owns a status-bar fallback for it.
    pub fn set_error(&mut self, message_id: &MessageId, error: String) -> bool {
        let Some(entry) = self.entry_mut(message_id) else {
            return false;
        };

        entry.error = Some(error);
        entry.wrapped = None;
        self.follow_tail();

        true
    }

    /// Every entry on screen, oldest first, as its role and its parts.
    ///
    /// What the copy commands read. Deliberately the *rendered* transcript
    /// rather than the engine's history: what a person means by "copy this
    /// conversation" is the one they have been looking at, and the two agree
    /// because every entry here arrived as an engine event. Reverted entries
    /// are left out for the same reason — they are not on the screen either,
    /// and they are not in what the next request will carry.
    pub fn messages(&self) -> Vec<crate::transcript::Entry<'_>> {
        self.shown()
            .iter()
            .map(|entry| (entry.role, entry.parts.as_slice()))
            .collect()
    }

    /// How many `task` calls on screen are still running — the delegated
    /// children the status bar counts (**D462**).
    ///
    /// Read off the transcript rather than kept as a tally beside it: the parts
    /// already say it, and a number maintained in parallel would have to be
    /// corrected on every path that rewrites the chat — a resume, a revert, a
    /// redo — instead of simply following it.
    #[must_use]
    pub fn running_tasks(&self) -> usize {
        self.shown()
            .iter()
            .flat_map(|entry| entry.parts.iter())
            .filter(|part| {
                matches!(
                    &part.body,
                    PartBody::Tool { tool, state: ToolState::Running { .. }, .. }
                        if tool == ganja_tool::task::ID
                )
            })
            .count()
    }

    /// Every checkpoint the rewind picker offers, **newest first**: one per
    /// user message on screen, carrying its first line and how many distinct
    /// files the turns between it and the next checkpoint changed.
    ///
    /// Read off the rendered transcript for [`Chat::messages`]'s reason — what
    /// a person means by "take me back to there" is a message they can see —
    /// which also means a session already showing a revert offers only what is
    /// still visible.
    ///
    /// The file count comes from the `Patch` parts the engine already sends
    /// (`PartBody::Patch`), so no new state and no engine round trip is needed
    /// to annotate a row. Line-level `+adds -dels` would need a diff between
    /// two tree hashes and is deliberately not built here; see
    /// [`crate::component::rewind`].
    pub fn checkpoints(&self) -> Vec<rewind::Checkpoint> {
        let shown = self.shown();
        let mut checkpoints: Vec<rewind::Checkpoint> = shown
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.role == Role::User)
            .map(|(index, entry)| {
                let mut files: Vec<&str> = Vec::new();
                for later in shown
                    .iter()
                    .skip(index + 1)
                    .take_while(|later| later.role != Role::User)
                {
                    for part in &later.parts {
                        if let PartBody::Patch { files: changed, .. } = &part.body {
                            for file in changed {
                                if !files.contains(&file.as_str()) {
                                    files.push(file);
                                }
                            }
                        }
                    }
                }

                rewind::Checkpoint {
                    message_id: entry.id.clone(),
                    title: title(entry),
                    files: files.len(),
                }
            })
            .collect();
        checkpoints.reverse();

        checkpoints
    }

    /// Moves the backtrack highlight to `anchor`, or clears it (**D467**).
    ///
    /// The next render scrolls the highlighted message into view; a cleared
    /// highlight leaves the viewport wherever it stands, because exiting the
    /// walk is not a scroll.
    pub fn set_backtrack(&mut self, anchor: Option<MessageId>) {
        self.backtrack_unseen = anchor.is_some();
        self.backtrack = anchor;
    }

    /// The message the backtrack highlight is on, while the walk is up.
    #[cfg(test)]
    pub(crate) fn backtrack_anchor(&self) -> Option<&MessageId> {
        self.backtrack.as_ref()
    }

    /// Hides everything from `anchor` on, and says so in one row naming
    /// `files`.
    ///
    /// Nothing is dropped: an undo is reversible until the next prompt, so the
    /// entries stay exactly where they are and only stop being drawn.
    pub fn revert(&mut self, anchor: MessageId, files: Vec<String>) {
        self.revert = Some(Revert {
            anchor,
            files,
            wrapped: None,
        });
        self.follow_tail();
    }

    /// Shows the hidden entries again, which is what a redo past the newest
    /// undone prompt does.
    pub fn unrevert(&mut self) {
        self.revert = None;
        self.follow_tail();
    }

    /// Deletes the hidden entries, which is what a prompt or a shell command
    /// after an undo does: the engine has just removed them from its history
    /// and from storage, so there is nothing left for a redo to bring back.
    pub fn drop_reverted(&mut self) {
        if let Some(revert) = self.revert.take() {
            self.entries.retain(|entry| entry.id < revert.anchor);
        }
        self.follow_tail();
    }

    /// Whether some tail of the transcript is currently hidden.
    #[cfg(test)]
    #[must_use]
    pub fn is_reverted(&self) -> bool {
        self.revert.is_some()
    }

    /// Says that a turn is running, or that none is (**D487**).
    ///
    /// Deliberately not a `follow_tail`: a line appearing at the bottom is not
    /// a reason to take a reader who scrolled up back down, and the offset
    /// clamp already keeps a pinned viewport where it was put.
    pub fn set_working(&mut self, working: Option<Working>) {
        self.working = working;
        if working.is_none() {
            self.working_line = None;
        }
    }

    /// Empties the transcript, which is what switching sessions does to it.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.revert = None;
        self.backtrack = None;
        self.set_working(None);
        self.follow_tail();
    }

    /// The entries the viewport draws: everything before the revert anchor,
    /// and everything when there is no revert.
    fn shown(&self) -> &[Entry] {
        &self.entries[..self.first_hidden()]
    }

    /// Index of the first entry a revert hid, which is `entries.len()` when
    /// nothing is hidden.
    fn first_hidden(&self) -> usize {
        self.revert.as_ref().map_or(self.entries.len(), |revert| {
            self.entries
                .iter()
                .position(|entry| entry.id >= revert.anchor)
                .unwrap_or(self.entries.len())
        })
    }

    /// Appends a part to the message that is streaming.
    ///
    /// Does nothing for a message the transcript never saw start: an event
    /// stream joined halfway is missing the entry, not broken.
    pub fn start_part(&mut self, message_id: &MessageId, part: Part) {
        if let Some(entry) = self.entry_mut(message_id) {
            entry.parts.push(part);
            entry.wrapped = None;
        }
    }

    /// Extends a part, which is how a streamed reply — or the thinking on its
    /// way to one — grows.
    ///
    /// Through [`Part::streamed_mut`] rather than `as_text_mut`, because the
    /// event says an id and a fragment and never which of the two this is.
    pub fn append_delta(&mut self, message_id: &MessageId, part_id: &PartId, delta: &str) {
        let Some(entry) = self.entry_mut(message_id) else {
            return;
        };

        if let Some(text) = entry
            .parts
            .iter_mut()
            .find(|part| part.id == *part_id)
            .and_then(Part::streamed_mut)
        {
            text.push_str(delta);
            entry.wrapped = None;
        }
    }

    /// Replaces a part with the same id; appends it instead so a frontend
    /// that missed `PartStarted` still converges on the same transcript.
    pub fn update_part(&mut self, message_id: &MessageId, part: Part) {
        let Some(entry) = self.entry_mut(message_id) else {
            return;
        };

        match entry
            .parts
            .iter_mut()
            .find(|existing| existing.id == part.id)
        {
            Some(existing) => *existing = part,
            None => entry.parts.push(part),
        }
        entry.wrapped = None;
    }

    /// Finds an entry by id, newest first: deltas address the message that is
    /// still streaming, which is the one at the end.
    fn entry_mut(&mut self, message_id: &MessageId) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .rev()
            .find(|entry| entry.id == *message_id)
    }

    /// Moves the viewport by `delta` lines, negative being towards the top.
    pub fn scroll_lines(&mut self, delta: isize) {
        let current = self.offset.unwrap_or_else(|| self.max_offset());
        let target = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs())
        };

        self.set_offset(target);
    }

    /// Moves the viewport by `delta` screenfuls, keeping one line of overlap so
    /// the reader has an anchor.
    pub fn scroll_pages(&mut self, delta: isize) {
        let page = self.height.saturating_sub(1).max(1);
        let lines = isize::try_from(page).unwrap_or(isize::MAX);

        self.scroll_lines(delta.saturating_mul(lines));
    }

    /// Pins the viewport to the newest line.
    pub fn follow_tail(&mut self) {
        self.offset = None;
    }

    /// Moves the viewport to the oldest line.
    ///
    /// The other half of [`Chat::follow_tail`]: Home and End mean the two ends
    /// of the conversation the way they mean the two ends of a line.
    pub fn scroll_to_top(&mut self) {
        self.set_offset(0);
    }

    /// Whether new text will scroll into view on its own.
    #[must_use]
    pub fn is_following_tail(&self) -> bool {
        self.offset.is_none()
    }

    /// Draws the visible slice of the transcript into `area`.
    pub fn render(&mut self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        self.height = usize::from(area.height);
        let first_hidden = self.first_hidden();
        let hidden = self.entries.len() - first_hidden;
        for entry in &mut self.entries[..first_hidden] {
            entry.wrap(area.width, theme);
        }
        if let Some(revert) = &mut self.revert {
            revert.wrap(hidden, area.width, theme);
        }
        // Before the offset math below, so the line it adds is one the
        // viewport already knows about — the marker row's own arrangement.
        self.working_line = self.working.map(|working| working.line(theme));

        // The highlight span exists only after the wrap above, which is why
        // the walk into view happens here rather than in the setter: the
        // setter runs before anybody knows how many lines anything takes.
        let highlight = self.backtrack_span();
        if let Some((start, length)) = highlight
            && std::mem::take(&mut self.backtrack_unseen)
        {
            let current = self.offset.unwrap_or_else(|| self.max_offset());
            let whole = start + length <= current + self.height;
            let taller = length >= self.height;
            if start < current || !(whole || taller) {
                self.set_offset(start);
            }
        }

        let offset = self
            .offset
            .map_or_else(|| self.max_offset(), |offset| offset.min(self.max_offset()));
        self.offset = self.offset.map(|_| offset);

        for (row, line) in self.visible(offset).enumerate() {
            let Ok(row) = u16::try_from(row) else {
                break;
            };
            buffer.set_line(area.x, area.y + row, line, area.width);
        }

        // Painted over the finished rows rather than baked into the wrap
        // cache, so stepping the highlight costs a restyle and never a
        // rewrap.
        if let Some((start, length)) = highlight {
            let first = start.max(offset);
            let last = (start + length).min(offset + self.height);
            for line in first..last {
                let Ok(row) = u16::try_from(line - offset) else {
                    break;
                };
                buffer.set_style(
                    Rect::new(area.x, area.y + row, area.width, 1),
                    theme.selection,
                );
            }
        }
    }

    /// The highlighted entry's first line and how many of its lines to paint,
    /// in transcript-line coordinates — or [`None`] when nothing is
    /// highlighted, the anchor is hidden by a revert, or it left the
    /// transcript entirely.
    ///
    /// The trailing breathing-room blank every entry wraps to stays
    /// unpainted: a full-width colored bar under the message would read as
    /// part of the next one.
    fn backtrack_span(&self) -> Option<(usize, usize)> {
        let anchor = self.backtrack.as_ref()?;
        let mut start = 0;
        for entry in self.shown() {
            let lines = entry.lines();
            if entry.id == *anchor {
                let mut length = lines.len();
                while length > 0 && lines[length - 1].width() == 0 {
                    length -= 1;
                }
                return Some((start, length));
            }
            start += lines.len();
        }

        None
    }

    /// Lines the whole transcript wrapped to at the last rendered width.
    pub(crate) fn line_count(&self) -> usize {
        let entries: usize = self.shown().iter().map(|entry| entry.lines().len()).sum();

        entries
            + self
                .revert
                .as_ref()
                .map_or(0, |revert| revert.lines().len())
            + usize::from(self.working_line.is_some())
    }

    /// Widths the entries are currently cached at, which is how a test tells
    /// that a resize actually invalidated the cache.
    #[cfg(test)]
    pub(crate) fn cached_widths(&self) -> Vec<Option<u16>> {
        self.entries
            .iter()
            .map(|entry| entry.wrapped.as_ref().map(|wrapped| wrapped.width))
            .collect()
    }

    fn max_offset(&self) -> usize {
        self.line_count().saturating_sub(self.height)
    }

    /// Follows the tail again once the viewport reaches the bottom, so that
    /// scrolling down past the end behaves like never having scrolled.
    fn set_offset(&mut self, offset: usize) {
        let max = self.max_offset();
        self.offset = if offset >= max { None } else { Some(offset) };
    }

    /// Yields the lines the viewport shows, skipping whole entries rather than
    /// stepping over every line above the offset.
    ///
    /// The marker row rides along as one more block at the end, which is where
    /// it belongs: the entries a revert hides are always the tail of the
    /// transcript, so what it stands in for is always below everything shown.
    /// The working line rides the same seam, one block further down still —
    /// what a turn is doing now is below everything it has already said
    /// (**D487**).
    fn visible(&self, offset: usize) -> impl Iterator<Item = &Line<'static>> {
        let mut left_to_skip = offset;
        let marker: &[Line<'static>] = self.revert.as_ref().map_or(&[], Revert::lines);
        let working: &[Line<'static>] = self.working_line.as_slice();

        self.shown()
            .iter()
            .map(Entry::lines)
            .chain(std::iter::once(marker))
            .chain(std::iter::once(working))
            .flat_map(move |lines| {
                let skip = left_to_skip.min(lines.len());
                left_to_skip -= skip;
                &lines[skip..]
            })
            .take(self.height)
    }
}

impl Revert {
    fn lines(&self) -> &[Line<'static>] {
        self.wrapped
            .as_ref()
            .map_or(&[], |wrapped| wrapped.lines.as_slice())
    }

    /// Lays the marker out for `hidden` hidden entries.
    ///
    /// Keyed on width and theme like every other cached wrap, and on nothing
    /// else: the count and the file list are fixed for the life of a revert —
    /// a deeper undo replaces the whole [`Revert`] rather than editing this
    /// one.
    fn wrap(&mut self, hidden: usize, width: u16, theme: &Theme) {
        if self
            .wrapped
            .as_ref()
            .is_some_and(|wrapped| wrapped.width == width && wrapped.revision == theme.revision())
        {
            return;
        }

        let mut lines: Vec<Line<'static>> = wrap(&reverted_headline(hidden), usize::from(width))
            .into_iter()
            .map(|line| Line::styled(line, theme.warning))
            .collect();
        for file in &self.files {
            lines.extend(
                wrap(&format!("{REVERTED_FILE}{file}"), usize::from(width))
                    .into_iter()
                    .map(|line| Line::styled(line, theme.dim)),
            );
        }
        // Breathing room, exactly as an entry leaves.
        lines.push(Line::styled(String::new(), Style::default()));

        self.wrapped = Some(Wrapped {
            width,
            revision: theme.revision(),
            lines,
        });
    }
}

impl Entry {
    fn lines(&self) -> &[Line<'static>] {
        self.wrapped
            .as_ref()
            .map_or(&[], |wrapped| wrapped.lines.as_slice())
    }

    fn wrap(&mut self, width: u16, theme: &Theme) {
        if self
            .wrapped
            .as_ref()
            .is_some_and(|wrapped| wrapped.width == width && wrapped.revision == theme.revision())
        {
            return;
        }

        let columns = usize::from(width);
        let mut lines: Vec<Line<'static>> = Vec::new();
        // Parts lay themselves out so that a tool block can carry its own
        // prefixes and styles instead of the running text's.
        //
        // A prompt is **one** block however many parts it was built from: the
        // caret leads its first line and everything after hangs under it, so a
        // prompt that is nothing but an attachment is still marked as
        // something a person said. A reply is **many**: each text block and
        // each call carries a bullet of its own, which is the whole of what
        // the grammar claims about who did what (**D487**).
        for part in &self.parts {
            match &part.body {
                // A reply is markdown; a prompt is what the user typed. The
                // split is the whole of R12's scope, and it is made here so
                // that neither renderer has to ask who wrote the part.
                PartBody::Text { text } if self.role == Role::Assistant => {
                    let document = self.markdown.entry(part.id.clone()).or_default();
                    document.update(text, theme);

                    let indent = BULLET.width();
                    let hang = " ".repeat(indent);
                    let body = columns.saturating_sub(indent).max(1);
                    let mut led = false;
                    for line in document.lines().flat_map(|line| markdown::wrap(line, body)) {
                        // The blank between two blocks stays blank, for
                        // [`lay_out`]'s reason.
                        if led && line.width() == 0 {
                            lines.push(line);
                            continue;
                        }
                        let lead = if std::mem::replace(&mut led, true) {
                            Span::raw(hang.clone())
                        } else {
                            Span::styled(BULLET.to_owned(), theme.fg)
                        };
                        let mut spans = Vec::with_capacity(line.spans.len() + 1);
                        spans.push(lead);
                        spans.extend(line.spans);
                        lines.push(Line::from(spans));
                    }
                }
                PartBody::Text { text } => {
                    let row = Row::new(&prompt_lead(lines.is_empty()), text.clone(), theme.accent);
                    lines.extend(lay_out(&[row], columns));
                }
                PartBody::Tool { tool, state, .. } => {
                    lines.extend(lay_out(&tool_lines(tool, state, theme), columns));
                }
                // A file the user attached, rendered as the token they typed
                // — `@path`, with its `#line-range` when one was named —
                // rather than as its contents: the engine reads the file when
                // it builds a request, and pasting it into the transcript
                // would show the user their own file back. A mime outside
                // `text/plain` is named beside it, which is all a transcript
                // can honestly say about bytes it never reads.
                PartBody::File {
                    path,
                    mime,
                    start,
                    end,
                    ..
                } => {
                    let token = mention::token(path, *start, *end);
                    let label = if mime == "text/plain" {
                        token
                    } else {
                        format!("{token} ({mime})")
                    };
                    let prefix = match self.role {
                        Role::User => prompt_lead(lines.is_empty()),
                        Role::Assistant => BULLET.to_owned(),
                    };
                    lines.extend(lay_out(&[Row::new(&prefix, label, theme.dim)], columns));
                }
                // Thinking a person can read, behind its own marker and dimmed
                // into italics so it never competes with the answer it is on
                // the way to. Clamped from the **tail** while it arrives, for
                // the reason a streaming command's output is: a long think
                // would otherwise push the reply off the screen, and the
                // newest lines are the ones worth the rows. The whole of it is
                // one Ctrl+T away, which is what the hint says.
                PartBody::ReasoningText { text } if !text.is_empty() => {
                    let (tail, hidden) = clamp_tail(text);
                    let style = theme.dim.add_modifier(Modifier::ITALIC);
                    let hang = " ".repeat(THINKING.width());
                    let mut rows: Vec<Row> = Vec::new();
                    if hidden > 0 {
                        rows.push(Row::new(THINKING, clamp_hint(hidden), style));
                    }
                    rows.extend(tail.into_iter().enumerate().map(|(index, line)| {
                        let lead = if index == 0 && hidden == 0 {
                            THINKING
                        } else {
                            hang.as_str()
                        };
                        Row::new(lead, line, style)
                    }));
                    lines.extend(lay_out(&rows, columns));
                }
                // An empty one is a part the provider opened and has not
                // filled yet; a marker alone would be a claim about nothing.
                PartBody::ReasoningText { .. } => {}
                // Sealed reasoning has no rendering: what it holds is opaque
                // to everything but the provider, so a line about it would be
                // a line about a blob.
                PartBody::StepStart
                | PartBody::StepFinish { .. }
                | PartBody::Patch { .. }
                | PartBody::Reasoning { .. } => {}
            }
        }
        // Both of these answer the same question a failed call's own `⎿` row
        // does — why what is above stops where it does — so they are told in
        // the same shape.
        if let Some(error) = &self.error {
            let row = Row::new(RESULT, format!("[error] {error}"), theme.error);
            lines.extend(lay_out(&[row], columns));
        }
        if self.interrupted {
            let row = Row::new(RESULT, INTERRUPTED, theme.error);
            lines.extend(lay_out(&[row], columns));
        }
        // Breathing room before the next entry.
        lines.push(Line::styled(String::new(), Style::default()));

        self.wrapped = Some(Wrapped {
            width,
            revision: theme.revision(),
            lines,
        });
    }
}

/// What leads a prompt's row: the caret on the entry's first line, the columns
/// it occupies under every row after it.
fn prompt_lead(first: bool) -> String {
    if first {
        PROMPT.to_owned()
    } else {
        " ".repeat(PROMPT.width())
    }
}

/// Tool argument keys named first in a call's header. Tool-agnostic on
/// purpose: an unfamiliar tool still shows something recognizable instead of
/// just its bare name, and the field that says what a call is *doing* belongs
/// at the front of a summary that may be cut.
const TITLE_KEYS: [&str; 5] = ["command", "filePath", "path", "pattern", "url"];

/// Arguments a header names before the rest become an ellipsis. A header is
/// one line: the whole payload is what the permission dialog draws and what
/// the Ctrl+T inspector replays.
const HEADER_ARGS: usize = 3;

/// Columns one argument's value may fill before it is clipped.
const HEADER_VALUE: usize = 40;

/// Lines a tool call's output or diff may show before the rest is clamped.
/// The full text is what the model saw; the transcript only needs the gist.
const TOOL_PREVIEW_LINES: usize = 4;

/// The tool whose call is a whole second agent loop, and which is drawn as one
/// inline row rather than as a block of output.
const TASK_TOOL: &str = "task";

/// A call's arguments, condensed to the one line its header has room for:
/// `key: "value"` pairs, the recognizable fields first, capped at
/// [`HEADER_ARGS`] of them.
///
/// [`None`] when there is nothing to say, which is what a call whose arguments
/// have not arrived yet has. A nested payload is named by its shape rather than
/// drawn — an array of todos is still an argument the header must admit to, and
/// still not one a single line can carry.
fn derive_args(input: &serde_json::Value) -> Option<String> {
    let object = input.as_object()?;
    let named = TITLE_KEYS
        .iter()
        .copied()
        .filter(|key| object.contains_key(*key));
    let rest = object
        .keys()
        .map(String::as_str)
        .filter(|key| !TITLE_KEYS.contains(key));

    let mut shown: Vec<String> = Vec::new();
    let mut cut = false;
    for key in named.chain(rest) {
        if shown.len() == HEADER_ARGS {
            cut = true;
            break;
        }
        let value = object.get(key).map_or_else(String::new, arg_value);
        shown.push(format!("{key}: {value}"));
    }

    if shown.is_empty() {
        return None;
    }
    if cut {
        shown.push("\u{2026}".to_owned());
    }

    Some(shown.join(", "))
}

/// One argument's value, as short as it can honestly be said.
///
/// A string is quoted and cut to its first line and [`HEADER_VALUE`] columns —
/// a `write` call carries a whole file in one of these — a number or a boolean
/// is drawn as it is, and a nested payload as the shape it has.
fn arg_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => quoted(text),
        serde_json::Value::Array(_) => "[\u{2026}]".to_owned(),
        serde_json::Value::Object(_) => "{\u{2026}}".to_owned(),
        other => other.to_string(),
    }
}

/// `text` as a header draws it: quoted, first line only, and cut to
/// [`HEADER_VALUE`] columns with the cut admitted.
fn quoted(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default();
    let mut shown = clip(first, HEADER_VALUE);
    if shown.len() < text.len() {
        shown.push('\u{2026}');
    }

    format!("\"{shown}\"")
}

/// The one line a call is announced on: the tool, and what it was called with.
///
/// The name is title-cased the way the screenshot draws it and the way the
/// task row already draws an agent's; the id itself is unchanged everywhere it
/// is a name rather than a heading.
fn tool_heading(tool: &str, input: Option<&serde_json::Value>) -> String {
    let name = titlecase(tool);
    match input.and_then(derive_args) {
        Some(args) => format!("{name}({args})"),
        None => name,
    }
}

/// The rows a result lays out as: the first behind the `⎿` when nothing has
/// claimed that marker yet, the rest under the columns it occupies.
fn result_rows(lines: Vec<(String, Style)>, claimed: bool) -> Vec<Row> {
    let under = " ".repeat(RESULT.width());

    lines
        .into_iter()
        .enumerate()
        .map(|(index, (text, style))| {
            let prefix = if index == 0 && !claimed {
                RESULT
            } else {
                under.as_str()
            };
            Row::new(prefix, text, style)
        })
        .collect()
}

/// What a clamped preview says about the lines it left out.
///
/// Claude Code's own hint names its `ctrl+o` expander; the whole of a call's
/// output lives in ganja's Ctrl+T inspector, whose transcript tab replays
/// exactly what `/copy` writes, so the hint names that one instead (**D487**).
fn clamp_hint(hidden: usize) -> String {
    format!(
        "\u{2026} +{hidden} line{plural} (ctrl+t to expand)",
        plural = if hidden == 1 { "" } else { "s" },
    )
}

/// The first `TOOL_PREVIEW_LINES` lines of `text`, and how many were cut.
fn clamp_preview(text: &str) -> (Vec<String>, usize) {
    let mut lines = text.lines();
    let preview: Vec<String> = lines
        .by_ref()
        .take(TOOL_PREVIEW_LINES)
        .map(str::to_owned)
        .collect();

    (preview, lines.count())
}

/// The **last** `TOOL_PREVIEW_LINES` lines of `text`, and how many were cut.
///
/// The other end from [`clamp_preview`], and for the other case: output that
/// is still arriving. A command's newest line is the one worth a row, where a
/// finished call's first line is the one that says what it did.
fn clamp_tail(text: &str) -> (Vec<String>, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let skipped = lines.len().saturating_sub(TOOL_PREVIEW_LINES);

    (
        lines[skipped..]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
        skipped,
    )
}

/// Styles one line of a unified diff by its leading marker. Hunk headers and
/// context lines recede like the rest of the chrome; only the change itself
/// stands out.
fn diff_line_style(line: &str, theme: &Theme) -> Style {
    if line.starts_with('+') && !line.starts_with("+++") {
        theme.add
    } else if line.starts_with('-') && !line.starts_with("---") {
        theme.remove
    } else {
        theme.dim
    }
}

/// One compact block for a tool call, in whatever state it currently stands.
///
/// Every state shares the header's **shape** — `\u{25cf} Tool(args)`, the same
/// line before and after the call settles — and differs only in the color it
/// is painted and in what hangs under it: a running call's newest output, a
/// finished call's summary and preview, a failed call's first line of why
/// (**D487**). `StepStart`/`StepFinish` never reach here.
fn tool_lines(tool: &str, state: &ToolState, theme: &Theme) -> Vec<Row> {
    // A delegated turn is one row, never a transcript of its own: everything
    // the child said reaches the model inside the tool result, and repeating
    // it here would show the same work twice.
    if tool == TASK_TOOL && !matches!(state, ToolState::Error { .. }) {
        return task_lines(state, theme);
    }

    match state {
        ToolState::Pending => vec![Row::new(BULLET, tool_heading(tool, None), theme.dim)],
        ToolState::Running {
            input, metadata, ..
        } => {
            let mut rows = vec![Row::new(BULLET, tool_heading(tool, Some(input)), theme.dim)];
            // A call that reports as it goes — the `!` passthrough streaming a
            // command's output — redraws its tail every time the part is
            // republished, so the newest lines are the ones on screen.
            if let Some(output) = metadata
                .get("output")
                .and_then(serde_json::Value::as_str)
                .filter(|output| !output.is_empty())
            {
                let (tail, hidden) = clamp_tail(output);
                let mut preview: Vec<(String, Style)> = Vec::new();
                // In front, because these are the lines that already scrolled
                // past: what was cut is above what is shown, not below it.
                if hidden > 0 {
                    preview.push((clamp_hint(hidden), theme.dim));
                }
                preview.extend(tail.into_iter().map(|line| (line, theme.dim)));
                rows.extend(result_rows(preview, false));
            }

            rows
        }
        ToolState::Completed {
            input,
            output,
            title,
            metadata,
            ..
        } => {
            let mut rows = vec![Row::new(BULLET, tool_heading(tool, Some(input)), theme.fg)];
            // What the tool itself called the work it did. A tool that named
            // nothing leaves the marker to the preview rather than drawing an
            // empty row above it.
            let summary = title.trim();
            let claimed = !summary.is_empty();
            if claimed {
                rows.push(Row::new(RESULT, summary.to_owned(), theme.fg));
            }

            let diff = metadata
                .get("diff")
                .and_then(serde_json::Value::as_str)
                .filter(|diff| !diff.is_empty());
            let (preview, hidden) = match diff {
                Some(diff) => {
                    let (lines, hidden) = clamp_preview(diff);
                    let styled = lines
                        .into_iter()
                        .map(|line| {
                            let style = diff_line_style(&line, theme);
                            (line, style)
                        })
                        .collect();
                    (styled, hidden)
                }
                None if !output.is_empty() => {
                    let (lines, hidden) = clamp_preview(output);
                    let styled = lines.into_iter().map(|line| (line, theme.dim)).collect();
                    (styled, hidden)
                }
                None => (Vec::new(), 0),
            };

            let mut body = preview;
            if hidden > 0 {
                body.push((clamp_hint(hidden), theme.dim));
            }
            rows.extend(result_rows(body, claimed));

            rows
        }
        ToolState::Error { input, error, .. } => {
            let mut rows = vec![Row::new(
                BULLET,
                tool_heading(tool, Some(input)),
                theme.error,
            )];
            if let Some(first) = error.lines().next().filter(|line| !line.is_empty()) {
                rows.push(Row::new(RESULT, format!("[error] {first}"), theme.error));
            }

            rows
        }
    }
}

/// The two lines a delegated turn gets, whatever it is doing.
///
/// Spec: upstream `routes/session/index.tsx:2213-2309` for the **facts** —
/// the agent doing the work, what it was asked for, the tool it is in or the
/// count and the clock it finished on. The **presentation** is D487's, not
/// upstream's: the header is a bullet and the progress is a `⎿` row, because
/// a delegated call is a call and a lone unbulleted block would break the one
/// claim the grammar makes. Upstream's `│`/`✓` markers retired with it — the
/// bullet's color says what they said. **The child's own answer is never on
/// the row** — it is inside the tool result the model reads, and a transcript
/// that printed it would be showing the same work twice, once as prose and
/// once as a result.
fn task_lines(state: &ToolState, theme: &Theme) -> Vec<Row> {
    match state {
        ToolState::Pending => vec![Row::new(BULLET, titlecase(TASK_TOOL), theme.dim)],
        ToolState::Running {
            input, metadata, ..
        } => {
            let agent = field(input, "subagent_type");
            let mut rows = vec![Row::new(
                BULLET,
                task_heading(agent, field(input, "description")),
                theme.dim,
            )];
            // Upstream's own priority: the tool the child is running right now
            // says more than how many it has run.
            let detail = match field(metadata, "current_tool") {
                Some(current) => current.to_owned(),
                None => format!("{} toolcalls", toolcalls(metadata)),
            };
            rows.push(Row::new(RESULT, detail, theme.dim));

            rows
        }
        ToolState::Completed {
            input,
            title,
            metadata,
            started,
            completed,
            ..
        } => {
            let agent = field(metadata, "agent").or_else(|| field(input, "subagent_type"));
            let description = field(input, "description").or(Some(title.as_str()));

            vec![
                Row::new(BULLET, task_heading(agent, description), theme.fg),
                Row::new(
                    RESULT,
                    format!(
                        "{calls} toolcalls \u{b7} {elapsed}",
                        calls = toolcalls(metadata),
                        elapsed = elapsed(*started, *completed),
                    ),
                    theme.dim,
                ),
            ]
        }
        // Never reached: a failed call keeps the shape every other failed call
        // has, so a refusal reads the same wherever it came from.
        ToolState::Error { .. } => Vec::new(),
    }
}

/// One string field of a JSON object, when it is there and is not empty.
fn field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|found| !found.is_empty())
}

/// How many tools the child has called, as its parent's part recorded it.
fn toolcalls(metadata: &serde_json::Value) -> u64 {
    metadata
        .get("toolcalls")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// The task row's header: the agent doing the work and what it was asked for,
/// in the shape every other call's header has.
///
/// Its own arguments rather than [`derive_args`]'s, because the raw input's
/// spelling is not the one a person reads: the agent arrives as
/// `subagent_type` on a running call's input and as `agent` on a finished
/// call's metadata, and a header that printed both spellings would be naming
/// the wire rather than the work.
fn task_heading(agent: Option<&str>, description: Option<&str>) -> String {
    let name = titlecase(TASK_TOOL);
    let args: Vec<String> = [("agent", agent), ("description", description)]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| format!("{key}: {}", quoted(value))))
        .collect();

    if args.is_empty() {
        return name;
    }

    format!("{name}({args})", args = args.join(", "))
}

/// `name` with its first character upper-cased, which is how upstream renders
/// an agent's name on this row.
fn titlecase(name: &str) -> String {
    let mut characters = name.chars();

    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + characters.as_str()
    })
}

/// How long a call took, from the two stamps its part carries.
///
/// Rounded to whatever unit reads as a duration rather than as a number: a
/// child that ran for two minutes should not be reported in milliseconds.
fn elapsed(started: u64, completed: u64) -> String {
    let millis = completed.saturating_sub(started);
    if millis < 1_000 {
        return format!("{millis}ms");
    }

    let seconds = millis / 1_000;
    if seconds < 60 {
        return format!("{seconds}.{tenths}s", tenths = millis % 1_000 / 100);
    }

    format!(
        "{minutes}m {rest}s",
        minutes = seconds / 60,
        rest = seconds % 60
    )
}

/// Greedily wraps `text` to `width` display columns, preserving blank lines and
/// chopping words too wide to ever fit.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0;

        for word in paragraph.split_whitespace() {
            let mut rest = word;

            while !rest.is_empty() {
                let separator = usize::from(line_width != 0);
                let room = width.saturating_sub(line_width + separator);

                if rest.width() <= room {
                    if separator == 1 {
                        line.push(' ');
                        line_width += 1;
                    }
                    line.push_str(rest);
                    line_width += rest.width();
                    break;
                }

                if line_width != 0 {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                    continue;
                }

                let (head, tail) = split_at_width(rest, width);
                lines.push(head.to_owned());
                rest = tail;
            }
        }

        lines.push(line);
    }

    lines
}

/// Splits `text` at the last boundary that fits in `width` columns, always
/// consuming at least one character so callers cannot loop forever.
pub(crate) fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0;

    for (index, character) in text.char_indices() {
        let advance = character.width().unwrap_or(0);
        if index > 0 && used + advance > width {
            return text.split_at(index);
        }
        used += advance;
    }

    (text, "")
}

/// `text` cut to `width` display columns.
pub(crate) fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }

    split_at_width(text, width).0.to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ganja_protocol::{Message, MessageId, Part, PartBody, PartId, ToolState};
    use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};

    use super::{Chat, Instant, Working, elapsed, split_at_width, wrap};
    use crate::theme::{Theme, Themes};

    /// A reply carrying one tool part in `state`, rendered wide enough that
    /// nothing wraps.
    fn tool_call(tool: &str, state: ToolState) -> Vec<String> {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: tool.to_owned(),
                state,
            },
        });
        chat.start_message(reply);

        rendered(&mut chat, Rect::new(0, 0, 80, 20))
    }

    const VIEWPORT: Rect = Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 6,
    };

    /// Fills a transcript the way the engine does: one complete user message
    /// per entry.
    fn transcript(chat: &mut Chat, entries: usize) {
        for index in 0..entries {
            chat.start_message(Message::user(format!("entry {index}")));
        }
    }

    fn rendered(chat: &mut Chat, area: Rect) -> Vec<String> {
        let mut buffer = Buffer::empty(area);
        chat.render(area, &mut buffer, &Theme::default());

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// A file part renders as the token the user typed — range and all — and
    /// names its mime when the bytes are not text.
    #[test]
    fn an_attached_file_renders_as_its_token_with_range_and_mime() {
        let mut chat = Chat::default();
        let mut message = Message::user("look");
        message.parts.push(Part {
            id: PartId::from("prt_f1".to_owned()),
            body: PartBody::File {
                path: "src/lib.rs".to_owned(),
                mime: "text/plain".to_owned(),
                start: Some(5),
                end: Some(9),
                content: None,
            },
        });
        message.parts.push(Part {
            id: PartId::from("prt_f2".to_owned()),
            body: PartBody::File {
                path: "shot.png".to_owned(),
                mime: "image/png".to_owned(),
                start: None,
                end: None,
                content: None,
            },
        });
        chat.start_message(message);

        let screen = rendered(&mut chat, Rect::new(0, 0, 40, 8)).join("\n");
        assert!(screen.contains("@src/lib.rs#5-9"), "{screen}");
        assert!(
            !screen.contains("@src/lib.rs#5-9 ("),
            "a text mention needs no mime label:\n{screen}"
        );
        assert!(screen.contains("@shot.png (image/png)"), "{screen}");
    }

    #[test]
    fn wrapping_breaks_on_word_boundaries() {
        assert_eq!(
            wrap("the quick brown fox", 10),
            vec!["the quick".to_owned(), "brown fox".to_owned()]
        );
    }

    #[test]
    fn wrapping_preserves_blank_lines_between_paragraphs() {
        assert_eq!(
            wrap("one\n\ntwo", 10),
            vec!["one".to_owned(), String::new(), "two".to_owned()]
        );
    }

    #[test]
    fn a_word_wider_than_the_viewport_is_chopped_not_dropped() {
        assert_eq!(
            wrap("abcdefghij", 4),
            vec!["abcd".to_owned(), "efgh".to_owned(), "ij".to_owned()]
        );
    }

    #[test]
    fn wrapping_measures_display_width_not_bytes() {
        // Each of these is two columns wide, so only two fit on a five-column
        // line.
        assert_eq!(
            wrap("ああ ああ", 5),
            vec!["ああ".to_owned(), "ああ".to_owned()]
        );
    }

    #[test]
    fn a_zero_width_viewport_wraps_to_nothing() {
        assert!(wrap("anything", 0).is_empty());
    }

    #[test]
    fn splitting_always_consumes_a_character() {
        // A double-width character cannot fit in one column, but returning an
        // empty head would spin the caller forever.
        assert_eq!(split_at_width("ああ", 1), ("あ", "あ"));
    }

    #[test]
    fn a_new_entry_scrolls_into_view() {
        let mut chat = Chat::default();
        transcript(&mut chat, 20);

        let lines = rendered(&mut chat, VIEWPORT);

        assert!(
            lines.iter().any(|line| line.contains("entry 19")),
            "the newest entry should be visible, got {lines:?}"
        );
        assert!(chat.is_following_tail());
    }

    #[test]
    fn scrolling_up_pins_the_viewport_and_scrolling_back_down_releases_it() {
        let mut chat = Chat::default();
        transcript(&mut chat, 20);
        rendered(&mut chat, VIEWPORT);

        chat.scroll_lines(-9);
        assert!(!chat.is_following_tail());
        let lines = rendered(&mut chat, VIEWPORT);
        assert!(
            !lines.iter().any(|line| line.contains("entry 19")),
            "a pinned viewport should not show the tail, got {lines:?}"
        );

        chat.scroll_lines(100);
        assert!(chat.is_following_tail());
    }

    #[test]
    fn paging_moves_about_a_screenful() {
        let mut chat = Chat::default();
        transcript(&mut chat, 40);
        rendered(&mut chat, VIEWPORT);

        chat.scroll_pages(-4);
        let after_paging_up = rendered(&mut chat, VIEWPORT);
        chat.scroll_pages(1);
        let after_paging_down = rendered(&mut chat, VIEWPORT);

        assert_ne!(after_paging_up, after_paging_down);
        chat.follow_tail();
        assert!(chat.is_following_tail());
    }

    #[test]
    fn a_streamed_entry_grows_in_place() {
        let mut chat = Chat::default();
        let reply = Message::assistant("canned");
        let part = Part::text("");
        chat.start_message(reply.clone());
        chat.start_part(&reply.id, part.clone());

        for fragment in ["hello ", "world"] {
            chat.append_delta(&reply.id, &part.id, fragment);
        }

        let lines = rendered(&mut chat, VIEWPORT);

        assert!(
            lines.iter().any(|line| line == "\u{25cf} hello world"),
            "streamed fragments should join into one entry, got {lines:?}"
        );
    }

    /// Every part of a message renders, which is what keeps P3's tool output
    /// from displacing the text around it.
    #[test]
    fn a_message_renders_all_of_its_parts() {
        let mut chat = Chat::default();
        let reply = Message::assistant("canned");
        chat.start_message(reply.clone());
        for text in ["first", "second"] {
            chat.start_part(&reply.id, Part::text(text));
        }

        let lines = rendered(&mut chat, VIEWPORT);

        assert!(
            lines.iter().any(|line| line == "\u{25cf} first")
                && lines.iter().any(|line| line == "\u{25cf} second"),
            "both parts should render, each behind a bullet of its own, got {lines:?}"
        );
    }

    #[test]
    fn events_for_a_message_the_transcript_never_saw_are_ignored() {
        let mut chat = Chat::default();
        let orphan = Message::assistant("canned");
        let part = Part::text("");

        chat.start_part(&orphan.id, part.clone());
        chat.append_delta(&orphan.id, &part.id, "orphan");

        assert!(rendered(&mut chat, VIEWPORT).iter().all(String::is_empty));
    }

    #[test]
    fn a_pending_tool_call_names_the_tool() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part::tool("call_1", "shell"));
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(
            lines.iter().any(|line| line == "\u{25cf} Shell"),
            "a call whose arguments have not arrived names the tool alone, got {lines:?}"
        );
    }

    #[test]
    fn a_running_tool_call_shows_a_title_derived_from_its_input() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Running {
                    input: serde_json::json!({"command": "cargo test"}),
                    metadata: serde_json::Value::Null,
                    started: 0,
                },
            },
        });
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(
            lines
                .iter()
                .any(|line| line == "\u{25cf} Shell(command: \"cargo test\")"),
            "got {lines:?}"
        );
    }

    /// **AC1.** The whole grammar of a settled call in one screen: the bullet
    /// and the condensed arguments on the header, the `⎿` marker carrying what
    /// the tool called the work, the preview hanging under that marker's own
    /// columns, and a hint naming what was cut and where the rest is.
    #[test]
    fn a_completed_tool_call_renders_as_a_bullet_a_result_marker_and_a_hanging_preview() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "a.rs"}),
                    output: "one\ntwo\nthree\nfour\nfive\nsix".to_owned(),
                    title: "a.rs".to_owned(),
                    metadata: serde_json::json!({}),
                    started: 0,
                    completed: 1,
                },
            },
        });
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec![
                "\u{25cf} Read(filePath: \"a.rs\")",
                "  \u{23bf} a.rs",
                "    one",
                "    two",
                "    three",
                "    four",
                "    \u{2026} +2 lines (ctrl+t to expand)",
            ],
            "got {lines:?}"
        );
    }

    /// **AC2.** A call that is running and the same call once it has settled
    /// are announced by the same line: what changed is the color it is painted
    /// in, not a word in the text.
    #[test]
    fn a_running_call_and_its_settled_self_share_their_header_line() {
        let input = serde_json::json!({"command": "cargo test"});
        let running = tool_call(
            "shell",
            ToolState::Running {
                input: input.clone(),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        );
        let completed = tool_call(
            "shell",
            ToolState::Completed {
                input: input.clone(),
                output: String::new(),
                title: "cargo test".to_owned(),
                metadata: serde_json::json!({}),
                started: 0,
                completed: 1,
            },
        );
        let failed = tool_call(
            "shell",
            ToolState::Error {
                input,
                error: "no such command".to_owned(),
                started: 0,
                completed: 1,
            },
        );

        let header = |lines: &[String]| {
            lines
                .iter()
                .find(|line| !line.is_empty())
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(header(&running), "\u{25cf} Shell(command: \"cargo test\")");
        assert_eq!(header(&running), header(&completed));
        assert_eq!(header(&running), header(&failed));
    }

    /// A header is one line, so the arguments on it are capped — and the cut
    /// is admitted rather than left to look like the whole call.
    #[test]
    fn a_header_names_a_few_arguments_and_says_when_it_left_some_out() {
        let lines = tool_call(
            "grep",
            ToolState::Running {
                input: serde_json::json!({
                    "include": "*.rs",
                    "pattern": "fn main",
                    "path": "src",
                    "limit": 20,
                    "todos": ["one", "two"],
                }),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        );

        assert!(
            lines.iter().any(|line| line
                == "\u{25cf} Grep(path: \"src\", pattern: \"fn main\", include: \"*.rs\", \u{2026})"),
            "the recognizable fields come first and the cut is named, got {lines:?}"
        );
    }

    /// A value that would not fit a line — a whole file a `write` carries, a
    /// command typed over several lines — is cut to something a header can
    /// hold, and says so.
    #[test]
    fn a_header_cuts_an_argument_too_long_or_too_tall_to_draw() {
        let lines = tool_call(
            "write",
            ToolState::Running {
                input: serde_json::json!({
                    "filePath": "a.rs",
                    "content": "fn main() {\n    println!(\"hello\");\n}\n",
                }),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        );

        assert!(
            lines.iter().any(|line| line
                == "\u{25cf} Write(filePath: \"a.rs\", content: \"fn main() {\u{2026}\")"),
            "got {lines:?}"
        );
    }

    /// A nested payload is named by the shape it has rather than drawn: it is
    /// still an argument the header must admit to, and still not one a single
    /// line can carry.
    #[test]
    fn a_header_names_a_nested_argument_by_its_shape() {
        let lines = tool_call(
            "todowrite",
            ToolState::Running {
                input: serde_json::json!({"todos": [{"content": "one"}]}),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        );

        assert!(
            lines
                .iter()
                .any(|line| line == "\u{25cf} Todowrite(todos: [\u{2026}])"),
            "got {lines:?}"
        );
    }

    /// **Pre-mortem 1.** The marker's columns are measured, so a preview line
    /// the viewport has to wrap keeps hanging under the marker instead of
    /// sliding back to the margin.
    #[test]
    fn a_wrapped_preview_line_keeps_hanging_under_its_own_marker() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({}),
                    output: "alpha bravo charlie delta".to_owned(),
                    title: String::new(),
                    metadata: serde_json::json!({}),
                    started: 0,
                    completed: 1,
                },
            },
        });
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 18, 10));
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec![
                "\u{25cf} Read",
                "  \u{23bf} alpha bravo",
                "    charlie delta",
            ],
            "the wrapped remainder sits under what the marker introduced, got {lines:?}"
        );
    }

    #[test]
    fn a_completed_tool_call_shows_its_title_and_a_clamped_output_preview() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "a.rs"}),
                    output: "one\ntwo\nthree\nfour\nfive".to_owned(),
                    title: "a.rs".to_owned(),
                    metadata: serde_json::json!({}),
                    started: 0,
                    completed: 1,
                },
            },
        });
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(
            lines
                .iter()
                .any(|line| line.contains("\u{25cf} Read(filePath: \"a.rs\")")),
            "got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("one")),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\u{2026} +1 line (ctrl+t to expand)")),
            "five lines should clamp to four plus a hint naming the one cut, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("five")),
            "the fifth line should have been clamped away, got {lines:?}"
        );
    }

    #[test]
    fn a_completed_tool_call_prefers_its_diff_over_plain_output() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "edit".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "a.rs"}),
                    output: "PLAIN_OUTPUT_MARKER".to_owned(),
                    title: "a.rs".to_owned(),
                    metadata: serde_json::json!({
                        "diff": "+DIFF_ADDED_MARKER\n-DIFF_REMOVED_MARKER"
                    }),
                    started: 0,
                    completed: 1,
                },
            },
        });
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(lines.iter().any(|line| line.contains("DIFF_ADDED_MARKER")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("DIFF_REMOVED_MARKER"))
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("PLAIN_OUTPUT_MARKER")),
            "a diff should be shown instead of the plain output, got {lines:?}"
        );
    }

    #[test]
    fn an_errored_tool_call_shows_only_the_first_line_of_the_error() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Error {
                    input: serde_json::json!({"command": "rm -rf /"}),
                    error: "refused: destructive command\nsecond line stays out of the transcript"
                        .to_owned(),
                    started: 0,
                    completed: 1,
                },
            },
        });
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(
            lines
                .iter()
                .any(|line| line.contains("\u{25cf} Shell(command: \"rm -rf /\")")),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "  \u{23bf} [error] refused: destructive command"),
            "got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("second line")),
            "only the first line of the error should show, got {lines:?}"
        );
    }

    #[test]
    fn update_part_replaces_a_known_id_and_appends_an_unknown_one() {
        let mut chat = Chat::default();
        let reply = Message::assistant("canned");
        let known = Part::tool("call_1", "shell");
        chat.start_message(reply.clone());
        chat.start_part(&reply.id, known.clone());

        chat.update_part(
            &reply.id,
            Part {
                id: known.id.clone(),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "shell".to_owned(),
                    state: ToolState::Completed {
                        input: serde_json::json!({"command": "echo hi"}),
                        output: "hi".to_owned(),
                        title: "echo hi".to_owned(),
                        metadata: serde_json::json!({}),
                        started: 0,
                        completed: 1,
                    },
                },
            },
        );
        chat.update_part(
            &reply.id,
            Part {
                id: PartId::from("prt_unseen".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_2".to_owned(),
                    tool: "read".to_owned(),
                    state: ToolState::Pending,
                },
            },
        );

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(
            lines
                .iter()
                .any(|line| line.contains("\u{25cf} Shell(command: \"echo hi\")")),
            "the known id should be replaced in place, got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "\u{25cf} Read"),
            "an update for an id never started should still append, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line == "\u{25cf} Shell"),
            "the pending block should have been replaced, not kept alongside, got {lines:?}"
        );
    }

    /// A dead turn's reason belongs where the person is looking: the
    /// provider's words land under the reply they ended, in the error style,
    /// and a message the transcript never met says so instead of vanishing.
    #[test]
    fn a_failed_turns_error_is_painted_under_its_reply() {
        let mut chat = Chat::default();
        let reply = Message::assistant("canned");
        chat.start_message(reply.clone());

        assert!(
            chat.set_error(
                &reply.id,
                "Our servers are currently overloaded.".to_owned()
            ),
            "the reply is on the transcript, so the error has a home"
        );
        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("[error] Our servers are currently overloaded.")),
            "the error paints under the reply, got {lines:?}"
        );

        assert!(
            !chat.set_error(&MessageId::from("msg_ghost".to_owned()), "lost".to_owned()),
            "an entry the transcript never met reports itself unplaceable"
        );
    }

    /// A reply whose process died mid-stream reads as a reply that simply
    /// stopped talking. Saying so is the difference between a transcript that
    /// is incomplete and one that is wrong.
    #[test]
    fn a_resumed_reply_the_store_never_saw_finish_says_it_was_interrupted() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part::text("half a thought"));
        assert_eq!(reply.time.completed, None, "the fixture must be unfinished");
        chat.restore_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 70, 20));

        assert!(
            lines.iter().any(|line| line.contains("[interrupted]")),
            "an unfinished stored reply should say so, got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("half a thought")),
            "what did reach the disk still has to render, got {lines:?}"
        );
    }

    #[test]
    fn a_resumed_reply_that_finished_carries_no_interrupted_marker() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part::text("a whole thought"));
        reply.complete();
        chat.restore_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 70, 20));

        assert!(
            !lines.iter().any(|line| line.contains("[interrupted]")),
            "a completed reply must not be accused of dying, got {lines:?}"
        );
    }

    /// The same field is absent on a reply that is merely still arriving, so
    /// the marker cannot key on it alone.
    #[test]
    fn a_streaming_reply_is_not_mistaken_for_an_interrupted_one() {
        let mut chat = Chat::default();
        let reply = Message::assistant("canned");
        let part = Part::text("");
        chat.start_message(reply.clone());
        chat.start_part(&reply.id, part.clone());
        chat.append_delta(&reply.id, &part.id, "still arriving");

        let lines = rendered(&mut chat, Rect::new(0, 0, 70, 20));

        assert!(
            !lines.iter().any(|line| line.contains("[interrupted]")),
            "a live reply is unfinished, not interrupted, got {lines:?}"
        );
    }

    /// Only a reply can be cut off mid-sentence: a user message is whole the
    /// moment it is sent, whatever the store recorded about its clock.
    #[test]
    fn a_resumed_prompt_is_never_marked_interrupted() {
        let mut chat = Chat::default();
        let mut prompt = Message::user("what did I ask");
        prompt.time.completed = None;
        chat.restore_message(prompt);

        let lines = rendered(&mut chat, Rect::new(0, 0, 70, 20));

        assert!(
            !lines.iter().any(|line| line.contains("[interrupted]")),
            "got {lines:?}"
        );
    }

    #[test]
    fn clearing_leaves_nothing_of_the_previous_session_on_screen() {
        let mut chat = Chat::default();
        transcript(&mut chat, 20);
        rendered(&mut chat, VIEWPORT);

        chat.clear();

        assert!(rendered(&mut chat, VIEWPORT).iter().all(String::is_empty));
        assert!(chat.is_following_tail());
    }

    /// The `!` passthrough streams its output into a running part, so the
    /// transcript has to show what has arrived rather than waiting for the
    /// command to end.
    #[test]
    fn a_running_call_that_reports_as_it_goes_shows_the_newest_of_it() {
        let lines = tool_call(
            "bash",
            ToolState::Running {
                input: serde_json::json!({"command": "cargo test"}),
                metadata: serde_json::json!({
                    "output": "compiling\nrunning 1 test\ntest a ... ok\ntest b ... ok\ntest c ... ok\nfinished"
                }),
                started: 0,
            },
        );

        assert!(
            lines.iter().any(|line| line.contains("finished")),
            "the newest line has to be on screen, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("compiling")),
            "the oldest lines are the ones that scroll off, got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "  \u{23bf} \u{2026} +2 lines (ctrl+t to expand)"),
            "and the cut has to be admitted, above what it cut, got {lines:?}"
        );
    }

    /// The common case has no such field, and its rows must not change.
    #[test]
    fn a_running_call_that_reports_nothing_is_one_line_as_it_always_was() {
        let lines = tool_call(
            "read",
            ToolState::Running {
                input: serde_json::json!({"filePath": "a.rs"}),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        );
        let drawn: Vec<&String> = lines.iter().filter(|line| !line.is_empty()).collect();

        assert_eq!(
            drawn,
            vec![&"\u{25cf} Read(filePath: \"a.rs\")".to_owned()],
            "got {lines:?}"
        );
    }

    /// A delegated turn is one row: an icon, who is doing it, what they were
    /// asked, and what they are doing about it now.
    #[test]
    fn a_running_task_names_the_agent_the_ask_and_the_tool_it_is_in() {
        let lines = tool_call(
            "task",
            ToolState::Running {
                input: serde_json::json!({
                    "description": "find the parser",
                    "subagent_type": "explore",
                }),
                metadata: serde_json::json!({"current_tool": "grep parser", "toolcalls": 3}),
                started: 0,
            },
        );

        assert!(
            lines.iter().any(|line| line
                == "\u{25cf} Task(agent: \"explore\", description: \"find the parser\")"),
            "got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "  \u{23bf} grep parser"),
            "got {lines:?}"
        );
    }

    /// Between tools there is no current one, so the count is what the row has
    /// to say.
    #[test]
    fn a_running_task_between_tools_counts_them_instead() {
        let lines = tool_call(
            "task",
            ToolState::Running {
                input: serde_json::json!({"description": "find the parser"}),
                metadata: serde_json::json!({"toolcalls": 3}),
                started: 0,
            },
        );

        assert!(
            lines.iter().any(|line| line == "  \u{23bf} 3 toolcalls"),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "\u{25cf} Task(description: \"find the parser\")"),
            "an agent nobody named is left off rather than invented, got {lines:?}"
        );
    }

    /// What the child actually said is inside the tool result the model reads.
    /// Printing it here would show the same work twice — once as the row, once
    /// as prose the user never asked to see.
    #[test]
    fn a_finished_task_reports_its_shape_and_never_the_childs_answer() {
        let lines = tool_call(
            "task",
            ToolState::Completed {
                input: serde_json::json!({
                    "description": "find the parser",
                    "subagent_type": "explore",
                }),
                output: "<task id=\"tsk_1\" state=\"completed\"><task_result>\
                         THE CHILD'S OWN ANSWER</task_result></task>"
                    .to_owned(),
                title: "find the parser".to_owned(),
                metadata: serde_json::json!({
                    "session": "ses_child",
                    "agent": "explore",
                    "model": "fake",
                    "toolcalls": 7,
                }),
                started: 1_000,
                completed: 13_400,
            },
        );

        assert!(
            lines.iter().any(|line| line
                == "\u{25cf} Task(agent: \"explore\", description: \"find the parser\")"),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "  \u{23bf} 7 toolcalls \u{b7} 12.4s"),
            "got {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("THE CHILD'S OWN ANSWER")),
            "the child's answer belongs to the model, not to the row, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("task_result")),
            "and neither does the envelope it came in, got {lines:?}"
        );
    }

    /// A refused delegation is a refused call, and reads like every other one.
    #[test]
    fn a_failed_task_keeps_the_shape_every_other_failure_has() {
        let lines = tool_call(
            "task",
            ToolState::Error {
                input: serde_json::json!({"description": "find the parser"}),
                error: "no agent named parser-hunter".to_owned(),
                started: 0,
                completed: 1,
            },
        );

        assert!(
            lines
                .iter()
                .any(|line| line.contains("\u{25cf} Task(description: \"find the parser\")")),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "  \u{23bf} [error] no agent named parser-hunter"),
            "got {lines:?}"
        );
    }

    #[test]
    fn a_duration_is_reported_in_whatever_unit_reads_as_one() {
        let cases = [
            (0_u64, 1_u64, "1ms"),
            (0, 999, "999ms"),
            (0, 1_000, "1.0s"),
            (1_000, 13_400, "12.4s"),
            (0, 59_900, "59.9s"),
            (0, 60_000, "1m 0s"),
            (0, 3_723_000, "62m 3s"),
            // A clock that moved backwards between the two stamps.
            (5_000, 1_000, "0ms"),
        ];

        for (started, completed, expected) in cases {
            assert_eq!(
                elapsed(started, completed),
                expected,
                "{started}..{completed}"
            );
        }
    }

    /// R12's scope, from the seam's side: a reply is markdown.
    #[test]
    fn an_assistant_reply_is_rendered_as_markdown() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply
            .parts
            .push(Part::text("# Heading\n\nand **loud** text"));
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 40, 10));

        assert!(
            lines.iter().any(|line| line == "\u{25cf} Heading"),
            "the heading's marker should be concealed, got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "  and loud text"),
            "and so should the emphasis markers, under the bullet's own columns, got {lines:?}"
        );
    }

    /// The other half of the scope: what a person typed is never re-read as
    /// markup, so their `#` and `**` stay on the screen — behind the caret
    /// that says a person is who typed them.
    #[test]
    fn a_user_message_is_left_exactly_as_it_was_typed() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("# Heading and **loud** text"));

        let lines = rendered(&mut chat, Rect::new(0, 0, 40, 10));

        assert!(
            lines
                .iter()
                .any(|line| line == "> # Heading and **loud** text"),
            "got {lines:?}"
        );
    }

    /// A prompt is one block however many parts it was built from: the caret
    /// leads it once, and everything after hangs under it — so a prompt that
    /// is nothing but an attachment is still marked as something a person
    /// said.
    #[test]
    fn a_prompt_carries_one_caret_and_hangs_the_rest_of_itself_under_it() {
        let mut chat = Chat::default();
        let mut message = Message::user("look at this");
        message.parts.push(Part {
            id: PartId::from("prt_f1".to_owned()),
            body: PartBody::File {
                path: "src/lib.rs".to_owned(),
                mime: "text/plain".to_owned(),
                start: None,
                end: None,
                content: None,
            },
        });
        chat.start_message(message);

        let lines = rendered(&mut chat, Rect::new(0, 0, 40, 8));
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec!["> look at this", "  @src/lib.rs"],
            "got {lines:?}"
        );
    }

    /// The wrap cache holds styled lines, so it is as stale after a theme
    /// switch as it is after a resize. Both frames here are the same width:
    /// only the revision can invalidate the cache.
    #[test]
    fn a_theme_switch_restyles_the_lines_the_cache_already_holds() {
        let area = Rect::new(0, 0, 40, 6);
        let mut chat = Chat::default();
        chat.start_message(Message::user("what color am I"));

        let mut themes = Themes::builtin();
        let first = themes.select("aura").expect("aura is builtin");
        let mut buffer = Buffer::empty(area);
        chat.render(area, &mut buffer, &first);
        let before = buffer[(0, 0)].fg;

        let second = themes.select("gruvbox").expect("gruvbox is builtin");
        assert_ne!(
            first.revision(),
            second.revision(),
            "a switch has to change the revision, or nothing below is tested"
        );
        chat.render(area, &mut buffer, &second);

        assert_ne!(
            before,
            buffer[(0, 0)].fg,
            "the cached line kept the old palette"
        );
    }

    /// A transcript of four entries, and the id of the third — which is what
    /// an undo of the last exchange anchors on.
    fn reverted_transcript() -> (Chat, MessageId) {
        /// A reply saying `text`, which is a part rather than the model name
        /// `Message::assistant` takes.
        fn reply(text: &str) -> Message {
            let mut message = Message::assistant("canned");
            message.parts.push(Part::text(text));

            message
        }

        let mut chat = Chat::default();
        chat.start_message(Message::user("the first question"));
        chat.start_message(reply("the first answer"));
        chat.start_message(Message::user("the second question"));
        chat.start_message(reply("the second answer"));

        let anchor = chat.entries[2].id.clone();

        (chat, anchor)
    }

    #[test]
    fn a_revert_hides_the_anchor_and_everything_after_it() {
        let (mut chat, anchor) = reverted_transcript();

        chat.revert(anchor, vec!["src/lib.rs".to_owned()]);
        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));
        let screen = lines.join("\n");

        assert!(screen.contains("the first question"), "{screen}");
        assert!(screen.contains("the first answer"), "{screen}");
        assert!(
            !screen.contains("the second question"),
            "the anchor itself is hidden too:\n{screen}"
        );
        assert!(!screen.contains("the second answer"), "{screen}");
    }

    /// The whole of the row's job: how much went away, and the way back.
    #[test]
    fn the_marker_row_counts_what_it_hides_and_names_the_files() {
        let (mut chat, anchor) = reverted_transcript();

        chat.revert(
            anchor,
            vec!["src/lib.rs".to_owned(), "src/app.rs".to_owned()],
        );
        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(
            lines
                .iter()
                .any(|line| line == "2 messages reverted \u{2014} /redo to restore"),
            "got {lines:?}"
        );
        for file in ["src/lib.rs", "src/app.rs"] {
            assert!(
                lines.iter().any(|line| line.contains(file)),
                "{file} should be named, got {lines:?}"
            );
        }
    }

    /// One hidden message is one message, not "1 messages".
    #[test]
    fn the_marker_row_counts_a_single_message_in_the_singular() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("kept"));
        chat.start_message(Message::user("taken back"));
        let anchor = chat.entries[1].id.clone();

        chat.revert(anchor, Vec::new());
        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(
            lines
                .iter()
                .any(|line| line == "1 message reverted \u{2014} /redo to restore"),
            "got {lines:?}"
        );
    }

    /// A revert that put no file back is a revert of the conversation, and
    /// still worth a row: the messages really are hidden.
    #[test]
    fn a_revert_that_moved_no_files_still_draws_its_row() {
        let (mut chat, anchor) = reverted_transcript();

        chat.revert(anchor, Vec::new());
        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

        assert!(
            lines
                .iter()
                .any(|line| line == "2 messages reverted \u{2014} /redo to restore"),
            "got {lines:?}"
        );
    }

    /// What a redo past the newest undone prompt gets: the entries were never
    /// gone.
    #[test]
    fn unreverting_shows_the_hidden_entries_again_and_takes_the_row_away() {
        let (mut chat, anchor) = reverted_transcript();
        chat.revert(anchor, vec!["src/lib.rs".to_owned()]);
        rendered(&mut chat, Rect::new(0, 0, 60, 20));

        chat.unrevert();
        let screen = rendered(&mut chat, Rect::new(0, 0, 60, 20)).join("\n");

        assert!(!chat.is_reverted());
        assert!(screen.contains("the second question"), "{screen}");
        assert!(screen.contains("the second answer"), "{screen}");
        assert!(!screen.contains("reverted"), "{screen}");
    }

    /// What a prompt after an undo gets: the engine deleted those messages, so
    /// there is nothing left for a later redo to bring back.
    #[test]
    fn dropping_a_revert_removes_the_hidden_entries_for_good() {
        let (mut chat, anchor) = reverted_transcript();
        chat.revert(anchor, vec!["src/lib.rs".to_owned()]);

        chat.drop_reverted();
        chat.unrevert();
        let screen = rendered(&mut chat, Rect::new(0, 0, 60, 20)).join("\n");

        assert_eq!(chat.entries.len(), 2, "the hidden tail should be gone");
        assert!(screen.contains("the first answer"), "{screen}");
        assert!(
            !screen.contains("the second question"),
            "an unrevert after a drop has nothing to put back:\n{screen}"
        );
    }

    /// A copy is of the conversation on screen, and the hidden tail is not on
    /// it — nor in what the next request will carry.
    #[test]
    fn the_copy_surfaces_read_the_visible_transcript_and_not_the_hidden_tail() {
        let (mut chat, anchor) = reverted_transcript();

        assert_eq!(chat.messages().len(), 4);
        chat.revert(anchor, Vec::new());
        assert_eq!(chat.messages().len(), 2);
    }

    /// Scrolling has to agree with what was drawn, so the row it stands in for
    /// counts as lines like everything else.
    #[test]
    fn the_marker_rows_lines_are_part_of_what_the_viewport_can_scroll() {
        let (mut chat, anchor) = reverted_transcript();
        rendered(&mut chat, Rect::new(0, 0, 60, 20));
        let whole = chat.line_count();

        chat.revert(anchor, vec!["src/lib.rs".to_owned()]);
        rendered(&mut chat, Rect::new(0, 0, 60, 20));

        // Two entries' lines are gone — two apiece, a caret or bullet row and
        // the blank every entry ends with — and the marker's three — a
        // headline, one file and a blank of its own — took their place.
        assert_eq!(chat.line_count(), whole - 4 + 3);
    }

    /// Starting a fresh conversation ends the revert with it: the session the
    /// undo happened in is not the one on screen any more.
    #[test]
    fn clearing_the_transcript_ends_the_revert_too() {
        let (mut chat, anchor) = reverted_transcript();
        chat.revert(anchor, Vec::new());

        chat.clear();

        assert!(!chat.is_reverted());
        assert_eq!(chat.line_count(), 0);
    }

    /// A reply carrying one patch part naming `files`.
    fn patched(files: &[&str]) -> Message {
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_patch".to_owned()),
            body: PartBody::Patch {
                hash: "4b825dc".to_owned(),
                files: files.iter().map(|file| (*file).to_owned()).collect(),
            },
        });

        reply
    }

    /// **F7.** One checkpoint per prompt, newest first, each counting the
    /// distinct files its own span changed — a file two steps of one turn both
    /// touched is one file, and a span with no patches counts none.
    #[test]
    fn checkpoints_are_the_prompts_newest_first_with_their_spans_file_counts() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("change two files"));
        chat.start_message(patched(&["src/lib.rs", "src/app.rs"]));
        // A second step of the same turn, touching one of them again.
        chat.start_message(patched(&["src/lib.rs"]));
        chat.start_message(Message::user("now just explain it"));
        chat.start_message(Message::assistant("canned"));

        let checkpoints = chat.checkpoints();

        assert_eq!(checkpoints.len(), 2);
        assert_eq!(checkpoints[0].title, "now just explain it");
        assert_eq!(checkpoints[0].files, 0, "that turn changed nothing");
        assert_eq!(checkpoints[1].title, "change two files");
        assert_eq!(
            checkpoints[1].files, 2,
            "two distinct files, however many patches named them"
        );
        assert_eq!(checkpoints[1].message_id, chat.entries[0].id);
    }

    /// The picker offers what the screen offers: a reverted tail is not on
    /// screen, so it is not something to rewind to either.
    #[test]
    fn checkpoints_leave_out_what_a_revert_is_already_hiding() {
        let (mut chat, anchor) = reverted_transcript();
        assert_eq!(chat.checkpoints().len(), 2);

        chat.revert(anchor, Vec::new());

        let checkpoints = chat.checkpoints();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].title, "the first question");
    }

    /// A prompt is one line on a checkpoint row however many it was typed
    /// over, and a prompt with no text at all is still identifiable.
    #[test]
    fn a_checkpoint_is_titled_by_the_prompts_first_line() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("\n\n  the first real line  \nand more"));
        let mut wordless = Message::user("");
        wordless.parts.clear();
        let id = wordless.id.clone();
        chat.start_message(wordless);

        let checkpoints = chat.checkpoints();

        assert_eq!(checkpoints[1].title, "the first real line");
        assert_eq!(checkpoints[0].title, id.as_str());
    }

    /// A turn that started `seconds` ago, having spent `output_tokens`.
    fn working(turn: u64, seconds: u64, output_tokens: u64) -> Working {
        Working {
            started: Instant::now()
                .checked_sub(Duration::from_secs(seconds))
                .expect("the test clock is well past the epoch"),
            turn,
            output_tokens,
        }
    }

    /// **AC4.** While a turn runs the tail says so, with what it has spent;
    /// when the turn settles the line is gone and nothing of it is left in the
    /// count the viewport scrolls by.
    #[test]
    fn the_tail_says_a_turn_is_working_and_takes_it_back_when_the_turn_settles() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("go on then"));
        let area = Rect::new(0, 0, 60, 10);
        rendered(&mut chat, area);
        let settled = chat.line_count();

        chat.set_working(Some(working(1, 12, 431)));
        let lines = rendered(&mut chat, area);

        assert!(
            lines
                .iter()
                .any(|line| line == "\u{273b} Thinking\u{2026} (12s \u{b7} \u{2191} 431 tokens)"),
            "got {lines:?}"
        );
        assert_eq!(chat.line_count(), settled + 1, "one line, and only one");

        chat.set_working(None);
        let lines = rendered(&mut chat, area);

        assert!(
            !lines.iter().any(|line| line.contains('\u{273b}')),
            "a settled turn leaves no working line, got {lines:?}"
        );
        assert_eq!(chat.line_count(), settled);
    }

    /// A session that has spent nothing yet says nothing about tokens, rather
    /// than claiming a zero the screen would contradict.
    #[test]
    fn a_working_line_with_nothing_spent_yet_draws_no_token_segment() {
        let mut chat = Chat::default();
        chat.set_working(Some(working(1, 3, 0)));

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 6));

        assert!(
            lines
                .iter()
                .any(|line| line == "\u{273b} Thinking\u{2026} (3s)"),
            "got {lines:?}"
        );
    }

    /// The verb rotates with the turn and repeats around the list, so the same
    /// transcript replayed twice reads the same both times.
    #[test]
    fn the_working_verb_rotates_with_the_turn_and_wraps_around() {
        let verb = |turn: u64| {
            let mut chat = Chat::default();
            chat.set_working(Some(working(turn, 0, 0)));
            rendered(&mut chat, Rect::new(0, 0, 40, 4))
                .into_iter()
                .find(|line| !line.is_empty())
                .unwrap_or_default()
        };

        assert_eq!(verb(0), "\u{273b} Working\u{2026} (0s)");
        assert_eq!(verb(1), "\u{273b} Thinking\u{2026} (0s)");
        assert_ne!(verb(1), verb(2));
        assert_eq!(
            verb(6),
            verb(0),
            "six verbs in, the list starts over rather than running out"
        );
    }

    /// **Pre-mortem 2.** The working line rides the marker row's seam, so it
    /// neither breaks the tail-follow nor moves a viewport somebody pinned.
    #[test]
    fn the_working_line_disturbs_neither_the_tail_nor_a_pinned_viewport() {
        let mut chat = Chat::default();
        transcript(&mut chat, 20);
        rendered(&mut chat, VIEWPORT);
        assert!(chat.is_following_tail());

        chat.set_working(Some(working(1, 5, 0)));
        let lines = rendered(&mut chat, VIEWPORT);
        assert!(chat.is_following_tail(), "the tail is still followed");
        assert!(
            lines.last().is_some_and(|line| line.contains('\u{273b}')),
            "and the working line is what the tail now ends on, got {lines:?}"
        );

        chat.set_working(None);
        chat.scroll_lines(-9);
        let pinned = rendered(&mut chat, VIEWPORT);
        assert!(!chat.is_following_tail());

        chat.set_working(Some(working(1, 6, 0)));

        assert_eq!(
            rendered(&mut chat, VIEWPORT),
            pinned,
            "a line arriving at the bottom must not move a reader who is not there"
        );
    }

    /// **AC3.** Thinking a person can read renders behind its own marker, dim
    /// and italic so it never competes with the answer it is on the way to,
    /// and hangs its continuation under the marker's own columns.
    #[test]
    fn readable_thinking_renders_dim_and_italic_behind_its_own_marker() {
        let theme = Theme::default();
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part::reasoning_text(
            "A greeting is enough, so keep it short",
        ));
        reply.parts.push(Part::text("Hello, world!"));
        chat.start_message(reply);

        let area = Rect::new(0, 0, 24, 10);
        let lines = rendered(&mut chat, area);
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec![
                "\u{273b} A greeting is enough,",
                "  so keep it short",
                "\u{25cf} Hello, world!",
            ],
            "got {lines:?}"
        );

        let mut buffer = Buffer::empty(area);
        chat.render(area, &mut buffer, &theme);
        let marker = buffer[(0u16, 0u16)].style();
        assert_eq!(marker.fg, theme.dim.fg, "thinking recedes");
        assert!(
            marker.add_modifier.contains(Modifier::ITALIC),
            "and is set apart from the reply by more than its color"
        );
    }

    /// **Pre-mortem 3.** A long think is clamped from the tail while it
    /// arrives — the newest lines are the ones worth the rows — and the cut
    /// says where the whole of it went.
    #[test]
    fn a_long_think_is_clamped_from_its_newest_end() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part::reasoning_text(
            "one\ntwo\nthree\nfour\nfive\nsix\nseven",
        ));
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 12));
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec![
                "\u{273b} \u{2026} +3 lines (ctrl+t to expand)",
                "  four",
                "  five",
                "  six",
                "  seven",
            ],
            "got {lines:?}"
        );
    }

    /// A part the provider opened and has not filled is not a thought yet, so
    /// it draws no marker standing on its own.
    #[test]
    fn an_empty_thinking_part_draws_nothing_at_all() {
        let mut chat = Chat::default();
        let reply = Message::assistant("canned");
        let part = Part::reasoning_text(String::new());
        chat.start_message(reply.clone());
        chat.start_part(&reply.id, part.clone());

        assert!(
            rendered(&mut chat, Rect::new(0, 0, 40, 6))
                .iter()
                .all(String::is_empty),
            "an unfilled part is a marker about nothing"
        );

        // And the same part grows on a delta, which is how it arrives at all:
        // the event names an id and a fragment, never which kind of text.
        chat.append_delta(&reply.id, &part.id, "now there is a thought");
        let screen = rendered(&mut chat, Rect::new(0, 0, 40, 6)).join("\n");
        assert!(
            screen.contains("\u{273b} now there is a thought"),
            "got:\n{screen}"
        );
    }

    /// **AC3, the half this build can answer.** Sealed reasoning is a blob only
    /// the provider can open; there is nothing in it for a `✻` line to say, so
    /// the part draws nothing at all — and the reply around it is untouched.
    #[test]
    fn sealed_reasoning_draws_nothing_and_leaves_the_reply_alone() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part::reasoning(
            "anthropic",
            "rs_1",
            Some("OPAQUE".to_owned()),
        ));
        reply.parts.push(Part::text("the answer itself"));
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 10));
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec!["\u{25cf} the answer itself"],
            "a blob is not a thought anybody can read, got {lines:?}"
        );
    }

    /// **D467.** The highlighted message's rows carry the selection style,
    /// its breathing-room blank stays unpainted, and every other row keeps
    /// its own colors.
    #[test]
    fn the_backtrack_highlight_paints_the_anchors_rows_only() {
        let theme = Theme::default();
        let mut chat = Chat::default();
        let first = Message::user("first prompt");
        let anchor = first.id.clone();
        chat.start_message(first);
        chat.start_message(Message::user("second prompt"));
        chat.set_backtrack(Some(anchor));

        let area = Rect::new(0, 0, 20, 10);
        let mut buffer = Buffer::empty(area);
        chat.render(area, &mut buffer, &theme);

        // Rows 0-1 are the first entry (its one caret row, then blank), 2-3
        // the second.
        let style = |row: u16| buffer[(0u16, row)].style();
        assert_eq!(style(0).fg, theme.selection.fg, "the prompt row is painted");
        assert_ne!(style(1), style(0), "the breathing-room blank is not");
        assert_ne!(style(2).fg, theme.selection.fg, "the next message is not");
    }

    /// Stepping the highlight to a message above the viewport scrolls it into
    /// view — once, on the frame after the step, so a later scroll stays
    /// where the reader put it.
    #[test]
    fn the_backtrack_highlight_scrolls_into_view_once() {
        let mut chat = Chat::default();
        let first = Message::user("the oldest prompt");
        let anchor = first.id.clone();
        chat.start_message(first);
        transcript(&mut chat, 12);

        let screen = rendered(&mut chat, VIEWPORT).join("\n");
        assert!(
            !screen.contains("the oldest prompt"),
            "the fixture starts with the anchor scrolled away:\n{screen}"
        );

        chat.set_backtrack(Some(anchor));
        let screen = rendered(&mut chat, VIEWPORT).join("\n");
        assert!(
            screen.contains("the oldest prompt"),
            "the highlight is brought into view:\n{screen}"
        );

        chat.scroll_lines(isize::try_from(chat.line_count()).unwrap_or(isize::MAX));
        let screen = rendered(&mut chat, VIEWPORT).join("\n");
        assert!(
            !screen.contains("the oldest prompt"),
            "a later scroll is not snapped back:\n{screen}"
        );
    }
}
