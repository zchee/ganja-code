use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
};

use super::{Editor, Mode};
use crate::theme::{Theme, Themes};

fn drawn(editor: &Editor) -> String {
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 5,
    };

    let mut buffer = Buffer::empty(AREA);
    editor.render(AREA, &mut buffer);

    (0..AREA.height)
        .map(|row| {
            (0..AREA.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn typing(editor: &mut Editor, text: &str) {
    for character in text.chars() {
        editor.input(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
}

/// **D518.** The hint is paint, not text: it shows dim after the typed
/// name, and the buffer a submit would read never contains it.
#[test]
fn the_hint_draws_after_the_text_and_never_enters_the_buffer() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "/team");
    editor.set_hint(Some("list | spawn <name>".to_owned()));

    let screen = drawn(&editor);
    assert!(
        screen.contains("/team list | spawn <name>"),
        "got:\n{screen}"
    );
    assert_eq!(editor.text(), "/team");
}

/// **D518.** A hint wider than the box clips at the border instead of
/// wrapping onto the next row.
#[test]
fn a_hint_wider_than_the_box_is_clipped_not_wrapped() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "/team");
    editor.set_hint(Some("x".repeat(80)));

    let screen = drawn(&editor);
    let rows: Vec<&str> = screen.lines().collect();
    assert!(rows[1].contains("xxx"), "got:\n{screen}");
    assert!(!rows[2].contains('x'), "got:\n{screen}");
    // The border column survives the clip.
    assert!(rows[1].ends_with('│'), "got:\n{screen}");
}

/// **D518.** Clearing the hint clears the paint.
#[test]
fn a_cleared_hint_paints_nothing() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "/team");
    editor.set_hint(Some("list".to_owned()));
    editor.set_hint(None);

    let screen = drawn(&editor);
    assert!(!screen.contains("list"), "got:\n{screen}");
}

#[test]
fn an_empty_editor_has_nothing_to_submit() {
    assert_eq!(Editor::new(&Theme::default()).prompt(), None);
}

#[test]
fn a_whitespace_only_editor_has_nothing_to_submit() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "   ");

    assert_eq!(editor.prompt(), None);
}

#[test]
fn typed_text_becomes_the_prompt_and_clearing_takes_it_back() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "hello");

    assert_eq!(editor.prompt().as_deref(), Some("hello"));

    editor.clear();
    assert_eq!(editor.prompt(), None);
}

/// The gate on every key that means two things.
#[test]
fn an_editor_holding_only_spaces_is_not_empty_even_though_it_has_nothing_to_submit() {
    let mut editor = Editor::new(&Theme::default());
    assert!(editor.is_empty(), "a fresh editor is empty");

    typing(&mut editor, "  ");

    assert!(
        !editor.is_empty(),
        "typed spaces are text, however unsubmittable"
    );
    assert_eq!(editor.prompt(), None);
}

#[test]
fn the_cursor_reports_where_typing_left_it_and_home_and_end_move_it() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "first");
    editor.insert_newline();
    typing(&mut editor, "second");

    assert_eq!(editor.cursor(), (1, 6));

    editor.line_home();
    assert_eq!(editor.cursor(), (1, 0), "home moves within the line");

    editor.line_end();
    assert_eq!(editor.cursor(), (1, 6));
}

#[test]
fn the_whole_buffer_reads_back_with_its_whitespace_intact() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "/models ");

    assert_eq!(editor.text(), "/models ");
    assert_eq!(
        editor.prompt().as_deref(),
        Some("/models "),
        "prompt only discounts whitespace, it does not strip it"
    );
}

#[test]
fn a_newline_keeps_both_lines_in_one_prompt() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "first");
    editor.insert_newline();
    typing(&mut editor, "second");

    assert_eq!(editor.prompt().as_deref(), Some("first\nsecond"));
}

/// The widget's own default underlines the cursor's whole line, which
/// would decorate every character as it is typed — nothing upstream or
/// Claude Code draws, so no cell here may carry it.
#[test]
fn the_line_being_typed_is_not_underlined() {
    use ratatui::style::Modifier;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 5,
    };

    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "no decoration");

    let mut buffer = Buffer::empty(AREA);
    editor.render(AREA, &mut buffer);

    for row in 0..AREA.height {
        for column in 0..AREA.width {
            assert!(
                !buffer[(column, row)]
                    .modifier
                    .contains(Modifier::UNDERLINED),
                "cell ({column}, {row}) is underlined"
            );
        }
    }
}

