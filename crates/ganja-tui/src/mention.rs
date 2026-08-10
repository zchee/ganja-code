//! `@` file mentions: when the composer is offering files, and which ones a
//! submitted prompt is carrying.
//!
//! Spec: upstream `packages/tui/src/component/prompt/display.ts`
//! (`mentionTriggerIndex`) and `component/prompt/autocomplete.tsx`. The rule is
//! narrow on purpose — the **last** `@` before the cursor, preceded by
//! start-of-text or whitespace, with no whitespace between it and the cursor —
//! so an email address or a `user@host` in the middle of a sentence never
//! raises a file menu.
//!
//! Two halves, and they share that rule rather than each having their own:
//! [`trigger`] is what the menu opens on while the user types, and [`scan`] is
//! what a submitted buffer is read with. A mention that could be typed but not
//! read back would attach nothing; one that could be read but not typed would
//! attach something the user never chose.
//!
//! The `@path` text **stays in the prompt**. Upstream leaves it there too: the
//! literal token is what the user wrote and what the transcript shows, and the
//! file's content is resolved separately when the request is built. A token
//! may carry a `#line-range` suffix — `@src/lib.rs#10-20` — upstream's
//! `extractLineRange` grammar (`autocomplete.tsx:39-50`), split off by
//! [`split_range`] and carried on the [`Mention`] so the send-time read can
//! slice to exactly the lines it names.

use std::path::Path;

use ganja_protocol::Mention;

/// A mention being typed, located in the buffer it was found in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// Which line of the buffer the `@` is on.
    pub row: usize,
    /// Which character of that line the `@` itself is, counted in `char`s.
    pub start: usize,
    /// What has been typed between the `@` and the cursor.
    pub text: String,
}

impl Fragment {
    /// How many characters the `@` and everything after it occupy, which is
    /// the span a chosen path replaces.
    #[must_use]
    pub fn width(&self) -> usize {
        self.text.chars().count() + 1
    }
}

/// The mention `text` is offering to complete with the cursor at
/// `(row, column)`, or [`None`] when nothing is being mentioned.
///
/// A newline is whitespace, so the `@` is always on the cursor's own line:
/// scanning that line's prefix is upstream's whole-buffer scan, said in the
/// terms this editor keeps its cursor in.
#[must_use]
pub fn trigger(text: &str, cursor: (usize, usize)) -> Option<Fragment> {
    let (row, column) = cursor;
    let line = text.split('\n').nth(row)?;
    let prefix: Vec<char> = line.chars().take(column).collect();

    let start = prefix.iter().rposition(|character| *character == '@')?;
    // Start-of-line or whitespace before it; anything else and the `@` belongs
    // to the word in front of it.
    if start > 0 && !prefix[start - 1].is_whitespace() {
        return None;
    }

    let fragment: String = prefix[start + 1..].iter().collect();
    if fragment.chars().any(char::is_whitespace) {
        return None;
    }

    Some(Fragment {
        row,
        start,
        text: fragment,
    })
}

/// Every file `text` mentions, in the order it mentions them.
///
/// The same trigger rule read across a whole buffer rather than up to a
/// cursor, with a mention running to the next whitespace and its `#line-range`
/// suffix split off by [`split_range`]. Repeats collapse **by whole mention**
/// — path and range together — because `@a.rs#5-9` and `@a.rs#30-40` are two
/// different slices, while attaching one slice twice would spend the context
/// window on a copy.
#[must_use]
pub fn scan(text: &str) -> Vec<Mention> {
    let mut found: Vec<Mention> = Vec::new();

    for line in text.split('\n') {
        let characters: Vec<char> = line.chars().collect();
        let mut index = 0;

        while index < characters.len() {
            let opens =
                characters[index] == '@' && (index == 0 || characters[index - 1].is_whitespace());
            if !opens {
                index += 1;
                continue;
            }

            let token: String = characters[index + 1..]
                .iter()
                .take_while(|character| !character.is_whitespace())
                .collect();
            index += token.chars().count() + 1;

            // A bare `@` names nothing.
            if token.is_empty() {
                continue;
            }
            let (path, start, end) = split_range(&token);
            // Neither does a bare range: `@#5` is lines of no file at all.
            if path.is_empty() {
                continue;
            }
            let mention = Mention {
                path: path.to_owned(),
                start,
                end,
            };
            if !found.contains(&mention) {
                found.push(mention);
            }
        }
    }

    found
}

