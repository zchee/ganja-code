use std::path::Path;

use super::{Bounded, is_binary, mime, read_bounded};

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

/// `yr3e`: one byte past the limit is over, and no bytes are handed back to
/// be encoded.
#[test]
fn a_file_one_byte_over_the_limit_is_over() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("shot.png");
    std::fs::write(&path, b"12345").expect("the fixture writes");

    assert_eq!(read_bounded(&path, 4).expect("the file reads"), Bounded::Over);
}

/// The limit is inclusive: a file holding exactly that many bytes is read
/// whole, every byte of it.
#[test]
fn a_file_exactly_at_the_limit_is_read_whole() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("shot.png");
    std::fs::write(&path, b"1234").expect("the fixture writes");

    assert_eq!(read_bounded(&path, 4).expect("the file reads"), Bounded::Whole(b"1234".to_vec()));
}

/// The read a size check before it cannot bound: `/dev/zero` has no end, so a
/// read that trusted the file to stop would never return, and this one stops
/// one byte past the limit.
#[cfg(unix)]
#[test]
fn an_endless_file_is_over_the_limit_rather_than_read_whole() {
    assert_eq!(read_bounded(Path::new("/dev/zero"), 16).expect("the device reads"), Bounded::Over);
}

/// A file that is not there is the open's own error, for the caller's
/// "could not be read" block to name.
#[test]
fn a_file_that_is_not_there_is_the_error_opening_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");

    let error = read_bounded(&dir.path().join("gone.png"), 4).expect_err("nothing to open");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
