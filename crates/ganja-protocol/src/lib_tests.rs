use std::{collections::BTreeSet, sync::Mutex, thread};

use super::{
    Command, Event, FinishReason, HeldDecision, HeldId, HeldOutcome, HoldCause, Mention, Message,
    MessageId, MessageTime, Part, PartBody, PartId, PermissionId, PermissionMode, PermissionReply,
    PolicySource, QuestionId, QuestionInfo, QuestionOption, QuestionSource, REASONING_TAG,
    RedactedText, RevertInfo, RevertScope, Role, SessionId, ToolState, UnknownPermissionMode,
    Usage, is_uuidv7, team, uuidv7,
};

/// The session every pinned event happens in.
fn pinned_session() -> SessionId {
    SessionId::from("ses_1".to_owned())
}

/// Builds a completed tool part with pinned ids and times, the richest
/// shape a part takes on the wire.
fn pinned_tool_part() -> Part {
    Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({"path": "a.rs"}),
                output: "fn main() {}".to_owned(),
                title: "a.rs".to_owned(),
                metadata: serde_json::json!({}),
                started: 7,
                completed: 9,
            },
        },
    }
}

/// Builds a message with pinned ids and times so a test can assert on the
/// exact bytes that reach the wire.
fn pinned_message() -> Message {
    Message {
        id: MessageId::from("msg_1".to_owned()),
        role: Role::User,
        parts: vec![Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Text {
                text: "hi".to_owned(),
            },
        }],
        time: MessageTime {
            created: 7,
            completed: Some(7),
        },
        model: None,
        usage: None,
    }
}

#[test]
fn uuidv7_ids_sort_in_creation_order() {
    let ids: Vec<MessageId> = (0..64).map(|_| MessageId::ascending()).collect();

    assert!(
        ids.iter().all(|id| is_uuidv7(id.as_str())),
        "ids should be bare lowercase hyphenated UUIDv7: {ids:?}"
    );
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "ids should sort in creation order: {ids:?}"
    );
    let distinct: BTreeSet<&str> = ids.iter().map(MessageId::as_str).collect();
    assert_eq!(distinct.len(), ids.len(), "no id should repeat: {ids:?}");

    let parts: Vec<PartId> = (0..64).map(|_| PartId::ascending()).collect();
    assert!(parts.iter().all(|id| is_uuidv7(id.as_str())));
    assert!(parts.windows(2).all(|pair| pair[0] < pair[1]));

    let sessions: Vec<SessionId> = (0..64).map(|_| SessionId::ascending()).collect();
    assert!(sessions.iter().all(|id| is_uuidv7(id.as_str())));
    assert!(sessions.windows(2).all(|pair| pair[0] < pair[1]));

    // The three ids nothing above mints, so that "every id here" is every id.
    assert!(is_uuidv7(PermissionId::ascending().as_str()));
    assert!(is_uuidv7(QuestionId::ascending().as_str()));
    assert!(is_uuidv7(HeldId::ascending().as_str()));
}

#[test]
fn is_uuidv7_accepts_only_the_spelling_the_mint_writes() {
    let minted = uuidv7();
    assert!(is_uuidv7(&minted));

    // The same UUID, spelled four other legal ways. Each is refused,
    // because the callers outside this crate — the store deciding whether
    // its rows predate this mint — are asking whether *this* wrote the id.
    assert!(!is_uuidv7(&minted.to_uppercase()));
    assert!(!is_uuidv7(&minted.replace('-', "")));
    assert!(!is_uuidv7(&format!("{{{minted}}}")));
    assert!(!is_uuidv7(&format!("urn:uuid:{minted}")));

    // The layout D493 retired, a UUID of another version, and text that is
    // no UUID at all.
    assert!(!is_uuidv7("ses_0198f2c4a1b000001"));
    assert!(!is_uuidv7("00000000-0000-4000-8000-000000000000"));
    assert!(!is_uuidv7(""));
}

#[test]
fn ids_are_monotonic_within_one_millisecond_across_threads() {
    /// A UUIDv7's leading `xxxxxxxx-xxxx` is the 48-bit millisecond, so two
    /// ids sharing this prefix were minted inside the same one.
    fn millisecond(id: &str) -> &str {
        &id[..13]
    }

    const THREADS: usize = 8;
    const PER_THREAD: usize = 512;

    // Minting and recording happen inside one critical section, so the
    // vector's order *is* the order the mints happened in. That is what
    // lets this assert "each id sorts after the one minted before it"
    // outright instead of settling for a claim about each thread alone —
    // and it is deterministic, where sorting afterwards would be a race
    // this test would sometimes lose.
    let minted: Mutex<Vec<String>> = Mutex::new(Vec::with_capacity(THREADS * PER_THREAD));

    thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                for _ in 0..PER_THREAD {
                    let mut minted = minted.lock().expect("no mint may panic under the lock");
                    minted.push(uuidv7());
                }
            });
        }
    });

    let minted = minted
        .into_inner()
        .expect("no mint may panic under the lock");

    assert_eq!(minted.len(), THREADS * PER_THREAD);
    assert!(minted.iter().all(|id| is_uuidv7(id)));
    assert!(
        minted.windows(2).all(|pair| pair[0] < pair[1]),
        "ids should sort in mint order even when eight threads mint them"
    );

    let distinct: BTreeSet<&String> = minted.iter().collect();
    assert_eq!(distinct.len(), minted.len(), "no id should repeat");

    // Without this the run could have spent a millisecond per id and told
    // us nothing about the case the counter exists for.
    assert!(
        minted
            .windows(2)
            .any(|pair| millisecond(&pair[0]) == millisecond(&pair[1])),
        "four thousand mints should have shared a millisecond somewhere"
    );
}

#[test]
fn a_user_message_is_born_complete_and_a_reply_is_not() {
    let user = Message::user("hi");

    assert_eq!(user.role, Role::User);
    assert_eq!(user.time.completed, Some(user.time.created));
    assert_eq!(user.parts.first().and_then(Part::as_text), Some("hi"));
    assert!(user.model.is_none());

    let mut assistant = Message::assistant("canned");

    assert_eq!(assistant.role, Role::Assistant);
    assert!(assistant.parts.is_empty());
    assert!(assistant.time.completed.is_none());
    assert_eq!(assistant.model.as_deref(), Some("canned"));

    let completed = assistant.complete();
    assert_eq!(assistant.time.completed, Some(completed));
}

#[test]
fn an_empty_part_is_not_content_but_a_filled_one_is() {
    let mut message = Message::assistant("canned");
    assert!(!message.has_content());

    message.parts.push(Part::text(""));
    assert!(!message.has_content());

    if let Some(text) = message.parts.last_mut().and_then(Part::as_text_mut) {
        text.push_str("hello");
    }
    assert!(message.has_content());
}

