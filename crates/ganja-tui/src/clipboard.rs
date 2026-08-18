//! The system clipboard, behind a seam.
//!
//! Spec: upstream `packages/tui/src/clipboard.ts` and
//! `context/clipboard.tsx`, which injects the same pair of functions so its
//! own tests can watch what was copied. Ganja needs that seam for a harder
//! reason: a headless CI has no clipboard at all, so a copy command that
//! spoke to [`arboard`] directly would be a command no test could ever run.
//!
//! Both channels upstream writes are written here too: this trait owns the
//! system clipboard (through [`arboard`]), and the app's copy path writes the
//! OSC 52 escape ([`osc52`]) beside it — the terminal's own channel, which is
//! what carries a copy from a tmux pane or an SSH session back to the machine
//! the terminal is attached to rather than the one the process runs on.
//!
//! **Images are read now (F3).** The workspace turns on `arboard`'s
//! `image-data` feature, which is what lets this trait tell an image-holding
//! clipboard from an empty one at all — without it, `arboard` answers
//! `ContentNotAvailable` for both, indistinguishably. [`Clipboard::read`] and
//! [`Clipboard::read_image`] are independent questions with independent
//! answers: a text clipboard has no image, an image clipboard has no text,
//! and a paste that finds neither tries the other before giving up. Encoding
//! the pixels to PNG is the app's job (`app.rs`), not this seam's — this
//! module hands back raw RGBA and nothing else.
//!
//! One deliberate narrowing from upstream remains:
//!
//! - **A long paste is not folded.** Upstream's composer collapses a paste of
//!   three or more lines (or over 150 characters) to a `[Pasted ~N lines]`
//!   placeholder that expands on demand (`component/prompt/index.tsx:1205-
//!   1211`); this build still inserts the whole thing. **D111** is narrowed to
//!   exactly this half now that the image half above is ported.
//!
//! Construction is **lazy**: the handle is built on the first copy or paste
//! and kept, so a session that never touches the clipboard never asks the
//! desktop for one — and a machine that cannot provide one costs a status
//! notice at the moment it is asked rather than a refusal at startup.

/// Why a clipboard operation could not be carried out.
///
/// Cloneable so a test double can hold one and hand it back on demand.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// There is no clipboard to talk to, or it refused.
    #[error("the clipboard is not available: {0}")]
    Unavailable(String),
    /// The clipboard does not hold text. It may hold an image instead — see
    /// [`Clipboard::read_image`] — or nothing this build can read at all.
    #[error("the clipboard does not hold text")]
    NotText,
    /// The clipboard does not hold an image. It may hold text instead — see
    /// [`Clipboard::read`] — or nothing this build can read at all.
    #[error("the clipboard does not hold an image")]
    NoImage,
    /// The clipboard does not hold copied files. A file copied in a file
    /// manager usually rides beside a text spelling of its *name* — which is
    /// why [`Clipboard::read_files`] must be asked before [`Clipboard::read`]
    /// (2026-08-15).
    #[error("the clipboard does not hold files")]
    NoFiles,
}

/// RGBA8 pixels read from the clipboard, row-major with no padding: exactly
/// `width * height * 4` bytes.
///
/// [`arboard::ImageData`]'s own shape, copied rather than reused in this
/// trait's signature — a `Recording` double has no `arboard` handle to
/// borrow pixels from, so the seam needs a type independent of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// Pixel width.
    pub width: usize,
    /// Pixel height.
    pub height: usize,
    /// Four bytes per pixel — red, green, blue, alpha — in row-major order.
    pub rgba: Vec<u8>,
}

/// Reading and writing the system clipboard.
///
/// `&mut self` on every method because the platform handles behind them are
/// not shareable, and because it keeps the lazy construction honest: the one
/// place that may build a handle is the one place that is already exclusive.
pub trait Clipboard: Send {
    /// Puts `text` on the clipboard, replacing whatever was there.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no clipboard, or when it refused what
    /// it was handed.
    fn write(&mut self, text: &str) -> Result<(), Error>;

    /// What the clipboard holds, as text.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotText`] when it holds something else — an image, or
    /// nothing — and [`Error::Unavailable`] when there is nothing to ask.
    fn read(&mut self) -> Result<String, Error>;

