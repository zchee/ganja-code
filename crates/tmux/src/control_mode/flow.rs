//! Spec: pandaemonium `pkg/tmux/flow.go` (the four command constants, the
//! `ClientFlag`/`SubscriptionTarget` value types, and the `refresh-client`
//! helpers on `Client`).
//!
//! # Wave split
//!
//! This module's public surface landed across two waves of the port. **W1**
//! ported the pure types and validators that need no running `Client`: the
//! command constants, [`ClientFlag`], [`SubscriptionTarget`], and the two
//! `pub(crate)` fragment validators — alongside the id newtypes that now
//! live in [`crate::ids`], since a pane id is vocabulary wider than control
//! mode. **W3** added the `refresh-client` helper methods on `Client`:
//! `refresh_client_size`, `set_client_flags`, `set_pause_after`, `pause_pane`,
//! `continue_pane`, `disable_pane_output`, `enable_pane_output`,
//! `subscribe_format`, and `unsubscribe_format`. They render the W1 types
//! into commands and read back a [`Response`].
//!
//! # Quoting divergence
//!
//! Every validator message below substitutes Rust's `{:?}` `Debug` quoting
//! for Go's `%q` verb, for the reason [`crate::ids`] documents once.

use std::borrow::Cow;

use crate::{
    control_mode::{
        client::Client,
        commandline::{Arg, Command, RenderError},
        protocol::Response,
    },
    error::Error,
    ids::PaneId,
};

/// The tmux `detach-client` command; its alias is `detach`.
pub const DETACH_CLIENT: Command = Command::from_static("detach-client");

/// The tmux `display-message` command; its alias is `display`.
pub const DISPLAY_MESSAGE: Command = Command::from_static("display-message");

/// The tmux `list-panes` command; its alias is `lsp`.
pub const LIST_PANES: Command = Command::from_static("list-panes");

/// The tmux `refresh-client` command; its alias is `refresh`.
pub const REFRESH_CLIENT: Command = Command::from_static("refresh-client");

/// A `refresh-client -f` flag value.
///
/// Ports Go's `ClientFlag` open string type. [`ClientFlag::NO_OUTPUT`] and
/// [`ClientFlag::WAIT_EXIT`] are the two flags tmux documents; the type stays
/// open (rather than a closed enum) because `refresh-client -f
/// pause-after=N` composes a flag value at runtime — no closed enum could
/// represent it — exactly mirroring why Go left `ClientFlag` an open string.
/// The `Cow<'static, str>` representation and the `const fn` constructor
/// mirror [`crate::control_mode::commandline::Command`]'s own shape for the
/// same reason: both need `const` well-known values alongside
/// runtime-composed ones.
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

    /// Builds a target from a runtime-composed value, such as a specific
    /// pane or window id.
    pub fn new(value: impl Into<Cow<'static, str>>) -> Self {
        Self(value.into())
    }

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
/// The [`Client::set_client_flags`] helper is its production caller;
/// [`validate_subscription_name`] shares the same fragment rules.
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
pub(crate) fn validate_subscription_name(name: &str) -> Result<(), String> {
    validate_refresh_fragment(name, "subscription name")?;
    if name.contains(':') {
        return Err(format!(
            "tmux: subscription name {name:?} must not contain colon"
        ));
    }
    Ok(())
}

impl Client {
    /// Resizes the control-mode client to `width` by `height` cells.
    ///
    /// Unlike Go's signed dimensions, `u32` makes negative values
    /// unrepresentable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCommand`] when either dimension is zero, or
    /// any command rendering, transport, or tmux response failure.
    pub async fn refresh_client_size(&self, width: u32, height: u32) -> Result<Response, Error> {
        if width == 0 || height == 0 {
            return Err(RenderError::new("tmux: client size must be positive").into());
        }
        self.exec(
            REFRESH_CLIENT,
            [Arg::raw("-C"), Arg::string(format!("{width}x{height}"))],
        )
        .await
    }