#[test]
fn commands_round_trip_through_json() {
    let cases = [
        Command::SendPrompt {
            text: "hello".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
        Command::SendPrompt {
            text: "what does this do".to_owned(),
            mentions: vec![Mention {
                path: "src/main.rs".to_owned(),
                ..Default::default()
            }],
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
        Command::SendPrompt {
            text: "explain these lines".to_owned(),
            mentions: vec![Mention {
                path: "src/main.rs".to_owned(),
                start: Some(10),
                end: Some(20),
            }],
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
        Command::Steer {
            id: "steer-1".to_owned(),
            text: "actually, use the other file".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
        Command::Steer {
            id: "steer-2".to_owned(),
            text: "this one".to_owned(),
            mentions: vec![Mention {
                path: "src/main.rs".to_owned(),
                start: Some(10),
                end: Some(20),
            }],
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
        Command::SendPrompt {
            text: "ask @backend about it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: vec!["backend".to_owned()],
            peers: Vec::new(),
        },
        Command::Steer {
            id: "steer-6".to_owned(),
            text: "and tell @worker too".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: vec!["worker".to_owned()],
            peers: Vec::new(),
        },
        Command::CancelTurn,
        Command::ReplyPermission {
            id: PermissionId::from("perm_1".to_owned()),
            reply: PermissionReply::Always,
        },
        Command::SwitchAgent {
            name: "plan".to_owned(),
        },
        Command::SwitchModel {
            model: "claude-haiku-4.5".to_owned(),
        },
        Command::SwitchEffort {
            effort: Some("max".to_owned()),
        },
        Command::SwitchEffort { effort: None },
        Command::SetPermissionMode {
            mode: PermissionMode::Ask,
        },
        Command::SetPermissionMode {
            mode: PermissionMode::Bypass,
        },
        Command::RunShell {
            command: "git status".to_owned(),
        },
        Command::RunCommand {
            name: "init".to_owned(),
            args: "focus on the tests".to_owned(),
        },
        Command::Compact,
        Command::NewSession,
        Command::Undo,
        Command::Redo,
        Command::RevertTo {
            message_id: MessageId::from("msg_1".to_owned()),
            scope: RevertScope::Both,
        },
        Command::RevertTo {
            message_id: MessageId::from("msg_2".to_owned()),
            scope: RevertScope::Conversation,
        },
        Command::RevertTo {
            message_id: MessageId::from("msg_3".to_owned()),
            scope: RevertScope::Files,
        },
    ];

    for command in cases {
        let encoded = serde_json::to_string(&command).expect("a command serializes");
        let decoded: Command = serde_json::from_str(&encoded).expect("a command deserializes");
        assert_eq!(decoded, command, "round trip changed {encoded}");
    }
}

/// The cases cover every variant, so the loop's accessor assertion is
/// also the proof that [`Event::session_id`] reads every one of them.
#[test]
fn events_round_trip_through_json() {
    let message = pinned_message();
    let cases = [
        Event::MessageStarted {
            session_id: pinned_session(),
            message: message.clone(),
        },
        Event::PartStarted {
            session_id: pinned_session(),
            message_id: message.id.clone(),
            part: Part::text(""),
        },
        Event::PartDelta {
            session_id: pinned_session(),
            message_id: message.id.clone(),
            part_id: PartId::from("prt_1".to_owned()),
            delta: "hi".to_owned(),
        },
        Event::MessageFinished {
            session_id: pinned_session(),
            message_id: message.id.clone(),
            reason: FinishReason::Failed,
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 4,
                reasoning_tokens: 5,
                cache_read_tokens: 6,
                cache_write_tokens: 7,
            }),
            error: Some("no credentials".to_owned()),
            completed: 9,
        },
        Event::PartUpdated {
            session_id: pinned_session(),
            message_id: message.id.clone(),
            part: pinned_tool_part(),
        },
        Event::PermissionRequested {
            session_id: pinned_session(),
            id: PermissionId::from("perm_1".to_owned()),
            call_id: "call_1".to_owned(),
            tool: "shell".to_owned(),
            title: "cargo test".to_owned(),
            args: serde_json::json!({"command": "cargo test"}),
            directories: vec!["/tmp/scratch".to_owned()],
        },
        Event::PermissionReplied {
            session_id: pinned_session(),
            id: PermissionId::from("perm_1".to_owned()),
            reply: PermissionReply::Reject,
        },
        Event::SteerConsumed {
            session_id: pinned_session(),
            id: "steer-1".to_owned(),
        },
        Event::RevertChanged {
            session_id: pinned_session(),
            revert: Some(RevertInfo {
                message_id: message.id.clone(),
                files: vec!["src/main.rs".to_owned()],
            }),
            prompt: Some("rename the thing".to_owned()),
        },
        Event::RevertChanged {
            session_id: pinned_session(),
            revert: None,
            prompt: None,
        },
        Event::AgentChanged {
            session_id: pinned_session(),
            agent: "build".to_owned(),
            model: "claude-sonnet-4-5".to_owned(),
        },
        Event::EffortChanged {
            session_id: pinned_session(),
            effort: Some("max".to_owned()),
        },
        Event::EffortChanged {
            session_id: pinned_session(),
            effort: None,
        },
        Event::PermissionModeChanged {
            session_id: pinned_session(),
            mode: PermissionMode::Bypass,
        },
        Event::PeerHeld {
            session_id: pinned_session(),
            id: HeldId::from("held_1".to_owned()),
            from: "w1@inbound".to_owned(),
            cause: HoldCause::NoModeAsserted,
            summary: Some(RedactedText::from("picked up W2".to_owned())),
            preview: RedactedText::from("starting on the protocol surface".to_owned()),
            expires_in_ms: Some(300_000),
        },
        Event::PeerHoldSettled {
            session_id: pinned_session(),
            id: HeldId::from("held_1".to_owned()),
            outcome: HeldOutcome::Delivered,
        },
    ];

    for event in cases {
        assert_eq!(
            event.session_id(),
            &pinned_session(),
            "the accessor reads the session off {event:?}"
        );

        let encoded = serde_json::to_string(&event).expect("an event serializes");
        let decoded: Event = serde_json::from_str(&encoded).expect("an event deserializes");
        assert_eq!(decoded, event, "round trip changed {encoded}");
    }
}

#[test]
fn agent_changed_carries_the_session_the_agent_and_the_model() {
    let event = Event::AgentChanged {
        session_id: pinned_session(),
        agent: "build".to_owned(),
        model: "claude-sonnet-4-5".to_owned(),
    };

    assert_eq!(event.session_id(), &pinned_session());
    match event {
        Event::AgentChanged { agent, model, .. } => {
            assert_eq!(agent, "build");
            assert_eq!(model, "claude-sonnet-4-5");
        }
        other => panic!("expected an agent change, got {other:?}"),
    }
}

#[test]
fn effort_changed_carries_the_session_and_the_effort() {
    let event = Event::EffortChanged {
        session_id: pinned_session(),
        effort: Some("max".to_owned()),
    };

    assert_eq!(event.session_id(), &pinned_session());
    match event {
        Event::EffortChanged { effort, .. } => {
            assert_eq!(effort.as_deref(), Some("max"));
        }
        other => panic!("expected an effort change, got {other:?}"),
    }
}

/// Pins the bytes of every variant. A change here is a protocol change: it
/// invalidates stored sessions and anything speaking the protocol over a
/// socket, so it has to be a deliberate edit rather than a side effect of
/// renaming a field.
#[test]
fn the_wire_format_is_stable() {
    let cases = [
        // A prompt with nothing attached, whose bytes are exactly what
        // they were before mentions existed.
        (
            serde_json::to_string(&Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            }),
            r#"{"type":"send_prompt","text":"hi"}"#,
        ),
        (
            serde_json::to_string(&Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: vec![
                    Mention {
                        path: "src/main.rs".to_owned(),
                        ..Default::default()
                    },
                    Mention {
                        path: "README.md".to_owned(),
                        ..Default::default()
                    },
                ],
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            }),
            r#"{"type":"send_prompt","text":"hi","mentions":[{"path":"src/main.rs"},{"path":"README.md"}]}"#,
        ),
        // An `@path#12-40` mention carries the lines it named; the range
        // rides beside the path exactly as the file part's does.
        (
            serde_json::to_string(&Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: vec![Mention {
                    path: "src/main.rs".to_owned(),
                    start: Some(12),
                    end: Some(40),
                }],
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            }),
            r#"{"type":"send_prompt","text":"hi","mentions":[{"path":"src/main.rs","start":12,"end":40}]}"#,
        ),
        // A steer with nothing attached writes no `mentions` key at all,
        // exactly as a prompt without one does: the two commands carry the
        // same payload and so keep the same absence rule.
        (
            serde_json::to_string(&Command::Steer {
                id: "steer-1".to_owned(),
                text: "use the other file".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            }),
            r#"{"type":"steer","id":"steer-1","text":"use the other file"}"#,
        ),
        (
            serde_json::to_string(&Command::Steer {
                id: "steer-2".to_owned(),
                text: "this one".to_owned(),
                mentions: vec![Mention {
                    path: "src/main.rs".to_owned(),
                    start: Some(10),
                    end: Some(20),
                }],
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            }),
            r#"{"type":"steer","id":"steer-2","text":"this one","mentions":[{"path":"src/main.rs","start":10,"end":20}]}"#,
        ),
        // A `$skill` invocation rides as names beside the mentions — the
        // token itself stays in the text — and keeps the mentions'
        // absence rule: no invocations, no key.
        (
            serde_json::to_string(&Command::SendPrompt {
                text: "use $porting here".to_owned(),
                mentions: Vec::new(),
                skills: vec!["porting".to_owned()],
                session_mentions: Vec::new(),
                peers: Vec::new(),
            }),
            r#"{"type":"send_prompt","text":"use $porting here","skills":["porting"]}"#,
        ),
        (
            serde_json::to_string(&Command::Steer {
                id: "steer-3".to_owned(),
                text: "and $tdd too".to_owned(),
                mentions: Vec::new(),
                skills: vec!["tdd".to_owned()],
                session_mentions: Vec::new(),
                peers: Vec::new(),
            }),
            r#"{"type":"steer","id":"steer-3","text":"and $tdd too","skills":["tdd"]}"#,
        ),
        // An `@`-mention that named a teammate or a live session rides as
        // names beside the skills — the token itself stays in the text —
        // and keeps the same absence rule (**D529**): the prompts above
        // that carry none still write exactly the bytes they wrote before
        // this field existed.
        (
            serde_json::to_string(&Command::SendPrompt {
                text: "ask @backend about it".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: vec!["backend".to_owned()],
                peers: Vec::new(),
            }),
            r#"{"type":"send_prompt","text":"ask @backend about it","session_mentions":["backend"]}"#,
        ),
        (
            serde_json::to_string(&Command::Steer {
                id: "steer-5".to_owned(),
                text: "and tell @worker too".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: vec!["worker".to_owned()],
                peers: Vec::new(),
            }),
            r#"{"type":"steer","id":"steer-5","text":"and tell @worker too","session_mentions":["worker"]}"#,
        ),
        // A teammate's message rides beside them under the same absence
        // rule, which is the whole of the backward-compatibility claim:
        // every prompt above still writes the bytes it wrote before teams
        // existed, because no `peers` key appears unless one is carried.
        (
            serde_json::to_string(&Command::SendPrompt {
                text: String::new(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: vec![team::PeerPayload::new(
                    "w1",
                    Some("picked up W2".to_owned()),
                    None,
                    "on the protocol",
                )],
            }),
            r#"{"type":"send_prompt","text":"","peers":[{"from":"w1","summary":"picked up W2","body":"on the protocol"}]}"#,
        ),
        (
            serde_json::to_string(&Command::Steer {
                id: "steer-4".to_owned(),
                text: String::new(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: vec![team::PeerPayload::new(
                    "w2",
                    None,
                    Some("red".to_owned()),
                    "and I have it",
                )],
            }),
            r#"{"type":"steer","id":"steer-4","text":"","peers":[{"from":"w2","color":"red","body":"and I have it"}]}"#,
        ),
        (
            serde_json::to_string(&Event::SteerConsumed {
                session_id: pinned_session(),
                id: "steer-1".to_owned(),
            }),
            r#"{"type":"steer_consumed","session_id":"ses_1","id":"steer-1"}"#,
        ),
        (
            serde_json::to_string(&Event::CompactionProgress {
                session_id: pinned_session(),
                tokens: 2_500,
                budget: 4_096,
            }),
            r#"{"type":"compaction_progress","session_id":"ses_1","tokens":2500,"budget":4096}"#,
        ),
        (
            serde_json::to_string(&Command::CancelTurn),
            r#"{"type":"cancel_turn"}"#,
        ),
        (
            serde_json::to_string(&Command::RunShell {
                command: "git status".to_owned(),
            }),
            r#"{"type":"run_shell","command":"git status"}"#,
        ),
        (
            serde_json::to_string(&Command::RunCommand {
                name: "init".to_owned(),
                args: String::new(),
            }),
            r#"{"type":"run_command","name":"init","args":""}"#,
        ),
        (
            serde_json::to_string(&Command::Compact),
            r#"{"type":"compact"}"#,
        ),
        (
            serde_json::to_string(&Command::NewSession),
            r#"{"type":"new_session"}"#,
        ),
        (serde_json::to_string(&Command::Undo), r#"{"type":"undo"}"#),
        (serde_json::to_string(&Command::Redo), r#"{"type":"redo"}"#),
        // The rewind picker's command: an anchor and a scope, both
        // required — a rewind that had to guess which half of the
        // checkpoint the user meant would be a worse `/undo`.
        (
            serde_json::to_string(&Command::RevertTo {
                message_id: MessageId::from("msg_1".to_owned()),
                scope: RevertScope::Both,
            }),
            r#"{"type":"revert_to","message_id":"msg_1","scope":"both"}"#,
        ),
        (
            serde_json::to_string(&Command::RevertTo {
                message_id: MessageId::from("msg_1".to_owned()),
                scope: RevertScope::Conversation,
            }),
            r#"{"type":"revert_to","message_id":"msg_1","scope":"conversation"}"#,
        ),
        (
            serde_json::to_string(&Command::RevertTo {
                message_id: MessageId::from("msg_1".to_owned()),
                scope: RevertScope::Files,
            }),
            r#"{"type":"revert_to","message_id":"msg_1","scope":"files"}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Patch {
                    hash: "4b825dc".to_owned(),
                    files: vec!["src/main.rs".to_owned()],
                },
            }),
            r#"{"id":"prt_1","type":"patch","hash":"4b825dc","files":["src/main.rs"]}"#,
        ),
        (
            serde_json::to_string(&Event::RevertChanged {
                session_id: pinned_session(),
                revert: Some(RevertInfo {
                    message_id: MessageId::from("msg_1".to_owned()),
                    files: vec!["src/main.rs".to_owned()],
                }),
                prompt: Some("rename it".to_owned()),
            }),
            r#"{"type":"revert_changed","session_id":"ses_1","revert":{"message_id":"msg_1","files":["src/main.rs"]},"prompt":"rename it"}"#,
        ),
        (
            serde_json::to_string(&Event::RevertChanged {
                session_id: pinned_session(),
                revert: None,
                prompt: None,
            }),
            r#"{"type":"revert_changed","session_id":"ses_1"}"#,
        ),
        // A whole-file part's bytes are exactly what they were before
        // ranges existed: the pin is the None-direction half of the
        // compatibility promise.
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::File {
                    path: "src/main.rs".to_owned(),
                    mime: "text/plain".to_owned(),
                    start: None,
                    end: None,
                    content: None,
                },
            }),
            r#"{"id":"prt_1","type":"file","path":"src/main.rs","mime":"text/plain"}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::File {
                    path: "src/main.rs".to_owned(),
                    mime: "text/plain".to_owned(),
                    start: Some(12),
                    end: Some(40),
                    content: None,
                },
            }),
            r#"{"id":"prt_1","type":"file","path":"src/main.rs","mime":"text/plain","start":12,"end":40}"#,
        ),
        // The request's own copy of a binary attachment, after the
        // send-time read filled `content` in. A stored part never carries
        // it, but the shape is on this table so growing one is a
        // deliberate edit like every other.
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::File {
                    path: "shot.png".to_owned(),
                    mime: "image/png".to_owned(),
                    start: None,
                    end: None,
                    content: Some("aGk=".to_owned()),
                },
            }),
            r#"{"id":"prt_1","type":"file","path":"shot.png","mime":"image/png","content":"aGk="}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Reasoning {
                    provider: "openai".to_owned(),
                    item: "rs_1".to_owned(),
                    encrypted: Some("sealed".to_owned()),
                },
            }),
            // The part's own id and the provider's item id are two keys,
            // which is why the second is not called `id`.
            r#"{"id":"prt_1","type":"reasoning","provider":"openai","item":"rs_1","encrypted":"sealed"}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Reasoning {
                    provider: "openai".to_owned(),
                    item: "rs_1".to_owned(),
                    encrypted: None,
                },
            }),
            // State this build does not hold is written as its absence
            // rather than as a null, so the record says "there is none"
            // in the one spelling a reader also accepts from a build that
            // never wrote the field.
            r#"{"id":"prt_1","type":"reasoning","provider":"openai","item":"rs_1"}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Text {
                    text: "hi".to_owned(),
                },
            }),
            r#"{"id":"prt_1","type":"text","text":"hi"}"#,
        ),
        (
            serde_json::to_string(&Event::MessageStarted {
                session_id: pinned_session(),
                message: pinned_message(),
            }),
            r#"{"type":"message_started","session_id":"ses_1","message":{"id":"msg_1","role":"user","parts":[{"id":"prt_1","type":"text","text":"hi"}],"time":{"created":7,"completed":7}}}"#,
        ),
        (
            serde_json::to_string(&Event::PartStarted {
                session_id: pinned_session(),
                message_id: MessageId::from("msg_1".to_owned()),
                part: Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Text {
                        text: String::new(),
                    },
                },
            }),
            r#"{"type":"part_started","session_id":"ses_1","message_id":"msg_1","part":{"id":"prt_1","type":"text","text":""}}"#,
        ),
        (
            serde_json::to_string(&Event::PartDelta {
                session_id: pinned_session(),
                message_id: MessageId::from("msg_1".to_owned()),
                part_id: PartId::from("prt_1".to_owned()),
                delta: "hi".to_owned(),
            }),
            r#"{"type":"part_delta","session_id":"ses_1","message_id":"msg_1","part_id":"prt_1","delta":"hi"}"#,
        ),
        (
            serde_json::to_string(&Event::MessageFinished {
                session_id: pinned_session(),
                message_id: MessageId::from("msg_1".to_owned()),
                reason: FinishReason::Completed,
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    ..Usage::default()
                }),
                error: None,
                completed: 9,
            }),
            r#"{"type":"message_finished","session_id":"ses_1","message_id":"msg_1","reason":"completed","usage":{"input_tokens":1,"output_tokens":2,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0},"completed":9}"#,
        ),
        (
            serde_json::to_string(&Event::MessageFinished {
                session_id: pinned_session(),
                message_id: MessageId::from("msg_1".to_owned()),
                reason: FinishReason::Cancelled,
                usage: None,
                error: None,
                completed: 9,
            }),
            r#"{"type":"message_finished","session_id":"ses_1","message_id":"msg_1","reason":"cancelled","completed":9}"#,
        ),
        (
            serde_json::to_string(&Event::MessageFinished {
                session_id: pinned_session(),
                message_id: MessageId::from("msg_1".to_owned()),
                reason: FinishReason::Failed,
                usage: None,
                error: Some("no credentials".to_owned()),
                completed: 9,
            }),
            r#"{"type":"message_finished","session_id":"ses_1","message_id":"msg_1","reason":"failed","error":"no credentials","completed":9}"#,
        ),
        (
            serde_json::to_string(&Command::ReplyPermission {
                id: PermissionId::from("perm_1".to_owned()),
                reply: PermissionReply::Once,
            }),
            r#"{"type":"reply_permission","id":"perm_1","reply":"once"}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "read".to_owned(),
                    state: ToolState::Pending { input: None },
                },
            }),
            // Streaming-era pending: the settled-arguments field stays off
            // the wire entirely, which is also what keeps every row
            // written before it existed reading back (2026-08-15).
            r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"read","state":{"status":"pending"}}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "read".to_owned(),
                    state: ToolState::Pending {
                        input: Some(serde_json::json!({"path": "a.rs"})),
                    },
                },
            }),
            r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"read","state":{"status":"pending","input":{"path":"a.rs"}}}"#,
        ),
        (
            serde_json::to_string(&pinned_tool_part()),
            r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"read","state":{"status":"completed","input":{"path":"a.rs"},"output":"fn main() {}","title":"a.rs","metadata":{},"started":7,"completed":9}}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "shell".to_owned(),
                    state: ToolState::Error {
                        input: serde_json::json!({"command": "rm -rf /"}),
                        error: "refused".to_owned(),
                        started: 7,
                        completed: 9,
                    },
                },
            }),
            r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"shell","state":{"status":"error","input":{"command":"rm -rf /"},"error":"refused","started":7,"completed":9}}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::StepStart,
            }),
            r#"{"id":"prt_1","type":"step_start"}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::StepFinish {
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 2,
                        ..Usage::default()
                    },
                },
            }),
            r#"{"id":"prt_1","type":"step_finish","usage":{"input_tokens":1,"output_tokens":2,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0}}"#,
        ),
        (
            serde_json::to_string(&Event::PartUpdated {
                session_id: pinned_session(),
                message_id: MessageId::from("msg_1".to_owned()),
                part: Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Tool {
                        call_id: "call_1".to_owned(),
                        tool: "shell".to_owned(),
                        state: ToolState::Running {
                            input: serde_json::json!({"command": "ls"}),
                            metadata: serde_json::Value::Null,
                            started: 7,
                        },
                    },
                },
            }),
            r#"{"type":"part_updated","session_id":"ses_1","message_id":"msg_1","part":{"id":"prt_1","type":"tool","call_id":"call_1","tool":"shell","state":{"status":"running","input":{"command":"ls"},"started":7}}}"#,
        ),
        // A call that reports progress while it runs — the `!` passthrough
        // streaming its output, or a task tool watching a subagent.
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "bash".to_owned(),
                    state: ToolState::Running {
                        input: serde_json::json!({"command": "ls"}),
                        metadata: serde_json::json!({"output": "a.rs\n"}),
                        started: 7,
                    },
                },
            }),
            r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"bash","state":{"status":"running","input":{"command":"ls"},"metadata":{"output":"a.rs\n"},"started":7}}"#,
        ),
        // A call that stays inside the checkout, whose `directories` are
        // absent from the wire exactly as they were when the field
        // arrived.
        (
            serde_json::to_string(&Event::PermissionRequested {
                session_id: pinned_session(),
                id: PermissionId::from("perm_1".to_owned()),
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                title: "ls".to_owned(),
                args: serde_json::json!({"command": "ls"}),
                directories: Vec::new(),
            }),
            r#"{"type":"permission_requested","session_id":"ses_1","id":"perm_1","call_id":"call_1","tool":"shell","title":"ls","args":{"command":"ls"}}"#,
        ),
        (
            serde_json::to_string(&Event::PermissionRequested {
                session_id: pinned_session(),
                id: PermissionId::from("perm_1".to_owned()),
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                title: "ls /etc".to_owned(),
                args: serde_json::json!({"command": "ls /etc"}),
                directories: vec!["/etc".to_owned(), "/tmp/scratch".to_owned()],
            }),
            r#"{"type":"permission_requested","session_id":"ses_1","id":"perm_1","call_id":"call_1","tool":"shell","title":"ls /etc","args":{"command":"ls /etc"},"directories":["/etc","/tmp/scratch"]}"#,
        ),
        (
            serde_json::to_string(&Event::PermissionReplied {
                session_id: pinned_session(),
                id: PermissionId::from("perm_1".to_owned()),
                reply: PermissionReply::Reject,
            }),
            r#"{"type":"permission_replied","session_id":"ses_1","id":"perm_1","reply":"reject"}"#,
        ),
        // A question with everything absent that may be absent: the two
        // optional flags the model did not send, and no call to name.
        (
            serde_json::to_string(&Event::QuestionAsked {
                session_id: pinned_session(),
                id: QuestionId::from("que_1".to_owned()),
                questions: vec![QuestionInfo {
                    question: "Which database?".to_owned(),
                    header: "Database".to_owned(),
                    options: vec![QuestionOption {
                        label: "Postgres".to_owned(),
                        description: "Relational, what the rest of the fleet runs".to_owned(),
                    }],
                    multiple: None,
                    custom: None,
                }],
                source: None,
            }),
            r#"{"type":"question_asked","session_id":"ses_1","id":"que_1","questions":[{"question":"Which database?","header":"Database","options":[{"label":"Postgres","description":"Relational, what the rest of the fleet runs"}]}]}"#,
        ),
        // And the same question with every optional field carried.
        (
            serde_json::to_string(&Event::QuestionAsked {
                session_id: pinned_session(),
                id: QuestionId::from("que_1".to_owned()),
                questions: vec![QuestionInfo {
                    question: "Which database?".to_owned(),
                    header: "Database".to_owned(),
                    options: Vec::new(),
                    multiple: Some(true),
                    custom: Some(false),
                }],
                source: Some(QuestionSource {
                    message_id: MessageId::from("msg_1".to_owned()),
                    call_id: "call_1".to_owned(),
                }),
            }),
            r#"{"type":"question_asked","session_id":"ses_1","id":"que_1","questions":[{"question":"Which database?","header":"Database","options":[],"multiple":true,"custom":false}],"source":{"message_id":"msg_1","call_id":"call_1"}}"#,
        ),
        (
            serde_json::to_string(&Event::QuestionReplied {
                session_id: pinned_session(),
                id: QuestionId::from("que_1".to_owned()),
                answers: vec![vec!["Postgres".to_owned()], Vec::new()],
            }),
            r#"{"type":"question_replied","session_id":"ses_1","id":"que_1","answers":[["Postgres"],[]]}"#,
        ),
        // Rejection carries its own payload — no `answers` field, because
        // a dismissal is not an answer.
        (
            serde_json::to_string(&Event::QuestionRejected {
                session_id: pinned_session(),
                id: QuestionId::from("que_1".to_owned()),
            }),
            r#"{"type":"question_rejected","session_id":"ses_1","id":"que_1"}"#,
        ),
        // Agent adoption announces both values because choosing an agent
        // may also choose its preferred model.
        (
            serde_json::to_string(&Event::AgentChanged {
                session_id: pinned_session(),
                agent: "build".to_owned(),
                model: "claude-sonnet-4-5".to_owned(),
            }),
            r#"{"type":"agent_changed","session_id":"ses_1","agent":"build","model":"claude-sonnet-4-5"}"#,
        ),
        (
            serde_json::to_string(&Command::ReplyQuestion {
                id: QuestionId::from("que_1".to_owned()),
                answers: vec![vec!["Postgres".to_owned()]],
            }),
            r#"{"type":"reply_question","id":"que_1","answers":[["Postgres"]]}"#,
        ),
        (
            serde_json::to_string(&Command::RejectQuestion {
                id: QuestionId::from("que_1".to_owned()),
            }),
            r#"{"type":"reject_question","id":"que_1"}"#,
        ),
        (
            serde_json::to_string(&Command::SwitchAgent {
                name: "plan".to_owned(),
            }),
            r#"{"type":"switch_agent","name":"plan"}"#,
        ),
        (
            serde_json::to_string(&Command::SwitchModel {
                model: "claude-haiku-4.5".to_owned(),
            }),
            r#"{"type":"switch_model","model":"claude-haiku-4.5"}"#,
        ),
        // The effort travels only when there is one: `None` is upstream's
        // "Default", and both the command that asks for it and the event
        // that announces it spell that as the field's absence.
        (
            serde_json::to_string(&Command::SwitchEffort {
                effort: Some("max".to_owned()),
            }),
            r#"{"type":"switch_effort","effort":"max"}"#,
        ),
        (
            serde_json::to_string(&Command::SwitchEffort { effort: None }),
            r#"{"type":"switch_effort"}"#,
        ),
        (
            serde_json::to_string(&Event::EffortChanged {
                session_id: pinned_session(),
                effort: Some("max".to_owned()),
            }),
            r#"{"type":"effort_changed","session_id":"ses_1","effort":"max"}"#,
        ),
        (
            serde_json::to_string(&Event::EffortChanged {
                session_id: pinned_session(),
                effort: None,
            }),
            r#"{"type":"effort_changed","session_id":"ses_1"}"#,
        ),
        // The posture a lead's `mode_set_request` ends up as, and the
        // acceptance that answers it. Two names, spelled as this crate
        // spells every other enum on the wire.
        (
            serde_json::to_string(&Command::SetPermissionMode {
                mode: PermissionMode::Bypass,
            }),
            r#"{"type":"set_permission_mode","mode":"bypass"}"#,
        ),
        (
            serde_json::to_string(&Command::SetPermissionMode {
                mode: PermissionMode::Ask,
            }),
            r#"{"type":"set_permission_mode","mode":"ask"}"#,
        ),
        (
            serde_json::to_string(&Event::PermissionModeChanged {
                session_id: pinned_session(),
                mode: PermissionMode::Bypass,
            }),
            r#"{"type":"permission_mode_changed","session_id":"ses_1","mode":"bypass"}"#,
        ),
        // The admission surface: both settlement decisions, then a hold
        // in its two wire shapes — an explicit cause carrying its source
        // and the sender's summary, which installs no timer, and the
        // collapsed-parity cause a timer and no summary write — then the
        // settlement that retires one.
        (
            serde_json::to_string(&Command::SettleHeld {
                id: HeldId::from("held_1".to_owned()),
                decision: HeldDecision::Release,
            }),
            r#"{"type":"settle_held","id":"held_1","decision":"release"}"#,
        ),
        (
            serde_json::to_string(&Command::SettleHeld {
                id: HeldId::from("held_1".to_owned()),
                decision: HeldDecision::Deny,
            }),
            r#"{"type":"settle_held","id":"held_1","decision":"deny"}"#,
        ),
        (
            serde_json::to_string(&Event::PeerHeld {
                session_id: pinned_session(),
                id: HeldId::from("held_1".to_owned()),
                from: "w1@inbound".to_owned(),
                cause: HoldCause::Explicit {
                    source: PolicySource::Global,
                },
                summary: Some(RedactedText::from("CI is red".to_owned())),
                preview: RedactedText::from("CI is red on main".to_owned()),
                expires_in_ms: None,
            }),
            r#"{"type":"peer_held","session_id":"ses_1","id":"held_1","from":"w1@inbound","cause":{"kind":"explicit","source":"global"},"summary":"CI is red","preview":"CI is red on main"}"#,
        ),
        (
            serde_json::to_string(&Event::PeerHeld {
                session_id: pinned_session(),
                id: HeldId::from("held_1".to_owned()),
                from: "w1@inbound".to_owned(),
                cause: HoldCause::NoModeAsserted,
                summary: None,
                preview: RedactedText::from("done".to_owned()),
                expires_in_ms: Some(300_000),
            }),
            r#"{"type":"peer_held","session_id":"ses_1","id":"held_1","from":"w1@inbound","cause":{"kind":"no_mode_asserted"},"preview":"done","expires_in_ms":300000}"#,
        ),
        (
            serde_json::to_string(&Event::PeerHoldSettled {
                session_id: pinned_session(),
                id: HeldId::from("held_1".to_owned()),
                outcome: HeldOutcome::Expired,
            }),
            r#"{"type":"peer_hold_settled","session_id":"ses_1","id":"held_1","outcome":"expired"}"#,
        ),
        // A peer's words, richest form first: both display fields
        // present, then the shape a message with neither writes — a
        // sender that wrote no summary and a member with no color assigned
        // put no keys on the wire at all.
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Peer {
                    from: "w1".to_owned(),
                    summary: Some("picked up W2".to_owned()),
                    color: Some("blue".to_owned()),
                    body: "starting on the protocol surface".to_owned(),
                },
            }),
            r#"{"id":"prt_1","type":"peer","from":"w1","summary":"picked up W2","color":"blue","body":"starting on the protocol surface"}"#,
        ),
        (
            serde_json::to_string(&Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Peer {
                    from: "w1".to_owned(),
                    summary: None,
                    color: None,
                    body: "done".to_owned(),
                },
            }),
            r#"{"id":"prt_1","type":"peer","from":"w1","body":"done"}"#,
        ),
    ];

    for (encoded, expected) in cases {
        assert_eq!(encoded.expect("the value serializes"), expected);
    }
}

