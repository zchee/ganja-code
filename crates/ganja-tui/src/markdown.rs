//! Assistant markdown, rendered to width-independent styled lines.
//!
//! Spec: upstream `packages/tui/src/routes/session/index.tsx:1685-1693` hands an
//! assistant text part to opentui's `<markdown>` renderable, which lexes with
//! `marked` and re-renders each top-level block as a tree-sitter–highlighted
//! source buffer. Ganja ports the *behavior* — block structure, which theme key
//! paints what, conceal, grid tables — on pulldown-cmark and syntect, because a
//! WASM tree-sitter worker is not a thing a Rust TUI should grow.
//!
//! Only **assistant text parts** arrive here (ruling R12). User messages, tool
//! output, file chips and every dialog stay plain, so nothing a person typed is
//! ever re-interpreted as markup.
//!
//! # The plain-text invariant, and how it is carried
//!
//! P1 rendered assistant text as plain text, and 25 snapshots pin what that
//! looks like. Those snapshots stay byte-identical not because plain text is
//! detected and routed around this module — a detector is exactly the thing
//! that goes wrong later — but because the renderer's own semantics agree with
//! plain text on plain text:
//!
//! - a source `\n` inside a paragraph is a **hard line break** (`SoftBreak` ⇒
//!   line break), so prose never re-flows across the lines its author wrote;
//! - [`Event::Text`] spans render **verbatim**, intra-line whitespace included;
//! - consecutive top-level blocks are separated by exactly one blank line,
//!   which is what a blank line between two paragraphs already produced;
//! - a markdown theme key the active theme does not name falls back to the
//!   body role, so a theme that names none paints exactly what P1 painted.
//!
//! # Two-stage cache
//!
//! Stage 1 lives here: [`Document`] holds each top-level block's styled lines
//! keyed by `(block source hash, theme revision)`. A streamed delta re-segments
//! the part — block boundaries are not prefix-stable in markdown, so upstream
//! re-lexes and compares `token.raw` too — but only blocks whose source
//! actually changed are re-styled, and syntect never re-runs over a stable
//! fence. A call that changed neither the part nor the theme does not even
//! segment, which is what makes a resize free here: the transcript re-enters
//! stage 1 on every wrap it has to redo, and a resize is one of those.
//!
//! Stage 2 is the width-keyed wrap the transcript already had
//! (`component/chat.rs`); [`wrap`] is its markdown-aware half.
//!
//! # Scope table
//!
//! Upstream maps tree-sitter capture names onto nine `syntax*` theme keys
//! (`packages/tui/src/theme/index.ts:553-1088`). Syntect emits TextMate scopes
//! instead, so that table is translated once, here, in [`SCOPE_RULES`]: rules
//! are listed most-specific-first and the **first rule whose scope is a prefix
//! of the token's scope wins**, tested against each scope on the stack from the
//! innermost outwards. The non-`syntax*` rows are upstream's own oddities kept
//! rather than tidied away — a builtin or a `self`/`super` reads as `error`, an
//! attribute as `warning`, an HTML tag name as `error`.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash as _, Hasher as _},
    ops::Range,
    sync::OnceLock,
};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::ScopeRegionIterator,
    parsing::{ParseState, Scope, ScopeStack, SyntaxSet},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    component::chat::split_at_width,
    theme::{Rgba, Theme},
};

/// What draws a blockquote's left edge (upstream draws a one-sided border
/// box, `index.bun.js:9646-9658`; a cell renderer draws the glyph).
const QUOTE_BORDER: &str = "\u{258c}";

/// What a horizontal rule is filled with.
const RULE_GLYPH: char = '\u{2500}';

/// Columns a table cell may occupy before its content is clipped (D108: the
/// grid never wraps inside a cell, so a cell has to end somewhere).
const TABLE_CELL_MAX: usize = 40;

/// Theme keys this module reads.
mod key {
    pub(super) const TEXT: &str = "markdownText";
    pub(super) const HEADING: &str = "markdownHeading";
    pub(super) const STRONG: &str = "markdownStrong";
    pub(super) const EMPH: &str = "markdownEmph";
    pub(super) const LIST_ITEM: &str = "markdownListItem";
    pub(super) const BLOCK_QUOTE: &str = "markdownBlockQuote";
    pub(super) const CODE: &str = "markdownCode";
    pub(super) const LINK: &str = "markdownLink";
    pub(super) const LINK_TEXT: &str = "markdownLinkText";

    pub(super) const SYNTAX: [&str; 9] = [
        "syntaxComment",
        "syntaxKeyword",
        "syntaxFunction",
        "syntaxVariable",
        "syntaxString",
        "syntaxNumber",
        "syntaxType",
        "syntaxOperator",
        "syntaxPunctuation",
    ];
}

/// Which of the nine `syntax*` slots — or which of upstream's three non-syntax
/// escapes — a TextMate scope resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scoped {
    Comment,
    Keyword,
    /// `keyword.control.import`/`export`, which upstream deliberately leaves
    /// upright where every other keyword is italic (`index.ts:553-1088`).
    KeywordUpright,
    Function,
    Variable,
    Text,
    Number,
    Type,
    Operator,
    Punctuation,
    /// Upstream's `*.builtin`, `variable.super` and `tag` rows.
    Error,
    /// Upstream's `attribute`/`annotation` row.
    Warning,
}

/// TextMate scope prefix → slot, **most specific first**; the first rule whose
/// scope prefixes the token's scope wins. See the module doc.
const SCOPE_RULES: [(&str, Scoped); 32] = [
    // A delimiter belongs to the thing it delimits: upstream's tree-sitter
    // queries capture `//` inside `@comment` and a quote inside `@string`,
    // where TextMate scopes them as punctuation.
    ("punctuation.definition.comment", Scoped::Comment),
    ("punctuation.definition.string", Scoped::Text),
    ("comment", Scoped::Comment),
    ("keyword.control.import", Scoped::KeywordUpright),
    ("keyword.control.export", Scoped::KeywordUpright),
    ("keyword.operator", Scoped::Operator),
    ("keyword", Scoped::Keyword),
    ("storage.modifier", Scoped::Keyword),
    ("storage.type", Scoped::Keyword),
    ("storage", Scoped::Keyword),
    ("constant.numeric", Scoped::Number),
    ("constant.language", Scoped::Keyword),
    ("constant.character.escape", Scoped::Text),
    ("constant", Scoped::Variable),
    ("string", Scoped::Text),
    ("entity.name.function", Scoped::Function),
    ("entity.name.tag", Scoped::Error),
    ("entity.name.type", Scoped::Type),
    ("entity.name", Scoped::Type),
    ("entity.other.attribute-name", Scoped::Warning),
    ("support.function.builtin", Scoped::Error),
    ("support.function", Scoped::Function),
    ("support.type", Scoped::Type),
    ("support.class", Scoped::Type),
    ("support.constant", Scoped::Variable),
    ("variable.language", Scoped::Error),
    ("variable.function", Scoped::Function),
    ("variable.parameter", Scoped::Variable),
    ("variable", Scoped::Variable),
    ("punctuation", Scoped::Punctuation),
    ("meta.annotation", Scoped::Warning),
    ("invalid", Scoped::Error),
];

