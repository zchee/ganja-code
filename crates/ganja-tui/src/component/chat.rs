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
//! instead wears a dot that breathes, `\u{b7}` up through `\u{25cf}` and
//! back (2026-08-25) — a `\u{23bf}` marker introduces what
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

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ganja_protocol::{Message, MessageId, Part, PartBody, PartId, Role, ToolState, team};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::component::rewind;
use crate::theme::Theme;
use crate::{graphics, markdown, mention};

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

/// What leads a tool call still in flight, one rung per wink height: the
/// reference dot breathes size as well as brightness — its lit area more
/// than triples from trough to crest in the recording — so each height wears
/// the next dot the font already holds, the full bullet at the crest (user
/// directive, 2026-08-25: the maximum a size up from the small point). Every
/// rung is one column wide, so the words after it never move; a settled call
/// keeps `BULLET`, whose color answers the verdict — told from the crest's
/// identical glyph by standing still.
const POINT_GLYPHS: [&str; 5] = ["\u{b7} ", "\u{2219} ", "\u{2022} ", "\u{2022} ", "\u{25cf} "];

/// The rung `level` wears, clamped to the crest.
fn point_glyph(level: u8) -> &'static str {
    POINT_GLYPHS[usize::from(level.min(POINT_BRIGHT))]
}

/// The top of the point's wink, in quarters above the chrome's dim: the
/// height `point_level` counts in and `point_style` paints from.
const POINT_BRIGHT: u8 = 4;

/// What leads what a call answered, one step under the header it answers —
/// and, in the `/teammate` dialog, a member's ring of recent calls: a call log is
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
    /// part — and, since 2026-08-25, per readable-thinking part, whose block
    /// renders the same markdown folded into the thinking tone. Deliberately
    /// *not* inside [`Wrapped`] — a resize and a streamed delta both clear
    /// that, and neither is a reason to parse again.
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

