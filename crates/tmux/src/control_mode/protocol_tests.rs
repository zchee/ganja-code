use super::*;
use crate::control_mode::notification::NotificationKind;

fn marker(time: i64, command: i64, flags: i64) -> BlockMarker {
    BlockMarker { time, command, flags }
}

fn feed_all(parser: &mut Parser, lines: &[&str]) -> Result<Option<Event>, ProtocolError> {
    let mut last = None;
    for line in lines {
        last = parser.feed(line)?;
        if last.is_some() {
            return Ok(last);
        }
    }
    Ok(last)
}

#[test]
fn an_empty_response_has_no_lines() {
    let mut parser = Parser::default();
    let event = feed_all(&mut parser, &["%begin 1578920019 258 0", "%end 1578920019 258 0"])
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        Event::Response(Response {
            begin: marker(1578920019, 258, 0),
            end: marker(1578920019, 258, 0),
            lines: Vec::new(),
            error: false,
        })
    );
}

#[test]
fn a_multiline_response_keeps_every_payload_line_in_order() {
    let mut parser = Parser::default();
    let event = feed_all(
        &mut parser,
        &[
            "%begin 1578922740 269 1",
            "one",
            "%not-a-notification inside output",
            "two",
            "%end 1578922740 269 1",
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        event,
        Event::Response(Response {
            begin: marker(1578922740, 269, 1),
            end: marker(1578922740, 269, 1),
            lines: vec![
                "one".to_string(),
                "%not-a-notification inside output".to_string(),
                "two".to_string(),
            ],
            error: false,
        })
    );
}

#[test]
fn a_command_error_response_is_flagged() {
    let mut parser = Parser::default();
    let event = feed_all(
        &mut parser,
        &["%begin 1578923149 270 1", "parse error", "%error 1578923149 270 1"],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        event,
        Event::Response(Response {
            begin: marker(1578923149, 270, 1),
            end: marker(1578923149, 270, 1),
            lines: vec!["parse error".to_string()],
            error: true,
        })
    );
}

#[test]
fn a_fake_end_payload_does_not_terminate_the_block() {
    let mut parser = Parser::default();
    let event =
        feed_all(&mut parser, &["%begin 1 2 1", "%end payload", "%end 1 2 1"]).unwrap().unwrap();
    assert_eq!(
        event,
        Event::Response(Response {
            begin: marker(1, 2, 1),
            end: marker(1, 2, 1),
            lines: vec!["%end payload".to_string()],
            error: false,
        })
    );
}

#[test]
fn malformed_end_like_payload_does_not_terminate() {
    let mut parser = Parser::default();
    let event =
        feed_all(&mut parser, &["%begin 1 2 1", "%end a b c", "%end 1 2 1"]).unwrap().unwrap();
    assert_eq!(
        event,
        Event::Response(Response {
            begin: marker(1, 2, 1),
            end: marker(1, 2, 1),
            lines: vec!["%end a b c".to_string()],
            error: false,
        })
    );
}

#[test]
fn malformed_error_like_payload_does_not_terminate() {
    let mut parser = Parser::default();
    let event = feed_all(&mut parser, &["%begin 1 2 1", "%error one two three", "%end 1 2 1"])
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        Event::Response(Response {
            begin: marker(1, 2, 1),
            end: marker(1, 2, 1),
            lines: vec!["%error one two three".to_string()],
            error: false,
        })
    );
}

#[test]
fn mismatched_end_marker_remains_output() {
    let mut parser = Parser::default();
    let event =
        feed_all(&mut parser, &["%begin 1 2 1", "%end 1 3 1", "%end 1 2 1"]).unwrap().unwrap();
    assert_eq!(
        event,
        Event::Response(Response {
            begin: marker(1, 2, 1),
            end: marker(1, 2, 1),
            lines: vec!["%end 1 3 1".to_string()],
            error: false,
        })
    );
}

#[test]
fn mismatched_error_marker_remains_output() {
    let mut parser = Parser::default();
    let event =
        feed_all(&mut parser, &["%begin 1 2 1", "%error 1 3 1", "%end 1 2 1"]).unwrap().unwrap();
    assert_eq!(
        event,
        Event::Response(Response {
            begin: marker(1, 2, 1),
            end: marker(1, 2, 1),
            lines: vec!["%error 1 3 1".to_string()],
            error: false,
        })
    );
}

#[test]
fn a_malformed_begin_marker_is_a_protocol_error() {
    let mut parser = Parser::default();
    let err = parser.feed("%begin one 2 1").unwrap_err();
    assert!(err.message.contains("invalid marker time"));
}

#[test]
fn a_known_notification_kind_is_recognized() {
    let mut parser = Parser::default();
    let event = parser.feed("%output %1 hello").unwrap().unwrap();
    let Event::Notification(notification) = event else {
        panic!("expected a notification");
    };
    assert_eq!(notification.kind, NotificationKind::Output);
}

#[test]
fn an_unknown_notification_kind_still_parses() {
    let mut parser = Parser::default();
    let event = parser.feed("%beginning future notification").unwrap().unwrap();
    let Event::Notification(notification) = event else {
        panic!("expected a notification");
    };
    assert_eq!(notification.kind, NotificationKind::Other("%beginning".to_string()));
}

#[test]
fn a_stray_non_control_line_is_a_protocol_error() {
    let mut parser = Parser::default();
    let err = parser.feed("stray line").unwrap_err();
    assert!(err.message.contains("unexpected non-control"));
}

#[test]
fn eof_mid_block_reports_the_open_command_number() {
    let mut parser = Parser::default();
    parser.feed("%begin 1 2 1").unwrap();
    let err = parser.close().unwrap_err();
    assert!(err.line.is_none());
    assert!(err.message.contains("unexpected EOF"));
    assert!(err.message.contains('2'));
}

#[test]
fn close_after_a_complete_block_is_a_no_op() {
    let mut parser = Parser::default();
    feed_all(&mut parser, &["%begin 1 2 1", "%end 1 2 1"]).unwrap();
    assert!(parser.close().is_ok());
}
