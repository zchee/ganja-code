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
//! width-independent markdown blocks by `crate::markdown` first, and only
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
//! is made of and every settled tool call it makes — a call still in flight
//! leads with a pulsing `\u{2022}` point instead (2026-08-25) — a `\u{23bf}`
//! marker introduces what
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
//! word. Two later screenshots (2026-08-14) pin two more results the same way:
//! a `read` of a **directory** answers with `Listed N entries` rather than the
//! envelope it writes for the model, and a `todowrite` answers with the
//! checklist itself — `\u{2610}`/`\u{2612}` a row each — drawn both on the call's
//! own row and, while the turn is still running, under the working line.
//!
//! Three more screenshots (2026-08-15) pin the strip and the verdicts: the
//! working line and the checklist under it sit in a strip **pinned above the
//! composer** rather than riding the transcript's tail, the line is painted
//! its own orange with a brighter band sweeping left to right, and a settled
//! call's bullet answers "did it work" — green for a call that did, red for
//! one that failed — while the heading beside it stays prose.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use ganja_protocol::{Message, MessageId, Part, PartBody, PartId, Role, ToolState, team};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::{component::rewind, graphics, markdown, mention, theme::Theme};

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

/// What leads a tool call still in flight: Claude Code's own smaller point,
/// pulsing while the call runs (user directive, 2026-08-25, two reference
/// screenshot pairs) — the icon moves, the words hold still. A settled call
/// takes `BULLET`, whose color answers the verdict.
const POINT: &str = "\u{2022} ";

/// How long each pulse phase holds: bright for one, the chrome's own dim for
/// the next — two states rather than a fade, because a theme's palette is a
/// set of styles, not RGB endpoints anything could blend.
const POINT_BLINK: Duration = Duration::from_millis(500);

/// What leads what a call answered, one step under the header it answers —
/// and, in the `/team` dialog, a member's ring of recent calls: a call log is
/// the same thing there and here and should read the same way.
pub(crate) const RESULT: &str = "  \u{23bf} ";

/// What leads a prompt, in place of the author's name the pane used to head
/// every message with.
const PROMPT: &str = "> ";

/// What leads thinking a person can read in the transcript: `∴`, the
/// therefore sign — a conclusion being drawn — where Claude Code's grammar
/// (D487) has `✻`. The one deliberate departure from that screenshot, by user
/// directive (2026-08-25).
const THINKING: &str = "\u{2234} ";

/// The frames the working line's glyph turns through: Claude Code's own
/// spinner set, forward and then back (`·✢✳✶✻✽✻✶✳✢`), of which `✻` was the
/// one frame the 2026-08-15 screenshot had frozen. The line has moved through
/// them since 2026-08-25 (user directive) — the working line is the turn's
/// pulse and keeps that program's mark, where a thought on the page is
/// ganja's to mark (`THINKING`).
const WORKING_FRAMES: [&str; 10] = [
    "\u{b7}", "\u{2722}", "\u{2733}", "\u{2736}", "\u{273b}", "\u{273d}", "\u{273b}", "\u{2736}",
    "\u{2733}", "\u{2722}",
];

/// Milliseconds one frame is held. A screenshot pins no cadence, so this is
/// set by eye: 120 — an ink spinner's neighbourhood — read as a flicker on
/// the day it landed, and 200 is slow enough that a frame is a thing seen
/// (user directive, 2026-08-25; a whole cycle is two seconds).
const WORKING_FRAME_STEP: u128 = 200;

/// The frame `elapsed` into a turn falls on — time-driven off the same clock
/// as the shimmer band and the seconds figure, so nothing here keeps a phase
/// of its own and the same instant read twice draws the same glyph twice.
fn working_frame(elapsed: Duration) -> &'static str {
    let index = usize::try_from(elapsed.as_millis() / WORKING_FRAME_STEP).unwrap_or(usize::MAX)
        % WORKING_FRAMES.len();
    WORKING_FRAMES[index]
}

/// The words a working line runs under, one per turn in order.
///
/// **Ganja's own vocabulary.** The *shape* of the line is Claude Code's, from
/// the screenshot; the words are not ported — those are that program's voice,
/// and upstream opencode has no such line at all to take one from. They are
/// deliberately machine-plain: a loop churns and grinds, and none of these
/// claims more about what is happening inside than is true.
const WORKING_VERBS: [&str; 16] = [
    "Working",
    "Thinking",
    "Churning",
    "Grinding",
    "Whirring",
    "Chewing",
    "Crunching",
    "Mulling",
    "Simmering",
    "Percolating",
    "Digesting",
    "Humming",
    "Spinning",
    "Ticking",
    "Brewing",
    "Kneading",
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
    /// Whether the terminal can draw pixels (kitty graphics, set once at
    /// startup): on, an attached image's row becomes rows of Unicode
    /// placeholder cells the terminal composites the picture over; off, the
    /// token-and-mime row stands as always (2026-08-15).
    graphics: bool,
    /// Per attached-image path: the id its pixels travel under and the cell
    /// columns its box stands, filled in by [`crate::app::App`] once the
    /// file has been read and transmitted. The zero id is a file that would
    /// not decode — its box stays blank and is never asked for again.
    image_cells: HashMap<String, (u32, u16)>,
    /// The image paths the last wrap wanted cells for and did not have:
    /// what the app loads and transmits after the frame.
    image_wanted: Vec<String>,
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
    /// The lines [`Chat::lay_out_working`] built on the last call: the
    /// working line itself, and under it this turn's checklist while the turn
    /// has one (**D487**).
    ///
    /// Rebuilt on every layout rather than cached on width and theme like
    /// every other block here, because its text moves with the clock and its
    /// paint with the shimmer — there is nothing for such a cache to key on,
    /// and the frames are ones a running turn already forces. Kept between
    /// the layout and [`Chat::render_working`] because the app sizes its
    /// vertical split off the count before it has an area to draw into.
    working_lines: Vec<Line<'static>>,
    /// When a finished compaction's turn settled, while its full gauge is
    /// being held on screen: [`Chat::settle_working`] starts this clock
    /// instead of clearing the strip, and the next layout past
    /// [`COMPACT_SETTLE`] clears it — a bar that reached 100 and vanished in
    /// the same frame would never have been seen to arrive.
    settling: Option<Instant>,
    /// When the in-flight pulse's clock started: seeded lazily on the first
    /// render, so `Chat::default()` needs no clock and a fresh transcript
    /// always opens on the bright phase.
    blink_epoch: Option<Instant>,
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

/// What a running turn shows in the strip pinned above the composer
/// (**D487**, its seam amended by the 2026-08-15 screenshots).
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
    /// The compaction this turn is running, while it is running one: what
    /// the engine's `compaction_progress` events said last. Set, the strip
    /// wears the compacting dress instead of the verb and the shimmer (the
    /// 2026-08-25 reference screenshots).
    pub compaction: Option<Compaction>,
}

/// How far a compaction has streamed, as its progress event reports it.
#[derive(Clone, Copy, Debug)]
pub struct Compaction {
    /// Estimated tokens of summary streamed so far.
    pub tokens: u64,
    /// The output budget a summary is expected to fit, the denominator the
    /// gauge's percentage is drawn from.
    pub budget: u64,
    /// Whether the summary has arrived whole. The gauge cannot reach 100 on
    /// the ratio — the budget is an expectation, not a total anybody knows
    /// mid-stream — so the finish is a fact delivered from outside: the
    /// summary's own complete arrival, which snaps the bar full.
    pub done: bool,
}

impl Working {
    /// The one line this draws to, in the strip's own paint — or, while a
    /// compaction runs, in the compacting dress's.
    fn line(&self, theme: &Theme) -> Line<'static> {
        // One reading of the clock for the glyph, the figure, the band and
        // the pulse.
        let elapsed = self.started.elapsed();
        if let Some(compaction) = self.compaction {
            return compacting_line(elapsed, compaction, theme);
        }
        let verbs = u64::try_from(WORKING_VERBS.len()).unwrap_or(1);
        let verb = WORKING_VERBS[usize::try_from(self.turn % verbs).unwrap_or(0)];
        let mut text = format!(
            "{glyph} {verb}\u{2026} ({seconds}s",
            glyph = working_frame(elapsed),
            seconds = elapsed.as_secs()
        );
        if self.output_tokens > 0 {
            text.push_str(&format!(
                " \u{b7} \u{2193} {tokens} tokens",
                tokens = self.output_tokens
            ));
        }
        text.push(')');

        shimmer(text, elapsed)
    }
}

/// The working line's own paint — orange, deliberately not a theme slot: the
/// screenshot that pinned it (2026-08-15, Claude Code's own shimmer as the
/// reference) named the color, not a role.
const SHIMMER_BASE: (u8, u8, u8) = (0xe0, 0x80, 0x30);

/// What the band brightens toward at its center.
const SHIMMER_PEAK: (u8, u8, u8) = (0xff, 0xe4, 0xb4);

/// Columns the band reaches to either side of its center.
const SHIMMER_RADIUS: u64 = 3;

/// Milliseconds the band takes to advance one column.
const SHIMMER_STEP: u128 = 45;

/// Lays `text` out in orange with a brighter band sweeping left to right.
///
/// Time-driven off the same elapsed clock as the figure inside the text, so
/// the band advances on exactly the frames [`crate::app::App`] already
/// redraws for a running turn: nothing here keeps a phase of its own, and the
/// same instant read twice draws the same line twice.
fn shimmer(text: String, elapsed: Duration) -> Line<'static> {
    let characters: Vec<char> = text.chars().collect();
    let count = u64::try_from(characters.len()).unwrap_or(u64::MAX);
    // The band walks in from before the first column and all the way out past
    // the last, so the sweep reads as a pass rather than a wrap.
    let cycle = count + SHIMMER_RADIUS * 2 + 1;
    let center = u64::try_from(elapsed.as_millis() / SHIMMER_STEP).unwrap_or(u64::MAX) % cycle;

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, character) in characters.into_iter().enumerate() {
        let column = u64::try_from(index).unwrap_or(u64::MAX) + SHIMMER_RADIUS;
        let reach = SHIMMER_RADIUS.saturating_sub(center.abs_diff(column));
        let style = Style::default().fg(blend(SHIMMER_BASE, SHIMMER_PEAK, reach, SHIMMER_RADIUS));
        match spans.last_mut() {
            Some(span) if span.style == style => span.content.to_mut().push(character),
            _ => spans.push(Span::styled(character.to_string(), style)),
        }
    }

    Line::from(spans)
}

/// The color `numerator / denominator` of the way from `base` to `peak`.
fn blend(base: (u8, u8, u8), peak: (u8, u8, u8), numerator: u64, denominator: u64) -> Color {
    let channel = |base: u8, peak: u8| {
        let (base, peak) = (u64::from(base), u64::from(peak));
        let mixed = if peak >= base {
            base + (peak - base) * numerator / denominator
        } else {
            base - (base - peak) * numerator / denominator
        };

        u8::try_from(mixed).unwrap_or(u8::MAX)
    };

    Color::Rgb(
        channel(base.0, peak.0),
        channel(base.1, peak.1),
        channel(base.2, peak.2),
    )
}

