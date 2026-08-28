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

use etcetera::BaseStrategy as _;
use etcetera::base_strategy::Xdg;
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
        Self { input: input.into(), mode: None, parts: Vec::new() }
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

/// One remembered submission, dated to the moment it can honestly be shown
/// by.
///
/// The JSONL format carries no per-entry timestamp and stays exactly that
/// shape — the store is P8-pinned and this wave does not reopen it
/// (deviation **D448**, history-search-age-approximated). A submission
/// appended this run is dated to the instant [`History::append`] ran; every
/// entry this run only *loaded* shares the file's own last-write time, which
/// is the newest honest instant available for it — not the moment it was
/// originally typed, if that predates this run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recalled {
    /// What was submitted.
    pub prompt: PromptInfo,
    /// Milliseconds since the Unix epoch it is dated to, `now_ms`'s
    /// convention.
    pub at: u64,
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
    /// [`Recalled::at`] for each of [`History::entries`], same index — kept
    /// in memory only, never written to the JSONL (see [`Recalled`]).
    times: Vec<u64>,
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

        // Every entry this call loaded shares one instant: the file's own
        // last-write time when that is readable, else the moment of this
        // load. Neither is when any individual line was actually typed — the
        // format never recorded that — but both are honest about what they
        // are (**D448**), unlike inventing a spread of fake per-line times.
        let loaded_at = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or_else(now_ms, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        let times = vec![loaded_at; entries.len()];

        let history = Self { path: Some(path), entries, times, index: 0 };

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
        self.times.push(now_ms());
        self.index = 0;

        if self.entries.len() > MAX_HISTORY_ENTRIES {
            let excess = self.entries.len() - MAX_HISTORY_ENTRIES;
            self.entries.drain(..excess);
            self.times.drain(..excess);
            self.rewrite();
            return;
        }

        self.append_line(&entry);
    }

    /// Every remembered submission, newest first and dated per [`Recalled`].
    ///
    /// Read-only, as the name promises: nothing here moves [`History::step`]'s
    /// walk or touches what [`History::append`] writes to disk.
    #[must_use]
    pub fn entries(&self) -> Vec<Recalled> {
        self.entries
            .iter()
            .cloned()
            .zip(self.times.iter().copied())
            .rev()
            .map(|(prompt, at)| Recalled { prompt, at })
            .collect()
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

        match std::fs::OpenOptions::new().create(true).append(true).open(path) {
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

/// Milliseconds since the Unix epoch "now", saturating rather than failing
/// when the machine's clock predates 1970 — [`crate::component::sessions::now`]'s
/// convention, kept as its own copy here rather than imported: a data model
/// reaching into a UI component for six lines would be the wrong direction
/// for this crate's module tree to lean.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
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
#[path = "history_tests.rs"]
mod tests;
