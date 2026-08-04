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
//! file's content is resolved separately when the request is built.
//! `#line-range` suffixes are not ported (**D12**).

use std::path::Path;

use ganja_core::Mention;

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
/// cursor, with a mention running to the next whitespace. Repeats collapse:
/// upstream dedupes its file parts by name, and attaching one file twice would
/// spend the context window on it twice.
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

            let path: String = characters[index + 1..]
                .iter()
                .take_while(|character| !character.is_whitespace())
                .collect();
            index += path.chars().count() + 1;

            // A bare `@` names nothing.
            if path.is_empty() {
                continue;
            }
            if !found.iter().any(|mention| mention.path == path) {
                found.push(Mention { path });
            }
        }
    }

    found
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

    use super::{Fragment, attachable, scan, trigger};

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
}