/// The compacting headline's paint at the spinner cycle's ends — a pale
/// blue, sampled off the reference screenshots (2026-08-25). A named color
/// rather than a theme slot, exactly as the shimmer's orange is: the
/// reference named colors, not roles.
const COMPACT_BLUE: (u8, u8, u8) = (0xb7, 0xd6, 0xfb);

/// What the pulse reaches at the middle of the cycle: the second frame's
/// periwinkle.
const COMPACT_PERIWINKLE: (u8, u8, u8) = (0xaf, 0xaf, 0xf9);

/// Segments the compacting gauge holds at full width, counted off the
/// reference screenshot: twenty-one filled and nineteen outlined under 52%.
const COMPACT_BAR: usize = 40;

/// How long the full gauge is held after a compacting turn settles, before
/// the strip is taken back. Long enough for an eye to register the arrival,
/// short enough that the screen is not claiming work after the work.
const COMPACT_SETTLE: Duration = Duration::from_millis(1_000);

/// The headline's paint `elapsed` into the compaction: out toward periwinkle
/// over the spinner cycle's first half and back over the second, riding
/// `WORKING_FRAME_STEP` so the color moves exactly when the glyph does — the
/// icon and the color changing together, as the reference's two frames show.
fn compact_pulse(elapsed: Duration) -> Color {
    let steps = u64::try_from(elapsed.as_millis() / WORKING_FRAME_STEP).unwrap_or(u64::MAX);
    let frames = u64::try_from(WORKING_FRAMES.len()).unwrap_or(2).max(2);
    let half = frames / 2;
    let toward = half.saturating_sub((steps % frames).abs_diff(half));
    blend(COMPACT_BLUE, COMPACT_PERIWINKLE, toward, half)
}

/// `59s` up to a minute, `2m 1s` past it — the reference's own spelling of a
/// clock that a compaction, unlike most turns, actually runs into minutes.
fn compact_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!(
            "{minutes}m {seconds}s",
            minutes = seconds / 60,
            seconds = seconds % 60
        )
    }
}

/// `840` below a thousand, `2.5k` past it (`4k` when the tenth is zero): the
/// reference abbreviates, and the raw figure would crowd a narrow strip.
fn compact_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    let tenths = tokens / 100;
    if tenths.is_multiple_of(10) {
        format!("{}k", tenths / 10)
    } else {
        format!("{}.{}k", tenths / 10, tenths % 10)
    }
}

/// The strip's line while a compaction runs: the spinner glyph and the
/// headline in the pulse's paint, the clock and the streamed estimate
/// receding beside them. The token figure is the gauge's own — the summary
/// streamed so far — not the session total the ordinary line shows, because
/// what is streaming is the summary and nothing else.
fn compacting_line(elapsed: Duration, compaction: Compaction, theme: &Theme) -> Line<'static> {
    let head = format!(
        "{glyph} Compacting conversation\u{2026} ",
        glyph = working_frame(elapsed)
    );
    let mut tail = format!("({clock}", clock = compact_elapsed(elapsed));
    if compaction.tokens > 0 {
        tail.push_str(&format!(
            " \u{b7} \u{2193} {tokens} tokens",
            tokens = compact_tokens(compaction.tokens)
        ));
    }
    tail.push(')');

    Line::from(vec![
        Span::styled(
            head,
            Style::default()
                .fg(compact_pulse(elapsed))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(tail, theme.dim),
    ])
}

/// The gauge under the compacting line: filled segments bright, the rest
/// outlined, the percentage beside them — clamped at 99 while the summary is
/// still streaming, because the budget is an expectation the stream may
/// outrun, and a bar claiming the end of work still under way would be the
/// strip's one lie.
fn compacting_bar(compaction: Compaction, width: usize, theme: &Theme) -> Option<Line<'static>> {
    // The indent, then the label at its widest: "  " and " 100%".
    let segments = COMPACT_BAR.min(width.saturating_sub(7));
    if segments == 0 {
        return None;
    }
    let percent = if compaction.done {
        100
    } else {
        usize::try_from(compaction.tokens.saturating_mul(100) / compaction.budget.max(1))
            .unwrap_or(usize::MAX)
            .min(99)
    };
    let filled = segments * percent / 100;

    Some(Line::from(vec![
        Span::styled(format!("  {}", "\u{25b0}".repeat(filled)), theme.fg),
        Span::styled("\u{25b1}".repeat(segments - filled), theme.dim),
        Span::styled(format!(" {percent}%"), theme.dim),
    ]))
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
    /// The prefix's own paint, where it differs from the body's: a settled
    /// call's bullet answers "did it work" — green for yes, red for no —
    /// while the heading beside it stays prose (the 2026-08-15 screenshots,
    /// matching Claude Code's own dots).
    lead: Option<Style>,
    text: String,
    style: Style,
}

impl Row {
    fn new(prefix: &str, text: impl Into<String>, style: Style) -> Self {
        Self {
            prefix: prefix.to_owned(),
            lead: None,
            text: text.into(),
            style,
        }
    }

