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
//!
//! Since D529 the submit-time classification has a second consumer: a token
//! [`scan`] does **not** resolve to a real file may still name a teammate or
//! a live session and ride `session_mentions` instead — that decision lives
//! in `app.rs`, downstream of this module, and the file rule here stays
//! byte-for-byte first: a token that resolves to a file is a file mention,
//! whatever else shares its name.

use std::path::{Path, PathBuf};

use ganja_protocol::Mention;
use url::Url;

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
    sigil_trigger(text, cursor, '@')
}

/// The `$name` skill invocation `text` is offering to complete, or [`None`]
/// — [`trigger`]'s rule with `$` for `@`, because the Codex CLI's skill
/// grammar (**D491**) is a mention like `@`'s, not a line prefix like `!`'s.
#[must_use]
pub fn skill_trigger(text: &str, cursor: (usize, usize)) -> Option<Fragment> {
    sigil_trigger(text, cursor, '$')
}

/// The two triggers' one rule: the sigil opens a word — start-of-line or
/// whitespace before it — and the fragment is what sits between it and the
/// cursor, unbroken by whitespace.
fn sigil_trigger(text: &str, cursor: (usize, usize), sigil: char) -> Option<Fragment> {
    let (row, column) = cursor;
    let line = text.split('\n').nth(row)?;
    let prefix: Vec<char> = line.chars().take(column).collect();

    let start = prefix.iter().rposition(|character| *character == sigil)?;
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

/// The paths `text` is a *drop* of, resolved against `root`; [`None`] when it
/// is not one.
///
/// Spec: upstream `pastedFilepath` (`component/prompt/index.tsx:78-80`),
/// generalized from upstream's single candidate to as many as the paste
/// carries — a terminal that drags in several files sends their paths
/// whitespace-separated (each quoted, if it needs to be) in one paste event,
/// and each is its own mention (**F5**). The rule stays upstream's otherwise:
/// **every** token `tokenize` finds must resolve, one miss and the whole
/// paste is ordinary text — a pasted shell one-liner naming a real path
/// (`cat file.txt | grep x`) must not have `file.txt` alone turned into a
/// mention while `cat`, `|` and `grep` stay text around it.
#[must_use]
pub fn classify_drop(text: &str, root: &Path) -> Option<Vec<String>> {
    let tokens = tokenize(text);
    if tokens.is_empty() {
        return None;
    }

    tokens
        .into_iter()
        .map(|token| {
            let candidate = resolve_dropped(&token)?;
            let absolute = if candidate.is_absolute() {
                candidate
            } else {
                root.join(candidate)
            };

            absolute.exists().then(|| display(root, &absolute))
        })
        .collect()
}

/// Splits a drop candidate into tokens the way a shell would, generalizing
/// upstream's single-candidate check to a whole paste: whitespace separates
/// tokens except inside a `'…'`/`"…"` run — the quoting a terminal reaches
/// for when a dropped path has a space in it — and, off Windows, right after
/// a backslash, which escapes whatever follows rather than ending the token
/// there. Quote characters are consumed rather than kept, matching upstream's
/// own edge-quote strip (`raw.replace(/^['"]+|['"]+$/g, "")`) generalized the
/// same way; a `\`-escape is left in the token for [`resolve_dropped`] to
/// undo, because whether it means anything depends on whether the token
/// turns out to be a `file://` URL.
fn tokenize(text: &str) -> Vec<String> {
    let escapes = !cfg!(windows);
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    let mut characters = text.chars();

    while let Some(character) = characters.next() {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }

        match character {
            '\'' | '"' => {
                quote = Some(character);
                has_token = true;
            }
            '\\' if escapes => {
                current.push(character);
                has_token = true;
                if let Some(escaped) = characters.next() {
                    current.push(escaped);
                }
            }
            character if character.is_whitespace() => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            character => {
                current.push(character);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(current);
    }

    tokens
}

/// One dropped token's own path, or [`None`] when it is neither a `file://`
/// URL nor a plain (possibly escaped) path.
///
/// A `file://` URL decodes through [`Url::to_file_path`], which carries the
/// percent-decoding and the empty-or-`localhost`-host rule upstream's
/// `fileURLToPath` applies; anything else is unescaped a backslash at a time.
fn resolve_dropped(token: &str) -> Option<PathBuf> {
    if token.starts_with("file://") {
        return Url::parse(token).ok()?.to_file_path().ok();
    }

    Some(PathBuf::from(unescape(token)))
}

/// Undoes a shell's backslash escaping — `\ ` back to a space, and so on —
/// everywhere except Windows, whose backslash *is* the path separator and
/// where unescaping one would corrupt the path instead of decoding it
/// (upstream's own carve-out: `if (platform === "win32") return raw`).
fn unescape(token: &str) -> String {
    if cfg!(windows) {
        return token.to_owned();
    }

    let mut unescaped = String::with_capacity(token.len());
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        // Only a backslash ever consumes a second character here — anything
        // else is pushed on its own, so a backslash that lands right after a
        // plain character is still seen as its own escape on the next turn
        // rather than swallowed as that character's pair.
        if character == '\\'
            && let Some(escaped) = characters.next()
        {
            unescaped.push(escaped);
        } else {
            unescaped.push(character);
        }
    }

    unescaped
}

/// `path`, relative to `root` when it is under it, absolute otherwise — the
/// same convention `ganja-tool`'s own `display()` follows for the same
/// reason: a project-relative path is what a mention normally reads as, and
/// a path a drop can name outside the project has nothing to be relative to.
fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
#[path = "mention_tests.rs"]
mod tests;
