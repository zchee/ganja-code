use std::{path::Path, sync::Arc};

use tokio_util::sync::CancellationToken;

use super::{
    Args, BOM, DESCRIPTION, EditTool, IDENTICAL, MULTIPLE_MATCHES, NOT_FOUND, REPLACERS, SEPARATOR,
    block, chars_from, is_disproportionate_match, levenshtein, line_spans, normalize_whitespace,
    remove_indentation, replace, trim_diff, unescape,
};
use crate::{Tool, ToolCtx, ToolError, ToolOutput};

/// A context over `cwd` whose file log starts empty.
fn ctx(cwd: &Path) -> ToolCtx {
    ToolCtx::fixture(cwd.to_owned())
}

/// Writes `content` to `name` under `cwd` and marks it read, which is what
/// a `read` call ahead of the edit would have done.
fn seed(cwd: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = cwd.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the fixture makes its directories");
    }
    std::fs::write(&path, content).expect("the fixture writes");
    path
}

/// Runs an edit and gives back what the model would see.
async fn run(ctx: &ToolCtx, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
    EditTool.run(args, ctx).await
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

/// An edit follows a link exactly as a write does, and a link planted at a
/// path the project allows is how the file outside it gets rewritten.
#[cfg(unix)]
#[tokio::test]
async fn an_edit_through_a_link_that_leaves_the_project_is_refused() {
    let (project, elsewhere) = project_and_elsewhere();
    let secret = elsewhere.path().join("secret.txt");
    std::fs::write(&secret, "alpha\n").expect("the fixture writes");
    let planted = project.path().join("notes.txt");
    std::os::unix::fs::symlink(&secret, &planted).expect("the link is creatable");

    let context = ctx(project.path());
    context.files.record(&planted);
    let refused = failure(
        &context,
        serde_json::json!({
            "filePath": "notes.txt",
            "oldString": "alpha",
            "newString": "omega",
        }),
    )
    .await;

    assert!(
        refused.contains("symbolic link"),
        "an edit through a link out of the project must say so: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&secret).expect("the file outside still exists"),
        "alpha\n",
        "the edit followed the link and rewrote a file outside the project"
    );
}

/// The same escape one level up, where it is the directory that leads out.
#[cfg(unix)]
#[tokio::test]
async fn an_edit_inside_a_linked_directory_that_leaves_the_project_is_refused() {
    let (project, elsewhere) = project_and_elsewhere();
    let secret = elsewhere.path().join("secret.txt");
    std::fs::write(&secret, "alpha\n").expect("the fixture writes");
    std::os::unix::fs::symlink(elsewhere.path(), project.path().join("escape"))
        .expect("the link is creatable");

    let context = ctx(project.path());
    // Recorded under the name the call spells, so read-before-write is
    // satisfied and the refusal below can only be the escape guard's.
    context
        .files
        .record(&project.path().join("escape").join("secret.txt"));
    let refused = failure(
        &context,
        serde_json::json!({
            "filePath": "escape/secret.txt",
            "oldString": "alpha",
            "newString": "omega",
        }),
    )
    .await;

    assert!(
        refused.contains("symbolic link"),
        "a linked parent leads out of the project just as well: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&secret).expect("the file outside still exists"),
        "alpha\n"
    );
}

/// `..` is not resolved by `std::path::absolute`, so a path can carry one
/// all the way here — `grep` hands the model absolute paths that may hold
/// one. A `..` *after* a link lands where the link led: the text collapses
/// to a path inside the project, so a prefix test on it would pass, while
/// the kernel resolves it somewhere else entirely. That is why both sides
/// of the comparison are canonical and never raw text.
#[cfg(unix)]
#[tokio::test]
async fn a_dot_dot_path_that_climbs_out_through_a_link_is_refused() {
    let (project, elsewhere) = project_and_elsewhere();
    // Two levels, so `link/..` lands somewhere this test owns rather than
    // in the shared temporary root.
    let inner = elsewhere.path().join("inner");
    std::fs::create_dir(&inner).expect("the fixture makes a directory");
    let landing = elsewhere.path().join("secret.txt");
    std::fs::write(&landing, "alpha\n").expect("the fixture writes");
    std::os::unix::fs::symlink(&inner, project.path().join("link")).expect("the link is creatable");

    let context = ctx(project.path());
    context
        .files
        .record(&project.path().join("link").join("..").join("secret.txt"));
    let refused = failure(
        &context,
        serde_json::json!({
            "filePath": "link/../secret.txt",
            "oldString": "alpha",
            "newString": "omega",
        }),
    )
    .await;

    assert!(
        refused.contains("symbolic link"),
        "`link/..` is the link's parent, not the project: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&landing).expect("the file outside still exists"),
        "alpha\n",
        "the edit escaped the project through `..` after a link"
    );
}

/// The other direction, and the one `grep` actually produces: a `..` that
/// comes back inside the project is an ordinary path, not an escape.
#[cfg(unix)]
#[tokio::test]
async fn a_dot_dot_path_that_lands_back_inside_the_project_is_edited() {
    let (project, _elsewhere) = project_and_elsewhere();
    std::fs::create_dir(project.path().join("nested")).expect("the fixture makes a directory");
    let file = project.path().join("a.rs");
    std::fs::write(&file, "alpha\n").expect("the fixture writes");

    let context = ctx(project.path());
    context
        .files
        .record(&project.path().join("nested").join("..").join("a.rs"));
    run(
        &context,
        serde_json::json!({
            "filePath": "nested/../a.rs",
            "oldString": "alpha",
            "newString": "omega",
        }),
    )
    .await
    .expect("a `..` that comes back inside the project is not an escape");

    assert_eq!(
        std::fs::read_to_string(&file).expect("the file is readable"),
        "omega\n"
    );
}

/// The case the guard must not break: a link that stays inside the project
/// is an ordinary way to arrange a checkout.
#[cfg(unix)]
#[tokio::test]
async fn an_edit_through_a_link_that_stays_inside_the_project_still_applies() {
    let (project, _elsewhere) = project_and_elsewhere();
    let real = project.path().join("real");
    std::fs::create_dir(&real).expect("the fixture makes a directory");
    let file = real.join("notes.txt");
    std::fs::write(&file, "alpha\n").expect("the fixture writes");
    std::os::unix::fs::symlink(&real, project.path().join("link")).expect("the link is creatable");

    let context = ctx(project.path());
    // Recorded under the name the edit uses: read-before-write keys on the
    // path as the call spells it, which is a link away from `file`.
    context
        .files
        .record(&project.path().join("link").join("notes.txt"));
    run(
        &context,
        serde_json::json!({
            "filePath": "link/notes.txt",
            "oldString": "alpha",
            "newString": "omega",
        }),
    )
    .await
    .expect("a link that goes nowhere new is not an escape");

    assert_eq!(
        std::fs::read_to_string(&file).expect("the file is readable"),
        "omega\n"
    );
}

/// The window the guard could only narrow, now closed — the edit half of
/// the same story `write` tells.
///
/// The link stays *inside* the project, so the lexical guard passes it,
/// which is asserted rather than assumed: without that, the refusal below
/// would prove nothing about where it came from. The old code read this
/// file through the link and wrote back through it. `openat` with
/// `O_NOFOLLOW` refuses the name outright, at the read, before any
/// replacement is even attempted.
#[cfg(unix)]
#[tokio::test]
async fn a_link_planted_at_the_name_is_refused_by_the_open_not_by_the_guard() {
    let (project, _elsewhere) = project_and_elsewhere();
    let target = project.path().join("real.txt");
    std::fs::write(&target, "alpha\n").expect("the fixture writes");
    let planted = project.path().join("notes.txt");
    std::os::unix::fs::symlink(&target, &planted).expect("the link is creatable");

    crate::anchor::refuse_link_escape(project.path(), &planted).expect(
        "a link that stays inside the project is no escape — if this starts \
             failing, the refusal below stops proving anything about the open",
    );

    let context = ctx(project.path());
    context.files.record(&planted);
    let refused = failure(
        &context,
        serde_json::json!({
            "filePath": "notes.txt",
            "oldString": "alpha",
            "newString": "omega",
        }),
    )
    .await;

    assert!(
        refused.contains("symbolic link"),
        "a link at the final component is refused by the open: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("the target still exists"),
        "alpha\n",
        "the edit followed a link planted at the name"
    );
    assert!(
        std::fs::symlink_metadata(&planted)
            .expect("the link is still there")
            .file_type()
            .is_symlink(),
        "the link is refused, not replaced"
    );
}

/// The message of a failed edit.
async fn failure(ctx: &ToolCtx, args: serde_json::Value) -> String {
    match run(ctx, args).await {
        Ok(output) => panic!("the edit was expected to fail, and applied: {output:?}"),
        Err(error) => error.to_string(),
    }
}

/// `path` as it stands on disk.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("the file is readable")
}

