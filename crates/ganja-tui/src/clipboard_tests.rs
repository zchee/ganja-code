use super::{Clipboard as _, Error, Image, Recording, osc52};

/// The payload of an OSC 52 sequence, between the `;c;` introducer and the
/// BEL terminator. Read out of the whole sequence rather than compared
/// against it, because the tmux wrap this process may or may not be under
/// decides the outer framing and nothing else.
fn payload(sequence: &str) -> &str {
    let (_, rest) = sequence.split_once(";c;").expect("the OSC 52 introducer");
    rest.split_once('\u{7}').expect("the BEL terminator").0
}

/// The RFC 4648 test vectors, plus the padding boundaries, asserted where
/// the encoding is actually load-bearing: what a terminal reads off the
/// wire has to be the standard alphabet, padded, or the paste is garbage.
#[test]
fn an_osc52_payload_matches_the_standard_encoding_including_padding() {
    let cases = [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ];

    for (input, expected) in cases {
        assert_eq!(
            payload(&osc52::sequence(input)),
            expected,
            "encoding {input:?}"
        );
    }
}

/// Non-ASCII text is encoded as its UTF-8 bytes, the way upstream's
/// `Buffer.from(text)` does.
#[test]
fn an_osc52_payload_encodes_the_utf8_bytes_of_multibyte_text() {
    // "é" is two UTF-8 bytes (0xC3 0xA9).
    assert_eq!(payload(&osc52::sequence("é")), "w6k=");
}

/// A copy produces the exact escape upstream writes, and its base64 decodes
/// to the copied text. The wrap case is the multiplexer's, so the bare form
/// is what a plain terminal (no `$TMUX`/`$STY`) sees.
#[test]
fn a_copy_emits_one_osc52_sequence_whose_base64_is_the_text() {
    // The env this test process runs under decides the wrap; assert on the
    // payload rather than the exact framing so a CI running inside tmux is
    // not a false failure.
    let sequence = osc52::sequence("copy me");

    assert!(
        sequence.contains("\x1b]52;c;"),
        "the OSC 52 opener: {sequence:?}"
    );
    assert!(
        // The literal rather than a re-encode: a pin that computes its own
        // expectation is a pin against nothing.
        sequence.contains("Y29weSBtZQ=="),
        "the payload is the text's base64: {sequence:?}"
    );
    assert!(
        sequence.ends_with('\u{7}') || sequence.ends_with('\\'),
        "a terminator: {sequence:?}"
    );
}

#[test]
fn a_recording_clipboard_keeps_every_write_in_order() {
    let mut clipboard = Recording::default();
    let log = clipboard.log();

    clipboard.write("first").expect("the write is accepted");
    clipboard.write("second").expect("the write is accepted");

    assert_eq!(
        *log.lock().expect("the lock holds"),
        vec!["first".to_owned(), "second".to_owned()]
    );
}

#[test]
fn a_refused_write_is_not_recorded() {
    let mut clipboard = Recording::refusing_writes(Error::Unavailable("no display".to_owned()));
    let log = clipboard.log();

    let refusal = clipboard.write("nothing lands").expect_err("it refuses");

    assert!(format!("{refusal}").contains("no display"));
    assert!(log.lock().expect("the lock holds").is_empty());
}

/// An empty clipboard holds neither — `image-data` is exactly what makes
/// that distinguishable from holding one or the other (**F3**, D111's
/// image half).
#[test]
fn a_clipboard_nothing_was_put_on_holds_neither_text_nor_image() {
    assert_eq!(Recording::default().read(), Err(Error::NotText));
    assert_eq!(Recording::default().read_image(), Err(Error::NoImage));
}

#[test]
fn a_clipboard_holding_text_reads_it_and_has_no_image() {
    let mut clipboard = Recording::holding("typed");

    assert_eq!(clipboard.read(), Ok("typed".to_owned()));
    assert_eq!(clipboard.read_image(), Err(Error::NoImage));
}

#[test]
fn a_clipboard_holding_an_image_reads_it_and_has_no_text() {
    let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255];
    let mut clipboard = Recording::holding_image(2, 1, rgba.clone());

    assert_eq!(
        clipboard.read_image(),
        Ok(Image {
            width: 2,
            height: 1,
            rgba,
        })
    );
    assert_eq!(clipboard.read(), Err(Error::NotText));
}

/// The real [`System`] fails both questions the same way when there is no
/// clipboard to ask at all, so the double must too.
#[test]
fn an_unreachable_clipboard_refuses_both_text_and_image_reads() {
    let error = Error::Unavailable("no display".to_owned());
    let mut clipboard = Recording::refusing_reads(error.clone());

    assert_eq!(clipboard.read(), Err(error.clone()));
    assert_eq!(clipboard.read_image(), Err(error));
}

/// The one thing the seam above cannot prove: that [`System`] is wired to
/// a clipboard that really works.
///
/// `#[ignore]`d because it needs a desktop CI does not have, **and because
/// it overwrites whatever the person running it had copied** — which is
/// why it puts something recognizable there rather than a fixture string
/// that would look like a bug in whatever they paste it into. Run with
/// `cargo test -p ganja-tui -- --ignored the_system_clipboard`.
#[test]
#[ignore = "needs a desktop clipboard, and overwrites what is on it"]
fn the_system_clipboard_round_trips_what_it_is_handed() {
    let mut clipboard = super::System::default();
    let written = "ganja clipboard smoke test";

    clipboard.write(written).expect("the desktop accepts text");

    assert_eq!(clipboard.read().as_deref(), Ok(written));
}