/// Info-string aliases upstream declares on its parsers
/// (`packages/tui/src/parsers-config.ts:300-301, 343-344`). Everything else
/// reaches syntect's own token lookup unchanged, which already answers to a
/// language's name and to its file extensions.
const INFO_ALIASES: [(&str, &str); 3] =
    [("udiff", "diff"), ("patch", "diff"), ("makefile", "make")];

/// The scope rules, compiled once. Compiling a [`Scope`] interns its atoms, so
/// matching afterwards is an integer compare rather than a string one.
fn scope_rules() -> &'static [(Scope, Scoped)] {
    static RULES: OnceLock<Vec<(Scope, Scoped)>> = OnceLock::new();

    RULES.get_or_init(|| {
        SCOPE_RULES
            .iter()
            .filter_map(|(name, slot)| Scope::new(name).ok().map(|scope| (scope, *slot)))
            .collect()
    })
}

/// Syntect's bundled syntax set, loaded on the first fenced block that names a
/// language it knows.
///
/// Deliberately lazy: unpacking the dump costs tens of milliseconds, and a
/// session that never shows a fence — a resume of 200 prose messages, which
/// P4's budget test measures — must not pay it.
fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// How a logical line becomes visual lines at a given width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineKind {
    /// Prose: greedy word wrap, whitespace preserved except where a break
    /// consumes it.
    Flow,
    /// Code: chopped at the column, because a wrapped token is a lie about
    /// what the file says and a dropped one is worse.
    Chop,
    /// A table row: clipped, so the grid's verticals stay vertical (D108).
    Clip,
    /// A horizontal rule: filled to the width it is drawn at.
    Rule,
}

/// One logical line of rendered markdown, before any width is known.
#[derive(Clone, Debug)]
pub(crate) struct MdLine {
    /// Drawn at the start of **every** visual line this one wraps to: the
    /// blockquote border, the indent a nested list sits at.
    prefix: Vec<Span<'static>>,
    spans: Vec<Span<'static>>,
    /// Extra columns continuation lines are indented by, which is what hangs a
    /// list item's text under its own first word instead of under its marker.
    hang: usize,
    kind: LineKind,
}

impl MdLine {
    fn new(spans: Vec<Span<'static>>, kind: LineKind) -> Self {
        Self {
            prefix: Vec::new(),
            spans,
            hang: 0,
            kind,
        }
    }

    /// The blank line that separates two blocks.
    fn blank() -> Self {
        Self::new(Vec::new(), LineKind::Flow)
    }

    /// What this line reads as, ignoring style. Tests assert on this, and so
    /// does the wrap when it measures.
    #[cfg(test)]
    fn text(&self) -> String {
        self.prefix
            .iter()
            .chain(self.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn prefix_width(&self) -> usize {
        self.prefix
            .iter()
            .map(|span| span.content.as_ref().width())
            .sum()
    }
}

/// The styles a theme resolves to for markdown and for highlighted code.
///
/// A key the theme does not name falls back to the body role rather than to an
/// invented color — which is what lets a theme that names no `markdown*` key
/// (the terminal theme) paint exactly what plain text painted.
#[derive(Clone, Debug)]
struct Styles {
    text: Style,
    heading: Style,
    strong: Style,
    emph: Style,
    list_item: Style,
    block_quote: Style,
    code: Style,
    link: Style,
    link_text: Style,
    /// Chrome: the quote border, the rule, the table grid. Upstream paints all
    /// three from the `conceal` scope rather than from a markdown key
    /// (`index.bun.js:9643-9644, 9852-9861`), which is `textMuted`.
    chrome: Style,
    /// `markup.strikethrough` → `textMuted` (`index.ts:1016-1021`).
    muted: Style,
    syntax: [Style; 9],
    warning: Style,
    error: Style,
}

impl Styles {
    fn new(theme: &Theme) -> Self {
        let body = fg(theme, key::TEXT).unwrap_or(theme.fg);
        let named = |name: &str| fg(theme, name).unwrap_or(body);
        let code = named(key::CODE);

        Self {
            text: body,
            heading: named(key::HEADING),
            strong: named(key::STRONG),
            emph: named(key::EMPH),
            list_item: named(key::LIST_ITEM),
            block_quote: named(key::BLOCK_QUOTE),
            code,
            link: named(key::LINK),
            link_text: named(key::LINK_TEXT),
            chrome: theme.dim,
            muted: theme.dim,
            syntax: key::SYNTAX.map(|name| fg(theme, name).unwrap_or(code)),
            warning: theme.warning,
            error: theme.error,
        }
    }

    fn scoped(&self, slot: Scoped) -> Style {
        match slot {
            Scoped::Comment => self.syntax[0].add_modifier(Modifier::ITALIC),
            Scoped::Keyword => self.syntax[1].add_modifier(Modifier::ITALIC),
            Scoped::KeywordUpright => self.syntax[1],
            Scoped::Function => self.syntax[2],
            Scoped::Variable => self.syntax[3],
            Scoped::Text => self.syntax[4],
            Scoped::Number => self.syntax[5],
            Scoped::Type => self.syntax[6],
            Scoped::Operator => self.syntax[7],
            Scoped::Punctuation => self.syntax[8],
            Scoped::Error => self.error,
            Scoped::Warning => self.warning,
        }
    }
}

/// The foreground a theme key resolves to, or [`None`] when it names nothing
/// or names something transparent.
fn fg(theme: &Theme, key: &str) -> Option<Style> {
    theme
        .color(key)
        .and_then(Rgba::color)
        .map(|color| Style::new().fg(color))
}

/// One top-level block's rendered lines, and the key they were rendered under.
#[derive(Debug)]
struct Cached {
    source: u64,
    lines: Vec<MdLine>,
}

/// The stage-1 cache for one assistant text part.
#[derive(Debug)]
pub(crate) struct Document {
    blocks: Vec<Cached>,
    /// The whole part's source, so that a call which changed nothing — every
    /// resize, and every frame of an entry the wrap had to drop — costs a hash
    /// rather than a parse.
    source: Option<u64>,
    /// The theme revision every block's styles were resolved under. A switch
    /// invalidates all of them, exactly as it invalidates the wrap.
    revision: Option<u64>,
    separator: MdLine,
    /// How many blocks this document has ever styled, and how many times it
    /// has walked the parser. The cache's only observable behavior, and what
    /// the streaming and resize tests assert on.
    styled: usize,
    parsed: usize,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            source: None,
            revision: None,
            separator: MdLine::blank(),
            styled: 0,
            parsed: 0,
        }
    }
}

impl Document {
    /// Re-reads `source`, styling only the blocks that changed.
    pub(crate) fn update(&mut self, source: &str, theme: &Theme) {
        let whole = hash(source);
        if self.source == Some(whole) && self.revision == Some(theme.revision()) {
            return;
        }
        if self.revision != Some(theme.revision()) {
            self.blocks.clear();
            self.revision = Some(theme.revision());
        }
        self.source = Some(whole);
        self.parsed += 1;
        let styles = Styles::new(theme);

        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);

