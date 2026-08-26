use std::path::Path;

use super::{LANGUAGE_EXTENSIONS, language_id};

#[test]
fn a_known_extension_names_its_language() {
    let cases = [
        ("/tmp/main.rs", "rust"),
        ("/tmp/main.go", "go"),
        ("/tmp/app.tsx", "typescriptreact"),
        ("/tmp/build.zig", "zig"),
        ("/tmp/notes.md", "markdown"),
        ("/tmp/main.c++", "cpp"),
    ];

    for (path, expected) in cases {
        assert_eq!(language_id(Path::new(path)), expected, "for {path}");
    }
}

#[test]
fn an_unclaimed_extension_is_plaintext() {
    let cases = [
        // No entry anywhere in the table.
        "/tmp/notes.wat",
        // A dotfile has no extension, so there is nothing to look up.
        "/tmp/.bashrc",
        // Neither has a file with no dot at all.
        "/tmp/Makefile",
        // The table is case-sensitive, as upstream's object lookup is.
        "/tmp/MAIN.RS",
    ];

    for path in cases {
        assert_eq!(language_id(Path::new(path)), "plaintext", "for {path}");
    }
}

#[test]
fn a_double_extension_resolves_on_its_last_dot() {
    // `.html.erb` is in the table and still unreachable, because the
    // lookup asks for `.erb` — which is also in the table, and answers.
    assert_eq!(language_id(Path::new("/tmp/show.html.erb")), "erb");
}

#[test]
fn the_table_holds_one_answer_per_extension() {
    let mut seen = std::collections::HashSet::new();
    for (extension, _) in LANGUAGE_EXTENSIONS {
        assert!(
            seen.insert(*extension),
            "{extension} appears twice, so which language it names depends on scan order"
        );
    }
}
