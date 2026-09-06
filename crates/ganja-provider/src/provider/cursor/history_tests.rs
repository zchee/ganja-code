use std::collections::HashMap;

use buffa::Message as _;
use sha2::{Digest as _, Sha256};

use super::super::proto;
use super::{Action, CALL_INPUT_LIMIT, Composed, compose, derived};
use crate::protocol::{Message, Part, PartBody, PartId, ToolState, Usage};
use crate::provider::ChatRequest;

/// A conversation whose current turn opens at `turn_start`, under the system
/// prompt every request here carries.
fn request(messages: Vec<Message>, turn_start: usize) -> ChatRequest {
    ChatRequest {
        model: "gpt-5.3-codex".to_owned(),
        system: Some("You are terse.".to_owned()),
        messages,
        turn_start,
        tools: Vec::new(),
        effort_options: Default::default(),
    }
}

/// An assistant reply carrying `text` and `parts` after it.
fn reply(text: &str, parts: Vec<Part>) -> Message {
    let mut message = Message::assistant("gpt-5.3-codex");
    if !text.is_empty() {
        message.parts.push(Part::text(text));
    }
    message.parts.extend(parts);

    message
}

fn part(body: PartBody) -> Part {
    Part { id: PartId::ascending(), body }
}

fn call(call_id: &str, tool: &str, state: ToolState) -> Part {
    part(PartBody::Tool { call_id: call_id.to_owned(), tool: tool.to_owned(), state })
}

fn completed(input: serde_json::Value, output: &str) -> ToolState {
    ToolState::Completed {
        input,
        output: output.to_owned(),
        title: String::new(),
        metadata: serde_json::Value::Null,
        started: 0,
        completed: 0,
    }
}

fn errored(input: serde_json::Value, error: &str) -> ToolState {
    ToolState::Error { input, error: error.to_owned(), started: 0, completed: 0 }
}

/// The root entry at `index`, decoded from the blob its id names.
fn root(composed: &Composed, index: usize) -> serde_json::Value {
    let id = &composed.root[index];
    let bytes = composed.blobs.get(id).expect("every root id names a blob in the store");

    serde_json::from_slice(bytes).expect("a root entry is JSON")
}

/// The reference's `{role, content: [{type: "text", text}]}` spelling.
fn text_entry(role: &str, text: &str) -> serde_json::Value {
    serde_json::json!({ "role": role, "content": [{ "type": "text", "text": text }] })
}

/// The turn at `index`: its user message and its steps' texts, each decoded
/// from the blob the turn names.
fn turn(composed: &Composed, index: usize) -> (proto::UserMessage, Vec<String>) {
    let blob = |id: &[u8]| composed.blobs.get(id).expect("every id a turn names is in the store");
    let decoded = proto::ConversationTurn::decode_from_slice(blob(&composed.turns[index]))
        .expect("a turn blob decodes");
    let agent = decoded.agent_conversation_turn.as_option().expect("the agent arm");
    let user = proto::UserMessage::decode_from_slice(blob(
        agent.user_message.as_deref().expect("the user message id"),
    ))
    .expect("a user blob decodes");
    let steps = agent
        .steps
        .iter()
        .map(|id| {
            proto::ConversationStep::decode_from_slice(blob(id))
                .expect("a step blob decodes")
                .assistant_message
                .as_option()
                .and_then(|message| message.text.clone())
                .expect("every step is an assistant message with text")
        })
        .collect();

    (user, steps)
}

/// Every id the state names is a key in the store, and each key is the
/// sha256 of what it holds — the reference's content addressing
/// (`proxy.ts:726-733`).
fn assert_content_addressed(composed: &Composed) {
    for id in composed.root.iter().chain(&composed.turns) {
        assert_eq!(id.len(), 32, "a raw sha256 digest");
        assert!(composed.blobs.contains_key(id), "an id the request names is in the store");
    }
    for (id, bytes) in &composed.blobs {
        assert_eq!(id.as_slice(), Sha256::digest(bytes).as_slice(), "the id is the hash");
    }
}

