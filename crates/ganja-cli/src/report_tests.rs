use super::*;

/// The escape a repaint would be built out of never reaches the terminal, and
/// what stands in its place is one character wide, so the columns still line
/// up.
#[test]
fn a_key_carrying_control_characters_renders_as_replacements() {
    let rendered = printable("mcp.\u{1b}[2Kevil\u{7}");

    assert_eq!(rendered, "mcp.\u{fffd}[2Kevil\u{fffd}");
    assert!(!rendered.contains('\u{1b}'), "{rendered}");
    assert!(!rendered.contains('\u{7}'), "{rendered}");
}

/// A carriage return is what rewrites a line that was already printed, so it
/// goes the same way an escape does — as does the newline that would end the
/// row early.
#[test]
fn a_row_cannot_return_to_the_start_of_its_own_line() {
    assert_eq!(
        printable("safe\rrewritten\nnext"),
        "safe\u{fffd}rewritten\u{fffd}next"
    );
}

/// Accepted, and said so out loud: the columns are space-aligned, so a tab was
/// always going to break them.
#[test]
fn a_tab_in_ordinary_text_is_replaced_too() {
    assert_eq!(printable("./run.sh\t--flag"), "./run.sh\u{fffd}--flag");
}

/// Text with nothing to neutralize comes out as itself, multi-byte characters
/// included — the filter is about control characters and not about ASCII.
#[test]
fn text_with_no_control_characters_is_returned_unchanged() {
    assert_eq!(
        printable("[[hooks.PreToolUse]] 日本語"),
        "[[hooks.PreToolUse]] 日本語"
    );
}

/// One character in, one character out, which is what keeps the width
/// `print_table` measured from the unfiltered key correct for the filtered
/// one.
#[test]
fn filtering_never_changes_how_wide_a_cell_is() {
    let raw = "a\u{1b}b\tc\rd";

    assert_eq!(printable(raw).chars().count(), raw.chars().count());
}
