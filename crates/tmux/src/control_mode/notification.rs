//! Spec: pandaemonium `pkg/tmux/notification.go`.
//!
//! [`parse`] turns one `%`-prefixed control-mode line into a [`Notification`]
//! carrying its raw, unclassified fields; the typed accessors on
//! [`Notification`] (`output`, `extended_output`, `subscription_changed`,
//! `exit`, `pause`, `continue_`, `message`) classify and validate those
//! fields into the shapes tmux actually documents for each kind.
//!
//! # Open kind, closed enum (divergence)
//!
//! Go's `NotificationKind` is an open `string` type: any `%token` is a
//! valid value, known or not. [`NotificationKind`] is a closed enum with the
//! same twenty known kinds, plus [`NotificationKind::ProtocolError`] — a
//! kind tmux itself never sends; [`crate::control_mode::Client`] synthesizes
//! it from `%protocol-error`, a symbolic name it manufactures internally
//! when a [`crate::error::ProtocolError`] surfaces — and a catch-all
//! [`NotificationKind::Other`] that preserves any unrecognized token
//! verbatim, so forward compatibility with a newer tmux is a value, not a
//! parse failure.
//!
//! # The `(T, bool, error)` triple becomes `Option<Result<T, E>>`
//!
//! Go's typed accessors return three values: a zero value, a `bool` for
//! "this notification's kind matches," and an `error` for "the matching
//! fields were malformed." Rust folds that into `Option<Result<T,
//! NotificationError>>`: `None` is Go's `(_, false, nil)` — kind mismatch —
//! and `Some(Err(_))`/`Some(Ok(_))` are Go's `(_, true, err)`/`(val, true,
//! nil)`. A caller who only cares "did I get one" still writes
//! `if let Some(result) = n.output() { ... }`, matching Go's `if ok { ...
//! }` shape one level up.
//!
//! # Extended-output age: a wider, not narrower, bound (divergence)
//!
//! Go parses the age field as `int64` milliseconds, then separately rejects
//! it if negative or if `ageMillis * time.Millisecond` would overflow
//! `time.Duration` — a single `int64` nanosecond count, so anything past
//! roughly 292 years overflows. Rust's [`std::time::Duration`] is a
//! `(u64 seconds, u32 nanos)` pair with far more headroom (its milliseconds
//! ceiling is `u64::MAX`, about 584 million years), so
//! `Duration::from_millis` cannot overflow on any value that fits in a
//! `u64`. This port parses the field directly as `u64` — which folds Go's
//! separate "not negative" check into the parse itself (a negative literal
//! fails a `u64` parse outright) — so the *only* remaining rejection is a
//! value too large for `u64` altogether. Go's own overflow test
//! (`TestExtendedOutputAgeOverflowRejected`) uses `1<<62` milliseconds,
//! which parses and constructs cleanly here (it is nowhere near `u64::MAX`);
//! this port's `extended_output_age_overflow_is_rejected` test instead uses
//! `u64::MAX as u128 + 1` to exercise the bound that actually exists in
//! Rust.
//!
//! # Session-scoped subscription sentinels (divergence)
//!
//! Real tmux sends `-` for the window, window index, and pane fields when a
//! subscription is scoped no narrower than a session. A live capture against
//! tmux next-3.8 produced `%subscription-changed live-test $0 - - - : x`.
//! Go's `validateWindowID` and `validatePaneID` in `notification.go` are
//! applied unconditionally to those fields too, but Go's own real-tmux suite
//! never exercises a session-scoped subscription. That is an upstream defect
//! this port fixes rather than reproduces: the three affected fields represent
//! tmux's not-applicable sentinel as `Option::None`.

use std::time::Duration;

use crate::{
    control_mode::output::DecodeError,
    error::ProtocolError,
    ids::{InvalidId, PaneId, SessionId, WindowId},
};

