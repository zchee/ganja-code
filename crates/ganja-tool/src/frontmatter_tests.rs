use super::{fields, split};

/// The reader's own answers, asked of it directly rather than through a
/// file kind: both callers depend on every one of these, and a case that
/// only one of them exercises is a case the other silently loses.
#[test]
fn the_shapes_a_frontmatter_may_be_written_in_read_as_written() {
    let cases = [
        (
            "quoted values",
            "---\na: \"one\"\nb: 'two'\n---\nbody",
            "one",
        ),
        (
            "an unquoted colon, kept whole",
            "---\na: Use when: it matches\n---\nbody",
            "Use when: it matches",
        ),
        (
            "a literal block scalar",
            "---\na: |\n  first\n  second\n---\nbody",
            "first\nsecond",
        ),
        (
            "a folded block scalar",
            "---\na: >-\n  first\n  second\n---\nbody",
            "first second",
        ),
        (
            "a list under a key, which is not read",
            "---\na:\n  - one\n  - two\n---\nbody",
            "",
        ),
    ];

    for (what, text, expected) in cases {
        let (frontmatter, body) = split(text).expect("the fixture opens with a fence");
        assert_eq!(
            fields(frontmatter).get("a").map(String::as_str),
            Some(expected),
            "{what}"
        );
        assert_eq!(body, "body", "{what}");
    }
}

/// A byte-order mark, carriage returns, a `---` inside a value, and a
/// fence that never closes.
#[test]
fn the_fence_is_found_or_honestly_missed() {
    assert_eq!(
        split("\u{feff}---\r\nname: a\r\n---\r\nbody"),
        Some(("name: a\r", "body")),
        "a mark and CRLF are what another platform's editor leaves"
    );
    let (frontmatter, body) =
        split("---\na: one --- two\n---\nbody").expect("the fence closes on its own line");
    assert_eq!(
        fields(frontmatter).get("a").map(String::as_str),
        Some("one --- two")
    );
    assert_eq!(body, "body");
    assert_eq!(split("---\nname: a\nbody"), None, "an unterminated fence");
    assert_eq!(split("# just markdown\n"), None, "no frontmatter at all");
}
