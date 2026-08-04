//! The `glob` tool.
//!
//! Spec: upstream `packages/opencode/src/tool/glob.ts`, `glob.txt`, and
//! `packages/core/src/ripgrep.ts` for the walk this tool's `Ripgrep.Service`
//! shells out to (`rg --files --glob=<pattern> --glob=!**/.git/**`).
//!
//! This port walks in-process with the `ignore` crate instead of spawning
//! `rg`, sharing the same gitignore-aware matching engine ripgrep itself is
//! built on — the same crate that supplies `--glob`'s semantics there
//! supplies `Override`'s here, so a pattern like `*.ts` or `src/**/*.ts`
//! matches exactly as it would under `rg --files -g <pattern>`.
//!
//! Upstream's own result order is whatever `rg --files` happened to emit —
//! this pinned `ripgrep.ts` has no explicit sort for `glob` at all, so the
//! order is walk-order-dependent and not reproducible across platforms or
//! file systems. Results here are sorted by relative path instead: a
//! deliberate improvement for determinism (and testability), not a literal
//! port of a sort key that does not exist upstream.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{Tool, ToolCtx, ToolError, ToolOutput};

/// Most paths a call returns. Upstream's `limit` in `tool/glob.ts`.
const LIMIT: usize = 100;

/// What the model passes to `glob`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The glob pattern to match files against
    pattern: String,
    /// The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter "undefined" or "null" - simply omit it for the default behavior. Must be a valid directory path if provided.
    #[serde(default)]
    path: Option<String>,
}

