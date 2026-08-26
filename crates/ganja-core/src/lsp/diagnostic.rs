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
#[path = "diagnostic_tests.rs"]
mod tests;
