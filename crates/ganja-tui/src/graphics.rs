//! Inline image previews over the kitty graphics protocol — the same APC
//! (`ESC _ G … ESC \`) surface kitty's own `icat` speaks.
//!
//! Spec: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>. The subset
//! here is `icat`'s own for an in-band PNG: transmit with `a=t,f=100,t=d`,
//! base64 payload chunked at 4096 with `m=1`/`m=0`, then a placement
//! (`a=p`) at the cursor sized in cells, deleted by id (`a=d,d=i`) when the
//! preview moves or goes. Every command carries `q=2` so the terminal sends
//! no response back — this side never reads the APC channel, and an
//! unsolicited reply would land in the input stream as garbage keys.
//!
//! Under tmux each APC travels inside a passthrough envelope
//! (`ESC Ptmux;` … `ESC \` with every inner `ESC` doubled), which is how
//! `icat` reaches the real terminal through a multiplexer. tmux drops the
//! envelope unless `allow-passthrough` is on; degradation is silence, never
//! an error — exactly a graphics-less terminal's behavior.
//!
//! Support is **detected, not assumed**: `KITTY_WINDOW_ID` in the
//! environment names a kitty ancestor even where tmux rewrote `TERM`, and
//! everything else draws no previews at all. The tokens in the composer are
//! the feature; the pixels are a terminal's bonus.

/// Base64 characters per transmission chunk — `icat`'s own chunk size.
const CHUNK: usize = 4096;

/// How a graphics-capable terminal is reached: directly, or wrapped in
/// tmux's passthrough envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emitter {
    tmux: bool,
}

impl Emitter {
    /// The emitter this environment supports, or [`None`] where no kitty
    /// ancestor is present and a preview would be escape-sequence noise.
    #[must_use]
    pub fn detect() -> Option<Self> {
        std::env::var_os("KITTY_WINDOW_ID")?;

        Some(Self {
            tmux: std::env::var_os("TMUX").is_some(),
        })
    }

    /// An emitter for tests, reaching the terminal directly.
    #[cfg(test)]
    #[must_use]
    pub fn direct() -> Self {
        Self { tmux: false }
    }

    /// Transmits `png` under `id`, chunked the way `icat` chunks: the first
    /// command carries the control data, every command but the last says
    /// `m=1`, and the payload is base64 throughout.
    #[must_use]
    pub fn transmit(&self, id: u32, png: &[u8]) -> String {
        let encoded = crate::clipboard::base64(png);
        let chunks: Vec<&str> = encoded
            .as_bytes()
            .chunks(CHUNK)
            .map(|chunk| std::str::from_utf8(chunk).expect("base64 is ascii"))
            .collect();
        let mut wire = String::new();
        let last = chunks.len().saturating_sub(1);
        for (index, chunk) in chunks.iter().enumerate() {
            let more = u8::from(index != last);
            let apc = if index == 0 {
                format!("\x1b_Ga=t,f=100,t=d,i={id},q=2,m={more};{chunk}\x1b\\")
            } else {
                format!("\x1b_Gm={more};{chunk}\x1b\\")
            };
            wire.push_str(&self.wrapped(&apc));
        }

        wire
    }

    /// Creates the **virtual** placement (`U=1`) placeholder cells refer to,
    /// `columns` by `rows` cells — the tmux-proof half of the Unicode
    /// placeholder scheme: the image has no position of its own, so no
    /// cursor race can misplace it (2026-08-15, retiring the cursor-move
    /// placements the first cut used).
    #[must_use]
    pub fn virtual_placement(&self, id: u32, columns: u16, rows: u16) -> String {
        self.wrapped(&format!(
            "\x1b_Ga=p,U=1,i={id},c={columns},r={rows},q=2\x1b\\"
        ))
    }

    /// Deletes every placement this program made — the teardown broom.
    #[must_use]
    pub fn delete_all(&self) -> String {
        self.wrapped("\x1b_Ga=d,d=a,q=2\x1b\\")
    }

