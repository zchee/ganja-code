//! The `write` tool.
//!
//! Spec: upstream `packages/opencode/src/tool/write.ts` and `write.txt`.
//!
//! Upstream's diff-for-permission-prompt, BOM preservation, format-on-write
//! and LSP diagnostics reporting all lean on services this port does not
//! have at the tool layer (`ctx.ask`, `Format.Service`, `LSP.Service`) —
//! none of them are wired into [`ToolCtx`], so the base case upstream falls
//! back to without those services, `"Wrote file successfully."`, is exactly
//! what this port always returns.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput};

/// What the model passes to `write`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The content to write to the file
    content: String,
    /// The absolute path to the file to write (must be absolute, not relative)
    #[serde(rename = "filePath")]
    file_path: String,
}

/// Writes a file to disk.
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn id(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &str {
        include_str!("write.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let path = args
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        format!("write {path}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let filepath = resolve(&ctx.cwd, &args.file_path);
        let title = display(&ctx.cwd, &filepath);

        // Upstream's "you MUST use the Read tool first" rule, enforced only
        // for a file that already exists — a brand-new file has nothing on
        // disk a stale read could have missed.
        let existed = filepath.exists();
        if existed {
            ctx.files.check_fresh(&filepath)?;
        }

        if let Some(parent) = filepath.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                ToolError::Failed(format!(
                    "could not create directory {}: {error}",
                    parent.display()
                ))
            })?;
        }

        std::fs::write(&filepath, &args.content).map_err(|error| {
            ToolError::Failed(format!("could not write {}: {error}", filepath.display()))
        })?;

        // A write is also a read, as far as freshness goes: the model now
        // knows exactly what is on disk, so an immediate follow-up edit
        // should not be refused as stale.
        ctx.files.record(&filepath);

        Ok(ToolOutput {
            title,
            output: "Wrote file successfully.".to_owned(),
            metadata: serde_json::json!({
                "filepath": filepath,
                "exists": existed,
            }),
        })
    }
}

/// Resolves `file_path` against `cwd` — never against the process cwd, so a
/// relative argument means what the call site meant, not what the engine's
/// own working directory happens to be.
fn resolve(cwd: &Path, file_path: &str) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

/// `path` relative to `cwd` when it is under it, absolute otherwise — for a
/// title or one-line description a person can actually read.
fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::WriteTool;
    use crate::tool::{FileTimes, Tool, ToolCtx, ToolError};

    /// A context rooted at `cwd`, with a fresh, empty read log.
    fn ctx(cwd: PathBuf) -> ToolCtx {
        ToolCtx {
            cwd,
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
        }
    }

    #[tokio::test]
    async fn a_new_file_is_created_without_having_been_read() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("fresh.txt");

        let out = WriteTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "content": "hello" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a brand-new file needs no prior read");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert_eq!(out.output, "Wrote file successfully.");
        assert_eq!(out.title, "fresh.txt");
        assert_eq!(out.metadata["exists"], false);
    }

    #[tokio::test]
    async fn parent_directories_are_created_as_needed() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a/b/c/deep.txt");

        WriteTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "content": "x" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("missing parents are created rather than refused");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
    }

    #[tokio::test]
    async fn overwriting_an_existing_file_without_reading_it_first_is_refused() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original").expect("the fixture writes");

        let refused = WriteTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "content": "new" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("an unread existing file must not be silently overwritten");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("read it first")),
            "got {refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "original",
            "a refused write must not touch the file"
        );
    }

    #[tokio::test]
    async fn a_file_read_first_may_be_overwritten_and_becomes_fresh_again() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original").expect("the fixture writes");
        let context = ctx(dir.path().to_owned());
        context.files.record(&path);

        let out = WriteTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "content": "updated" }),
                &context,
            )
            .await
            .expect("a file read this session may be overwritten");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
        assert_eq!(out.metadata["exists"], true);

        // The write itself counts as a fresh read, so a second write right
        // after does not need another explicit read in between.
        context
            .files
            .check_fresh(&path)
            .expect("a write leaves the file fresh for the next call");
    }

    #[tokio::test]
    async fn a_relative_path_resolves_against_the_call_cwd() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let out = WriteTool
            .run(
                serde_json::json!({ "filePath": "relative.txt", "content": "hi" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a relative filePath resolves against ctx.cwd");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("relative.txt")).unwrap(),
            "hi"
        );
        assert_eq!(out.title, "relative.txt");
    }

    #[test]
    fn the_schema_requires_content_and_file_path() {
        let schema = serde_json::to_value(WriteTool.schema()).expect("a schema is JSON");

        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .expect("write has required arguments")
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["content", "filePath"]);
    }
}
