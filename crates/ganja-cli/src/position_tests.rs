use super::*;

#[test]
fn the_first_byte_of_a_file_is_line_one_column_one() {
    assert_eq!(at("key = 1\n", 0), (1, 1));
    assert_eq!(line_of("key = 1\n", 0), 1);
}

#[test]
fn a_column_counts_characters_rather_than_bytes() {
    // Four characters before the offset, spelled in seven bytes.
    let text = "k = \"日本語\"\n";
    let offset = text.find('本').expect("the fixture holds it");

    assert_eq!(at(text, offset), (1, 7));
}

#[test]
fn an_offset_past_the_end_lands_on_the_last_position_rather_than_panicking() {
    assert_eq!(at("a\nb", 900), (2, 2));
}

#[test]
fn a_span_less_error_renders_as_its_message_alone() {
    assert_eq!(located("unbalanced", None, "whatever"), "unbalanced");
}

#[test]
fn a_located_message_names_the_line_and_the_column() {
    let text = "first = 1\nsecond = oops\n";
    let offset = text.find("oops").expect("the fixture holds it");

    assert_eq!(
        located("invalid string", Some(offset..offset + 4), text),
        "invalid string at line 2, column 10"
    );
}

/// The whole point of the helper: what it renders is the two facts, and never
/// the bytes they point at.
#[test]
fn a_located_message_never_reproduces_the_line_it_points_at() {
    let text = "token = NEVER-PRINT-ME\n";
    let offset = text.find("NEVER").expect("the fixture holds it");
    let rendered = located("invalid string", Some(offset..text.len()), text);

    assert!(rendered.contains("line 1, column 9"), "{rendered}");
    assert!(!rendered.contains("NEVER-PRINT-ME"), "{rendered}");
}
