use super::{Names, frame, native};

const WHO: Names<'static> = Names { name: "w1", team: "session-abcd1234", lead: "team-lead" };

/// The frame opens on the three names and closes on the task, whatever
/// the channel paragraph says.
#[test]
fn the_frame_opens_on_the_names_and_ends_with_the_task() {
    let text = frame(WHO, "answer however you like", "have a look at the parser");

    assert!(
        text.starts_with(
            "You are w1, a teammate on the team session-abcd1234. Your lead is team-lead."
        ),
        "{text}"
    );
    assert!(text.contains("\n\nanswer however you like\n\n"), "{text}");
    assert!(
        text.ends_with("Your task:\n\nhave a look at the parser"),
        "the task is what the message ends with: {text}"
    );
}

/// The native channel names ganja's tool, the lead as its `to`, and `main`
/// as the address that reaches nobody.
#[test]
fn the_native_preamble_names_the_tool_the_lead_and_the_dead_main() {
    let text = native(WHO, "hold the fort");

    assert!(text.contains("`send_message`"), "{text}");
    assert!(text.contains("`to: \"team-lead\"`"), "{text}");
    assert!(text.contains("Do not address \"main\""), "{text}");
    assert!(text.ends_with("Your task:\n\nhold the fort"), "{text}");
}
