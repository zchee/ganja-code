use std::convert::Infallible;
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
use crate::provider::{ChatRequest, ProviderEvent};
use crate::tool::ToolDefinition;

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
///
/// `pub(super)` with the helpers below it: `bridge::tests` and
/// `request::tests` drive the same fold, and one spelling of a frame is one
/// thing to get wrong.
pub(super) fn framed(update: proto::Update) -> Vec<u8> {
    let message = proto::ServerMessage {
        interaction_update: buffa::MessageField::some(update),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

pub(super) fn text(delta: &str) -> proto::Update {
    proto::Update {
        text_delta: buffa::MessageField::some(proto::TextDelta::default().with_text(delta)),
        ..Default::default()
    }
}

pub(super) fn turn_ended() -> proto::Update {
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
/// asking this client to run a shell for it — refused on a roster without
/// `bash`, bridged to it on one that offers it.
fn shell_stream_framed(id: u32) -> Vec<u8> {
    let exec = proto::ExecRequest {
        id: Some(id),
        exec_id: Some("exec-shell".to_owned()),
        shell_stream_args: buffa::MessageField::some(
            proto::ShellArgs::default().with_command("ls").with_working_directory("/repo"),
        ),
        ..Default::default()
    };

    let message = proto::ServerMessage {
        exec_request: buffa::MessageField::some(exec),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

/// A duplex whose answers nobody reads, for fixtures without an ask.
fn promptless_duplex() -> super::Duplex {
    let (answers, _) = futures::channel::mpsc::unbounded();

    super::Duplex::for_tests(answers, Vec::new())
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
pub(super) fn end_stream(payload: &str) -> Vec<u8> {
    let mut frame = vec![0b0000_0010];
    frame.extend_from_slice(
        &u32::try_from(payload.len()).expect("a test payload fits").to_be_bytes(),
    );
    frame.extend_from_slice(payload.as_bytes());

    frame
}

#[test]
fn ganja_calls_it_cursor_everywhere_the_wire_can_see() {
    assert_eq!(CursorProvider::default().id(), ID);
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

    // The selectable identity renders where it points and nothing else: it
    // holds no credential, and the runs it is holding open are somebody's
    // conversation.
    let selectable = format!("{:?}", CursorProvider::default());
    assert!(
        selectable.contains("api2.cursor.sh"),
        "the default provider is the stored login at cursor's own endpoint: {selectable}"
    );
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
    let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let mut stream = super::events(receiver, CancellationToken::new(), promptless_duplex(), None);

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

/// The run-level heartbeat while the fold is still **reading**. The keeper
/// beats only once a Run is held, so a fold waiting on a slow server keeps the
/// exchange alive on its own clock — and the clock is pinned, not merely the
/// beating: the first beat is one period out and there is one per period, so
/// an interval that fired at zero, or one that burst to catch up, would both
/// count wrong here. The beats are liveness and never events.
#[tokio::test(start_paused = true)]
async fn a_fold_still_reading_beats_on_the_body_once_a_period() {
    let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, mut answered) = futures::channel::mpsc::unbounded();
    let stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex::for_tests(answers, Vec::new()),
        None,
    );

    // Driven in a task of its own: an interval ticks only under a poll, and
    // the poll that matters here is the one waiting on a chunk that never
    // comes.
    let driver = tokio::spawn(stream.collect::<Vec<ProviderEvent>>());

    tokio::time::sleep(super::bridge::HEARTBEAT * 2 + Duration::from_secs(1)).await;
    let beats = sent_so_far(&mut answered)
        .into_iter()
        .filter(|message| message.client_heartbeat.is_set())
        .count();
    assert_eq!(beats, 2, "one beat a period, the first a period out, no burst");

    let mut rest = framed(text("Hello"));
    rest.extend(framed(turn_ended()));
    rest.extend(end_stream("{}"));
    sender.unbounded_send(Ok(rest)).expect("the body is open");
    drop(sender);

    let events = driver.await.expect("the fold finishes");
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hello".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "a heartbeat is liveness, never an event"
    );
}

/// The exchange the 2026-08-10 live turn hung on, both directions at
/// the fold: the server asks for context mid-stream, the answer rides
/// out on the held-open request body — ids echoed, the prompt on
/// `cloud_rule` — before any event, and only then does the turn's text
/// flow.
#[tokio::test]
async fn the_context_ask_is_answered_on_the_open_body_before_the_turn_flows() {
    let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, mut answered) = futures::channel::mpsc::unbounded();
    let mut stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex::speaking(answers, Some("You are terse.")),
        None,
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

    let sent = client_message(&answer);
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
    let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, answered) = futures::channel::mpsc::unbounded();
    let stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex::speaking(answers, Some("You are terse.")),
        None,
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

    let decoded = sent(answered).await;
    assert_eq!(decoded.len(), 4, "every ask was answered, none twice");

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
    let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, answered) = futures::channel::mpsc::unbounded();
    let stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex::for_tests(answers, Vec::new()),
        None,
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

    let decoded = sent(answered).await;
    assert_eq!(decoded.len(), 2, "the rejection, and the close that ends it");

    let rejected = decoded[0]
        .exec_response
        .as_option()
        .expect("the rejection went out first, on the exec channel");
    assert_eq!(rejected.id, Some(5), "the id the server minted comes back");
    assert_eq!(rejected.exec_id.as_deref(), Some("exec-shell"), "and so does the exec id");
    let event = rejected
        .shell_stream
        .as_option()
        .and_then(|stream| stream.rejected.as_option())
        .expect("the streamed kind's own rejected event");
    assert_eq!(event.command.as_deref(), Some("ls"), "echoing what the server asked to run");
    assert!(
        event.reason.as_deref().is_some_and(|reason| reason.contains("shell_stream_args")),
        "the server's agent loop is told what was refused and why: {event:?}"
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
        futures::stream::iter([Ok::<Vec<u8>, Infallible>(kv_set(1, b"blob-a", b"opaque-state"))]),
        CancellationToken::new(),
        promptless_duplex(),
        None,
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
        futures::stream::iter([Ok::<Vec<u8>, Infallible>(exec_framed(1, "exec-dead"))]),
        CancellationToken::new(),
        promptless_duplex(),
        None,
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

/// The roster a bridged turn declares, and the tools an `mcp_args` may name:
/// the two every test file under `cursor` shares.
pub(super) fn roster() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read".to_owned(),
            description: "Reads a file.".to_owned(),
            schema: serde_json::json!({ "type": "object" }),
        },
        ToolDefinition {
            name: "bash".to_owned(),
            description: "Runs a command.".to_owned(),
            schema: serde_json::json!({ "type": "object" }),
        },
    ]
}

