use std::sync::Arc;
use std::time::Duration;

use buffa::Message as _;
use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

use super::super::PROVIDERS;
use super::{
    CursorProvider, CursorWire, DEFAULT_BASE_URL, ID, Provider as _, ProviderError, connect, proto,
};
use crate::auth::{self, AuthError, OauthCredential, RefreshOauth};
use crate::protocol::FinishReason;
use crate::provider::ProviderEvent;

/// A renewal that must never run, for the cases that are about
/// construction rather than about a token endpoint.
struct NeverRenews;

#[async_trait::async_trait]
impl RefreshOauth for NeverRenews {
    async fn refresh(
        &self,
        provider_id: &str,
        _credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        panic!("{provider_id} was renewed by a test that only builds a provider");
    }
}

/// A data frame holding one update, built with the real message types
/// so the fold is driven by exactly what the server would send.
fn framed(update: proto::Update) -> Vec<u8> {
    let message = proto::ServerMessage {
        interaction_update: buffa::MessageField::some(update),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

fn text(delta: &str) -> proto::Update {
    proto::Update {
        text_delta: buffa::MessageField::some(proto::TextDelta::default().with_text(delta)),
        ..Default::default()
    }
}

fn turn_ended() -> proto::Update {
    proto::Update {
        turn_ended: buffa::MessageField::some(proto::TurnEnded::default()),
        ..Default::default()
    }
}

/// A data frame holding the server's context ask, ids and all — the
/// exchange the 2026-08-10 live turn hung on.
fn exec_framed(id: u32, exec_id: &str) -> Vec<u8> {
    let message = proto::ServerMessage {
        exec_request: buffa::MessageField::some(
            proto::ExecRequest {
                request_context_args: buffa::MessageField::some(proto::ContextArgs::default()),
                ..Default::default()
            }
            .with_id(id)
            .with_exec_id(exec_id),
        ),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

/// A data frame holding the exec the live turn died on: the server
/// asking this client to run a shell for it. The kind arrives as the
/// args oneof's field 14, which this build models by number rather than
/// by shape, because it never runs one.
fn shell_stream_framed(id: u32) -> Vec<u8> {
    let mut exec = proto::ExecRequest::default().with_id(id);
    exec.__buffa_unknown_fields.push(buffa::UnknownField {
        number: 14,
        data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
    });

    let message = proto::ServerMessage {
        exec_request: buffa::MessageField::some(exec),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

/// A duplex whose answers nobody reads, for fixtures without an ask.
fn promptless_duplex() -> super::Duplex {
    let (answers, _) = futures::channel::mpsc::unbounded();
    super::Duplex { answers, system: None, blobs: std::collections::HashMap::new() }
}

/// A data frame holding one kv exchange, built with the real message
/// types the way the server frames them.
fn kv_framed(kv: proto::KvRequest) -> Vec<u8> {
    let message =
        proto::ServerMessage { kv_request: buffa::MessageField::some(kv), ..Default::default() };

    connect::envelope(&message.encode_to_vec())
}

fn kv_set(id: u32, blob_id: &[u8], data: &[u8]) -> Vec<u8> {
    kv_framed(proto::KvRequest {
        id: Some(id),
        set_blob_args: buffa::MessageField::some(
            proto::SetBlobArgs::default()
                .with_blob_id(blob_id.to_vec())
                .with_blob_data(data.to_vec()),
        ),
        ..Default::default()
    })
}

fn kv_get(id: u32, blob_id: &[u8]) -> Vec<u8> {
    kv_framed(proto::KvRequest {
        id: Some(id),
        get_blob_args: buffa::MessageField::some(
            proto::GetBlobArgs::default().with_blob_id(blob_id.to_vec()),
        ),
        ..Default::default()
    })
}

/// An EndStream frame carrying `payload`.
fn end_stream(payload: &str) -> Vec<u8> {
    let mut frame = vec![0b0000_0010];
    frame.extend_from_slice(
        &u32::try_from(payload.len()).expect("a test payload fits").to_be_bytes(),
    );
    frame.extend_from_slice(payload.as_bytes());

    frame
}

#[test]
fn ganja_calls_it_cursor_everywhere_the_wire_can_see() {
    assert_eq!(CursorProvider.id(), ID);
    assert_eq!(ID, "cursor");
    assert_eq!(
        ID,
        auth::cursor::PROVIDER_ID,
        "one constant, or a login stores under a name the provider does not read"
    );
    assert!(PROVIDERS.contains(&ID), "a provider nothing can select is a provider nobody has");
}

#[test]
fn the_endpoint_is_cursors_own_and_the_debug_holds_no_secret() {
    assert_eq!(DEFAULT_BASE_URL, "https://api2.cursor.sh");

    // Built through `at` at the same constant `from_stored` passes,
    // because `from_stored` reads whatever credential store the machine
    // running this suite really holds.
    let wire = CursorWire::at(DEFAULT_BASE_URL, Arc::new(NeverRenews)).expect("a client builds");
    let rendered = format!("{wire:?}");
    assert!(rendered.contains("Oauth") && rendered.contains("cursor"), "{rendered}");
    assert!(
        rendered.contains("https://api2.cursor.sh"),
        "the endpoint is what tells one wire from another: {rendered}"
    );

    // The selectable identity stays a bare name: nothing to leak, and
    // nothing read at construction.
    assert_eq!(format!("{CursorProvider:?}"), "CursorProvider");
}

/// The endpoint is not exempt from the rule every other base URL is held
/// to just because the credential arrived as a token rather than a key.
#[test]
fn an_access_token_may_not_be_sent_anywhere_a_key_could_not_be() {
    let refused = CursorWire::at("http://api2.cursor.sh", Arc::new(NeverRenews))
        .expect_err("plain http to a public host puts the token on the wire in the clear");
    assert!(matches!(refused, ProviderError::Transport(_)), "{refused:?}");
    assert!(
        CursorWire::at("http://127.0.0.1:4096", Arc::new(NeverRenews)).is_ok(),
        "loopback never reaches a network, which is what a test depends on"
    );
}

/// The whole reason this fold exists: an event reaches the session while
/// the response body is still open. A fold that buffered until the end
/// of the body would leave the first `next()` waiting on a channel
/// nothing has closed, which the timeout turns into a readable failure.
#[tokio::test]
async fn a_delta_is_handed_over_while_the_body_is_still_open() {
    let (sender, receiver) =
        futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::convert::Infallible>>();
    let mut stream = super::events(receiver, CancellationToken::new(), promptless_duplex());

    sender.unbounded_send(Ok(framed(text("Hello")))).expect("the body is open");
    let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("the delta must arrive before the body ends");
    assert_eq!(
        first,
        Some(ProviderEvent::TextDelta("Hello".to_owned())),
        "the first frame's event, with the rest of the body unwritten"
    );

    let mut rest = framed(text(" world"));
    rest.extend(framed(turn_ended()));
    rest.extend(end_stream("{}"));
    sender.unbounded_send(Ok(rest)).expect("the body is open");
    drop(sender);

    let tail: Vec<ProviderEvent> = stream.collect().await;
    assert_eq!(
        tail,
        vec![
            ProviderEvent::TextDelta(" world".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]
    );
}

/// The exchange the 2026-08-10 live turn hung on, both directions at
/// the fold: the server asks for context mid-stream, the answer rides
/// out on the held-open request body — ids echoed, the prompt on
/// `cloud_rule` — before any event, and only then does the turn's text
/// flow.
#[tokio::test]
async fn the_context_ask_is_answered_on_the_open_body_before_the_turn_flows() {
    let (sender, receiver) =
        futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::convert::Infallible>>();
    let (answers, mut answered) = futures::channel::mpsc::unbounded();
    let mut stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex {
            answers,
            system: Some("You are terse.".to_owned()),
            blobs: std::collections::HashMap::new(),
        },
    );

    sender.unbounded_send(Ok(exec_framed(7, "exec-abc"))).expect("the body is open");

    // Polling the stream is what answers the ask, so the answer must
    // land while `next()` is still pending — an event arriving first
    // would mean generation was read past an unanswered question.
    let drive = stream.next();
    let raced = tokio::time::timeout(
        Duration::from_secs(10),
        futures::future::select(drive, answered.next()),
    )
    .await
    .expect("the answer must go out while the body is still open");
    let answer = match raced {
        futures::future::Either::Right((answer, _)) => answer
            .expect("the fold holds the sender")
            .expect("the channel's error type is infallible"),
        futures::future::Either::Left((event, _)) => {
            panic!("an ask is a question, not an event: {event:?}")
        }
    };

    assert_eq!(answer[0], 0, "an ordinary data frame");
    let sent = proto::ClientMessage::decode_from_slice(&answer[5..])
        .expect("the answered bytes are the client message");
    assert!(sent.run_request.as_option().is_none(), "an answer is not a second run request");
    let exec = sent.exec_response.as_option().expect("the exec answer");
    assert_eq!(exec.id, Some(7), "the id the server minted comes back");
    assert_eq!(exec.exec_id.as_deref(), Some("exec-abc"));
    assert_eq!(
        exec.request_context_result
            .as_option()
            .and_then(|result| result.success.as_option())
            .and_then(|success| success.request_context.as_option())
            .and_then(|context| context.cloud_rule.as_deref()),
        Some("You are terse."),
        "the prompt travels the channel cursor honors"
    );

    let mut rest = framed(text("Hello"));
    rest.extend(framed(turn_ended()));
    rest.extend(end_stream("{}"));
    sender.unbounded_send(Ok(rest)).expect("the body is open");
    drop(sender);

    let tail: Vec<ProviderEvent> = stream.collect().await;
    assert_eq!(
        tail,
        vec![
            ProviderEvent::TextDelta("Hello".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]
    );
}

/// The channel the second 2026-08-10 live run left waiting: the server
/// stores state with the client and reads its own writes back, all
/// mid-stream, and every answer rides the same open body the context
/// answer does — in frame order, because an answer overtaking the one
/// ahead of it would cross the server's questions.
#[tokio::test]
async fn the_kv_channel_is_answered_in_frame_order_behind_the_context_answer() {
    let (sender, receiver) =
        futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::convert::Infallible>>();
    let (answers, answered) = futures::channel::mpsc::unbounded();
    let stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex {
            answers,
            system: Some("You are terse.".to_owned()),
            blobs: std::collections::HashMap::new(),
        },
    );

    let mut body = exec_framed(7, "exec-abc");
    body.extend(kv_set(8, b"blob-a", b"opaque-state"));
    body.extend(kv_get(9, b"blob-a"));
    body.extend(kv_get(10, b"blob-b"));
    body.extend(framed(text("Hello")));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));
    sender.unbounded_send(Ok(body)).expect("the body is open");
    drop(sender);

    // Collecting drives the fold to the end; the asks are answered as
    // their frames decode, and dropping the drained stream closes the
    // answer channel so the collect below has an end.
    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("a fully-answered turn ends");
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hello".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the kv exchanges are questions, never events"
    );

    let sent: Vec<Vec<u8>> = answered
        .map(|answer| answer.expect("the channel's error type is infallible"))
        .collect()
        .await;
    assert_eq!(sent.len(), 4, "every ask was answered, none twice");
    let decoded: Vec<proto::ClientMessage> = sent
        .iter()
        .map(|answer| {
            assert_eq!(answer[0], 0, "an ordinary data frame");
            proto::ClientMessage::decode_from_slice(&answer[5..])
                .expect("the answered bytes are client messages")
        })
        .collect();

    // First out: the context answer, because its frame came first.
    assert_eq!(
        decoded[0].exec_response.as_option().and_then(|exec| exec.id),
        Some(7),
        "the context answer went out ahead of every kv answer"
    );

    let set_ack = decoded[1].kv_response.as_option().expect("the set's ack");
    assert_eq!(set_ack.id, Some(8));
    assert!(
        set_ack.set_blob_result.is_set(),
        "the ack is the present-but-empty result the plugin sends"
    );

    let hit = decoded[2].kv_response.as_option().expect("the get's answer");
    assert_eq!(hit.id, Some(9));
    assert_eq!(
        hit.get_blob_result.as_option().and_then(|result| result.blob_data.as_deref()),
        Some(b"opaque-state".as_slice()),
        "the get reads back exactly what the set stored"
    );

    let miss = decoded[3].kv_response.as_option().expect("the miss answer");
    assert_eq!(miss.id, Some(10));
    assert_eq!(
        miss.get_blob_result.as_option().and_then(|result| result.blob_data.as_deref()),
        None,
        "a blob nobody stored is answered not-found, not failed"
    );
}

/// The turn the live `shell_stream_args` exec used to kill (**D486**):
/// the server asks this client to run a shell mid-stream, the refusal
/// rides out on the held-open body as the pair the shipped client
/// writes, and the turn generates past it to a clean finish. Nothing in
/// the event stream says a word about it — a refusal is an answer, and
/// the session sees the reply it asked for.
#[tokio::test]
async fn a_tool_exec_is_refused_on_the_open_body_and_the_turn_survives() {
    let (sender, receiver) =
        futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::convert::Infallible>>();
    let (answers, answered) = futures::channel::mpsc::unbounded();
    let stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex { answers, system: None, blobs: std::collections::HashMap::new() },
    );

    let mut body = shell_stream_framed(5);
    body.extend(framed(text("Hello")));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));
    sender.unbounded_send(Ok(body)).expect("the body is open");
    drop(sender);

    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("a refused exec ends the exchange rather than hanging it");
    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::Failed(_))),
        "a refused tool exec is not a failed turn: {events:?}"
    );
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hello".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the exec is a question, never an event"
    );

    let sent: Vec<Vec<u8>> = answered
        .map(|answer| answer.expect("the channel's error type is infallible"))
        .collect()
        .await;
    assert_eq!(sent.len(), 2, "the throw, and the close that ends it");

    let decoded: Vec<proto::ClientMessage> = sent
        .iter()
        .map(|answer| {
            assert_eq!(answer[0], 0, "an ordinary data frame");
            proto::ClientMessage::decode_from_slice(&answer[5..])
                .expect("the answered bytes are client messages")
        })
        .collect();

    let thrown = decoded[0]
        .exec_control
        .as_option()
        .and_then(|control| control.throw.as_option())
        .expect("the throw went out first");
    assert_eq!(thrown.id, Some(5), "the id the server minted comes back");
    assert!(
        thrown.error.as_deref().is_some_and(|reason| reason.contains("shell_stream_args")),
        "the server's agent loop is told what was refused: {thrown:?}"
    );
    assert_eq!(
        decoded[1]
            .exec_control
            .as_option()
            .and_then(|control| control.stream_close.as_option())
            .and_then(|close| close.id),
        Some(5),
        "and then the exchange is closed, the way the shipped client closes one"
    );
}