    /// The command as the terminal must receive it: bare, or inside tmux's
    /// passthrough envelope with every inner escape doubled.
    fn wrapped(&self, apc: &str) -> String {
        if self.tmux {
            format!("\x1bPtmux;{}\x1b\\", apc.replace('\x1b', "\x1b\x1b"))
        } else {
            apc.to_owned()
        }
    }
}

/// A preview's transmissible form: PNG bytes and the pixel box they hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// The thumbnail, re-encoded PNG — the one format `f=100` transmits.
    pub png: Vec<u8>,
    /// Thumbnail width in pixels.
    pub width: u32,
    /// Thumbnail height in pixels.
    pub height: u32,
}

/// Longest edge a preview keeps, in pixels: a five-row strip never needs a
/// photo's megapixels, and every pixel rides the wire base64ed.
const MAX_EDGE: u32 = 512;

/// Decodes any of the four attachment image formats — png, jpeg, gif (its
/// first frame), webp — bounds its longest edge to 512 pixels with aspect
/// kept, and re-encodes PNG for transmission. [`None`] is a file that is missing,
/// unreadable, or none of the four; the token in the composer still stands,
/// exactly as it does on a terminal with no graphics at all.
#[must_use]
pub fn load(path: &str) -> Option<Preview> {
    let decoded = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    // `thumbnail` fits the box in both directions — it would inflate a small
    // image too, and an upscaled preview is worse than the original pixels.
    let bounded = if decoded.width() > MAX_EDGE || decoded.height() > MAX_EDGE {
        decoded.thumbnail(MAX_EDGE, MAX_EDGE)
    } else {
        decoded
    };
    let rgba = bounded.to_rgba8();
    let (width, height) = rgba.dimensions();

    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&rgba).ok()?;
    drop(writer);

    Some(Preview { png, width, height })
}

/// The base character of a kitty Unicode image placeholder (U+10EEEE): a
/// cell holding it, with the image id in its foreground color and the
/// row/column diacritics after it, is composited over by the terminal.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// The first 64 of kitty's own row/column diacritics
/// (`gen/rowcolumn-diacritics.txt`), in table order — placements here never
/// exceed 60 columns, so the tail of the 297 is never needed.
const DIACRITICS: [char; 64] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
];

/// One placeholder grapheme: the base character plus the diacritics naming
/// which cell of the virtual placement this is.
#[must_use]
pub fn placeholder(row: u16, column: u16) -> String {
    let mut cell = String::new();
    cell.push(PLACEHOLDER);
    cell.push(DIACRITICS[usize::from(row) % DIACRITICS.len()]);
    cell.push(DIACRITICS[usize::from(column) % DIACRITICS.len()]);

    cell
}

/// The foreground color that carries `id` to the terminal: its low 24 bits
/// as RGB, which is how a placeholder cell says which image it shows.
#[must_use]
pub fn id_color(id: u32) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(
        u8::try_from((id >> 16) & 0xFF).unwrap_or(0),
        u8::try_from((id >> 8) & 0xFF).unwrap_or(0),
        u8::try_from(id & 0xFF).unwrap_or(0),
    )
}

/// The cell box a `width`×`height` image fills at `rows` rows tall, aspect
/// kept under a terminal cell twice as tall as it is wide.
#[must_use]
pub fn columns_for(width: u32, height: u32, rows: u16) -> u16 {
    let columns = (u64::from(width) * u64::from(rows) * 2).div_ceil(u64::from(height.max(1)));

    u16::try_from(columns).unwrap_or(u16::MAX).max(1)
}

#[cfg(test)]
mod tests {
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
        assert_eq!(commands.len(), 2, "5334 base64 chars split once");
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
        82, 73, 70, 70, 30, 0, 0, 0, 87, 69, 66, 80, 86, 80, 56, 76, 17, 0, 0, 0, 47, 5, 128, 0, 0,
        7, 80, 178, 234, 151, 162, 255, 129, 136, 232, 127, 0, 0,
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
}
