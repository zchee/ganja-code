//! What the composer remembers across submissions, so an Up-arrow on an empty
//! prompt brings the last one back.
//!
//! Spec: upstream `packages/tui/src/prompt/history.tsx`. A JSONL file under the
//! interface's own state directory (`prompt-history.jsonl` in `<XDG data>/
//! ganja`, the same home the theme pick and the session store use — standing
//! in for upstream's `paths.state`), capped at [`MAX_HISTORY_ENTRIES`], loaded
//! parse-what-parses and rewritten to self-heal, appended on every submission
//! with consecutive duplicates suppressed.
//!
//! The entry shape is upstream's `PromptInfo` from day one, `parts` included
//! and empty: attachments (a later wave) fill it, and starting with the field
//! present means the file format never migrates when they do. `serde` skips
//! the `None`s so a file this build writes reads back byte-for-byte the same.
//!
//! Every disk failure is swallowed to a warning: a history that cannot be read
//! is a convenience lost, never a reason a prompt does not send.

use std::path::{Path, PathBuf};

use etcetera::{BaseStrategy as _, base_strategy::Xdg};
use serde::{Deserialize, Serialize};

/// The most entries kept on disk and in memory. Upstream's number.
pub const MAX_HISTORY_ENTRIES: usize = 50;

/// The file, under the interface's state home. Upstream's name.
const FILE: &str = "prompt-history.jsonl";

/// The directory under the XDG data home the interface keeps its own state in,
/// shared with the theme pick and the session store — the same `ganja` the
/// rest of the app resolves, so redirecting `XDG_DATA_HOME` moves this too.
const DIRECTORY: &str = "ganja";

/// One remembered submission, upstream's `PromptInfo`.
///
/// `mode` and `parts` mirror the fields a prompt carries; `parts` stays empty
/// until the attachment wave puts things in it, but the field is here now so
/// the on-disk shape never has to change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PromptInfo {
    /// What was typed.
    pub input: String,
    /// Whether it was sent to the model or run in the shell, when that was not
    /// the default. Absent means the ordinary prompt mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// The non-text parts the prompt carried. Empty until attachments land,
    /// and omitted from the file while it is empty so the format is stable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<serde_json::Value>,
}

/// Which of the two things a remembered prompt did. Upstream's `"normal"` is
/// this build's default and is never written, so the file only ever names the
/// shell case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Sent to the model.
    Normal,
    /// Run in the shell.
    Shell,
}

impl PromptInfo {
    /// A plain text prompt, the shape a submission without attachments takes.
    #[must_use]
    pub fn text(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            mode: None,
            parts: Vec::new(),
        }
    }
}

/// The direction a history walk moves: toward older entries or back toward the
/// live buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Up: one entry further back.
    Older,
    /// Down: one entry more recent, ending at the live buffer.
    Newer,
}

/// The remembered prompts and where a walk through them currently sits.
///
/// `index` counts backward from zero: `0` is the live buffer (nothing
/// recalled), `-1` the newest entry, `-len` the oldest — exactly upstream's
/// `store.index`, negated because Rust's index is unsigned nowhere it is used
/// as one.
#[derive(Debug, Default)]
pub struct History {
    /// Where the JSONL lives, or [`None`] when no state home could be
    /// resolved — then every operation is inert.
    path: Option<PathBuf>,
    /// Oldest first, so the newest is last: upstream pushes and reads `at(-1)`.
    entries: Vec<PromptInfo>,
    /// How far back the current walk has gone; `0` is the live buffer.
    index: isize,
}

impl History {
    /// Loads the history from the interface's own state home,
    /// `<XDG data>/ganja/prompt-history.jsonl` — the same home the theme pick
    /// lives in, so a test that redirects `XDG_DATA_HOME` reaches this too.
    #[must_use]
    pub fn load() -> Self {
        let Some(path) = default_path() else {
            return Self::default();
        };

        Self::load_from(path)
    }

