//! Spec: pandaemonium `pkg/tmux/flow.go` (the four command constants, the
//! `ClientFlag`/`SubscriptionTarget` value types) and `pkg/tmux/notification.go`
//! (`validateSessionID`/`validateWindowID`, consolidated here beside
//! `validatePaneID` so all three id validators live with the newtypes they
//! guard).
//!
//! # Wave split
//!
//! This module's public surface splits across two waves of the port. **W1**
//! (this wave) ports the pure types and validators that need no running
//! `Client`: the command constants, the [`PaneId`]/[`WindowId`]/[`SessionId`]
//! newtypes, [`ClientFlag`], [`SubscriptionTarget`], and the two
//! `pub(crate)` fragment validators. **W3** ports the `refresh-client` helper
//! *methods* on `Client` (`refresh_client_size`, `set_client_flags`,
//! `set_pause_after`, `pause_pane`, `continue_pane`, `disable_pane_output`,
//! `enable_pane_output`, `subscribe_format`, `unsubscribe_format`) that
//! render these types into commands and read back a `Response`. Nothing here
//! depends on the client; the client will depend on this.
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
//! [`crate::notification::Notification`]'s raw `%`-fields stay plain
//! `String`/`Vec<String>` until a typed accessor classifies and validates
//! them into one of these newtypes exactly once, rather than Go's
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

use std::borrow::Cow;

use crate::commandline::Command;

/// The tmux `detach-client` command; its alias is `detach`.
pub const DETACH_CLIENT: Command = Command::from_static("detach-client");

/// The tmux `display-message` command; its alias is `display`.
pub const DISPLAY_MESSAGE: Command = Command::from_static("display-message");

/// The tmux `list-panes` command; its alias is `lsp`.
pub const LIST_PANES: Command = Command::from_static("list-panes");

/// The tmux `refresh-client` command; its alias is `refresh`.
pub const REFRESH_CLIENT: Command = Command::from_static("refresh-client");

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

/// A `refresh-client -f` flag value.
///
/// Ports Go's `ClientFlag` open string type. [`ClientFlag::NO_OUTPUT`] and
/// [`ClientFlag::WAIT_EXIT`] are the two flags tmux documents; the type stays
/// open (rather than a closed enum) because `refresh-client -f
/// pause-after=N` composes a flag value at runtime — no closed enum could
/// represent it — exactly mirroring why Go left `ClientFlag` an open string.
/// The `Cow<'static, str>` representation and the `const fn` constructor
/// mirror [`crate::commandline::Command`]'s own shape for the same reason:
/// both need `const` well-known values alongside runtime-composed ones.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClientFlag(Cow<'static, str>);

impl ClientFlag {
    /// Disables `%output` notifications for the client.
    pub const NO_OUTPUT: ClientFlag = ClientFlag(Cow::Borrowed("no-output"));

    /// Asks tmux to wait for an empty line after `%exit`.
    pub const WAIT_EXIT: ClientFlag = ClientFlag(Cow::Borrowed("wait-exit"));

    /// Builds a flag from a runtime-composed value, such as
    /// `pause-after=2`.
    pub fn new(value: impl Into<Cow<'static, str>>) -> Self {
        Self(value.into())
    }

    /// The flag's wire text, such as `no-output`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ClientFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The target part of a `refresh-client -B` format subscription.
///
/// Ports Go's `SubscriptionTarget` open string type; see [`ClientFlag`] for
/// why this crate represents an open string type as `Cow<'static, str>`
/// rather than a closed enum.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionTarget(Cow<'static, str>);

impl SubscriptionTarget {
    /// Subscribes to the attached session.
    pub const ATTACHED_SESSION: SubscriptionTarget = SubscriptionTarget(Cow::Borrowed(""));

    /// Subscribes to all panes in the attached session.
    pub const ALL_PANES: SubscriptionTarget = SubscriptionTarget(Cow::Borrowed("%*"));

    /// Subscribes to all windows in the attached session.
    pub const ALL_WINDOWS: SubscriptionTarget = SubscriptionTarget(Cow::Borrowed("@*"));

    /// The target's wire text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SubscriptionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validates a `refresh-client` argument fragment.
///
/// Ports Go's `validateRefreshFragment`: `name` is refused if it is empty or
/// only whitespace, or if it contains a carriage return or newline — either
/// would let a caller smuggle a second tmux command line into what tmux
/// reads as one `refresh-client` argument.
///
/// The `refresh-client` helper *methods* that call this land in W3 (see the
/// module doc); it is ported now because [`validate_subscription_name`]
/// already needs it, and that one is exercised directly by this wave's own
/// tests.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed by the W3 Client refresh-client helpers; this \
                  wave's own #[cfg(test)] module already calls it (through \
                  validate_subscription_name), so the expectation only \
                  applies to the non-test build"
    )
)]
pub(crate) fn validate_refresh_fragment(value: &str, name: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("tmux: {name} must not be empty"));
    }
    if value.contains(['\r', '\n']) {
        return Err(format!("tmux: {name} must not contain a newline"));
    }
    Ok(())
}