// -----------------------------------------------------------------------
// Strategies, ported from upstream's replacer suite
// -----------------------------------------------------------------------

/// One replacement, and which strategy upstream resolves it with.
struct Case {
    content: &'static str,
    old: &'static str,
    new: &'static str,
    replace_all: bool,
    expected: &'static str,
    strategy: &'static str,
}

#[test]
fn an_edit_is_resolved_by_the_strategy_upstream_resolves_it_with() {
    let cases: std::collections::BTreeMap<&str, Case> = [
        (
            "simple: the model quoted the file exactly",
            Case {
                content: "old content here",
                old: "old content",
                new: "new content",
                replace_all: false,
                expected: "new content here",
                strategy: "simple",
            },
        ),
        (
            "simple: a multi-line block quoted exactly",
            Case {
                content: "line1\nline2\nline3",
                old: "line2",
                new: "new line 2\nextra line",
                replace_all: false,
                expected: "line1\nnew line 2\nextra line\nline3",
                strategy: "simple",
            },
        ),
        (
            "line-trimmed: the model lost the indentation",
            Case {
                content: "function a() {\n    const value = 1\n    return value\n}",
                old: "const value = 1\nreturn value",
                new: "const value = 2\nreturn value",
                replace_all: false,
                // The indented span is what matched, and it is replaced
                // by exactly what the model wrote, indentation included
                // or not. Upstream does not re-indent either.
                expected: "function a() {\nconst value = 2\nreturn value\n}",
                strategy: "line-trimmed",
            },
        ),
        (
            "line-trimmed: the model added trailing whitespace",
            Case {
                content: "alpha\nbeta\ngamma",
                old: "beta   ",
                new: "beta-updated",
                replace_all: false,
                expected: "alpha\nbeta-updated\ngamma",
                strategy: "line-trimmed",
            },
        ),
        (
            "block-anchor: the middle drifted but the frame held",
            Case {
                content: "function configure() {\n  const enabled = true\n  return enabled\n}",
                old: "function configure() {\n  const enable = true\n  return enable\n}",
                new: "function configure() {\n  return false\n}",
                replace_all: false,
                expected: "function configure() {\n  return false\n}",
                strategy: "block-anchor",
            },
        ),
        (
            "whitespace-normalized: the model reflowed the line",
            Case {
                content: "const   value   =   compute( a,   b )",
                old: "const value = compute( a, b )",
                new: "const value = compute(a, b)",
                replace_all: false,
                expected: "const value = compute(a, b)",
                strategy: "whitespace-normalized",
            },
        ),
        (
            "whitespace-normalized: the words sit inside a longer line",
            Case {
                content: "prefix const   value = 1 suffix",
                old: "const value = 1",
                new: "const value = 2",
                replace_all: false,
                expected: "prefix const value = 2 suffix",
                strategy: "whitespace-normalized",
            },
        ),
        (
            // The block was copied at another depth. Line-trimmed reaches
            // it first, and replaces the span it matched rather than the
            // one the model wrote, so the file's own indentation is not
            // preserved — upstream behaves the same way.
            "line-trimmed: the block was copied at another depth",
            Case {
                content: "class A {\n        method() {\n            return 1\n        }\n}",
                old: "  method() {\n      return 1\n  }",
                new: "  method() {\n      return 2\n  }",
                replace_all: false,
                expected: "class A {\n  method() {\n      return 2\n  }\n}",
                strategy: "line-trimmed",
            },
        ),
        (
            "escape-normalized: the model escaped what the file spells out",
            Case {
                content: "const message = \"hello\nworld\"",
                old: "const message = \\\"hello\\nworld\\\"",
                new: "const message = \"goodbye\"",
                replace_all: false,
                expected: "const message = \"goodbye\"",
                strategy: "escape-normalized",
            },
        ),
        (
            // The model wrapped the text in blank space. Whitespace
            // normalization takes it before the boundary trimmer does.
            "whitespace-normalized: the model wrapped the text in blank space",
            Case {
                content: "alpha\nbeta\ngamma",
                old: "\n  beta  \n",
                new: "beta-updated",
                replace_all: false,
                expected: "alpha\nbeta-updated\ngamma",
                strategy: "whitespace-normalized",
            },
        ),
        (
            "block-anchor: one middle line renamed inside the frame",
            Case {
                content: "function go() {\n  first()\n  second()\n  third()\n}",
                old: "function go() {\n  first()\n  second()\n  renamed()\n}",
                new: "function go() {\n  only()\n}",
                replace_all: false,
                expected: "function go() {\n  only()\n}",
                strategy: "block-anchor",
            },
        ),
        (
            // Two middle lines, one quoted exactly and one nothing like
            // the file: the anchor's average similarity falls under its
            // bar while the share of exactly matching lines clears the
            // looser one, which is the only gap this strategy fills.
            "context-aware: half the middle matches exactly, the rest not at all",
            Case {
                content: "fn go() {\n  first()\n  second()\n}",
                old: "fn go() {\n  first()\n  zzzzzzzzzzzzzzzz()\n}",
                new: "fn go() {\n  only()\n}",
                replace_all: false,
                expected: "fn go() {\n  only()\n}",
                strategy: "context-aware",
            },
        ),
        (
            "context-aware: the same, one indent deeper",
            Case {
                content: "class A:\n    keep()\n    drop()\n    end()",
                old: "class A:\n    keep()\n    qqqqqqqqqqqqqqqqqqq()\n    end()",
                new: "class A:\n    replaced()\n    end()",
                replace_all: false,
                expected: "class A:\n    replaced()\n    end()",
                strategy: "context-aware",
            },
        ),
        (
            "multi-occurrence: every exact occurrence, on request",
            Case {
                content: "foo bar foo baz foo",
                old: "foo",
                new: "qux",
                replace_all: true,
                expected: "qux bar qux baz qux",
                strategy: "simple",
            },
        ),
    ]
    .into_iter()
    .collect();

    for (name, case) in cases {
        let replaced = replace(case.content, case.old, case.new, case.replace_all)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(replaced.text, case.expected, "{name}");
        assert_eq!(replaced.strategy, case.strategy, "{name}: wrong strategy");
    }
}

