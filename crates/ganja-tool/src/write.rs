//! The `write` tool.
//!
//! Spec: upstream `packages/opencode/src/tool/write.ts` and `write.txt`.
//!
//! The file is opened through the directory holding it rather than by name
//! (`tool/anchor.rs`), which upstream does not do: a link planted at the path
//! between the permission dialog's answer and the write has nothing left to
//! redirect, and a link *at* the file's own name is refused rather than
//! followed.
//!
//! Upstream's diff-for-permission-prompt, format-on-write and LSP
//! diagnostics reporting lean on two services this port has nothing for at
//! the tool layer — `Format.Service` and `LSP.Service`, neither wired into
//! [`ToolCtx`] — and on upstream's permission asker, which is not
//! [`ToolCtx::ask`]: that seam is the `question` tool's, for asking a person
//! something, and the permission dialog is the engine's. BOM preservation is
//! a separate absence and needs no service at all: `edit` keeps a file's mark
//! from the file's own bytes (`join_bom`) and `write` does not. So the base
//! case upstream falls back to without those services,
//! `"Wrote file successfully."`, is exactly what this port always returns.

use std::io::Write as _;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::anchor::{self, Anchor};
use crate::{Tool, ToolCtx, ToolError, ToolOutput, display, resolve};

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
    fn id(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        include_str!("write.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let path = args.get("filePath").and_then(serde_json::Value::as_str).unwrap_or_default();

        format!("write {path}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let filepath = resolve(&ctx.cwd, &args.file_path);
        let title = display(&ctx.cwd, &filepath);
        // Before anything touches the disk, including the parent directories
        // created below — materialising a path is itself a way through a link.
        anchor::refuse_link_escape(&ctx.cwd, &filepath)?;

        // From here the file is addressed through a directory this call holds
        // open (`tool/anchor.rs`): the second containment check, the freshness
        // stamp and the write all speak to that one descriptor, so a link
        // planted at the name after the permission dialog was answered has
        // nothing left to redirect. Missing parents are made under the same
        // anchor rather than by `create_dir_all`, whose own walk would resolve
        // those names afresh.
        let anchor = Anchor::open(&filepath, true)?;
        anchor::refuse_anchor_escape(&ctx.cwd, &filepath, &anchor)?;
        let (mut file, existed) = anchor.write()?;

        // Upstream's "you MUST use the Read tool first" rule, enforced only
        // for a file that already exists — a brand-new file has nothing on
        // disk a stale read could have missed. Nothing has been truncated
        // yet: a refusal here leaves the file exactly as it was.
        if existed {
            ctx.files.check_fresh_stat(&filepath, anchor::stamp(&file))?;
            file.set_len(0).map_err(|error| {
                ToolError::Failed(format!("could not write {}: {error}", filepath.display()))
            })?;
        }

        file.write_all(args.content.as_bytes()).map_err(|error| {
            ToolError::Failed(format!("could not write {}: {error}", filepath.display()))
        })?;

        // A write is also a read, as far as freshness goes: the model now
        // knows exactly what is on disk, so an immediate follow-up edit
        // should not be refused as stale.
        //
        // The ordering is load-bearing for `crate::watch`, not only for the
        // next call: this write is about to arrive back as a filesystem event,
        // and the watcher decides staleness by comparing the file's stamp
        // against the recorded one. Recorded here — before that event can be
        // processed, because it happens inside the call that caused it — the
        // agent's own write compares clean. Recorded any later and the session
        // would condemn its own edits.
        ctx.files.record_stat(&filepath, anchor::stamp(&file));

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

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