/// The leading `%name` token of a tmux control-mode notification.
///
/// See the module doc for how this differs from Go's open `string` type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NotificationKind {
    /// A `%output` pane-output notification.
    Output,
    /// A `%extended-output` flow-control output notification.
    ExtendedOutput,
    /// A `%subscription-changed` notification.
    SubscriptionChanged,
    /// A `%exit` control-client-exit notification.
    Exit,
    /// A `%pause` flow-control notification.
    Pause,
    /// A `%continue` flow-control notification.
    Continue,
    /// A `%message` tmux message notification.
    Message,
    /// A `%pane-mode-changed` notification.
    PaneModeChanged,
    /// A `%window-pane-changed` notification.
    WindowPaneChanged,
    /// A `%window-close` notification.
    WindowClose,
    /// A `%unlinked-window-close` notification.
    UnlinkedWindowClose,
    /// A `%window-add` notification.
    WindowAdd,
    /// A `%unlinked-window-add` notification.
    UnlinkedWindowAdd,
    /// A `%window-renamed` notification.
    WindowRenamed,
    /// A `%unlinked-window-renamed` notification.
    UnlinkedWindowRenamed,
    /// A `%session-changed` notification.
    SessionChanged,
    /// A `%client-session-changed` notification.
    ClientSessionChanged,
    /// A `%session-renamed` notification.
    SessionRenamed,
    /// A `%sessions-changed` notification.
    SessionsChanged,
    /// A `%session-window-changed` notification.
    SessionWindowChanged,
    /// A `%protocol-error` notification synthesized by
    /// [`crate::control_mode::Client`] from a parse failure; not a wire token
    /// tmux itself sends. See the module doc.
    ProtocolError,
    /// Any `%`-token this build does not recognize, preserved verbatim.
    Other(String),
}

impl NotificationKind {
    /// The notification's wire token, such as `%output`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Output => "%output",
            Self::ExtendedOutput => "%extended-output",
            Self::SubscriptionChanged => "%subscription-changed",
            Self::Exit => "%exit",
            Self::Pause => "%pause",
            Self::Continue => "%continue",
            Self::Message => "%message",
            Self::PaneModeChanged => "%pane-mode-changed",
            Self::WindowPaneChanged => "%window-pane-changed",
            Self::WindowClose => "%window-close",
            Self::UnlinkedWindowClose => "%unlinked-window-close",
            Self::WindowAdd => "%window-add",
            Self::UnlinkedWindowAdd => "%unlinked-window-add",
            Self::WindowRenamed => "%window-renamed",
            Self::UnlinkedWindowRenamed => "%unlinked-window-renamed",
            Self::SessionChanged => "%session-changed",
            Self::ClientSessionChanged => "%client-session-changed",
            Self::SessionRenamed => "%session-renamed",
            Self::SessionsChanged => "%sessions-changed",
            Self::SessionWindowChanged => "%session-window-changed",
            Self::ProtocolError => "%protocol-error",
            Self::Other(token) => token,
        }
    }

    fn from_token(token: &str) -> Self {
        match token {
            "%output" => Self::Output,
            "%extended-output" => Self::ExtendedOutput,
            "%subscription-changed" => Self::SubscriptionChanged,
            "%exit" => Self::Exit,
            "%pause" => Self::Pause,
            "%continue" => Self::Continue,
            "%message" => Self::Message,
            "%pane-mode-changed" => Self::PaneModeChanged,
            "%window-pane-changed" => Self::WindowPaneChanged,
            "%window-close" => Self::WindowClose,
            "%unlinked-window-close" => Self::UnlinkedWindowClose,
            "%window-add" => Self::WindowAdd,
            "%unlinked-window-add" => Self::UnlinkedWindowAdd,
            "%window-renamed" => Self::WindowRenamed,
            "%unlinked-window-renamed" => Self::UnlinkedWindowRenamed,
            "%session-changed" => Self::SessionChanged,
            "%client-session-changed" => Self::ClientSessionChanged,
            "%session-renamed" => Self::SessionRenamed,
            "%sessions-changed" => Self::SessionsChanged,
            "%session-window-changed" => Self::SessionWindowChanged,
            "%protocol-error" => Self::ProtocolError,
            other => Self::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for NotificationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One raw asynchronous tmux control-mode notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// The notification token, such as `%output`.
    pub kind: NotificationKind,
    /// The complete notification line, without its line ending.
    pub raw: String,
    /// Whitespace-split generic arguments, for forward-compatible callers
    /// that want a kind's fields without a typed accessor.
    pub args: Vec<String>,
}

/// Parses a `%`-prefixed control-mode notification line.
///
/// # Errors
///
/// Returns [`ProtocolError`] when `line` does not start with `%`, or when
/// its leading token is the bare `%` with nothing after it.
pub fn parse(line: &str) -> Result<Notification, ProtocolError> {
    if !line.starts_with('%') {
        return Err(ProtocolError {
            line: Some(line.to_string()),
            message: "notification must start with %".to_string(),
        });
    }
    let (kind_token, rest) = line.split_once(' ').unwrap_or((line, ""));
    if kind_token == "%" {
        return Err(ProtocolError {
            line: Some(line.to_string()),
            message: "notification kind is empty".to_string(),
        });
    }
    let args = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split_whitespace().map(str::to_string).collect()
    };
    Ok(Notification {
        kind: NotificationKind::from_token(kind_token),
        raw: line.to_string(),
        args,
    })
}