    /// A row whose prefix is painted `lead` while its text keeps `style`.
    fn led(prefix: &str, lead: Style, text: impl Into<String>, style: Style) -> Self {
        Self {
            lead: Some(lead),
            ..Self::new(prefix, text, style)
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

        // Decoration never reaches the margin: a struck todo strikes its
        // words, not the elbow and the blank columns leading up to them —
        // ink drawn across an indent reads as a rule floating out to the
        // left of the row (2026-08-15).
        let margin = row
            .lead
            .unwrap_or_else(|| row.style.remove_modifier(Modifier::CROSSED_OUT));

        for (index, line) in wrap(&row.text, body).into_iter().enumerate() {
            // A blank line inside a block stays blank — a row of spaces is an
            // indent nobody can see, and one the backtrack highlight would
            // have to treat as content.
            let line = match (index, line.is_empty(), margin == row.style) {
                (0, _, false) => Line::from(vec![
                    Span::styled(row.prefix.clone(), margin),
                    Span::styled(line, row.style),
                ]),
                (0, _, true) => {
                    Line::styled(format!("{prefix}{line}", prefix = row.prefix), row.style)
                }
                (_, true, _) => Line::styled(String::new(), row.style),
                (_, false, false) => Line::from(vec![
                    Span::styled(hang.clone(), margin),
                    Span::styled(line, row.style),
                ]),
                (_, false, true) => Line::styled(format!("{hang}{line}"), row.style),
            };
            lines.push(line);
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
    /// The attached-image paths this wrap wanted placeholder cells for and
    /// did not have yet, empty once every image is answered (2026-08-15).
    images: Vec<(usize, String)>,
    /// The pulse phase these lines were drawn on, [`None`] for an entry with
    /// no call in flight: what lets the point move without every settled
    /// entry losing its cache.
    blink: Option<bool>,
}

/// Rows an attached image's reserved box stands in the transcript.
pub const IMAGE_ROWS: u16 = 5;

impl Chat {
    /// Turns pixel drawing on, set once at startup by the frontend that
    /// detected a kitty ancestor: attached images reserve boxes instead of
    /// spelling their paths (2026-08-15).
    pub fn set_graphics(&mut self, graphics: bool) {
        self.graphics = graphics;
    }

    /// The image paths the last render wanted placeholder cells for and did
    /// not have — what the app loads, transmits, and answers through
    /// [`Chat::set_image_cell`].
    #[must_use]
    pub fn images_wanting_cells(&self) -> &[String] {
        &self.image_wanted
    }

    /// Answers a wanted image: the id its pixels were transmitted under and
    /// the columns its box stands — or the zero id for a file that would not
    /// decode, whose box stays honestly blank. Every entry rewraps, because
    /// any of them may hold the same path.
    pub fn set_image_cell(&mut self, path: &str, id: u32, columns: u16) {
        self.image_cells.insert(path.to_owned(), (id, columns));
        for entry in &mut self.entries {
            entry.wrapped = None;
        }
    }

    /// Appends `message` and returns to following the tail.
    pub fn start_message(&mut self, message: Message) {
        self.push(message, false);
    }

    /// Appends a message read back from a resumed session's store.
    ///
    /// The same append a live `MessageStarted` performs, plus the one thing a
    /// stored message can say that a live one cannot: an assistant message the
    /// store never saw finish was cut off by a crash. Both routes end in
    /// `Chat::push`, so a resumed transcript and a streamed one are the same
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
        self.settling = None;
        self.working = working;
        if working.is_none() {
            self.working_lines.clear();
        }
    }

    /// Which phase of the in-flight pulse this instant falls on: `true` is
    /// the bright half. Time-driven like `working_frame`, so the same
    /// instant read twice draws the same frame twice.
    fn blink_on(&mut self) -> bool {
        let epoch = *self.blink_epoch.get_or_insert_with(Instant::now);
        (epoch.elapsed().as_millis() / POINT_BLINK.as_millis()).is_multiple_of(2)
    }

    /// Tells the strip a compaction is running and how far its summary has
    /// streamed.
    ///
    /// Arms the strip when nothing else has — the automatic trigger fires at
    /// a turn's start, before any message opens — and updates in place when
    /// something has, so the clock stays the compaction's own from its first
    /// event rather than restarting with every report.
    pub fn set_compacting(&mut self, tokens: u64, budget: u64) {
        self.settling = None;
        let compaction = Some(Compaction {
            tokens,
            budget,
            done: false,
        });
        match &mut self.working {
            Some(working) => working.compaction = compaction,
            None => {
                self.working = Some(Working {
                    started: Instant::now(),
                    // The verb never draws under the compacting dress, so
                    // the rotation has nothing to say here.
                    turn: 0,
                    output_tokens: 0,
                    compaction,
                });
            }
        }
    }

    /// Marks the running compaction finished — the summary arrived whole —
    /// so the gauge snaps full. Answers whether there was one to finish,
    /// which is how the app tells the summary's arrival from an ordinary
    /// reply opening.
    pub fn finish_compacting(&mut self) -> bool {
        match &mut self.working {
            Some(Working {
                compaction: Some(compaction),
                ..
            }) => {
                compaction.done = true;
                true
            }
            _ => false,
        }
    }

    /// Takes the strip back at a turn's end — except for a finished
    /// compaction, whose full gauge is held for `COMPACT_SETTLE` first so
    /// the 100% is a thing a person saw rather than a frame that never
    /// rendered. A compaction the turn's end caught *unfinished* — a cancel,
    /// a dead provider — clears immediately: there is no arrival to show.
    pub fn settle_working(&mut self) {
        match self.working {
            Some(Working {
                compaction: Some(Compaction { done: true, .. }),
                ..
            }) => self.settling = Some(Instant::now()),
            _ => self.set_working(None),
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
    #[cfg(test)]
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
        let graphics = self.graphics;
        let blink = self.blink_on();
        for entry in &mut self.entries[..first_hidden] {
            entry.wrap(area.width, theme, graphics, &self.image_cells, blink);
        }
        if let Some(revert) = &mut self.revert {
            revert.wrap(hidden, area.width, theme);
        }
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

        // The image paths this frame's wraps wanted cells for and did not
        // have — deduped, for the app's post-frame load-and-transmit
        // (2026-08-15).
        let mut wanted: Vec<String> = Vec::new();
        for entry in self.shown() {
            for (_, path) in entry.images() {
                if !wanted.contains(path) {
                    wanted.push(path.clone());
                }
            }
        }
        self.image_wanted = wanted;

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

    /// The checklist the working line carries: the newest settled `todowrite`
    /// of the turn that is running, or nothing when this turn has written no
    /// list (**D487**).
    ///
    /// Bounded to the current turn by walking back only as far as the prompt
    /// that started it — a plan the previous turn wrote is not what this one is
    /// working through, and a stale list under a live clock is the one reading
    /// that would be false. Settled calls only, so the rows under the working
    /// line and the rows on the call's own transcript row are the same rows
    /// from the same source.
    fn working_todos(&self, theme: &Theme) -> Vec<(String, Style)> {
        self.shown()
            .iter()
            .rev()
            .take_while(|entry| entry.role != Role::User)
            .flat_map(|entry| entry.parts.iter().rev())
            .find_map(|part| match &part.body {
                PartBody::Tool {
                    tool,
                    state: ToolState::Completed { input, .. },
                    ..
                } if tool == TODO_TOOL => todo_rows(input, theme),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Lays this frame's working block out at `width` and says how tall it
    /// is: the working line, and under it this turn's checklist while the
    /// turn has one.
    ///
    /// The block is **pinned above the composer** rather than ridden at the
    /// transcript's tail (the 2026-08-15 screenshots, pinning Claude Code's
    /// own arrangement over **D487**'s seam): what a turn is doing now is a
    /// status, not a message, and a status that scrolls with the history is
    /// lost exactly when the history gets long. The app calls this before its
    /// vertical split so the strip is sized on this frame's lines, then
    /// [`Chat::render_working`] draws what was built here.
    pub fn lay_out_working(&mut self, width: u16, theme: &Theme) -> u16 {
        // A held gauge expires here rather than on an event: the frames a
        // settled screen still draws — the tick-driven ones — are what carry
        // the clock past the hold.
        if self
            .settling
            .is_some_and(|settled| settled.elapsed() >= COMPACT_SETTLE)
        {
            self.set_working(None);
        }
        let lines = match self.working {
            Some(working) => {
                let mut lines = vec![working.line(theme)];
                if let Some(compaction) = working.compaction {
                    // The reference puts a blank row of air between the line
                    // and its gauge, and the gauge where the checklist would
                    // stand — a compaction runs no tools, so there is no
                    // checklist for it to displace.
                    lines.push(Line::styled(String::new(), Style::default()));
                    lines.extend(compacting_bar(compaction, usize::from(width), theme));
                } else {
                    let todos = result_rows(self.working_todos(theme), false);
                    lines.extend(lay_out(&todos, usize::from(width)));
                }

                lines
            }
            None => Vec::new(),
        };
        self.working_lines = lines;

        u16::try_from(self.working_lines.len()).unwrap_or(u16::MAX)
    }

    /// Draws what [`Chat::lay_out_working`] built into `area`, top-aligned so
    /// a strip the terminal cut short keeps the working line itself.
    pub fn render_working(&self, area: Rect, buffer: &mut Buffer) {
        for (row, line) in self.working_lines.iter().enumerate() {
            let Ok(row) = u16::try_from(row) else {
                break;
            };
            if row >= area.height {
                break;
            }
            buffer.set_line(area.x, area.y + row, line, area.width);
        }
    }

    /// Yields the lines the viewport shows, skipping whole entries rather than
    /// stepping over every line above the offset.
    ///
    /// The marker row rides along as one more block at the end, which is where
    /// it belongs: the entries a revert hides are always the tail of the
    /// transcript, so what it stands in for is always below everything shown.
    /// The working line used to ride the same seam and no longer does: what a
    /// turn is doing now lives outside the scroll entirely, in the strip
    /// [`Chat::lay_out_working`] builds (**D487**, amended 2026-08-15).
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
            images: Vec::new(),
            blink: None,
        });
    }
}

impl Entry {
    fn lines(&self) -> &[Line<'static>] {
        self.wrapped
            .as_ref()
            .map_or(&[], |wrapped| wrapped.lines.as_slice())
    }

    /// The attached images the last wrap reserved boxes for.
    fn images(&self) -> &[(usize, String)] {
        self.wrapped
            .as_ref()
            .map_or(&[], |wrapped| wrapped.images.as_slice())
    }

    fn wrap(
        &mut self,
        width: u16,
        theme: &Theme,
        graphics: bool,
        cells: &HashMap<String, (u32, u16)>,
        blink: bool,
    ) {
        // An entry holding a call still in flight keys its cache on the
        // pulse phase too, so the point can move without anything else
        // being rebuilt — and every settled entry stays as cacheable as it
        // always was.
        let animated = self.parts.iter().any(in_flight);
        if self.wrapped.as_ref().is_some_and(|wrapped| {
            wrapped.width == width
                && wrapped.revision == theme.revision()
                && wrapped.blink.is_none_or(|drawn| drawn == blink)
        }) {
            return;
        }

        let columns = usize::from(width);
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut images: Vec<(usize, String)> = Vec::new();
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
                    lines.extend(lay_out(&tool_lines(tool, state, theme, blink), columns));
                }
                // A tool the provider ran on its own side (**D489**), drawn in
                // the same grammar a local call is: the question a person is
                // asking — *what did it do, and what came back* — is the same
                // one, and a second presentation for it would be a second
                // thing to learn. The state is synthesized rather than stored
                // because the row is finished when it arrives: the gateway
                // reports no timings, and the grammar shows none for a settled
                // call anyway.
                PartBody::ServerTool {
                    tool,
                    input,
                    output,
                } => {
                    let state = ToolState::Completed {
                        input: input.clone(),
                        output: output.clone(),
                        // The vendor names its work in neither field, so the
                        // marker leads straight to the preview rather than to
                        // a summary line this side made up.
                        title: String::new(),
                        metadata: serde_json::Value::Object(serde_json::Map::new()),
                        started: 0,
                        completed: 0,
                    };
                    lines.extend(lay_out(&tool_lines(tool, &state, theme, blink), columns));
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
                    // An attached image the terminal can draw is drawn
                    // (2026-08-15): the token-and-mime row gives way to rows
                    // of kitty Unicode placeholder cells the terminal
                    // composites the picture over — cells rather than a
                    // positioned placement, because cells scroll, clip and
                    // survive tmux exactly as text does, where the first
                    // cut's cursor-move placements landed on whatever row a
                    // multiplexer's redraw left the real cursor on. Every
                    // other file, and every image on a pixel-less terminal,
                    // keeps the token row.
                    if graphics && mime.starts_with("image/") {
                        match cells.get(path) {
                            Some(&(id, columns)) if id != 0 => {
                                let style = Style::default().fg(graphics::id_color(id));
                                for row in 0..IMAGE_ROWS {
                                    let mut text = String::from("  ");
                                    for column in 0..columns {
                                        text.push_str(&graphics::placeholder(row, column));
                                    }
                                    lines.push(Line::styled(text, style));
                                }
                            }
                            // The zero id is a file that would not decode:
                            // the box stays honestly blank, forever.
                            Some(_) => lines.extend(std::iter::repeat_n(
                                Line::default(),
                                usize::from(IMAGE_ROWS),
                            )),
                            None => {
                                images.push((lines.len(), path.clone()));
                                lines.extend(std::iter::repeat_n(
                                    Line::default(),
                                    usize::from(IMAGE_ROWS),
                                ));
                            }
                        }
                        continue;
                    }
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
                // the way to. Rendered **whole**, paragraph breaks included —
                // the shape the user's screenshot pinned (2026-08-14,
                // retiring the plan's pre-mortem-3 tail clamp): a long think
                // scrolls back the way a long reply does, and hiding the
                // start of a thought was the one cut the clamp could make.
                PartBody::ReasoningText { text } if !text.is_empty() => {
                    let style = theme.dim.add_modifier(Modifier::ITALIC);
                    let hang = " ".repeat(THINKING.width());
                    let rows: Vec<Row> = text
                        .lines()
                        .enumerate()
                        .map(|(index, line)| {
                            let lead = if index == 0 { THINKING } else { hang.as_str() };
                            Row::new(lead, line.to_owned(), style)
                        })
                        .collect();
                    lines.extend(lay_out(&rows, columns));
                }
                // An empty one is a part the provider opened and has not
                // filled yet; a marker alone would be a claim about nothing.
                PartBody::ReasoningText { .. } => {}
                // What a teammate said (**D495**), under a head of its own
                // rather than the caret or the bullet: `@ <name>\u{276f}`
                // opens the block — Claude Code's own dress for a teammate's
                // words — painted `info` so the sender reads apart from what
                // the person typed and from the reply around it without
                // borrowing either marker's claim (a `>` would say a person
                // said this, a bullet that the model did). The sender's
                // one-line summary follows the head where it wrote one,
                // dimmed as chrome, with what it said hanging under the head
                // in prose, the same split a call's header and its result
                // use. The member's assigned `color` is deliberately unread:
                // a palette this pane never mixed is not one it can trust
                // against an arbitrary theme.
                PartBody::Peer {
                    from,
                    summary,
                    body,
                    ..
                } => {
                    let head = format!("@ {from}\u{276f}");
                    // `display_summary` owns the blank-dropped, capped
                    // projection this renderer shares with the engine's
                    // envelope and the copy formatter.
                    let mut rows = vec![match team::display_summary(summary.as_deref()) {
                        Some(line) => Row::led(&format!("{head} "), theme.info, line, theme.dim),
                        None => Row::led(&head, theme.info, String::new(), theme.dim),
                    }];
                    // Two columns of hang, not the head's own width: the body
                    // is prose, and prose pushed past a name-sized margin
                    // reads as a quotation rather than as what the block says.
                    rows.extend(
                        body.lines()
                            .map(|line| Row::new("  ", line.to_owned(), theme.fg)),
                    );
                    lines.extend(lay_out(&rows, columns));
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
            images,
            blink: animated.then_some(blink),
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

/// The tool whose result is a count rather than a preview (**D487**, pinned by
/// the user's screenshots).
///
/// Every other settled call shows some of what it produced. A read shows how
/// much it took and nothing else — how many lines of a file, or how many
/// entries of a directory (2026-08-14, the second screenshot: a listing spilled
/// its envelope onto the screen exactly as a file read used to): the content is
/// what the *model* asked for, and a person reading the transcript does not need
/// it read back to them through a four-line window — least of all wrapped in the
/// envelope the tool writes for the model's benefit. The whole of it is one
/// Ctrl+T away, which is where a reader who wants it goes.
const READ_TOOL: &str = "read";

/// The tool whose result is its own list, drawn as a checklist (**D487**,
/// pinned by the user's screenshot 2026-08-14).
///
/// A todo list is the one tool output that is *for the person*: the model
/// already knows what it wrote, and what a reader wants is the plan, not the
/// JSON the plan travelled in. It is short by construction — the tool's own
/// prompt keeps it so — which is why it is the one preview here that is never
/// clamped.
const TODO_TOOL: &str = "todowrite";

/// What leads a task still to be done, and one that will not be done again.
///
/// The screenshot shows only the open box, so what the other states look like is
/// this build's reading of it: `in_progress` keeps the open box — the task is
/// still open — and is painted in the accent so the one row being worked on can
/// be found at a glance, while `completed` and `cancelled` share the crossed box
/// dimmed and struck through, because both are rows a reader is done with.
const TODO_OPEN: char = '\u{2610}';

/// See [`TODO_OPEN`].
const TODO_DONE: char = '\u{2612}';

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

/// Whether a part is a tool call still in flight — what the transcript
/// pulses, and what makes its entry's wrap re-key on the pulse phase.
fn in_flight(part: &Part) -> bool {
    matches!(
        &part.body,
        PartBody::Tool {
            state: ToolState::Pending { .. } | ToolState::Running { .. },
            ..
        }
    )
}

/// The in-flight point's paint: bright on the pulse's on-phase, the chrome's
/// own dim off it.
fn point_style(theme: &Theme, on: bool) -> Style {
    if on { theme.fg } else { theme.dim }
}

/// The one line a call is announced on: the tool, and what it was called with.
///
/// The name is title-cased the way the screenshot draws it and the way the
/// task row already draws an agent's; the id itself is unchanged everywhere it
/// is a name rather than a heading.
fn tool_heading(tool: &str, input: Option<&serde_json::Value>) -> String {
    let name = titlecase(tool);
    let args = match tool {
        READ_TOOL => input.and_then(read_args),
        _ => input.and_then(derive_args),
    };

    match args {
        Some(args) => format!("{name}({args})"),
        None => name,
    }
}

/// A read call's arguments as the screenshot draws them: the path bare rather
/// than as a `key: "value"` pair, and the lines it asked for beside it.
///
/// Read off the **input** and never off a settled call's metadata, which is
/// what keeps a running read and the same read once it lands on one line: the
/// header is a statement about what was asked, and what was asked does not
/// change when the answer arrives.
///
/// The range wants both ends to be one. An `offset` with no `limit` is a read
/// from a line to wherever the file stops, which nothing here can name before
/// the file is read and which is not a range in the sense the row means.
fn read_args(input: &serde_json::Value) -> Option<String> {
    let path = field(input, "filePath")?;
    let offset = input.get("offset").and_then(serde_json::Value::as_u64);
    let limit = input
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .filter(|limit| *limit > 0);

    match (offset, limit) {
        (Some(offset), Some(limit)) => Some(format!(
            "{path} \u{b7} lines {offset}-{last}",
            last = offset.saturating_add(limit).saturating_sub(1)
        )),
        _ => Some(path.to_owned()),
    }
}

/// What a settled read answered with, in the one line the row shows: how much
/// of a file it took, or how many entries it listed.
///
/// Read off the call's own `display` block rather than scraped out of the
/// envelope the tool writes (`ganja_tool::read`, whose file branch publishes
/// `type`/`lineStart`/`lineEnd` and whose directory branch publishes
/// `type`/`entries`/`totalEntries`). The block is the same fact in a shape that
/// cannot be miscounted: the envelope's text is clamped to a byte budget
/// before it ships, so a large listing can lose its own `</entries>` close,
/// where the metadata beside it is untouched.
///
/// The directory count is of the entries this call **listed**, not of the
/// entries the directory holds — the same reading the file branch takes, where
/// the count is of the lines read and not of the file's length. What was left
/// out is in the envelope, one Ctrl+T away.
///
/// [`None`] for a read that is neither — a PDF, an image — each of which
/// carries no `display` block at all and keeps the rendering every other tool's
/// result has, because neither a line count nor an entry count is what it did.
fn read_summary(metadata: &serde_json::Value) -> Option<String> {
    let display = metadata.get("display")?;

    match field(display, "type")? {
        "file" => {
            let start = display
                .get("lineStart")
                .and_then(serde_json::Value::as_u64)?;
            let end = display.get("lineEnd").and_then(serde_json::Value::as_u64)?;
            // An empty file reports an end before its start, and read nothing.
            let read = if end < start { 0 } else { end - start + 1 };

            Some(format!(
                "Read {read} line{plural}",
                plural = if read == 1 { "" } else { "s" }
            ))
        }
        "directory" => {
            let entries = display
                .get("entries")
                .and_then(serde_json::Value::as_array)?;

            Some(format!(
                "Listed {count} entr{plural}",
                count = entries.len(),
                plural = if entries.len() == 1 { "y" } else { "ies" }
            ))
        }
        _ => None,
    }
}

/// The checklist a `todowrite` call draws: one row per task, its box telling
/// where the task has got to (**D487**).
///
/// Read off the call's **input** rather than its output, for the reason
/// [`read_args`] reads the input: the list is structured there — `todos` of
/// `{content, status, priority}`, `ganja_tool::todo`'s own shape — where the
/// output is that same list re-serialized as JSON for the model to read back.
/// The tool republishes it in its metadata too, but only on a call that
/// settled, and the input is the one copy every state carries.
///
/// [`None`] when the argument is not a list this can draw — a list that is
/// empty, an element without content, an input that is not the tool's at all —
/// and then the call keeps the preview every other tool's result has, which at
/// least shows what really arrived. A checklist with no rows would be a `⎿`
/// pointing at nothing.
fn todo_rows(input: &serde_json::Value, theme: &Theme) -> Option<Vec<(String, Style)>> {
    let todos = input
        .get("todos")?
        .as_array()
        .filter(|todos| !todos.is_empty())?;

    todos
        .iter()
        .map(|todo| {
            let content = field(todo, "content")?;
            // An unfamiliar status is a task nobody has said is finished, so it
            // draws as one still open rather than as an error the row cannot
            // show anyway.
            let (box_glyph, style) = match todo.get("status").and_then(serde_json::Value::as_str) {
                Some("completed" | "cancelled") => {
                    (TODO_DONE, theme.dim.add_modifier(Modifier::CROSSED_OUT))
                }
                Some("in_progress") => (TODO_OPEN, theme.accent.add_modifier(Modifier::BOLD)),
                _ => (TODO_OPEN, theme.fg),
            };

            Some((format!("{box_glyph} {content}"), style))
        })
        .collect()
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
/// Every state shares the header's **words** — `Tool(args)`, the same text
/// before and after the call settles — and differs in the lead it wears and
/// the color it is painted, and in what hangs under it: a running call's newest output, a
/// finished call's summary and preview, a failed call's first line of why
/// (**D487**). `StepStart`/`StepFinish` never reach here — and a call still
/// in flight leads with the pulsing `POINT` on `blink`'s phase rather than
/// the bullet, the words unmoved (2026-08-25).
fn tool_lines(tool: &str, state: &ToolState, theme: &Theme, blink: bool) -> Vec<Row> {
    // A delegated turn is one row, never a transcript of its own: everything
    // the child said reaches the model inside the tool result, and repeating
    // it here would show the same work twice.
    if tool == TASK_TOOL && !matches!(state, ToolState::Error { .. }) {
        return task_lines(state, theme, blink);
    }

    match state {
        // A call whose turn has not come yet still says what it will do, once
        // the stream has finished saying so (2026-08-15): the settled
        // arguments ride the pending state, and a bare name means they are
        // still streaming.
        ToolState::Pending { input } => vec![Row::led(
            POINT,
            point_style(theme, blink),
            tool_heading(tool, input.as_ref()),
            theme.dim,
        )],
        ToolState::Running {
            input, metadata, ..
        } => {
            let mut rows = vec![Row::led(
                POINT,
                point_style(theme, blink),
                tool_heading(tool, Some(input)),
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
            // The bullet alone answers "did it work" (2026-08-15): green
            // here, red on a failed call, while the heading stays prose.
            let mut rows = vec![Row::led(
                BULLET,
                theme.success,
                tool_heading(tool, Some(input)),
                theme.fg,
            )];
            // A read answers with a count and stops there; see [`READ_TOOL`].
            if tool == READ_TOOL
                && let Some(summary) = read_summary(metadata)
            {
                rows.push(Row::new(RESULT, summary, theme.dim));

                return rows;
            }
            // A todo list answers with the list; see [`TODO_TOOL`]. The tool's
            // own `N todos` title goes with the JSON it titled — the rows say
            // how much is left, and say it in the words the model wrote.
            if tool == TODO_TOOL
                && let Some(todos) = todo_rows(input, theme)
            {
                rows.extend(result_rows(todos, false));

                return rows;
            }

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
            let mut rows = vec![Row::led(
                BULLET,
                theme.error,
                tool_heading(tool, Some(input)),
                theme.fg,
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
fn task_lines(state: &ToolState, theme: &Theme, blink: bool) -> Vec<Row> {
    match state {
        ToolState::Pending { input } => {
            let heading = input.as_ref().map_or_else(
                || titlecase(TASK_TOOL),
                |input| task_heading(field(input, "subagent_type"), field(input, "description")),
            );

            vec![Row::led(
                POINT,
                point_style(theme, blink),
                heading,
                theme.dim,
            )]
        }
        ToolState::Running {
            input, metadata, ..
        } => {
            let agent = field(input, "subagent_type");
            let mut rows = vec![Row::led(
                POINT,
                point_style(theme, blink),
                task_heading(agent, field(input, "description")),
                theme.dim,
            )];
            let log = call_log(metadata);
            if log.is_empty() {
                // Upstream's own priority: the tool the child is running right
                // now says more than how many it has run.
                let detail = match field(metadata, "current_tool") {
                    Some(current) => current.to_owned(),
                    None => format!("{} toolcalls", toolcalls(metadata)),
                };
                rows.push(Row::new(RESULT, detail, theme.dim));
            } else {
                // What runs inside the task, expanded on the row itself
                // (2026-08-15): the newest calls in call order — the streaming
                // shell's posture, what was cut is above what is shown — with
                // the cut priced off the true total, so the collapsed rows the
                // engine's own cap dropped are admitted too.
                let start = log.len().saturating_sub(TOOL_PREVIEW_LINES);
                let total = usize::try_from(toolcalls(metadata)).unwrap_or(usize::MAX);
                let hidden = total.saturating_sub(log.len() - start);
                let mut preview: Vec<(String, Style)> = Vec::new();
                if hidden > 0 {
                    preview.push((clamp_hint(hidden), theme.dim));
                }
                preview.extend(log.into_iter().skip(start).map(|call| (call, theme.dim)));
                rows.extend(result_rows(preview, false));
            }

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
                Row::led(
                    BULLET,
                    theme.success,
                    task_heading(agent, description),
                    theme.fg,
                ),
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

/// The child's own calls, as the watcher logged them onto the parent's part —
/// empty for a part that carries no log, which is also every foreign tool's.
fn call_log(metadata: &serde_json::Value) -> Vec<String> {
    metadata
        .get("calls")
        .and_then(serde_json::Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
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
/// consuming at least one grapheme cluster so callers cannot loop forever.
///
/// The boundaries are **clusters, not `char`s**: a `char` walk cut a ZWJ emoji
/// or a combining sequence apart mid-glyph, and neither half of that cut is
/// something a terminal can draw back as what the text meant. Each cluster is
/// measured whole for the same reason the rest of this module measures whole
/// strings — [`unicode_width`] reads a fully-qualified ZWJ sequence as the one
/// two-column glyph it renders as, which summing its characters would not.
pub(crate) fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0;

    for (index, cluster) in text.grapheme_indices(true) {
        let advance = cluster.width();
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

    use super::{
        BULLET, COMPACT_BLUE, COMPACT_PERIWINKLE, Chat, Compaction, Instant, RESULT,
        WORKING_FRAME_STEP, WORKING_FRAMES, WORKING_VERBS, Working, compact_elapsed, compact_pulse,
        compact_tokens, elapsed, split_at_width, working_frame, wrap,
    };
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

    /// A tool the gateway ran on its own side, drawn in the same grammar a
    /// local call is (**D489**): the marker, the name it came under, the
    /// arguments condensed onto that line, and the result under `⎿`.
    ///
    /// The name is deliberately the namespaced one — a row a reader could
    /// mistake for a call this machine made would be the one wrong thing to
    /// draw.
    #[test]
    fn a_provider_run_tool_draws_in_the_same_grammar_a_local_call_does() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::ServerTool {
                tool: "openrouter:web_search".to_owned(),
                input: serde_json::json!({"query": "rust 2024"}),
                output: "3 results".to_owned(),
            },
        });
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 80, 20));
        assert_eq!(
            &lines[..2],
            [
                // The leading capital is the grammar's own titlecase, which
                // every tool name on this pane gets — an `mcp__…` row reads
                // `Mcp__…` today. What matters is that the *namespace*
                // survives it: nobody should read this row as a call the
                // machine in front of them made.
                format!("{BULLET}Openrouter:web_search(query: \"rust 2024\")"),
                format!("{RESULT}3 results"),
            ],
            "got {lines:?}"
        );
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

    /// The working strip as [`crate::app::App`] composes it: laid out at
    /// `width`, drawn into exactly the rows it asked for.
    fn strip(chat: &mut Chat, width: u16) -> Vec<String> {
        let height = chat.lay_out_working(width, &Theme::default());
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        chat.render_working(area, &mut buffer);

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

    /// With pixels available, an attached image's row gives way to a blank
    /// box and the render asks for its cells; answered, the box fills with
    /// kitty placeholder cells carrying the id in their color — the picture
    /// in the transcript, not its path (2026-08-15). A text attachment
    /// keeps its token row beside it, and a decode failure leaves the box
    /// honestly blank forever.
    #[test]
    fn with_graphics_an_attached_image_asks_for_cells_and_then_draws_them() {
        let mut chat = Chat::default();
        chat.set_graphics(true);
        let mut message = Message::user("look");
        message.parts.push(Part {
            id: PartId::from("prt_f1".to_owned()),
            body: PartBody::File {
                path: "shot.png".to_owned(),
                mime: "image/png".to_owned(),
                start: None,
                end: None,
                content: None,
            },
        });
        message.parts.push(Part {
            id: PartId::from("prt_f2".to_owned()),
            body: PartBody::File {
                path: "src/lib.rs".to_owned(),
                mime: "text/plain".to_owned(),
                start: None,
                end: None,
                content: None,
            },
        });
        chat.start_message(message);

        let area = Rect::new(0, 0, 40, 12);
        let screen = rendered(&mut chat, area).join("\n");
        assert!(
            !screen.contains("shot.png"),
            "the image's path is off the screen:\n{screen}"
        );
        assert!(screen.contains("@src/lib.rs"), "{screen}");
        assert_eq!(
            chat.images_wanting_cells(),
            &["shot.png".to_owned()],
            "the render asks for the cells it does not have"
        );

        chat.set_image_cell("shot.png", 7, 3);
        let mut buffer = Buffer::empty(area);
        chat.render(area, &mut buffer, &Theme::default());
        let cell = &buffer[(2, 1)];
        assert!(
            cell.symbol().starts_with('\u{10EEEE}'),
            "the box holds placeholder cells, got {:?}",
            cell.symbol()
        );
        assert_eq!(
            cell.style().fg,
            Some(crate::graphics::id_color(7)),
            "and the id rides the color"
        );
        assert!(
            chat.images_wanting_cells().is_empty(),
            "an answered image is not asked for again"
        );

        chat.set_image_cell("shot.png", 0, 0);
        let mut blank = Buffer::empty(area);
        chat.render(area, &mut blank, &Theme::default());
        assert_eq!(
            blank[(2, 1)].symbol(),
            " ",
            "a decode failure keeps the box blank"
        );
        assert!(chat.images_wanting_cells().is_empty(), "and never re-asks");
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

    /// A wrap lands between grapheme clusters and never inside one. Both
    /// shapes here overflow a one-column budget on their first cluster, which
    /// is exactly where a `char` walk used to cut: after the family's leading
    /// emoji, and between the kana and the mark that voices it. Half a cluster
    /// is not a glyph any terminal can draw back.
    #[test]
    fn a_wrap_never_splits_a_zwj_family_or_a_combining_sequence() {
        // Four emoji joined by three ZERO WIDTH JOINERs — 25 bytes, one glyph.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
        // "か" plus the combining voiced sound mark that makes it "が".
        let voiced = "\u{304b}\u{3099}";

        assert_eq!(
            split_at_width(&format!("{family}x"), 1),
            (family, "x"),
            "the family is consumed whole"
        );
        assert_eq!(
            split_at_width(&format!("{voiced}x"), 1),
            (voiced, "x"),
            "the mark stays with the kana it voices"
        );
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

    /// The invariant stated where this loop begins: a prompt is one block
    /// however many parts it was built from. A peer part arriving beside what
    /// the person typed opens its own `@` head under the caret that part
    /// already drew, because two carets on one entry would claim two things
    /// were said.
    #[test]
    fn a_prompt_carrying_a_peers_words_draws_one_caret_for_the_whole_entry() {
        // Wider and taller than `VIEWPORT`: this entry is four rows and the
        // question is which glyph leads each of them, so none may scroll off.
        const AREA: Rect = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 12,
        };

        let mut chat = Chat::default();
        let prompt = Message::user("what did w1 say");
        chat.start_message(prompt.clone());
        chat.start_part(
            &prompt.id,
            Part::peer(
                "w1",
                Some("picked up W2".to_owned()),
                None,
                "on the protocol",
            ),
        );
        chat.start_part(&prompt.id, Part::peer("w2", None, None, "and I have it"));

        let lines = rendered(&mut chat, AREA);
        let carets = lines
            .iter()
            .filter(|line| line.starts_with("\u{3e} "))
            .count();

        assert_eq!(carets, 1, "one entry, one caret, got {lines:?}");
        assert!(
            lines.iter().any(|line| line == "\u{3e} what did w1 say"),
            "the caret leads what the person typed, got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "@ w1\u{276f} picked up W2")
                && lines.iter().any(|line| line == "@ w2\u{276f}"),
            "both peers head their own blocks under that caret, got {lines:?}"
        );
    }

    /// The same part on a reply is not something a person said, and not the
    /// reply's own words either: it takes the `@` head there exactly as it
    /// does on a prompt, claiming neither the caret nor the bullet.
    #[test]
    fn a_peers_words_on_a_reply_take_their_own_head_and_not_the_bullet() {
        let mut chat = Chat::default();
        let reply = Message::assistant("canned");
        chat.start_message(reply.clone());
        chat.start_part(&reply.id, Part::peer("w1", None, None, "relayed"));

        let lines = rendered(&mut chat, VIEWPORT);

        assert!(
            lines.iter().any(|line| line == "@ w1\u{276f}"),
            "a peer part on a reply heads its own block, got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.starts_with("\u{3e} ") && !line.starts_with("\u{25cf} ")),
            "neither the caret nor the bullet claims these words, got {lines:?}"
        );
    }

    /// **AC-7.** The whole of the teammate rendering in one frame: the
    /// sender's `@ name\u{276f}` head painted `info` at the top of its block
    /// with its own one-line summary dimmed beside it, what it said hanging
    /// under that head in body text, and one caret for the entry however many
    /// parts it arrived in.
    ///
    /// The dump is symbols only, the palette-independent shape this crate's
    /// snapshots use — so the two styles that carry the meaning here are
    /// asserted beside it rather than left to a theme change to break.
    ///
    /// It lives beside the pane it pins rather than in `app.rs`, because what
    /// is under test is one component's own drawing; it writes into the crate's
    /// one snapshot directory all the same.
    #[test]
    fn snapshot_teammate_message() {
        // Wide and tall enough for the whole entry: what this pins is which
        // glyph and which style leads each row, so no row may scroll off.
        const AREA: Rect = Rect {
            x: 0,
            y: 0,
            width: 46,
            height: 10,
        };

        let mut chat = Chat::default();
        let prompt = Message::user("what did w1 say");
        chat.start_message(prompt.clone());
        chat.start_part(
            &prompt.id,
            Part::peer(
                "w1",
                Some("picked up W2".to_owned()),
                None,
                "The protocol surface is mine.\nThe envelope is W6's.",
            ),
        );
        chat.start_part(&prompt.id, Part::peer("w2", None, None, "and I have it"));

        insta::with_settings!({snapshot_path => "../snapshots"}, {
            insta::assert_snapshot!(rendered(&mut chat, AREA).join("\n"));
        });

        let mut buffer = Buffer::empty(AREA);
        chat.render(AREA, &mut buffer, &Theme::default());
        let row_of = |needle: &str| {
            (0..AREA.height)
                .find(|row| {
                    (0..AREA.width)
                        .map(|column| buffer[(column, *row)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("the frame holds {needle:?}"))
        };
        let theme = Theme::default();
        assert_eq!(
            buffer[(0, row_of("@ w1\u{276f} picked up W2"))].style().fg,
            theme.info.fg,
            "the head that says whose words these are is painted info"
        );
        assert_eq!(
            buffer[(6, row_of("@ w1\u{276f} picked up W2"))].style().fg,
            theme.dim.fg,
            "and its one-line summary recedes beside it"
        );
        assert_eq!(
            buffer[(2, row_of("The protocol surface"))].style().fg,
            theme.fg.fg,
            "and what it said is body text under it"
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
            lines.iter().any(|line| line == "\u{2022} Shell"),
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
                .any(|line| line == "\u{2022} Shell(command: \"cargo test\")"),
            "got {lines:?}"
        );
    }

    /// An in-flight call's point pulses — bright on one phase, the chrome's
    /// own dim on the other — while its words hold still (user directive,
    /// 2026-08-25): the two frames differ in paint alone.
    #[test]
    fn an_in_flight_calls_point_pulses_and_its_words_hold_still() {
        let area = Rect::new(0, 0, 60, 4);
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

        let frame = |chat: &mut Chat| {
            let mut buffer = Buffer::empty(area);
            chat.render(area, &mut buffer, &Theme::default());
            let words: Vec<String> = (0..area.height)
                .map(|row| {
                    (0..area.width)
                        .map(|column| buffer[(column, row)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect();
            (words, buffer[(0, 0)].style().fg)
        };

        chat.blink_epoch = Some(Instant::now());
        let (bright, lead) = frame(&mut chat);
        assert_eq!(
            bright[0], "\u{2022} Shell(command: \"cargo test\")",
            "the point leads a call still in flight"
        );
        assert_eq!(lead, Theme::default().fg.fg, "bright on the on-phase");

        chat.blink_epoch = Instant::now().checked_sub(super::POINT_BLINK);
        let (dim, lead) = frame(&mut chat);
        assert_eq!(bright, dim, "the words hold still; only the paint moves");
        assert_eq!(lead, Theme::default().dim.fg, "the chrome's own dim off it");
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
                tool: "grep".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"pattern": "fn main"}),
                    output: "one\ntwo\nthree\nfour\nfive\nsix".to_owned(),
                    title: "6 matches".to_owned(),
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
                "\u{25cf} Grep(pattern: \"fn main\")",
                "  \u{23bf} 6 matches",
                "    one",
                "    two",
                "    three",
                "    four",
                "    \u{2026} +2 lines (ctrl+t to expand)",
            ],
            "got {lines:?}"
        );
    }

    /// A settled `read`, as the screenshot pins it: the path bare and absolute
    /// on the header, and a count as the whole of the result — no preview, and
    /// none of the envelope the tool writes for the model.
    #[test]
    fn a_settled_read_is_a_path_and_a_count_and_nothing_else() {
        let lines = tool_call(
            "read",
            ToolState::Completed {
                input: serde_json::json!({"filePath": "/repo/src/lib.rs"}),
                output: "<path>/repo/src/lib.rs</path>\n<content>\n1: fn main() {}\n</content>"
                    .to_owned(),
                title: "src/lib.rs".to_owned(),
                metadata: serde_json::json!({
                    "display": {
                        "type": "file",
                        "path": "/repo/src/lib.rs",
                        "lineStart": 1,
                        "lineEnd": 77,
                        "totalLines": 77,
                    },
                }),
                started: 0,
                completed: 1,
            },
        );
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec![
                "\u{25cf} Read(/repo/src/lib.rs)",
                "  \u{23bf} Read 77 lines"
            ],
            "got {lines:?}"
        );
    }

    /// A read that asked for a range says so, and says it the same way before
    /// and after the answer arrives — the header is about what was asked.
    #[test]
    fn a_read_of_a_range_names_it_and_names_it_the_same_while_running() {
        let input = serde_json::json!({
            "filePath": "/repo/src/lib.rs",
            "offset": 1158,
            "limit": 60,
        });
        let running = tool_call(
            "read",
            ToolState::Running {
                input: input.clone(),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        );
        let settled = tool_call(
            "read",
            ToolState::Completed {
                input,
                output: "the envelope".to_owned(),
                title: "src/lib.rs".to_owned(),
                metadata: serde_json::json!({
                    "display": {
                        "type": "file",
                        "path": "/repo/src/lib.rs",
                        "lineStart": 1158,
                        "lineEnd": 1217,
                    },
                }),
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
        assert_eq!(
            header(&running),
            "\u{2022} Read(/repo/src/lib.rs \u{b7} lines 1158-1217)"
        );
        assert_eq!(
            header(&settled),
            "\u{25cf} Read(/repo/src/lib.rs \u{b7} lines 1158-1217)"
        );
        assert!(
            settled
                .iter()
                .any(|line| line == "  \u{23bf} Read 60 lines"),
            "got {settled:?}"
        );
        assert!(
            !settled.iter().any(|line| line.contains("envelope")),
            "the tool's output is the model's, not the transcript's: {settled:?}"
        );
    }

    /// An open-ended read is not a range: an `offset` with no `limit` stops
    /// wherever the file does, which nothing can name before it is read.
    #[test]
    fn a_read_from_a_line_to_the_end_of_the_file_claims_no_range() {
        let lines = tool_call(
            "read",
            ToolState::Running {
                input: serde_json::json!({"filePath": "/repo/a.rs", "offset": 40}),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        );

        assert!(
            lines.iter().any(|line| line == "\u{2022} Read(/repo/a.rs)"),
            "got {lines:?}"
        );
    }

    /// The count is of what was read, so an empty file reports none — and a
    /// read that is not of a file at all keeps the rendering every other tool
    /// has, because a line count is not what it did.
    #[test]
    fn a_read_that_is_not_of_a_files_lines_keeps_the_ordinary_shape() {
        let empty = tool_call(
            "read",
            ToolState::Completed {
                input: serde_json::json!({"filePath": "/repo/empty.rs"}),
                output: String::new(),
                title: "empty.rs".to_owned(),
                metadata: serde_json::json!({
                    "display": {"type": "file", "lineStart": 1, "lineEnd": 0},
                }),
                started: 0,
                completed: 1,
            },
        );
        assert!(
            empty.iter().any(|line| line == "  \u{23bf} Read 0 lines"),
            "got {empty:?}"
        );

        // A PDF is the kind of read that is neither a file's lines nor a
        // directory's entries: `ganja_tool::read` publishes no `display` block
        // for one at all, so there is nothing here to count.
        let pdf = tool_call(
            "read",
            ToolState::Completed {
                input: serde_json::json!({"filePath": "/repo/paper.pdf"}),
                output: "PDF read successfully. This tool cannot hand file bytes to the model yet."
                    .to_owned(),
                title: "paper.pdf".to_owned(),
                metadata: serde_json::json!({
                    "preview": "PDF read successfully",
                    "truncated": false,
                    "mime": "application/pdf",
                }),
                started: 0,
                completed: 1,
            },
        );
        assert!(
            pdf.iter().any(|line| line == "  \u{23bf} paper.pdf")
                && pdf
                    .iter()
                    .any(|line| line.contains("PDF read successfully")),
            "a read that counted nothing keeps the ordinary shape, got {pdf:?}"
        );
    }

    /// A settled read of a **directory**, as the second screenshot pins it: the
    /// path on the header and a count of what was listed as the whole of the
    /// result — none of the envelope the tool writes for the model.
    #[test]
    fn a_settled_read_of_a_directory_is_a_count_and_no_envelope() {
        let lines = tool_call(
            "read",
            ToolState::Completed {
                input: serde_json::json!({"filePath": "/repo/src"}),
                output: "<path>/repo/src</path>\n<type>directory</type>\n<entries>\n\
                         lib.rs\nmain.rs\ncomponent/\n\n(3 entries)\n</entries>"
                    .to_owned(),
                title: "src".to_owned(),
                metadata: serde_json::json!({
                    "display": {
                        "type": "directory",
                        "path": "/repo/src",
                        "entries": ["component/", "lib.rs", "main.rs"],
                        "offset": 1,
                        "totalEntries": 3,
                        "truncated": false,
                    },
                }),
                started: 0,
                completed: 1,
            },
        );
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec!["\u{25cf} Read(/repo/src)", "  \u{23bf} Listed 3 entries"],
            "got {lines:?}"
        );
    }

    /// The header states the ask, on a directory as on a file: a listing that
    /// asked for a window says which one, and the count under it is of what
    /// that window actually held.
    #[test]
    fn a_read_of_a_range_of_a_directory_keeps_the_range_it_asked_for() {
        let lines = tool_call(
            "read",
            ToolState::Completed {
                input: serde_json::json!({
                    "filePath": "/repo/src",
                    "offset": 3,
                    "limit": 2,
                }),
                output: "<path>/repo/src</path>\n<type>directory</type>\n<entries>\n\
                         lib.rs\nmain.rs\n\n(Showing 2 of 9 entries. \
                         Use 'offset' parameter to read beyond entry 5)\n</entries>"
                    .to_owned(),
                title: "src".to_owned(),
                metadata: serde_json::json!({
                    "display": {
                        "type": "directory",
                        "path": "/repo/src",
                        "entries": ["lib.rs", "main.rs"],
                        "offset": 3,
                        "totalEntries": 9,
                        "truncated": true,
                    },
                }),
                started: 0,
                completed: 1,
            },
        );

        assert!(
            lines
                .iter()
                .any(|line| line == "\u{25cf} Read(/repo/src \u{b7} lines 3-4)")
                && lines
                    .iter()
                    .any(|line| line == "  \u{23bf} Listed 2 entries"),
            "got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("<entries>")),
            "the envelope is the model's, not the transcript's: {lines:?}"
        );
    }

    /// A `todowrite` carrying `todos` in each state the tool defines.
    fn todo_call(todos: serde_json::Value) -> ToolState {
        ToolState::Completed {
            input: serde_json::json!({ "todos": todos }),
            output: "[\n  {\n    \"content\": \"port cell.slang\"\n  }\n]".to_owned(),
            title: "2 todos".to_owned(),
            metadata: serde_json::json!({}),
            started: 0,
            completed: 1,
        }
    }

    /// The list every checklist test writes, one task per state that draws
    /// differently.
    fn todos() -> serde_json::Value {
        serde_json::json!([
            {"content": "port cell.slang", "status": "completed", "priority": "high"},
            {"content": "port graphics.slang", "status": "in_progress", "priority": "high"},
            {"content": "port bgimage.slang", "status": "pending", "priority": "medium"},
            {"content": "port the old shim", "status": "cancelled", "priority": "low"},
        ])
    }

    /// **The checklist screenshot.** A settled `todowrite` answers with the list
    /// itself — a box per task, the first on the elbow and the rest hanging
    /// under it — and never with the JSON the list travelled in.
    #[test]
    fn a_settled_todowrite_draws_its_list_as_a_checklist() {
        let lines = tool_call("todowrite", todo_call(todos()));
        let drawn: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| !line.is_empty())
            .collect();

        assert_eq!(
            drawn,
            vec![
                "\u{25cf} Todowrite(todos: [\u{2026}])",
                "  \u{23bf} \u{2612} port cell.slang",
                "    \u{2610} port graphics.slang",
                "    \u{2610} port bgimage.slang",
                "    \u{2612} port the old shim",
            ],
            "got {lines:?}"
        );
    }

    /// Each state is told by its box and by how the row is painted: the one
    /// being worked on stands out, and the two nobody will work on again are
    /// struck through.
    #[test]
    fn a_checklist_paints_the_task_in_hand_and_strikes_the_ones_that_are_done() {
        let theme = Theme::default();
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "todowrite".to_owned(),
                state: todo_call(todos()),
            },
        });
        chat.start_message(reply);

        let area = Rect::new(0, 0, 60, 10);
        let mut buffer = Buffer::empty(area);
        chat.render(area, &mut buffer, &theme);

        // Row 0 is the header, so the four tasks follow in the order written;
        // column 6 is the first column of each row's own words, past the
        // marker columns and the box.
        let done = buffer[(6, 1)].style();
        let in_hand = buffer[(6, 2)].style();
        let pending = buffer[(6, 3)].style();
        let cancelled = buffer[(6, 4)].style();

        assert!(
            done.add_modifier.contains(Modifier::CROSSED_OUT)
                && cancelled.add_modifier.contains(Modifier::CROSSED_OUT),
            "a finished task is struck through, got {done:?} and {cancelled:?}"
        );
        assert!(
            !buffer[(0, 1)]
                .style()
                .add_modifier
                .contains(Modifier::CROSSED_OUT),
            "the strike stays off the margin rather than ruling a line out to the left"
        );
        assert!(
            in_hand.add_modifier.contains(Modifier::BOLD) && in_hand.fg == theme.accent.fg,
            "the task in hand is the one the eye should land on, got {in_hand:?}"
        );
        assert!(
            !pending.add_modifier.contains(Modifier::CROSSED_OUT) && pending.fg == theme.fg.fg,
            "a task still to do is ordinary body text, got {pending:?}"
        );
    }

    /// An argument that is not a list this can draw keeps the preview every
    /// other tool's result has: what really arrived is more use than a `⎿`
    /// pointing at nothing.
    #[test]
    fn a_todowrite_whose_list_cannot_be_read_keeps_the_ordinary_preview() {
        for todos in [
            serde_json::json!("all of them"),
            serde_json::json!([]),
            serde_json::json!([{"status": "pending"}]),
        ] {
            let lines = tool_call("todowrite", todo_call(todos.clone()));

            assert!(
                lines.iter().any(|line| line == "  \u{23bf} 2 todos")
                    && lines.iter().any(|line| line.contains("port cell.slang")),
                "the tool's own title and preview stand in for {todos}: {lines:?}"
            );
            assert!(
                !lines
                    .iter()
                    .any(|line| line.contains(super::TODO_OPEN) || line.contains(super::TODO_DONE)),
                "nothing is drawn as a checklist it is not: {lines:?}"
            );
        }
    }

    /// **The checklist screenshot's other half.** While a turn runs, its newest
    /// list hangs under the working line in the strip pinned above the
    /// composer; when the turn settles the strip goes, and the call's own rows
    /// stay where they are.
    #[test]
    fn the_working_line_carries_this_turns_checklist_and_drops_it_on_settle() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("port the shaders"));
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "todowrite".to_owned(),
                state: todo_call(todos()),
            },
        });
        chat.start_message(reply);
        chat.set_working(Some(Working {
            started: Instant::now(),
            turn: 1,
            output_tokens: 0,
            compaction: None,
        }));

        let boxes = |lines: &[String]| {
            lines
                .iter()
                .filter(|line| line.contains(super::TODO_OPEN) || line.contains(super::TODO_DONE))
                .count()
        };
        let area = Rect::new(0, 0, 60, 24);
        let transcript = rendered(&mut chat, area);
        let running = strip(&mut chat, 60);

        assert!(
            !transcript.iter().any(|line| line.contains("\u{2026} (")),
            "the transcript itself no longer carries the line: {transcript:?}"
        );
        assert_eq!(
            boxes(&transcript),
            4,
            "the call's own rows stay in the transcript: {transcript:?}"
        );
        assert!(
            running
                .first()
                .is_some_and(|line| line.contains("\u{2026} (")),
            "the strip opens on the working line, got {running:?}"
        );
        assert_eq!(
            boxes(&running),
            4,
            "and carries this turn's list: {running:?}"
        );
        assert_eq!(
            running[1], "  \u{23bf} \u{2612} port cell.slang",
            "the copy hangs off the working line's own elbow: {running:?}"
        );

        chat.set_working(None);
        let settled = strip(&mut chat, 60);

        assert!(
            settled.is_empty(),
            "a settled turn leaves no strip: {settled:?}"
        );
        assert_eq!(
            boxes(&rendered(&mut chat, area)),
            4,
            "and the transcript keeps the rows it already drew"
        );
    }

    /// The copy under the working line is *this* turn's: a plan the last turn
    /// wrote is not what the running one is working through.
    #[test]
    fn the_working_line_carries_no_checklist_from_a_turn_that_is_over() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("port the shaders"));
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "todowrite".to_owned(),
                state: todo_call(todos()),
            },
        });
        chat.start_message(reply);
        chat.start_message(Message::user("now something else"));
        chat.start_message(Message::assistant("canned"));
        chat.set_working(Some(Working {
            started: Instant::now(),
            turn: 2,
            output_tokens: 0,
            compaction: None,
        }));

        let lines = strip(&mut chat, 60);

        assert!(
            lines
                .first()
                .is_some_and(|line| line.contains("\u{2026} (")),
            "the strip opens on the working line: {lines:?}"
        );
        assert_eq!(
            lines.len(),
            1,
            "the new turn has written no list, so nothing hangs under it: {lines:?}"
        );
    }

    /// **AC2.** A call that is running and the same call once it has settled
    /// are announced by the same words: what changed is the lead it wears —
    /// the pulsing point in flight, the verdict bullet after (2026-08-25) —
    /// and the color it is painted in, not a word in the text.
    #[test]
    fn a_running_call_and_its_settled_self_share_their_header_words() {
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
        assert_eq!(header(&running), "\u{2022} Shell(command: \"cargo test\")");
        assert_eq!(
            header(&completed),
            "\u{25cf} Shell(command: \"cargo test\")"
        );
        assert_eq!(header(&completed), header(&failed));
        assert_eq!(
            header(&running).strip_prefix('\u{2022}'),
            header(&completed).strip_prefix('\u{25cf}'),
            "past the lead, not a word moves when the call settles"
        );
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
                == "\u{2022} Grep(path: \"src\", pattern: \"fn main\", include: \"*.rs\", \u{2026})"),
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
                == "\u{2022} Write(filePath: \"a.rs\", content: \"fn main() {\u{2026}\")"),
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
                .any(|line| line == "\u{2022} Todowrite(todos: [\u{2026}])"),
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
                tool: "grep".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"pattern": "fn main"}),
                    output: "one\ntwo\nthree\nfour\nfive".to_owned(),
                    title: "5 matches".to_owned(),
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
                .any(|line| line.contains("\u{25cf} Grep(pattern: \"fn main\")")),
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
                    state: ToolState::Pending { input: None },
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
            lines.iter().any(|line| line == "\u{2022} Read"),
            "an update for an id never started should still append, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line == "\u{2022} Shell"),
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
            vec![&"\u{2022} Read(a.rs)".to_owned()],
            "got {lines:?}"
        );
    }

    /// A call waiting its turn behind the step's earlier calls names its
    /// arguments the moment the stream finishes saying them (2026-08-15), and
    /// stays a bare name while they are still arriving.
    #[test]
    fn a_waiting_call_names_its_arguments_once_they_have_settled() {
        let named = tool_call(
            "shell",
            ToolState::Pending {
                input: Some(serde_json::json!({"command": "cargo test"})),
            },
        );
        assert!(
            named
                .iter()
                .any(|line| line == "\u{2022} Shell(command: \"cargo test\")"),
            "got {named:?}"
        );

        let streaming = tool_call("shell", ToolState::Pending { input: None });
        assert!(
            streaming.iter().any(|line| line == "\u{2022} Shell"),
            "got {streaming:?}"
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
                == "\u{2022} Task(agent: \"explore\", description: \"find the parser\")"),
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
                .any(|line| line == "\u{2022} Task(description: \"find the parser\")"),
            "an agent nobody named is left off rather than invented, got {lines:?}"
        );
    }

    /// What runs inside a task is on the row, not behind a count
    /// (2026-08-15): once the watcher's log arrives, the newest calls hang
    /// under the heading in call order, the cut admitted above them off the
    /// true total.
    #[test]
    fn a_running_task_expands_the_childs_recent_calls_and_admits_the_cut() {
        let lines = tool_call(
            "task",
            ToolState::Running {
                input: serde_json::json!({
                    "description": "map it",
                    "subagent_type": "explore",
                }),
                metadata: serde_json::json!({
                    "toolcalls": 7,
                    "current_tool": "grep five",
                    "calls": ["grep one", "grep two", "grep three", "grep four", "grep five"],
                }),
                started: 0,
            },
        );

        assert!(
            lines
                .iter()
                .any(|line| line == "  \u{23bf} \u{2026} +3 lines (ctrl+t to expand)"),
            "three of seven calls are off the row and said to be: {lines:?}"
        );
        let two = lines
            .iter()
            .position(|line| line == "    grep two")
            .expect("the oldest shown call is on screen");
        assert_eq!(
            lines[two + 3],
            "    grep five",
            "the newest call ends the block in call order: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("grep one")),
            "the cut call is cut: {lines:?}"
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
            compaction: None,
        }
    }

    /// A turn that has been compacting for `seconds`, its summary `tokens`
    /// into a `budget`.
    fn compacting(seconds: u64, tokens: u64, budget: u64) -> Working {
        Working {
            compaction: Some(Compaction {
                tokens,
                budget,
                done: false,
            }),
            ..working(0, seconds, 0)
        }
    }

    /// The summary's whole arrival snaps the gauge full, and the settled
    /// turn holds it on screen for the settle window instead of taking the
    /// strip back in the same frame — then a layout past the window clears
    /// it like any settled turn's.
    #[test]
    fn a_finished_compaction_snaps_full_and_lingers_before_settling() {
        let mut chat = Chat::default();
        chat.set_compacting(500, 4_096);
        assert!(chat.finish_compacting(), "there was a compaction to finish");

        let snapped = strip(&mut chat, 60);
        assert_eq!(
            snapped[2],
            format!("  {} 100%", "\u{25b0}".repeat(40)),
            "the gauge is full the moment the summary lands"
        );

        chat.settle_working();
        assert!(
            !strip(&mut chat, 60).is_empty(),
            "the full gauge is held past the turn's end"
        );

        chat.settling = Instant::now().checked_sub(super::COMPACT_SETTLE);
        assert!(
            strip(&mut chat, 60).is_empty(),
            "and a layout past the hold takes the strip back"
        );
    }

    /// Only an arrival is held: a turn that ends mid-stream — a cancel, a
    /// dead provider — clears at once, and so does an ordinary turn's end.
    #[test]
    fn settling_holds_nothing_that_never_finished() {
        let mut chat = Chat::default();
        chat.set_compacting(500, 4_096);
        chat.settle_working();
        assert!(
            strip(&mut chat, 60).is_empty(),
            "an unfinished compaction has no arrival to show"
        );

        chat.set_working(Some(working(1, 3, 0)));
        assert!(
            !chat.finish_compacting(),
            "no compaction, nothing to finish"
        );
        chat.settle_working();
        assert!(
            strip(&mut chat, 60).is_empty(),
            "an ordinary turn settles the way it always did"
        );
    }

    /// The strip in its compacting dress (the 2026-08-25 reference
    /// screenshots): the spinner glyph on the headline, the clock in minutes
    /// and the streamed estimate abbreviated beside it, and under a blank
    /// row the forty-segment gauge with its percentage.
    #[test]
    fn a_compacting_turn_wears_its_own_dress_and_gauge() {
        let mut chat = Chat::default();
        chat.set_working(Some(compacting(121, 2_500, 4_096)));

        let lines = strip(&mut chat, 60);

        assert_eq!(
            lines[0],
            format!(
                "{} Compacting conversation\u{2026} (2m 1s \u{b7} \u{2193} 2.5k tokens)",
                working_frame(Duration::from_secs(121))
            ),
            "got {lines:?}"
        );
        assert_eq!(lines[1], "", "a blank row of air before the gauge");
        assert_eq!(
            lines[2],
            format!("  {}{} 61%", "\u{25b0}".repeat(24), "\u{25b1}".repeat(16)),
            "2500 of 4096 is 61%, and 61% of forty segments is 24"
        );
    }

    /// The gauge never claims the end while the stream is still coming, and
    /// a compaction that has streamed nothing yet shows a bare clock rather
    /// than a zero.
    #[test]
    fn the_compacting_gauge_opens_bare_and_clamps_at_ninety_nine() {
        let mut chat = Chat::default();
        chat.set_working(Some(compacting(3, 0, 4_096)));
        let opened = strip(&mut chat, 60);
        assert_eq!(
            opened[0],
            format!(
                "{} Compacting conversation\u{2026} (3s)",
                working_frame(Duration::from_secs(3))
            ),
            "no token clause before anything streamed"
        );
        assert_eq!(opened[2], format!("  {} 0%", "\u{25b1}".repeat(40)));

        chat.set_working(Some(compacting(3, 8_192, 4_096)));
        let overrun = strip(&mut chat, 60);
        assert_eq!(
            overrun[2],
            format!("  {}\u{25b1} 99%", "\u{25b0}".repeat(39)),
            "a stream past the budget is 99%, never a claimed end"
        );
    }

    /// The first progress event arms the strip on its own — the automatic
    /// trigger fires before any message opens — and later ones update it in
    /// place, keeping the compaction's own clock.
    #[test]
    fn compaction_progress_arms_the_strip_and_updates_it_in_place() {
        let mut chat = Chat::default();
        chat.set_compacting(0, 4_096);

        let armed = strip(&mut chat, 60);
        assert!(
            armed
                .first()
                .is_some_and(|line| line.contains("Compacting conversation\u{2026}")),
            "the first event arms the strip: {armed:?}"
        );

        chat.set_compacting(2_048, 4_096);
        let updated = strip(&mut chat, 60);
        assert!(
            updated.iter().any(|line| line.ends_with(" 50%")),
            "a later event moves the gauge: {updated:?}"
        );

        chat.set_working(None);
        assert!(
            strip(&mut chat, 60).is_empty(),
            "the strip settles the way every turn's does"
        );
    }

    /// The headline's paint rides the glyph's own clock — blue at the
    /// cycle's ends, periwinkle at its far frame, between the two on the way
    /// — so the icon and the color change together, as the reference's two
    /// frames show.
    #[test]
    fn the_compacting_pulse_swaps_paints_on_the_spinner_clock() {
        let step = u64::try_from(WORKING_FRAME_STEP).expect("a step fits in u64");
        let at = |steps: u64| compact_pulse(Duration::from_millis(steps * step));
        let paint = |(r, g, b): (u8, u8, u8)| ratatui::style::Color::Rgb(r, g, b);

        assert_eq!(at(0), paint(COMPACT_BLUE));
        assert_eq!(at(5), paint(COMPACT_PERIWINKLE), "the far frame");
        assert_eq!(at(10), at(0), "the whole cycle in, it starts over");
        assert_ne!(at(2), at(0), "the way there passes between the two");
        assert_ne!(at(2), at(5));
    }

    /// The two figures spell themselves the reference's way: minutes past a
    /// minute, `k` past a thousand, the tenth dropped when it is zero.
    #[test]
    fn the_compacting_figures_abbreviate_the_reference_way() {
        assert_eq!(compact_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(compact_elapsed(Duration::from_secs(121)), "2m 1s");
        assert_eq!(compact_tokens(840), "840");
        assert_eq!(compact_tokens(2_500), "2.5k");
        assert_eq!(compact_tokens(4_000), "4k");
    }

    /// **AC4.** While a turn runs the strip says so, with what it has spent;
    /// when the turn settles the strip is gone — and the viewport's own count
    /// never counted it, because the strip is not the transcript's to scroll.
    #[test]
    fn the_strip_says_a_turn_is_working_and_takes_it_back_when_the_turn_settles() {
        let mut chat = Chat::default();
        chat.start_message(Message::user("go on then"));
        let area = Rect::new(0, 0, 60, 10);
        rendered(&mut chat, area);
        let settled = chat.line_count();

        chat.set_working(Some(working(1, 12, 431)));
        let lines = strip(&mut chat, 60);

        assert!(
            lines.iter().any(|line| {
                *line
                    == format!(
                        "{} Thinking\u{2026} (12s \u{b7} \u{2193} 431 tokens)",
                        working_frame(Duration::from_secs(12))
                    )
            }),
            "got {lines:?}"
        );
        assert_eq!(
            chat.line_count(),
            settled,
            "the strip is not the transcript's to scroll"
        );

        chat.set_working(None);

        assert!(
            strip(&mut chat, 60).is_empty(),
            "a settled turn leaves no strip"
        );
        assert_eq!(chat.line_count(), settled);
    }

    /// A session that has spent nothing yet says nothing about tokens, rather
    /// than claiming a zero the screen would contradict.
    #[test]
    fn a_working_line_with_nothing_spent_yet_draws_no_token_segment() {
        let mut chat = Chat::default();
        chat.set_working(Some(working(1, 3, 0)));

        let lines = strip(&mut chat, 60);

        assert!(
            lines.iter().any(|line| {
                *line
                    == format!(
                        "{} Thinking\u{2026} (3s)",
                        working_frame(Duration::from_secs(3))
                    )
            }),
            "got {lines:?}"
        );
    }

    /// The glyph turns through Claude Code's spinner frames forward and back,
    /// one per step, off the turn's own clock — the first frame at the start,
    /// the far one at the fifth step, the first again after the tenth — so a
    /// line drawn twice at one instant is the same line, and nothing keeps a
    /// phase of its own.
    #[test]
    fn the_working_glyph_turns_through_the_frames_and_back_on_the_turns_clock() {
        let step = u64::try_from(WORKING_FRAME_STEP).expect("a step fits in u64");
        let at = |steps: u64| working_frame(Duration::from_millis(steps * step));
        assert_eq!(at(0), "\u{b7}");
        assert_eq!(at(5), "\u{273d}", "the far frame at the fifth step");
        assert_eq!(at(6), "\u{273b}", "and back the way it came");
        assert_eq!(at(10), at(0), "the whole cycle in, it starts over");
        // Within a step the frame holds: the same instant read twice.
        assert_eq!(
            working_frame(Duration::from_millis(step * 3 + step / 2)),
            at(3)
        );
        let forward: Vec<&str> = WORKING_FRAMES[..6].to_vec();
        let mut back = WORKING_FRAMES[6..].to_vec();
        back.reverse();
        assert_eq!(
            forward[1..5].to_vec(),
            back,
            "the way back is the way forward reversed, minus its ends"
        );
    }

    /// The verb rotates with the turn and repeats around the list, so the same
    /// transcript replayed twice reads the same both times.
    #[test]
    fn the_working_verb_rotates_with_the_turn_and_wraps_around() {
        let verb = |turn: u64| {
            let mut chat = Chat::default();
            chat.set_working(Some(working(turn, 0, 0)));
            strip(&mut chat, 40)
                .into_iter()
                .find(|line| !line.is_empty())
                .unwrap_or_default()
        };

        assert_eq!(verb(0), "\u{b7} Working\u{2026} (0s)");
        assert_eq!(verb(1), "\u{b7} Thinking\u{2026} (0s)");
        assert_ne!(verb(1), verb(2));
        let len = u64::try_from(WORKING_VERBS.len()).expect("verb count fits in u64");
        assert_eq!(
            verb(len),
            verb(0),
            "the whole list in, it starts over rather than running out"
        );
    }

    /// **Pre-mortem 2.** The working block lives outside the scroll entirely
    /// (2026-08-15): it can neither break the tail-follow nor move a viewport
    /// somebody pinned, because the transcript's own lines are the same with
    /// and without it.
    #[test]
    fn the_working_line_disturbs_neither_the_tail_nor_a_pinned_viewport() {
        let mut chat = Chat::default();
        transcript(&mut chat, 20);
        let tail = rendered(&mut chat, VIEWPORT);
        assert!(chat.is_following_tail());

        chat.set_working(Some(working(1, 5, 0)));
        assert_eq!(
            rendered(&mut chat, VIEWPORT),
            tail,
            "the strip is not the viewport's to show"
        );
        assert!(chat.is_following_tail(), "the tail is still followed");

        chat.set_working(None);
        chat.scroll_lines(-9);
        let pinned = rendered(&mut chat, VIEWPORT);
        assert!(!chat.is_following_tail());

        chat.set_working(Some(working(1, 6, 0)));

        assert_eq!(
            rendered(&mut chat, VIEWPORT),
            pinned,
            "a strip appearing at the bottom must not move a reader who is not there"
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
                "\u{2234} A greeting is enough,",
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

    /// A think renders whole, first line to last, with its paragraph breaks
    /// kept — the user's screenshot ruling (2026-08-14) that retired the tail
    /// clamp: a long thought scrolls back the way a long reply does, and no
    /// hint row stands where its opening lines used to be cut away.
    #[test]
    fn a_long_think_renders_whole_with_its_paragraphs() {
        let mut chat = Chat::default();
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part::reasoning_text(
            "one\ntwo\nthree\nfour\n\nfive\nsix\nseven",
        ));
        chat.start_message(reply);

        let lines = rendered(&mut chat, Rect::new(0, 0, 60, 14));
        let think_and_gap = &lines[..8];

        assert_eq!(
            think_and_gap,
            [
                "\u{2234} one",
                "  two",
                "  three",
                "  four",
                "",
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
            screen.contains("\u{2234} now there is a thought"),
            "got:\n{screen}"
        );
    }

    /// **AC3, the half this build can answer.** Sealed reasoning is a blob only
    /// the provider can open; there is nothing in it for a `∴` line to say, so
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
