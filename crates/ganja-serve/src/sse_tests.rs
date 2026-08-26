use super::frame;

#[test]
fn a_frame_is_one_event_line_one_data_line_and_a_blank() {
    let bytes = frame("message", "{\"type\":\"x\"}");
    assert_eq!(&bytes[..], b"event: message\ndata: {\"type\":\"x\"}\n\n");
}
