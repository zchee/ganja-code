use std::path::PathBuf;

use super::GlobTool;
use crate::{Tool, ToolCtx, ToolError};

/// A context rooted at `cwd`, with a cancel nobody has pulled.
fn ctx(cwd: PathBuf) -> ToolCtx {
    ToolCtx::fixture(cwd)
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
    // Joined a component at a time rather than as `nested/x.rs`: the tool
    // answers with a path the platform built, so an expectation carrying a
    // separator this platform does not write would be comparing two
    // spellings of one file.
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("x.rs"), "").unwrap();

    let out = GlobTool
        .run(
            serde_json::json!({ "pattern": "*.rs", "path": "nested" }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect("a relative path resolves against ctx.cwd, not the process cwd");

    assert_eq!(out.output, nested.join("x.rs").display().to_string());
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