/// A kv ask arriving after nothing holds the request body open gets the
/// context ask's discipline: the turn fails with the reason on it
/// instead of reproducing the silence.
#[tokio::test]
async fn a_kv_ask_nobody_can_answer_fails_the_turn_instead_of_hanging() {
    let events: Vec<ProviderEvent> = super::events(
        futures::stream::iter([Ok::<Vec<u8>, std::convert::Infallible>(kv_set(
            1,
            b"blob-a",
            b"opaque-state",
        ))]),
        CancellationToken::new(),
        promptless_duplex(),
    )
    .collect()
    .await;

    assert!(
        matches!(
            events.as_slice(),
            [ProviderEvent::Failed(ProviderError::Transport(message))]
                if message.contains("kv ask")
        ),
        "{events:?}"
    );
}

/// An ask arriving after nothing holds the request body open cannot be
/// answered, and an unanswered exec is a hang — so the turn fails with
/// the reason on it instead of reproducing the silence.
#[tokio::test]
async fn a_context_ask_nobody_can_answer_fails_the_turn_instead_of_hanging() {
    let events: Vec<ProviderEvent> = super::events(
        futures::stream::iter([Ok::<Vec<u8>, std::convert::Infallible>(exec_framed(
            1,
            "exec-dead",
        ))]),
        CancellationToken::new(),
        promptless_duplex(),
    )
    .collect()
    .await;

    assert!(
        matches!(
            events.as_slice(),
            [ProviderEvent::Failed(ProviderError::Transport(message))]
                if message.contains("context ask")
        ),
        "{events:?}"
    );
}