        let mut events = Vec::new();
        let mut ranges = Vec::new();
        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            events.push(event);
            ranges.push(range);
        }

        let mut previous: Vec<Option<Cached>> = std::mem::take(&mut self.blocks)
            .into_iter()
            .map(Some)
            .collect();

        for (index, span) in segment(&events).into_iter().enumerate() {
            let key = hash(&source[block_source(&ranges, &span, source)]);

            if let Some(slot) = previous.get_mut(index)
                && slot.as_ref().is_some_and(|cached| cached.source == key)
            {
                self.blocks
                    .push(slot.take().expect("the slot was just checked"));
                continue;
            }

            self.styled += 1;
            let mut lines = Vec::new();
            Walker {
                events: &events[span.clone()],
                ranges: &ranges[span.clone()],
                source,
                styles: &styles,
                body: styles.text,
                at: 0,
            }
            .block(&mut lines);
            self.blocks.push(Cached { source: key, lines });
        }
    }

    /// Every rendered line, blocks separated by one blank line — which is
    /// what a blank line between two paragraphs already produced when this was
    /// plain text.
    pub(crate) fn lines(&self) -> impl Iterator<Item = &MdLine> {
        self.blocks.iter().enumerate().flat_map(|(index, block)| {
            (index > 0)
                .then_some(&self.separator)
                .into_iter()
                .chain(block.lines.iter())
        })
    }

    /// How many blocks this document has styled since it was created. A
    /// streamed delta that re-styles a stable block shows up here and nowhere
    /// else.
    #[cfg(test)]
    pub(crate) fn styled(&self) -> usize {
        self.styled
    }

    /// How many times this document has walked the parser. A resize that
    /// reached stage 1 shows up here.
    #[cfg(test)]
    pub(crate) fn parsed(&self) -> usize {
        self.parsed
    }
}

/// The source range a top-level block covers, trimmed of the blank lines that
/// follow it so that appending to a document does not change an earlier
/// block's key.
fn block_source(ranges: &[Range<usize>], span: &Range<usize>, source: &str) -> Range<usize> {
    let range = ranges[span.start].clone();
    let trimmed = source[range.clone()].trim_end_matches('\n').len();

    range.start..range.start + trimmed
}

/// Splits the event stream into one span per top-level block.
fn segment(events: &[Event<'_>]) -> Vec<Range<usize>> {
    let mut blocks = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (index, event) in events.iter().enumerate() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    start = index;
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    blocks.push(start..index + 1);
                }
            }
            // A rule, or a raw HTML block: a whole block in one event.
            _ if depth == 0 => blocks.push(index..index + 1),
            _ => {}
        }
    }

    blocks
}

fn hash(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);

    hasher.finish()
}

/// Walks one block's events, emitting logical lines.
struct Walker<'a, 'e> {
    events: &'a [Event<'e>],
    ranges: &'a [Range<usize>],
    source: &'a str,
    styles: &'a Styles,
    /// The style plain text is painted in here. A blockquote swaps it for its
    /// own so the body italic reaches nested blocks too, rather than being
    /// patched over their styles afterwards.
    body: Style,
    at: usize,
}

