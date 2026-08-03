//! Output truncation, so one tool call cannot flood the context window.
//!
//! Spec: upstream `packages/opencode/src/tool/truncate.ts` and
//! `truncation-dir.ts`. The budgets below are upstream's
//! `MAX_LINES`/`MAX_BYTES`, and the notice mirrors the `removed {unit}
//! truncated` wording `Truncate.output` appends in its "head" direction (the
//! only direction any caller here needs).
//!
//! Several ported prompts — `bash`'s among them — tell the model, verbatim,
//! that truncated output "will be written to a file" it can `Read` with
//! `offset`/`limit`. For that to be true, a truncating clamp has to actually
//! write it: `write_overflow` spills the full, untouched text to a file and
//! [`hint`] tells the model where, the same as upstream's `Truncate.output`.
//!
//! Two upstream pieces are deliberately not ported:
//!
//! - **Where the file lives.** Upstream's `TRUNCATION_DIR` is
//!   `path.join(Global.Path.data, "tool-output")`, and this port has no
//!   `Global.Path` equivalent. The location is resolved the way
//!   [`crate::auth`] and [`crate::project`] already resolve their own
//!   state — same crate, same `ganja` directory under the XDG data
//!   home — landing on `<XDG data home>/ganja/tool-output/`.
//! - **The file name, and the 7-day cleanup sweep.** Upstream names the file
//!   with a `ToolID` (a sortable identifier tied to session bookkeeping this
//!   crate does not have yet) and prunes the directory hourly from a forked
//!   background fiber. Files here are named `tool_<hex timestamp>_<hex
//!   counter>` — unique and creation-ordered, which is all a stray file on
//!   disk actually needs — and nothing here deletes old ones; a periodic
//!   sweep is a background-job concern for whichever part of the engine ends
//!   up owning one, not a pure clamp function.
//!
//! A truncating clamp tries the ganja data directory first and a process
//! temp directory second — the "app data path" upstream anchors under has no
//! equivalent this port can always resolve (no `$HOME`, a read-only data
//! volume), and the prompt's promise that the file exists should survive
//! more than the single most common way to make it not exist. Only when
//! neither candidate can be written — no resolvable home directory, a full
//! disk, a path a stray file blocks — does this degrade to the pathless
//! notice, silently. The tool call already succeeded by the time truncation
//! runs; losing the overflow file is never a reason to fail it.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};

/// Upper bound on the bytes a tool result may carry. Upstream's `MAX_BYTES`
/// (`50 * 1024`); named `MAX_CHARS` because other tools in this crate quote
/// it as a budget on `&str::len()`, which is bytes, not `char`s.
pub const MAX_CHARS: usize = 50 * 1024;

/// Upper bound on the lines a tool result may carry. Upstream's `MAX_LINES`.
pub const MAX_LINES: usize = 2_000;

/// Directory ganja keeps its state in, under the XDG data home. Matches
/// [`crate::auth`] and [`crate::project`], which resolve their own state the
/// same way.
const DIRECTORY: &str = "ganja";

/// Where a truncating clamp spills its full text, under [`DIRECTORY`].
/// Upstream's `TRUNCATION_DIR`.
const TOOL_OUTPUT: &str = "tool-output";

/// A possibly-clamped tool output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Truncated {
    /// What survives, with a note appended when anything was cut.
    pub text: String,
    /// Whether anything was cut.
    pub truncated: bool,
}

/// Clamps `text` to the line and byte budgets, spilling the full original to
/// a file when anything was cut — the ganja data directory first, a temp
/// directory second if that could not be resolved or written to (see this
/// module's doc comment).
///
/// There is no error to report when neither candidate can be written: the
/// call this wraps already succeeded, and [`clamp_in`] degrades to the
/// pathless notice rather than fail it.
#[must_use]
pub fn clamp(text: &str) -> Truncated {
    clamp_in(text, candidate_dirs())
}

/// Same as [`clamp`], but spills to exactly `dir` — no XDG resolution, no
/// temp-dir fallback — so a caller can assert on the overflow file without
/// touching a real person's data directory, and so a test can force the
/// degraded path by pointing `dir` somewhere writing will fail.
#[must_use]
pub fn clamp_with(text: &str, dir: &Path) -> Truncated {
    clamp_in(text, [dir.to_owned()])
}

