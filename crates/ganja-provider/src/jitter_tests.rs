use super::draw;

/// The clock field both ladders used to read is under a billion, so a draw
/// that ever sets a bit above the low thirty-two is a draw no clock could
/// have produced. Sixty-four of them make the alternative reading — that
/// every draw happened to land low — a one-in-2^2048 event.
#[test]
fn a_draw_reaches_past_the_range_a_clocks_nanoseconds_could_fill() {
    assert!(
        (0..64).any(|_| draw() >> 32 != 0),
        "the draws all fit in a nanosecond field, which is what they replaced"
    );
}
