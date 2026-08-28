use super::{FRAME_HEAD, envelope, head};

#[test]
fn a_frames_head_is_cut_on_a_character_boundary() {
    let wide: String = "あ".repeat(FRAME_HEAD * 2);
    let cut = head(&wide);

    assert_eq!(cut.chars().count(), FRAME_HEAD);
    assert!(wide.starts_with(cut));
    // A frame shorter than the cap is not touched at all.
    assert_eq!(head("{\"type\":\"idle_notification\"}"), "{\"type\":\"idle_notification\"}");
}

#[test]
fn a_delivered_message_says_who_wrote_it() {
    assert_eq!(
        envelope("team-lead", "have a look at the parser"),
        "A message from team-lead:\nhave a look at the parser"
    );
}