    /// Replaces the control-mode client's flags with `flags`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCommand`] when `flags` is empty or any flag
    /// is blank or contains a newline, or any command rendering, transport,
    /// or tmux response failure.
    pub async fn set_client_flags(&self, flags: &[ClientFlag]) -> Result<Response, Error> {
        if flags.is_empty() {
            return Err(RenderError::new("tmux: at least one client flag is required").into());
        }
        for flag in flags {
            validate_refresh_fragment(flag.as_str(), "client flag").map_err(RenderError::new)?;
        }
        let values = flags
            .iter()
            .map(ClientFlag::as_str)
            .collect::<Vec<_>>()
            .join(",");
        self.exec(REFRESH_CLIENT, [Arg::raw("-f"), Arg::string(values)])
            .await
    }

    /// Pauses client output after `d` whole seconds of backpressure.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCommand`] when `d` is zero or not a whole
    /// number of seconds, or any failure from [`Client::set_client_flags`].
    pub async fn set_pause_after(&self, d: std::time::Duration) -> Result<Response, Error> {
        if d.is_zero() {
            return Err(RenderError::new("tmux: pause-after duration must be positive").into());
        }
        if d.subsec_nanos() != 0 {
            return Err(RenderError::new(format!(
                "tmux: pause-after duration {d:?} must be a whole number of seconds"
            ))
            .into());
        }
        let seconds = d.as_secs();
        let flag = ClientFlag::new(format!("pause-after={seconds}"));
        self.set_client_flags(std::slice::from_ref(&flag)).await
    }

    /// Pauses output from `pane`.
    ///
    /// # Errors
    ///
    /// Returns any command rendering, transport, or tmux response failure.
    pub async fn pause_pane(&self, pane: &PaneId) -> Result<Response, Error> {
        self.pane_flow(pane, "pause").await
    }

    /// Resumes output from `pane`.
    ///
    /// # Errors
    ///
    /// Returns any command rendering, transport, or tmux response failure.
    pub async fn continue_pane(&self, pane: &PaneId) -> Result<Response, Error> {
        self.pane_flow(pane, "continue").await
    }

    /// Disables output from `pane`.
    ///
    /// # Errors
    ///
    /// Returns any command rendering, transport, or tmux response failure.
    pub async fn disable_pane_output(&self, pane: &PaneId) -> Result<Response, Error> {
        self.pane_flow(pane, "off").await
    }

    /// Enables output from `pane`.
    ///
    /// # Errors
    ///
    /// Returns any command rendering, transport, or tmux response failure.
    pub async fn enable_pane_output(&self, pane: &PaneId) -> Result<Response, Error> {
        self.pane_flow(pane, "on").await
    }

    /// Subscribes `name` to `format` updates for `target`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCommand`] when the name, target, or format
    /// cannot be represented safely, or any command rendering, transport, or
    /// tmux response failure.
    pub async fn subscribe_format(
        &self,
        name: &str,
        target: &SubscriptionTarget,
        format: &str,
    ) -> Result<Response, Error> {
        validate_subscription_name(name).map_err(RenderError::new)?;
        if target.as_str().contains(['\r', '\n', ':']) {
            return Err(RenderError::new(format!(
                "tmux: subscription target {:?} must not contain newline or colon",
                target.as_str()
            ))
            .into());
        }
        if format.contains(['\r', '\n']) {
            return Err(
                RenderError::new("tmux: subscription format must not contain a newline").into(),
            );
        }
        let subscription = format!("{name}:{}:{format}", target.as_str());
        self.exec(REFRESH_CLIENT, [Arg::raw("-B"), Arg::string(subscription)])
            .await
    }

    /// Removes the format subscription named `name`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCommand`] when `name` is blank or contains a
    /// newline or colon, or any command rendering, transport, or tmux response
    /// failure.
    pub async fn unsubscribe_format(&self, name: &str) -> Result<Response, Error> {
        validate_subscription_name(name).map_err(RenderError::new)?;
        self.exec(REFRESH_CLIENT, [Arg::raw("-B"), Arg::string(name)])
            .await
    }