/// Why a notification's typed accessor could not classify its fields.
///
/// Ports the `error` half of Go's `(T, bool, error)` accessor triple; see
/// the module doc for the full mapping.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct NotificationError {
    message: String,
}

impl NotificationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<InvalidId> for NotificationError {
    fn from(source: InvalidId) -> Self {
        Self::new(source.to_string())
    }
}

impl Notification {
    /// Returns the typed form of a `%output` notification.
    ///
    /// `None` when `self` is not a `%output` notification; see the module
    /// doc for the full `Option<Result<_, _>>` mapping.
    #[must_use]
    pub fn output(&self) -> Option<Result<OutputNotification, NotificationError>> {
        if self.kind != NotificationKind::Output {
            return None;
        }
        Some(self.parse_output())
    }

    fn parse_output(&self) -> Result<OutputNotification, NotificationError> {
        let Some((_, rest)) = self.raw.split_once(' ') else {
            return Err(NotificationError::new("%output missing pane id"));
        };
        let rest = rest.trim_start_matches(' ');
        let (pane, value) = match rest.split_once(' ') {
            Some((pane, value)) => (pane, value),
            None => (rest, ""),
        };
        let pane = PaneId::new(pane)?;
        Ok(OutputNotification {
            pane,
            value: value.to_string(),
        })
    }

    /// Returns the typed form of a `%extended-output` notification.
    ///
    /// `None` when `self` is not a `%extended-output` notification; see the
    /// module doc for the full `Option<Result<_, _>>` mapping and the
    /// age-overflow divergence.
    #[must_use]
    pub fn extended_output(&self) -> Option<Result<ExtendedOutputNotification, NotificationError>> {
        if self.kind != NotificationKind::ExtendedOutput {
            return None;
        }
        Some(self.parse_extended_output())
    }

    fn parse_extended_output(&self) -> Result<ExtendedOutputNotification, NotificationError> {
        let Some((_, rest)) = self.raw.split_once(' ') else {
            return Err(NotificationError::new("%extended-output missing fields"));
        };
        let (fields, value) = split_fields_before_value(rest)?;
        if fields.len() < 2 {
            return Err(NotificationError::new(
                "%extended-output requires pane id and age",
            ));
        }
        let pane = PaneId::new(fields[0].as_str())?;
        let age_millis: u64 = fields[1].parse().map_err(|_| {
            NotificationError::new(format!("invalid %extended-output age {:?}", fields[1]))
        })?;
        Ok(ExtendedOutputNotification {
            pane,
            age: Duration::from_millis(age_millis),
            extension_fields: fields[2..].to_vec(),
            value,
        })
    }

    /// Returns the typed form of a `%subscription-changed` notification.
    ///
    /// `None` when `self` is not a `%subscription-changed` notification; see
    /// the module doc for the full `Option<Result<_, _>>` mapping.
    #[must_use]
    pub fn subscription_changed(
        &self,
    ) -> Option<Result<SubscriptionChangedNotification, NotificationError>> {
        if self.kind != NotificationKind::SubscriptionChanged {
            return None;
        }
        Some(self.parse_subscription_changed())
    }