/// One strategy, asked directly what it would offer.
struct Offer {
    replacer: &'static str,
    content: &'static str,
    find: &'static str,
    candidates: &'static [&'static str],
}

#[test]
fn every_strategy_offers_the_candidates_upstream_offers() {
    // Three of the nine rarely or never win in the driver, because the
    // ones above them accept the same spans first, so they are pinned
    // here at the only level where their behavior is observable. Each
    // expectation is what upstream's generator yields for the same input.
    let cases: std::collections::BTreeMap<&str, Offer> = [
        (
            "simple offers the string whether or not it is there",
            Offer {
                replacer: "simple",
                content: "anything",
                find: "zz",
                candidates: &["zz"],
            },
        ),
        (
            "line-trimmed offers every line whose trimmed form matches",
            Offer {
                replacer: "line-trimmed",
                content: "value = 1\nvalue = 1\n   value = 1   ",
                find: "value = 1",
                candidates: &["value = 1", "value = 1", "   value = 1   "],
            },
        ),
        (
            "block-anchor declines a frame whose middle shares nothing",
            Offer {
                replacer: "block-anchor",
                content: "a\nb\nc\na\nd\nc",
                find: "a\nX\nc",
                candidates: &[],
            },
        ),
        (
            "whitespace-normalized offers the file's own spacing",
            Offer {
                replacer: "whitespace-normalized",
                content: "prefix const   value = 1 suffix",
                find: "const value = 1",
                candidates: &["const   value = 1"],
            },
        ),
        (
            "indentation-flexible offers the block at the depth it sits at",
            Offer {
                replacer: "indentation-flexible",
                content: "class A {\n        method() {\n            return 1\n        }\n}",
                find: "  method() {\n      return 1\n  }",
                candidates: &["        method() {\n            return 1\n        }"],
            },
        ),
        (
            "indentation-flexible keeps relative indentation",
            Offer {
                replacer: "indentation-flexible",
                content: "  a\n    b\n",
                find: "a\n  b",
                candidates: &["  a\n    b"],
            },
        ),
        (
            "escape-normalized offers the resolved text and the line holding it",
            Offer {
                replacer: "escape-normalized",
                content: "a\tb",
                find: "a\\tb",
                candidates: &["a\tb", "a\tb"],
            },
        ),
        (
            "trimmed-boundary offers the text without its blank space",
            Offer {
                replacer: "trimmed-boundary",
                content: "alpha\nbeta\ngamma",
                find: "\n  beta  \n",
                candidates: &["beta"],
            },
        ),
        (
            "trimmed-boundary declines a string with nothing to trim",
            Offer {
                replacer: "trimmed-boundary",
                content: "x\ny\nz",
                find: "y",
                candidates: &[],
            },
        ),
        (
            "context-aware offers the framed block of the same length",
            Offer {
                replacer: "context-aware",
                content: "fn go() {\n  first()\n  second()\n}",
                find: "fn go() {\n  first()\n  zzzzzzzzzzzzzzzz()\n}",
                candidates: &["fn go() {\n  first()\n  second()\n}"],
            },
        ),
        (
            "multi-occurrence offers the string once per occurrence",
            Offer {
                replacer: "multi-occurrence",
                content: "foo bar foo",
                find: "foo",
                candidates: &["foo", "foo"],
            },
        ),
        (
            "multi-occurrence counts occurrences without overlapping them",
            Offer {
                replacer: "multi-occurrence",
                content: "aaaa",
                find: "aa",
                candidates: &["aa", "aa"],
            },
        ),
    ]
    .into_iter()
    .collect();

    for (name, case) in cases {
        let (_, replacer) = REPLACERS
            .iter()
            .find(|(named, _)| *named == case.replacer)
            .unwrap_or_else(|| panic!("{name}: no strategy called {}", case.replacer));
        let offered: Vec<String> = replacer(case.content, case.find)
            .into_iter()
            .map(std::borrow::Cow::into_owned)
            .collect();
        assert_eq!(offered, case.candidates, "{name}");
    }
}