    /// Loads the history from `path` specifically.
    ///
    /// Parse-what-parses: a line that is not a `PromptInfo` is dropped rather
    /// than failing the load, and only the last [`MAX_HISTORY_ENTRIES`] are
    /// kept. When anything was loaded, the file is rewritten from the retained
    /// entries — that is what heals a file a crash left half-written and what
    /// enforces the cap on a file an older build let grow.
    #[must_use]
    pub fn load_from(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "the prompt history could not be read");
                Vec::new()
            }
        };

        let history = Self {
            path: Some(path),
            entries,
            index: 0,
        };

        if !history.entries.is_empty() {
            history.rewrite();
        }

        history
    }

    /// Records `entry`, unless it repeats the last one.
    ///
    /// A submission identical to the newest entry stores nothing and only
    /// resets the walk — pressing Enter on a recalled prompt should not fill
    /// the history with copies of it. Otherwise the entry is appended; a single
    /// line is added to the file when the cap still has room, and the whole
    /// file is rewritten when the cap forced an entry out.
    pub fn append(&mut self, entry: PromptInfo) {
        if self.entries.last() == Some(&entry) {
            self.index = 0;
            return;
        }

        self.entries.push(entry.clone());
        self.index = 0;

        if self.entries.len() > MAX_HISTORY_ENTRIES {
            let excess = self.entries.len() - MAX_HISTORY_ENTRIES;
            self.entries.drain(..excess);
            self.rewrite();
            return;
        }

        self.append_line(&entry);
    }

    /// Walks one step and returns the buffer that step should show, or [`None`]
    /// when nothing moves.
    ///
    /// The guard is upstream's exactly: a walk starts or continues only while
    /// the buffer is empty or still holds the entry the last step put there —
    /// a buffer the user has since edited is theirs, and an Up-arrow leaves it
    /// alone. Walking to index `0` returns an empty buffer (the live prompt);
    /// walking past either end returns [`None`] and moves nothing.
    pub fn step(&mut self, direction: Direction, current_input: &str) -> Option<PromptInfo> {
        if self.entries.is_empty() {
            return None;
        }

        // Upstream's guard reads `history.at(store.index)`: a walk is refused
        // when the buffer has been edited away from the entry the index names.
        // `at()` counts `0` from the front and negatives from the end, so the
        // resting index `0` names the *oldest* entry here — which is why a
        // buffer holding something the user typed is blocked before the first
        // step, and an empty buffer (`current_input.is_empty()`) is always let
        // through.
        if let Some(current) = self.entry_at(self.index)
            && current.input != current_input
            && !current_input.is_empty()
        {
            return None;
        }

        let step = match direction {
            Direction::Older => -1,
            Direction::Newer => 1,
        };
        let next = self.index + step;
        // `Math.abs(next) > length` past the oldest, `next > 0` past the live
        // buffer: both leave the index where it was.
        if next.unsigned_abs() > self.entries.len() || next > 0 {
            return None;
        }
        self.index = next;

        if self.index == 0 {
            return Some(PromptInfo::text(""));
        }

        self.entry_at(self.index).cloned()
    }

    /// The entry an index names under JavaScript `Array.at` semantics: `0`
    /// counts from the front, negatives from the end, out of range is [`None`].
    ///
    /// Only two indices are ever asked for — the resting `0` (the oldest
    /// entry, which the dirty-buffer guard compares against) and the negative
    /// index a walk has reached (the entry it should show) — but keeping the
    /// full `at()` rule is what makes this a faithful port rather than a lucky
    /// one.
    fn entry_at(&self, index: isize) -> Option<&PromptInfo> {
        let resolved = if index < 0 {
            self.entries.len().checked_sub(index.unsigned_abs())?
        } else {
            usize::try_from(index).ok()?
        };

        self.entries.get(resolved)
    }

    /// Rewrites the whole file from the retained entries.
    fn rewrite(&self) {
        let Some(path) = &self.path else {
            return;
        };

        let mut body = String::new();
        for entry in &self.entries {
            match serde_json::to_string(entry) {
                Ok(line) => {
                    body.push_str(&line);
                    body.push('\n');
                }
                // An entry this build cannot re-encode is one it never wrote;
                // dropping it from the rewrite is the same self-heal a corrupt
                // line already gets on load.
                Err(error) => {
                    tracing::warn!(%error, "a history entry could not be re-encoded and was dropped");
                }
            }
        }

        write_all(path, &body);
    }

    /// Appends one entry as a line, creating the file and its directory if this
    /// is the first write.
    fn append_line(&self, entry: &PromptInfo) {
        use std::io::Write as _;

        let Some(path) = &self.path else {
            return;
        };
        let Ok(line) = serde_json::to_string(entry) else {
            return;
        };

        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(path = %parent.display(), %error, "the prompt history directory could not be created");
            return;
        }

        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut file) => {
                if let Err(error) = writeln!(file, "{line}") {
                    tracing::warn!(path = %path.display(), %error, "a prompt history entry could not be appended");
                }
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "the prompt history file could not be opened");
            }
        }
    }
}

