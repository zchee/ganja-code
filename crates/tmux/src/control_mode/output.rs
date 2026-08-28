//! Spec: pandaemonium pkg/tmux/output.go.
//!
//! tmux escapes control bytes and backslash inside `%output`/
//! `%extended-output` notification values as `\NNN` octal sequences over
//! raw bytes, which are not necessarily valid UTF-8 — pane output is not
//! guaranteed to be text. [`decode_output_value`] recovers the underlying
//! bytes; the crate-private `text`/`text_lossy` variants (used by the typed
//! notification accessors in the `notification` module) additionally
//! validate or lossily repair UTF-8.

/// An error decoding a tmux `%output`/`%extended-output` escaped value.
///
/// Spec: pandaemonium pkg/tmux/output.go (the `fmt.Errorf` sites in
/// `decodeOutputValuePartial` and `decodeOutputText`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// A trailing `\` had fewer than three digits left in the value.
    #[error("tmux: incomplete octal escape at byte {byte}")]
    IncompleteEscape {
        /// Byte offset of the `\` that started the incomplete escape.
        byte: usize,
    },
    /// An escape digit fell outside the octal range `0`-`7`.
    #[error("tmux: invalid octal digit {digit:?} at byte {byte}")]
    InvalidOctalDigit {
        /// The offending byte, rendered as a character for display.
        digit: char,
        /// Byte offset of the offending digit.
        byte: usize,
    },
    /// A three-digit octal escape decoded past `0xff`.
    #[error("tmux: octal escape at byte {byte} is out of range")]
    EscapeOutOfRange {
        /// Byte offset of the `\` that started the out-of-range escape.
        byte: usize,
    },
    /// The fully decoded byte sequence was not valid UTF-8.
    #[error("tmux: decoded output is not valid UTF-8")]
    NotUtf8,
}

/// Decodes a tmux `%output`/`%extended-output` escaped value to terminal
/// bytes.
///
/// Spec: pandaemonium pkg/tmux/output.go (`DecodeOutputValue`).
pub fn decode_output_value(value: &str) -> Result<Vec<u8>, DecodeError> {
    let (bytes, err) = decode_output_value_partial(value);
    match err {
        Some(err) => Err(err),
        None => Ok(bytes),
    }
}

/// Decodes `value` and returns the bytes successfully decoded before the
/// first error, alongside that error if decoding stopped early. The
/// returned bytes always reflect everything decoded before the error site,
/// so a caller can build a best-effort lossy rendering without re-scanning
/// the input.
///
/// Spec: pandaemonium pkg/tmux/output.go (`decodeOutputValuePartial`).
pub(crate) fn decode_output_value_partial(value: &str) -> (Vec<u8>, Option<DecodeError>) {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        if i + 3 >= bytes.len() {
            return (out, Some(DecodeError::IncompleteEscape { byte: i }));
        }
        let mut v: u32 = 0;
        for offset in 1..=3usize {
            let digit = bytes[i + offset];
            if !(b'0'..=b'7').contains(&digit) {
                return (
                    out,
                    Some(DecodeError::InvalidOctalDigit { digit: digit as char, byte: i + offset }),
                );
            }
            v = v * 8 + u32::from(digit - b'0');
        }
        if v > 0xff {
            return (out, Some(DecodeError::EscapeOutOfRange { byte: i }));
        }
        out.push(v as u8);
        i += 4;
    }
    (out, None)
}

/// Decodes `value` to terminal bytes and validates the result as UTF-8.
///
/// Spec: pandaemonium pkg/tmux/output.go (`decodeOutputText`); used by the
/// typed notification accessors in the `notification` module.
pub(crate) fn decode_output_text(value: &str) -> Result<String, DecodeError> {
    let bytes = decode_output_value(value)?;
    String::from_utf8(bytes).map_err(|_| DecodeError::NotUtf8)
}

/// Decodes `value` to terminal bytes and lossily repairs any invalid UTF-8
/// (each invalid byte sequence becomes U+FFFD), returning everything
/// decoded before a decode error rather than an empty string.
///
/// Spec: pandaemonium pkg/tmux/output.go (`decodeOutputTextLossy`).
pub(crate) fn decode_output_text_lossy(value: &str) -> String {
    let (bytes, _) = decode_output_value_partial(value);
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