#[test]
fn a_strategy_only_ever_sees_what_the_ones_above_it_declined() {
    // The exact text is present twice and a spaced-out copy once. Simple
    // offers the ambiguous candidate, which is skipped rather than
    // accepted, and the search goes on down the list until a strategy
    // offers one that resolves to a single place.
    let content = "value = 1\nvalue = 1\n   value = 1   ";

    let replaced =
        replace(content, "value = 1", "value = 2", false).expect("a later strategy resolves it");
    assert_eq!(replaced.strategy, "line-trimmed");
    assert_eq!(replaced.text, "value = 1\nvalue = 1\nvalue = 2");
}

#[test]
fn a_match_far_larger_than_the_model_asked_for_is_refused() {
    // The frame matches and one of the two middle lines matches exactly,
    // which is enough for the loosest strategy to offer the block — and
    // the block is six hundred characters the model never asked about.
    let long = "z".repeat(600);
    let content = format!("head\n  keep()\n  {long}\ntail");

    let refused = replace(&content, "head\n  keep()\n  x()\ntail", "head\ntail", false)
        .expect_err("the span is disproportionate");
    assert!(
        refused.to_string().contains("much larger than oldString"),
        "got {refused}"
    );
}

#[test]
fn a_match_is_disproportionate_by_lines_or_by_length() {
    // Three lines more, or twice as many, whichever bound is larger.
    assert!(is_disproportionate_match("a\nb\nc\nd", "a"));
    assert!(!is_disproportionate_match("a\nb\nc", "a\nb"));
    assert!(is_disproportionate_match("a\nb\nc\nd\ne", "a\nb"));
    // A single-line request is never measured by length.
    assert!(!is_disproportionate_match(&"x".repeat(5_000), "x"));
    // A multi-line one is.
    assert!(is_disproportionate_match(
        &format!("a\n{}", "x".repeat(5_000)),
        "a\nx"
    ));
    assert!(!is_disproportionate_match("a\nb", "a\nb"));
}

#[test]
fn a_string_in_two_places_is_refused_unless_every_place_was_asked_for() {
    let refused =
        replace("same same", "same", "other", false).expect_err("two matches are ambiguous");
    assert_eq!(refused.to_string(), MULTIPLE_MATCHES);

    let replaced = replace("same same", "same", "other", true).expect("replaceAll takes both");
    assert_eq!(replaced.text, "other other");
}

#[test]
fn a_string_that_is_nowhere_in_the_file_says_so() {
    let refused = replace("actual content", "not in file", "replacement", false)
        .expect_err("nothing matches");
    assert_eq!(refused.to_string(), NOT_FOUND);
}

#[test]
fn the_two_strings_being_the_same_is_refused_before_anything_is_searched() {
    let refused = replace("content", "same", "same", false).expect_err("nothing would change");
    assert_eq!(refused.to_string(), IDENTICAL);
}

#[test]
fn an_empty_old_string_never_reaches_a_strategy() {
    let refused =
        replace("content", "", "new", false).expect_err("an empty string matches everywhere");
    assert!(
        refused.to_string().contains("oldString cannot be empty"),
        "got {refused}"
    );
}