impl Walker<'_, '_> {
    /// Renders the block starting at the cursor, consuming it whole.
    fn block(&mut self, out: &mut Vec<MdLine>) {
        let Some(event) = self.events.get(self.at) else {
            return;
        };

        match event {
            Event::Start(Tag::Paragraph) => {
                self.at += 1;
                let lines = self.inline();
                self.at += 1;
                out.extend(
                    lines
                        .into_iter()
                        .map(|spans| MdLine::new(spans, LineKind::Flow)),
                );
            }
            Event::Start(Tag::Heading { level, .. }) => {
                let level = *level;
                self.at += 1;
                let saved = self.body;
                self.body = heading_style(self.styles, level);
                let lines = self.inline();
                self.at += 1;
                self.body = saved;
                out.extend(
                    lines
                        .into_iter()
                        .map(|spans| MdLine::new(spans, LineKind::Flow)),
                );
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.at += 1;
                let saved = self.body;
                self.body = self.styles.block_quote.add_modifier(Modifier::ITALIC);
                let mut inner = Vec::new();
                self.children(&mut inner, true);
                self.body = saved;

                for mut line in inner {
                    line.prefix.splice(
                        0..0,
                        [
                            Span::styled(QUOTE_BORDER.to_owned(), self.styles.chrome),
                            Span::styled(" ".to_owned(), self.styles.chrome),
                        ],
                    );
                    out.push(line);
                }
            }
            Event::Start(Tag::List(first)) => {
                let mut number = *first;
                self.at += 1;
                let mut loose = false;

                while matches!(self.events.get(self.at), Some(Event::Start(Tag::Item))) {
                    let marker = self.marker(number);
                    number = number.map(|value| value.saturating_add(1));
                    self.at += 1;

                    let mut inner = Vec::new();
                    loose |= self.item(&mut inner);
                    if loose && !out.is_empty() {
                        out.push(MdLine::blank());
                    }
                    indent(&mut inner, &marker, self.styles.list_item);
                    out.append(&mut inner);
                }

                // The list's own `End`.
                self.at += 1;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().map(str::to_owned)
                    }
                    CodeBlockKind::Indented => None,
                };
                self.at += 1;
                let mut code = String::new();
                while let Some(event) = self.events.get(self.at) {
                    self.at += 1;
                    match event {
                        Event::End(TagEnd::CodeBlock) => break,
                        Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                            code.push_str(text);
                        }
                        _ => {}
                    }
                }
                out.extend(highlight(&code, language.as_deref(), self.styles));
            }
            Event::Start(Tag::Table(_)) => {
                self.at += 1;
                let rows = self.table();
                out.extend(table_lines(&rows, self.styles));
            }
            Event::Rule => {
                self.at += 1;
                out.push(MdLine::new(
                    vec![Span::styled(String::new(), self.styles.chrome)],
                    LineKind::Rule,
                ));
            }
            // A raw HTML block reaches the screen as what it says, which is the
            // only rendering of it that cannot be wrong.
            Event::Html(html) => {
                let html = html.to_string();
                self.at += 1;
                out.extend(html.trim_end_matches('\n').split('\n').map(|line| {
                    MdLine::new(
                        vec![Span::styled(line.to_owned(), self.body)],
                        LineKind::Flow,
                    )
                }));
            }
            // A tight list item's text, which pulldown-cmark emits without a
            // paragraph around it — and anything else that lands at block
            // position reads as the paragraph it looks like.
            _ => {
                let before = self.at;
                let lines = self.inline();
                out.extend(
                    lines
                        .into_iter()
                        .map(|spans| MdLine::new(spans, LineKind::Flow)),
                );
                // [`Walker::inline`] leaves its terminator in place; if it
                // consumed nothing at all, stepping over the event is what
                // keeps the caller's loop moving.
                if self.at == before {
                    self.at += 1;
                }
            }
        }
    }

    /// Renders sibling blocks until the enclosing container's `End`, which it
    /// consumes. Returns nothing; `separate_lists` says whether a list that
    /// follows another block gets a blank line in front of it — inside a list
    /// item it does not, because a nested list belongs to the item above it.
    fn children(&mut self, out: &mut Vec<MdLine>, separate_lists: bool) {
        let mut first = true;

        while let Some(event) = self.events.get(self.at) {
            if matches!(event, Event::End(_)) {
                self.at += 1;
                break;
            }

            let is_list = matches!(event, Event::Start(Tag::List(_)));
            let mut block = Vec::new();
            self.block(&mut block);
            if !first && (separate_lists || !is_list) {
                out.push(MdLine::blank());
            }
            out.append(&mut block);
            first = false;
        }
    }

    /// Renders one list item's content. Returns whether the item was loose —
    /// pulldown-cmark wraps a loose item's text in a paragraph and a tight
    /// item's in nothing, which is the only signal there is.
    fn item(&mut self, out: &mut Vec<MdLine>) -> bool {
        let loose = matches!(self.events.get(self.at), Some(Event::Start(Tag::Paragraph)));

        if let Some(Event::TaskListMarker(checked)) = self.events.get(self.at) {
            let mark = if *checked { "[x] " } else { "[ ] " };
            let style = if *checked {
                self.styles.list_item
            } else {
                self.styles.muted
            };
            self.at += 1;
            let mut inner = Vec::new();
            self.children(&mut inner, true);
            if let Some(line) = inner.first_mut() {
                line.spans.insert(0, Span::styled(mark.to_owned(), style));
                line.hang += mark.width();
            }
            for line in inner.iter_mut().skip(1) {
                line.prefix
                    .push(Span::styled(" ".repeat(mark.width()), Style::new()));
            }
            out.append(&mut inner);

            return loose;
        }

        self.children(out, false);

        loose
    }

    /// The marker the source wrote for the item at the cursor, kept verbatim
    /// (R12) rather than normalized to one bullet character.
    fn marker(&self, number: Option<u64>) -> String {
        if let Some(number) = number {
            let delimiter = self
                .item_source()
                .and_then(|text| {
                    text.chars()
                        .find(|character| matches!(character, '.' | ')'))
                })
                .unwrap_or('.');

            return format!("{number}{delimiter} ");
        }

        let bullet = self
            .item_source()
            .and_then(|text| text.chars().next())
            .filter(|character| matches!(character, '-' | '*' | '+'))
            .unwrap_or('-');

        format!("{bullet} ")
    }

    /// The item's own source text, with the indent that positioned it removed.
    fn item_source(&self) -> Option<&str> {
        let range = self.ranges.get(self.at)?.clone();

        Some(self.source.get(range)?.trim_start())
    }

    /// Collects a table's cells, the head row first.
    fn table(&mut self) -> Vec<Vec<Vec<Span<'static>>>> {
        let mut rows = Vec::new();
        let mut row = Vec::new();
        let mut head = false;

        while let Some(event) = self.events.get(self.at) {
            match event {
                Event::Start(Tag::TableHead) => {
                    head = true;
                    self.at += 1;
                }
                Event::Start(Tag::TableRow) => {
                    self.at += 1;
                }
                Event::Start(Tag::TableCell) => {
                    self.at += 1;
                    let saved = self.body;
                    if head {
                        self.body = self.styles.heading.add_modifier(Modifier::BOLD);
                    }
                    let mut cell = self.inline();
                    self.at += 1;
                    self.body = saved;
                    // A cell is one row of the grid whatever its source did,
                    // so a break inside it collapses rather than splitting the
                    // table (D108).
                    row.push(cell.pop().unwrap_or_default());
                }
                Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                    head = false;
                    self.at += 1;
                    rows.push(std::mem::take(&mut row));
                }
                Event::End(TagEnd::Table) => {
                    self.at += 1;
                    break;
                }
                _ => self.at += 1,
            }
        }

        rows
    }

    /// Renders inline content up to — but **not** including — the first event
    /// that is not inline, leaving that terminator for the caller: a
    /// paragraph's own `End`, a tight list item's `End`, or the `Start` of a
    /// list nested inside one.
    ///
    /// Returns one span list per source line, because a `SoftBreak` is a line
    /// break (R12) and prose never re-flows across the lines its author wrote.
    fn inline(&mut self) -> Vec<Vec<Span<'static>>> {
        let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut stack: Vec<Style> = Vec::new();
        let mut style = self.body;
        let mut link: Option<(String, usize)> = None;

        while let Some(event) = self.events.get(self.at) {
            if !is_inline(event) {
                break;
            }
            self.at += 1;

            match event {
                Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough) => {
                    style = stack.pop().unwrap_or(self.body);
                }
                Event::End(TagEnd::Link | TagEnd::Image) => {
                    style = stack.pop().unwrap_or(self.body);
                    if let Some((url, from)) = link.take() {
                        let label: String = lines
                            .last()
                            .map(|spans| {
                                spans[from.min(spans.len())..]
                                    .iter()
                                    .map(|span| span.content.as_ref())
                                    .collect()
                            })
                            .unwrap_or_default();
                        // `label (url)`, collapsed when the label *is* the url
                        // — which is what an autolink always is.
                        if label != url
                            && let Some(spans) = lines.last_mut()
                        {
                            spans.push(Span::styled(
                                format!(" ({url})"),
                                self.styles.link.add_modifier(Modifier::UNDERLINED),
                            ));
                        }
                    }
                }
                Event::Start(Tag::Emphasis) => {
                    stack.push(style);
                    style = self.styles.emph.add_modifier(Modifier::ITALIC);
                }
                Event::Start(Tag::Strong) => {
                    stack.push(style);
                    style = self.styles.strong.add_modifier(Modifier::BOLD);
                }
                Event::Start(Tag::Strikethrough) => {
                    stack.push(style);
                    style = self.styles.muted.add_modifier(Modifier::CROSSED_OUT);
                }
                Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                    stack.push(style);
                    style = self.styles.link_text.add_modifier(Modifier::UNDERLINED);
                    link = Some((
                        dest_url.to_string(),
                        lines.last().map(Vec::len).unwrap_or_default(),
                    ));
                }
                Event::Text(text) | Event::InlineHtml(text) | Event::Html(text) => {
                    push_verbatim(&mut lines, text, style);
                }
                Event::Code(code) => {
                    push_verbatim(&mut lines, code, self.styles.code);
                }
                Event::SoftBreak | Event::HardBreak => lines.push(Vec::new()),
                Event::TaskListMarker(checked) => {
                    let mark = if *checked { "[x] " } else { "[ ] " };
                    if let Some(spans) = lines.last_mut() {
                        spans.push(Span::styled(mark.to_owned(), self.styles.list_item));
                    }
                }
                Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
                // Unreachable: `is_inline` is what let this event in.
                Event::Start(_) | Event::End(_) | Event::Rule => {}
            }
        }

        lines
    }
}