/// Pins the spelling of every hold cause, policy source, decision and
/// outcome — the whole admission vocabulary — and that each reads back as
/// itself, so a dialog that switches on a cause and a store that replays a
/// settlement can never drift out from under the wire.
#[test]
fn the_hold_vocabulary_spells_every_variant_and_round_trips() {
    let causes = [
        (
            HoldCause::Explicit {
                source: PolicySource::Global,
            },
            r#"{"kind":"explicit","source":"global"}"#,
        ),
        (
            HoldCause::Explicit {
                source: PolicySource::ExplicitFile,
            },
            r#"{"kind":"explicit","source":"explicit_file"}"#,
        ),
        (
            HoldCause::Explicit {
                source: PolicySource::Project,
            },
            r#"{"kind":"explicit","source":"project"}"#,
        ),
        (HoldCause::ModeMismatch, r#"{"kind":"mode_mismatch"}"#),
        (HoldCause::NoModeAsserted, r#"{"kind":"no_mode_asserted"}"#),
        (HoldCause::ModeUnknown, r#"{"kind":"mode_unknown"}"#),
    ];
    for (cause, expected) in causes {
        let encoded = serde_json::to_string(&cause).expect("a cause serializes");
        assert_eq!(encoded, expected);
        let decoded: HoldCause = serde_json::from_str(&encoded).expect("a cause deserializes");
        assert_eq!(decoded, cause);
    }

    let decisions = [
        (HeldDecision::Release, r#""release""#),
        (HeldDecision::Deny, r#""deny""#),
    ];
    for (decision, expected) in decisions {
        let encoded = serde_json::to_string(&decision).expect("a decision serializes");
        assert_eq!(encoded, expected);
        let decoded: HeldDecision =
            serde_json::from_str(&encoded).expect("a decision deserializes");
        assert_eq!(decoded, decision);
    }

    let outcomes = [
        (HeldOutcome::Delivered, r#""delivered""#),
        (HeldOutcome::Denied, r#""denied""#),
        (HeldOutcome::Expired, r#""expired""#),
    ];
    for (outcome, expected) in outcomes {
        let encoded = serde_json::to_string(&outcome).expect("an outcome serializes");
        assert_eq!(encoded, expected);
        let decoded: HeldOutcome = serde_json::from_str(&encoded).expect("an outcome deserializes");
        assert_eq!(decoded, outcome);
    }
}

/// The redaction is the type's, never a caller's discipline: a debugged
/// hold prints each body's size and none of its words — through the
/// event's derived `Debug` too — while serde carries the words untouched
/// for the dialogs that exist to show them.
#[test]
fn a_held_body_debugs_as_a_size_and_never_the_text() {
    let body = "the sender's own sentence";
    let preview = RedactedText::from(body.to_owned());

    let debugged = format!("{preview:?}");
    assert_eq!(debugged, format!("<{} bytes>", body.len()));
    assert!(
        !debugged.contains(body),
        "a debug rendering must never carry the text: {debugged}"
    );

    let event = Event::PeerHeld {
        session_id: pinned_session(),
        id: HeldId::from("held_1".to_owned()),
        from: "w1@inbound".to_owned(),
        cause: HoldCause::ModeUnknown,
        summary: Some(RedactedText::from(body.to_owned())),
        preview: preview.clone(),
        expires_in_ms: None,
    };
    let debugged = format!("{event:?}");
    assert!(
        !debugged.contains(body),
        "the event's derived debug must inherit the redaction: {debugged}"
    );

    let encoded = serde_json::to_string(&preview).expect("the text serializes");
    assert_eq!(encoded, format!("\"{body}\""));
    let decoded: RedactedText = serde_json::from_str(&encoded).expect("the text deserializes");
    assert_eq!(decoded.as_str(), body);
}

/// The shape every frontend written before mentions existed sends. It has
/// to keep parsing, and it has to keep meaning "no files attached" rather
/// than failing on a field that is not there.
#[test]
fn a_prompt_written_before_mentions_existed_still_parses() {
    let decoded: Command = serde_json::from_str(r#"{"type":"send_prompt","text":"hi"}"#)
        .expect("the original shape parses");

    assert_eq!(
        decoded,
        Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        }
    );

    let decoded: Command =
        serde_json::from_str(r#"{"type":"send_prompt","text":"hi","mentions":[{"path":"a.rs"}]}"#)
            .expect("the new shape parses");
    assert_eq!(
        decoded,
        Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: vec![Mention {
                path: "a.rs".to_owned(),
                start: None,
                end: None,
            }],
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        }
    );
}

/// A steer's payload is a prompt's, so it keeps a prompt's absence rule in
/// both directions: a command written without the key reads back as
/// "nothing attached" rather than failing, and one written with it reads
/// back whole.
#[test]
fn a_steer_without_mentions_parses_as_one_with_nothing_attached() {
    let decoded: Command =
        serde_json::from_str(r#"{"type":"steer","id":"steer-1","text":"use the other file"}"#)
            .expect("the mention-free shape parses");

    assert_eq!(
        decoded,
        Command::Steer {
            id: "steer-1".to_owned(),
            text: "use the other file".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        }
    );

    let decoded: Command = serde_json::from_str(
        r#"{"type":"steer","id":"steer-2","text":"this one","mentions":[{"path":"a.rs"}]}"#,
    )
    .expect("the attached shape parses");
    assert_eq!(
        decoded,
        Command::Steer {
            id: "steer-2".to_owned(),
            text: "this one".to_owned(),
            mentions: vec![Mention {
                path: "a.rs".to_owned(),
                start: None,
                end: None,
            }],
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        }
    );
}

/// The rewind command's other direction: every scope reads back off the
/// wire as the one that was written, and no scope is optional — a
/// `revert_to` without one is a rewind that never said what to restore, and
/// failing is the only honest answer to it.
#[test]
fn a_rewind_reads_back_the_scope_it_was_written_with() {
    let cases = [
        (
            r#"{"type":"revert_to","message_id":"msg_1","scope":"both"}"#,
            RevertScope::Both,
        ),
        (
            r#"{"type":"revert_to","message_id":"msg_1","scope":"conversation"}"#,
            RevertScope::Conversation,
        ),
        (
            r#"{"type":"revert_to","message_id":"msg_1","scope":"files"}"#,
            RevertScope::Files,
        ),
    ];

    for (encoded, scope) in cases {
        let decoded: Command = serde_json::from_str(encoded).expect("the shape parses");
        assert_eq!(
            decoded,
            Command::RevertTo {
                message_id: MessageId::from("msg_1".to_owned()),
                scope,
            }
        );
    }

    assert!(
        serde_json::from_str::<Command>(r#"{"type":"revert_to","message_id":"msg_1"}"#).is_err(),
        "a rewind with no scope names nothing to restore"
    );
    assert!(
        serde_json::from_str::<Command>(
            r#"{"type":"revert_to","message_id":"msg_1","scope":"code"}"#
        )
        .is_err(),
        "a scope this build does not have is refused rather than guessed at"
    );
}

/// The two questions the engine asks a scope, answered here so that a
/// fourth variant cannot be added without deciding both.
#[test]
fn a_scope_says_which_halves_of_a_checkpoint_it_puts_back() {
    for scope in [
        RevertScope::Both,
        RevertScope::Conversation,
        RevertScope::Files,
    ] {
        let (files, conversation) = match scope {
            RevertScope::Both => (true, true),
            RevertScope::Conversation => (false, true),
            RevertScope::Files => (true, false),
        };

        assert_eq!(scope.touches_files(), files, "{scope:?}");
        assert_eq!(scope.touches_conversation(), conversation, "{scope:?}");
    }
}

/// The event a queue entry is retired by, read back off the wire: its id
/// is the frontend's own string and travels unchanged, because a frontend
/// matching on anything else would be matching on a guess.
#[test]
fn a_consumed_steer_reads_back_naming_the_id_the_command_carried() {
    let decoded: Event =
        serde_json::from_str(r#"{"type":"steer_consumed","session_id":"ses_1","id":"steer-1"}"#)
            .expect("the event parses");

    assert_eq!(decoded.session_id(), &pinned_session());
    assert_eq!(
        decoded,
        Event::SteerConsumed {
            session_id: pinned_session(),
            id: "steer-1".to_owned(),
        }
    );
}

/// The other direction of the range extension's promise: a file part
/// stored before ranges existed reads back as a whole-file reference, and
/// a mention written by an older frontend reads back range-free — neither
/// fails on fields that are not there.
#[test]
fn a_file_part_written_before_ranges_existed_still_parses() {
    let decoded: Part = serde_json::from_str(
        r#"{"id":"prt_1","type":"file","path":"src/main.rs","mime":"text/plain"}"#,
    )
    .expect("the original shape parses");

    assert_eq!(
        decoded.body,
        PartBody::File {
            path: "src/main.rs".to_owned(),
            mime: "text/plain".to_owned(),
            start: None,
            end: None,
            content: None,
        }
    );

    let decoded: Mention =
        serde_json::from_str(r#"{"path":"a.rs"}"#).expect("a range-free mention parses");
    assert_eq!(
        decoded,
        Mention {
            path: "a.rs".to_owned(),
            start: None,
            end: None,
        }
    );
}

/// A running part written before it could report progress still parses,
/// and reads back as one that reports nothing.
#[test]
fn a_running_part_without_metadata_still_parses() {
    let decoded: Part = serde_json::from_str(
            r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"bash","state":{"status":"running","input":{},"started":7}}"#,
        )
        .expect("the original shape parses");

    let PartBody::Tool {
        state: ToolState::Running { metadata, .. },
        ..
    } = decoded.body
    else {
        panic!("the fixture is a running tool part");
    };
    assert!(metadata.is_null());
}

/// A stored assistant message keeps its model and usage; a stored user
/// message keeps neither, and reading one back does not invent them.
#[test]
fn an_assistant_message_round_trips_with_its_model_and_usage() {
    let mut message = Message::assistant("canned");
    message.parts.push(Part::text("hello"));
    message.usage = Some(Usage {
        input_tokens: 1,
        output_tokens: 2,
        ..Usage::default()
    });
    message.complete();

    let encoded = serde_json::to_string(&message).expect("a message serializes");
    let decoded: Message = serde_json::from_str(&encoded).expect("a message deserializes");

    assert_eq!(decoded, message, "round trip changed {encoded}");

    let user = Message::user("hi");
    let encoded = serde_json::to_string(&user).expect("a message serializes");
    assert!(
        !encoded.contains("model") && !encoded.contains("usage"),
        "a user message should carry neither: {encoded}"
    );
}

/// The tag is the whole of the downgrade contract: a reader too old to
/// decode the record recognizes it by this prefix and nothing else, so a
/// rename would silently turn every future reasoning record into a part
/// that vanishes without a trace.
#[test]
fn a_reasoning_part_is_tagged_with_the_prefix_a_later_variant_must_keep() {
    let part = Part::reasoning("openai", "rs_1", Some("sealed".to_owned()));
    let encoded = serde_json::to_value(&part).expect("a part serializes");

    assert_eq!(encoded["type"], serde_json::json!(REASONING_TAG));
    assert!(
        encoded["type"]
            .as_str()
            .is_some_and(|tag| tag.starts_with(REASONING_TAG)),
        "the reserved prefix is what a decoder that cannot read the rest \
             still matches on: {encoded}"
    );
}

/// The readable half honors the same contract, which is the whole of what
/// makes it safe to add: a build too old to decode one still recognizes
/// the record as reasoning and leaves storage's marker in its place rather
/// than dropping the row without a word.
#[test]
fn readable_thinking_keeps_the_reserved_prefix_too() {
    let part = Part::reasoning_text("weighing a greeting");
    let encoded = serde_json::to_value(&part).expect("a part serializes");

    assert_eq!(encoded["type"], serde_json::json!("reasoning_text"));
    assert!(
        encoded["type"]
            .as_str()
            .is_some_and(|tag| tag.starts_with(REASONING_TAG)),
        "an older reader matches this record on the prefix alone: {encoded}"
    );

    let decoded: Part = serde_json::from_value(encoded).expect("what it wrote is what it reads");
    assert_eq!(decoded.body, part.body, "round trip changed the part");
}

/// Thinking is not the reply, and the accessors are where that is
/// enforced: `as_text` is what titles a checkpoint and what the copy
/// surfaces read, and thinking answering it would put the model's scratch
/// paper where its answer belongs.
#[test]
fn thinking_is_its_own_body_and_never_reply_text() {
    let mut thinking = Part::reasoning_text("weighing a greeting");
    let mut reply = Part::text("hello");

    assert!(
        matches!(&thinking.body, PartBody::ReasoningText { text } if text == "weighing a greeting")
    );
    assert_eq!(thinking.as_text(), None, "thinking is not the reply");
    assert!(thinking.as_text_mut().is_none());
    assert!(
        matches!(&reply.body, PartBody::Text { .. }),
        "and the reply is not thinking"
    );

    // The one accessor that spans both, because a delta names an id and a
    // fragment and never which of the two it is growing.
    for part in [&mut thinking, &mut reply] {
        part.streamed_mut()
            .expect("both kinds of text grow by delta")
            .push('!');
    }
    assert!(
        matches!(&thinking.body, PartBody::ReasoningText { text } if text == "weighing a greeting!")
    );
    assert_eq!(reply.as_text(), Some("hello!"));

    assert!(
        Part::reasoning("openai", "rs_1", Some("sealed".to_owned()))
            .streamed_mut()
            .is_none(),
        "a sealed blob is bytes, not text a fragment could be appended to"
    );
}

/// A turn that thought and then died said nothing: no wire carries
/// thinking, so a message holding only that would enter the history as an
/// assistant turn with no content at all.
#[test]
fn a_message_holding_only_thinking_has_no_content() {
    let mut message = Message::assistant("canned");
    message.parts.push(Part::reasoning_text("weighing it"));

    assert!(!message.has_content());

    message.parts.push(Part::text("hello"));
    assert!(message.has_content(), "the reply beside it is content");
}

/// The two shapes a reader has to accept, and the one it must never
/// invent: a record whose state field was never written reads back as
/// state this build does not hold, not as an empty blob it could send.
#[test]
fn a_reasoning_record_without_state_reads_back_as_state_nobody_holds() {
    let decoded: Part = serde_json::from_str(
        r#"{"id":"prt_1","type":"reasoning","provider":"openai","item":"rs_1"}"#,
    )
    .expect("a record written without the field parses");

    assert_eq!(
        decoded.body,
        PartBody::Reasoning {
            provider: "openai".to_owned(),
            item: "rs_1".to_owned(),
            encrypted: None,
        }
    );

    let held: Part = serde_json::from_str(
            r#"{"id":"prt_1","type":"reasoning","provider":"openai","item":"rs_1","encrypted":"sealed"}"#,
        )
        .expect("a record written with the field parses");
    assert_eq!(
        held.body,
        PartBody::Reasoning {
            provider: "openai".to_owned(),
            item: "rs_1".to_owned(),
            encrypted: Some("sealed".to_owned()),
        }
    );
}

/// Sealed state is not something the model said. A turn that produced only
/// this is a turn that produced nothing, and letting it into the history
/// would carry an unreplayable blob into every later request.
#[test]
fn a_message_holding_only_sealed_reasoning_has_no_content() {
    let mut message = Message::assistant("gpt");
    message
        .parts
        .push(Part::reasoning("openai", "rs_1", Some("sealed".to_owned())));

    assert!(!message.has_content());
    assert!(
        message.parts[0].as_text().is_none(),
        "nothing renders sealed state, so nothing may read text out of it"
    );

    message.parts.push(Part::text("and here is the answer"));
    assert!(message.has_content());
}

/// AC-23's protocol half. A teammate's words are carried, drawn and sent —
/// and are still not this session's text, because `as_text` is what titles
/// a checkpoint and answers the copy surfaces, and what a peer said is not
/// what this conversation said.
#[test]
fn a_peer_part_is_not_text() {
    let mut part = Part::peer(
        "w1",
        Some("picked up W2".to_owned()),
        Some("blue".to_owned()),
        "starting on the protocol surface",
    );

    assert_eq!(part.as_text(), None, "a peer's words are not the reply");
    assert!(part.as_text_mut().is_none());
    assert!(
        part.streamed_mut().is_none(),
        "a mailbox delivers a message whole; no delta ever names this part"
    );

    // The constructor's four arguments land where the field order says,
    // which is worth pinning because two of them are adjacent options.
    assert_eq!(
        part.body,
        PartBody::Peer {
            from: "w1".to_owned(),
            summary: Some("picked up W2".to_owned()),
            color: Some("blue".to_owned()),
            body: "starting on the protocol surface".to_owned(),
        }
    );

    let encoded = serde_json::to_string(&part).expect("a part serializes");
    let decoded: Part = serde_json::from_str(&encoded).expect("a part deserializes");
    assert_eq!(decoded.body, part.body, "round trip changed {encoded}");
}

/// The cap belongs to whoever builds the part, and the part keeps what it
/// was built with: a decoded record that read back shorter than it was
/// written would be a store that quietly rewrote somebody's message.
#[test]
fn a_peer_part_keeps_the_summary_it_was_built_with() {
    let long = "e".repeat(team::DISPLAY_FIELD_CAP * 2);

    // Handed a summary nobody capped, the part carries it as given — the
    // type states where the cap lives rather than applying it twice.
    let uncapped = Part::peer("w1", Some(long.clone()), None, "hi");
    let PartBody::Peer { summary, .. } = &uncapped.body else {
        unreachable!("the constructor built a peer part")
    };
    assert_eq!(summary.as_deref().map(str::len), Some(long.len()));
}

/// The wire's own door to that part caps on the way through — which is
/// what keeps a sender from spending the context window on a display
/// field by writing a long one.
#[test]
fn a_peer_payload_becomes_a_part_through_the_capping_constructor() {
    let long = "e".repeat(team::DISPLAY_FIELD_CAP * 2);
    let part = team::PeerPayload::new(
        "w1",
        Some(long.clone()),
        Some("blue".to_owned()),
        long.clone(),
    )
    .into_part();

    let PartBody::Peer {
        from,
        summary,
        color,
        body,
    } = &part.body
    else {
        unreachable!("a payload becomes a peer part and nothing else")
    };
    assert_eq!(from, "w1");
    assert_eq!(
        summary.as_deref().map(str::len),
        Some(team::DISPLAY_FIELD_CAP),
        "the display field is capped on the way into the part"
    );
    assert_eq!(color.as_deref(), Some("blue"), "the color travels as given");
    assert_eq!(body.len(), long.len(), "and the message itself is whole");
}

/// The one display-shaped part that is content: the request assembly
/// renders it into the user turn, so a message carrying only a teammate's
/// words is a message the model was told — where a message carrying only
/// thinking is not.
#[test]
fn a_message_carrying_only_a_peers_words_has_content() {
    let message = Message {
        id: MessageId::from("msg_1".to_owned()),
        role: Role::User,
        parts: vec![Part::peer("w1", None, None, "done")],
        time: MessageTime {
            created: 7,
            completed: Some(7),
        },
        model: None,
        usage: None,
    };

    assert!(message.has_content());
}

/// Claude Code's four names against ganja's two, refusals included: `plan`
/// is refused because this build already has that switch as an agent, and
/// an unrecognized name is refused as itself rather than quietly becoming
/// the safe value — a posture a sender asked for and did not get is
/// something it has to be told.
#[test]
fn claudes_mode_names_map_to_ganjas_two_or_are_refused_by_name() {
    assert_eq!(
        PermissionMode::from_claude_name("bypassPermissions"),
        Ok(PermissionMode::Bypass)
    );
    assert_eq!(
        PermissionMode::from_claude_name("default"),
        Ok(PermissionMode::Ask)
    );
    assert_eq!(
        PermissionMode::from_claude_name("acceptEdits"),
        Ok(PermissionMode::Ask)
    );

    assert_eq!(
        PermissionMode::from_claude_name("plan"),
        Err(UnknownPermissionMode::Plan)
    );
    assert_eq!(
        PermissionMode::from_claude_name("plan")
            .unwrap_err()
            .to_string(),
        "plan is an agent here, not a permission mode: switch to it with /agent plan"
    );

    // Casing is somebody else's, so nothing here guesses at it: the four
    // names are matched exactly and everything else is named back.
    for name in ["bypasspermissions", "accept_edits", "", "ask"] {
        assert_eq!(
            PermissionMode::from_claude_name(name),
            Err(UnknownPermissionMode::Unknown(name.to_owned())),
            "{name} is not one of the four"
        );
    }
    assert_eq!(
        PermissionMode::from_claude_name("ask")
            .unwrap_err()
            .to_string(),
        "ask is not a permission mode this build knows: it takes \
             bypassPermissions, default or acceptEdits"
    );
}