/// Milliseconds the band takes to advance one column: 45 read as too quick
/// a flicker once the line sat beside the calmer wink, and 70 keeps a pass
/// over a typical line under two seconds (user directive, 2026-08-25).
const SHIMMER_STEP: u128 = 70;

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

    Color::Rgb(channel(base.0, peak.0), channel(base.1, peak.1), channel(base.2, peak.2))
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
        format!("{minutes}m {seconds}s", minutes = seconds / 60, seconds = seconds % 60)
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
    let head = format!("{glyph} Compacting conversation\u{2026} ", glyph = working_frame(elapsed));
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
            Style::default().fg(compact_pulse(elapsed)).add_modifier(Modifier::BOLD),
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
        Self { prefix: prefix.to_owned(), lead: None, text: text.into(), style }
    }

    /// A row whose prefix is painted `lead` while its text keeps `style`.
    fn led(prefix: &str, lead: Style, text: impl Into<String>, style: Style) -> Self {
        Self { lead: Some(lead), ..Self::new(prefix, text, style) }
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
        let margin = row.lead.unwrap_or_else(|| row.style.remove_modifier(Modifier::CROSSED_OUT));

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
    /// The wink height these lines were drawn at, [`None`] for an entry
    /// with no call in flight: what lets the point move without every
    /// settled entry losing its cache.
    blink: Option<u8>,
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
        self.shown().iter().map(|entry| (entry.role, entry.parts.as_slice())).collect()
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
                for later in
                    shown.iter().skip(index + 1).take_while(|later| later.role != Role::User)
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
        self.revert = Some(Revert { anchor, files, wrapped: None });
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

    /// The in-flight wink's height this instant, in `point_level`'s
    /// quarters. Time-driven like `working_frame`, so the same instant read
    /// twice draws the same frame twice.
    fn point_phase(&mut self) -> u8 {
        let epoch = *self.blink_epoch.get_or_insert_with(Instant::now);
        point_level(epoch.elapsed())
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
        let compaction = Some(Compaction { tokens, budget, done: false });
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
            Some(Working { compaction: Some(compaction), .. }) => {
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
            Some(Working { compaction: Some(Compaction { done: true, .. }), .. }) => {
                self.settling = Some(Instant::now())
            }
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

        if let Some(text) =
            entry.parts.iter_mut().find(|part| part.id == *part_id).and_then(Part::streamed_mut)
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

        match entry.parts.iter_mut().find(|existing| existing.id == part.id) {
            Some(existing) => *existing = part,
            None => entry.parts.push(part),
        }
        entry.wrapped = None;
    }

    /// Finds an entry by id, newest first: deltas address the message that is
    /// still streaming, which is the one at the end.
    fn entry_mut(&mut self, message_id: &MessageId) -> Option<&mut Entry> {
        self.entries.iter_mut().rev().find(|entry| entry.id == *message_id)
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
        let blink = self.point_phase();
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

        let offset =
            self.offset.map_or_else(|| self.max_offset(), |offset| offset.min(self.max_offset()));
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
                buffer.set_style(Rect::new(area.x, area.y + row, area.width, 1), theme.selection);
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

        entries + self.revert.as_ref().map_or(0, |revert| revert.lines().len())
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
                PartBody::Tool { tool, state: ToolState::Completed { input, .. }, .. }
                    if tool == TODO_TOOL =>
                {
                    todo_rows(input, theme)
                }
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
        if self.settling.is_some_and(|settled| settled.elapsed() >= COMPACT_SETTLE) {
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
        self.wrapped.as_ref().map_or(&[], |wrapped| wrapped.lines.as_slice())
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
        self.wrapped.as_ref().map_or(&[], |wrapped| wrapped.lines.as_slice())
    }

    /// The attached images the last wrap reserved boxes for.
    fn images(&self) -> &[(usize, String)] {
        self.wrapped.as_ref().map_or(&[], |wrapped| wrapped.images.as_slice())
    }

    fn wrap(
        &mut self,
        width: u16,
        theme: &Theme,
        graphics: bool,
        cells: &HashMap<String, (u32, u16)>,
        blink: u8,
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
                PartBody::ServerTool { tool, input, output } => {
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
                PartBody::File { path, mime, start, end, .. } => {
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
                    let label =
                        if mime == "text/plain" { token } else { format!("{token} ({mime})") };
                    let prefix = match self.role {
                        Role::User => prompt_lead(lines.is_empty()),
                        Role::Assistant => BULLET.to_owned(),
                    };
                    lines.extend(lay_out(&[Row::new(&prefix, label, theme.dim)], columns));
                }
                // Thinking a person can read, behind its own marker and toned
                // so it never competes with the answer it is on the way to.
                // Rendered **whole**, paragraph breaks included — the shape
                // the user's screenshot pinned (2026-08-14, retiring the
                // plan's pre-mortem-3 tail clamp): a long think scrolls back
                // the way a long reply does, and hiding the start of a
                // thought was the one cut the clamp could make. Since
                // 2026-08-25 the block renders its own markdown, through the
                // same cached document a reply's text uses, every span folded
                // back into the thinking tone by `thought` — the reasoning
                // summaries some providers write lead with `**bold**`
                // headings, and the markers belong to the shape, not on the
                // screen.
                PartBody::ReasoningText { text } if !text.is_empty() => {
                    let document = self.markdown.entry(part.id.clone()).or_default();
                    document.update(text, theme);

                    let indent = THINKING.width();
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
                            // The marker wears the block's own italic too:
                            // AC3's contract, that thinking is set apart from
                            // the reply by more than its color.
                            Span::styled(
                                THINKING.to_owned(),
                                theme.dim.add_modifier(Modifier::ITALIC),
                            )
                        };
                        let mut spans = Vec::with_capacity(line.spans.len() + 1);
                        spans.push(lead);
                        spans.extend(line.spans.into_iter().map(|span| thought(span, theme)));
                        lines.push(Line::from(spans));
                    }
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
                PartBody::Peer { from, summary, body, .. } => {
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
                    rows.extend(body.lines().map(|line| Row::new("  ", line.to_owned(), theme.fg)));
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
    if first { PROMPT.to_owned() } else { " ".repeat(PROMPT.width()) }
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
    let named = TITLE_KEYS.iter().copied().filter(|key| object.contains_key(*key));
    let rest = object.keys().map(String::as_str).filter(|key| !TITLE_KEYS.contains(key));

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
        PartBody::Tool { state: ToolState::Pending { .. } | ToolState::Running { .. }, .. }
    )
}

/// The wink's height at `elapsed`, in quarters of the way up from the
/// chrome's dim to bright: the envelope measured frame by frame off the
/// reference recording (2026-08-25) — bright through a long hold, straight
/// down past it, flat at the bottom, easing back up to meet the next hold.
fn point_level(elapsed: Duration) -> u8 {
    /// One whole wink, milliseconds — the reference's ~2 s cycle.
    const CYCLE: u128 = 2_000;
    /// Where the drop starts, where the rest at the bottom starts, and
    /// where the rise home starts — ~1.4 s of hold, ~140 ms down, ~160 ms
    /// flat, ~300 ms back, as measured.
    const DROP: u128 = 1_400;
    const REST: u128 = 1_540;
    const RISE: u128 = 1_700;

    let t = elapsed.as_millis() % CYCLE;
    let level = if t < DROP {
        u128::from(POINT_BRIGHT)
    } else if t < REST {
        (REST - t) * u128::from(POINT_BRIGHT) / (REST - DROP)
    } else if t < RISE {
        0
    } else {
        (t - RISE) * u128::from(POINT_BRIGHT) / (CYCLE - RISE)
    };

    u8::try_from(level).unwrap_or(POINT_BRIGHT)
}

/// The in-flight point's paint at `level` quarters of the way up from the
/// chrome's dim to bright. Both ends are theme styles; the way between
/// exists only where both name RGB values — an ANSI theme's middle collapses
/// to the nearer end instead, a wink rather than a fade, because named
/// palette slots are not endpoints anything could blend.
fn point_style(theme: &Theme, level: u8) -> Style {
    if level == 0 {
        return theme.dim;
    }
    if level >= POINT_BRIGHT {
        return theme.fg;
    }
    match (theme.dim.fg, theme.fg.fg) {
        (Some(Color::Rgb(dr, dg, db)), Some(Color::Rgb(br, bg, bb))) => Style::default().fg(blend(
            (dr, dg, db),
            (br, bg, bb),
            u64::from(level),
            u64::from(POINT_BRIGHT),
        )),
        _ => {
            if level * 2 >= POINT_BRIGHT {
                theme.fg
            } else {
                theme.dim
            }
        }
    }
}

/// A rendered markdown span folded back into the thinking tone: the shapes
/// markdown gave it stay — bold stays bold — while every color comes home to
/// the chrome's dim italic, so a thought keeps reading as a thought whatever
/// its markup asked for.
fn thought(span: Span<'static>, theme: &Theme) -> Span<'static> {
    let kept = span.style.add_modifier;
    Span::styled(span.content, theme.dim.add_modifier(Modifier::ITALIC).add_modifier(kept))
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
    let limit = input.get("limit").and_then(serde_json::Value::as_u64).filter(|limit| *limit > 0);

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
            let start = display.get("lineStart").and_then(serde_json::Value::as_u64)?;
            let end = display.get("lineEnd").and_then(serde_json::Value::as_u64)?;
            // An empty file reports an end before its start, and read nothing.
            let read = if end < start { 0 } else { end - start + 1 };

            Some(format!("Read {read} line{plural}", plural = if read == 1 { "" } else { "s" }))
        }
        "directory" => {
            let entries = display.get("entries").and_then(serde_json::Value::as_array)?;

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
    let todos = input.get("todos")?.as_array().filter(|todos| !todos.is_empty())?;

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
            let prefix = if index == 0 && !claimed { RESULT } else { under.as_str() };
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
    let preview: Vec<String> = lines.by_ref().take(TOOL_PREVIEW_LINES).map(str::to_owned).collect();

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

    (lines[skipped..].iter().map(|line| (*line).to_owned()).collect(), skipped)
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
/// in flight leads with the winking point at `blink`'s height — sized and
/// painted by its rung, biggest at the crest — the words unmoved
/// (2026-08-25).
fn tool_lines(tool: &str, state: &ToolState, theme: &Theme, blink: u8) -> Vec<Row> {
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
            point_glyph(blink),
            point_style(theme, blink),
            tool_heading(tool, input.as_ref()),
            theme.dim,
        )],
        ToolState::Running { input, metadata, .. } => {
            let mut rows = vec![Row::led(
                point_glyph(blink),
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
        ToolState::Completed { input, output, title, metadata, .. } => {
            // The bullet alone answers "did it work" (2026-08-15): green
            // here, red on a failed call, while the heading stays prose.
            let mut rows =
                vec![Row::led(BULLET, theme.success, tool_heading(tool, Some(input)), theme.fg)];
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
            let mut rows =
                vec![Row::led(BULLET, theme.error, tool_heading(tool, Some(input)), theme.fg)];
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
fn task_lines(state: &ToolState, theme: &Theme, blink: u8) -> Vec<Row> {
    match state {
        ToolState::Pending { input } => {
            let heading = input.as_ref().map_or_else(
                || titlecase(TASK_TOOL),
                |input| task_heading(field(input, "subagent_type"), field(input, "description")),
            );

            vec![Row::led(point_glyph(blink), point_style(theme, blink), heading, theme.dim)]
        }
        ToolState::Running { input, metadata, .. } => {
            let agent = field(input, "subagent_type");
            let mut rows = vec![Row::led(
                point_glyph(blink),
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
        ToolState::Completed { input, title, metadata, started, completed, .. } => {
            let agent = field(metadata, "agent").or_else(|| field(input, "subagent_type"));
            let description = field(input, "description").or(Some(title.as_str()));

            vec![
                Row::led(BULLET, theme.success, task_heading(agent, description), theme.fg),
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
    value.get(key).and_then(serde_json::Value::as_str).filter(|found| !found.is_empty())
}

/// How many tools the child has called, as its parent's part recorded it.
fn toolcalls(metadata: &serde_json::Value) -> u64 {
    metadata.get("toolcalls").and_then(serde_json::Value::as_u64).unwrap_or(0)
}

/// The child's own calls, as the watcher logged them onto the parent's part —
/// empty for a part that carries no log, which is also every foreign tool's.
fn call_log(metadata: &serde_json::Value) -> Vec<String> {
    metadata
        .get("calls")
        .and_then(serde_json::Value::as_array)
        .map(|calls| {
            calls.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect()
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

    format!("{minutes}m {rest}s", minutes = seconds / 60, rest = seconds % 60)
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
#[path = "chat_tests.rs"]
mod tests;