/// The one component whose styles are set once rather than read per frame,
/// which is why a theme switch has to reach in and repaint it.
#[test]
fn restyling_repaints_the_box_without_disturbing_what_is_typed() {
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 30,
        height: 5,
    };

    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "a draft mid-switch");

    let mut buffer = Buffer::empty(AREA);
    editor.render(AREA, &mut buffer);
    let before = buffer[(0, 0)].fg;

    editor.restyle(
        &Themes::builtin()
            .select("gruvbox")
            .expect("gruvbox is builtin"),
    );
    editor.render(AREA, &mut buffer);

    assert_ne!(
        before,
        buffer[(0, 0)].fg,
        "the border kept the styles it was built with"
    );
    assert_eq!(
        editor.prompt().as_deref(),
        Some("a draft mid-switch"),
        "restyling must not touch the buffer"
    );
}

/// The mode is what the box says it is: a person about to press Enter has
/// to be able to see whether it reaches the model or the shell.
#[test]
fn the_box_says_which_of_the_two_things_the_next_enter_does() {
    let mut editor = Editor::new(&Theme::default());
    assert_eq!(editor.mode(), Mode::Prompt);

    let prompting = drawn(&editor);
    assert!(prompting.contains("message"), "got:\n{prompting}");
    assert!(prompting.contains("Ask ganja"), "got:\n{prompting}");

    editor.set_mode(Mode::Shell);

    let shelling = drawn(&editor);
    assert_eq!(editor.mode(), Mode::Shell);
    assert!(shelling.contains("Shell"), "got:\n{shelling}");
    assert!(shelling.contains("Run a command"), "got:\n{shelling}");
    assert!(!shelling.contains("message"), "got:\n{shelling}");
}

/// A theme switch must not quietly put the prompt chrome back on a box
/// that is running shell commands.
#[test]
fn restyling_keeps_the_mode_the_box_is_in() {
    let mut editor = Editor::new(&Theme::default());
    editor.set_mode(Mode::Shell);

    editor.restyle(
        &Themes::builtin()
            .select("gruvbox")
            .expect("gruvbox is builtin"),
    );

    assert!(drawn(&editor).contains("Shell"), "got:\n{}", drawn(&editor));
}

/// Flipping into shell mode and back leaves the text alone: upstream runs
/// the raw buffer, so what was typed before the flip is part of it.
#[test]
fn changing_mode_does_not_disturb_what_is_typed() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "ls -la");

    editor.set_mode(Mode::Shell);
    assert_eq!(editor.prompt().as_deref(), Some("ls -la"));

    editor.set_mode(Mode::Prompt);
    assert_eq!(editor.prompt().as_deref(), Some("ls -la"));
}

/// The cursor is the terminal's, not a painted cell: the render reports
/// the cell the widget put its cursor on — the blank one before the
/// placeholder, the one after typed text, the character `Home` moves onto
/// — with nothing painted there, and nothing on an area too small for a
/// cursor.
#[test]
fn the_render_reports_the_cursor_cell_and_paints_nothing_on_it() {
    use ratatui::style::Modifier;
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 5,
    };
    let place = |editor: &Editor| {
        let mut buffer = Buffer::empty(AREA);
        let at = editor.render(AREA, &mut buffer);
        let unpainted = at.is_none_or(|(x, y)| {
            !buffer[(x, y)]
                .modifier
                .intersects(Modifier::REVERSED | Modifier::SLOW_BLINK)
        });
        assert!(unpainted, "nothing is painted on the cursor cell");
        at
    };
    let mut editor = Editor::new(&Theme::default());
    assert_eq!(
        place(&editor),
        Some((1, 1)),
        "empty: before the placeholder"
    );

    typing(&mut editor, "ok");
    assert_eq!(place(&editor), Some((3, 1)), "after the typed text");
    editor.line_home();
    assert_eq!(place(&editor), Some((1, 1)), "on the first character");

    let mut tiny = Buffer::empty(Rect::new(0, 0, 2, 2));
    assert_eq!(
        editor.render(Rect::new(0, 0, 2, 2), &mut tiny),
        None,
        "an area with no inside has no cursor cell"
    );
}

#[test]
fn replacing_the_buffer_leaves_the_cursor_at_the_end() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "a draft");

    editor.set_text("what the editor wrote\nover two lines");

    assert_eq!(
        editor.prompt().as_deref(),
        Some("what the editor wrote\nover two lines")
    );
    assert_eq!(editor.cursor(), (1, "over two lines".chars().count()));
}

#[test]
fn replacing_the_buffer_can_put_the_cursor_where_the_caller_asks() {
    let mut editor = Editor::new(&Theme::default());

    editor.set_text_at("look at @src/lib.rs and say why", 0, 19);

    assert_eq!(editor.cursor(), (0, 19));
    assert_eq!(
        editor.prompt().as_deref(),
        Some("look at @src/lib.rs and say why")
    );
}

#[test]
fn replacing_the_buffer_with_nothing_empties_it() {
    let mut editor = Editor::new(&Theme::default());
    typing(&mut editor, "a draft");

    editor.set_text("");

    assert!(editor.is_empty());
    assert_eq!(editor.cursor(), (0, 0));
}