/// `<XDG data>/ganja/prompt-history.jsonl`, or [`None`] when there is no home
/// to resolve it against.
///
/// The same `<XDG data>/ganja` the theme pick and the session store use, so
/// the whole interface agrees on where a user's own state lives.
fn default_path() -> Option<PathBuf> {
    let base = Xdg::new().ok()?;

    Some(base.data_dir().join(DIRECTORY).join(FILE))
}

/// The `PromptInfo`s a JSONL file holds, corrupt lines dropped, capped to the
/// last [`MAX_HISTORY_ENTRIES`].
fn parse(text: &str) -> Vec<PromptInfo> {
    let mut entries: Vec<PromptInfo> = text
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    if entries.len() > MAX_HISTORY_ENTRIES {
        let excess = entries.len() - MAX_HISTORY_ENTRIES;
        entries.drain(..excess);
    }

    entries
}

/// Replaces `path`'s contents with `body`, creating the directory if needed.
fn write_all(path: &Path, body: &str) {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(path = %parent.display(), %error, "the prompt history directory could not be created");
        return;
    }

    if let Err(error) = std::fs::write(path, body) {
        tracing::warn!(path = %path.display(), %error, "the prompt history could not be rewritten");
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{Direction, History, MAX_HISTORY_ENTRIES, PromptInfo, parse};

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    fn history_in(directory: &TempDir) -> History {
        History::load_from(directory.path().join("prompt-history.jsonl"))
    }

    #[test]
    fn a_submission_reads_back_on_the_next_load() {
        let directory = temporary();
        let mut history = history_in(&directory);
        history.append(PromptInfo::text("what does this crate do"));

        let mut reloaded = history_in(&directory);
        assert_eq!(
            reloaded.step(Direction::Older, ""),
            Some(PromptInfo::text("what does this crate do"))
        );
    }

    #[test]
    fn fifty_one_submissions_keep_exactly_fifty() {
        let directory = temporary();
        let path = directory.path().join("prompt-history.jsonl");
        let mut history = History::load_from(path.clone());

        for n in 0..=MAX_HISTORY_ENTRIES {
            history.append(PromptInfo::text(format!("prompt {n}")));
        }

        let on_disk = std::fs::read_to_string(&path).expect("the file reads");
        assert_eq!(
            on_disk.lines().filter(|line| !line.is_empty()).count(),
            MAX_HISTORY_ENTRIES,
            "the cap holds on disk"
        );
        // The oldest fell off the front; the newest is still there.
        assert!(!on_disk.contains("\"prompt 0\""), "the oldest was dropped");
        assert!(
            on_disk.contains(&format!("\"prompt {MAX_HISTORY_ENTRIES}\"")),
            "the newest was kept"
        );
    }

    #[test]
    fn a_corrupt_line_is_dropped_and_the_file_self_heals() {
        let directory = temporary();
        let path = directory.path().join("prompt-history.jsonl");
        std::fs::write(
            &path,
            "{\"input\":\"good one\"}\nnot json at all\n{\"input\":\"good two\"}\n",
        )
        .expect("the fixture writes");

        let mut history = History::load_from(path.clone());

        // The load kept the two that parsed and rewrote the file without the
        // corrupt line.
        let healed = std::fs::read_to_string(&path).expect("the file reads");
        assert!(!healed.contains("not json at all"), "got:\n{healed}");
        assert_eq!(
            healed.lines().filter(|line| !line.is_empty()).count(),
            2,
            "only the two good lines remain:\n{healed}"
        );
        assert_eq!(
            history.step(Direction::Older, ""),
            Some(PromptInfo::text("good two")),
            "the newest good line is reachable"
        );
    }

    #[test]
    fn two_identical_submissions_store_once() {
        let directory = temporary();
        let path = directory.path().join("prompt-history.jsonl");
        let mut history = History::load_from(path.clone());

        history.append(PromptInfo::text("say it again"));
        history.append(PromptInfo::text("say it again"));

        let on_disk = std::fs::read_to_string(&path).expect("the file reads");
        assert_eq!(
            on_disk.lines().filter(|line| !line.is_empty()).count(),
            1,
            "the consecutive duplicate was suppressed:\n{on_disk}"
        );
    }

    #[test]
    fn a_non_consecutive_repeat_is_stored_again() {
        let directory = temporary();
        let mut history = history_in(&directory);

        history.append(PromptInfo::text("first"));
        history.append(PromptInfo::text("second"));
        history.append(PromptInfo::text("first"));

        // Only a run of the *same* prompt is collapsed; the same words after
        // something else is a real second visit.
        assert_eq!(history.entries.len(), 3);
    }

    #[test]
    fn up_on_a_dirty_buffer_moves_nothing() {
        let directory = temporary();
        let mut history = history_in(&directory);
        history.append(PromptInfo::text("the stored one"));

        // The buffer holds something the user typed that is not the entry a
        // walk would show, so the walk is refused and the buffer is left alone.
        assert_eq!(history.step(Direction::Older, "a half-typed thought"), None);
    }

    #[test]
    fn up_restores_the_last_prompt_and_down_returns_to_empty() {
        let directory = temporary();
        let mut history = history_in(&directory);
        history.append(PromptInfo::text("older"));
        history.append(PromptInfo::text("newer"));

        assert_eq!(
            history.step(Direction::Older, ""),
            Some(PromptInfo::text("newer")),
            "the first Up is the newest entry"
        );
        assert_eq!(
            history.step(Direction::Older, "newer"),
            Some(PromptInfo::text("older")),
            "the second Up is the one before it"
        );
        assert_eq!(
            history.step(Direction::Older, "older"),
            None,
            "there is nothing older to reach"
        );
        assert_eq!(
            history.step(Direction::Newer, "older"),
            Some(PromptInfo::text("newer")),
            "Down comes back toward the buffer"
        );
        assert_eq!(
            history.step(Direction::Newer, "newer"),
            Some(PromptInfo::text("")),
            "and lands on the empty live buffer"
        );
    }

    /// The shell mode and the empty parts vector are on the wire the way
    /// upstream's `PromptInfo` is, and a file this build writes reads back
    /// unchanged.
    #[test]
    fn the_mode_survives_the_round_trip_and_the_empty_parts_stay_off_the_file() {
        let directory = temporary();
        let path = directory.path().join("prompt-history.jsonl");
        let mut history = History::load_from(path.clone());
        history.append(PromptInfo {
            input: "ls -la".to_owned(),
            mode: Some(super::Mode::Shell),
            parts: Vec::new(),
        });

        let on_disk = std::fs::read_to_string(&path).expect("the file reads");
        assert!(on_disk.contains("\"mode\":\"shell\""), "got:\n{on_disk}");
        assert!(
            !on_disk.contains("\"parts\""),
            "an empty parts vector is not written:\n{on_disk}"
        );

        let recalled = parse(&on_disk);
        assert_eq!(
            recalled.first().and_then(|e| e.mode),
            Some(super::Mode::Shell)
        );
    }
}
