//! What an `@`-mentioned file attaches *as*: the mime its extension names,
//! and whether that mime travels as text or as bytes.
//!
//! Spec: upstream `packages/tui/src/component/prompt/local-attachment.ts`,
//! whose extension table this carries verbatim. Upstream splits the same way
//! this module's two questions do: SVG is read as *text*, every other
//! allowlisted mime as *bytes*, and anything outside the table is not an
//! attachment at all. One deliberate spelling difference: upstream answers an
//! unknown extension with `application/octet-stream` and then declines to
//! attach it, while here the answer is `text/plain` directly — a declined
//! attachment *is* a text mention in this build, and `text/plain` is the mime
//! every mention carried before the table existed, so old transcripts and new
//! ones spell the common case identically.
//!
//! The table lives in the engine rather than the TUI because the *read*
//! happens at request build (`session::resolve_mentions`), but it is public
//! because a frontend needs the same answer earlier: the mime a mention will
//! carry is what the status line's degradation notice is about.

use std::path::Path;

/// The mime `path`'s extension names, or `text/plain` for everything the
/// attachment allowlist does not.
///
/// Case-insensitive on the extension, as upstream's `toLowerCase()` is.
#[must_use]
pub fn mime(path: &str) -> &'static str {
    let Some(extension) = Path::new(path).extension().and_then(|ext| ext.to_str()) else {
        return "text/plain";
    };

    // Upstream's `mimeTypes` table, entry for entry.
    match extension.to_ascii_lowercase().as_str() {
        "avif" => "image/avif",
        "gif" => "image/gif",
        "jpeg" | "jpg" => "image/jpeg",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        _ => "text/plain",
    }
}

/// Whether `mime` is read as bytes at send time — base64 into the request's
/// file part — rather than inlined as text.
///
/// Upstream's split exactly: the allowlist's images and its PDF are bytes,
/// SVG is text despite being an image, and everything else was never an
/// attachment to begin with.
#[must_use]
pub fn is_binary(mime: &str) -> bool {
    matches!(
        mime,
        "image/avif" | "image/gif" | "image/jpeg" | "image/png" | "image/webp" | "application/pdf"
    )
}

#[cfg(test)]
#[path = "attachment_tests.rs"]
mod tests;