/// Whether an event belongs to the run of inline content, as opposed to being
/// the block boundary that ends it.
fn is_inline(event: &Event<'_>) -> bool {
    match event {
        Event::Start(tag) => matches!(
            tag,
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. }
        ),
        Event::End(tag) => matches!(
            tag,
            TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::Image
        ),
        Event::Rule => false,
        _ => true,
    }
}

/// Appends `text` to the line under construction **verbatim** — intra-line
/// whitespace and all — splitting only where the text itself carries a newline.
fn push_verbatim(lines: &mut Vec<Vec<Span<'static>>>, text: &str, style: Style) {
    for (index, piece) in text.split('\n').enumerate() {
        if index > 0 {
            lines.push(Vec::new());
        }
        if piece.is_empty() {
            continue;
        }
        if let Some(spans) = lines.last_mut() {
            spans.push(Span::styled(piece.to_owned(), style));
        }
    }
}

/// The style a heading of `level` is painted in: H1 bold and underlined, the
/// rest bold, all of them `markdownHeading` (R12).
fn heading_style(styles: &Styles, level: HeadingLevel) -> Style {
    let style = styles.heading.add_modifier(Modifier::BOLD);

    match level {
        HeadingLevel::H1 => style.add_modifier(Modifier::UNDERLINED),
        _ => style,
    }
}

/// Hangs a list item's rendered lines off its marker: the marker leads the
/// first line, and every later line of the same item is pushed under it.
fn indent(lines: &mut [MdLine], marker: &str, style: Style) {
    let width = marker.width();

    for (index, line) in lines.iter_mut().enumerate() {
        if index == 0 {
            line.spans.insert(0, Span::styled(marker.to_owned(), style));
            line.hang += width;
        } else {
            line.prefix
                .insert(0, Span::styled(" ".repeat(width), Style::new()));
        }
    }
}

/// The name syntect should be asked for, after upstream's alias table.
fn resolve_language(info: &str) -> Option<String> {
    let name = info.trim().to_lowercase();
    if name.is_empty() {
        return None;
    }

    Some(
        INFO_ALIASES
            .iter()
            .find(|(alias, _)| *alias == name)
            .map_or(name, |(_, target)| (*target).to_owned()),
    )
}

/// Renders a code block's lines, highlighted when the info string names a
/// language syntect knows and flat `markdownCode` when it does not (R12).
fn highlight(code: &str, info: Option<&str>, styles: &Styles) -> Vec<MdLine> {
    let plain = |code: &str| {
        code.trim_end_matches('\n')
            .split('\n')
            .map(|line| {
                MdLine::new(
                    vec![Span::styled(line.to_owned(), styles.code)],
                    LineKind::Chop,
                )
            })
            .collect::<Vec<_>>()
    };

    let Some(name) = info.and_then(resolve_language) else {
        return plain(code);
    };
    let syntaxes = syntaxes();
    let Some(syntax) = syntaxes.find_syntax_by_token(&name) else {
        return plain(code);
    };

    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();

    for line in code.trim_end_matches('\n').split_inclusive('\n') {
        let Ok(operations) = state.parse_line(line, syntaxes) else {
            return plain(code);
        };

        let mut spans: Vec<Span<'static>> = Vec::new();
        for (text, operation) in ScopeRegionIterator::new(&operations, line) {
            if stack.apply(operation).is_err() {
                return plain(code);
            }
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(text.to_owned(), style_for(&stack, styles)));
        }

        lines.push(MdLine::new(spans, LineKind::Chop));
    }

    lines
}

/// The style a token carries, resolved from the innermost scope on the stack
/// that any rule matches. See [`SCOPE_RULES`].
fn style_for(stack: &ScopeStack, styles: &Styles) -> Style {
    for scope in stack.as_slice().iter().rev() {
        if let Some((_, slot)) = scope_rules()
            .iter()
            .find(|(rule, _)| rule.is_prefix_of(*scope))
        {
            return styles.scoped(*slot);
        }
    }

    styles.code
}

/// Lays a table out as a grid whose cells are clipped, never wrapped (D108).
fn table_lines(rows: &[Vec<Vec<Span<'static>>>], styles: &Styles) -> Vec<MdLine> {
    if rows.is_empty() {
        return Vec::new();
    }

    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| span_width(cell).min(TABLE_CELL_MAX))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();

    let mut lines = Vec::new();
    lines.push(border(&widths, '\u{250c}', '\u{252c}', '\u{2510}', styles));
    for (index, row) in rows.iter().enumerate() {
        lines.push(grid_row(row, &widths, styles));
        if index == 0 && rows.len() > 1 {
            lines.push(border(&widths, '\u{251c}', '\u{253c}', '\u{2524}', styles));
        }
    }
    lines.push(border(&widths, '\u{2514}', '\u{2534}', '\u{2518}', styles));

    lines
}

fn span_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|span| span.content.as_ref().width()).sum()
}

fn border(widths: &[usize], left: char, middle: char, right: char, styles: &Styles) -> MdLine {
    let mut text = String::from(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            text.push(middle);
        }
        text.extend(std::iter::repeat_n(RULE_GLYPH, width + 2));
    }
    text.push(right);

    MdLine::new(vec![Span::styled(text, styles.chrome)], LineKind::Clip)
}

fn grid_row(row: &[Vec<Span<'static>>], widths: &[usize], styles: &Styles) -> MdLine {
    let mut spans = Vec::new();

    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::styled("\u{2502} ".to_owned(), styles.chrome));
        let empty = Vec::new();
        let cell = row.get(index).unwrap_or(&empty);
        let mut used = 0;
        for span in cell {
            let room = width.saturating_sub(used);
            if room == 0 {
                break;
            }
            let (head, _) = split_at_width(span.content.as_ref(), room);
            let head_width = head.width();
            if head_width > room {
                break;
            }
            used += head_width;
            spans.push(Span::styled(head.to_owned(), span.style));
        }
        spans.push(Span::styled(
            " ".repeat(width.saturating_sub(used) + 1),
            styles.chrome,
        ));
    }
    spans.push(Span::styled("\u{2502}".to_owned(), styles.chrome));

    MdLine::new(spans, LineKind::Clip)
}

/// Stage 2: lays one logical line out at `width` columns.
pub(crate) fn wrap(line: &MdLine, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }

    match line.kind {
        LineKind::Flow => flow(line, width),
        LineKind::Chop => chop(line, width),
        LineKind::Clip => vec![finish(clip(line, width))],
        LineKind::Rule => vec![finish(rule(line, width))],
    }
}

/// The visual line a wrapped line starts from: the prefix, then the hanging
/// indent when this is not the first one.
fn opening(line: &MdLine, first: bool) -> (Vec<Span<'static>>, usize) {
    let mut spans = line.prefix.clone();
    let mut used = line.prefix_width();

    if !first && line.hang > 0 {
        spans.push(Span::raw(" ".repeat(line.hang)));
        used += line.hang;
    }

    (spans, used)
}