/// A request that opens a turn — one user message, the roster declared — and
/// so one that can key a held run.
pub(super) fn opening(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_owned(),
        system: None,
        messages: vec![crate::protocol::Message::user("read the file")],
        turn_start: 0,
        tools: roster(),
        effort_options: serde_json::Map::new(),
    }
}

/// One frame the fold wrote on the request body, read back as the client
/// message it carries — having checked it is an ordinary data frame.
pub(super) fn client_message(frame: &[u8]) -> proto::ClientMessage {
    assert_eq!(frame[0], 0, "an ordinary data frame");

    proto::ClientMessage::decode_from_slice(&frame[5..]).expect("the answered bytes decode")
}

/// Everything the fold wrote on the request body, once the fold is gone and
/// the channel has closed behind it.
pub(super) async fn sent(answered: Answered) -> Vec<proto::ClientMessage> {
    answered
        .map(|frame| client_message(&frame.expect("the channel's error type is infallible")))
        .collect()
        .await
}

/// Everything the fold has written on the request body **so far**, for a
/// channel a held run keeps open — a collect would wait on a turn that has
/// paused.
pub(super) fn sent_so_far(answered: &mut Answered) -> Vec<proto::ClientMessage> {
    std::iter::from_fn(|| answered.try_recv().ok())
        .map(|frame| client_message(&frame.expect("the channel's error type is infallible")))
        .collect()
}

