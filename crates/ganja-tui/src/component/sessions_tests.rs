use ganja_core::{SessionId, SessionInfo, storage::VERSION};
use ganja_protocol::Usage;
use ratatui::{buffer::Buffer, layout::Rect};

use super::{DAY, HOUR, MINUTE, Sessions, age};
use crate::theme::Theme;

/// The moment every fixture is aged against; sessions are placed relative
/// to it so a test asserts on the interval it asked for.
const NOW: u64 = 1_000 * DAY;

fn info(id: &str, title: Option<&str>, updated: u64, tokens: u64) -> SessionInfo {
    SessionInfo {
        effort: None,
        id: SessionId::from(id.to_owned()),
        version: VERSION,
        title: title.map(str::to_owned),
        created: 0,
        updated,
        usage: Usage {
            input_tokens: tokens,
            ..Usage::default()
        },
        context_tokens: 0,
        summary: None,
        agent: None,
        model: None,
        activated_tools: std::collections::BTreeSet::new(),
        parent: None,
        revert: None,
    }
}

/// Two sessions, newest first, as the engine lists them.
fn sessions() -> Sessions {
    Sessions::new(
        vec![
            info(
                "ses_newer",
                Some("porting storage"),
                NOW - 5 * MINUTE,
                1_234,
            ),
            info("ses_older", None, NOW - 3 * HOUR, 42),
        ],
        NOW,
    )
}

fn rendered(sessions: &Sessions, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    sessions.render(area, &mut buffer, &Theme::default());

    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_list_shows_what_a_person_chooses_by() {
    let screen = rendered(&sessions(), Rect::new(0, 0, 80, 20));

    assert!(screen.contains("porting storage"), "got:\n{screen}");
    assert!(screen.contains("5m ago"), "got:\n{screen}");
    assert!(screen.contains("3h ago"), "got:\n{screen}");
    assert!(screen.contains("1.2k tokens"), "got:\n{screen}");
    assert!(screen.contains("42 tokens"), "got:\n{screen}");
}

/// A session that never earned a title still has to be identifiable, and
/// by the same string `--session` would take.
#[test]
fn an_untitled_session_is_listed_by_its_id() {
    let screen = rendered(&sessions(), Rect::new(0, 0, 80, 20));

    assert!(screen.contains("ses_older"), "got:\n{screen}");
}

#[test]
fn a_title_of_only_whitespace_falls_back_to_the_id_too() {
    let sessions = Sessions::new(vec![info("ses_blank", Some("   "), NOW, 0)], NOW);

    assert!(
        rendered(&sessions, Rect::new(0, 0, 80, 20)).contains("ses_blank"),
        "a blank title is no title"
    );
}

#[test]
fn the_selection_starts_on_the_newest_and_moves_within_the_list() {
    let mut sessions = sessions();
    assert_eq!(
        sessions.selected().map(|info| info.id.as_str()),
        Some("ses_newer")
    );

    sessions.move_selection(1);
    assert_eq!(
        sessions.selected().map(|info| info.id.as_str()),
        Some("ses_older")
    );

    // Clamped at both ends rather than wrapping around.
    sessions.move_selection(9);
    assert_eq!(
        sessions.selected().map(|info| info.id.as_str()),
        Some("ses_older")
    );
    sessions.move_selection(-9);
    assert_eq!(
        sessions.selected().map(|info| info.id.as_str()),
        Some("ses_newer")
    );
}

#[test]
fn the_marker_follows_the_selection() {
    let mut sessions = sessions();
    let first = rendered(&sessions, Rect::new(0, 0, 80, 20));
    sessions.move_selection(1);
    let second = rendered(&sessions, Rect::new(0, 0, 80, 20));

    assert!(first.contains("> porting storage"), "got:\n{first}");
    assert!(second.contains("> ses_older"), "got:\n{second}");
    assert!(
        !second.contains("> porting storage"),
        "only one row is selected:\n{second}"
    );
}

/// More sessions than rows: the list has to move under the selection, or
/// the user cannot reach what they are selecting.
#[test]
fn a_selection_below_the_fold_scrolls_the_list_to_it() {
    let entries = (0..40_u32)
        .map(|index| {
            info(
                &format!("ses_{index:02}"),
                Some(&format!("session number {index:02}")),
                NOW - u64::from(index) * MINUTE,
                0,
            )
        })
        .collect();
    let mut sessions = Sessions::new(entries, NOW);
    let area = Rect::new(0, 0, 80, 20);

    let top = rendered(&sessions, area);
    assert!(top.contains("session number 00"), "got:\n{top}");
    assert!(!top.contains("session number 39"), "got:\n{top}");

    sessions.move_selection(39);
    let bottom = rendered(&sessions, area);

    assert!(
        bottom.contains("> session number 39"),
        "the selection must be on screen:\n{bottom}"
    );
    assert!(
        !bottom.contains("session number 00"),
        "the list should have scrolled:\n{bottom}"
    );
}

#[test]
fn a_title_too_wide_for_the_column_is_cut_rather_than_wrapped() {
    let sessions = Sessions::new(vec![info("ses_1", Some(&"wide ".repeat(40)), NOW, 0)], NOW);

    let screen = rendered(&sessions, Rect::new(0, 0, 60, 20));

    for line in screen.lines() {
        assert!(
            line.chars().count() <= 60,
            "a row must not overflow the dialog: {line:?}"
        );
    }
    assert!(screen.contains("wide"), "got:\n{screen}");
}

#[test]
fn ages_round_to_the_unit_they_are_reported_in() {
    assert_eq!(age(NOW, NOW), "just now");
    assert_eq!(age(NOW, NOW - 59 * 1_000), "just now");
    assert_eq!(age(NOW, NOW - 5 * MINUTE), "5m ago");
    assert_eq!(age(NOW, NOW - 3 * HOUR), "3h ago");
    assert_eq!(age(NOW, NOW - 2 * DAY), "2d ago");
    // A clock that moved backwards between runs.
    assert_eq!(age(NOW, NOW + DAY), "just now");
}

#[test]
fn an_empty_list_has_nothing_selected_and_does_not_panic() {
    let sessions = Sessions::new(Vec::new(), NOW);

    assert!(sessions.is_empty());
    assert!(sessions.selected().is_none());
    rendered(&sessions, Rect::new(0, 0, 80, 20));
}

#[test]
fn a_zero_area_draws_nothing_and_does_not_panic() {
    let screen = rendered(&sessions(), Rect::new(0, 0, 0, 0));

    assert!(
        screen.is_empty(),
        "a zero area has no cell to hold: {screen}"
    );
}

/// The same protection the permission dialog is pinned for, on the other
/// piece of chrome that renders text the model chose: a session's title is
/// written by a titling request, so it must not be able to repaint the
/// screen the user is choosing from.
#[test]
fn an_escape_sequence_in_a_title_never_reaches_the_buffer() {
    let sessions = Sessions::new(
        vec![info(
            "ses_1",
            Some("\u{1b}[2J\u{1b}[31mchoose me\u{7}"),
            NOW,
            0,
        )],
        NOW,
    );

    let screen = rendered(&sessions, Rect::new(0, 0, 80, 20));
    let leaked: Vec<char> = screen
        .chars()
        .filter(|character| *character != '\n' && character.is_control())
        .collect();

    assert!(
        leaked.is_empty(),
        "control characters reached the buffer: {leaked:?}\n{screen}"
    );
    assert!(
        screen.contains("choose me"),
        "the printable remainder still has to render:\n{screen}"
    );
}