/// Greedy word wrap that keeps intra-line whitespace: only the whitespace a
/// break actually consumes disappears, which is what makes `a  b` render as
/// `a  b` (R12's verbatim rule) while `the quick brown fox` still breaks
/// between words.
fn flow(line: &MdLine, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let (mut spans, mut used) = opening(line, true);
    let mut base = used;
    let mut pending: Vec<Span<'static>> = Vec::new();
    let mut pending_width = 0usize;

    for (text, style) in atoms(&line.spans) {
        if text.chars().all(char::is_whitespace) {
            pending_width += text.width();
            pending.push(Span::styled(text, style));
            continue;
        }

        let mut rest = text;
        loop {
            if rest.width() <= width.saturating_sub(used + pending_width) {
                spans.append(&mut pending);
                used += pending_width + rest.width();
                pending_width = 0;
                spans.push(Span::styled(rest, style));
                break;
            }

            // Something is already on this line: break, and the whitespace the
            // break consumed goes with it.
            if used > base {
                out.push(finish(std::mem::take(&mut spans)));
                pending.clear();
                pending_width = 0;
                (spans, used) = opening(line, false);
                base = used;
                continue;
            }

            // The line is empty and the word still does not fit: chop it, so a
            // word wider than the viewport is shortened rather than lost.
            spans.append(&mut pending);
            used += pending_width;
            pending_width = 0;
            let (head, tail) = split_at_width(&rest, width.saturating_sub(used).max(1));
            let tail = tail.to_owned();
            spans.push(Span::styled(head.to_owned(), style));
            out.push(finish(std::mem::take(&mut spans)));
            (spans, used) = opening(line, false);
            base = used;
            if tail.is_empty() {
                break;
            }
            rest = tail;
        }
    }

    // Trailing whitespace is part of the line its author wrote.
    if !pending.is_empty() && used + pending_width <= width {
        spans.append(&mut pending);
    }
    out.push(finish(spans));

    out
}

/// Closes a visual line, merging neighbours that ended up in the same style.
///
/// The wrap works in atoms — a word, a whitespace run — so a settled line can
/// hold a dozen spans that paint identically. Merging them is what keeps a
/// buffer write per word from becoming the transcript's cost, and it is what
/// makes a line's spans read as the phrases they are.
fn finish(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut merged: Vec<Span<'static>> = Vec::with_capacity(spans.len());

    for span in spans {
        match merged.last_mut() {
            Some(last) if last.style == span.style => {
                last.content.to_mut().push_str(span.content.as_ref());
            }
            _ => merged.push(span),
        }
    }

    Line::from(merged)
}

/// Splits a span list into whitespace runs and word runs, each keeping the
/// style of the span it came from.
fn atoms(spans: &[Span<'static>]) -> Vec<(String, Style)> {
    let mut out = Vec::new();

    for span in spans {
        let text = span.content.as_ref();
        let mut start = 0;
        let mut space = None;

        for (index, character) in text.char_indices() {
            let is_space = character.is_whitespace();
            match space {
                Some(was) if was == is_space => {}
                Some(_) => {
                    out.push((text[start..index].to_owned(), span.style));
                    start = index;
                }
                None => {}
            }
            space = Some(is_space);
        }
        if start < text.len() {
            out.push((text[start..].to_owned(), span.style));
        }
    }

    out
}

/// Chops a line at the column, which is what code does instead of wrapping.
fn chop(line: &MdLine, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let (mut spans, mut used) = opening(line, true);

    for span in &line.spans {
        let mut rest = span.content.as_ref();

        while !rest.is_empty() {
            let room = width.saturating_sub(used);
            if room == 0 {
                out.push(finish(std::mem::take(&mut spans)));
                (spans, used) = opening(line, false);
                // A prefix wider than the viewport would spin here forever.
                if used >= width {
                    return out;
                }
                continue;
            }
            if rest.width() <= room {
                used += rest.width();
                spans.push(Span::styled(rest.to_owned(), span.style));
                break;
            }
            let (head, tail) = split_at_width(rest, room);
            used += head.width();
            spans.push(Span::styled(head.to_owned(), span.style));
            rest = tail;
        }
    }
    out.push(finish(spans));

    out
}

/// Truncates a line at the column, which is what a grid row does so that its
/// verticals stay vertical.
fn clip(line: &MdLine, width: usize) -> Vec<Span<'static>> {
    let (mut spans, mut used) = opening(line, true);

    for span in &line.spans {
        let room = width.saturating_sub(used);
        if room == 0 {
            break;
        }
        let text = span.content.as_ref();
        if text.width() <= room {
            used += text.width();
            spans.push(span.clone());
            continue;
        }
        let (head, _) = split_at_width(text, room);
        spans.push(Span::styled(head.to_owned(), span.style));
        break;
    }

    spans
}