/// **AC-1.** The first turn is history — the system head, the prompt and the
/// reply as root entries, one turn with one step — and the second is the
/// action, under a conversation id derived from the first message.
#[test]
fn a_two_turn_request_composes_the_first_turn_as_history_and_the_second_as_the_action() {
    let u1 = Message::user("What does this crate do?");
    let u1_id = u1.id.clone();
    let composed =
        compose(&request(vec![u1, reply("It parses TOML.", Vec::new()), Message::user("How?")], 2));

    assert_eq!(composed.root.len(), 3, "the system head, the prompt, the reply");
    assert_eq!(
        root(&composed, 0),
        serde_json::json!({ "role": "system", "content": "You are terse." }),
        "the head is the reference's bare-string shape"
    );
    assert_eq!(root(&composed, 1), text_entry("user", "What does this crate do?"));
    assert_eq!(root(&composed, 2), text_entry("assistant", "It parses TOML."));

    assert_eq!(composed.turns.len(), 1);
    let (user, steps) = turn(&composed, 0);
    assert_eq!(user.text.as_deref(), Some("What does this crate do?"));
    assert_eq!(user.message_id.as_deref(), Some(derived(&u1_id).as_str()));
    assert_eq!(steps, vec!["It parses TOML.".to_owned()]);

    assert_eq!(composed.action, Action::User { text: "How?".to_owned() });
    assert_eq!(composed.conversation_id.as_deref(), Some(derived(&u1_id).as_str()));
    assert_eq!(composed.clamped_calls, 0);
    assert_eq!(composed.blobs.len(), 6, "three root entries, one turn, its user blob, its step");
    assert_content_addressed(&composed);
}

/// **AC-2.** A reply that called tools composes as one assistant entry —
/// its text, then one `[Tool Call]` paragraph per call in call order — and
/// then one result entry per call in the same order, marked when the call
/// failed; the turn's steps carry the same three texts.
#[test]
fn an_assistant_reply_with_tool_calls_composes_the_calls_beside_its_text_and_the_results_after_it()
{
    let replied = reply(
        "Looking.",
        vec![
            call("c1", "read", completed(serde_json::json!({ "path": "a.rs" }), "fn a() {}")),
            call("c2", "bash", errored(serde_json::json!({ "command": "false" }), "exit 1")),
        ],
    );
    let composed = compose(&request(
        vec![Message::user("Read a.rs, then run false."), replied, Message::user("Thanks.")],
        2,
    ));

    let assistant = "Looking.\n\n[Tool Call] read {\"path\":\"a.rs\"}\n\n[Tool Call] bash \
                     {\"command\":\"false\"}";
    assert_eq!(composed.root.len(), 5, "head, prompt, assistant, two results");
    assert_eq!(root(&composed, 2), text_entry("assistant", assistant));
    assert_eq!(root(&composed, 3), text_entry("user", "[Tool Result]\nfn a() {}"));
    assert_eq!(root(&composed, 4), text_entry("user", "[Tool Result (error)]\nexit 1"));

    let (_, steps) = turn(&composed, 0);
    assert_eq!(
        steps,
        vec![
            assistant.to_owned(),
            "[Tool Result]\nfn a() {}".to_owned(),
            "[Tool Result (error)]\nexit 1".to_owned(),
        ],
        "the steps are the root's texts, as assistant messages"
    );
    assert_eq!(composed.clamped_calls, 0, "both inputs are under the limit");
    assert_content_addressed(&composed);

    // A call the model never finished, and one still running when the turn
    // died: no result to show for either — rendered as the hole every wire
    // renders a dead turn's call by — and no input to show for the first.
    let dead = reply(
        "",
        vec![
            call("c1", "read", ToolState::Pending { input: None }),
            call(
                "c2",
                "bash",
                ToolState::Running {
                    input: serde_json::json!({ "command": "sleep 9" }),
                    metadata: serde_json::Value::Null,
                    started: 0,
                },
            ),
        ],
    );
    let composed = compose(&request(vec![Message::user("Read."), dead, Message::user("Well?")], 2));
    assert_eq!(
        root(&composed, 2),
        text_entry(
            "assistant",
            "[Tool Call] read {}\n\n[Tool Call] bash {\"command\":\"sleep 9\"}"
        )
    );
    let hole = text_entry("user", "[Tool Result (error)]\n[no result recorded]");
    assert_eq!(root(&composed, 3), hole, "the call that never finished streaming");
    assert_eq!(root(&composed, 4), hole, "and the one that was still running");
}