    /// What the clipboard holds, as an image.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoImage`] when it holds something else — text, or
    /// nothing — and [`Error::Unavailable`] when there is nothing to ask.
    fn read_image(&mut self) -> Result<Image, Error>;

    /// The files the clipboard holds, when what was copied was files.
    ///
    /// Asked **first** at paste time (2026-08-15): a file copied in Finder
    /// puts the file's URL *and* its bare name as text on the pasteboard, so
    /// a paste that asked for text first would insert a basename that
    /// resolves nowhere — the screenshot that pinned this bug.
    fn read_files(&mut self) -> Result<Vec<std::path::PathBuf>, Error>;
}

/// The clipboard the desktop this process runs on provides.
#[derive(Default)]
pub struct System {
    /// Built on first use and kept: `arboard` holds a connection to the
    /// display server on Linux, and reconnecting per copy would be a
    /// round trip for every keystroke that pastes.
    handle: Option<arboard::Clipboard>,
}

impl System {
    /// The handle, building it if this is the first call.
    ///
    /// A failure is **not** remembered. Nothing here can tell a desktop that
    /// will never appear from one that is still coming up, and the cost of
    /// being wrong the optimistic way is one more failed call.
    fn handle(&mut self) -> Result<&mut arboard::Clipboard, Error> {
        if self.handle.is_none() {
            self.handle = Some(
                arboard::Clipboard::new().map_err(|error| Error::Unavailable(error.to_string()))?,
            );
        }

        self.handle
            .as_mut()
            .ok_or_else(|| Error::Unavailable("the clipboard went away".to_owned()))
    }
}

impl Clipboard for System {
    fn write(&mut self, text: &str) -> Result<(), Error> {
        self.handle()?
            .set_text(text)
            .map_err(|error| Error::Unavailable(error.to_string()))
    }

    fn read(&mut self) -> Result<String, Error> {
        self.handle()?.get_text().map_err(|error| match error {
            // The one error that is about the *contents* rather than about
            // the clipboard: `arboard` documents it as "empty, or holding a
            // different format", which now specifically means "not text" —
            // an image clipboard answers `read_image` instead.
            arboard::Error::ContentNotAvailable => Error::NotText,
            other => Error::Unavailable(other.to_string()),
        })
    }

    fn read_image(&mut self) -> Result<Image, Error> {
        let image = self.handle()?.get_image().map_err(|error| match error {
            arboard::Error::ContentNotAvailable => Error::NoImage,
            other => Error::Unavailable(other.to_string()),
        })?;

        Ok(Image {
            width: image.width,
            height: image.height,
            rgba: image.bytes.into_owned(),
        })
    }

    fn read_files(&mut self) -> Result<Vec<std::path::PathBuf>, Error> {
        self.handle()?
            .get()
            .file_list()
            .map_err(|error| match error {
                arboard::Error::ContentNotAvailable => Error::NoFiles,
                other => Error::Unavailable(other.to_string()),
            })
    }
}

/// The terminal's own clipboard channel, OSC 52.
///
/// Spec: upstream `packages/tui/src/clipboard.ts:25`. Written beside the
/// system clipboard on every copy, not instead of it — the escape is what
/// reaches the terminal a tmux pane or an SSH session is really attached to,
/// which [`arboard`] on the far machine never can.
pub mod osc52 {
    use base64::Engine as _;

    /// The OSC 52 sequence that puts `text` on the terminal's clipboard.
    ///
    /// Upstream's exact bytes: `ESC ] 52 ; c ; <base64> BEL`. When `$TMUX` or
    /// `$STY` names an outer multiplexer, the sequence is wrapped in the
    /// passthrough that gets it past the multiplexer to the real terminal
    /// (`clipboard.ts:26`) — without that wrap a copy from inside tmux never
    /// leaves the pane, which is the whole SSH/tmux case this channel exists
    /// for.
    ///
    /// No size cap: upstream emits unbounded, and matching that is chosen over
    /// guarding against a terminal that truncates a very large selection —
    /// upstream fidelity wins, and the plan left the cap to review
    /// (deviation-free: this is upstream's behavior).
    #[must_use]
    pub fn sequence(text: &str) -> String {
        // Standard alphabet, padded: RFC 4648 §4 is the encoding OSC 52 is
        // defined in terms of, and the one upstream's `toString("base64")`
        // produces.
        let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let inner = format!("\x1b]52;c;{payload}\x07");

        if wrapped_for_multiplexer() {
            // The multiplexer swallows an escape it does not recognize; the
            // `ESC P tmux ; ESC <inner> ESC \` passthrough tells it to forward
            // the bytes verbatim, and every `ESC` inside `inner` is doubled by
            // being preceded with one here.
            format!("\x1bPtmux;\x1b{inner}\x1b\\")
        } else {
            inner
        }
    }