#[test]
fn a_loose_block_anchor_match_is_declined_rather_than_guessed_at() {
    // Upstream's case: the anchors line up but the body is unrelated and
    // much longer, so no strategy may claim it.
    let content = "function configure() {\n  keepImportantState()\n  removeAllUserData()\n  archiveBackups()\n  auditLog()\n}";
    let old = "function configure() {\n  const enabled = true\n}";

    let refused = replace(
        content,
        old,
        "function configure() {\n  const enabled = false\n}",
        false,
    )
    .expect_err("the block is not the one the model meant");
    assert_eq!(refused.to_string(), NOT_FOUND);
}

#[test]
fn a_block_anchor_match_with_unrelated_middle_content_is_declined() {
    let content = "function configure() {\n  removeAllUserData()\n}";
    let old = "function configure() {\n  const enabled = true\n}";

    let refused = replace(
        content,
        old,
        "function configure() {\n  const enabled = false\n}",
        false,
    )
    .expect_err("the middle line shares nothing with the one asked for");
    assert_eq!(refused.to_string(), NOT_FOUND);
}

#[test]
fn replace_all_takes_every_occurrence_a_strategy_offers() {
    let replaced =
        replace("  keep  \n  keep  ", "keep", "kept", true).expect("both lines are replaced");
    assert_eq!(replaced.text, "  kept  \n  kept  ");
}

// -----------------------------------------------------------------------
// Text helpers
// -----------------------------------------------------------------------

#[test]
fn a_block_of_lines_is_the_slice_joining_them_would_be() {
    let text = "alpha\nbeta\ngamma\n";
    let spans = line_spans(text);
    let lines: Vec<&str> = text.split('\n').collect();

    assert_eq!(spans.len(), lines.len());
    for first in 0..lines.len() {
        for count in 1..=lines.len() - first {
            assert_eq!(
                block(text, &spans, first, count),
                lines[first..first + count].join("\n")
            );
        }
    }
}

#[test]
fn line_spans_survive_multi_byte_characters() {
    let text = "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\nsecond\n";
    let spans = line_spans(text);

    assert_eq!(
        block(text, &spans, 0, 1),
        "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}"
    );
    assert_eq!(
        block(text, &spans, 0, 2),
        "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\nsecond"
    );
}

#[test]
fn whitespace_normalizes_to_single_spaces_with_the_ends_cut() {
    assert_eq!(normalize_whitespace("  a \t\n  b  "), "a b");
    assert_eq!(normalize_whitespace("   "), "");
    assert_eq!(normalize_whitespace(""), "");
}

#[test]
fn indentation_comes_off_every_line_by_the_shallowest_one() {
    assert_eq!(remove_indentation("    a\n      b\n"), "a\n  b\n");
    assert_eq!(remove_indentation("\n\n"), "\n\n");
    assert_eq!(remove_indentation("no indent"), "no indent");
}

#[test]
fn escapes_resolve_only_where_javascript_resolves_them() {
    assert_eq!(unescape("a\\nb"), "a\nb");
    assert_eq!(unescape("a\\tb"), "a\tb");
    assert_eq!(unescape("\\\\n"), "\\n");
    assert_eq!(unescape("\\q"), "\\q");
    assert_eq!(unescape("trailing\\"), "trailing\\");
}

#[test]
fn levenshtein_counts_single_character_edits() {
    assert_eq!(levenshtein("", ""), 0);
    assert_eq!(levenshtein("abc", ""), 3);
    assert_eq!(levenshtein("", "abc"), 3);
    assert_eq!(levenshtein("kitten", "sitting"), 3);
    assert_eq!(levenshtein("flaw", "lawn"), 2);
    // Characters, not bytes: one substitution, whatever it is encoded in.
    assert_eq!(levenshtein("\u{3042}\u{3044}", "\u{3042}\u{3046}"), 1);
}

#[test]
fn a_character_slice_never_lands_inside_a_character() {
    assert_eq!(
        chars_from("\u{3042}\u{3044}\u{3046}", 1),
        "\u{3044}\u{3046}"
    );
    assert_eq!(chars_from("ab", 5), "");
}

#[test]
fn a_patch_loses_the_indentation_all_of_its_lines_share() {
    let diff = "Index: a\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n     kept\n-    old\n+    new\n";

    assert_eq!(
        trim_diff(diff),
        "Index: a\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n kept\n-old\n+new\n"
    );
    // Nothing shared, nothing taken.
    assert_eq!(
        trim_diff("--- a\n+++ a\n-old\n+new\n"),
        "--- a\n+++ a\n-old\n+new\n"
    );
    assert_eq!(SEPARATOR.len(), 67);
}

// -----------------------------------------------------------------------
// The tool
// -----------------------------------------------------------------------

#[test]
fn the_description_is_upstreams_prompt_file() {
    assert_eq!(EditTool.description(), DESCRIPTION);
    assert!(
        EditTool
            .description()
            .starts_with("Performs exact string replacements in files.")
    );
    assert!(EditTool.description().contains("replaceAll"));
}

#[test]
fn the_schema_is_the_one_the_model_was_trained_against() {
    let schema = serde_json::to_value(EditTool.schema()).expect("a schema is JSON");
    let properties = schema["properties"]
        .as_object()
        .expect("an object of properties");

    let mut names: Vec<&String> = properties.keys().collect();
    names.sort();
    assert_eq!(names, ["filePath", "newString", "oldString", "replaceAll"]);
    assert_eq!(
        schema["required"],
        serde_json::json!(["filePath", "oldString", "newString"])
    );
    assert_eq!(
        properties["filePath"]["description"],
        serde_json::json!("The absolute path to the file to modify")
    );
    assert_eq!(
        properties["oldString"]["description"],
        serde_json::json!("The text to replace")
    );
    assert_eq!(
        properties["newString"]["description"],
        serde_json::json!("The text to replace it with (must be different from oldString)")
    );
    assert_eq!(
        properties["replaceAll"]["description"],
        serde_json::json!("Replace all occurrences of oldString (default false)")
    );
}

