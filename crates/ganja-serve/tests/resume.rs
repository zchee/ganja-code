//! Resume from retained cursors, declared gaps and the permission projection barrier.

mod support;

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::StreamExt as _;
use ganja_core::permission::{Action, Permissions, Rule};
use ganja_core::provider::ProviderEvent;
use ganja_core::tool::Registry;
use ganja_core::{Engine, Storage};
use ganja_protocol::{Command, Event, FinishReason, Role};
use ganja_serve::{EventJournal, Handle};
use ganja_testkit::{RecorderTool, says, tool_call};
use support::{DEADLINE, Frame, base_url, loopback_config};

fn stored_engine() -> (Arc<Engine>, Storage, tempfile::TempDir) {
    let data = ganja_testkit::temp_dir();
    let storage = Storage::open(data.path().join("storage"));
    let mut script =
        vec![ProviderEvent::TextDelta("x".into()); ganja_core::engine::EVENT_CAPACITY + 32];
    script.push(ProviderEvent::Finish(FinishReason::Completed));
    let (provider, _) = ganja_testkit::ScriptedProvider::named("fake", vec![script]);
    let engine = Engine::persistent(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage.clone(),
    );
    (Arc::new(engine), storage, data)
}

fn prompt() -> Command {
    Command::SendPrompt {
        text: "hello".into(),
        mentions: Vec::new(),
        skills: Vec::new(),
        peers: Vec::new(),
        session_mentions: Vec::new(),
    }
}