/// The other wires' cancellation contract, replicated from
/// `anthropic`'s test of the same name: the whole transcript delivered
/// as one chunk is the worst case — every frame already parsed and
/// waiting — and the cancel still wins.
#[tokio::test]
async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
    let mut body = framed(text("Hello"));
    body.extend(framed(text(" world")));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));

    let cancel = CancellationToken::new();
    let mut stream = super::replay(body, cancel.clone());

    assert_eq!(stream.next().await, Some(ProviderEvent::TextDelta("Hello".to_owned())));
    cancel.cancel();

    let rest: Vec<ProviderEvent> = stream.collect().await;
    assert!(
        rest.is_empty(),
        "a cancelled stream ends; the engine is what calls that Cancelled, and it \
             cannot if a Finish or a Failed arrives: {rest:?}"
    );
}

/// A terminal event drops whatever decoded behind it — the shared
/// folds' contract — so a body talking past its EndStream frame ends on
/// the verdict rather than on the splitter's complaint about the
/// trailing bytes.
#[tokio::test]
async fn nothing_follows_the_streams_verdict() {
    let mut body = framed(text("done"));
    body.extend(end_stream("{}"));
    body.push(0x00);

    let events: Vec<ProviderEvent> = super::replay(body, CancellationToken::new()).collect().await;
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("done".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]
    );
}

