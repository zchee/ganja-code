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

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    project::Project,
    tool::{Tool, ToolCtx, ToolError, ToolOutput},
};

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
        // Before anything touches the disk, including the parent directories
        // created below — materialising a path is itself a way through a link.
        refuse_link_escape(&ctx.cwd, &filepath)?;

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

/// Refuses a path that is inside the project by its text but lands outside it
/// once the filesystem has its say — that is, one a symbolic link redirects.
///
/// The permission gate resolves the same path and asks about it
/// (`permission.rs`, `outside`), but it answers *before* the call runs: a link
/// planted between the answer and the write redirects it afterwards, and a
/// [`crate::permission::Permissions::default`] set has no project to compare
/// anything against at all. `fs::write` follows links, so this is the check
/// the write itself makes, immediately before opening the file.
///
/// Only a link that *escapes* is refused. A call that openly names somewhere
/// outside the project is the gate's business and not this function's — the
/// user was asked about that directory and may well have allowed it — so the
/// rule is precisely the one this tool's contract states: do not follow a link
/// out of the project. Text inside plus reality outside is that link and
/// nothing else.
///
/// Resolution mirrors `permission::resolve` deliberately, so the gate and the
/// tool cannot disagree about where a path goes: canonicalise what exists, so
/// every link along it and every `..` is already applied, then apply what does
/// not exist yet by text — a `..` there cannot cross a link, because nothing
/// it names is on the disk to be one.
///
/// The window between this check and the open is narrowed, not closed. Only
/// opening the canonical parent with `O_NOFOLLOW` would close it, which this
/// port does not do yet; what is refused here is the escape that exists when
/// the call is made, which is the shape a planted link takes.
///
/// The twin of this lives in `tool/edit.rs`. The two are deliberately
/// duplicated rather than shared: exactly two tools write, and the alternative
/// is a module whose only job is to hold one function.
fn refuse_link_escape(cwd: &Path, path: &Path) -> Result<(), ToolError> {
    let root = Project::resolve(cwd).root().to_owned();
    let real = real_path(path);
    if real.starts_with(&root) {
        return Ok(());
    }
    if !claimed(cwd, path).starts_with(&root) {
        return Ok(());
    }

    Err(ToolError::Failed(format!(
        "{} is inside the project but resolves to {}, which is not: a symbolic \
         link on that path leads out of {}. Refusing to write through it — name \
         the real path if that is what you meant.",
        path.display(),
        real.display(),
        root.display()
    )))
}

/// Where `path` really lands: canonical for as much of it as exists, lexical
/// for whatever does not exist yet.
fn real_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let mut ancestors: Vec<Component> = path.components().collect();
    let mut rest: Vec<Component> = Vec::new();
    while let Some(component) = ancestors.pop() {
        rest.push(component);
        let existing: PathBuf = ancestors.iter().collect();
        if existing.as_os_str().is_empty() {
            continue;
        }
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            return apply(canonical, rest.iter().rev().copied());
        }
    }

    lexical(path)
}

/// What the call claimed the path was, before any link was followed: the
/// session directory in canonical form, with everything the call added applied
/// by text.
///
/// Only the anchor is canonicalised, and that asymmetry is the whole point —
/// the anchor is the one part of the path the model did not choose. Comparing
/// raw text against a canonical root would be wrong in the other direction on
/// any machine where the session directory is reached through a link, which on
/// macOS is every temporary directory there is (`/var` -> `/private/var`):
/// every path under it would read as "outside the project" and every write
/// would be refused.
fn claimed(cwd: &Path, path: &Path) -> PathBuf {
    let anchor = std::fs::canonicalize(cwd).unwrap_or_else(|_| lexical(cwd));

    match path.strip_prefix(cwd) {
        Ok(rest) => apply(anchor, rest.components()),
        Err(_) => lexical(path),
    }
}

/// `path` made absolute with its `.` and `..` applied by text alone, which is
/// what it claims to be before the filesystem is consulted.
fn lexical(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());

    apply(PathBuf::new(), absolute.components())
}