#[tokio::test]
async fn a_saved_completed_envelope_can_precede_its_finish_at_the_fresh_head() {
    // Measure the event shape using the same fixture, then leave precisely one
    // queue's worth of pre-finish events unread. No production test hook is needed.
    let (reference, _, _reference_data) = stored_engine();
    let mut reference_events = reference.subscribe().await.expect("reference subscription");
    reference.send(prompt()).await.expect("reference prompt");
    let expected = ganja_testkit::drain(&mut reference_events).await;
    assert!(matches!(expected.last(), Some(Event::MessageFinished { .. })));
    let before_finish = expected.len() - 1;
    assert!(before_finish >= ganja_core::engine::EVENT_CAPACITY);

    let (engine, storage, _data) = stored_engine();
    let mut blocked = engine.subscribe().await.expect("first subscription");
    let journal = EventJournal::new();
    let mut config = loopback_config();
    config.storage = Some(storage.clone());
    config.journal = journal.clone();
    let handle = ganja_serve::serve(engine.clone(), config).await.expect("serve");
    engine.send(prompt()).await.expect("prompt");
    for _ in 0..before_finish - ganja_core::engine::EVENT_CAPACITY {
        blocked.next().await.expect("pre-finish event");
    }
    let session = engine.session_id();
    let completed = tokio::time::timeout(DEADLINE, async {
        loop {
            let transcript = storage.load_transcript(&session).expect("stored transcript");
            if let Some(message) = transcript
                .into_iter()
                .find(|message| message.role == Role::Assistant && message.time.completed.is_some())
            {
                break message;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("envelope saved while finish is blocked");
    let mut fresh = Frames::connect(&handle, None).await;
    let connected = fresh.next().await;
    let head = connected.id.rsplit_once('-').expect("cursor").1.parse::<usize>().expect("sequence");
    assert_eq!(head, before_finish, "finish is not in the journal yet");
    let stored: Vec<ganja_protocol::Message> =
        reqwest::get(format!("{}/session/{}/message", base_url(&handle), session.as_str()))
            .await
            .expect("transcript route")
            .json()
            .await
            .expect("transcript JSON");
    assert!(
        stored.iter().any(|message| message.id == completed.id && message.time.completed.is_some())
    );
    let text: String = completed.parts.iter().filter_map(|part| part.as_text()).collect();
    assert_eq!(text, "x".repeat(ganja_core::engine::EVENT_CAPACITY + 32));
    drop(blocked);
    let finish = fresh.next().await;
    assert_eq!(finish.event, "message");
    assert!(
        matches!(serde_json::from_str::<Event>(&finish.data).expect("finish JSON"), Event::MessageFinished { message_id, .. } if message_id == completed.id)
    );
    drop(fresh);
    handle.shutdown().await.expect("stop");
}

#[tokio::test]
async fn dirty_text_before_a_gap_is_recovered_by_reading_after_the_finish() {
    let (engine, storage, _data) = stored_engine();
    let blocked = engine.subscribe().await.expect("first subscription");
    let journal = EventJournal::new();
    let mut config = loopback_config();
    config.storage = Some(storage.clone());
    config.journal = journal.clone();
    let handle = ganja_serve::serve(engine.clone(), config).await.expect("serve");
    engine.send(prompt()).await.expect("prompt");
    tokio::time::timeout(DEADLINE, async {
        while journal.head().rsplit_once('-').expect("cursor").1.parse::<usize>().expect("sequence")
            < ganja_core::engine::EVENT_CAPACITY
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queue is full");
    let mut gap = Frames::connect(&handle, Some(&EventJournal::new().head())).await;
    let connected = gap.next().await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&connected.data).expect("JSON")["replay"],
        "gap"
    );
    drop(gap);
    let mut fresh = Frames::connect(&handle, None).await;
    fresh.next().await;
    let before = storage.load_transcript(&engine.session_id()).expect("partial transcript");
    let partial = before.iter().find(|message| message.role == Role::Assistant).expect("assistant");
    assert!(partial.time.completed.is_none());
    let partial_text: String = partial.parts.iter().filter_map(|part| part.as_text()).collect();
    assert!(partial_text.len() < ganja_core::engine::EVENT_CAPACITY + 32);
    drop(blocked);
    fresh.finish().await;
    let after: Vec<ganja_protocol::Message> = reqwest::get(format!(
        "{}/session/{}/message",
        base_url(&handle),
        engine.session_id().as_str()
    ))
    .await
    .expect("transcript route")
    .json()
    .await
    .expect("transcript JSON");
    let completed = after.iter().find(|message| message.id == partial.id).expect("same assistant");
    assert!(completed.time.completed.is_some());
    let text: String = completed.parts.iter().filter_map(|part| part.as_text()).collect();
    assert_eq!(text, "x".repeat(ganja_core::engine::EVENT_CAPACITY + 32));
    drop(fresh);
    handle.shutdown().await.expect("stop");
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn a_failed_completed_envelope_write_can_leave_no_completion_after_a_retained_finish() {
    let (engine, storage, _data) = stored_engine();
    let fault = ganja_core::session::CompletedEnvelopeFailure::new(engine.session_id());
    let mut config = loopback_config();
    config.storage = Some(storage.clone());
    let handle = ganja_serve::serve(engine.clone(), config).await.expect("serve");
    let mut stream = Frames::connect(&handle, None).await;
    stream.next().await;
    engine.send(prompt()).await.expect("prompt");
    let frames = stream.finish().await;
    let terminal = frames.last().expect("finish");
    let Event::MessageFinished { message_id, .. } =
        serde_json::from_str::<Event>(&terminal.data).expect("finish JSON")
    else {
        panic!("terminal frame");
    };
    let transcript = storage.load_transcript(&engine.session_id()).expect("transcript");
    let assistant =
        transcript.iter().find(|message| message.id == message_id).expect("assistant persisted");
    assert!(assistant.time.completed.is_none(), "only the final envelope save failed");
    let text: String = assistant.parts.iter().filter_map(|part| part.as_text()).collect();
    assert_eq!(
        text,
        "x".repeat(ganja_core::engine::EVENT_CAPACITY + 32),
        "part flushes still succeeded"
    );
    let mut fresh = Frames::connect(&handle, None).await;
    let ack = fresh.next().await;
    assert_eq!(ack.id, terminal.id, "the finish is already at or before the fresh head");
    assert_eq!(fresh.next().await.event, "heartbeat", "fresh stream cannot recover the old finish");
    drop(fault);
    drop(fresh);
    drop(stream);
    handle.shutdown().await.expect("stop");
}

struct Frames {
    response: reqwest::Response,
    pending: VecDeque<Frame>,
    bytes: Vec<u8>,
}

impl Frames {
    async fn connect(handle: &Handle, cursor: Option<&str>) -> Self {
        let mut request = reqwest::Client::new().get(format!("{}/event", base_url(handle)));
        if let Some(cursor) = cursor {
            request = request.header("last-event-id", cursor);
        }
        Self {
            response: request.send().await.expect("stream"),
            pending: VecDeque::new(),
            bytes: Vec::new(),
        }
    }

    async fn next(&mut self) -> Frame {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if let Some(frame) = self.pending.pop_front() {
                    return frame;
                }
                let chunk = self.response.chunk().await.expect("chunk").expect("open stream");
                self.bytes.extend_from_slice(&chunk);
                self.pending.extend(support::drain_frames(&mut self.bytes));
            }
        })
        .await
        .expect("frame before deadline")
    }

    async fn finish(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        loop {
            let frame = self.next().await;
            let finished = frame.event == "message"
                && serde_json::from_str::<serde_json::Value>(&frame.data).expect("event JSON")["type"]
                    == "message_finished";
            frames.push(frame);
            if finished {
                return frames;
            }
        }
    }
}

#[tokio::test]
async fn a_cut_after_connected_replays_every_event_after_listener_restart_once() {
    let engine = support::engine();
    let journal = EventJournal::new();
    let mut config = loopback_config();
    config.journal = journal.clone();
    let handle = ganja_serve::serve(engine.clone(), config).await.expect("serve");
    let mut stream = Frames::connect(&handle, None).await;
    let connected = stream.next().await;
    assert_eq!(connected.event, "connected");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&connected.data).expect("JSON")["replay"],
        "fresh"
    );
    drop(stream);
    engine
        .send(Command::SendPrompt {
            text: "hello".into(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
            session_mentions: Vec::new(),
        })
        .await
        .expect("prompt");
    let mut replay = Frames::connect(&handle, Some(&connected.id)).await;
    assert_eq!(replay.next().await.id, connected.id);
    let first = replay.finish().await;
    let expected: Vec<_> = first.into_iter().filter(|frame| frame.event == "message").collect();
    drop(replay);
    handle.shutdown().await.expect("stop");
    let mut config = loopback_config();
    config.journal = journal;
    let handle = ganja_serve::serve(engine, config).await.expect("restart listener");
    let mut replay = Frames::connect(&handle, Some(&connected.id)).await;
    let ack = replay.next().await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ack.data).expect("JSON")["replay"],
        "from"
    );
    let actual: Vec<_> =
        replay.finish().await.into_iter().filter(|frame| frame.event == "message").collect();
    assert_eq!(actual, expected, "replay preserves every event and byte");
    let cursor = actual.last().expect("events").id.clone();
    drop(replay);
    let mut resumed = Frames::connect(&handle, Some(&cursor)).await;
    assert_eq!(resumed.next().await.id, cursor);
    let heartbeat = resumed.next().await;
    assert_eq!(heartbeat.event, "heartbeat");
    assert_eq!(heartbeat.id, cursor, "control frames do not advance");
    drop(resumed);
    handle.shutdown().await.expect("stop");
}

