use super::{is_binary, mime};

/// Upstream's table, row for row, plus the fallback everything else gets.
#[test]
fn the_mime_table_is_upstreams_allowlist_verbatim() {
    let cases = [
        ("a.avif", "image/avif"),
        ("a.gif", "image/gif"),
        ("a.jpeg", "image/jpeg"),
        ("a.jpg", "image/jpeg"),
        ("a.pdf", "application/pdf"),
        ("a.png", "image/png"),
        ("a.svg", "image/svg+xml"),
        ("a.webp", "image/webp"),
        ("src/lib.rs", "text/plain"),
        ("README", "text/plain"),
        ("archive.tar.gz", "text/plain"),
    ];

    for (path, expected) in cases {
        assert_eq!(mime(path), expected, "{path}");
    }
}

#[test]
fn the_extension_lookup_is_case_insensitive() {
    assert_eq!(mime("SHOT.PNG"), "image/png");
    assert_eq!(mime("photo.JpG"), "image/jpeg");
}

/// SVG is the one image that reads as text, which is the whole reason the
/// binary question is asked of the mime rather than of the extension.
#[test]
fn svg_reads_as_text_and_the_other_attachments_as_bytes() {
    assert!(!is_binary("image/svg+xml"));
    assert!(!is_binary("text/plain"));

    for mime in
        ["image/avif", "image/gif", "image/jpeg", "image/png", "image/webp", "application/pdf"]
    {
        assert!(is_binary(mime), "{mime}");
    }
}