/// Splits the `#line-range` suffix off a mention token.
///
/// Upstream's `extractLineRange` (`autocomplete.tsx:32-57`), applied at the
/// **last** `#`: the suffix must be the whole of `start` or `start-end` in
/// digits (`^(\d+)(?:-(\d*))?$`), and anything else — `#TODO`, `#5-9-12`, a
/// bare trailing `#` — is no range at all, so the whole token stays a path,
/// because `#` is a character a file name may contain. An empty `end` (`#5-`)
/// is a start alone, and `end` is kept only when `start < end`
/// (`autocomplete.tsx:47`), so `#20-10` normalizes to `#20`. One narrowing:
/// a number past `u32` is not a line this build recognizes and leaves the
/// token a path, where upstream parses into a float and carries it.
#[must_use]
pub fn split_range(token: &str) -> (&str, Option<u32>, Option<u32>) {
    let Some((path, suffix)) = token.rsplit_once('#') else {
        return (token, None, None);
    };

    match parse_range(suffix) {
        Some((start, end)) => (path, Some(start), end),
        None => (token, None, None),
    }
}

/// The `start`/`start-end` a valid suffix names, normalized; [`None`] for a
/// suffix outside the grammar.
fn parse_range(suffix: &str) -> Option<(u32, Option<u32>)> {
    // Spelled out because `u32::from_str` accepts a leading `+`, which the
    // grammar's `\d+` does not.
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());

    let (start, end) = match suffix.split_once('-') {
        None => (suffix, None),
        Some((start, end)) => (start, Some(end)),
    };
    if !digits(start) || !end.is_none_or(|end| end.is_empty() || digits(end)) {
        return None;
    }

    let start: u32 = start.parse().ok()?;
    let end = match end {
        Some(end) if !end.is_empty() => Some(end.parse::<u32>().ok()?),
        _ => None,
    };

    Some((start, end.filter(|end| start < *end)))
}

/// The literal token a mention renders back as: `@path`, with its normalized
/// `#start` or `#start-end` when a range was named.
///
/// One spelling shared by the menu's insertion and the transcript's `File`
/// part — upstream's `filename` recomposition (`autocomplete.tsx:250`) — and
/// the other half of [`split_range`]: scanning what this prints yields the
/// mention it was printed from.
#[must_use]
pub fn token(path: &str, start: Option<u32>, end: Option<u32>) -> String {
    match (start, end) {
        (Some(start), Some(end)) => format!("@{path}#{start}-{end}"),
        (Some(start), None) => format!("@{path}#{start}"),
        (None, _) => format!("@{path}"),
    }
}

/// Every file `text` mentions that is **actually there**, resolved against
/// `root`.
///
/// [`scan`] is the lexer and this is the filter, and a submitted prompt goes
/// through both (**D113**, **R15(a)**). The reason is that `@` in prose is
/// older and more common than `@` as a file mention: "ask @alice about it"
/// mentions a person, and attaching a `File` part for her would put an
/// attachment-error block in front of the model instead of the sentence the
/// user wrote. A path that does not resolve stays in the text verbatim, which
/// is the same thing that happens to a mistyped one — the model reads it and
/// can still act on it.
///
/// `root` is the **project root**, because that is what the engine resolves a
/// `File` part against (`session.rs::resolve_mentions`). Filtering against
/// anything else would drop mentions the engine would have read, or keep ones
/// it could not.
///
/// Directories do not survive: upstream's menu offers files, the engine's
/// attachment answers a directory with a note telling the model to name a file
/// inside it, and a token that names one is more use to the model as the text
/// the user typed.
#[must_use]
pub fn attachable(text: &str, root: &Path) -> Vec<Mention> {
    scan(text)
        .into_iter()
        .filter(|mention| root.join(&mention.path).is_file())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Fragment, attachable, scan, split_range, token, trigger};

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
}