#[tokio::test]
async fn an_overflow_or_another_epoch_is_a_declared_gap() {
    let engine = support::engine();
    let journal = EventJournal::with_limits(
        NonZeroUsize::new(1).expect("nonzero"),
        NonZeroUsize::new(65536).expect("nonzero"),
    );
    let cursor = journal.head();
    let mut config = loopback_config();
    config.journal = journal.clone();
    let handle = ganja_serve::serve(engine.clone(), config).await.expect("serve");
    let mut events = Frames::connect(&handle, None).await;
    events.next().await;
    let mut direct = engine.subscribe().await.expect("direct subscriber");
    engine
        .send(Command::SendPrompt {
            text: "hello".into(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
            session_mentions: Vec::new(),
        })
        .await
        .expect("prompt");
    let observed = ganja_testkit::drain(&mut direct).await;
    tokio::time::timeout(DEADLINE, async {
        while journal.head().rsplit_once('-').expect("cursor").1.parse::<usize>().expect("sequence")
            < observed.len()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("journal catches up");
    for old in [cursor, EventJournal::new().head()] {
        let mut stream = Frames::connect(&handle, Some(&old)).await;
        let ack = stream.next().await;
        assert_eq!(ack.id, old);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&ack.data).expect("JSON")["replay"],
            "gap"
        );
    }
    drop(events);
    handle.shutdown().await.expect("stop");
}

#[tokio::test]
async fn a_permission_before_the_fresh_head_is_visible_after_registration() {
    let (tool, _) = RecorderTool::new("lookup", "lookup ran", "found it");
    let mut permissions = Permissions::default();
    permissions.set_baseline(vec![Rule {
        permission: "lookup".into(),
        pattern: "*".into(),
        action: Action::Ask,
    }]);
    let engine = support::scripted_engine(
        vec![tool_call("lookup", serde_json::json!({})), says("done")],
        Registry::new(vec![tool]),
        permissions,
    );
    let handle = ganja_serve::serve(Arc::clone(&engine), loopback_config()).await.expect("serve");
    let mut events = Frames::connect(&handle, None).await;
    events.next().await;
    engine
        .send(Command::SendPrompt {
            text: "lookup".into(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
            session_mentions: Vec::new(),
        })
        .await
        .expect("prompt");
    loop {
        let frame = events.next().await;
        if frame.event == "message"
            && serde_json::from_str::<serde_json::Value>(&frame.data).expect("JSON")["type"]
                == "permission_requested"
        {
            break;
        }
    }
    let mut fresh = Frames::connect(&handle, None).await;
    fresh.next().await;
    let pending: serde_json::Value = reqwest::get(format!("{}/permission", base_url(&handle)))
        .await
        .expect("permissions")
        .json()
        .await
        .expect("JSON");
    assert_eq!(pending.as_array().expect("array").len(), 1);
    engine.send(Command::CancelTurn).await.expect("cancel");
    drop(fresh);
    drop(events);
    handle.shutdown().await.expect("stop");
}
