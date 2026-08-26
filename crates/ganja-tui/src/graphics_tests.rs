use super::{Emitter, columns_for, load};

/// A one-chunk transmission is `icat`'s own control string: in-band PNG,
/// the id, responses suppressed, and the final chunk marked last.
#[test]
fn a_small_transmission_is_one_final_chunk() {
    let wire = Emitter::direct().transmit(7, b"png-bytes");

    assert_eq!(wire, "\x1b_Ga=t,f=100,t=d,i=7,q=2,m=0;cG5nLWJ5dGVz\x1b\\");
}

/// Payloads over the chunk size split at 4096 base64 characters, `m=1`
/// on every chunk but the last — and only the first carries the control
/// data.
#[test]
fn a_large_transmission_chunks_at_the_icat_size() {
    let wire = Emitter::direct().transmit(1, &[0u8; 4000]);

    let commands: Vec<&str> = wire.split("\x1b\\").filter(|s| !s.is_empty()).collect();
    assert_eq!(commands.len(), 2, "5336 base64 chars split once");
    assert!(commands[0].starts_with("\x1b_Ga=t,f=100,t=d,i=1,q=2,m=1;"));
    assert!(commands[1].starts_with("\x1b_Gm=0;"));
}

/// The virtual placement carries `U=1` beside its cell box, and the
/// teardown broom deletes everything — both silenced.
#[test]
fn virtual_placement_and_deletion_speak_the_documented_keys() {
    let emitter = Emitter::direct();

    assert_eq!(
        emitter.virtual_placement(3, 10, 5),
        "\x1b_Ga=p,U=1,i=3,c=10,r=5,q=2\x1b\\"
    );
    assert_eq!(emitter.delete_all(), "\x1b_Ga=d,d=a,q=2\x1b\\");
}

/// A placeholder grapheme is the base character with kitty's own row and
/// column diacritics, and the id rides the foreground color's 24 bits.
#[test]
fn placeholders_carry_kittys_own_diacritics_and_the_id_rides_the_color() {
    assert_eq!(super::placeholder(0, 0), "\u{10EEEE}\u{0305}\u{0305}");
    assert_eq!(super::placeholder(1, 2), "\u{10EEEE}\u{030D}\u{030E}");
    assert_eq!(
        super::id_color(0x0001_0203),
        ratatui::style::Color::Rgb(1, 2, 3)
    );
}

/// Under tmux the whole APC rides the passthrough envelope with every
/// inner escape doubled — `icat`'s own multiplexer road.
#[test]
fn tmux_wraps_the_apc_in_a_passthrough_envelope() {
    let emitter = Emitter { tmux: true };

    assert_eq!(
        emitter.delete_all(),
        "\x1bPtmux;\x1b\x1b_Ga=d,d=a,q=2\x1b\x1b\\\x1b\\"
    );
}

/// A real 6×3 lossless WebP, encoded once with `cwebp` and carried as
/// bytes because the `image` crate decodes WebP but does not encode it —
/// the fixture cannot be generated at test time.
const WEBP: [u8; 38] = [
    82, 73, 70, 70, 30, 0, 0, 0, 87, 69, 66, 80, 86, 80, 56, 76, 17, 0, 0, 0, 47, 5, 128, 0, 0, 7,
    80, 178, 234, 151, 162, 255, 129, 136, 232, 127, 0, 0,
];

/// The four formats the attachment table names all load, land under the
/// bound, and come back out as PNG; garbage is a refusal, not a panic.
#[test]
fn every_attachment_image_format_loads_and_garbage_does_not() {
    let dir = tempfile::tempdir().expect("a directory for the fixtures");
    // RGB rather than RGBA because the jpeg encoder refuses an alpha
    // channel; the loader hands back RGBA regardless.
    let source = image::RgbImage::from_pixel(6, 3, image::Rgb([250, 100, 20]));
    for name in ["a.png", "a.jpg", "a.gif"] {
        source
            .save(dir.path().join(name))
            .expect("the fixture encodes");
    }
    std::fs::write(dir.path().join("a.webp"), WEBP).expect("the webp fixture writes");

    for name in ["a.png", "a.jpg", "a.gif", "a.webp"] {
        let path = dir.path().join(name).display().to_string();
        let preview = load(&path).expect("the four formats all decode");
        assert_eq!(
            (preview.width, preview.height),
            (6, 3),
            "{name} keeps its box"
        );
        assert!(
            preview.png.starts_with(&[0x89, b'P', b'N', b'G']),
            "{name} re-encodes as PNG"
        );
    }

    let garbage = dir.path().join("garbage.webp");
    std::fs::write(&garbage, b"not an image at all").expect("the garbage writes");
    assert_eq!(load(&garbage.display().to_string()), None);
    assert_eq!(load("/nowhere/missing.png"), None);
}

/// Cell aspect is two rows to a square: a square image five rows tall is
/// ten columns wide, and a zero-height image cannot divide by zero.
#[test]
fn the_cell_box_keeps_aspect_under_tall_cells() {
    assert_eq!(columns_for(100, 100, 5), 10);
    assert_eq!(columns_for(200, 100, 5), 20);
    assert_eq!(columns_for(100, 0, 5), 1000);
}
