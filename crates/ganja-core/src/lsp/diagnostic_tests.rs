use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use super::{MAX_PER_FILE, pretty, report};

/// A diagnostic at `line`/`column` (both zero-based, as a server sends
/// them) saying `message`.
fn at(line: u32, column: u32, severity: Option<DiagnosticSeverity>, message: &str) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position { line, character: column },
            end: Position { line, character: column + 1 },
        },
        severity,
        message: message.to_owned(),
        ..Diagnostic::default()
    }
}

#[test]
fn a_diagnostic_renders_one_based() {
    let rendered = pretty(&at(0, 0, Some(DiagnosticSeverity::ERROR), "mismatched types"));

    assert_eq!(rendered, "ERROR [1:1] mismatched types");
}

#[test]
fn a_severity_the_server_omitted_is_an_error() {
    let cases = [
        (Some(DiagnosticSeverity::ERROR), "ERROR"),
        (Some(DiagnosticSeverity::WARNING), "WARN"),
        (Some(DiagnosticSeverity::INFORMATION), "INFO"),
        (Some(DiagnosticSeverity::HINT), "HINT"),
        (None, "ERROR"),
    ];

    for (severity, expected) in cases {
        let rendered = pretty(&at(4, 2, severity, "something"));
        assert_eq!(rendered, format!("{expected} [5:3] something"), "{severity:?}");
    }
}

#[test]
fn only_errors_reach_the_model() {
    let issues = [
        at(0, 0, Some(DiagnosticSeverity::WARNING), "unused import"),
        at(1, 0, Some(DiagnosticSeverity::ERROR), "mismatched types"),
        at(2, 0, Some(DiagnosticSeverity::HINT), "consider renaming"),
    ];

    let block = report("src/main.rs", &issues).expect("the error is reported");

    assert_eq!(
        block,
        "<diagnostics file=\"src/main.rs\">\nERROR [2:1] mismatched types\n</diagnostics>"
    );
}

#[test]
fn a_file_with_no_errors_reports_nothing() {
    let issues = [at(0, 0, Some(DiagnosticSeverity::WARNING), "unused import")];

    assert_eq!(report("src/main.rs", &issues), None);
    assert_eq!(report("src/main.rs", &[]), None);
}

#[test]
fn errors_past_the_cap_are_counted_rather_than_printed() {
    let issues: Vec<_> = (0..MAX_PER_FILE as u32 + 3)
        .map(|line| at(line, 0, Some(DiagnosticSeverity::ERROR), "boom"))
        .collect();

    let block = report("src/main.rs", &issues).expect("errors are reported");

    assert_eq!(block.lines().filter(|line| line.starts_with("ERROR")).count(), MAX_PER_FILE);
    assert!(block.contains("\n... and 3 more"), "the tail is counted: {block}");
    // The suffix sits inside the block, not after it.
    assert!(block.ends_with("... and 3 more\n</diagnostics>"), "{block}");
}

#[test]
fn a_file_exactly_at_the_cap_says_nothing_about_more() {
    let issues: Vec<_> = (0..MAX_PER_FILE as u32)
        .map(|line| at(line, 0, Some(DiagnosticSeverity::ERROR), "boom"))
        .collect();

    let block = report("src/main.rs", &issues).expect("errors are reported");

    assert!(!block.contains("more"), "{block}");
}
