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

fn rendered_with(source: &str, width: usize, theme: &Theme) -> Vec<ratatui::text::Line<'static>> {
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
