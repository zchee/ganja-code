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
    Scripted { client, peer_read, peer_write }
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
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buf).await.unwrap();
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
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
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
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
    let call = client.set_client_flags(&[ClientFlag::NO_OUTPUT, ClientFlag::WAIT_EXIT]);
    let answer =
        answer_command(&mut peer_read, &mut peer_write, "refresh-client -f no-output,wait-exit");

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
    let error = client.set_client_flags(&[ClientFlag::new("bad\nflag")]).await.unwrap_err();
    assert_invalid_command(error, "client flag");
}

#[tokio::test]
async fn set_pause_after_renders_whole_seconds() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
    let call = client.set_pause_after(std::time::Duration::from_secs(2));
    let answer = answer_command(&mut peer_read, &mut peer_write, "refresh-client -f pause-after=2");

    let (result, ()) = tokio::join!(call, answer);
    result.unwrap();
}

#[tokio::test]
async fn set_pause_after_refuses_zero_duration() {
    let Scripted { client, .. } = scripted_client();
    let error = client.set_pause_after(std::time::Duration::ZERO).await.unwrap_err();
    assert_invalid_command(error, "positive");
}

#[tokio::test]
async fn set_pause_after_refuses_fractional_seconds() {
    let Scripted { client, .. } = scripted_client();
    let error = client.set_pause_after(std::time::Duration::from_millis(1_500)).await.unwrap_err();
    assert_invalid_command(error, "whole number of seconds");
}

// Validated PaneId makes Go's PausePane error case a prefix-rejection
// test on the newtype itself, over in `crate::ids`.
#[tokio::test]
async fn pause_pane_renders_pause_state() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
    let pane = PaneId::new("%1").unwrap();
    let call = client.pause_pane(&pane);
    let answer = answer_command(&mut peer_read, &mut peer_write, "refresh-client -A '%1:pause'");

    let (result, ()) = tokio::join!(call, answer);
    result.unwrap();
}

#[tokio::test]
async fn continue_pane_renders_continue_state() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
    let pane = PaneId::new("%1").unwrap();
    let call = client.continue_pane(&pane);
    let answer = answer_command(&mut peer_read, &mut peer_write, "refresh-client -A '%1:continue'");

    let (result, ()) = tokio::join!(call, answer);
    result.unwrap();
}

#[tokio::test]
async fn disable_pane_output_renders_the_off_action() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
    let pane = PaneId::new("%1").unwrap();
    let call = client.disable_pane_output(&pane);
    let answer = answer_command(&mut peer_read, &mut peer_write, "refresh-client -A '%1:off'");

    let (result, ()) = tokio::join!(call, answer);
    result.unwrap();
}

#[tokio::test]
async fn enable_pane_output_renders_the_on_action() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
    let pane = PaneId::new("%1").unwrap();
    let call = client.enable_pane_output(&pane);
    let answer = answer_command(&mut peer_read, &mut peer_write, "refresh-client -A '%1:on'");

    let (result, ()) = tokio::join!(call, answer);
    result.unwrap();
}

#[tokio::test]
async fn subscribe_format_renders_target_and_format() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
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
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client();
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
    let error = client.subscribe_format("sub", &target, "#{pane_id}").await.unwrap_err();
    assert_invalid_command(error, "target");
}

#[tokio::test]
async fn subscribe_format_refuses_a_newline_in_the_format() {
    let Scripted { client, .. } = scripted_client();
    let error =
        client.subscribe_format("sub", &SubscriptionTarget::ALL_PANES, "bad\n").await.unwrap_err();
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