/// Finds files by glob pattern.
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn id(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        include_str!("glob.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let pattern = args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        match args.get("path").and_then(serde_json::Value::as_str) {
            Some(path) => format!("glob {pattern} in {path}"),
            None => format!("glob {pattern}"),
        }
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let search = resolve(&ctx.cwd, args.path.as_deref());
        let title = display(&ctx.cwd, &search);

        if search.is_file() {
            return Err(ToolError::Failed(format!(
                "glob path must be a directory: {}",
                search.display()
            )));
        }

        let pattern = args.pattern;
        let cancel = ctx.cancel.clone();
        let walked = search.clone();
        let matches = tokio::task::spawn_blocking(move || walk(&walked, &pattern, &cancel))
            .await
            .map_err(|error| {
                ToolError::Failed(format!("the glob walk did not finish: {error}"))
            })??;

        // Upstream's own quirk, preserved rather than fixed: `tool/glob.ts`
        // reconstructs `truncated` from `files.length === limit` after the
        // list already came back capped, so an exact `limit`-sized result
        // with nothing left over is still reported as truncated.
        let truncated = matches.len() == LIMIT;

        let output = if matches.is_empty() {
            "No files found".to_owned()
        } else {
            let mut lines: Vec<String> = matches
                .iter()
                .map(|relative| search.join(relative).display().to_string())
                .collect();
            if truncated {
                lines.push(String::new());
                lines.push(format!(
                    "(Results are truncated: showing first {LIMIT} results. Consider using a more specific path or pattern.)"
                ));
            }
            lines.join("\n")
        };

        Ok(ToolOutput {
            title,
            output,
            metadata: serde_json::json!({
                "count": matches.len(),
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

/// `path` relative to `cwd` when it is under it, absolute otherwise.
fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

/// Walks `search` for files matching `pattern`, sorted by relative path and
/// capped to [`LIMIT`].
///
/// Runs on a blocking thread: `ignore::Walk` is a synchronous iterator, and a
/// large tree is real, uninterruptible work that must not sit on the async
/// runtime. `cancel` is polled between batches of entries rather than after
/// every one, since [`CancellationToken::is_cancelled`] is cheap but not
/// free, and a directory this large is exactly the case worth checking.
///
/// A pattern that matches a file overrides both the default hidden-file
/// exclusion and `.gitignore` — confirmed against a real `rg` binary, not
/// assumed. `ignore::WalkBuilder` documents its own precedence as stopping
/// at the first override match ("glob overrides are checked[, and if] a
/// path matches a glob override, then matching stops"), which runs before
/// either the ignore-file check or the hidden check. Upstream's `--glob`
/// flag is the identical mechanism, so a pattern naming a dotfile or a
/// gitignored file directly still finds it — see the tests below.
fn walk(
    search: &Path,
    pattern: &str,
    cancel: &CancellationToken,
) -> Result<Vec<String>, ToolError> {
    let mut overrides = ignore::overrides::OverrideBuilder::new(search);
    overrides.add(pattern).map_err(|error| {
        ToolError::InvalidArgs(format!("invalid glob pattern {pattern:?}: {error}"))
    })?;
    let overrides = overrides.build().map_err(|error| {
        ToolError::InvalidArgs(format!("invalid glob pattern {pattern:?}: {error}"))
    })?;

    let mut builder = ignore::WalkBuilder::new(search);
    // `hidden(true)` (the crate's default) matches upstream's lack of a
    // `--hidden` flag on `rg --files`. It only ever applies to an entry the
    // override did *not* match, per the precedence note above.
    builder.hidden(true).overrides(overrides);

    let mut matches = Vec::new();
    for (checked, entry) in builder.build().enumerate() {
        if checked % 256 == 0 && cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let relative = entry.path().strip_prefix(search).unwrap_or(entry.path());
        matches.push(to_slash(relative));
    }

    matches.sort_unstable();
    matches.truncate(LIMIT);
    Ok(matches)
}

/// `path`'s components joined with `/`, regardless of the platform's own
/// separator — matching upstream's explicit `replaceAll("\\", "/")`.
fn to_slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::GlobTool;
    use crate::{FileTimes, Tool, ToolCtx, ToolError};

    /// A context rooted at `cwd`, with a cancel nobody has pulled.
    fn ctx(cwd: PathBuf) -> ToolCtx {
        ToolCtx {
            cwd,
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: crate::Credentials::Unguarded,
            spawn: None,
        }
    }

    #[tokio::test]
    async fn matching_files_are_returned_as_absolute_paths_sorted_by_relative_path() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/a.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();

        let out = GlobTool
            .run(
                serde_json::json!({ "pattern": "**/*.rs" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a glob over a real directory succeeds");

        let expected = format!(
            "{}\n{}",
            dir.path().join("b.rs").display(),
            dir.path().join("nested/a.rs").display()
        );
        assert_eq!(out.output, expected);
        assert_eq!(out.metadata["count"], 2);
        assert_eq!(out.metadata["truncated"], false);
    }

    #[tokio::test]
    async fn no_matches_says_so_plainly() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let out = GlobTool
            .run(
                serde_json::json!({ "pattern": "*.nonexistent" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("an empty match set is still a successful call");

        assert_eq!(out.output, "No files found");
        assert_eq!(out.metadata["count"], 0);
    }

    #[tokio::test]
    async fn a_pattern_matching_a_gitignored_file_includes_it_even_inside_a_real_repo() {
        // Real `rg --files -g '*.rs' --glob='!**/.git/**' .` was run against
        // this exact fixture shape while porting this tool, inside an actual
        // git repository: the gitignored file still came back. An explicit
        // pattern match overrides `.gitignore`, per `walk`'s doc comment.
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "").unwrap();

        let out = GlobTool
            .run(
                serde_json::json!({ "pattern": "*.rs" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a glob inside a git repository succeeds");

        assert!(out.output.contains("kept.rs"));
        assert!(
            out.output.contains("ignored.rs"),
            "an explicit pattern match overrides .gitignore too: {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn a_pattern_matching_a_hidden_file_includes_it_despite_no_hidden_flag() {
        // Same override-wins precedence, confirmed against the real `rg`
        // binary while porting this tool: `rg --files -g '*.rs'` (no
        // `--hidden`) still lists a dotfile the pattern names explicitly.
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join(".hidden.rs"), "").unwrap();
        std::fs::write(dir.path().join("visible.rs"), "").unwrap();

        let out = GlobTool
            .run(
                serde_json::json!({ "pattern": "*.rs" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a glob over a directory with dotfiles succeeds");

        assert!(out.output.contains("visible.rs"));
        assert!(
            out.output.contains(".hidden.rs"),
            "an explicit pattern match overrides the hidden default: {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn a_hidden_file_not_matching_the_pattern_stays_excluded() {
        // The non-degenerate case: the override only short-circuits the
        // hidden check for entries it actually matches. A dotfile the
        // pattern never names is excluded exactly as the default would.
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join(".env"), "").unwrap();
        std::fs::write(dir.path().join("visible.rs"), "").unwrap();

        let out = GlobTool
            .run(
                serde_json::json!({ "pattern": "*.rs" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a glob over a directory with a non-matching dotfile succeeds");

        assert!(out.output.contains("visible.rs"));
        assert!(!out.output.contains(".env"), "got {:?}", out.output);
    }

    #[tokio::test]
    async fn a_path_argument_naming_a_file_is_refused() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let file = dir.path().join("not-a-directory.txt");
        std::fs::write(&file, "").unwrap();

        let refused = GlobTool
            .run(
                serde_json::json!({ "pattern": "*", "path": file.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("globbing a file path is not a directory search");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("must be a directory")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn more_than_the_limit_is_capped_and_reported_truncated() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        for index in 0..150 {
            std::fs::write(dir.path().join(format!("f{index:04}.rs")), "").unwrap();
        }

        let out = GlobTool
            .run(
                serde_json::json!({ "pattern": "*.rs" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a glob over many files still succeeds, capped");

        assert_eq!(out.metadata["count"], 100);
        assert_eq!(out.metadata["truncated"], true);
        assert!(
            out.output
                .contains("Results are truncated: showing first 100 results")
        );
    }

    #[tokio::test]
    async fn an_invalid_pattern_is_refused_as_a_bad_argument() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let refused = GlobTool
            .run(
                serde_json::json!({ "pattern": "[" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("an unclosed character class is not a valid glob");

        assert!(
            matches!(refused, ToolError::InvalidArgs(_)),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn a_relative_path_argument_resolves_against_the_call_cwd() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/x.rs"), "").unwrap();

        let out = GlobTool
            .run(
                serde_json::json!({ "pattern": "*.rs", "path": "nested" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a relative path resolves against ctx.cwd, not the process cwd");

        assert_eq!(
            out.output,
            dir.path().join("nested/x.rs").display().to_string()
        );
    }

    #[tokio::test]
    async fn a_cancelled_call_stops_rather_than_walking_the_tree() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        let context = ctx(dir.path().to_owned());
        context.cancel.cancel();

        let refused = GlobTool
            .run(serde_json::json!({ "pattern": "*.rs" }), &context)
            .await
            .expect_err("a pre-cancelled call must not return a match list");

        assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
    }

    #[test]
    fn the_schema_requires_only_the_pattern() {
        let schema = serde_json::to_value(GlobTool.schema()).expect("a schema is JSON");

        assert_eq!(schema["required"], serde_json::json!(["pattern"]));
        assert!(schema["properties"]["pattern"].is_object());
        assert!(schema["properties"]["path"].is_object());
    }
}
