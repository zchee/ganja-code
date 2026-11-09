use super::frame;

#[test]
fn a_frame_is_one_event_line_one_data_line_and_a_blank() {
    let bytes = frame("message", "epoch-1", "{\"type\":\"x\"}");
    assert_eq!(&bytes[..], b"event: message\nid: epoch-1\ndata: {\"type\":\"x\"}\n\n");
}