/// The receiving end of a duplex's answer channel.
pub(super) type Answered = futures::channel::mpsc::UnboundedReceiver<Result<Vec<u8>, Infallible>>;

/// A frame carrying one `mcp_args`, built from whatever the case is about.
fn mcp_framed(id: u32, args: proto::McpArgs) -> Vec<u8> {
    let message = proto::ServerMessage {
        exec_request: buffa::MessageField::some(proto::ExecRequest {
            id: Some(id),
            exec_id: Some("exec-mcp".to_owned()),
            mcp_args: buffa::MessageField::some(args),
            ..Default::default()
        }),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

/// Drives one exec frame through a **bridging** fold with `declared` on the
/// roster, and returns the events it produced beside what it wrote back.
///
/// The bridge is what makes the two observations below mean anything. Without
/// one the fold has no key to park under, so *every* exec is answered at the
/// pauseless guard and a `ToolCallStart` is structurally impossible — an
/// absent one would be a tautology rather than a measurement. Here a
/// bridgeable exec really would pause the Run and emit one, so "nothing
/// executed" is something the harness could have contradicted.
async fn bridged_exec(
    exec: Vec<u8>,
    declared: Vec<ToolDefinition>,
) -> (Vec<ProviderEvent>, Vec<proto::ClientMessage>) {
    // A request that can key a held run, so this harness is a wire that really
    // could pause — which is what makes "this call was bridged rather than
    // answered" observable instead of indistinguishable from "this wire cannot
    // bridge".
    let held = Arc::new(super::bridge::HeldRuns::default());
    let request = opening("auto");
    let key = super::bridge::Key::of(&request).expect("a request with a message keys");

    let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, mut answered) = futures::channel::mpsc::unbounded();
    let stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex::for_tests(answers, declared),
        Some(super::Bridge::new(Arc::clone(&held), key)),
    );

    sender.unbounded_send(Ok(exec)).expect("the body is open");

    // The turn's end arrives in a **second** chunk, a gather window later, and
    // that gap is load-bearing: a terminal event reaching the fold before the
    // window closes clears the pending batch and the Run never pauses at all.
    // Delivered in one chunk — which is what this harness used to do — no exec
    // could ever be bridged, and every "nothing executed" assertion below
    // would hold for the harness's reasons rather than the code's.
    tokio::spawn(async move {
        tokio::time::sleep(super::GATHER_WINDOW * 3).await;
        let mut tail = framed(turn_ended());
        tail.extend(end_stream("{}"));
        // A bridged exec has already moved the fold into the held table by
        // now, so this reaches a body nobody is reading. That is the shape a
        // live turn has too.
        let _ = sender.unbounded_send(Ok(tail));
    });

    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("an answered exec ends the exchange rather than hanging it");
    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::Failed(_))),
        "an answered exec is not a failed turn: {events:?}"
    );

    // So far, not to the close: a *bridged* exec moves the duplex — and with it
    // the sender — into the held run, so the channel never closes.
    (events, sent_so_far(&mut answered))
}

/// The same, for one `mcp_args`: the `McpResult` it was answered with — or
/// `None` when the exec was handed to the engine instead.
///
/// A bridged call pauses the Run, so it produces tool-call events and *no*
/// answer on the wire; every refusal is decided before a pause could happen
/// and produces an answer and no events. That difference is the assertion.
async fn answered_mcp(
    args: proto::McpArgs,
    declared: Vec<ToolDefinition>,
) -> Option<proto::McpResult> {
    let (_, sent) = bridged_exec(mcp_framed(3, args), declared).await;

    sent.iter()
        .find_map(|message| message.exec_response.as_option())
        .and_then(|response| response.mcp_result.as_option())
        .cloned()
}

