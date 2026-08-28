use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{Files, Row};
use crate::mention::Fragment;
use crate::theme::Theme;

fn fragment(text: &str) -> Fragment {
    Fragment { row: 0, start: 0, text: text.to_owned() }
}

fn files(paths: &[&str]) -> Files {
    Files::new(
        fragment("lib"),
        paths.iter().map(|path| Row::File((*path).to_owned())).collect(),
        None,
    )
}

fn rendered(files: &Files, anchor: Rect, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    files.render(anchor, &mut buffer, &Theme::default());

    (0..area.height)
        .map(|row| (0..area.width).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Upstream says it twice in comments: the walk's order is the order.
#[test]
fn the_rows_keep_the_order_the_walk_returned_them_in() {
    let walked = ["zebra/lib.rs", "alpha/lib.rs", "middle/lib.rs"];
    let files = files(&walked);

    let screen = rendered(&files, Rect::new(0, 10, 40, 5), Rect::new(0, 0, 40, 16));
    let rows: Vec<&str> = screen.lines().filter(|line| line.contains("lib.rs")).collect();

    for (row, expected) in rows.iter().zip(walked.iter()) {
        assert!(row.contains(expected), "got:\n{screen}");
    }
    assert_eq!(rows.len(), walked.len(), "got:\n{screen}");
}

#[test]
fn the_cursor_starts_on_the_first_row_and_clamps_at_both_ends() {
    let mut files = files(&["a/lib.rs", "b/lib.rs"]);
    assert_eq!(files.selected(), Some(&Row::File("a/lib.rs".to_owned())));

    files.move_selection(1);
    assert_eq!(files.selected(), Some(&Row::File("b/lib.rs".to_owned())));

    files.move_selection(9);
    assert_eq!(files.selected(), Some(&Row::File("b/lib.rs".to_owned())));
    files.move_selection(-9);
    assert_eq!(files.selected(), Some(&Row::File("a/lib.rs".to_owned())));
}

#[test]
fn a_fragment_nothing_matches_says_so_instead_of_drawing_an_empty_box() {
    let files = files(&[]);
    assert!(files.is_empty());
    assert_eq!(files.selected(), None);

    let screen = rendered(&files, Rect::new(0, 10, 40, 5), Rect::new(0, 0, 40, 16));
    assert!(screen.contains("no matches"), "{screen}");
}

#[test]
fn the_menu_draws_above_the_editor_it_is_anchored_to() {
    let anchor = Rect::new(0, 10, 40, 5);
    let screen = rendered(&files(&["src/lib.rs"]), anchor, Rect::new(0, 0, 40, 16));

    let row = screen
        .lines()
        .position(|line| line.contains("src/lib.rs"))
        .expect("the path should be on screen");
    assert!(
        row < usize::from(anchor.y),
        "the menu should sit above row {}, found it at {row}:\n{screen}",
        anchor.y
    );
}

#[test]
fn an_editor_with_no_room_above_it_gets_no_menu() {
    let screen = rendered(&files(&["src/lib.rs"]), Rect::new(0, 0, 40, 5), Rect::new(0, 0, 40, 8));

    assert!(screen.trim().is_empty(), "nothing should have been drawn:\n{screen}");
}

/// The list depends on the fragment alone, which is what lets the app skip
/// a walk when nothing about the mention changed.
#[test]
fn a_list_answers_the_fragment_it_was_opened_for_and_no_other() {
    let files = files(&["src/lib.rs"]);

    assert!(files.answers(&fragment("lib")));
    assert!(!files.answers(&fragment("li")));
    assert!(!files.answers(&Fragment { row: 1, start: 0, text: "lib".to_owned() }));
}

/// AC-23: roster and live-session rows carry their own label, a lead
/// teammate marked, a colliding session showing its stem, a shadowed
/// session marked.
#[test]
fn roster_and_session_rows_carry_their_own_labels() {
    let files = Files::new(
        fragment("work"),
        vec![
            Row::Teammate { name: "worker".to_owned(), lead: true },
            Row::Session {
                name: "worker".to_owned(),
                cwd: "/work/a".into(),
                stem: "0198c1a2".to_owned(),
                address: "uds:/tmp/ganja-0/0198c1a2.sock".to_owned(),
                colliding: true,
                shadowed: false,
            },
            Row::Session {
                name: "backend".to_owned(),
                cwd: "/work/b".into(),
                stem: "0299d2b3".to_owned(),
                address: "uds:/tmp/ganja-0/0299d2b3.sock".to_owned(),
                colliding: false,
                shadowed: true,
            },
        ],
        None,
    );

    let screen = rendered(&files, Rect::new(0, 10, 80, 5), Rect::new(0, 0, 80, 16));

    assert!(screen.contains("(teammate, lead)"), "{screen}");
    assert!(screen.contains("(session · 0198c1a2) /work/a"), "{screen}");
    assert!(screen.contains("(session) /work/b — shadowed by a file"), "{screen}");
}