    fn parse_subscription_changed(
        &self,
    ) -> Result<SubscriptionChangedNotification, NotificationError> {
        let Some((_, rest)) = self.raw.split_once(' ') else {
            return Err(NotificationError::new(
                "%subscription-changed missing fields",
            ));
        };
        let (fields, value) = split_fields_before_value(rest)?;
        if fields.len() < 5 {
            return Err(NotificationError::new(
                "%subscription-changed requires at least five fields before value",
            ));
        }
        let session = SessionId::new(fields[1].as_str())?;
        // See "Session-scoped subscription sentinels (divergence)" in the
        // module doc for why these three scope fields are optional.
        let window = dash_as_none(fields[2].as_str())
            .map(WindowId::new)
            .transpose()?;
        let window_index = dash_as_none(fields[3].as_str()).map(str::to_string);
        let pane = dash_as_none(fields[4].as_str())
            .map(PaneId::new)
            .transpose()?;
        Ok(SubscriptionChangedNotification {
            name: fields[0].clone(),
            session,
            window,
            window_index,
            pane,
            extension_fields: fields[5..].to_vec(),
            value,
        })
    }

    /// Returns the typed form of a `%exit` notification.
    ///
    /// `None` when `self` is not a `%exit` notification. Infallible by
    /// kind, unlike the other typed accessors: Go's `Exit` has no error
    /// return either, since any trailing text is a valid (if empty) reason.
    #[must_use]
    pub fn exit(&self) -> Option<ExitNotification> {
        if self.kind != NotificationKind::Exit {
            return None;
        }
        let reason = self
            .raw
            .split_once(' ')
            .map_or_else(String::new, |(_, reason)| reason.to_string());
        Some(ExitNotification { reason })
    }

    /// Returns the pane id from a `%pause` notification.
    ///
    /// `None` when `self` is not a `%pause` notification; see the module
    /// doc for the full `Option<Result<_, _>>` mapping.
    #[must_use]
    pub fn pause(&self) -> Option<Result<PaneId, NotificationError>> {
        self.pane_notification(&NotificationKind::Pause)
    }

    /// Returns the pane id from a `%continue` notification.
    ///
    /// `None` when `self` is not a `%continue` notification; see the module
    /// doc for the full `Option<Result<_, _>>` mapping. Named `continue_`
    /// because `continue` is a Rust keyword, where Go's `Continue` needed no
    /// such adjustment.
    #[must_use]
    pub fn continue_(&self) -> Option<Result<PaneId, NotificationError>> {
        self.pane_notification(&NotificationKind::Continue)
    }

    fn pane_notification(
        &self,
        kind: &NotificationKind,
    ) -> Option<Result<PaneId, NotificationError>> {
        if &self.kind != kind {
            return None;
        }
        if self.args.len() != 1 {
            return Some(Err(NotificationError::new(format!(
                "{kind} requires one pane id"
            ))));
        }
        Some(PaneId::new(self.args[0].as_str()).map_err(NotificationError::from))
    }

    /// Returns the tmux message from a `%message` notification.
    ///
    /// `None` when `self` is not a `%message` notification; the message
    /// text is empty when tmux sent no payload, mirroring Go's `Message`.
    #[must_use]
    pub fn message(&self) -> Option<String> {
        if self.kind != NotificationKind::Message {
            return None;
        }
        Some(
            self.raw
                .split_once(' ')
                .map_or_else(String::new, |(_, msg)| msg.to_string()),
        )
    }
}

/// Maps tmux's `-` not-applicable sentinel to `None`.
///
/// Real tmux reports `-` for a `%subscription-changed` field that does
/// not apply at the subscription's scope (window, window index, and
/// pane are all `-` for a session-scoped subscription); any other value
/// is returned unchanged for the caller to validate.
fn dash_as_none(field: &str) -> Option<&str> {
    if field == "-" { None } else { Some(field) }
}

