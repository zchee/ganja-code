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

use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput, is_same_file};

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

        let requested = resolve(&ctx.cwd, args.path.as_deref());
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
        let store = ctx.credentials.clone();
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

/// Resolves `path` against `cwd` — absolute as given, relative joined to it,
/// or `cwd` itself when the call named no `path` at all.
fn resolve(cwd: &Path, path: Option<&str>) -> PathBuf {
    let Some(path) = path else {
        return cwd.to_owned();
    };
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
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
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{GrepTool, search};
    use crate::tool::{FileTimes, Tool, ToolCtx, ToolError, read::ReadTool};

    /// A context rooted at `cwd`, with a cancel nobody has pulled and the
    /// credential store the engine would have named sitting under it.
    fn ctx(cwd: PathBuf) -> ToolCtx {
        let credentials = cwd.join("ganja").join("auth.json");

        ToolCtx {
            cwd,
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: Some(credentials),
            spawn: None,
        }
    }

    #[tokio::test]
    async fn matches_are_grouped_by_file_with_line_numbers() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "struct Three;\n").unwrap();

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "fn \\w+" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a grep over a real directory succeeds");

        assert!(
            out.output.starts_with("Found 2 matches\n"),
            "got {:?}",
            out.output
        );
        assert!(
            out.output
                .contains("a.rs:\n  Line 1: fn one() {}\n  Line 2: fn two() {}")
        );
        assert!(!out.output.contains("b.rs"));
        assert_eq!(out.metadata["matches"], 2);
    }

    #[tokio::test]
    async fn every_match_names_its_file_by_absolute_path() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("a.rs"), "needle\n").expect("the fixture writes");

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "needle" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a grep over a real directory succeeds");

        let header = format!("{}:", dir.path().join("a.rs").display());
        assert!(
            out.output.contains(&header),
            "expected the header {header:?} in {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn a_path_taken_from_greps_output_reads_back_through_the_read_tool() {
        // The chain the model actually walks: grep, then read the file grep
        // named. Both calls share one context, as they do inside a turn, and
        // grep is pointed at a subdirectory — so a relative match path would
        // be resolved by `read` against the session directory rather than
        // against grep's search base, and would name a file that is not there.
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::create_dir(dir.path().join("nested")).expect("the fixture writes");
        std::fs::write(
            dir.path().join("nested").join("found.rs"),
            "fn needle() {}\n",
        )
        .expect("the fixture writes");
        let context = ctx(dir.path().to_owned());

        let found = GrepTool
            .run(
                serde_json::json!({ "pattern": "needle", "path": "nested" }),
                &context,
            )
            .await
            .expect("a grep over a subdirectory succeeds");

        // Lifted out of the output text the way the model lifts it: nothing
        // here rebuilds the path from what the fixture knows.
        let quoted = found
            .output
            .lines()
            .find_map(|line| line.strip_suffix(':'))
            .expect("grep heads each file's matches with that file's path");

        let read = ReadTool
            .run(serde_json::json!({ "filePath": quoted }), &context)
            .await
            .unwrap_or_else(|error| {
                panic!("read must accept the path grep printed ({quoted:?}): {error:?}")
            });

        assert!(
            read.output.contains("1: fn needle() {}"),
            "the chain must land on the file grep matched: {:?}",
            read.output
        );
    }

    #[tokio::test]
    async fn no_matches_says_so_plainly() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("a.rs"), "nothing interesting here").unwrap();

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "will-not-match-anything" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a zero-match search is still a successful call");

        assert_eq!(out.output, "No files found");
        assert_eq!(out.metadata["matches"], 0);
    }

    #[tokio::test]
    async fn the_include_pattern_filters_which_files_are_searched() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("a.rs"), "needle").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle").unwrap();

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "needle", "include": "*.rs" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("an include filter still succeeds");

        assert!(out.output.contains("a.rs"));
        assert!(!out.output.contains("b.txt"), "got {:?}", out.output);
    }

    #[tokio::test]
    async fn gitignored_files_are_not_searched() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        // `.gitignore` is only honored inside an actual git repository — the
        // `ignore` crate's `require_git` default, which real `rg` shares —
        // so the fixture needs a `.git` marker, not just the ignore file.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "needle").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "needle").unwrap();

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "needle" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a grep respecting .gitignore succeeds");

        assert!(out.output.contains("kept.rs"));
        assert!(!out.output.contains("ignored.rs"), "got {:?}", out.output);
    }

    #[tokio::test]
    async fn hidden_files_are_searched_unlike_glob() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join(".hidden.rs"), "needle").unwrap();

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "needle" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a grep over a directory with dotfiles succeeds");

        assert!(
            out.output.contains(".hidden.rs"),
            "grep's `--hidden` default differs from glob's: {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn an_empty_pattern_is_refused_as_a_bad_argument() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let refused = GrepTool
            .run(
                serde_json::json!({ "pattern": "" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("an empty pattern matches nothing meaningfully");

        assert!(
            matches!(&refused, ToolError::InvalidArgs(message) if message.contains("pattern is required")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn an_invalid_regex_is_refused_as_a_bad_argument() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let refused = GrepTool
            .run(
                serde_json::json!({ "pattern": "(unclosed" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("an unbalanced group is not a valid regex");

        assert!(
            matches!(refused, ToolError::InvalidArgs(_)),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn naming_a_file_as_path_searches_its_parent_directory() {
        // Documented upstream quirk: `path` naming a file does not restrict
        // the search to that file, because upstream never forwards a `file`
        // argument to ripgrep. It searches the file's parent directory.
        let dir = tempfile::tempdir().expect("a scratch directory");
        let target = dir.path().join("target.rs");
        std::fs::write(&target, "needle").unwrap();
        std::fs::write(dir.path().join("sibling.rs"), "needle").unwrap();

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "needle", "path": target.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a file path still resolves to a search");

        assert!(out.output.contains("target.rs"));
        assert!(
            out.output.contains("sibling.rs"),
            "a file `path` searches its parent directory, not just itself: {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn more_than_the_limit_is_capped_and_reported_truncated() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        for index in 0..150 {
            std::fs::write(dir.path().join(format!("f{index:04}.rs")), "needle\n").unwrap();
        }

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "needle" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a grep over many matches still succeeds, capped");

        assert_eq!(out.metadata["matches"], 100);
        assert_eq!(out.metadata["truncated"], true);
        assert!(
            out.output
                .contains("(Results truncated. Consider using a more specific path or pattern.)")
        );
        assert!(
            out.output
                .starts_with("Found 100 matches (more matches available)")
        );
    }

    #[tokio::test]
    async fn a_cancelled_call_stops_rather_than_searching_the_tree() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("a.rs"), "needle").unwrap();
        let context = ctx(dir.path().to_owned());
        context.cancel.cancel();

        let refused = GrepTool
            .run(serde_json::json!({ "pattern": "needle" }), &context)
            .await
            .expect_err("a pre-cancelled call must not return matches");

        assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
    }

    #[test]
    fn the_schema_requires_only_the_pattern() {
        let schema = serde_json::to_value(GrepTool.schema()).expect("a schema is JSON");

        assert_eq!(schema["required"], serde_json::json!(["pattern"]));
        for name in ["pattern", "path", "include"] {
            assert!(
                schema["properties"][name].is_object(),
                "missing {name}: {schema}"
            );
        }
    }

    #[test]
    fn the_credential_store_contributes_no_line_to_a_search() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let store = dir.path().join("auth.json");
        std::fs::write(&store, "{ \"anthropic\": { \"key\": \"sk-canary-8842\" } }")
            .expect("the fixture writes");
        std::fs::write(dir.path().join("notes.md"), "sk-canary-8842 was rotated\n")
            .expect("the fixture writes");

        let found = search(
            dir.path(),
            "sk-canary-8842",
            None,
            Some(&store),
            &CancellationToken::new(),
        )
        .expect("the search runs");

        let hits: Vec<(&str, u64)> = found
            .iter()
            .map(|item| (item.path.as_str(), item.line))
            .collect();

        let notes = dir.path().join("notes.md").display().to_string();
        assert_eq!(
            hits,
            vec![(notes.as_str(), 1)],
            "the store must contribute nothing, and a sibling must still match"
        );
    }

    #[tokio::test]
    async fn a_project_file_that_only_shares_the_stores_name_is_still_searched() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("auth.json"), "needle").expect("the fixture writes");

        let out = GrepTool
            .run(
                serde_json::json!({ "pattern": "needle" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("the guard is identity-based: any other auth.json is still searched");

        assert!(out.output.contains("auth.json"), "got {:?}", out.output);
    }
}