/// Shared implementation behind [`clamp`] and [`clamp_with`]: clamps `text`,
/// then writes it to the first of `dirs` that accepts it.
fn clamp_in(text: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Truncated {
    let Some(body) = clamp_body(text) else {
        return Truncated {
            text: text.to_owned(),
            truncated: false,
        };
    };

    let written = dirs.into_iter().find_map(|dir| write_overflow(&dir, text));
    let text = match written {
        Some(file) => format!("{body}\n\n{}", hint(&file)),
        None => body,
    };

    Truncated {
        text,
        truncated: true,
    }
}

/// The clamped preview and upstream's `...N {unit} truncated...` notice, or
/// [`None`] when `text` already fits both budgets.
///
/// Splitting on `\n` first and only ever rejoining whole lines is what keeps
/// a clamp from splitting a UTF-8 code point: every piece rejoined here was
/// already a valid `&str` before the split.
fn clamp_body(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let total_bytes = text.len();

    if lines.len() <= MAX_LINES && total_bytes <= MAX_CHARS {
        return None;
    }

    let mut bytes = 0_usize;
    let mut kept = 0_usize;
    let mut hit_bytes = false;
    for (index, line) in lines.iter().enumerate() {
        if index >= MAX_LINES {
            break;
        }
        let size = line.len() + usize::from(index > 0);
        if bytes + size > MAX_CHARS {
            hit_bytes = true;
            break;
        }
        bytes += size;
        kept = index + 1;
    }

    let preview = lines[..kept].join("\n");
    let removed = if hit_bytes {
        total_bytes - bytes
    } else {
        lines.len() - kept
    };
    let unit = if hit_bytes { "bytes" } else { "lines" };

    Some(format!("{preview}\n\n...{removed} {unit} truncated..."))
}

/// Tells the model where the full output went and how to read it without
/// pulling the whole thing back into context. Upstream's `hint`, minus the
/// branch for an agent with a Task tool: this port has not shipped one yet,
/// so every call is upstream's other branch — the one that points at `grep`
/// and `read` instead.
fn hint(file: &Path) -> String {
    format!(
        "The tool call succeeded but the output was truncated. Full output saved to: {}\n\
         Use Grep to search the full content or Read with offset/limit to view specific sections.",
        file.display()
    )
}

/// Writes `text` to a fresh file under `dir`, creating `dir` first if it
/// does not exist yet. [`None`] on any failure — the directory cannot be
/// created, the write fails — which is exactly the signal [`clamp_in`]
/// needs to fall back to the pathless notice instead.
fn write_overflow(dir: &Path, text: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let file = dir.join(overflow_filename());
    std::fs::write(&file, text).ok()?;
    Some(file)
}

/// The ganja data directory's `tool-output` subdirectory, or [`None`] when
/// there is no home directory to resolve it against.
fn default_dir() -> Option<PathBuf> {
    let base = Xdg::new().ok()?;
    Some(base.data_dir().join(DIRECTORY).join(TOOL_OUTPUT))
}

/// Directories a truncating [`clamp`] tries, in order: the resolved data
/// directory when there is one, then a process temp directory, which is
/// nearly always writable even where a home directory is not (a sandboxed
/// or read-only-home environment, for instance).
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(2);
    dirs.extend(default_dir());
    dirs.push(std::env::temp_dir().join(DIRECTORY).join(TOOL_OUTPUT));
    dirs
}

