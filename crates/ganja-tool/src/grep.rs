//! The `grep` tool.
//!
//! Spec: upstream `packages/opencode/src/tool/grep.ts`, `grep.txt`, and
//! `packages/core/src/ripgrep.ts` for the search this tool's
//! `Ripgrep.Service` shells out to (`rg --json --hidden ... -- pattern .`).
//!
//! This port searches in-process with `grep-searcher`/`grep-regex` over an
//! `ignore`-crate walk instead of spawning `rg` — the same regex engine and
//! ignore-file handling ripgrep itself is built on. Match order (and so
//! which consecutive matches get grouped under one path header) is sorted by
//! path here, for the same determinism reason `glob` sorts: no explicit sort
//! exists in this pinned upstream, only whatever order `rg --json` happened
//! to emit results in.
//!
//! One upstream quirk survives the port intact rather than being "fixed":
//! naming a specific *file* as `path` does not restrict the search to that
//! file. `tool/grep.ts` never forwards a `file` argument to
//! `Ripgrep.Service.grep`, so ripgrep always searches `.` under whatever
//! `cwd` was computed for the call — which, when `path` names a file, is
//! that file's *parent directory*. `path` is documented as "the directory to
//! search in"; this is what actually happens when it names a file instead.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use grep_searcher::sinks::UTF8;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, is_same_file, resolve_or_cwd};

/// Most matches a call returns. Upstream's `limit` in `tool/grep.ts`.
const LIMIT: usize = 100;

/// Longest a single matched line's text may be before it is cut. Upstream's
/// `2_000` in `ripgrep.ts`'s `grep` result mapping.
const MAX_MATCH_TEXT: usize = 2_000;

/// What the model passes to `grep`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The regex pattern to search for in file contents
    pattern: String,
    /// The directory to search in. Defaults to the current working directory.
    #[serde(default)]
    path: Option<String>,
    /// File pattern to include in the search (e.g. "*.js", "*.{ts,tsx}")
    #[serde(default)]
    include: Option<String>,
}

/// One matched line.
struct Match {
    /// The matched file, as an absolute path — upstream's
    /// `path.resolve(<search base>, item.entry.path)`.
    path: String,
    /// 1-indexed line number within that file.
    line: u64,
    /// The line's text, cut to [`MAX_MATCH_TEXT`] characters.
    text: String,
}

/// Searches file contents by regular expression.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn id(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        include_str!("grep.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let pattern = args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        format!("grep {pattern}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        if args.pattern.is_empty() {
            return Err(ToolError::InvalidArgs("pattern is required".to_owned()));
        }
        let title = args.pattern.clone();

        let requested = resolve_or_cwd(&ctx.cwd, args.path.as_deref());
        // Upstream searches the requested directory itself, or — the quirk
        // documented at the top of this file — the parent directory of a
        // requested *file*.
        let base_dir = if requested.is_dir() {
            requested
        } else {
            requested
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_owned)
        };
        // What the model does with a match is read the file, and `read`
        // resolves a relative argument against the *session* directory, not
        // against grep's search base — so a relative match path names a
        // different file, or none at all. Upstream never hands one out:
        // `tool/grep.ts` maps every row through
        // `path.resolve(<base>, item.entry.path)`. Absolutising the base is
        // what makes every path below absolute, because a match is reported as
        // its walk path under this directory, and `std::path::absolute` is
        // `path.resolve`'s operation — process-directory fallback included,
        // which the engine's own `cwd` needs when `current_dir()` fails.
        let base_dir = std::path::absolute(&base_dir).unwrap_or(base_dir);

        let pattern = args.pattern;
        let include = args.include;
        let cancel = ctx.cancel.clone();
        let searched = base_dir.clone();
        // Owned before the walk rather than borrowed into it: the search runs
        // on a blocking thread that outlives this call's context, and the walk
        // compares every file in the tree against it.
        let store = ctx.credentials.guarded().map(Path::to_owned);
        let matches = tokio::task::spawn_blocking(move || {
            search(
                &searched,
                &pattern,
                include.as_deref(),
                store.as_deref(),
                &cancel,
            )
        })
        .await
        .map_err(|error| ToolError::Failed(format!("the grep search did not finish: {error}")))??;

        if matches.is_empty() {
            return Ok(ToolOutput {
                title,
                output: "No files found".to_owned(),
                metadata: serde_json::json!({ "matches": 0, "truncated": false }),
            });
        }

        // Upstream's own quirk, preserved rather than fixed: `tool/grep.ts`
        // reconstructs `truncated` from `rows.length === limit` after the
        // list already came back capped, so an exact `limit`-sized result
        // with nothing left over is still reported as truncated.
        let truncated = matches.len() == LIMIT;
        let total = matches.len();

        let mut lines = vec![format!(
            "Found {total} matches{}",
            if truncated {
                " (more matches available)"
            } else {
                ""
            }
        )];

        let mut current: Option<&str> = None;
        for item in &matches {
            if current != Some(item.path.as_str()) {
                if current.is_some() {
                    lines.push(String::new());
                }
                current = Some(&item.path);
                lines.push(format!("{}:", item.path));
            }
            lines.push(format!("  Line {}: {}", item.line, item.text));
        }

        if truncated {
            lines.push(String::new());
            lines.push(
                "(Results truncated. Consider using a more specific path or pattern.)".to_owned(),
            );
        }

        Ok(ToolOutput {
            title,
            output: lines.join("\n"),
            metadata: serde_json::json!({
                "matches": total,
                "truncated": truncated,
            }),
        })
    }
}