/// **AC-2b.** A call input past [`CALL_INPUT_LIMIT`] is cut on a char
/// boundary with an elision naming exactly the bytes omitted; one under it
/// is rendered whole; and the composition counts the cut.
#[test]
fn a_call_input_past_the_limit_is_cut_at_a_char_boundary_naming_what_was_omitted() {
    // Three-byte characters, so the limit lands inside one and the cut has
    // to step back to the boundary before it.
    let content = "€".repeat(3_000);
    let big = serde_json::json!({ "content": content });
    let small = serde_json::json!({ "path": "a.rs" });
    let replied = reply(
        "",
        vec![
            call("c1", "write", completed(big.clone(), "wrote")),
            call("c2", "read", completed(small.clone(), "fn a() {}")),
        ],
    );
    let composed =
        compose(&request(vec![Message::user("Write it."), replied, Message::user("Done?")], 2));

    let compact = serde_json::to_string(&big).expect("serializes");
    assert!(compact.len() > CALL_INPUT_LIMIT, "the fixture is over the limit");
    let cut = compact.floor_char_boundary(CALL_INPUT_LIMIT);
    assert!(cut < CALL_INPUT_LIMIT, "the limit fell inside a character");
    let omitted = compact.len() - cut;
    let expected = format!(
        "[Tool Call] write {}… [+{omitted} bytes]\n\n[Tool Call] read {}",
        &compact[..cut],
        serde_json::to_string(&small).expect("serializes")
    );
    assert_eq!(root(&composed, 2), text_entry("assistant", &expected));
    assert_eq!(composed.clamped_calls, 1, "one of the two calls was cut");
}

/// **AC-3.** A history message carrying every part a wire never sends, and
/// nothing else, contributes no root entry, no step and no blob — and with
/// no entry to stand beside, the system head is not sent either.
#[test]
fn a_message_carrying_only_parts_a_wire_never_sends_leaves_no_trace_in_the_state() {
    let excluded = || {
        vec![
            part(PartBody::File {
                path: "a.png".to_owned(),
                mime: "image/png".to_owned(),
                start: None,
                end: None,
                content: Some("aGk=".to_owned()),
            }),
            part(PartBody::File {
                path: "a.rs".to_owned(),
                mime: "text/x-rust".to_owned(),
                start: None,
                end: None,
                content: None,
            }),
            part(PartBody::StepStart),
            part(PartBody::StepFinish { usage: Usage::default() }),
            part(PartBody::Patch { hash: "abc".to_owned(), files: vec!["a.rs".to_owned()] }),
            part(PartBody::ReasoningText { text: "thinking".to_owned() }),
            part(PartBody::ServerTool {
                tool: "web_search".to_owned(),
                input: serde_json::json!({ "q": "x" }),
                output: "y".to_owned(),
            }),
            part(PartBody::Peer {
                from: "worker".to_owned(),
                summary: None,
                color: None,
                body: "done".to_owned(),
            }),
            part(PartBody::Reasoning {
                provider: "openai".to_owned(),
                item: "rs_1".to_owned(),
                encrypted: Some("sealed".to_owned()),
            }),
        ]
    };
    let mut user = Message::user("");
    user.parts = excluded();
    let assistant = reply("", excluded());

    let composed = compose(&request(vec![user, assistant, Message::user("Hello?")], 2));

    assert!(composed.root.is_empty(), "no root entry, and no head without one");
    assert!(composed.turns.is_empty(), "no turn, no step");
    assert!(composed.blobs.is_empty(), "no blob");
    assert_eq!(composed.action, Action::User { text: "Hello?".to_owned() });
}