#[test]
fn the_arguments_parse_by_the_names_upstream_uses() {
    let args: Args = serde_json::from_value(serde_json::json!({
        "filePath": "/a", "oldString": "x", "newString": "y", "replaceAll": true
    }))
    .expect("all four fields parse");
    assert_eq!(args.file_path, "/a");
    assert_eq!(args.replace_all, Some(true));

    let args: Args = serde_json::from_value(
        serde_json::json!({"filePath": "/a", "oldString": "x", "newString": "y"}),
    )
    .expect("replaceAll is optional");
    assert_eq!(args.replace_all, None);

    serde_json::from_value::<Args>(serde_json::json!({"oldString": "x", "newString": "y"}))
        .expect_err("filePath is required");
}

#[test]
fn describe_names_the_file_the_call_would_change() {
    let described = EditTool.describe(&serde_json::json!({"filePath": "src/main.rs"}));
    assert_eq!(described, "edit src/main.rs");
    assert_eq!(EditTool.describe(&serde_json::json!({})), "edit");
}

#[tokio::test]
async fn an_empty_old_string_creates_the_file_it_names() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = dir.path().join("newfile.txt");

    let output = run(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "", "newString": "new content"}),
    )
    .await
    .expect("a new file is created");

    assert_eq!(read(&path), "new content");
    assert!(
        output.metadata["diff"]
            .as_str()
            .expect("a patch")
            .contains("new content"),
        "got {}",
        output.metadata["diff"]
    );
}

#[tokio::test]
async fn creating_a_file_makes_the_directories_it_sits_in() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = dir.path().join("nested").join("dir").join("file.txt");

    run(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "", "newString": "nested file"}),
    )
    .await
    .expect("the directories are made");

    assert_eq!(read(&path), "nested file");
}

#[tokio::test]
async fn an_empty_old_string_against_an_existing_file_is_refused_and_changes_nothing() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let original = format!("{BOM}using System;\n");
    let path = seed(dir.path(), "existing.cs", &original);
    ctx.files.record(&path);

    let refused = failure(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "", "newString": "using Up;\n"}),
    )
    .await;

    assert!(
        refused.contains("oldString cannot be empty"),
        "got {refused}"
    );
    assert_eq!(read(&path), original);
}

#[tokio::test]
async fn an_edit_replaces_the_text_it_names() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(dir.path(), "existing.txt", "old content here");
    ctx.files.record(&path);

    let output = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old content", "newString": "new content"}),
        )
        .await
        .expect("the edit applies");

    assert_eq!(output.output, "Edit applied successfully.");
    assert_eq!(output.title, "existing.txt");
    assert_eq!(read(&path), "new content here");
}

#[tokio::test]
async fn a_file_with_a_byte_order_mark_keeps_it_and_the_patch_does_not_show_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(
        dir.path(),
        "existing.cs",
        &format!("{BOM}using System;\nclass Test {{}}\n"),
    );
    ctx.files.record(&path);

    let output = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "using System;", "newString": "using Up;"}),
        )
        .await
        .expect("the mark does not hide the first line");

    let diff = output.metadata["diff"].as_str().expect("a patch");
    assert!(diff.contains("-using System;"), "got {diff}");
    assert!(diff.contains("+using Up;"), "got {diff}");
    assert!(!diff.contains(BOM), "the patch shows the mark");

    let content = read(&path);
    assert!(content.starts_with(BOM));
    assert_eq!(&content[BOM.len_utf8()..], "using Up;\nclass Test {}\n");
}

#[tokio::test]
async fn editing_a_file_that_is_not_there_says_so() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = dir.path().join("nonexistent.txt");

    let refused = failure(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "old", "newString": "new"}),
    )
    .await;

    assert!(refused.contains("not found"), "got {refused}");
}

#[tokio::test]
async fn editing_a_directory_says_so() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = dir.path().join("adir");
    std::fs::create_dir(&path).expect("the fixture makes a directory");

    let refused = failure(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "old", "newString": "new"}),
    )
    .await;

    assert!(refused.contains("directory"), "got {refused}");
}

#[tokio::test]
async fn the_two_strings_being_the_same_is_refused_before_the_file_is_opened() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(dir.path(), "file.txt", "content");

    for old in ["same", ""] {
        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": old, "newString": old}),
        )
        .await;
        assert!(refused.contains("identical"), "got {refused}");
    }
    assert_eq!(read(&path), "content");
}

#[tokio::test]
async fn an_edit_that_finds_nothing_leaves_the_file_byte_for_byte_as_it_was() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let original = "actual content\n";
    let path = seed(dir.path(), "file.txt", original);
    ctx.files.record(&path);
    let before = std::fs::read(&path).expect("the fixture is readable");

    let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "not in file", "newString": "replacement"}),
        )
        .await;

    assert_eq!(refused, NOT_FOUND);
    assert_eq!(
        std::fs::read(&path).expect("the file is still there"),
        before
    );
}

#[tokio::test]
async fn an_ambiguous_edit_leaves_the_file_byte_for_byte_as_it_was() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(dir.path(), "file.txt", "same same");
    ctx.files.record(&path);
    let before = std::fs::read(&path).expect("the fixture is readable");

    let refused = failure(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "same", "newString": "other"}),
    )
    .await;

    assert_eq!(refused, MULTIPLE_MATCHES);
    assert_eq!(
        std::fs::read(&path).expect("the file is still there"),
        before
    );
}