/// A name unique within a process and ordered by creation, `tool_`-prefixed
/// to match the prefix upstream's own cleanup sweep looks for (see this
/// module's doc comment for why the sweep itself is not ported).
///
/// The counter is what keeps two clamps in the same nanosecond from
/// colliding — a real possibility on a coarser clock, and free insurance
/// against one here regardless.
fn overflow_filename() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());

    format!("tool_{stamp:x}_{count:x}")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{MAX_CHARS, MAX_LINES, Truncated, clamp, clamp_with};

    /// The one file `dir` holds, panicking if that is not exactly true.
    fn only_entry(dir: &Path) -> PathBuf {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("the overflow directory was created")
            .map(|entry| entry.expect("a readable directory entry").path())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one overflow file in {dir:?}, got {entries:?}"
        );
        entries.remove(0)
    }

    #[test]
    fn candidate_dirs_tries_the_data_directory_before_the_temp_directory() {
        let dirs = super::candidate_dirs();

        assert!(
            !dirs.is_empty(),
            "the temp directory is always a candidate, even with no resolvable home"
        );
        let last = dirs.last().expect("checked non-empty above");
        assert_eq!(
            *last,
            std::env::temp_dir()
                .join(super::DIRECTORY)
                .join(super::TOOL_OUTPUT),
            "the temp directory is always the last resort"
        );
        if dirs.len() > 1 {
            assert_ne!(
                dirs[0], *last,
                "a resolvable data directory must be tried before the temp fallback"
            );
        }
    }

    #[test]
    fn short_output_passes_through_untouched() {
        assert_eq!(
            clamp("hello"),
            Truncated {
                text: "hello".to_owned(),
                truncated: false,
            }
        );
        assert_eq!(
            clamp(""),
            Truncated {
                text: String::new(),
                truncated: false,
            }
        );
    }

    #[test]
    fn exactly_at_both_budgets_is_not_truncated() {
        let at_lines = "a\n".repeat(MAX_LINES - 1) + "a";
        assert_eq!(at_lines.split('\n').count(), MAX_LINES);
        assert!(!clamp(&at_lines).truncated);

        let at_bytes = "x".repeat(MAX_CHARS);
        assert!(!clamp(&at_bytes).truncated);
    }

    #[test]
    fn output_over_the_byte_budget_is_cut_and_says_so() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let long = "x".repeat(MAX_CHARS + 1);

        let clamped = clamp_with(&long, dir.path());
        assert!(clamped.truncated);
        assert!(
            clamped.text.contains("bytes truncated"),
            "got {:?}",
            clamped.text
        );
    }

    #[test]
    fn output_over_the_line_budget_is_cut_at_the_budget() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let long = "line\n".repeat(MAX_LINES + 10);

        let clamped = clamp_with(&long, dir.path());
        assert!(clamped.truncated);
        assert!(
            clamped.text.contains("lines truncated"),
            "a line-budget cut reports lines, not bytes: {:?}",
            clamped.text
        );
        assert_eq!(
            clamped
                .text
                .lines()
                .take_while(|line| *line == "line")
                .count(),
            MAX_LINES
        );
    }

    #[test]
    fn a_clamp_never_splits_a_code_point() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let long = "\u{1F980}".repeat(MAX_CHARS);

        let clamped = clamp_with(&long, dir.path());
        assert!(clamped.truncated);
        assert!(clamped.text.contains("bytes truncated"));
        // The assertion of interest: building this `Truncated` at all did not
        // panic on a byte index that split the emoji's 4-byte encoding.
    }

    #[test]
    fn a_huge_single_line_keeps_no_preview_but_still_reports_the_full_size() {
        // No newline at all, so the line-count budget never applies; only the
        // byte budget can trigger, and it triggers on the very first line.
        let dir = tempfile::tempdir().expect("a scratch directory");
        let long = "x".repeat(MAX_CHARS * 2);

        let clamped = clamp_with(&long, dir.path());
        assert!(clamped.truncated);
        assert!(
            clamped
                .text
                .starts_with(&format!("\n\n...{} bytes truncated...", MAX_CHARS * 2)),
            "got {:?}",
            clamped.text
        );
    }

    #[test]
    fn the_overflow_file_holds_the_full_untouched_original_not_the_preview() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let long = "x".repeat(MAX_CHARS * 2);

        let clamped = clamp_with(&long, dir.path());

        let file = only_entry(dir.path());
        assert_eq!(
            std::fs::read_to_string(&file).expect("the overflow file was written"),
            long,
            "the file must hold everything, not just the clamped preview"
        );
        assert!(
            clamped.text.contains(&file.display().to_string()),
            "the notice must name the exact file that was written: {:?}",
            clamped.text
        );
    }

    #[test]
    fn the_notice_names_the_overflow_file_and_how_to_read_it() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let long = "line\n".repeat(MAX_LINES + 10);

        let clamped = clamp_with(&long, dir.path());

        assert!(
            clamped.text.contains(
                "The tool call succeeded but the output was truncated. Full output saved to:"
            ),
            "got {:?}",
            clamped.text
        );
        assert!(
            clamped
                .text
                .contains("Use Grep to search the full content or Read with offset/limit to view specific sections."),
            "got {:?}",
            clamped.text
        );
    }

    #[test]
    fn a_write_that_cannot_succeed_degrades_to_the_pathless_notice_rather_than_failing() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        // A regular file sits where the overflow directory would need to be
        // created, so `create_dir_all` fails on it — the same failure shape
        // as a read-only or missing home directory in the field.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").expect("the fixture writes");
        let long = "x".repeat(MAX_CHARS + 1);

        let clamped = clamp_with(&long, &blocked);

        assert!(clamped.truncated, "the budget was still exceeded");
        assert!(
            !clamped.text.contains("Full output saved to:"),
            "a failed write must degrade silently, not claim a file exists: {:?}",
            clamped.text
        );
        assert!(
            clamped.text.contains("bytes truncated"),
            "the pathless notice is still the one from clamp_body: {:?}",
            clamped.text
        );
    }
}