/// **AC-4, the unit half.** A request with nothing before its newest run —
/// a first turn, a title or summary one-shot — composes the empty state it
/// always sent, under a conversation id all the same; a request with no
/// messages composes nothing and names no conversation.
#[test]
fn a_request_with_nothing_before_its_newest_run_composes_the_empty_state() {
    let opening = Message::user("say hi");
    let id = opening.id.clone();
    let composed = compose(&request(vec![opening], 0));

    assert!(composed.root.is_empty());
    assert!(composed.turns.is_empty());
    assert!(composed.blobs.is_empty());
    assert_eq!(composed.action, Action::User { text: "say hi".to_owned() });
    assert_eq!(composed.conversation_id.as_deref(), Some(derived(&id).as_str()));

    let empty = compose(&request(Vec::new(), 0));
    assert!(empty.root.is_empty() && empty.turns.is_empty() && empty.blobs.is_empty());
    assert_eq!(empty.action, Action::User { text: String::new() }, "today's empty message");
    assert_eq!(empty.conversation_id, None, "nothing to derive one from");
}

/// **AC-5.** A compaction summary — an assistant message before any user
/// message — is carried in the root, where the server reads it, and dropped
/// from the turns, the reference's rule.
#[test]
fn a_compaction_summary_is_carried_in_the_root_and_dropped_from_the_turns() {
    let composed = compose(&request(
        vec![reply("Earlier: the loader was ported.", Vec::new()), Message::user("Now tests.")],
        1,
    ));

    assert_eq!(composed.root.len(), 2);
    assert_eq!(
        root(&composed, 0),
        serde_json::json!({ "role": "system", "content": "You are terse." })
    );
    assert_eq!(root(&composed, 1), text_entry("assistant", "Earlier: the loader was ported."));
    assert!(composed.turns.is_empty(), "no user entry opened a turn for the summary to step");
    assert_eq!(composed.action, Action::User { text: "Now tests.".to_owned() });
    assert_content_addressed(&composed);
}

/// **AC-6.** A steer a finished turn consumed is history — a turn of its own,
/// with no steps — and a continuation block emitted where nothing was steered
/// now carries the prompt and the reply it is about, the hole
/// `newest_user_text` alone could not close.
#[test]
fn a_consumed_steer_is_a_turn_of_its_own_and_a_continuation_block_carries_its_prompt() {
    let across_turns = compose(&request(
        vec![
            Message::user("write the config parser"),
            reply("Written.", Vec::new()),
            Message::user("actually make it lenient about unknown keys"),
            Message::user("now add tests"),
        ],
        3,
    ));
    assert_eq!(across_turns.root.len(), 4, "head, prompt, reply, steer");
    assert_eq!(root(&across_turns, 1), text_entry("user", "write the config parser"));
    assert_eq!(root(&across_turns, 2), text_entry("assistant", "Written."));
    assert_eq!(
        root(&across_turns, 3),
        text_entry("user", "actually make it lenient about unknown keys")
    );
    assert_eq!(across_turns.turns.len(), 2);
    let (prompt, steps) = turn(&across_turns, 0);
    assert_eq!(prompt.text.as_deref(), Some("write the config parser"));
    assert_eq!(steps, vec!["Written.".to_owned()]);
    let (steer, steps) = turn(&across_turns, 1);
    assert_eq!(steer.text.as_deref(), Some("actually make it lenient about unknown keys"));
    assert!(steps.is_empty(), "the steer was consumed by a reply already composed");
    assert_eq!(across_turns.action, Action::User { text: "now add tests".to_owned() });

    let continued = compose(&request(
        vec![
            Message::user("port the config loader"),
            reply("Ported.", Vec::new()),
            Message::user("<team_still_working>keep going</team_still_working>"),
        ],
        0,
    ));
    assert_eq!(continued.root.len(), 3, "head, prompt, reply");
    assert_eq!(root(&continued, 1), text_entry("user", "port the config loader"));
    assert_eq!(root(&continued, 2), text_entry("assistant", "Ported."));
    assert_eq!(continued.turns.len(), 1);
    let (prompt, steps) = turn(&continued, 0);
    assert_eq!(prompt.text.as_deref(), Some("port the config loader"));
    assert_eq!(steps, vec!["Ported.".to_owned()]);
    assert_eq!(
        continued.action,
        Action::User { text: "<team_still_working>keep going</team_still_working>".to_owned() }
    );
}

