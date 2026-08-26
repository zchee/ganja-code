use std::path::Path;

use super::{Fragment, attachable, classify_drop, scan, split_range, token, trigger};

/// `crates/ganja-core/src/command.rs::mentions` is this scan spelled again
/// across the core/TUI dependency boundary. If they drift, a token the
/// composer attaches can be ignored by a template, or the reverse; this
/// table pins the one grammar both sites must keep.
#[test]
fn command_templates_and_the_composer_scan_mentions_the_same_way() {
    let cases = [
        "@a.rs",
        "compare @a.rs with @b.rs and @dir/c.rs",
        "first @a.rs\nsecond @b.rs",
        "ask @alice about it",
        "mail me@example.com",
        "an @ on its own",
        "",
        "@a.rs#5",
        "@a.rs#5-9",
        "@a.rs#5-",
        "@a.rs#20-10",
        "@a.rs#5-5",
        "@a.rs#0",
        "@we#ird.rs#5-9",
        "@a.rs#TODO",
        "@a.rs#5-9-12",
        "@a.rs#-5",
        "@a.rs#+5",
        "@a.rs#",
        "@a.rs#99999999999999999999",
        "@a.rs#5-9 and again @a.rs#5-9",
        "@a.rs#5-9 then @a.rs#30-40",
        "look at @#5-9",
    ];

    for text in cases {
        assert_eq!(
            scan(text),
            ganja_core::command::mentions(text),
            "the two mention scans drifted for {text:?}"
        );
    }
}

/// The exact shape of the trigger, which is the whole difference between a
/// file menu and a menu that pops up over an email address.
#[test]
fn the_menu_opens_only_for_an_at_that_starts_a_word() {
    let cases = [
        // Buffer, cursor, and the fragment it should be completing.
        ("@", (0, 1), Some("")),
        ("@src", (0, 4), Some("src")),
        ("look at @src", (0, 12), Some("src")),
        ("look at @src/lib.rs", (0, 19), Some("src/lib.rs")),
        // No `@` at all.
        ("look at src", (0, 11), None),
        // Attached to the word in front of it.
        ("mail me@example.com", (0, 19), None),
        // Whitespace between the `@` and the cursor: the mention ended.
        ("@src and then", (0, 13), None),
        // The cursor moved back into the mention, which is still one.
        ("@src and then", (0, 4), Some("src")),
        // The cursor moved in front of the `@`.
        ("@src", (0, 0), None),
        // A second line is scanned on its own terms.
        ("first\n@second", (1, 7), Some("second")),
        ("first\n@second", (0, 5), None),
        // The last `@` wins.
        ("@one @two", (0, 9), Some("two")),
    ];

    for (text, cursor, expected) in cases {
        assert_eq!(
            trigger(text, cursor).map(|fragment| fragment.text),
            expected.map(str::to_owned),
            "{text:?} with the cursor at {cursor:?}"
        );
    }
}

#[test]
fn the_trigger_reports_where_the_at_sits_so_a_choice_can_replace_it() {
    assert_eq!(
        trigger("look at @src", (0, 12)),
        Some(Fragment {
            row: 0,
            start: 8,
            text: "src".to_owned(),
        })
    );
    assert_eq!(
        trigger("look at @src", (0, 12)).map(|fragment| fragment.width()),
        Some(4),
        "the `@` plus the three characters after it"
    );
}

/// A cursor past the end of the buffer — nothing produces one, but the
/// arithmetic must not panic if something ever does.
#[test]
fn a_cursor_off_the_end_of_the_buffer_triggers_nothing() {
    assert_eq!(trigger("one line", (4, 0)), None);
    assert_eq!(
        trigger("@src", (0, 99)).map(|fragment| fragment.text),
        Some("src".to_owned())
    );
}

