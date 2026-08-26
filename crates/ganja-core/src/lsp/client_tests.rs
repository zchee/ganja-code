use std::{path::PathBuf, time::Duration};

use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use serde_json::json;
use tokio::time::Instant;

use super::{
    Client, DIAGNOSTICS_DEBOUNCE, DIAGNOSTICS_REQUEST_TIMEOUT, DOCUMENT_WAIT_TIMEOUT, Pending,
    Store, configuration, end_position, file_path, split_lines, sync_kind, uri,
};

/// One error diagnostic, so a publish carries something.
fn error(message: &str) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        message: message.to_owned(),
        ..Diagnostic::default()
    }
}

/// The fixture project, spelled the way this platform spells an absolute
/// path.
///
/// A `file://` URI can only be built from one — [`url`] refuses a relative
/// path outright — and `/p/src` is not absolute on Windows, where a path
/// begins at a drive. A fixture that ignored that would key its
/// diagnostics under [`None`] and assert nothing at all.
#[cfg(unix)]
const ROOT: &str = "/p/src";
#[cfg(windows)]
const ROOT: &str = r"C:\p\src";

fn path() -> PathBuf {
    PathBuf::from(ROOT).join("main.rs")
}

#[tokio::test(start_paused = true)]
async fn a_publish_after_the_touch_satisfies_the_wait() {
    // The store is driven from the test, so the whole wait contract is
    // proven without a language server anywhere near it.
    let store = std::sync::Arc::new(Store::default());
    let after = Instant::now();
    let handle = std::sync::Arc::clone(&store);
    let waiting = tokio::spawn(async move {
        handle
            .wait_fresh(&path(), 1, after, DOCUMENT_WAIT_TIMEOUT)
            .await
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    store.publish(path(), None, vec![error("mismatched types")]);

    assert!(waiting.await.expect("the wait finishes"));
    assert_eq!(store.for_path(&path()).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_publish_from_before_the_touch_is_not_fresh_enough() {
    let store = std::sync::Arc::new(Store::default());
    store.publish(path(), None, vec![error("stale")]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let after = Instant::now();

    let fresh = store
        .wait_fresh(&path(), 1, after, Duration::from_millis(300))
        .await;

    assert!(
        !fresh,
        "the only publish predates the edit that is being waited on"
    );
    assert_eq!(
        store.for_path(&path()).len(),
        1,
        "and the caches still serve what they hold"
    );
}

#[tokio::test(start_paused = true)]
async fn a_publish_naming_the_touched_version_is_fresh_however_old_it_is() {
    let store = std::sync::Arc::new(Store::default());
    store.publish(path(), Some(7), vec![error("mismatched types")]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let after = Instant::now();

    assert!(
        store
            .wait_fresh(&path(), 7, after, DOCUMENT_WAIT_TIMEOUT)
            .await,
        "the version matches the touch, which outranks the clock"
    );
    assert!(
        !store
            .wait_fresh(&path(), 8, after, Duration::from_millis(300))
            .await,
        "a different version is never fresh"
    );
}

#[tokio::test(start_paused = true)]
async fn a_server_still_revising_restarts_the_debounce() {
    let store = std::sync::Arc::new(Store::default());
    let after = Instant::now();
    let handle = std::sync::Arc::clone(&store);
    let waiting = tokio::spawn(async move {
        let settled = handle
            .wait_fresh(&path(), 1, after, DOCUMENT_WAIT_TIMEOUT)
            .await;

        (settled, Instant::now())
    });

    // Three publishes, each landing before the previous one's quiet period
    // is up. The wait must not resolve until 150ms after the last.
    let started = Instant::now();
    for revision in 0..3 {
        tokio::time::sleep(DIAGNOSTICS_DEBOUNCE / 2).await;
        store.publish(path(), None, vec![error(&format!("revision {revision}"))]);
    }
    let last = Instant::now();

    let (settled, finished) = waiting.await.expect("the wait finishes");

    assert!(settled);
    assert!(
        finished.duration_since(last) >= DIAGNOSTICS_DEBOUNCE,
        "the quiet period is measured from the last publish, not the first"
    );
    assert!(
        finished.duration_since(started) >= DIAGNOSTICS_DEBOUNCE * 2,
        "so the restarts really did extend the wait"
    );
}

#[tokio::test(start_paused = true)]
async fn a_wait_nobody_answers_times_out_silently() {
    let store = Store::default();

    let settled = store
        .wait_fresh(&path(), 1, Instant::now(), DOCUMENT_WAIT_TIMEOUT)
        .await;

    assert!(!settled);
    assert!(
        store.diagnostics().is_empty(),
        "a timeout leaves no marker of any kind behind"
    );
}

#[tokio::test(start_paused = true)]
async fn a_wait_with_no_budget_does_not_wait() {
    let store = std::sync::Arc::new(Store::default());
    store.publish(path(), None, vec![error("boom")]);

    assert!(
        !store
            .wait_fresh(&path(), 0, Instant::now(), Duration::ZERO)
            .await
    );
}

/// **Regression, pending-map leak.** A request nobody ever answers used
/// to leave its `id -> oneshot::Sender` entry in [`Pending`] forever once
/// the caller stopped waiting on it — `pull`'s own `tokio::time::timeout`
/// drops [`Client::request`]'s future the moment a request runs past
/// [`DIAGNOSTICS_REQUEST_TIMEOUT`], and a plain `async fn` has no way to
/// clean up when it is dropped mid-`await` instead of returned from. One
/// entry leaked per timed-out pull, for the rest of the client's life.
///
/// Built directly rather than through [`Client::start`]: nothing here
/// needs a real process, only the channel a request goes out on and the
/// map its id is tracked in, and every `Client` field is visible to a
/// test in this same module. The outgoing queue's receiver is bound to
/// `_queue` and held for the whole test — dropping it would fail the
/// request immediately instead of leaving it pending, which is not the
/// case this is proving.
#[tokio::test(start_paused = true)]
async fn a_timed_out_request_leaves_no_trace_in_the_pending_map() {
    let (outgoing, _queue) = tokio::sync::mpsc::unbounded_channel();
    let pending = Pending::default();
    let client = Client {
        id: "fake".to_owned(),
        root: PathBuf::from("/"),
        store: std::sync::Arc::new(Store::default()),
        outgoing,
        pending: std::sync::Arc::clone(&pending),
        next_id: std::sync::atomic::AtomicI64::new(1),
        documents: tokio::sync::Mutex::default(),
        incremental: false,
        child: std::sync::Mutex::new(None),
    };

    let answered = tokio::time::timeout(
        DIAGNOSTICS_REQUEST_TIMEOUT,
        client.request("textDocument/diagnostic", json!({})),
    )
    .await;

    assert!(answered.is_err(), "nothing in this test ever answers");
    assert!(
        pending
            .lock()
            .expect("the pending requests are never poisoned")
            .is_empty(),
        "a request the caller stopped waiting on must not leave its sender behind"
    );
}

#[test]
fn a_full_report_answers_the_pull_and_replaces_what_was_cached() {
    let store = Store::default();
    let report = json!({
        "kind": "full",
        "resultId": "rust-analyzer",
        "items": [{
            "range": { "start": { "line": 1, "character": 4 }, "end": { "line": 1, "character": 20 } },
            "severity": 1,
            "code": "E0308",
            "source": "rust-analyzer",
            "message": "expected i32, found &'static str",
        }],
    });

    assert!(
        store.absorb(&path(), &report),
        "the server had something to say"
    );
    assert_eq!(
        store
            .for_path(&path())
            .first()
            .map(|issue| issue.message.clone()),
        Some("expected i32, found &'static str".to_owned())
    );
}

#[test]
fn a_report_with_nothing_in_it_does_not_answer_the_pull() {
    let store = Store::default();

    assert!(
        !store.absorb(&path(), &json!({ "kind": "full", "items": [] })),
        "a clean file is not an answer to wait on; the push may still speak"
    );
    assert!(store.for_path(&path()).is_empty());
}

#[test]
fn an_unchanged_report_leaves_the_cache_standing() {
    let store = Store::default();
    store.absorb(
            &path(),
            &json!({ "kind": "full", "items": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "severity": 1,
                "message": "mismatched types",
            }]}),
        );

    let answered = store.absorb(&path(), &json!({ "kind": "unchanged", "resultId": "x" }));

    assert!(answered, "\"still true\" is an answer");
    assert_eq!(
        store.for_path(&path()).len(),
        1,
        "and it did not wipe what is true"
    );
}

#[test]
fn a_related_document_carries_another_files_errors() {
    let store = Store::default();
    let other = PathBuf::from(ROOT).join("other.rs");
    let report = json!({
        "kind": "full",
        "items": [],
        "relatedDocuments": {
            super::uri(&other): { "kind": "full", "items": [{
                "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 5 } },
                "severity": 1,
                "message": "this call now has the wrong number of arguments",
            }]},
        },
    });

    store.absorb(&path(), &report);

    assert!(
        store.for_path(&path()).is_empty(),
        "the edited file itself is fine"
    );
    assert_eq!(
        store.for_path(&other).len(),
        1,
        "but the file it broke is reported, which is what a write's cross-file section is made of"
    );
}

#[test]
fn the_same_error_arriving_on_both_channels_is_reported_once() {
    let store = Store::default();
    let issue = error("mismatched types");
    store.publish(path(), None, vec![issue.clone()]);
    store.absorb(
        &path(),
        &json!({ "kind": "full", "items": [serde_json::to_value(&issue).expect("it serializes")] }),
    );

    assert_eq!(
        store.for_path(&path()).len(),
        1,
        "a model shown it twice is a model told there are two problems"
    );
}

#[test]
fn two_different_errors_on_one_line_both_survive_the_dedupe() {
    let store = Store::default();
    store.publish(path(), None, vec![error("mismatched types")]);
    store.absorb(
        &path(),
        &json!({ "kind": "full", "items": [
            serde_json::to_value(error("no method named `frobnicate`")).expect("it serializes"),
        ]}),
    );

    assert_eq!(
        store.for_path(&path()).len(),
        2,
        "the message is part of the identity"
    );
}

#[test]
fn a_configuration_section_is_a_dotted_path() {
    let settings = json!({ "rust-analyzer": { "check": { "command": "clippy" } } });

    let cases = [
        (Some("rust-analyzer.check.command"), json!("clippy")),
        (Some("rust-analyzer.check"), json!({ "command": "clippy" })),
        (Some("rust-analyzer.missing"), json!(null)),
        (Some("nothing.at.all"), json!(null)),
        (None, settings.clone()),
    ];

    for (section, expected) in cases {
        assert_eq!(
            configuration(Some(&settings), section),
            expected,
            "{section:?}"
        );
    }
    assert_eq!(configuration(None, Some("anything")), json!(null));
}

#[test]
fn the_negotiated_sync_kind_is_read_in_either_spelling() {
    assert_eq!(
        sync_kind(&json!({ "capabilities": { "textDocumentSync": 2 } })),
        Some(lsp_types::TextDocumentSyncKind::INCREMENTAL)
    );
    assert_eq!(
        sync_kind(&json!({ "capabilities": { "textDocumentSync": { "change": 1 } } })),
        Some(lsp_types::TextDocumentSyncKind::FULL)
    );
    assert_eq!(sync_kind(&json!({ "capabilities": {} })), None);
}

#[test]
fn a_documents_end_is_where_the_next_character_would_go() {
    let cases = [
        ("", json!({ "line": 0, "character": 0 })),
        ("fn main() {}", json!({ "line": 0, "character": 12 })),
        ("a\nbb\n", json!({ "line": 2, "character": 0 })),
        ("a\r\nbb", json!({ "line": 1, "character": 2 })),
        ("a\rbb", json!({ "line": 1, "character": 2 })),
        // UTF-16 code units, so an astral character counts twice.
        ("\u{1F600}", json!({ "line": 0, "character": 2 })),
    ];

    for (text, expected) in cases {
        assert_eq!(end_position(text), expected, "{text:?}");
    }
}

#[test]
fn lines_split_on_every_ending_and_keep_the_empty_tail() {
    assert_eq!(split_lines("a\nb"), ["a", "b"]);
    assert_eq!(split_lines("a\n"), ["a", ""]);
    assert_eq!(split_lines(""), [""]);
    assert_eq!(split_lines("a\r\n\rb"), ["a", "", "b"]);
}

#[test]
fn a_path_survives_the_round_trip_through_a_uri() {
    let original = PathBuf::from(ROOT).join("a project").join("main.rs");

    let round_tripped = file_path(&uri(&original));

    assert_eq!(round_tripped, Some(original), "spaces and all");
}

/// A server spells a drive letter however it likes — rust-analyzer sends
/// back a percent-encoded lower-case one — and this port builds its own
/// paths from the filesystem, which gives an upper-case drive. Two
/// [`PathBuf`]s differing only there are two map keys, so a file's errors
/// would be filed where nothing looks for them.
#[cfg(windows)]
#[test]
fn a_drive_letter_keys_one_file_however_the_server_spelled_it() {
    let expected = Some(PathBuf::from(r"C:\p\src\main.rs"));

    for spelling in [
        "file:///C:/p/src/main.rs",
        "file:///c:/p/src/main.rs",
        "file:///c%3A/p/src/main.rs",
        "file:///C%3A/p/src/main.rs",
    ] {
        assert_eq!(file_path(spelling), expected, "{spelling}");
    }
}

#[test]
fn a_uri_that_is_not_a_file_names_no_path() {
    assert_eq!(file_path("untitled:Untitled-1"), None);
    assert_eq!(file_path("https://example.com/x.rs"), None);
    assert_eq!(file_path("not a uri"), None);
}
