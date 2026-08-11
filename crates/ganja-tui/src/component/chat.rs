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

use std::collections::HashMap;

use ganja_protocol::{Message, MessageId, Part, PartBody, PartId, Role, ToolState};
use ratatui::{buffer::Buffer, layout::Rect, style::Style, text::Line};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::{markdown, mention, theme::Theme};

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

/// How an entry names who wrote it.
fn label(role: Role) -> &'static str {
    match role {
        Role::User => "you",
        Role::Assistant => "ganja",
    }
}

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
    /// Stage 1 of the cache: one parsed markdown document per assistant text
    /// part. Deliberately *not* inside [`Wrapped`] — a resize and a streamed
    /// delta both clear that, and neither is a reason to parse again.
    markdown: HashMap<PartId, markdown::Document>,
    wrapped: Option<Wrapped>,
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
            markdown: HashMap::new(),
            wrapped: None,
        });
        self.follow_tail();
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

    /// Empties the transcript, which is what switching sessions does to it.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.revert = None;
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

    /// Extends a part, which is how a streamed reply grows.
    pub fn append_delta(&mut self, message_id: &MessageId, part_id: &PartId, delta: &str) {
        let Some(entry) = self.entry_mut(message_id) else {
            return;
        };

        if let Some(text) = entry
            .parts
            .iter_mut()
            .find(|part| part.id == *part_id)
            .and_then(Part::as_text_mut)
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
    }

    /// Lines the whole transcript wrapped to at the last rendered width.
    pub(crate) fn line_count(&self) -> usize {
        let entries: usize = self.shown().iter().map(|entry| entry.lines().len()).sum();

        entries
            + self
                .revert
                .as_ref()
                .map_or(0, |revert| revert.lines().len())
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
    fn visible(&self, offset: usize) -> impl Iterator<Item = &Line<'static>> {
        let mut left_to_skip = offset;
        let marker: &[Line<'static>] = self.revert.as_ref().map_or(&[], Revert::lines);

        self.shown()
            .iter()
            .map(Entry::lines)
            .chain(std::iter::once(marker))
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

        let body_style = match self.role {
            Role::User => theme.accent,
            Role::Assistant => theme.fg,
        };

        let mut lines = vec![Line::styled(label(self.role).to_owned(), theme.dim)];
        // Parts wrap on their own so that a tool block can carry its own
        // style instead of the running text style around it.
        for part in &self.parts {
            match &part.body {
                // A reply is markdown; a prompt is what the user typed. The
                // split is the whole of R12's scope, and it is made here so
                // that neither renderer has to ask who wrote the part.
                PartBody::Text { text } if self.role == Role::Assistant => {
                    let document = self.markdown.entry(part.id.clone()).or_default();
                    document.update(text, theme);
                    lines.extend(
                        document
                            .lines()
                            .flat_map(|line| markdown::wrap(line, usize::from(width))),
                    );
                }
                PartBody::Text { text } => {
                    lines.extend(
                        wrap(text, usize::from(width))
                            .into_iter()
                            .map(|line| Line::styled(line, body_style)),
                    );
                }
                PartBody::Tool { tool, state, .. } => {
                    for (text, style) in tool_lines(tool, state, theme) {
                        lines.extend(
                            wrap(&text, usize::from(width))
                                .into_iter()
                                .map(|line| Line::styled(line, style)),
                        );
                    }
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
                    lines.push(Line::styled(label, theme.dim));
                }
                // Sealed reasoning has no rendering: what it holds is opaque
                // to everything but the provider, so a line about it would be
                // a line about a blob.
                PartBody::StepStart
                | PartBody::StepFinish { .. }
                | PartBody::Patch { .. }
                | PartBody::Reasoning { .. } => {}
            }
        }
        if self.interrupted {
            lines.extend(
                wrap(INTERRUPTED, usize::from(width))
                    .into_iter()
                    .map(|line| Line::styled(line, theme.error)),
            );
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

/// Tool argument keys tried in priority order when deriving a compact title
/// from a call's input. Tool-agnostic on purpose: an unfamiliar tool still
/// shows something recognizable instead of just its bare name.
const TITLE_KEYS: [&str; 5] = ["command", "filePath", "path", "pattern", "url"];

/// Lines a tool call's output or diff may show before the rest is clamped.
/// The full text is what the model saw; the transcript only needs the gist.
const TOOL_PREVIEW_LINES: usize = 4;

/// The tool whose call is a whole second agent loop, and which is drawn as one
/// inline row rather than as a block of output.
const TASK_TOOL: &str = "task";

/// What marks a task row that is still running, and one that finished
/// (upstream `routes/session/index.tsx:2213-2309`).
const TASK_RUNNING: &str = "\u{2502}";
/// See [`TASK_RUNNING`].
const TASK_DONE: &str = "\u{2713}";

/// What introduces the task row's second line.
///
/// The arrow alone, without the indent upstream draws in front of it: the
/// transcript's wrap collapses leading whitespace on every line it lays out, so
/// an indent here would be a claim the screen never honors.
const TASK_DETAIL: &str = "\u{21b3} ";

/// Picks a short, recognizable field out of a call's arguments, so a running
/// or failed call can name what it is doing without repeating the raw JSON.
fn derive_title(input: &serde_json::Value) -> Option<String> {
    let object = input.as_object()?;
    TITLE_KEYS
        .iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

/// One line naming the tool, with `title` appended when there is one.
fn tool_heading(tool: &str, marker: &str, title: Option<&str>) -> String {
    match title.filter(|title| !title.is_empty()) {
        Some(title) => format!("[{marker}] {tool}: {title}"),
        None => format!("[{marker}] {tool}"),
    }
}

/// The first `TOOL_PREVIEW_LINES` lines of `text`, with a marker appended
/// when more were cut.
fn clamp_preview(text: &str) -> Vec<String> {
    let mut lines = text.lines();
    let mut preview: Vec<String> = lines
        .by_ref()
        .take(TOOL_PREVIEW_LINES)
        .map(str::to_owned)
        .collect();
    if lines.next().is_some() {
        preview.push("...".to_owned());
    }
    preview
}

/// The **last** `TOOL_PREVIEW_LINES` lines of `text`, with a marker in front
/// when earlier ones were cut.
///
/// The other end from [`clamp_preview`], and for the other case: output that
/// is still arriving. A command's newest line is the one worth a row, where a
/// finished call's first line is the one that says what it did.
fn clamp_tail(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let skipped = lines.len().saturating_sub(TOOL_PREVIEW_LINES);

    let mut tail: Vec<String> = lines[skipped..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
    if skipped > 0 {
        tail.insert(0, "...".to_owned());
    }

    tail
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
/// `Pending`/`Running` share a heading so a call reads the same before and
/// after its arguments arrive; `StepStart`/`StepFinish` never reach here.
fn tool_lines(tool: &str, state: &ToolState, theme: &Theme) -> Vec<(String, Style)> {
    // A delegated turn is one row, never a transcript of its own: everything
    // the child said reaches the model inside the tool result, and repeating
    // it here would show the same work twice.
    if tool == TASK_TOOL && !matches!(state, ToolState::Error { .. }) {
        return task_lines(state, theme);
    }

    match state {
        ToolState::Pending => vec![(tool_heading(tool, "running", None), theme.dim)],
        ToolState::Running {
            input, metadata, ..
        } => {
            let mut lines = vec![(
                tool_heading(tool, "running", derive_title(input).as_deref()),
                theme.dim,
            )];
            // A call that reports as it goes — the `!` passthrough streaming a
            // command's output — redraws its tail every time the part is
            // republished, so the newest lines are the ones on screen.
            if let Some(output) = metadata
                .get("output")
                .and_then(serde_json::Value::as_str)
                .filter(|output| !output.is_empty())
            {
                lines.extend(
                    clamp_tail(output)
                        .into_iter()
                        .map(|line| (format!("  {line}"), theme.dim)),
                );
            }

            lines
        }
        ToolState::Completed {
            output,
            title,
            metadata,
            ..
        } => {
            let mut lines = vec![(tool_heading(tool, "done", Some(title.as_str())), theme.fg)];
            let diff = metadata
                .get("diff")
                .and_then(serde_json::Value::as_str)
                .filter(|diff| !diff.is_empty());

            if let Some(diff) = diff {
                lines.extend(clamp_preview(diff).into_iter().map(|line| {
                    let style = diff_line_style(&line, theme);
                    (format!("  {line}"), style)
                }));
            } else if !output.is_empty() {
                lines.extend(
                    clamp_preview(output)
                        .into_iter()
                        .map(|line| (format!("  {line}"), theme.dim)),
                );
            }

            lines
        }
        ToolState::Error { input, error, .. } => {
            let mut lines = vec![(
                tool_heading(tool, "error", derive_title(input).as_deref()),
                theme.error,
            )];
            if let Some(first) = error.lines().next().filter(|line| !line.is_empty()) {
                lines.push((format!("  {first}"), theme.error));
            }
            lines
        }
    }
}

/// The two lines a delegated turn gets, whatever it is doing.
///
/// Spec: upstream `routes/session/index.tsx:2213-2309`. Line one names the
/// agent and what it was asked for; line two says what it is doing now, or
/// what it did. **The child's own answer is never on the row** — it is inside
/// the tool result the model reads, and a transcript that printed it would be
/// showing the same work twice, once as prose and once as a result.
fn task_lines(state: &ToolState, theme: &Theme) -> Vec<(String, Style)> {
    match state {
        ToolState::Pending => vec![(format!("{TASK_RUNNING} Task"), theme.dim)],
        ToolState::Running {
            input, metadata, ..
        } => {
            let agent = field(input, "subagent_type");
            let mut lines = vec![(
                task_heading(TASK_RUNNING, agent, field(input, "description")),
                theme.dim,
            )];
            // Upstream's own priority: the tool the child is running right now
            // says more than how many it has run.
            let detail = match field(metadata, "current_tool") {
                Some(current) => current.to_owned(),
                None => format!("{} toolcalls", toolcalls(metadata)),
            };
            lines.push((format!("{TASK_DETAIL}{detail}"), theme.dim));

            lines
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
                (task_heading(TASK_DONE, agent, description), theme.fg),
                (
                    format!(
                        "{TASK_DETAIL}{calls} toolcalls \u{b7} {elapsed}",
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

/// The task row's first line: a marker, the agent doing the work, and what it
/// was asked for.
fn task_heading(marker: &str, agent: Option<&str>, description: Option<&str>) -> String {
    let mut heading = String::from(marker);
    heading.push(' ');
    if let Some(agent) = agent {
        heading.push_str(&titlecase(agent));
        heading.push(' ');
    }
    heading.push_str("Task");
    if let Some(description) = description {
        heading.push_str(" \u{2014} ");
        heading.push_str(description);
    }

    heading
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

#[cfg(test)]
mod tests {
    use ganja_protocol::{Message, MessageId, Part, PartBody, PartId, ToolState};
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Chat, elapsed, split_at_width, wrap};
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
            lines.iter().any(|line| line == "hello world"),
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
            lines.iter().any(|line| line == "first") && lines.iter().any(|line| line == "second"),
            "both parts should render, got {lines:?}"
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
            lines.iter().any(|line| line.contains("[running] shell")),
            "a pending call should read as running, got {lines:?}"
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
                .any(|line| line.contains("[running] shell: cargo test")),
            "got {lines:?}"
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
            lines.iter().any(|line| line.contains("[done] read: a.rs")),
            "got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("one")),
            "got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("...")),
            "five lines should clamp to four plus a marker, got {lines:?}"
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
            lines.iter().any(|line| line.contains("[error] shell")),
            "got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("refused")),
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
                .any(|line| line.contains("[done] shell: echo hi")),
            "the known id should be replaced in place, got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("[running] read")),
            "an update for an id never started should still append, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("[running] shell")),
            "the pending block should have been replaced, not kept alongside, got {lines:?}"
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
            lines.iter().any(|line| line.trim() == "..."),
            "and the cut has to be admitted, got {lines:?}"
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
            vec![&"ganja".to_owned(), &"[running] read: a.rs".to_owned()],
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
            lines
                .iter()
                .any(|line| line.contains("\u{2502} Explore Task \u{2014} find the parser")),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\u{21b3} grep parser")),
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
            lines
                .iter()
                .any(|line| line.contains("\u{21b3} 3 toolcalls")),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Task \u{2014} find the parser")),
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
            lines
                .iter()
                .any(|line| line.contains("\u{2713} Explore Task \u{2014} find the parser")),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\u{21b3} 7 toolcalls \u{b7} 12.4s")),
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
            lines.iter().any(|line| line.contains("[error] task")),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("no agent named parser-hunter")),
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
            lines.iter().any(|line| line == "Heading"),
            "the heading's marker should be concealed, got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "and loud text"),
            "and so should the emphasis markers, got {lines:?}"
        );
    }

    /// The other half of the scope: what a person typed is never re-read as
    /// markup, so their `#` and `**` stay on the screen.
    #[test]
    fn a_user_message_is_left_exactly_as_it_was_typed() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("# Heading and **loud** text"));

        let lines = rendered(&mut chat, Rect::new(0, 0, 40, 10));

        assert!(
            lines
                .iter()
                .any(|line| line == "# Heading and **loud** text"),
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
        let before = buffer[(0, 1)].fg;

        let second = themes.select("gruvbox").expect("gruvbox is builtin");
        assert_ne!(
            first.revision(),
            second.revision(),
            "a switch has to change the revision, or nothing below is tested"
        );
        chat.render(area, &mut buffer, &second);

        assert_ne!(
            before,
            buffer[(0, 1)].fg,
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

        // Two entries' lines are gone and the marker's three — a headline, one
        // file and the blank line every block ends with — took their place.
        assert_eq!(chat.line_count(), whole - 6 + 3);
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
}
