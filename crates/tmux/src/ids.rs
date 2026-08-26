//! Spec: pandaemonium `pkg/tmux/notification.go`
//! (`validatePaneID`/`validateWindowID`/`validateSessionID`, consolidated
//! here so all three id validators live with the newtypes they guard; Go
//! spells the pane validator in `flow.go` and the other two in
//! `notification.go`).
//!
//! An id names a tmux object, not a control-mode concept: the `%0` a
//! `%output` notification carries is the same `%0` a one-shot `tmux
//! list-panes` prints. The newtypes therefore live at the crate root, where
//! any surface can speak them, rather than under [`crate::control_mode`].
//!
//! # Validated newtypes, not validate-at-use (divergence)
//!
//! Go's `PaneID`, `WindowID`, and `SessionID` are unchecked string aliases —
//! `type PaneID string` — validated only at each call site
//! (`validatePaneID` is invoked separately by every consumer: `flow.go`'s
//! `Client` helpers, and `notification.go`'s typed accessors). Rust makes
//! them **validated newtypes**: [`PaneId::new`], [`WindowId::new`], and
//! [`SessionId::new`] are the only way to build one, so construction is the
//! single validation point and every value already in the type system is
//! guaranteed well-formed thereafter (parse, don't validate). This is why a
//! [`crate::control_mode::notification::Notification`]'s raw `%`-fields stay
//! plain `String`/`Vec<String>` until a typed accessor classifies and
//! validates them into one of these newtypes exactly once, rather than Go's
//! re-validate-per-call pattern.
//!
//! # Quoting divergence
//!
//! Every validator message below mirrors Go's wording with one mechanical
//! substitution: Go's `%q` verb (Go-escaped double-quoting) becomes Rust's
//! `{:?}` `Debug` quoting for `&str`. The two escaping rules differ only on
//! exotic non-printable/non-ASCII input; since this text is a human-readable
//! error message and never wire data, the substitution is not documented
//! again at each call site.

/// A tmux pane id was refused: it did not have the expected prefix followed
/// by one or more decimal digits.
///
/// Shared by [`PaneId`], [`WindowId`], and [`SessionId`], which differ only
/// in which prefix character and noun they validate against.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct InvalidId {
    message: String,
}

/// Checks that `value` is `prefix` followed by one or more ASCII decimal
/// digits and nothing else.
///
/// Ports the shared shape of Go's `validatePaneID`/`validateWindowID`/
/// `validateSessionID`: `len(value) < 2 || value[0] != prefix` is one
/// rejection (too short to hold a prefix and a digit, or the wrong prefix),
/// and any non-digit rune after the prefix is the other.
fn validate_prefixed_digits(value: &str, prefix: char, noun: &str) -> Result<(), InvalidId> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes[0] != prefix as u8 {
        return Err(InvalidId {
            message: format!("tmux: {noun} {value:?} must have {prefix} prefix"),
        });
    }
    if !value[1..].chars().all(|c| c.is_ascii_digit()) {
        return Err(InvalidId {
            message: format!("tmux: {noun} {value:?} must contain decimal digits after {prefix}"),
        });
    }
    Ok(())
}

/// A stable tmux pane id such as `%0`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(String);

impl PaneId {
    /// Validates and wraps a tmux pane id.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidId`] when `value` does not have a `%` prefix
    /// followed by one or more decimal digits.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
        let value = value.into();
        validate_prefixed_digits(&value, '%', "pane ID")?;
        Ok(Self(value))
    }

    /// The id's wire text, such as `%0`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stable tmux window id such as `@1`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(String);

impl WindowId {
    /// Validates and wraps a tmux window id.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidId`] when `value` does not have a `@` prefix
    /// followed by one or more decimal digits.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
        let value = value.into();
        validate_prefixed_digits(&value, '@', "window ID")?;
        Ok(Self(value))
    }

    /// The id's wire text, such as `@1`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stable tmux session id such as `$2`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Validates and wraps a tmux session id.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidId`] when `value` does not have a `$` prefix
    /// followed by one or more decimal digits.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
        let value = value.into();
        validate_prefixed_digits(&value, '$', "session ID")?;
        Ok(Self(value))
    }

    /// The id's wire text, such as `$2`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Gives an id the one conversion the argv-building surface asks of a
/// target: into an [`OsString`], owned or borrowed.
///
/// [`crate::commands`]'s `-t` and `-s` methods take `impl Into<OsString>` so
/// a raw `mysession:1.2` spelling passes as readily as an id does. Without
/// these, an id read out of a previous answer would have to be turned back
/// into a string to be used in the next call — which is the parse-don't-
/// validate discipline above undone one line after it was applied.
///
/// Deliberately one-way: nothing here converts an `OsString` *into* an id,
/// because that direction is validation and validation is [`PaneId::new`]'s.
macro_rules! into_os_string {
    ($($type:ident),*) => {
        $(
            impl From<$type> for std::ffi::OsString {
                fn from(id: $type) -> Self {
                    Self::from(id.0)
                }
            }

            impl From<&$type> for std::ffi::OsString {
                fn from(id: &$type) -> Self {
                    Self::from(id.0.clone())
                }
            }
        )*
    };
}

into_os_string!(PaneId, WindowId, SessionId);

#[cfg(test)]
#[path = "ids_tests.rs"]
mod tests;
