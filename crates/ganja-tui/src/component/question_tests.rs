use ganja_protocol::{QuestionId, QuestionInfo, QuestionOption};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::Question;
use crate::theme::Theme;

const AREA: Rect = Rect { x: 0, y: 0, width: 60, height: 14 };

fn question(options: Vec<QuestionOption>) -> Question {
    question_with_custom(options, None)
}

fn question_with_custom(options: Vec<QuestionOption>, custom: Option<bool>) -> Question {
    Question::new(
        QuestionId::from("que_1".to_owned()),
        QuestionInfo {
            question: "Which database should the service use?".to_owned(),
            header: "Database".to_owned(),
            options,
            multiple: None,
            custom,
        },
    )
}

fn rendered(question: &Question) -> String {
    let mut buffer = Buffer::empty(AREA);
    question.render(AREA, &mut buffer, &Theme::default());

    (0..AREA.height)
        .map(|row| (0..AREA.width).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_dialog_renders_the_question_header_labels_and_descriptions() {
    let screen = rendered(&question(vec![
        QuestionOption {
            label: "Postgres".to_owned(),
            description: "Relational database".to_owned(),
        },
        QuestionOption { label: "SQLite".to_owned(), description: "One local file".to_owned() },
    ]));

    for expected in [
        "Database",
        "Which database should the service use?",
        "Postgres",
        "Relational database",
        "SQLite",
        "One local file",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
    }
}

#[test]
fn a_question_with_custom_unset_offers_a_free_text_row() {
    let mut question = question(vec![QuestionOption {
        label: "Postgres".to_owned(),
        description: "Relational database".to_owned(),
    }]);

    assert!(question.offers_custom());
    assert!(rendered(&question).contains("Type your own answer"));

    question.move_selection(1);

    assert!(question.on_custom_row());
    assert_eq!(question.selected_label(), None);
}

#[test]
fn the_typed_string_arrives_as_the_answer() {
    let mut question = question(vec![QuestionOption {
        label: "Postgres".to_owned(),
        description: "Relational database".to_owned(),
    }]);
    question.move_selection(1);

    assert_eq!(question.submit(), None);
    assert!(question.is_editing());
    for character in "  SQLite WAL  ".chars() {
        question.push(character);
    }

    assert_eq!(question.submit(), Some("SQLite WAL".to_owned()));
    assert!(!question.is_editing());
}

#[test]
fn empty_custom_text_falls_back_to_the_highlighted_option() {
    let mut question = question(vec![QuestionOption {
        label: "Postgres".to_owned(),
        description: "Relational database".to_owned(),
    }]);
    question.move_selection(1);
    assert_eq!(question.submit(), None);
    for character in "   ".chars() {
        question.push(character);
    }

    // This is upstream's empty-text branch: it drops the custom answer instead of replying it.
    assert_eq!(question.submit(), None);
    assert!(!question.is_editing());
    assert_eq!(question.input(), "");

    question.move_selection(-1);

    assert_eq!(question.submit(), Some("Postgres".to_owned()));
}

#[test]
fn custom_false_hides_the_free_text_row() {
    let mut question = question_with_custom(
        vec![
            QuestionOption {
                label: "Postgres".to_owned(),
                description: "Relational database".to_owned(),
            },
            QuestionOption { label: "SQLite".to_owned(), description: "One local file".to_owned() },
        ],
        Some(false),
    );

    assert!(!question.offers_custom());
    assert!(!rendered(&question).contains("Type your own answer"));

    question.move_selection(isize::MAX);

    assert_eq!(question.selected_label(), Some("SQLite"));
}

#[test]
fn a_question_with_no_options_and_no_custom_row_is_dismissal_only() {
    let question = question_with_custom(Vec::new(), Some(false));

    assert_eq!(question.selected_label(), None);
    assert!(
        rendered(&question).contains("no selectable options; Esc dismisses"),
        "{}",
        rendered(&question)
    );
}