    /// Whether an outer terminal multiplexer needs the passthrough wrap.
    fn wrapped_for_multiplexer() -> bool {
        std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some()
    }
}

/// A clipboard that remembers what it was handed and answers reads from a
/// script.
///
/// The whole reason the trait above exists: every copy command asserts against
/// this, so the assertions are about what ganja decided to copy rather than
/// about what some machine's desktop happened to accept.
#[cfg(test)]
#[derive(Debug)]
pub struct Recording {
    /// Everything written, in order.
    pub written: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// What the next text read answers with.
    pub holds: Result<String, Error>,
    /// What the next image read answers with.
    pub holds_image: Result<Image, Error>,
    /// What the next file-list read answers with.
    pub holds_files: Result<Vec<std::path::PathBuf>, Error>,
    /// What every write answers with; [`Ok`] by default.
    pub accepts: Result<(), Error>,
}

#[cfg(test)]
impl Default for Recording {
    /// A clipboard nothing has been put on, which is the answer a real one
    /// gives for an empty desktop selection: neither text nor an image.
    fn default() -> Self {
        Self {
            written: std::sync::Arc::default(),
            holds: Err(Error::NotText),
            holds_image: Err(Error::NoImage),
            holds_files: Err(Error::NoFiles),
            accepts: Ok(()),
        }
    }
}

#[cfg(test)]
impl Recording {
    /// A clipboard holding `text`, accepting everything written to it.
    pub fn holding(text: &str) -> Self {
        Self {
            holds: Ok(text.to_owned()),
            ..Self::default()
        }
    }

    /// A clipboard holding a `width`×`height` image, its pixels `rgba`.
    pub fn holding_image(width: usize, height: usize, rgba: Vec<u8>) -> Self {
        Self {
            holds_image: Ok(Image {
                width,
                height,
                rgba,
            }),
            ..Self::default()
        }
    }

    /// A clipboard holding copied files, the way a Finder ⌘C leaves one —
    /// beside, on a real pasteboard, a text spelling of the bare names that
    /// this double deliberately also carries, so a test exercises the order
    /// the paste consults the two in (2026-08-15).
    pub fn holding_files(files: Vec<std::path::PathBuf>) -> Self {
        let names = files
            .iter()
            .filter_map(|file| file.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");

        Self {
            holds: Ok(names),
            holds_files: Ok(files),
            ..Self::default()
        }
    }

    /// A clipboard whose reads — text and image alike — fail with `error`.
    ///
    /// The real [`System`] cannot fail one and not the other for the same
    /// reason: both go through the one lazily built [`arboard::Clipboard`]
    /// handle, so a machine with no clipboard at all refuses both questions
    /// identically.
    pub fn refusing_reads(error: Error) -> Self {
        Self {
            holds: Err(error.clone()),
            holds_image: Err(error.clone()),
            holds_files: Err(error),
            ..Self::default()
        }
    }

    /// A clipboard whose writes fail with `error`.
    pub fn refusing_writes(error: Error) -> Self {
        Self {
            accepts: Err(error),
            ..Self::default()
        }
    }

    /// A handle onto what this clipboard is handed, which survives the move
    /// into the app.
    pub fn log(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        std::sync::Arc::clone(&self.written)
    }
}

#[cfg(test)]
impl Clipboard for Recording {
    fn write(&mut self, text: &str) -> Result<(), Error> {
        self.accepts.clone()?;
        self.written
            .lock()
            .expect("the recording lock is never poisoned")
            .push(text.to_owned());

        Ok(())
    }

    fn read(&mut self) -> Result<String, Error> {
        self.holds.clone()
    }

    fn read_image(&mut self) -> Result<Image, Error> {
        self.holds_image.clone()
    }

    fn read_files(&mut self) -> Result<Vec<std::path::PathBuf>, Error> {
        self.holds_files.clone()
    }
}

#[cfg(test)]
mod tests {
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
}
