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