/// **AC-7.** A request whose newest message is the assistant's — the shape
/// only a bridged step can leave — goes out as a resume over the whole
/// history, its call and result included; a steer after the same step goes
/// out as a user action over that same history.
#[test]
fn a_request_ending_in_the_assistants_message_resumes_over_the_whole_history() {
    let stepped = reply(
        "",
        vec![call("c1", "read", completed(serde_json::json!({ "path": "a.rs" }), "fn a() {}"))],
    );
    let prompt = Message::user("Read a.rs.");

    let resumed = compose(&request(vec![prompt.clone(), stepped.clone()], 0));
    assert_eq!(resumed.action, Action::Resume);
    assert_eq!(resumed.root.len(), 4, "head, prompt, the call, its result");
    assert_eq!(root(&resumed, 1), text_entry("user", "Read a.rs."));
    assert_eq!(root(&resumed, 2), text_entry("assistant", "[Tool Call] read {\"path\":\"a.rs\"}"));
    assert_eq!(root(&resumed, 3), text_entry("user", "[Tool Result]\nfn a() {}"));
    assert_content_addressed(&resumed);

    let steered = compose(&request(vec![prompt, stepped, Message::user("And b.rs.")], 0));
    assert_eq!(steered.action, Action::User { text: "And b.rs.".to_owned() });
    assert_eq!(steered.root, resumed.root, "the same history, under a user action");
    assert_eq!(steered.turns, resumed.turns);
}

/// **AC-8, the composition half.** A derived id is v4-shaped, distinct for
/// distinct messages and the same for the same one; a rebuild of one request
/// names the same ids; and a message's user blob keeps its id on the next
/// request, which is what makes the history the server cached still name
/// the same blobs.
#[test]
fn derived_ids_are_v4_shaped_distinct_per_message_and_stable_across_rebuilds() {
    let u1 = Message::user("first");
    let u2 = Message::user("second");
    let id = derived(&u1.id);
    assert_eq!(id.len(), 36);
    assert_eq!(id.as_bytes()[14], b'4', "the version nibble: {id}");
    assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "the variant bits: {id}");
    assert_ne!(id, derived(&u2.id), "distinct messages, distinct ids");
    assert_eq!(id, derived(&u1.id), "the same message, the same id");

    let first = request(vec![u1.clone(), reply("one", Vec::new()), u2.clone()], 2);
    let once = compose(&first);
    let twice = compose(&first);
    assert_eq!(once.root, twice.root, "the same request names the same root ids");
    assert_eq!(once.turns, twice.turns, "and the same turn ids");
    assert_eq!(
        once.blobs.keys().collect::<std::collections::BTreeSet<_>>(),
        twice.blobs.keys().collect::<std::collections::BTreeSet<_>>(),
        "and holds the same blobs"
    );
    assert_eq!(once.conversation_id, twice.conversation_id);

    let mut later = first.messages.clone();
    later.push(reply("two", Vec::new()));
    later.push(Message::user("third"));
    let next = compose(&request(later, 4));
    let (on_first, _) = turn(&once, 0);
    let (on_next, _) = turn(&next, 0);
    assert_eq!(
        on_first.message_id, on_next.message_id,
        "the first message's user blob keeps its id on the next request"
    );
    assert_eq!(once.turns[0], next.turns[0], "and so does the turn that names it");
    assert_eq!(next.conversation_id, once.conversation_id, "one conversation, one id");
    let unchanged: HashMap<_, _> =
        once.blobs.iter().filter(|(id, _)| next.blobs.contains_key(*id)).collect();
    assert_eq!(unchanged.len(), once.blobs.len(), "every earlier blob is still named");
}