#[tokio::test]
async fn replace_all_changes_every_occurrence() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(dir.path(), "file.txt", "foo bar foo baz foo");
    ctx.files.record(&path);

    run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "foo", "newString": "qux", "replaceAll": true}),
        )
        .await
        .expect("every occurrence is replaced");

    assert_eq!(read(&path), "qux bar qux baz qux");
}

#[tokio::test]
async fn a_file_that_was_never_read_is_not_edited() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let original = "old content here";
    let path = seed(dir.path(), "file.txt", original);

    let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old content", "newString": "new content"}),
        )
        .await;

    assert!(refused.contains("read it first"), "got {refused}");
    assert_eq!(read(&path), original);
}

#[tokio::test]
async fn a_file_that_changed_since_it_was_read_is_not_edited() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let original = "old content here";
    let path = seed(dir.path(), "file.txt", original);
    ctx.files.record(&path);
    // Filesystem stamps can be coarse; force one that differs. Opened for
    // writing because a stamp is metadata a handle must be allowed to
    // write: unix grants that with the file's own permissions, Windows only
    // through a handle that asked for write access.
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_modified(std::time::SystemTime::UNIX_EPOCH))
        .expect("the fixture can move the stamp");

    let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old content", "newString": "new content"}),
        )
        .await;

    assert!(refused.contains("read it again"), "got {refused}");
    assert_eq!(read(&path), original);
}

#[tokio::test]
async fn a_successful_edit_records_the_file_so_the_next_one_may_follow_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(dir.path(), "file.txt", "one\ntwo\n");
    ctx.files.record(&path);

    run(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "one", "newString": "uno"}),
    )
    .await
    .expect("the first edit applies");
    run(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "two", "newString": "dos"}),
    )
    .await
    .expect("the second edit follows without another read");

    assert_eq!(read(&path), "uno\ndos\n");
}

#[tokio::test]
async fn a_cancelled_turn_leaves_the_file_alone() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut ctx = ctx(dir.path());
    let original = "old content here";
    let path = seed(dir.path(), "file.txt", original);
    ctx.files.record(&path);
    ctx.cancel = CancellationToken::new();
    ctx.cancel.cancel();

    let refused = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old content", "newString": "new content"}),
        )
        .await
        .expect_err("a cancelled turn does not write");

    assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
    assert_eq!(read(&path), original);
}

#[tokio::test]
async fn a_relative_path_resolves_against_the_session_directory() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(dir.path(), "file.txt", "before");
    ctx.files.record(&path);

    let output = run(
        &ctx,
        serde_json::json!({"filePath": "file.txt", "oldString": "before", "newString": "after"}),
    )
    .await
    .expect("the path resolves");

    assert_eq!(output.title, "file.txt");
    assert_eq!(read(&path), "after");
}

#[tokio::test]
async fn the_metadata_carries_the_patch_and_what_it_counts() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(dir.path(), "file.txt", "line1\nline2\nline3");
    ctx.files.record(&path);

    let output = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "line2", "newString": "new line a\nnew line b"}),
        )
        .await
        .expect("the edit applies");

    let filediff = &output.metadata["filediff"];
    assert_eq!(
        filediff["file"],
        serde_json::json!(path.display().to_string())
    );
    assert_eq!(filediff["patch"], output.metadata["diff"]);
    assert_eq!(filediff["additions"], serde_json::json!(2));
    assert_eq!(filediff["deletions"], serde_json::json!(1));
}

#[tokio::test]
async fn the_file_keeps_the_line_endings_it_had() {
    struct Case {
        content: &'static str,
        old: &'static str,
        new: &'static str,
        replace_all: bool,
        expected: &'static str,
    }

    // Upstream's line-ending table: what the file uses wins, whatever the
    // model quoted.
    let cases: std::collections::BTreeMap<&str, Case> = [
        (
            "lf file, lf strings",
            Case {
                content: "alpha\nbeta\ngamma\n",
                old: "alpha\nbeta\ngamma",
                new: "alpha\nbeta-updated\ngamma",
                replace_all: false,
                expected: "alpha\nbeta-updated\ngamma\n",
            },
        ),
        (
            "crlf file, crlf strings",
            Case {
                content: "alpha\r\nbeta\r\ngamma\r\n",
                old: "alpha\r\nbeta\r\ngamma",
                new: "alpha\r\nbeta-updated\r\ngamma",
                replace_all: false,
                expected: "alpha\r\nbeta-updated\r\ngamma\r\n",
            },
        ),
        (
            "lf file, crlf strings",
            Case {
                content: "alpha\nbeta\ngamma\n",
                old: "alpha\r\nbeta\r\ngamma",
                new: "alpha\r\nbeta-updated\r\ngamma",
                replace_all: false,
                expected: "alpha\nbeta-updated\ngamma\n",
            },
        ),
        (
            "crlf file, lf strings",
            Case {
                content: "alpha\r\nbeta\r\ngamma\r\n",
                old: "alpha\nbeta\ngamma",
                new: "alpha\nbeta-updated\ngamma",
                replace_all: false,
                expected: "alpha\r\nbeta-updated\r\ngamma\r\n",
            },
        ),
        (
            "lf file, crlf replacement only",
            Case {
                content: "alpha\nbeta\ngamma\n",
                old: "alpha\nbeta\ngamma",
                new: "alpha\r\nbeta-updated\r\ngamma",
                replace_all: false,
                expected: "alpha\nbeta-updated\ngamma\n",
            },
        ),
        (
            "crlf file, lf replacement only",
            Case {
                content: "alpha\r\nbeta\r\ngamma\r\n",
                old: "alpha\r\nbeta\r\ngamma",
                new: "alpha\nbeta-updated\ngamma",
                replace_all: false,
                expected: "alpha\r\nbeta-updated\r\ngamma\r\n",
            },
        ),
        (
            "lf file, mixed strings",
            Case {
                content: "alpha\nbeta\ngamma\n",
                old: "alpha\nbeta\r\ngamma",
                new: "alpha\r\nbeta\nomega",
                replace_all: false,
                expected: "alpha\nbeta\nomega\n",
            },
        ),
        (
            "crlf file, mixed strings",
            Case {
                content: "alpha\r\nbeta\r\ngamma\r\n",
                old: "alpha\r\nbeta\ngamma",
                new: "alpha\nbeta\r\nomega",
                replace_all: false,
                expected: "alpha\r\nbeta\r\nomega\r\n",
            },
        ),
        (
            "lf file, every block replaced",
            Case {
                content: "alpha\nbeta\nalpha\nbeta\n",
                old: "alpha\nbeta",
                new: "alpha\nbeta-updated",
                replace_all: true,
                expected: "alpha\nbeta-updated\nalpha\nbeta-updated\n",
            },
        ),
        (
            "crlf file, every block replaced",
            Case {
                content: "alpha\r\nbeta\r\nalpha\r\nbeta\r\n",
                old: "alpha\r\nbeta",
                new: "alpha\r\nbeta-updated",
                replace_all: true,
                expected: "alpha\r\nbeta-updated\r\nalpha\r\nbeta-updated\r\n",
            },
        ),
    ]
    .into_iter()
    .collect();

    for (name, case) in cases {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "test.txt", case.content);
        ctx.files.record(&path);

        run(
            &ctx,
            serde_json::json!({
                "filePath": path,
                "oldString": case.old,
                "newString": case.new,
                "replaceAll": case.replace_all,
            }),
        )
        .await
        .unwrap_or_else(|error| panic!("{name}: {error}"));

        assert_eq!(read(&path), case.expected, "{name}");
    }
}