/// **AC-14.** A name outside the roster this request declared is answered with
/// `tool_not_found` **carrying that roster** — which is the arm this build
/// could not honestly use before it published one — and the turn survives.
#[tokio::test]
async fn a_call_naming_an_undeclared_tool_is_answered_with_the_roster_it_is_missing_from() {
    let result = answered_mcp(
        proto::McpArgs::default()
            .with_name("rm_minus_rf")
            .with_tool_name("rm_minus_rf")
            .with_tool_call_id("call-1")
            .with_provider_identifier("ganja"),
        roster(),
    )
    .await
    .expect("an undeclared name is answered here, never bridged");

    let missing = result.tool_not_found.as_option().expect("the arm that carries a roster");
    assert_eq!(missing.name.as_deref(), Some("rm_minus_rf"));
    assert_eq!(
        missing.available_tools,
        vec!["read".to_owned(), "bash".to_owned()],
        "the roster is this request's own, in the order the engine advertised it"
    );
    assert!(!result.rejected.is_set(), "the roster exists now, so the honest arm is the typed one");
}

/// **AC-22.** A call naming somebody else's server is not this client's to
/// look up, and is answered `server_not_found` rather than executed.
#[tokio::test]
async fn a_call_for_another_server_is_answered_server_not_found_and_never_run() {
    let result = answered_mcp(
        proto::McpArgs::default()
            .with_name("read")
            .with_tool_name("read")
            .with_tool_call_id("call-1")
            .with_provider_identifier("somebody-elses-mcp"),
        roster(),
    )
    .await
    .expect("a foreign identifier is answered here, never bridged");

    let missing = result.server_not_found.as_option().expect("the server arm");
    assert_eq!(missing.name.as_deref(), Some("somebody-elses-mcp"));
    assert_eq!(missing.available_servers, vec!["ganja".to_owned()]);
}

/// The other half of AC-22, and the one the fixture settles: a **present**
/// `server_identifier` on a `"ganja"` call is the measured norm — it arrived on
/// every recorded call — so it refuses nothing, and the call is bridged.
#[tokio::test]
async fn a_present_server_identifier_on_our_own_call_refuses_nothing() {
    let bridged = answered_mcp(
        proto::McpArgs::default()
            .with_name("read")
            .with_tool_name("read")
            .with_tool_call_id("call-1")
            .with_provider_identifier("ganja")
            .with_server_identifier("whatever-the-server-calls-us"),
        roster(),
    )
    .await;

    assert!(
        bridged.is_none(),
        "a call this client serves is handed to the engine, not answered on the wire"
    );
}

/// **AC-22**, the matching half: when a call's two spellings disagree, the
/// bridge runs the one the *declaration* named.
///
/// `McpArgs` carries both `name = 1` and `tool_name = 5`, and the shipped
/// client fills them from one declaration (`index.js@5699717`), so every
/// recorded call has them identical and no recording can settle which of the
/// two this build reads. [`super::cursor::decode::McpCall::called`] prefers
/// `tool_name`. Both spellings here name a tool the roster really holds, so a
/// reversed precedence would refuse nothing and *quietly run the other tool* —
/// which is the failure this pins, and the one no roster-membership test can
/// see.
#[tokio::test]
async fn a_call_whose_two_spellings_disagree_runs_the_one_the_declaration_named() {
    let (events, _) = bridged_exec(
        mcp_framed(
            3,
            proto::McpArgs::default()
                .with_name("bash")
                .with_tool_name("read")
                .with_tool_call_id("call-1")
                .with_provider_identifier("ganja"),
        ),
        roster(),
    )
    .await;

    let called: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallStart { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        called,
        vec!["read"],
        "`tool_name = 5` names the tool; `name = 1` is the fallback for a server that sent none",
    );
}

