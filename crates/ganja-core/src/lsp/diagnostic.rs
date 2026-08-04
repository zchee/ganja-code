//! What a diagnostic looks like by the time the model reads it.
//!
//! Spec: upstream `packages/opencode/src/lsp/diagnostic.ts`, both functions.
//!
//! Only errors survive [`report`]. That is upstream's filter and it is worth
//! saying why it is right: the block is appended to a tool result the model
//! reads as the consequence of an edit it just made, and a warning it did not
//! cause is an invitation to go and fix something nobody asked about.

use lsp_types::{Diagnostic, DiagnosticSeverity};

/// How many errors one file may contribute before the rest are counted rather
/// than printed (`diagnostic.ts:3`).
const MAX_PER_FILE: usize = 20;

/// One diagnostic as one line: `SEVERITY [line:col] message`.
///
/// Positions are LSP's zero-based ones rendered one-based, which is how every
/// editor and compiler in the world numbers them.
#[must_use]
pub fn pretty(diagnostic: &Diagnostic) -> String {
    // Upstream's `severityMap[diagnostic.severity || 1]`: an absent severity
    // is an error, because a server that did not say is not a server saying
    // "this is fine".
    let severity = match diagnostic.severity {
        Some(DiagnosticSeverity::WARNING) => "WARN",
        Some(DiagnosticSeverity::INFORMATION) => "INFO",
        Some(DiagnosticSeverity::HINT) => "HINT",
        _ => "ERROR",
    };
    let line = diagnostic.range.start.line + 1;
    let column = diagnostic.range.start.character + 1;
    let message = &diagnostic.message;

    format!("{severity} [{line}:{column}] {message}")
}

/// The `<diagnostics>` block for one file, or `None` when it has no errors.
///
/// [`None`] rather than an empty string on purpose: upstream's caller guards
/// on `if (block)`, and an `Option` is that guard spelled in the type instead
/// of in every call site.
#[must_use]
pub fn report(file: &str, issues: &[Diagnostic]) -> Option<String> {
    let errors: Vec<&Diagnostic> = issues
        .iter()
        .filter(|issue| {
            issue
                .severity
                .is_none_or(|s| s == DiagnosticSeverity::ERROR)
        })
        .collect();
    if errors.is_empty() {
        return None;
    }

    let lines = errors
        .iter()
        .take(MAX_PER_FILE)
        .map(|issue| pretty(issue))
        .collect::<Vec<_>>()
        .join("\n");
    let suffix = match errors.len().saturating_sub(MAX_PER_FILE) {
        0 => String::new(),
        more => format!("\n... and {more} more"),
    };

    Some(format!(
        "<diagnostics file=\"{file}\">\n{lines}{suffix}\n</diagnostics>"
    ))
}

#[cfg(test)]
mod tests {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    use super::{MAX_PER_FILE, pretty, report};

    /// A diagnostic at `line`/`column` (both zero-based, as a server sends
    /// them) saying `message`.
    fn at(
        line: u32,
        column: u32,
        severity: Option<DiagnosticSeverity>,
        message: &str,
    ) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: column,
                },
                end: Position {
                    line,
                    character: column + 1,
                },
            },
            severity,
            message: message.to_owned(),
            ..Diagnostic::default()
        }
    }

    #[test]
    fn a_diagnostic_renders_one_based() {
        let rendered = pretty(&at(
            0,
            0,
            Some(DiagnosticSeverity::ERROR),
            "mismatched types",
        ));

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
            assert_eq!(
                rendered,
                format!("{expected} [5:3] something"),
                "{severity:?}"
            );
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

        assert_eq!(
            block
                .lines()
                .filter(|line| line.starts_with("ERROR"))
                .count(),
            MAX_PER_FILE
        );
        assert!(
            block.contains("\n... and 3 more"),
            "the tail is counted: {block}"
        );
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
}