/// Validates a `refresh-client -B` subscription name.
///
/// Ports Go's `validateSubscriptionName`: a valid [`validate_refresh_fragment`]
/// that additionally may not contain a colon, since the wire encodes a
/// subscription as `name:target:format`.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed by the W3 Client subscribe_format/unsubscribe_format \
                  helpers; this wave's own #[cfg(test)] module already calls \
                  it, so the expectation only applies to the non-test build"
    )
)]
pub(crate) fn validate_subscription_name(name: &str) -> Result<(), String> {
    validate_refresh_fragment(name, "subscription name")?;
    if name.contains(':') {
        return Err(format!(
            "tmux: subscription name {name:?} must not contain colon"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC-4 waiver: every case in flow_test.go (`TestRefreshClientHelpersRenderCommands`,
    // `TestRefreshClientHelperValidation`) drives a scripted `Client`, which
    // does not exist until W3. The validation *rules* those cases exercise
    // are split here into what W1 can test directly (the id newtypes and the
    // two `pub(crate)` fragment validators) and what genuinely cannot exist
    // before `Client` does:
    //   - "client size positive", "flags required", "pause after positive",
    //     "pause after sub-second", "subscription target no colon",
    //     "subscription format no newline" all validate *inline* inside a
    //     `Client` method body in Go (`RefreshClientSize`, `SetClientFlags`,
    //     `SetPauseAfter`, `SubscribeFormat`) rather than through a shared
    //     validator this wave exposes; they move to W3 verbatim.
    //   - "pane id prefix" is ported here as `a_pane_id_without_the_percent_prefix_is_refused`.
    //   - "subscription name required" is ported here as
    //     `an_empty_subscription_name_is_refused`.
    //   - `TestRefreshClientHelpersRenderCommands`'s command-rendering
    //     assertions all require a `Client` to exec through; they move to W3.

    #[test]
    fn a_pane_id_without_the_percent_prefix_is_refused() {
        let err = PaneId::new("1").unwrap_err();
        assert!(err.to_string().contains("pane ID"));
    }

    #[test]
    fn a_pane_id_with_only_the_percent_prefix_is_refused() {
        let err = PaneId::new("%").unwrap_err();
        assert!(err.to_string().contains("pane ID"));
    }

    #[test]
    fn a_pane_id_with_a_non_digit_after_the_prefix_is_refused() {
        let err = PaneId::new("%a").unwrap_err();
        assert!(err.to_string().contains("decimal digits"));
    }

    #[test]
    fn a_well_formed_pane_id_round_trips_through_as_str() {
        let pane = PaneId::new("%12").unwrap();
        assert_eq!(pane.as_str(), "%12");
        assert_eq!(pane.to_string(), "%12");
    }

    #[test]
    fn a_window_id_without_the_at_prefix_is_refused() {
        let err = WindowId::new("1").unwrap_err();
        assert!(err.to_string().contains("window ID"));
    }

    #[test]
    fn a_well_formed_window_id_round_trips_through_as_str() {
        let window = WindowId::new("@3").unwrap();
        assert_eq!(window.as_str(), "@3");
    }

    #[test]
    fn a_session_id_without_the_dollar_prefix_is_refused() {
        let err = SessionId::new("1").unwrap_err();
        assert!(err.to_string().contains("session ID"));
    }

    #[test]
    fn a_well_formed_session_id_round_trips_through_as_str() {
        let session = SessionId::new("$4").unwrap();
        assert_eq!(session.as_str(), "$4");
    }

    #[test]
    fn an_empty_subscription_name_is_refused() {
        let err = validate_subscription_name("").unwrap_err();
        assert!(err.contains("subscription name"));
    }

    #[test]
    fn a_subscription_name_containing_a_colon_is_refused() {
        let err = validate_subscription_name("bad:name").unwrap_err();
        assert!(err.contains("colon"));
    }

    #[test]
    fn a_refresh_fragment_containing_a_newline_is_refused() {
        let err = validate_refresh_fragment("bad\nvalue", "client flag").unwrap_err();
        assert!(err.contains("newline"));
    }

    #[test]
    fn client_flag_well_known_constants_render_their_wire_text() {
        assert_eq!(ClientFlag::NO_OUTPUT.as_str(), "no-output");
        assert_eq!(ClientFlag::WAIT_EXIT.as_str(), "wait-exit");
    }

    #[test]
    fn client_flag_new_composes_a_runtime_value() {
        let flag = ClientFlag::new(format!("pause-after={}", 2));
        assert_eq!(flag.as_str(), "pause-after=2");
    }

    #[test]
    fn subscription_target_well_known_constants_render_their_wire_text() {
        assert_eq!(SubscriptionTarget::ATTACHED_SESSION.as_str(), "");
        assert_eq!(SubscriptionTarget::ALL_PANES.as_str(), "%*");
        assert_eq!(SubscriptionTarget::ALL_WINDOWS.as_str(), "@*");
    }

    // The four command constants (DETACH_CLIENT, DISPLAY_MESSAGE, LIST_PANES,
    // REFRESH_CLIENT) are not independently unit-tested here: flow_test.go
    // itself never tests the bare Go constants either, only the Client
    // helpers that render them (W3), and this wave's contract with Lane A
    // guarantees only `Command::from_static` as a `const fn` — not a
    // specific accessor to assert wire text through. `use crate::commandline::Command;`
    // above already makes a mismatch with Lane A's shape a compile error.
}