#[test]
fn a_submitted_buffer_carries_every_file_it_mentions() {
    let mentions = scan("compare @src/lib.rs with @src/app.rs and say why");

    assert_eq!(
        mentions
            .iter()
            .map(|mention| mention.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs", "src/app.rs"]
    );
}

#[test]
fn a_scan_reads_every_line_of_a_multi_line_prompt() {
    let mentions = scan("first @a.rs\nsecond @b.rs");

    assert_eq!(
        mentions
            .iter()
            .map(|mention| mention.path.as_str())
            .collect::<Vec<_>>(),
        vec!["a.rs", "b.rs"]
    );
}

/// The same file twice is one attachment: the second would spend the
/// context window on a copy.
#[test]
fn a_file_mentioned_twice_is_carried_once() {
    assert_eq!(scan("@a.rs and again @a.rs").len(), 1);
}

/// What the trigger refuses to open on, a scan has to refuse to read.
#[test]
fn a_scan_skips_what_the_trigger_would_never_have_opened() {
    for text in [
        "mail me@example.com about it",
        "an @ on its own",
        "no mentions here",
        "",
    ] {
        assert!(scan(text).is_empty(), "{text:?} mentions nothing");
    }
}

/// A project root holding `files`, each written with its own name.
fn project(files: &[&str]) -> tempfile::TempDir {
    let root = tempfile::TempDir::new().expect("a temporary directory is creatable");

    for file in files {
        let path = root.path().join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory is creatable");
        }
        std::fs::write(&path, file).expect("the fixture file is writable");
    }

    root
}

/// **D113's named case.** `@alice` is a person, not a file, and the whole
/// point of the filter is that the sentence reaches the model as a
/// sentence.
#[test]
fn a_word_that_names_no_file_is_carried_as_text_rather_than_attached() {
    let root = project(&["src/lib.rs"]);

    assert!(
        attachable("ask @alice about it", root.path()).is_empty(),
        "a name is not an attachment"
    );
    assert_eq!(
        scan("ask @alice about it").len(),
        1,
        "the lexer still finds it, or the filter above proves nothing"
    );
}

