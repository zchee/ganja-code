use std::path::PathBuf;

use super::ReadTool;
use crate::{Tool, ToolCtx, ToolError};

/// A model writes `docs/guide.md` on every platform, and the path this
/// tool echoes back is built by joining that onto the session's directory.
/// On Windows a plain join keeps the argument's own `/` and prints
/// `C:\project\docs/guide.md` — a spelling that opens fine and that no
/// Windows program would ever produce, which is a gratuitous divergence
/// from upstream in a string the golden differential compares.
///
/// Trivially true on unix, where there is one separator; it bites on
/// Windows, where there are two.
#[test]
fn a_resolved_path_never_mixes_the_two_separators() {
    let cwd = std::path::Path::new(if cfg!(windows) { r"C:\project" } else { "/project" });

    for argument in ["docs/guide.md", "docs/deep/nested.md", "guide.md"] {
        let resolved = super::resolve(cwd, argument).display().to_string();

        assert!(
            !(resolved.contains('/') && resolved.contains('\\')),
            "{argument} resolved to a path spelled two ways at once: {resolved}"
        );
        assert!(
            resolved.ends_with("guide.md") || resolved.ends_with("nested.md"),
            "{argument} resolved to {resolved}"
        );
    }
}

/// A context rooted at `cwd`, with a fresh, empty read log and no
/// credential store to refuse.
fn ctx(cwd: PathBuf) -> ToolCtx {
    ToolCtx::fixture(cwd)
}

/// The same, told where the credentials are — which is what the engine
/// hands every call, and the only thing the guard tests below need.
fn guarding(cwd: PathBuf, store: &std::path::Path) -> ToolCtx {
    ToolCtx { credentials: crate::Credentials::Guarded(store.to_owned()), ..ctx(cwd) }
}

/// Where a fixture's credential store sits. Deliberately never written:
/// what the model learns must not depend on whether this machine is logged
/// in, so the guard answers for a store that is not there yet.
fn store_in(dir: &std::path::Path) -> PathBuf {
    dir.join("ganja").join("auth.json")
}

#[tokio::test]
async fn a_short_file_is_read_whole_and_numbered_from_one() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one\ntwo\nthree").expect("the fixture writes");

    let out = ReadTool
        .run(serde_json::json!({ "filePath": path.to_str().unwrap() }), &ctx(dir.path().to_owned()))
        .await
        .expect("a short file reads cleanly");

    assert!(out.output.contains("1: one\n2: two\n3: three"), "got {:?}", out.output);
    assert!(out.output.contains("(End of file - total 3 lines)"), "got {:?}", out.output);
    assert_eq!(out.title, "a.txt");
}

#[tokio::test]
async fn an_offset_and_limit_select_a_window_and_say_so() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "1\n2\n3\n4\n5\n").expect("the fixture writes");

    let out = ReadTool
        .run(
            serde_json::json!({ "filePath": path.to_str().unwrap(), "offset": 2, "limit": 2 }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect("a windowed read succeeds");

    assert!(out.output.contains("2: 2\n3: 3"), "got {:?}", out.output);
    assert!(!out.output.contains("1: 1") && !out.output.contains("4: 4"));
    assert!(
        out.output.contains("Showing lines 2-3 of 5. Use offset=4 to continue."),
        "got {:?}",
        out.output
    );
}

#[tokio::test]
async fn an_offset_past_end_of_file_is_refused_with_the_line_count() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "only one line").expect("the fixture writes");

    let refused = ReadTool
        .run(
            serde_json::json!({ "filePath": path.to_str().unwrap(), "offset": 50 }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect_err("an offset beyond the file is out of range");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("out of range") && message.contains("(1 lines)")),
        "got {refused:?}"
    );
}

#[tokio::test]
async fn an_empty_file_at_the_default_offset_reads_as_zero_lines_not_an_error() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("empty.txt");
    std::fs::write(&path, "").expect("the fixture writes");

    let out = ReadTool
        .run(serde_json::json!({ "filePath": path.to_str().unwrap() }), &ctx(dir.path().to_owned()))
        .await
        .expect("an empty file is not an out-of-range offset");

    assert!(out.output.contains("(End of file - total 0 lines)"), "got {:?}", out.output);
}

#[tokio::test]
async fn an_explicit_zero_offset_falls_back_to_one_like_an_absent_offset() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one\ntwo").expect("the fixture writes");

    let out = ReadTool
        .run(
            serde_json::json!({ "filePath": path.to_str().unwrap(), "offset": 0 }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect("offset 0 is treated as absent, per upstream's `offset || 1`");

    assert!(out.output.contains("1: one\n2: two"), "got {:?}", out.output);
}

#[tokio::test]
async fn a_missing_file_names_itself_and_suggests_lookalikes() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    std::fs::write(dir.path().join("readme.txt"), "x").expect("the fixture writes");
    let missing = dir.path().join("readme.tx");

    let refused = ReadTool
        .run(
            serde_json::json!({ "filePath": missing.to_str().unwrap() }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect_err("the file does not exist");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("File not found") && message.contains("readme.txt")),
        "got {refused:?}"
    );
}

#[tokio::test]
async fn a_missing_file_with_no_lookalikes_gets_the_plain_message() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let missing = dir.path().join("nothing-close-to-this.txt");

    let refused = ReadTool
        .run(
            serde_json::json!({ "filePath": missing.to_str().unwrap() }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect_err("the file does not exist and the directory is empty");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message == &format!("File not found: {}", missing.display())),
        "got {refused:?}"
    );
}