/// Walks `input` for the `splitFieldsBeforeValue` grammar: whitespace-split
/// fields, terminated by a bare `:` field (rest of the line is the value)
/// or a field ending in `: ` (everything after that space is the value).
///
/// Ports Go's `splitFieldsBeforeValue` verbatim, including its three
/// termination shapes (`s == ":"`, `strings.CutPrefix(s, ": ")`, and a
/// field that equals exactly `":"` after a `strings.Cut` split) and its
/// `strings.TrimLeft(rest, " ")` — trimming only literal spaces between
/// fields, not all whitespace.
fn split_fields_before_value(input: &str) -> Result<(Vec<String>, String), NotificationError> {
    if input.is_empty() {
        return Err(NotificationError::new("missing fields"));
    }
    let mut fields = Vec::new();
    let mut s = input;
    loop {
        if s.is_empty() {
            return Err(NotificationError::new("missing : value separator"));
        }
        if s == ":" {
            return Ok((fields, String::new()));
        }
        if let Some(after) = s.strip_prefix(": ") {
            return Ok((fields, after.to_string()));
        }
        let Some((field, rest)) = s.split_once(' ') else {
            return Err(NotificationError::new("missing : value separator"));
        };
        if field == ":" {
            return Ok((fields, rest.to_string()));
        }
        fields.push(field.to_string());
        s = rest.trim_start_matches(' ');
    }
}

/// A typed `%output` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputNotification {
    /// The tmux pane id that produced the output.
    pub pane: PaneId,
    /// The tmux octal-escaped output value.
    pub value: String,
}

impl OutputNotification {
    /// Decodes the output value to terminal bytes.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] on a malformed octal escape.
    pub fn bytes(&self) -> Result<Vec<u8>, DecodeError> {
        crate::control_mode::output::decode_output_value(&self.value)
    }

    /// Returns the decoded output as UTF-8 text.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] on a malformed octal escape, or on output
    /// that does not decode to valid UTF-8.
    pub fn text(&self) -> Result<String, DecodeError> {
        crate::control_mode::output::decode_output_text(&self.value)
    }

    /// Returns the decoded output as UTF-8 text, replacing invalid
    /// sequences rather than failing.
    #[must_use]
    pub fn text_lossy(&self) -> String {
        crate::control_mode::output::decode_output_text_lossy(&self.value)
    }
}

/// A typed `%extended-output` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtendedOutputNotification {
    /// The tmux pane id that produced the output.
    pub pane: PaneId,
    /// How long tmux buffered the pane output before delivery.
    pub age: Duration,
    /// Currently reserved fields before the `:` separator.
    pub extension_fields: Vec<String>,
    /// The tmux octal-escaped output value.
    pub value: String,
}

impl ExtendedOutputNotification {
    /// Decodes the extended output value to terminal bytes.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] on a malformed octal escape.
    pub fn bytes(&self) -> Result<Vec<u8>, DecodeError> {
        crate::control_mode::output::decode_output_value(&self.value)
    }

    /// Returns the decoded extended output as UTF-8 text.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] on a malformed octal escape, or on output
    /// that does not decode to valid UTF-8.
    pub fn text(&self) -> Result<String, DecodeError> {
        crate::control_mode::output::decode_output_text(&self.value)
    }

    /// Returns the decoded extended output as UTF-8 text, replacing invalid
    /// sequences rather than failing.
    #[must_use]
    pub fn text_lossy(&self) -> String {
        crate::control_mode::output::decode_output_text_lossy(&self.value)
    }
}

/// A typed `%subscription-changed` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionChangedNotification {
    /// The caller-provided subscription name.
    pub name: String,
    /// The session id reported by tmux.
    pub session: SessionId,
    /// The window id reported by tmux, or `None` for the `-` sentinel in a
    /// session-scoped capture such as `%subscription-changed live-test $0 - - - : x`.
    pub window: Option<WindowId>,
    /// The window index reported by tmux, or `None` for the `-` sentinel in a
    /// session-scoped capture such as `%subscription-changed live-test $0 - - - : x`.
    pub window_index: Option<String>,
    /// The pane id reported by tmux, or `None` for the `-` sentinel in a
    /// session-scoped capture such as `%subscription-changed live-test $0 - - - : x`.
    pub pane: Option<PaneId>,
    /// Currently reserved fields before the `:` separator.
    pub extension_fields: Vec<String>,
    /// The expanded format value.
    pub value: String,
}