/// Searches every file under `base_dir` for `pattern`, honoring `include`
/// when given, sorted by path and capped to [`LIMIT`] matches.
///
/// Each match names its file by the walk's path for it, which is `base_dir`
/// plus the path under it — so an absolute `base_dir` is what makes the
/// reported paths absolute, as upstream's row mapping does.
///
/// `store` is ganja's own credential store, and the one file the walk steps
/// over: `grep` runs without asking and prints the lines it matched, so a
/// search that reached it would hand the model this machine's provider API keys
/// one line at a time.
///
/// Runs on a blocking thread: the walk and `grep-searcher` are both
/// synchronous. `cancel` is polled between batches — once per file while
/// searching content, more often while just enumerating files, since listing
/// is cheaper per step than searching.
fn search(
    base_dir: &Path,
    pattern: &str,
    include: Option<&str>,
    store: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<Vec<Match>, ToolError> {
    let matcher = grep_regex::RegexMatcher::new(pattern)
        .map_err(|error| ToolError::InvalidArgs(format!("invalid pattern {pattern:?}: {error}")))?;

    let mut builder = ignore::WalkBuilder::new(base_dir);
    // Upstream's grep passes `--hidden` to ripgrep, unlike glob: hidden
    // files and directories are searched by default here too.
    builder.hidden(false);
    if let Some(include) = include {
        let mut overrides = ignore::overrides::OverrideBuilder::new(base_dir);
        overrides.add(include).map_err(|error| {
            ToolError::InvalidArgs(format!("invalid include pattern {include:?}: {error}"))
        })?;
        let overrides = overrides.build().map_err(|error| {
            ToolError::InvalidArgs(format!("invalid include pattern {include:?}: {error}"))
        })?;
        builder.overrides(overrides);
    }

    let mut files: Vec<PathBuf> = Vec::new();
    for (checked, entry) in builder.build().enumerate() {
        if checked % 256 == 0 && cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            let path = entry.into_path();
            if store.is_some_and(|store| is_same_file(&path, store)) {
                continue;
            }
            files.push(path);
        }
    }
    files.sort_unstable();

    let mut searcher = grep_searcher::SearcherBuilder::new()
        .line_number(true)
        .binary_detection(grep_searcher::BinaryDetection::quit(0))
        .build();

    let mut matches: Vec<Match> = Vec::new();
    for (checked, path) in files.iter().enumerate() {
        if checked % 32 == 0 && cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if matches.len() > LIMIT {
            break;
        }

        // The walk's own path, which carries `base_dir` as its prefix, rather
        // than a relative form: this is where a match becomes something the
        // model can hand straight to `read`. Nothing here answers `ripgrep.ts`'s
        // `replaceAll("\\", "/")` on the relative path, because the
        // `path.resolve` it feeds puts the platform's own separator back — so
        // this platform's separator is what upstream ends up printing too.
        let reported = path.display().to_string();
        // A file that cannot be searched — permission denied, a race with a
        // delete, or content `grep-searcher` refuses as binary — is skipped
        // rather than failing the whole call, matching upstream's
        // `--no-messages` (which suppresses exactly these warnings).
        let _ = searcher.search_path(
            &matcher,
            path,
            UTF8(|line_number, line| {
                // Deliberate divergence, and a shipped decision rather than an
                // oversight: upstream keeps the line terminator that ripgrep's
                // `lines.text` carries (`packages/core/src/ripgrep.ts:267` hands
                // the field through untouched), and since `tool/grep.ts` then
                // joins its rows with `\n`, upstream's output ends every match
                // row with a blank line — including one *between* consecutive
                // matches in the same file. Trimming per line is what makes this
                // port's output readable, and readable is what was chosen.
                //
                // The golden differential compares grep's output against
                // upstream's and therefore has to forgive exactly this one
                // difference; `tests/golden.rs` names and documents that
                // exception. Changing the trim here without changing it there
                // will fail that comparison.
                let text = clamp_match_text(line.trim_end_matches(['\n', '\r']));
                matches.push(Match {
                    path: reported.clone(),
                    line: line_number,
                    text,
                });
                Ok(matches.len() <= LIMIT)
            }),
        );
    }

    matches.truncate(LIMIT);
    Ok(matches)
}

/// `text`, cut to [`MAX_MATCH_TEXT`] characters with upstream's `...`
/// suffix appended when it was too long.
fn clamp_match_text(text: &str) -> String {
    if text.chars().count() <= MAX_MATCH_TEXT {
        return text.to_owned();
    }

    let mut kept: String = text.chars().take(MAX_MATCH_TEXT).collect();
    kept.push_str("...");
    kept
}

#[cfg(test)]
#[path = "grep_tests.rs"]
mod tests;