/// Fills the line with the rule glyph, which is the only part of a horizontal
/// rule that needs to know how wide the screen is.
fn rule(line: &MdLine, width: usize) -> Vec<Span<'static>> {
    let (mut spans, used) = opening(line, true);
    let style = line
        .spans
        .first()
        .map_or_else(Style::new, |span| span.style);

    spans.push(Span::styled(
        std::iter::repeat_n(RULE_GLYPH, width.saturating_sub(used)).collect::<String>(),
        style,
    ));

    spans
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Modifier, Style};

    use super::{Document, MdLine, atoms, resolve_language, wrap};
    use crate::theme::{Theme, Themes};

    /// Renders `source` at `width`, as the transcript would.
    fn render(source: &str, width: usize) -> Vec<String> {
        rendered_with(source, width, &Theme::default())
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn rendered_with(
        source: &str,
        width: usize,
        theme: &Theme,
    ) -> Vec<ratatui::text::Line<'static>> {
        let mut document = Document::default();
        document.update(source, theme);

        document
            .lines()
            .flat_map(|line| wrap(line, width))
            .collect()
    }

    /// The logical lines a source segments and renders to, before any width.
    fn logical(source: &str) -> Vec<String> {
        let mut document = Document::default();
        document.update(source, &Theme::default());

        document.lines().map(MdLine::text).collect()
    }

    /// The style of the first span whose text is `needle`.
    fn style_of(source: &str, needle: &str, theme: &Theme) -> Style {
        rendered_with(source, 80, theme)
            .into_iter()
            .flat_map(|line| line.spans)
            .find(|span| span.content.as_ref() == needle)
            .unwrap_or_else(|| {
                let all: Vec<String> = rendered_with(source, 80, theme)
                    .into_iter()
                    .flat_map(|line| line.spans)
                    .map(|span| span.content.into_owned())
                    .collect();
                panic!("no span reads {needle:?} in {source:?}; spans are {all:?}")
            })
            .style
    }

    fn themed(name: &str) -> Theme {
        let mut themes = Themes::builtin();

        themes
            .select(name)
            .unwrap_or_else(|| panic!("{name} is builtin"))
    }

    // ---- the plain-text invariant -------------------------------------

    /// The ruled mechanism (R12): a source newline inside a paragraph is a
    /// hard break. Without it this renders as one re-flowed line and every
    /// snapshot of an assistant reply moves.
    #[test]
    fn a_newline_inside_a_paragraph_is_a_hard_line_break() {
        assert_eq!(
            render("first line\nsecond line", 80),
            vec!["first line".to_owned(), "second line".to_owned()]
        );
    }

    /// The other half of the invariant: text is not re-spaced on its way to
    /// the screen.
    #[test]
    fn text_renders_verbatim_including_the_whitespace_inside_it() {
        assert_eq!(
            render("two  spaces\tand a tab", 80),
            vec!["two  spaces\tand a tab".to_owned()]
        );
    }

    #[test]
    fn a_blank_line_between_paragraphs_stays_one_blank_line() {
        assert_eq!(
            render("one\n\ntwo", 20),
            vec!["one".to_owned(), String::new(), "two".to_owned()]
        );
    }

    /// What P1's wrap did, and what this has to keep doing.
    #[test]
    fn prose_wraps_on_word_boundaries_the_way_plain_text_did() {
        assert_eq!(
            render("the quick brown fox", 10),
            vec!["the quick".to_owned(), "brown fox".to_owned()]
        );
    }

    #[test]
    fn a_word_wider_than_the_viewport_is_chopped_not_dropped() {
        assert_eq!(
            render("abcdefghij", 4),
            vec!["abcd".to_owned(), "efgh".to_owned(), "ij".to_owned()]
        );
    }

    #[test]
    fn wrapping_measures_display_width_not_bytes() {
        assert_eq!(
            render("ああ ああ", 5),
            vec!["ああ".to_owned(), "ああ".to_owned()]
        );
    }

    #[test]
    fn a_zero_width_viewport_renders_nothing() {
        assert!(render("anything", 0).is_empty());
    }

    /// A theme that names no `markdown*` key paints exactly the body role, so
    /// the terminal theme's transcript is the one P1 shipped.
    #[test]
    fn a_theme_that_names_no_markdown_key_falls_back_to_the_body_role() {
        let theme = Theme::default();

        assert_eq!(
            theme.color("markdownText"),
            None,
            "the fixture must be bare"
        );
        assert_eq!(style_of("plain prose", "plain prose", &theme), theme.fg);
    }

    // ---- block segmentation -------------------------------------------

    #[test]
    fn every_top_level_block_kind_segments_on_its_own() {
        let source = "# heading\n\nparagraph\n\n- item\n\n> quoted\n\n```\ncode\n```\n\n---\n";

        assert_eq!(
            logical(source),
            vec![
                "heading".to_owned(),
                String::new(),
                "paragraph".to_owned(),
                String::new(),
                "- item".to_owned(),
                String::new(),
                "\u{258c} quoted".to_owned(),
                String::new(),
                "code".to_owned(),
                String::new(),
                String::new(),
            ],
            "the trailing empty line is the rule, which only a width can fill"
        );
    }

    #[test]
    fn a_table_segments_into_a_grid_with_a_header_rule() {
        let lines = logical("| a | bb |\n| - | -- |\n| 1 | 2 |\n");

        assert_eq!(
            lines,
            vec![
                "\u{250c}───┬────\u{2510}".to_owned(),
                "\u{2502} a \u{2502} bb \u{2502}".to_owned(),
                "\u{251c}───┼────\u{2524}".to_owned(),
                "\u{2502} 1 \u{2502} 2  \u{2502}".to_owned(),
                "\u{2514}───┴────\u{2518}".to_owned(),
            ]
        );
    }

    #[test]
    fn a_horizontal_rule_fills_whatever_width_it_is_drawn_at() {
        assert_eq!(render("---", 6), vec!["──────".to_owned()]);
    }

    // ---- style mapping -------------------------------------------------

    /// R12's table, one row per assertion. `aura` is the fixture because it
    /// names every markdown key with a distinct color.
    #[test]
    fn each_construct_takes_the_theme_key_ruled_for_it() {
        let theme = themed("aura");
        let color = |key: &str| {
            Style::new().fg(theme
                .color(key)
                .and_then(crate::theme::Rgba::color)
                .unwrap_or_else(|| panic!("aura names {key}")))
        };

        let cases: [(&str, &str, Style); 6] = [
            (
                "# top",
                "top",
                color("markdownHeading")
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            (
                "## next",
                "next",
                color("markdownHeading").add_modifier(Modifier::BOLD),
            ),
            (
                "**loud**",
                "loud",
                color("markdownStrong").add_modifier(Modifier::BOLD),
            ),
            (
                "*soft*",
                "soft",
                color("markdownEmph").add_modifier(Modifier::ITALIC),
            ),
            ("`code`", "code", color("markdownCode")),
            ("- item", "- ", color("markdownListItem")),
        ];

        for (source, needle, expected) in cases {
            assert_eq!(style_of(source, needle, &theme), expected, "{source}");
        }
    }

    #[test]
    fn a_blockquote_takes_a_dim_border_and_an_italic_body() {
        let theme = themed("aura");
        let quote = theme
            .color("markdownBlockQuote")
            .and_then(crate::theme::Rgba::color)
            .expect("aura names markdownBlockQuote");

        assert_eq!(style_of("> quoted", "\u{258c} ", &theme), theme.dim);
        assert_eq!(
            style_of("> quoted", "quoted", &theme),
            Style::new().fg(quote).add_modifier(Modifier::ITALIC)
        );
    }

    #[test]
    fn a_link_renders_its_label_then_its_url_and_collapses_when_they_match() {
        assert_eq!(
            render("[docs](https://ganja.dev)", 80),
            vec!["docs (https://ganja.dev)".to_owned()]
        );
        assert_eq!(
            render("<https://ganja.dev>", 80),
            vec!["https://ganja.dev".to_owned()],
            "an autolink's label is its url, so the url is not repeated"
        );
    }

    /// Conceal is always on (D107): no `#`, no `**`, no backticks reach the
    /// screen.
    #[test]
    fn emphasis_code_and_heading_markers_are_never_drawn() {
        for source in ["# heading", "**loud**", "`code`", "*soft*"] {
            let drawn = render(source, 80).join("");
            assert!(
                !drawn.contains('#') && !drawn.contains('*') && !drawn.contains('`'),
                "{source} drew its markers: {drawn:?}"
            );
        }
    }

    #[test]
    fn a_list_keeps_its_source_marker_and_hangs_its_wrapped_text() {
        assert_eq!(
            render("* alpha beta gamma", 9),
            vec![
                "* alpha".to_owned(),
                "  beta".to_owned(),
                "  gamma".to_owned()
            ],
            "the source bullet is kept and continuation lines hang under it"
        );
        assert_eq!(
            render("3. third\n4. fourth", 20),
            vec!["3. third".to_owned(), "4. fourth".to_owned()],
            "an ordered list counts from where the source started"
        );
    }

    #[test]
    fn a_nested_list_indents_under_the_item_that_holds_it() {
        assert_eq!(
            render("- outer\n  - inner", 20),
            vec!["- outer".to_owned(), "  - inner".to_owned()]
        );
    }

    #[test]
    fn a_task_item_renders_its_checkbox() {
        assert_eq!(
            render("- [x] done\n- [ ] todo", 20),
            vec!["- [x] done".to_owned(), "- [ ] todo".to_owned()]
        );
    }

    // ---- syntax highlighting -------------------------------------------

    #[test]
    fn a_fence_that_names_a_language_is_highlighted_by_scope() {
        let theme = themed("aura");
        let source = "```rust\nfn main() {}\n```";
        let keyword = theme
            .color("syntaxKeyword")
            .and_then(crate::theme::Rgba::color)
            .expect("aura names syntaxKeyword");

        assert_eq!(
            style_of(source, "fn", &theme),
            Style::new().fg(keyword).add_modifier(Modifier::ITALIC),
            "`fn` is `storage.type.function.rust`, which the table sends to syntaxKeyword"
        );
    }

    #[test]
    fn a_comment_is_italic_in_the_comment_slot() {
        let theme = themed("aura");
        let comment = theme
            .color("syntaxComment")
            .and_then(crate::theme::Rgba::color)
            .expect("aura names syntaxComment");

        assert_eq!(
            style_of("```rust\n// note\n```", "// note", &theme),
            Style::new().fg(comment).add_modifier(Modifier::ITALIC)
        );
    }

    #[test]
    fn a_fence_whose_language_is_unknown_or_absent_is_flat_code() {
        let theme = themed("aura");
        let code = theme
            .color("markdownCode")
            .and_then(crate::theme::Rgba::color)
            .expect("aura names markdownCode");

        for source in [
            "```\nfn main() {}\n```",
            "```notalanguage\nfn main() {}\n```",
            "    fn main() {}",
        ] {
            assert_eq!(
                style_of(source, "fn main() {}", &theme),
                Style::new().fg(code),
                "{source}"
            );
        }
    }

    #[test]
    fn upstreams_info_string_aliases_are_honored() {
        let cases = [
            ("udiff", Some("diff")),
            ("patch", Some("diff")),
            ("makefile", Some("make")),
            ("Rust", Some("rust")),
            ("", None),
            ("   ", None),
        ];

        for (info, expected) in cases {
            assert_eq!(resolve_language(info).as_deref(), expected, "{info:?}");
        }
    }

    // ---- the stage-1 cache ---------------------------------------------

    /// Streaming's whole point: the blocks above the one still arriving are
    /// never styled again, so syntect never re-runs over a settled fence.
    #[test]
    fn a_streamed_delta_styles_only_the_block_that_is_still_growing() {
        let theme = Theme::default();
        let mut document = Document::default();

        document.update("# title\n\n```rust\nfn main() {}\n```\n\nand th", &theme);
        assert_eq!(document.styled(), 3, "three blocks on the first pass");

        document.update(
            "# title\n\n```rust\nfn main() {}\n```\n\nand then some",
            &theme,
        );
        assert_eq!(
            document.styled(),
            4,
            "only the trailing paragraph may be styled again"
        );
    }

    #[test]
    fn an_unchanged_source_is_neither_parsed_nor_styled_again() {
        let theme = Theme::default();
        let mut document = Document::default();

        document.update("alpha\n\nbeta", &theme);
        document.update("alpha\n\nbeta", &theme);

        assert_eq!(document.styled(), 2, "two blocks, styled once each");
        assert_eq!(document.parsed(), 1, "the second call had nothing to read");
    }

    /// The cache holds styles, so it is as stale after a theme switch as the
    /// wrap is. Drop the revision from the key and this is what fails.
    #[test]
    fn a_theme_switch_restyles_every_block_the_cache_holds() {
        let mut themes = Themes::builtin();
        let first = themes.select("aura").expect("aura is builtin");
        let second = themes.select("gruvbox").expect("gruvbox is builtin");
        assert_ne!(
            first.revision(),
            second.revision(),
            "a switch has to change the revision, or nothing below is tested"
        );

        let mut document = Document::default();
        document.update("alpha\n\nbeta", &first);
        assert_eq!(document.styled(), 2);

        document.update("alpha\n\nbeta", &second);
        assert_eq!(
            document.styled(),
            4,
            "both blocks have to be styled again under the new palette"
        );
    }

    /// The two stages are independent: a resize is stage 2's business, and it
    /// must reach neither the parser nor the styler.
    #[test]
    fn a_width_change_alone_never_reparses_or_restyles_a_block() {
        let theme = Theme::default();
        let mut document = Document::default();
        document.update("alpha beta gamma delta", &theme);
        let (styled, parsed) = (document.styled(), document.parsed());

        for width in [10, 20, 5, 80] {
            // The transcript re-enters stage 1 on every wrap it has to redo,
            // which a resize is; the source is what says there is nothing to
            // do.
            document.update("alpha beta gamma delta", &theme);
            let lines: Vec<_> = document
                .lines()
                .flat_map(|line| wrap(line, width))
                .collect();
            assert!(!lines.is_empty(), "width {width} rendered nothing");
        }

        assert_eq!(document.parsed(), parsed, "a wrap is not a parse");
        assert_eq!(document.styled(), styled, "a wrap is not a re-style");
    }

    // ---- wrap internals -------------------------------------------------

    #[test]
    fn atoms_split_a_span_into_whitespace_and_word_runs() {
        let spans = vec![ratatui::text::Span::raw("a  b".to_owned())];
        let split: Vec<String> = atoms(&spans).into_iter().map(|(text, _)| text).collect();

        assert_eq!(split, vec!["a".to_owned(), "  ".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn a_code_line_is_chopped_at_the_column_rather_than_re_flowed() {
        assert_eq!(
            render("```\nlet x = 1;\n```", 5),
            vec!["let x".to_owned(), " = 1;".to_owned()],
            "code keeps its own spacing and simply runs out of room"
        );
    }

    #[test]
    fn a_table_row_is_clipped_rather_than_wrapped() {
        let lines = render("| alpha | beta |\n| - | - |\n| 1 | 2 |\n", 9);

        for line in &lines {
            assert!(
                line.chars().count() <= 9,
                "a grid row must not wrap, got {line:?}"
            );
        }
        assert!(
            lines.iter().any(|line| line.starts_with('\u{2502}')),
            "got {lines:?}"
        );
    }

    #[test]
    fn a_style_survives_the_wrap_it_is_carried_through() {
        let theme = themed("aura");
        let strong = theme
            .color("markdownStrong")
            .and_then(crate::theme::Rgba::color)
            .expect("aura names markdownStrong");
        let lines = rendered_with("plain **loud words here** plain", 12, &theme);
        let bold = Style::new().fg(strong).add_modifier(Modifier::BOLD);

        let emphasized: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style == bold)
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(
            emphasized, "loudwords here",
            "the bold run has to survive the break inside it, got {lines:?}"
        );
    }

    #[test]
    fn an_empty_source_renders_nothing() {
        assert!(render("", 20).is_empty());
        assert!(render("   \n\n  ", 20).is_empty(), "and neither does blank");
    }
}
