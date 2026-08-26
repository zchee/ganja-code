use ratatui::{buffer::Buffer, layout::Rect};

use super::{HINT, Queue};
use crate::theme::Theme;

/// Renders the strip over an editor anchored at the bottom of a `height`
/// row screen, and hands back what it wrote.
fn rendered(queue: &Queue, width: u16, height: u16) -> String {
    let anchor = Rect::new(0, height - 3, width, 3);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    queue.render(anchor, &mut buffer, &Theme::default());

    (0..height)
        .map(|row| {
            (0..width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_empty_queue_draws_nothing_at_all() {
    let queue = Queue::default();

    assert_eq!(rendered(&queue, 40, 10).trim(), "");
    assert!(queue.is_empty());
    assert_eq!(queue.depth(), 0);
}

#[test]
fn a_queued_message_is_on_screen_with_the_hint_under_it() {
    let mut queue = Queue::default();
    queue.push_steered("steer-1".to_owned(), "use the other file".to_owned());

    let screen = rendered(&queue, 60, 10);

    assert!(screen.contains("use the other file"), "got:\n{screen}");
    assert!(screen.contains(HINT), "got:\n{screen}");
    assert_eq!(queue.depth(), 1);
}

/// The strip is a reminder, not a second transcript: a multi-line entry
/// takes one row.
#[test]
fn a_multi_line_entry_shows_only_its_first_line() {
    let mut queue = Queue::default();
    queue.push_steered("steer-1".to_owned(), "first line\nsecond line".to_owned());

    let screen = rendered(&queue, 60, 10);

    assert!(screen.contains("first line"), "got:\n{screen}");
    assert!(!screen.contains("second line"), "got:\n{screen}");
}

/// The composer must survive its own queue: past the cap the strip stops
/// growing and the newest entries are the ones kept, because the newest is
/// what an Up arrow acts on.
#[test]
fn a_long_queue_stops_growing_and_keeps_the_newest_entries() {
    let mut queue = Queue::default();
    for index in 0..9 {
        queue.push_steered(format!("steer-{index}"), format!("message {index}"));
    }

    let screen = rendered(&queue, 60, 20);

    assert_eq!(queue.depth(), 9);
    assert!(screen.contains("message 8"), "got:\n{screen}");
    assert!(!screen.contains("message 0"), "got:\n{screen}");
    assert_eq!(
        screen.lines().filter(|line| !line.is_empty()).count(),
        6,
        "five entries and the hint, and no more:\n{screen}"
    );
}

/// A screen with no room above the composer draws nothing rather than half
/// a strip.
#[test]
fn a_screen_with_no_room_above_the_composer_draws_nothing() {
    let mut queue = Queue::default();
    queue.push_steered("steer-1".to_owned(), "waiting".to_owned());

    assert_eq!(rendered(&queue, 40, 4).trim(), "");
}

#[test]
fn consuming_an_id_retires_exactly_that_entry_and_a_miss_is_ordinary() {
    let mut queue = Queue::default();
    queue.push_steered("steer-1".to_owned(), "first".to_owned());
    queue.push_steered("steer-2".to_owned(), "second".to_owned());

    assert!(queue.consume("steer-1"));
    assert_eq!(queue.depth(), 1);
    assert_eq!(queue.entries()[0].text, "second");

    assert!(
        !queue.consume("steer-1"),
        "a withdrawn entry is already gone, and saying so is not an error"
    );
}

/// The end of a turn is what moves a steer nobody took into the lane that
/// replays it.
#[test]
fn stranding_moves_every_unconsumed_steer_into_the_fallback_lane() {
    let mut queue = Queue::default();
    queue.push_steered("steer-1".to_owned(), "first".to_owned());
    assert!(!queue.has_fallback());

    queue.strand();

    assert!(queue.has_fallback());
    let taken = queue.take_next_fallback().expect("the entry is replayable");
    assert_eq!(taken.text, "first");
    assert!(!taken.is_steered());
    assert!(queue.is_empty());
}

/// A steered entry is still waiting on the engine, so the replay lane must
/// not take it: that is what would send the same message twice.
#[test]
fn the_replay_lane_never_takes_an_entry_the_engine_still_owes() {
    let mut queue = Queue::default();
    queue.push_steered("steer-1".to_owned(), "steered".to_owned());
    queue.push_fallback("steer-2".to_owned(), "queued".to_owned());

    let taken = queue.take_next_fallback().expect("the fallback entry");
    assert_eq!(taken.text, "queued");
    assert_eq!(queue.depth(), 1);
    assert!(queue.take_next_fallback().is_none());
}

#[test]
fn a_requeued_entry_goes_back_in_front_of_what_came_after_it() {
    let mut queue = Queue::default();
    queue.push_fallback("steer-1".to_owned(), "first".to_owned());
    queue.push_fallback("steer-2".to_owned(), "second".to_owned());

    let taken = queue.take_next_fallback().expect("the first entry");
    queue.requeue_front(taken);

    assert_eq!(
        queue
            .entries()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
}

#[test]
fn withdrawing_takes_the_newest_entry_back() {
    let mut queue = Queue::default();
    queue.push_steered("steer-1".to_owned(), "first".to_owned());
    queue.push_steered("steer-2".to_owned(), "second".to_owned());

    let taken = queue.withdraw_newest().expect("the newest entry");

    assert_eq!(taken.text, "second");
    assert_eq!(queue.depth(), 1);
    assert!(taken.is_steered());
}

/// **§7-5.** The composer is a consent surface, so a teammate's words may
/// not be handed to it: Up passes a peer's row over and takes the newest
/// one this person really wrote, and a strip holding only a peer's answers
/// nothing at all.
#[test]
fn withdrawing_passes_over_a_peers_row_and_takes_the_persons_own() {
    let mut queue = Queue::default();
    queue.push_steered("steer-1".to_owned(), "mine".to_owned());
    queue.push_peer("peer-1".to_owned(), "@Cargo.toml /init $skill".to_owned());

    let taken = queue.withdraw_newest().expect("the person's own entry");

    assert_eq!(taken.text, "mine");
    assert_eq!(queue.depth(), 1, "and the peer's row is still waiting");
    assert_eq!(queue.entries()[0].text, "@Cargo.toml /init $skill");

    assert!(
        queue.withdraw_newest().is_none(),
        "a strip holding only a peer's message has nothing to hand back"
    );
    assert_eq!(queue.depth(), 1);
}

/// The other half of the same rule: the replay lane resolves mentions and
/// runs command names, so the end of a turn must not put a peer's row into
/// it.
#[test]
fn stranding_leaves_a_peers_row_out_of_the_replay_lane() {
    let mut queue = Queue::default();
    queue.push_peer("peer-1".to_owned(), "@Cargo.toml /init".to_owned());
    queue.push_steered("steer-1".to_owned(), "mine".to_owned());

    queue.strand();

    let taken = queue.take_next_fallback().expect("the person's own entry");
    assert_eq!(taken.text, "mine");
    assert!(
        !queue.has_fallback(),
        "and the peer's row is nothing the lane may replay"
    );
    assert_eq!(queue.depth(), 1);
}

/// One `Command::Steer` carries a whole pass of a teammate's messages, so
/// its id stands for several rows and the event that names it retires all
/// of them.
#[test]
fn consuming_a_batchs_id_retires_every_row_it_stood_for() {
    let mut queue = Queue::default();
    queue.push_peer("steer-1".to_owned(), "the parser is done".to_owned());
    queue.push_peer("steer-1".to_owned(), "and the lexer".to_owned());
    queue.push_steered("steer-2".to_owned(), "mine".to_owned());

    assert!(queue.consume("steer-1"));

    assert_eq!(queue.depth(), 1);
    assert_eq!(queue.entries()[0].text, "mine");
}