    /// Applies a flow-control state to an already validated pane id.
    ///
    /// # Validated `PaneId` divergence
    ///
    /// Go's `paneFlow` validates its unchecked string on every call. Here,
    /// `&PaneId` proves construction already validated the id, so that
    /// per-call check vanishes into the type.
    ///
    /// tmux misparses a bare `%<digits>:word` token, although a bare
    /// `%<digits>` pane id is valid; single-quoting the compound argument
    /// avoids that lexer ambiguity. The validated pane id and fixed states
    /// make this raw quoting safe.
    async fn pane_flow(&self, pane: &PaneId, state: &str) -> Result<Response, Error> {
        self.exec(
            REFRESH_CLIENT,
            [Arg::raw("-A"), Arg::raw(format!("'{pane}:{state}'"))],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_mode::options::Options;

    type PeerRead = tokio::io::ReadHalf<tokio::io::DuplexStream>;
    type PeerWrite = tokio::io::WriteHalf<tokio::io::DuplexStream>;

    struct Scripted {
        client: crate::control_mode::client::Client,
        peer_read: PeerRead,
        peer_write: PeerWrite,
    }

    fn scripted_client() -> Scripted {
        let options = Options::new().with_session_name("test");
        let (client_end, peer) = tokio::io::duplex(8192);
        let client = crate::control_mode::client::Client::from_duplex(&options, client_end, None);
        let (peer_read, peer_write) = tokio::io::split(peer);
        Scripted {
            client,
            peer_read,
            peer_write,
        }
    }

    async fn peer_send(write: &mut PeerWrite, line: &str) {
        use tokio::io::AsyncWriteExt;

        write.write_all(line.as_bytes()).await.unwrap();
        write.write_all(b"\n").await.unwrap();
        write.flush().await.unwrap();
    }

    async fn peer_recv_written(read: &mut PeerRead) -> String {
        let mut reader = tokio::io::BufReader::new(read);
        let mut buf = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buf)
            .await
            .unwrap();
        crate::control_mode::client::trim_line_ending(&buf).to_owned()
    }

    async fn answer_command(read: &mut PeerRead, write: &mut PeerWrite, expected: &str) {
        let written = peer_recv_written(read).await;
        assert_eq!(written, expected);
        peer_send(write, "%begin 1 1 1").await;
        peer_send(write, "%end 1 1 1").await;
    }

    fn assert_invalid_command(error: Error, expected_message_fragment: &str) {
        assert!(
            matches!(&error, Error::InvalidCommand(_)),
            "expected Error::InvalidCommand, got {error:?}"
        );
        assert!(
            error.to_string().contains(expected_message_fragment),
            "expected {error:?} to contain {expected_message_fragment:?}"
        );
    }

    #[tokio::test]
    async fn refresh_client_size_renders_dimensions_as_wxh() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let call = client.refresh_client_size(120, 40);
        let answer = answer_command(&mut peer_read, &mut peer_write, "refresh-client -C 120x40");

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn refresh_client_size_refuses_zero_dimensions() {
        let Scripted { client, .. } = scripted_client();
        let error = client.refresh_client_size(0, 40).await.unwrap_err();
        assert_invalid_command(error, "positive");
    }

    #[tokio::test]
    async fn set_client_flags_renders_comma_separated_values() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let call = client.set_client_flags(&[ClientFlag::NO_OUTPUT, ClientFlag::WAIT_EXIT]);
        let answer = answer_command(
            &mut peer_read,
            &mut peer_write,
            "refresh-client -f no-output,wait-exit",
        );

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn set_client_flags_requires_at_least_one_flag() {
        let Scripted { client, .. } = scripted_client();
        let error = client.set_client_flags(&[]).await.unwrap_err();
        assert_invalid_command(error, "at least one");
    }

    #[tokio::test]
    async fn set_client_flags_surfaces_invalid_fragments_as_invalid_commands() {
        let Scripted { client, .. } = scripted_client();
        let error = client
            .set_client_flags(&[ClientFlag::new("bad\nflag")])
            .await
            .unwrap_err();
        assert_invalid_command(error, "client flag");
    }

    #[tokio::test]
    async fn set_pause_after_renders_whole_seconds() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let call = client.set_pause_after(std::time::Duration::from_secs(2));
        let answer = answer_command(
            &mut peer_read,
            &mut peer_write,
            "refresh-client -f pause-after=2",
        );

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn set_pause_after_refuses_zero_duration() {
        let Scripted { client, .. } = scripted_client();
        let error = client
            .set_pause_after(std::time::Duration::ZERO)
            .await
            .unwrap_err();
        assert_invalid_command(error, "positive");
    }

    #[tokio::test]
    async fn set_pause_after_refuses_fractional_seconds() {
        let Scripted { client, .. } = scripted_client();
        let error = client
            .set_pause_after(std::time::Duration::from_millis(1_500))
            .await
            .unwrap_err();
        assert_invalid_command(error, "whole number of seconds");
    }

    // Validated PaneId makes Go's PausePane error case a prefix-rejection
    // test on the newtype itself, over in `crate::ids`.
    #[tokio::test]
    async fn pause_pane_renders_pause_state() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let pane = PaneId::new("%1").unwrap();
        let call = client.pause_pane(&pane);
        let answer = answer_command(
            &mut peer_read,
            &mut peer_write,
            "refresh-client -A '%1:pause'",
        );

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn continue_pane_renders_continue_state() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let pane = PaneId::new("%1").unwrap();
        let call = client.continue_pane(&pane);
        let answer = answer_command(
            &mut peer_read,
            &mut peer_write,
            "refresh-client -A '%1:continue'",
        );

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn disable_pane_output_renders_the_off_action() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let pane = PaneId::new("%1").unwrap();
        let call = client.disable_pane_output(&pane);
        let answer = answer_command(
            &mut peer_read,
            &mut peer_write,
            "refresh-client -A '%1:off'",
        );

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn enable_pane_output_renders_the_on_action() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let pane = PaneId::new("%1").unwrap();
        let call = client.enable_pane_output(&pane);
        let answer = answer_command(&mut peer_read, &mut peer_write, "refresh-client -A '%1:on'");

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn subscribe_format_renders_target_and_format() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let call = client.subscribe_format(
            "sub",
            &SubscriptionTarget::ALL_PANES,
            "#{pane_id}:#{pane_current_command}",
        );
        let answer = answer_command(
            &mut peer_read,
            &mut peer_write,
            "refresh-client -B 'sub:%*:#{pane_id}:#{pane_current_command}'",
        );

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn unsubscribe_format_renders_the_subscription_name() {
        let Scripted {
            client,
            mut peer_read,
            mut peer_write,
        } = scripted_client();
        let call = client.unsubscribe_format("sub");
        let answer = answer_command(&mut peer_read, &mut peer_write, "refresh-client -B sub");

        let (result, ()) = tokio::join!(call, answer);
        result.unwrap();
    }

    #[tokio::test]
    async fn subscribe_format_requires_a_subscription_name() {
        let Scripted { client, .. } = scripted_client();
        let error = client
            .subscribe_format("", &SubscriptionTarget::ALL_PANES, "#{pane_id}")
            .await
            .unwrap_err();
        assert_invalid_command(error, "subscription name");
    }

    #[tokio::test]
    async fn subscribe_format_refuses_a_colon_in_the_target() {
        let Scripted { client, .. } = scripted_client();
        let target = SubscriptionTarget::new("bad:target");
        let error = client
            .subscribe_format("sub", &target, "#{pane_id}")
            .await
            .unwrap_err();
        assert_invalid_command(error, "target");
    }

    #[tokio::test]
    async fn subscribe_format_refuses_a_newline_in_the_format() {
        let Scripted { client, .. } = scripted_client();
        let error = client
            .subscribe_format("sub", &SubscriptionTarget::ALL_PANES, "bad\n")
            .await
            .unwrap_err();
        assert_invalid_command(error, "format");
    }

    #[tokio::test]
    async fn unsubscribe_format_requires_a_subscription_name() {
        let Scripted { client, .. } = scripted_client();
        let error = client.unsubscribe_format("").await.unwrap_err();
        assert_invalid_command(error, "subscription name");
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

    #[test]
    fn subscription_target_new_composes_a_runtime_value() {
        let pane = 7;
        let target = SubscriptionTarget::new(format!("%{pane}"));
        assert_eq!(target.as_str(), "%7");
    }

    // The four command constants (DETACH_CLIENT, DISPLAY_MESSAGE, LIST_PANES,
    // REFRESH_CLIENT) are not independently unit-tested here: flow_test.go
    // itself never tests the bare Go constants either, only the Client
    // helpers that render them (W3), and this wave's contract with Lane A
    // guarantees only `Command::from_static` as a `const fn` — not a
    // specific accessor to assert wire text through. The
    // `crate::control_mode::commandline::Command` import above already makes a
    // mismatch with Lane A's shape a compile error.
}