#[tokio::test]
async fn a_directory_lists_its_entries_sorted_with_trailing_slashes_on_subdirectories() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    std::fs::write(dir.path().join("b.txt"), "x").unwrap();
    std::fs::create_dir(dir.path().join("a-dir")).unwrap();
    std::fs::write(dir.path().join("c.txt"), "x").unwrap();

    let out = ReadTool
        .run(
            serde_json::json!({ "filePath": dir.path().to_str().unwrap() }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect("a directory is read as a listing");

    assert!(out.output.contains("<type>directory</type>"));
    assert!(
        out.output.contains("a-dir/\nb.txt\nc.txt"),
        "entries should be sorted with a trailing slash on the directory: {:?}",
        out.output
    );
    assert!(out.output.contains("(3 entries)"));
}

#[tokio::test]
async fn a_binary_extension_is_refused_without_being_opened_for_content() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("archive.zip");
    // Deliberately text content: the extension alone must be enough to
    // refuse it, matching upstream's extension fast-path.
    std::fs::write(&path, "not actually binary").expect("the fixture writes");

    let refused = ReadTool
        .run(serde_json::json!({ "filePath": path.to_str().unwrap() }), &ctx(dir.path().to_owned()))
        .await
        .expect_err("a binary extension is refused");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("Cannot read binary file")),
        "got {refused:?}"
    );
}

#[tokio::test]
async fn a_file_with_null_bytes_is_refused_as_binary() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("data.txt");
    std::fs::write(&path, [b'a', 0, b'b']).expect("the fixture writes");

    let refused = ReadTool
        .run(serde_json::json!({ "filePath": path.to_str().unwrap() }), &ctx(dir.path().to_owned()))
        .await
        .expect_err("a NUL byte marks the file binary");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("Cannot read binary file"))
    );
}

#[tokio::test]
async fn a_png_signature_is_reported_rather_than_refused_or_faked() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("pic.png");
    let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&[0u8; 32]);
    std::fs::write(&path, &bytes).expect("the fixture writes");

    let out = ReadTool
        .run(serde_json::json!({ "filePath": path.to_str().unwrap() }), &ctx(dir.path().to_owned()))
        .await
        .expect("an image is a successful call, not a failure");

    assert!(out.output.contains("Image read successfully"), "got {:?}", out.output);
    assert_eq!(out.metadata["mime"], "image/png");
}

#[tokio::test]
async fn a_line_longer_than_the_budget_is_cut_with_the_upstream_suffix() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("long.txt");
    std::fs::write(&path, "x".repeat(3_000)).expect("the fixture writes");

    let out = ReadTool
        .run(serde_json::json!({ "filePath": path.to_str().unwrap() }), &ctx(dir.path().to_owned()))
        .await
        .expect("a long line is truncated, not refused");

    assert!(out.output.contains("... (line truncated to 2000 chars)"), "got {:?}", out.output);
}

#[tokio::test]
async fn a_read_file_may_then_be_overwritten() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "hello").expect("the fixture writes");
    let context = ctx(dir.path().to_owned());

    ReadTool
        .run(serde_json::json!({ "filePath": path.to_str().unwrap() }), &context)
        .await
        .expect("the file reads");

    context
        .files
        .check_fresh(&path)
        .expect("a read records the file as fresh for a follow-up write");
}

#[test]
fn the_schema_requires_only_the_file_path() {
    let schema = serde_json::to_value(ReadTool.schema()).expect("a schema is JSON");

    assert_eq!(schema["required"], serde_json::json!(["filePath"]));
    for name in ["filePath", "offset", "limit"] {
        assert!(schema["properties"][name].is_object(), "missing {name}: {schema}");
    }
}

#[tokio::test]
async fn ganjas_credential_store_is_refused_by_absolute_path() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let store = store_in(dir.path());

    let refused = ReadTool
        .run(
            serde_json::json!({ "filePath": store.to_str().unwrap() }),
            &guarding(dir.path().to_owned(), &store),
        )
        .await
        .expect_err("the credential store is never readable");

    let ToolError::Failed(message) = &refused else {
        panic!("well-formed arguments are not an argument error: {refused:?}");
    };
    assert!(message.contains("is ganja's credential store"), "got {message}");
    assert!(message.contains("provider API keys"), "got {message}");
    assert!(
        message.contains("retrying will not help"),
        "a model that thinks the refusal is retryable will retry: {message}"
    );
}

#[tokio::test]
async fn ganjas_credential_store_is_refused_through_a_relative_route_onto_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let store = store_in(dir.path());
    let directory = store.parent().expect("the store lives in a directory");
    let name = store.file_name().expect("the store is a file");

    let refused = ReadTool
        .run(
            serde_json::json!({ "filePath": name.to_str().unwrap() }),
            &guarding(directory.to_owned(), &store),
        )
        .await
        .expect_err("a relative route onto the store is still the store");

    let ToolError::Failed(message) = &refused else {
        panic!("well-formed arguments are not an argument error: {refused:?}");
    };
    assert!(message.contains("is ganja's credential store"), "got {message}");
}

#[tokio::test]
async fn a_project_file_that_only_shares_the_stores_name_still_reads() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let namesake = dir.path().join("auth.json");
    std::fs::write(&namesake, "not a credential store").expect("the fixture writes");

    let out = ReadTool
        .run(
            serde_json::json!({ "filePath": namesake.to_str().unwrap() }),
            &guarding(dir.path().to_owned(), &store_in(dir.path())),
        )
        .await
        .expect("the guard is identity-based: any other auth.json still reads");

    assert!(out.output.contains("1: not a credential store"), "got {:?}", out.output);
}