/// **AC-23.** A `smart_mode_approval_only` preflight is a policy question, and
/// answering it by running the tool would run a side-effecting call for one —
/// possibly twice, since the real call follows. So it is approved and
/// **nothing executes**: no tool call reaches the engine, which is what the
/// absent `ToolCallStart` below asserts.
///
/// Driven through the bridging harness on purpose: `bash` is on this roster,
/// so without the preflight arm this exec *would* pause the Run and emit a
/// `ToolCallStart`. Under a fold with no bridge the absence would prove
/// nothing at all.
#[tokio::test]
async fn an_approval_preflight_is_approved_without_executing_anything() {
    let (events, sent) = bridged_exec(
        mcp_framed(
            3,
            proto::McpArgs::default()
                .with_name("bash")
                .with_tool_name("bash")
                .with_tool_call_id("call-1")
                .with_provider_identifier("ganja")
                .with_smart_mode_approval_only(true),
        ),
        roster(),
    )
    .await;

    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::ToolCallStart { .. })),
        "a preflight must not reach the engine as a call: {events:?}"
    );

    assert!(
        sent.iter()
            .filter_map(|message| message.exec_response.as_option())
            .filter_map(|response| response.mcp_result.as_option())
            .any(|result| result.approved.is_set()),
        "the preflight is answered `approved`"
    );
    assert!(
        sent.iter().any(|message| message
            .exec_control
            .as_option()
            .is_some_and(|control| control.stream_close.is_set())),
        "and then the close that ends the exec"
    );
}

/// An argument map this build cannot read fails **that one call** with the
/// error arm and never the turn — the model reads it, tries something else,
/// and the reply still arrives.
#[tokio::test]
async fn an_unreadable_argument_map_fails_one_call_and_not_the_turn() {
    let unreadable = proto::McpArgs::default()
        .with_name("read")
        .with_tool_name("read")
        .with_tool_call_id("call-1")
        .with_provider_identifier("ganja");
    let unreadable = proto::McpArgs {
        args: vec![proto::McpArgEntry {
            key: Some("filePath".to_owned()),
            // A value with no arm set: on the wire a oneof always writes its
            // case, so this is a shape this build refuses rather than guesses.
            value: buffa::MessageField::some(proto::JsonValue::default()),
            ..Default::default()
        }],
        ..unreadable
    };

    let result =
        answered_mcp(unreadable, roster()).await.expect("an unreadable map is answered here");
    assert!(
        result
            .error
            .as_option()
            .and_then(|error| error.error.as_deref())
            .is_some_and(|error| error.contains("could not read the argument values")),
        "{result:?}"
    );
}

/// A turn that declared **no** tools keeps the answer it gave before the
/// bridge: with nothing declared there is no roster to be missing from, so the
/// sentence is about the name that was called.
#[tokio::test]
async fn a_turn_declaring_no_tools_still_refuses_a_call_by_name() {
    let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, answered) = futures::channel::mpsc::unbounded();
    let stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex::for_tests(answers, Vec::new()),
        None,
    );

    let mut body =
        mcp_framed(3, proto::McpArgs::default().with_name("read").with_tool_call_id("call-1"));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));
    sender.unbounded_send(Ok(body)).expect("the body is open");
    drop(sender);

    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("a refused exec ends the exchange");
    assert_eq!(
        events,
        vec![ProviderEvent::Finish(FinishReason::Completed)],
        "a refusal is an answer, never an event"
    );

    let sent = sent(answered).await;
    assert!(
        sent.iter()
            .filter_map(|message| message.exec_response.as_option())
            .filter_map(|response| response.mcp_result.as_option())
            .filter_map(|result| result.rejected.as_option())
            .any(|rejected| rejected
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no tool named read"))),
        "with no roster declared, the answer is about the name: {sent:?}"
    );
}