#[tokio::test]
async fn a_crlf_file_edited_by_a_single_line_stays_crlf() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(dir.path(), "file.txt", "line1\r\nold\r\nline3");
    ctx.files.record(&path);

    run(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "old", "newString": "new"}),
    )
    .await
    .expect("the edit applies");

    assert_eq!(read(&path), "line1\r\nnew\r\nline3");
}

#[tokio::test]
async fn text_outside_ascii_is_replaced_without_being_cut_apart() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(
        dir.path(),
        "file.txt",
        "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\n\u{1f980} crab\n\u{4e16}\u{754c}\n",
    );
    ctx.files.record(&path);

    run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "\u{1f980} crab", "newString": "\u{1f980} \u{30ab}\u{30cb}"}),
        )
        .await
        .expect("the edit applies");

    assert_eq!(
        read(&path),
        "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\n\u{1f980} \u{30ab}\u{30cb}\n\u{4e16}\u{754c}\n"
    );
}

#[tokio::test]
async fn text_outside_ascii_is_matched_through_the_looser_strategies_too() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = seed(
        dir.path(),
        "file.txt",
        "    \u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\n    \u{4e16}\u{754c}\n",
    );
    ctx.files.record(&path);

    run(
        &ctx,
        serde_json::json!({
            "filePath": path,
            "oldString": "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\n\u{4e16}\u{754c}",
            "newString": "\u{3055}\u{3088}\u{3046}\u{306a}\u{3089}\n\u{4e16}\u{754c}",
        }),
    )
    .await
    .expect("the indentation is forgiven");

    // The indented span is what matched, so the replacement stands where
    // it stood without its indentation — upstream does the same.
    assert_eq!(
        read(&path),
        "\u{3055}\u{3088}\u{3046}\u{306a}\u{3089}\n\u{4e16}\u{754c}\n"
    );
}

#[tokio::test]
async fn a_file_that_is_not_text_is_refused_rather_than_rewritten() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = ctx(dir.path());
    let path = dir.path().join("binary.bin");
    std::fs::write(&path, [0xff_u8, 0xfe, 0x00, 0x01]).expect("the fixture writes bytes");
    ctx.files.record(&path);
    let before = std::fs::read(&path).expect("the fixture is readable");

    let refused = failure(
        &ctx,
        serde_json::json!({"filePath": path, "oldString": "old", "newString": "new"}),
    )
    .await;

    assert!(refused.contains("not valid UTF-8"), "got {refused}");
    assert_eq!(
        std::fs::read(&path).expect("the file is still there"),
        before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_edits_to_one_file_both_survive() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ctx = Arc::new(ctx(dir.path()));
    let path = seed(
        dir.path(),
        "file.txt",
        "top = 0\nmiddle = keep\nbottom = 0\n",
    );
    ctx.files.record(&path);

    let top = {
        let ctx = Arc::clone(&ctx);
        let path = path.clone();
        tokio::spawn(async move {
            run(
                    &ctx,
                    serde_json::json!({"filePath": path, "oldString": "top = 0", "newString": "top = 1"}),
                )
                .await
        })
    };
    let bottom = {
        let ctx = Arc::clone(&ctx);
        let path = path.clone();
        tokio::spawn(async move {
            run(
                    &ctx,
                    serde_json::json!({"filePath": path, "oldString": "bottom = 0", "newString": "bottom = 2"}),
                )
                .await
        })
    };

    top.await
        .expect("the task runs")
        .expect("the first edit applies");
    bottom
        .await
        .expect("the task runs")
        .expect("the second edit applies");

    assert_eq!(read(&path), "top = 1\nmiddle = keep\nbottom = 2\n");
}