/// `base` with `rest` applied by text: `.` ignored, `..` popped, anything else
/// pushed.
fn apply<'a>(mut base: PathBuf, rest: impl Iterator<Item = Component<'a>>) -> PathBuf {
    for component in rest {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                base.pop();
            }
            other => base.push(other.as_os_str()),
        }
    }

    base
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
            spawn: None,
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

    /// A project whose root is pinned by a `.git`, and somewhere outside it.
    ///
    /// The marker is what makes the boundary deterministic: `Project::resolve`
    /// walks up for one, so without it the root would be whatever ancestor of
    /// the temporary directory happened to be a checkout — and on a machine
    /// whose `TMPDIR` sits inside one, that ancestor would contain "outside"
    /// too and these tests would quietly stop testing anything.
    #[cfg(unix)]
    fn project_and_elsewhere() -> (tempfile::TempDir, tempfile::TempDir) {
        let project = tempfile::tempdir().expect("a scratch directory");
        std::fs::create_dir(project.path().join(".git")).expect("the marker is creatable");
        let elsewhere = tempfile::tempdir().expect("a second scratch directory");

        (project, elsewhere)
    }

    /// A link planted where the model is about to write is the whole attack:
    /// the path is one the project allows, and the bytes land wherever the
    /// link points.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_write_through_a_link_that_leaves_the_project_is_refused() {
        let (project, elsewhere) = project_and_elsewhere();
        let secret = elsewhere.path().join("secret.txt");
        std::fs::write(&secret, "before").expect("the fixture writes");
        let planted = project.path().join("notes.txt");
        std::os::unix::fs::symlink(&secret, &planted).expect("the link is creatable");

        let context = ctx(project.path().to_owned());
        context.files.record(&planted);
        let refused = WriteTool
            .run(
                serde_json::json!({ "filePath": "notes.txt", "content": "after" }),
                &context,
            )
            .await
            .expect_err("a link out of the project is not a path this tool writes");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("symbolic link")),
            "got {refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&secret).expect("the file outside still exists"),
            "before",
            "the write followed the link and landed outside the project"
        );
    }

    /// The same escape one level up: the file is new and innocent, and it is
    /// the directory holding it that leads out.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_write_into_a_linked_directory_that_leaves_the_project_is_refused() {
        let (project, elsewhere) = project_and_elsewhere();
        std::os::unix::fs::symlink(elsewhere.path(), project.path().join("escape"))
            .expect("the link is creatable");

        let refused = WriteTool
            .run(
                serde_json::json!({ "filePath": "escape/planted.txt", "content": "x" }),
                &ctx(project.path().to_owned()),
            )
            .await
            .expect_err("a linked parent leads out of the project just as well");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("symbolic link")),
            "got {refused:?}"
        );
        assert!(
            !elsewhere.path().join("planted.txt").exists(),
            "the write was created outside the project"
        );
    }

    /// The case the guard must not break: a link is a perfectly ordinary way
    /// to arrange a checkout, and one that stays inside it changes nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_that_stays_inside_the_project_is_written_through_as_before() {
        let (project, _elsewhere) = project_and_elsewhere();
        let real = project.path().join("real");
        std::fs::create_dir(&real).expect("the fixture makes a directory");
        std::os::unix::fs::symlink(&real, project.path().join("link"))
            .expect("the link is creatable");

        WriteTool
            .run(
                serde_json::json!({ "filePath": "link/inside.txt", "content": "hello" }),
                &ctx(project.path().to_owned()),
            )
            .await
            .expect("a link that goes nowhere new is not an escape");

        assert_eq!(
            std::fs::read_to_string(real.join("inside.txt")).expect("the file was written"),
            "hello"
        );
    }

    /// `..` is not resolved by `std::path::absolute`, so a path can carry one
    /// all the way here — `grep` hands the model absolute paths that may hold
    /// one, and the model hands them straight back. A comparison made on the
    /// text would judge the wrong destination, in both directions: this is the
    /// direction where the text looks like an escape and the kernel disagrees.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dot_dot_path_that_lands_back_inside_the_project_is_written() {
        let (project, _elsewhere) = project_and_elsewhere();
        std::fs::create_dir(project.path().join("nested")).expect("the fixture makes a directory");

        WriteTool
            .run(
                serde_json::json!({ "filePath": "nested/../a.rs", "content": "fn main() {}" }),
                &ctx(project.path().to_owned()),
            )
            .await
            .expect("a `..` that comes back inside the project is not an escape");

        assert_eq!(
            std::fs::read_to_string(project.path().join("a.rs")).expect("the file was written"),
            "fn main() {}"
        );
    }

    /// And this is the other direction: a `..` *after* a link lands where the
    /// link led, not where it was written. The text collapses to a path inside
    /// the project — a prefix test on it would pass — while the kernel resolves
    /// it somewhere else entirely, which is exactly why the two sides of the
    /// comparison are canonical and never raw text.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dot_dot_path_that_climbs_out_through_a_link_is_refused() {
        let (project, elsewhere) = project_and_elsewhere();
        // Two levels, so `link/..` lands somewhere this test owns rather than
        // in the shared temporary root.
        let inner = elsewhere.path().join("inner");
        std::fs::create_dir(&inner).expect("the fixture makes a directory");
        std::os::unix::fs::symlink(&inner, project.path().join("link"))
            .expect("the link is creatable");
        let landing = elsewhere.path().join("secret.txt");

        let refused = WriteTool
            .run(
                serde_json::json!({ "filePath": "link/../secret.txt", "content": "x" }),
                &ctx(project.path().to_owned()),
            )
            .await
            .expect_err("`link/..` is the link's parent, not the project");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("symbolic link")),
            "got {refused:?}"
        );
        assert!(
            !landing.exists(),
            "the write escaped the project through `..` after a link"
        );
    }

    /// Both halves of the `..` story, pinned together because they are one
    /// decision and a refactor that collapsed them into a single rule would
    /// break exactly one of them. The input shape is model-reachable: `grep`
    /// hands back absolute paths that can carry a `..`, and the model hands
    /// them straight to this tool.
    ///
    /// - A `..` popped on the way to a link leaves the *claim* inside the
    ///   project, so the link at the end of it is still caught. Judge the
    ///   claim without popping and it reads as external, and the escape walks
    ///   out through the gap.
    /// - A `..` that walks the claim out of the project is not this tool's
    ///   call at all: it is openly external, indistinguishable from naming
    ///   the destination outright, and the permission gate is what asks.
    #[cfg(unix)]
    #[tokio::test]
    async fn dot_dot_is_popped_before_the_claim_is_judged() {
        // Nested on purpose, so `<cwd>/..` is a directory this test owns and
        // the openly-external half below cannot litter the shared temp root.
        let outer = tempfile::tempdir().expect("a scratch directory");
        let project = outer.path().join("project");
        let elsewhere = outer.path().join("elsewhere");
        for dir in [
            &project,
            &project.join(".git"),
            &project.join("nested"),
            &elsewhere,
        ] {
            std::fs::create_dir(dir).expect("the fixture makes its directories");
        }

        // Half one: `..` popped, then a link that leaves the project anyway.
        let secret = elsewhere.join("secret.txt");
        std::fs::write(&secret, "before").expect("the fixture writes");
        std::os::unix::fs::symlink(&secret, project.join("escape.txt"))
            .expect("the link is creatable");

        let context = ctx(project.clone());
        context
            .files
            .record(&project.join("nested").join("..").join("escape.txt"));
        let refused = WriteTool
            .run(
                serde_json::json!({ "filePath": "nested/../escape.txt", "content": "after" }),
                &context,
            )
            .await
            .expect_err("`nested/..` is the project, and the link still leaves it");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("symbolic link")),
            "got {refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&secret).expect("the file outside still exists"),
            "before",
            "a `..` earlier in the path hid the link from the guard"
        );

        // Half two: `..` that walks out. The guard stands aside; the gate asks.
        WriteTool
            .run(
                serde_json::json!({ "filePath": "../outside.txt", "content": "gated" }),
                &ctx(project),
            )
            .await
            .expect("an openly external `..` is the gate's decision, not a refusal here");

        assert_eq!(
            std::fs::read_to_string(outer.path().join("outside.txt"))
                .expect("the file was written"),
            "gated"
        );
    }

    /// A path that is openly outside the project — however it is spelled, `..`
    /// included — is the permission gate's decision and not this tool's: the
    /// user is asked about that directory (`permission.rs`, `outside`, which
    /// resolves the same way this guard does) and may well allow it. Refusing
    /// here would make that answer unusable, and the two spellings are the same
    /// destination once either side is resolved.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_path_that_openly_names_somewhere_else_is_left_to_the_permission_gate() {
        let (project, elsewhere) = project_and_elsewhere();
        let target = elsewhere.path().join("asked-for.txt");

        WriteTool
            .run(
                serde_json::json!({
                    "filePath": target.to_str().expect("a utf-8 path"),
                    "content": "allowed",
                }),
                &ctx(project.path().to_owned()),
            )
            .await
            .expect("an openly external write is gated, not refused outright");

        assert_eq!(
            std::fs::read_to_string(&target).expect("the file was written"),
            "allowed"
        );
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
