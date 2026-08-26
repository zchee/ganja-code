use ganja_core::config::{NotificationEvent, TuiConfig};

use super::{Capture, Notifier, body};

fn tui(json: serde_json::Value) -> TuiConfig {
    serde_json::from_value(json).expect("the fixture is a tui table")
}

fn emitted(config: TuiConfig, event: NotificationEvent, summary: &str) -> Vec<u8> {
    let capture = Capture::default();
    let log = capture.log();
    let mut notifier = Notifier::over(config, Box::new(capture));

    notifier.notify(event, summary);

    log.lock().expect("the capture lock holds").clone()
}

#[test]
fn a_moment_the_config_did_not_ask_for_writes_nothing() {
    let cases = [
        (serde_json::json!({}), NotificationEvent::TurnComplete),
        (
            serde_json::json!({"notifications": false}),
            NotificationEvent::TurnComplete,
        ),
        (
            serde_json::json!({"notifications": ["turn-complete"]}),
            NotificationEvent::ApprovalRequested,
        ),
    ];

    for (config, event) in cases {
        assert!(
            emitted(tui(config.clone()), event, "quiet").is_empty(),
            "{config} should announce nothing for {event:?}"
        );
    }
}

#[test]
fn an_asked_for_moment_writes_one_osc9_sequence_carrying_the_summary() {
    let bytes = emitted(
        tui(serde_json::json!({"notifications": true})),
        NotificationEvent::TurnComplete,
        "turn complete",
    );

    assert_eq!(bytes, b"\x1b]9;turn complete\x07");
}

#[test]
fn the_bel_method_writes_the_bell_byte_and_no_body() {
    let bytes = emitted(
        tui(serde_json::json!({"notifications": true, "notification_method": "bel"})),
        NotificationEvent::ApprovalRequested,
        "a summary the bell cannot carry",
    );

    assert_eq!(bytes, b"\x07");
}

/// The body rides inside the escape, so bytes that would end it — and
/// every other control byte — must never survive into it.
#[test]
fn the_osc9_body_carries_no_control_bytes_and_only_the_first_line() {
    assert_eq!(body("first line\nsecond line"), "first line");
    assert_eq!(body("es\x1bcape\x07bell"), "escapebell");
    assert_eq!(body(""), "");

    let long = "x".repeat(500);
    assert_eq!(body(&long).chars().count(), super::MAX_BODY);
}