/// A typed `%exit` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitNotification {
    /// The optional tmux exit reason; empty when tmux gave none.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_the_kind_raw_line_and_whitespace_split_args() {
        let n = parse("%window-renamed @1 new name").unwrap();
        assert_eq!(n.kind, NotificationKind::WindowRenamed);
        assert_eq!(n.raw, "%window-renamed @1 new name");
        assert_eq!(n.args, vec!["@1", "new", "name"]);
    }

    #[test]
    fn parse_rejects_a_line_that_does_not_start_with_percent() {
        let err = parse("window-renamed @1").unwrap_err();
        assert!(err.message.contains("must start"));
    }

    #[test]
    fn parse_rejects_a_bare_percent_kind() {
        let err = parse("%").unwrap_err();
        assert!(err.message.contains("kind is empty"));
    }

    #[test]
    fn output_typed_accessor_reads_pane_and_value() {
        let n = parse(r"%output %1 hello\015\012").unwrap();
        let out = n.output().unwrap().unwrap();
        assert_eq!(out.pane.as_str(), "%1");
        assert_eq!(out.value, r"hello\015\012");
    }

    #[test]
    fn output_typed_accessor_is_tolerant_of_repeated_spaces() {
        let n = parse("%output  %1 hello").unwrap();
        let out = n.output().unwrap().unwrap();
        assert_eq!(out.pane.as_str(), "%1");
        assert_eq!(out.value, "hello");
    }

    #[test]
    fn output_typed_accessor_rejects_an_invalid_pane() {
        let n = parse("%output bad value").unwrap();
        let err = n.output().unwrap().unwrap_err();
        assert!(err.message.contains("pane ID"));
    }

    #[test]
    fn extended_output_typed_accessor_keeps_future_fields() {
        let n = parse(r"%extended-output %2 1234 future : data\012").unwrap();
        let out = n.extended_output().unwrap().unwrap();
        assert_eq!(out.pane.as_str(), "%2");
        assert_eq!(out.age, Duration::from_millis(1234));
        assert_eq!(out.extension_fields, vec!["future".to_string()]);
        assert_eq!(out.value, r"data\012");
    }

    #[test]
    fn extended_output_typed_accessor_rejects_a_missing_colon() {
        let n = parse("%extended-output %1 10 value").unwrap();
        let err = n.extended_output().unwrap().unwrap_err();
        assert!(err.message.contains("missing : value separator"));
    }

    #[test]
    fn extended_output_age_overflow_is_rejected() {
        // See the module doc: Go's own overflow probe (1<<62 ms) fits
        // comfortably in a u64 and constructs a Duration cleanly here, so
        // this exercises the bound that actually exists in Rust — one past
        // u64::MAX.
        let overflow = u128::from(u64::MAX) + 1;
        let n = parse(&format!("%extended-output %1 {overflow} : value")).unwrap();
        let err = n.extended_output().unwrap().unwrap_err();
        assert!(err.message.contains("invalid %extended-output age"));
    }

    #[test]
    fn subscription_changed_typed_accessor_keeps_future_fields() {
        let n = parse("%subscription-changed sub $1 @2 3 %4 future : value with spaces").unwrap();
        let sub = n.subscription_changed().unwrap().unwrap();
        assert_eq!(sub.name, "sub");
        assert_eq!(sub.session.as_str(), "$1");
        assert_eq!(sub.window, Some(WindowId::new("@2").unwrap()));
        assert_eq!(sub.window_index, Some("3".to_string()));
        assert_eq!(sub.pane, Some(PaneId::new("%4").unwrap()));
        assert_eq!(sub.extension_fields, vec!["future".to_string()]);
        assert_eq!(sub.value, "value with spaces");
    }

    // This is the exact line captured live against tmux next-3.8.
    #[test]
    fn a_session_scoped_subscription_reports_dashes_as_not_applicable() {
        let n = parse("%subscription-changed live-test $0 - - - : x").unwrap();
        let sub = n.subscription_changed().unwrap().unwrap();
        assert_eq!(sub.name, "live-test");
        assert_eq!(sub.session.as_str(), "$0");
        assert!(sub.window.is_none());
        assert!(sub.window_index.is_none());
        assert!(sub.pane.is_none());
        assert_eq!(sub.value, "x");
    }

    #[test]
    fn subscription_changed_typed_accessor_rejects_too_few_fields() {
        let n = parse("%subscription-changed name : value").unwrap();
        let err = n.subscription_changed().unwrap().unwrap_err();
        assert!(err.message.contains("requires at least five fields"));
    }

    #[test]
    fn subscription_changed_typed_accessor_rejects_an_invalid_pane() {
        let n = parse("%subscription-changed name $1 @2 3 bad : value").unwrap();
        let err = n.subscription_changed().unwrap().unwrap_err();
        assert!(err.message.contains("pane ID"));
    }

    #[test]
    fn subscription_changed_typed_accessor_rejects_an_invalid_session() {
        let n = parse("%subscription-changed name bad @2 3 %4 : value").unwrap();
        let err = n.subscription_changed().unwrap().unwrap_err();
        assert!(err.message.contains("session ID"));
    }

    #[test]
    fn subscription_changed_typed_accessor_rejects_an_invalid_window() {
        let n = parse("%subscription-changed name $1 bad 3 %4 : value").unwrap();
        let err = n.subscription_changed().unwrap().unwrap_err();
        assert!(err.message.contains("window ID"));
    }

    #[test]
    fn exit_typed_accessor_reads_the_reason() {
        let n = parse("%exit detached").unwrap();
        let exit = n.exit().unwrap();
        assert_eq!(exit.reason, "detached");
    }

    #[test]
    fn exit_typed_accessor_defaults_to_an_empty_reason() {
        let n = parse("%exit").unwrap();
        let exit = n.exit().unwrap();
        assert_eq!(exit.reason, "");
    }

    #[test]
    fn message_typed_accessor_reads_the_payload() {
        let n = parse("%message hello world").unwrap();
        assert_eq!(n.message().unwrap(), "hello world");
    }

    #[test]
    fn pause_typed_accessor_reads_the_pane_id() {
        let n = parse("%pause %1").unwrap();
        let pane = n.pause().unwrap().unwrap();
        assert_eq!(pane.as_str(), "%1");
    }

    #[test]
    fn continue_typed_accessor_rejects_a_malformed_pane_id() {
        let n = parse("%continue bad").unwrap();
        let err = n.continue_().unwrap().unwrap_err();
        assert!(err.message.contains("pane ID"));
    }

    #[test]
    fn a_typed_accessor_for_the_wrong_kind_returns_none() {
        let n = parse("%message hi").unwrap();
        assert!(n.output().is_none());
        assert!(n.pause().is_none());
        assert!(n.exit().is_none());
    }

    #[test]
    fn output_notification_text_decodes_or_rejects_invalid_utf8() {
        let n = parse(r"%output %1 hello\012").unwrap();
        let output = n.output().unwrap().unwrap();
        assert_eq!(output.text().unwrap(), "hello\n");

        let n = parse(r"%output %1 bad\377").unwrap();
        let output = n.output().unwrap().unwrap();
        let err = output.text().unwrap_err();
        assert!(err.to_string().contains("valid UTF-8"));
    }

    #[test]
    fn output_notification_text_lossy_keeps_the_partial_decode() {
        // A valid prefix followed by an incomplete escape must keep the
        // bytes decoded before the error rather than collapsing to "".
        let n = parse(r"%output %1 ok\01").unwrap();
        let output = n.output().unwrap().unwrap();
        assert_eq!(output.text_lossy(), "ok");
    }
}
