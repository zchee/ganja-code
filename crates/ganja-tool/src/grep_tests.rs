use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use super::{GrepTool, search};
use crate::read::ReadTool;
use crate::{Tool, ToolCtx, ToolError};

/// A context rooted at `cwd`, with a cancel nobody has pulled and the
/// credential store the engine would have named sitting under it.
fn ctx(cwd: PathBuf) -> ToolCtx {
    let credentials = cwd.join("ganja").join("auth.json");
    let mut ctx = ToolCtx::fixture(cwd);
    ctx.credentials = crate::Credentials::Guarded(credentials);
    ctx
}

#[tokio::test]
async fn matches_are_grouped_by_file_with_line_numbers() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    std::fs::write(dir.path().join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "struct Three;\n").unwrap();

    let out = GrepTool
        .run(serde_json::json!({ "pattern": "fn \\w+" }), &ctx(dir.path().to_owned()))
        .await
        .expect("a grep over a real directory succeeds");

    assert!(out.output.starts_with("Found 2 matches\n"), "got {:?}", out.output);
    assert!(out.output.contains("a.rs:\n  Line 1: fn one() {}\n  Line 2: fn two() {}"));
    assert!(!out.output.contains("b.rs"));
    assert_eq!(out.metadata["matches"], 2);
}

#[tokio::test]
async fn every_match_names_its_file_by_absolute_path() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    std::fs::write(dir.path().join("a.rs"), "needle\n").expect("the fixture writes");

    let out = GrepTool
        .run(serde_json::json!({ "pattern": "needle" }), &ctx(dir.path().to_owned()))
        .await
        .expect("a grep over a real directory succeeds");

    let header = format!("{}:", dir.path().join("a.rs").display());
    assert!(out.output.contains(&header), "expected the header {header:?} in {:?}", out.output);
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
    std::fs::write(dir.path().join("nested").join("found.rs"), "fn needle() {}\n")
        .expect("the fixture writes");
    let context = ctx(dir.path().to_owned());

    let found = GrepTool
        .run(serde_json::json!({ "pattern": "needle", "path": "nested" }), &context)
        .await
        .expect("a grep over a subdirectory succeeds");

    // Lifted out of the output text the way the model lifts it: nothing
    // here rebuilds the path from what the fixture knows.
    let quoted = found
        .output
        .lines()
        .find_map(|line| line.strip_suffix(':'))
        .expect("grep heads each file's matches with that file's path");

    let read =
        ReadTool.run(serde_json::json!({ "filePath": quoted }), &context).await.unwrap_or_else(
            |error| panic!("read must accept the path grep printed ({quoted:?}): {error:?}"),
        );

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
        .run(serde_json::json!({ "pattern": "needle" }), &ctx(dir.path().to_owned()))
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
        .run(serde_json::json!({ "pattern": "needle" }), &ctx(dir.path().to_owned()))
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
        .run(serde_json::json!({ "pattern": "" }), &ctx(dir.path().to_owned()))
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
        .run(serde_json::json!({ "pattern": "(unclosed" }), &ctx(dir.path().to_owned()))
        .await
        .expect_err("an unbalanced group is not a valid regex");

    assert!(matches!(refused, ToolError::InvalidArgs(_)), "got {refused:?}");
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
        .run(serde_json::json!({ "pattern": "needle" }), &ctx(dir.path().to_owned()))
        .await
        .expect("a grep over many matches still succeeds, capped");

    assert_eq!(out.metadata["matches"], 100);
    assert_eq!(out.metadata["truncated"], true);
    assert!(
        out.output.contains("(Results truncated. Consider using a more specific path or pattern.)")
    );
    assert!(out.output.starts_with("Found 100 matches (more matches available)"));
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
        assert!(schema["properties"][name].is_object(), "missing {name}: {schema}");
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

    let found = search(dir.path(), "sk-canary-8842", None, Some(&store), &CancellationToken::new())
        .expect("the search runs");

    let hits: Vec<(&str, u64)> = found.iter().map(|item| (item.path.as_str(), item.line)).collect();

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
        .run(serde_json::json!({ "pattern": "needle" }), &ctx(dir.path().to_owned()))
        .await
        .expect("the guard is identity-based: any other auth.json is still searched");

    assert!(out.output.contains("auth.json"), "got {:?}", out.output);
}
