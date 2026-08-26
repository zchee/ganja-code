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

use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, display, resolve_or_cwd};

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
        let search = resolve_or_cwd(&ctx.cwd, args.path.as_deref());
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
/// gitignored file directly still finds it — see `glob_tests.rs`.
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
#[path = "glob_tests.rs"]
mod tests;