/// A wire that cannot pause refuses a call it **does** serve for the reason
/// that actually holds.
///
/// `read` is on this roster, so "no tool named read is served by this client"
/// would be false: what is missing is the hold, not the tool. Reachable by no
/// shipped session — every one of them has a message to key on — which is
/// exactly why it is worth a test: nothing else would ever read the sentence.
#[tokio::test]
async fn a_call_a_pauseless_wire_cannot_hold_is_refused_for_the_hold_and_not_the_roster() {
    let (sender, receiver) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, answered) = futures::channel::mpsc::unbounded();
    // No bridge: a fold with no key to park under is the state this refusal
    // is about.
    let stream = super::events(
        receiver,
        CancellationToken::new(),
        super::Duplex::for_tests(answers, roster()),
        None,
    );

    let mut body = mcp_framed(
        3,
        proto::McpArgs::default()
            .with_name("read")
            .with_tool_name("read")
            .with_tool_call_id("call-1")
            .with_provider_identifier("ganja"),
    );
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));
    sender.unbounded_send(Ok(body)).expect("the body is open");
    drop(sender);

    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("a refused exec ends the exchange");
    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::ToolCallStart { .. })),
        "a wire that cannot hold the run does not hand the call over: {events:?}"
    );

    let sent = sent(answered).await;
    let reason = sent
        .iter()
        .filter_map(|message| message.exec_response.as_option())
        .filter_map(|response| response.mcp_result.as_option())
        .filter_map(|result| result.rejected.as_option())
        .find_map(|rejected| rejected.reason.clone())
        .unwrap_or_else(|| panic!("the call is refused on the rejected arm: {sent:?}"));

    assert!(reason.contains("could not be paused"), "the reason is the hold: {reason}");
    assert!(
        !reason.contains("no tool named"),
        "and never the roster's sentence, which is false here: {reason}"
    );
}

/// A native exec whose ganja tool is **not** on this request's roster keeps
/// D550's typed refusal — a turn not offering `bash` does not run a shell
/// because the server asked for one.
///
/// The roster is the only thing standing between this exec and a bridged
/// shell, and the harness is what makes that observable: the same frame
/// against a roster holding `bash` pauses the Run and emits a
/// `ToolCallStart`, which the second half asserts, so the absence in the
/// first half is a measurement of the gate rather than of the harness.
#[tokio::test]
async fn a_native_exec_whose_tool_is_not_offered_keeps_the_typed_refusal() {
    let readonly = vec![ToolDefinition {
        name: "read".to_owned(),
        description: "Reads a file.".to_owned(),
        schema: serde_json::json!({ "type": "object" }),
    }];

    let (events, sent) = bridged_exec(shell_stream_framed(5), readonly).await;
    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::ToolCallStart { .. })),
        "a tool this turn does not offer is not run because the server asked: {events:?}"
    );
    assert!(
        sent.iter()
            .filter_map(|message| message.exec_response.as_option())
            .filter_map(|response| response.shell_stream.as_option())
            .any(|event| event.rejected.is_set()),
        "the streamed kind's own rejected event, exactly as before the bridge"
    );

    let (offered, answered) = bridged_exec(shell_stream_framed(5), roster()).await;
    assert!(
        offered.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCallStart { name, .. } if name == "bash"
        )),
        "and the same exec against a roster that does offer bash is bridged: {offered:?}"
    );
    assert!(
        !answered
            .iter()
            .filter_map(|message| message.exec_response.as_option())
            .any(|response| response.shell_stream.is_set()),
        "which answers nothing on the wire until the engine has run it: {answered:?}"
    );
}

/// **Dv-11.** The provider takes its credential as a value
/// ([`CursorProvider::at`] says why), so a caller that must never read
/// `auth.json` has no code path to it, and an engine-level bridge suite is
/// one binary rather than one binary per test.
///
/// The endpoint rule is not relaxed by the credential being handed over: a
/// token still may not travel anywhere a key could not.
#[test]
fn a_provider_may_be_given_its_credential_instead_of_a_store_to_read() {
    let credential = crate::provider::CredentialSource::key("at-cursor-canary")
        .expect("a non-blank token is a credential");
    let provider = CursorProvider::at("http://127.0.0.1:4096", credential.clone())
        .expect("loopback never reaches a network");

    let rendered = format!("{provider:?}");
    assert!(rendered.contains("127.0.0.1:4096"), "{rendered}");
    assert!(
        !rendered.contains("at-cursor-canary"),
        "a provider renders where it points, never what it presents: {rendered}"
    );

    let refused = CursorProvider::at("http://api2.cursor.sh", credential)
        .expect_err("plain http to a public host puts the token on the wire in the clear");
    assert!(matches!(refused, ProviderError::Transport(_)), "{refused:?}");

    assert!(
        crate::provider::CredentialSource::key("   ").is_none(),
        "a blank credential is refused at construction, not as a 401 mid-turn"
    );
}