/// The admitted runtime is real, not merely named: a generated message
/// round-trips through `buffa`'s encode/decode. This is what makes the
/// dependency reach the lock (so `cargo deny` audits its license) and
/// the live listing decode against the same version.
#[test]
fn the_admitted_protobuf_runtime_round_trips_a_generated_message() {
    use buffa::Message as _;

    let entry = super::proto::ModelEntry::default()
        .with_model_id("gpt-5.3-codex")
        .with_display_name("Codex 5.3");
    let bytes = entry.encode_to_vec();
    let decoded = super::proto::ModelEntry::decode_from_slice(&bytes)
        .expect("a message buffa encoded decodes");

    assert_eq!(decoded.model_id.as_deref(), Some("gpt-5.3-codex"));
    assert_eq!(decoded.display_name.as_deref(), Some("Codex 5.3"));
}

/// The checked-in generated code still matches its `.proto`:
/// regenerating with the same remote plugin must produce byte-identical
/// output. A drift here means somebody edited the `@generated` file by
/// hand or changed the `.proto` without regenerating — either way the
/// source of truth and the compiled code have diverged.
///
/// Skipped rather than failed when `buf` is absent: the drift check is a
/// developer-machine guard, and the workspace deliberately keeps `buf`
/// and `protoc` out of CI (the generated code is checked in for exactly
/// that reason). CI proves the code compiles and round-trips; this
/// proves it was not hand-edited, on a machine that can regenerate.
#[test]
fn the_checked_in_generated_code_matches_the_proto() {
    use std::process::Command;

    let crate_dir = env!("CARGO_MANIFEST_DIR");
    if Command::new("buf").arg("--version").output().is_err() {
        eprintln!("skipping the proto drift check: `buf` is not on PATH");
        return;
    }

    let generated = std::path::Path::new(crate_dir).join("src/provider/cursor/ganja.cursor.v1.rs");
    let before = std::fs::read_to_string(&generated).expect("the generated file is present");

    let status = Command::new("buf")
        .arg("generate")
        .current_dir(crate_dir)
        .status()
        .expect("buf generate runs");
    assert!(status.success(), "buf generate failed");

    let after = std::fs::read_to_string(&generated).expect("the generated file is present");
    assert_eq!(
        before, after,
        "the checked-in cursor protobuf code has drifted from cursor.proto; \
             run `buf generate` in crates/ganja-provider and commit the result"
    );
}