#[test]
fn a_file_that_is_there_still_attaches() {
    let root = project(&["src/lib.rs"]);

    assert_eq!(
        attachable("look at @src/lib.rs please", root.path())
            .iter()
            .map(|mention| mention.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs"]
    );
}

/// A path that is nearly right is the case the filter must not swallow
/// silently: it stays in the prompt, where the model can see the typo.
#[test]
fn a_mistyped_path_rides_as_text_beside_the_one_that_resolved() {
    let root = project(&["src/lib.rs"]);

    let mentions = attachable("compare @src/lib.rs with @src/libb.rs", root.path());

    assert_eq!(
        mentions
            .iter()
            .map(|mention| mention.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs"],
        "only the file that exists attaches"
    );
}

#[test]
fn a_directory_is_not_an_attachment() {
    let root = project(&["src/lib.rs"]);

    assert!(attachable("read @src", root.path()).is_empty());
}

/// The root is the project's, not the process's: the engine resolves the
/// part against the project root, so a filter reading anything else would
/// disagree with it.
#[test]
fn the_filter_resolves_against_the_root_it_is_given() {
    let root = project(&["notes.md"]);
    let elsewhere = project(&[]);

    assert_eq!(attachable("@notes.md", root.path()).len(), 1);
    assert!(attachable("@notes.md", elsewhere.path()).is_empty());
    assert!(attachable("@notes.md", Path::new("/nonexistent-ganja-root")).is_empty());
}

#[test]
fn a_mention_ends_at_the_first_whitespace_after_it() {
    let mentions = scan("@src/lib.rs, then what");

    assert_eq!(
        mentions.first().map(|mention| mention.path.as_str()),
        Some("src/lib.rs,"),
        "punctuation the user typed is part of what they typed"
    );
}

/// Upstream's suffix grammar (`autocomplete.tsx:39-50`), case by case:
/// split at the last `#`, digits only, end kept only when `start < end`,
/// and an unparseable tail stays part of the path.
#[test]
fn a_range_suffix_is_split_only_when_it_parses() {
    let cases = [
        ("a.rs", ("a.rs", None, None)),
        ("a.rs#5", ("a.rs", Some(5), None)),
        ("a.rs#5-9", ("a.rs", Some(5), Some(9))),
        // An empty end is a start alone.
        ("a.rs#5-", ("a.rs", Some(5), None)),
        // A reversed or flat range keeps its start only.
        ("a.rs#20-10", ("a.rs", Some(20), None)),
        ("a.rs#5-5", ("a.rs", Some(5), None)),
        // Line zero is what was typed; the read clamps it, not the scan.
        ("a.rs#0", ("a.rs", Some(0), None)),
        // The split is at the *last* `#`, so a path may contain one.
        ("we#ird.rs#5-9", ("we#ird.rs", Some(5), Some(9))),
        // Tails outside the grammar stay part of the path: `#` is a
        // character a file name may contain.
        ("notes#TODO", ("notes#TODO", None, None)),
        ("a.rs#", ("a.rs#", None, None)),
        ("a.rs#5-9-12", ("a.rs#5-9-12", None, None)),
        ("a.rs#-5", ("a.rs#-5", None, None)),
        ("a.rs#+5", ("a.rs#+5", None, None)),
        // A line number past `u32` is not a line number (the narrowing
        // named at `split_range`).
        (
            "a.rs#99999999999999999999",
            ("a.rs#99999999999999999999", None, None),
        ),
    ];

    for (mentioned, (path, start, end)) in cases {
        assert_eq!(split_range(mentioned), (path, start, end), "{mentioned:?}");
    }
}

/// `parse → render → parse`: what the menu writes, the scan reads back as
/// the same mention — the round-trip that keeps the two halves one
/// grammar.
#[test]
fn a_rendered_mention_scans_back_to_itself() {
    for text in [
        "@a.rs",
        "@src/lib.rs#5",
        "@src/lib.rs#5-9",
        "@we#ird.rs#12-40",
    ] {
        let scanned = scan(text);
        assert_eq!(scanned.len(), 1, "{text:?}");
        let mention = &scanned[0];
        let rendered = token(&mention.path, mention.start, mention.end);
        assert_eq!(
            rendered, text,
            "the render is the token it was scanned from"
        );
        assert_eq!(
            scan(&rendered),
            scanned,
            "{rendered:?} scans back unchanged"
        );
    }
}

/// The two normalizations a render applies, which are the grammar's own:
/// an empty end and a reversed range both collapse to their start.
#[test]
fn rendering_normalizes_what_the_grammar_collapsed() {
    let (_, start, end) = split_range("lib.rs#5-");
    assert_eq!(token("src/lib.rs", start, end), "@src/lib.rs#5");

    let (_, start, end) = split_range("lib.rs#20-10");
    assert_eq!(token("src/lib.rs", start, end), "@src/lib.rs#20");
}

/// Two slices of one file are two mentions; the same slice twice is one.
#[test]
fn mentions_dedupe_by_path_and_range_together() {
    assert_eq!(scan("@a.rs#5-9 and again @a.rs#5-9").len(), 1);

    let mentions = scan("@a.rs#5-9 then @a.rs#30-40 then @a.rs");
    assert_eq!(mentions.len(), 3, "{mentions:?}");
}

/// A ranged mention names the file, not the range: the filter resolves
/// the path half alone, and the range survives it.
#[test]
fn a_ranged_mention_attaches_when_its_file_is_there() {
    let root = project(&["src/lib.rs"]);

    let mentions = attachable("read @src/lib.rs#10-20 closely", root.path());

    assert_eq!(mentions.len(), 1, "{mentions:?}");
    assert_eq!(mentions[0].path, "src/lib.rs");
    assert_eq!(mentions[0].start, Some(10));
    assert_eq!(mentions[0].end, Some(20));
}

/// `@#5` would be lines of no file at all.
#[test]
fn a_range_with_no_path_mentions_nothing() {
    assert!(scan("look at @#5-9").is_empty());
}

/// **F5**, the baseline: one real path pasted alone is a drop.
#[test]
fn a_dropped_path_resolves_to_a_project_relative_mention() {
    let root = project(&["src/lib.rs"]);

    assert_eq!(
        classify_drop("src/lib.rs", root.path()),
        Some(vec!["src/lib.rs".to_owned()])
    );
}

/// Several files dragged in at once arrive whitespace-separated in one
/// paste; each becomes its own mention, in the order they were pasted.
#[test]
fn several_dropped_paths_resolve_in_the_order_they_were_pasted() {
    let root = project(&["b.rs", "a.rs"]);

    assert_eq!(
        classify_drop("b.rs a.rs", root.path()),
        Some(vec!["b.rs".to_owned(), "a.rs".to_owned()])
    );
}

/// A pasted shell one-liner that happens to name a real path must not
/// have that one path pulled out into a mention while `cat`, `|` and
/// `grep` are left behind as text: every token has to qualify, or none do.
#[test]
fn one_token_that_is_not_a_path_fails_the_whole_paste() {
    let root = project(&["file.txt"]);

    assert_eq!(classify_drop("cat file.txt | grep x", root.path()), None);
}

/// `file://` URLs percent-decode, and a `%20` is exactly the reason one
/// would appear in a dropped path's URL.
#[test]
fn a_file_url_percent_decodes_and_resolves() {
    let root = project(&["a b.png"]);
    let url = format!("file://{}/a%20b.png", root.path().display());

    assert_eq!(
        classify_drop(&url, root.path()),
        Some(vec!["a b.png".to_owned()])
    );
}

/// A terminal that quotes a dropped path because it has a space in it —
/// the space must not split the quoted run into two tokens.
#[test]
fn a_quoted_path_with_a_space_resolves_as_one_token() {
    let root = project(&["my file.txt"]);

    assert_eq!(
        classify_drop("'my file.txt'", root.path()),
        Some(vec!["my file.txt".to_owned()])
    );
}

/// The shell-escaped equivalent of the quoted case above, off Windows
/// only: there, a backslash is the path separator, not an escape.
#[cfg(not(windows))]
#[test]
fn a_backslash_escaped_space_resolves_as_one_token() {
    let root = project(&["my file.txt"]);

    assert_eq!(
        classify_drop(r"my\ file.txt", root.path()),
        Some(vec!["my file.txt".to_owned()])
    );
}

#[test]
fn a_relative_dot_path_resolves() {
    let root = project(&["src/lib.rs"]);

    assert_eq!(
        classify_drop("./src/lib.rs", root.path()),
        Some(vec!["src/lib.rs".to_owned()])
    );
}

#[test]
fn a_unicode_named_file_resolves() {
    let root = project(&["日本語.txt"]);

    assert_eq!(
        classify_drop("日本語.txt", root.path()),
        Some(vec!["日本語.txt".to_owned()])
    );
}

/// A path outside the project keeps its absolute form — the display
/// convention every other mention insertion already follows.
#[test]
fn an_absolute_path_outside_root_stays_absolute() {
    let root = project(&["a.rs"]);
    let outside = project(&["b.rs"]);
    let absolute = outside.path().join("b.rs").display().to_string();

    assert_eq!(
        classify_drop(&absolute, root.path()),
        Some(vec![absolute.clone()])
    );
}

#[test]
fn empty_or_whitespace_only_text_is_not_a_drop() {
    let root = project(&[]);

    assert_eq!(classify_drop("", root.path()), None);
    assert_eq!(classify_drop("   \n\t  ", root.path()), None);
}

#[test]
fn a_token_naming_nothing_on_disk_is_not_a_drop() {
    let root = project(&[]);

    assert_eq!(classify_drop("nope.rs", root.path()), None);
}
