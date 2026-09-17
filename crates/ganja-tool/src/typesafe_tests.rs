use std::collections::BTreeMap;
use std::time::Duration;

use fixture::{Endpoint, Reply, answer, canned, redirect};
use tokio_util::sync::CancellationToken;

use super::{
    Answer, Client, DEFAULT_MODEL, Error, MAX_BODY, MAX_QUESTIONS, NoulCriteria, PREVIEW_MODEL,
    Question, Request, Settings, State, Usage,
};
use crate::ToolError;

/// The loopback HTTP fixture the `evaluate` tests share.
///
/// `websearch_tests.rs`'s endpoint accepts exactly one connection and keeps
/// exactly one request, which cannot serve a suite whose whole claim is that
/// a refusal was **not** retried: proving "one attempt" needs a listener that
/// would have accepted a second. So this one loops, counts, and answers every
/// connection with `connection: close`.
///
/// `pub(crate)` rather than private because `evaluate_tests.rs` drives the
/// same vendor through the tool above this client and must not grow a second
/// copy of the server.
#[cfg(test)]
pub(crate) mod fixture {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// What the endpoint does with a connection.
    #[derive(Clone)]
    pub(crate) enum Reply {
        /// Write these bytes back, then close.
        Canned(Vec<u8>),
        /// Accept and never answer, which is what a deadline is tested
        /// against.
        Silent,
    }

    /// A loopback endpoint that answers every connection the same way and
    /// keeps every request it was sent.
    pub(crate) struct Endpoint {
        /// What to hand [`super::Settings::base_from`].
        base: String,
        /// Each request as it arrived, headers and body.
        seen: Arc<Mutex<Vec<String>>>,
        /// Kept so the server outlives the test talking to it, and ends with
        /// it: a bare `JoinHandle` detaches on drop, which under `cargo
        /// test`'s shared binary leaves one accept loop per fixture running
        /// for the rest of the run.
        _server: tokio_util::task::AbortOnDropHandle<()>,
    }

    impl Endpoint {
        /// The base URL a client is pointed at.
        pub(crate) fn base(&self) -> &str {
            &self.base
        }

        /// How many requests arrived. The number the no-retry claims read.
        pub(crate) fn count(&self) -> usize {
            self.requests().len()
        }

        /// Every request, in arrival order.
        pub(crate) fn requests(&self) -> Vec<String> {
            self.seen.lock().expect("the request log is never poisoned").clone()
        }

        /// The first request, which for a one-attempt client is the only one.
        pub(crate) fn first(&self) -> String {
            self.requests().first().cloned().unwrap_or_default()
        }
    }

    /// An endpoint replying `reply` to every connection.
    pub(crate) async fn serve(reply: Reply) -> Endpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
        let base =
            format!("http://{}", listener.local_addr().expect("a bound socket has an address"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let reply = reply.clone();
                let log = Arc::clone(&log);

                tokio::spawn(async move {
                    // Headers and body both: the body is what the wire
                    // assertions read, so the read runs until the declared
                    // length has arrived.
                    let mut request = Vec::new();
                    let mut chunk = vec![0_u8; 8192];
                    while let Ok(read) = socket.read(&mut chunk).await {
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        if complete(&request) {
                            break;
                        }
                    }
                    log.lock()
                        .expect("the request log is never poisoned")
                        .push(String::from_utf8_lossy(&request).into_owned());

                    match reply {
                        Reply::Canned(bytes) => {
                            let _ = socket.write_all(&bytes).await;
                            let _ = socket.flush().await;
                        }
                        // Long enough that no deadline under test outlives
                        // it, short enough that a leaked task ends.
                        Reply::Silent => tokio::time::sleep(Duration::from_secs(60)).await,
                    }
                });
            }
        });

        Endpoint { base, seen, _server: tokio_util::task::AbortOnDropHandle::new(server) }
    }

    /// Whether `request` holds a whole request: the headers, and as many body
    /// bytes as its `content-length` declared.
    fn complete(request: &[u8]) -> bool {
        let text = String::from_utf8_lossy(request);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            return false;
        };
        let declared = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or_default();

        body.len() >= declared
    }

    /// A response with `status`, `body` as JSON, and the connection closed
    /// after it — so the next attempt, if there were one, would have to open
    /// a second connection the listener would count.
    pub(crate) fn canned(status: u16, body: &str) -> Reply {
        Reply::Canned(
            format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        )
    }

    /// A 200 carrying `body`.
    pub(crate) fn answer(body: &str) -> Reply {
        canned(200, body)
    }

    /// A 302 pointing at `location`, which nothing may follow.
    pub(crate) fn redirect(location: &str) -> Reply {
        Reply::Canned(
            format!(
                "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\n\
                 connection: close\r\n\r\n"
            )
            .into_bytes(),
        )
    }
}

/// A canned answer carrying one of each type, matching [`three_questions`].
const ANSWERED: &str = r#"{"model":"jev-1.13.0","answers":{"dept":{"type":"choice","choice":"technical","probabilities":{"billing":0.08,"technical":0.92},"confidence":0.82},"frustration":{"type":"score","score":1.6,"legend":{"0":"calm","1":"angry"},"probabilities":{"0":0.4,"1":0.6},"confidence":0.78},"urgent":{"type":"noul","noul":0.92}},"usage":{"input_tokens":312,"output_tokens":48}}"#;

/// The state every wire test sends, kept distinctive so the leak check can
/// look for it by name.
const STATE: &str = "payouts have been failing for three days";

/// The key every wire test sends, likewise.
const KEY: &str = "sk-typesafe-fixture-0123456789";

/// One question of each type, which is what criterion 1a's body is.
fn three_questions() -> BTreeMap<String, Question> {
    BTreeMap::from([
        (
            "dept".to_owned(),
            Question::Choice {
                instructions: serde_json::json!("Which team should handle this?"),
                criteria: BTreeMap::from([
                    ("billing".to_owned(), Some("payments".to_owned())),
                    ("technical".to_owned(), None),
                ]),
            },
        ),
        (
            "frustration".to_owned(),
            Question::Score {
                instructions: serde_json::json!("How frustrated is the customer?"),
                criteria: vec!["calm".to_owned(), "angry".to_owned()],
            },
        ),
        (
            "urgent".to_owned(),
            Question::Noul {
                instructions: serde_json::json!("Does this convey urgency?"),
                criteria: Some(NoulCriteria { yes: Some("time-sensitive".to_owned()), no: None }),
            },
        ),
    ])
}

/// The smallest valid question set, for tests that are about the exchange
/// rather than about the body.
fn one_question() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        "urgent".to_owned(),
        Question::Noul { instructions: serde_json::json!("Urgent?"), criteria: None },
    )])
}

/// A request over `questions`, which the caller has already made valid.
fn request(questions: BTreeMap<String, Question>, model: &str) -> Request {
    Request::checked(State::Text(STATE.to_owned()), questions, model.to_owned())
        .expect("the fixture request is within every limit")
}

/// A client pointed at `endpoint`, under the default deadline.
fn client(endpoint: &Endpoint) -> Client {
    within(endpoint, super::TIMEOUT)
}

/// A client pointed at `endpoint`, under `deadline`.
fn within(endpoint: &Endpoint, deadline: Duration) -> Client {
    let base = Settings::base_from(endpoint.base()).expect("a loopback base is accepted");

    let settings = Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned())
        .expect("a checked base joins the endpoint path")
        .within(deadline);

    Client::new(settings).expect("an HTTP client builds")
}

/// What one evaluation against `reply` answered, and what the endpoint saw.
async fn evaluate(reply: Reply, request: &Request) -> (Result<super::Response, Error>, Endpoint) {
    let endpoint = fixture::serve(reply).await;
    let answered = client(&endpoint).evaluate(request, &CancellationToken::new()).await;

    (answered, endpoint)
}

#[test]
fn a_tagged_question_round_trips_and_refuses_an_unknown_key() {
    let json = r#"{"type":"choice","instructions":"which","criteria":{"a":null,"b":"bee"}}"#;
    let question: Question = serde_json::from_str(json).expect("the tagged form decodes");

    assert_eq!(serde_json::to_string(&question).expect("it encodes"), json);
    assert!(
        serde_json::from_str::<Question>(
            r#"{"type":"choice","instructions":"which","criteria":{},"nope":1}"#
        )
        .is_err(),
        "an unknown key is refused rather than ignored"
    );
    assert!(
        serde_json::from_str::<Question>(r#"{"type":"nope","instructions":"x"}"#).is_err(),
        "an unknown question type is refused"
    );
}

#[tokio::test]
async fn one_evaluation_posts_the_endpoint_path_the_bearer_key_and_exactly_this_body() {
    let (answered, endpoint) =
        evaluate(answer(ANSWERED), &request(three_questions(), DEFAULT_MODEL)).await;
    let answered = answered.expect("the canned answer parses");
    let seen = endpoint.first();
    let (head, body) = seen.split_once("\r\n\r\n").expect("the request has a body");

    assert!(head.starts_with("POST /v1/systemone HTTP/1.1\r\n"), "request line: {head}");
    assert!(
        head.lines().any(|line| line.eq_ignore_ascii_case(&format!("authorization: Bearer {KEY}"))),
        "the key travels as a bearer token: {head}"
    );
    assert!(
        head.lines().any(|line| {
            line.eq_ignore_ascii_case(&format!(
                "user-agent: ganja-code/{}",
                env!("CARGO_PKG_VERSION")
            ))
        }),
        "ganja names itself: {head}"
    );
    assert!(
        head.lines().any(|line| line.eq_ignore_ascii_case("content-type: application/json")),
        "the body is declared as JSON: {head}"
    );
    assert_eq!(
        body,
        format!(
            r#"{{"state":"{STATE}","questions":{{"dept":{{"type":"choice","instructions":"Which team should handle this?","criteria":{{"billing":"payments","technical":null}}}},"frustration":{{"type":"score","instructions":"How frustrated is the customer?","criteria":["calm","angry"]}},"urgent":{{"type":"noul","instructions":"Does this convey urgency?","criteria":{{"true":"time-sensitive"}}}}}},"model":"{DEFAULT_MODEL}"}}"#
        )
    );
    assert_eq!(answered.model, "jev-1.13.0");
    assert_eq!(answered.usage, Usage { input_tokens: 312, output_tokens: 48 });
    assert_eq!(answered.answers.len(), 3);
    assert_eq!(answered.answers["urgent"], Answer::Noul { noul: 0.92 });
    assert_eq!(endpoint.count(), 1);
}

#[tokio::test]
async fn an_answer_type_this_build_does_not_know_is_kept_whole_rather_than_failing_the_response() {
    let body = r#"{"model":"jev-latest","answers":{"urgent":{"type":"quanta","quanta":[0.1,0.9]}},"usage":{"input_tokens":7,"output_tokens":0}}"#;
    let (answered, _endpoint) =
        evaluate(answer(body), &request(one_question(), DEFAULT_MODEL)).await;
    let answered = answered.expect("an unknown answer type does not fail the response");

    assert_eq!(answered.answers["urgent"].kind(), "quanta");
    assert!(matches!(answered.answers["urgent"], Answer::Other(_)));
}

#[tokio::test]
async fn a_refused_credential_is_not_retried_and_tells_the_model_not_to_either() {
    for status in [401_u16, 403, 404] {
        let (answered, endpoint) = evaluate(
            canned(status, r#"{"detail":{"error_type":"authentication_error"}}"#),
            &request(one_question(), DEFAULT_MODEL),
        )
        .await;

        assert_eq!(answered.unwrap_err(), Error::Rejected { status });
        assert_eq!(endpoint.count(), 1, "HTTP {status} was not retried");

        let ToolError::Failed(message) = ToolError::from(Error::Rejected { status }) else {
            panic!("a rejection is a failure the model reads");
        };
        assert!(message.contains("do not retry"), "the model is told not to retry: {message}");

        // The two cases have different remedies, and a model told to check
        // its API key over a 404 would tell the user something false.
        if status == 404 {
            assert!(
                !message.contains(super::KEY_ENV) && message.contains(super::BASE_ENV),
                "a 404 names the endpoint, not the credential: {message}"
            );
        } else {
            assert!(message.contains(super::KEY_ENV), "a {status} names the credential: {message}");
        }
    }
}

#[tokio::test]
async fn an_unavailable_vendor_is_not_retried_either_and_the_judgement_is_simply_lost() {
    for status in [429_u16, 529, 500] {
        let (answered, endpoint) =
            evaluate(canned(status, "{}"), &request(one_question(), DEFAULT_MODEL)).await;

        assert_eq!(answered.unwrap_err(), Error::Unavailable { status });
        assert_eq!(endpoint.count(), 1, "HTTP {status} was not retried");

        let ToolError::Failed(message) = ToolError::from(Error::Unavailable { status }) else {
            panic!("an unavailable vendor is a failure the model reads");
        };
        assert!(
            message.contains("continue without this judgement"),
            "the model is told to carry on: {message}"
        );
    }
}

#[tokio::test]
async fn a_validation_refusal_names_the_field_without_quoting_the_state_back() {
    // The vendor's own 422, measured against the live API on 2026-09-18: a
    // pydantic fault array whose every element echoes the request body back
    // under `input`.
    let body = r#"{"detail":[{"type":"missing","loc":["body","questions"],"msg":"Field required","input":{"state":"PAYLOAD","model":"jev-latest"}}]}"#;
    let (answered, endpoint) =
        evaluate(canned(422, body), &request(one_question(), DEFAULT_MODEL)).await;

    assert_eq!(
        answered.unwrap_err(),
        Error::Invalid { detail: "body.questions: Field required".to_owned() }
    );
    assert_eq!(endpoint.count(), 1);

    // And the vendor's 401 shape, measured the same day, which carries its
    // sentence under `detail.message`.
    let rejected = r#"{"detail":{"error_type":"authentication_error","message":"Cannot authenticate with the server."}}"#;
    let (answered, _endpoint) =
        evaluate(canned(422, rejected), &request(one_question(), DEFAULT_MODEL)).await;

    assert_eq!(
        answered.unwrap_err(),
        Error::Invalid { detail: "Cannot authenticate with the server.".to_owned() }
    );

    // A fault element carrying nothing but the request echo says so, rather
    // than rendering the element — which is the fallback that would put the
    // state back into what the model reads.
    let only_input = r#"{"detail":[{"input":{"state":"PAYLOAD","model":"jev-latest"}}]}"#;
    let (answered, _endpoint) =
        evaluate(canned(422, only_input), &request(one_question(), DEFAULT_MODEL)).await;
    let Err(Error::Invalid { detail }) = answered else {
        panic!("a 422 is a validation refusal");
    };
    assert!(!detail.contains("PAYLOAD"), "the echo never travels: {detail}");
    assert!(!detail.is_empty(), "and the model is told something: {detail}");

    // A `detail` shape this build cannot read at all is the same case.
    for unreadable in [r#"{"detail":42}"#, r#"{"error":"nope"}"#, "{}", "[]"] {
        let (answered, _endpoint) =
            evaluate(canned(422, unreadable), &request(one_question(), DEFAULT_MODEL)).await;
        let Err(Error::Invalid { detail }) = answered else {
            panic!("a 422 is a validation refusal");
        };
        assert!(!detail.is_empty(), "{unreadable} still says something");
        assert!(!detail.contains("nope"), "an unknown envelope is not rendered whole: {detail}");
    }

    // A proxy's plain-text refusal is the sentence a reader wants, so it is
    // passed on as the text it was.
    let (answered, _endpoint) =
        evaluate(canned(422, "upstream said no"), &request(one_question(), DEFAULT_MODEL)).await;
    assert_eq!(answered.unwrap_err(), Error::Invalid { detail: "upstream said no".to_owned() });

    // A body that is not JSON at all is still a sentence, and an enormous one
    // is cut rather than carried whole into what the model reads.
    let huge = format!(r#"{{"detail":"{}"}}"#, "d".repeat(8 * 1024));
    let (answered, _endpoint) =
        evaluate(canned(422, &huge), &request(one_question(), DEFAULT_MODEL)).await;
    let Err(Error::Invalid { detail }) = answered else {
        panic!("a 422 is a validation refusal");
    };
    assert!(detail.len() <= 2 * 1024 + "…".len(), "the detail is clamped: {} bytes", detail.len());
}

#[tokio::test]
async fn a_vendor_that_accepts_the_connection_and_never_answers_runs_out_of_deadline() {
    // Real time, not a paused clock: under `start_paused` tokio auto-advances
    // while real socket I/O is pending, so the deadline would fire against an
    // answering listener too and this test could not fail. 200 ms is a
    // hundredfold margin over a loopback round trip and still cheap.
    let deadline = Duration::from_millis(200);

    // The control first, deliberately. It is the assertion a loaded machine
    // breaks, and running it first means such a machine fails *here* — where
    // the message names the deadline — rather than leaving a green timeout
    // assertion that would hold for any client at all.
    let answering = fixture::serve(answer(ANSWERED)).await;

    assert!(
        within(&answering, deadline)
            .evaluate(&request(three_questions(), DEFAULT_MODEL), &CancellationToken::new())
            .await
            .is_ok(),
        "a loopback answer arrives well inside {deadline:?}"
    );

    let silent = fixture::serve(Reply::Silent).await;

    assert_eq!(
        within(&silent, deadline)
            .evaluate(&request(one_question(), DEFAULT_MODEL), &CancellationToken::new())
            .await
            .unwrap_err(),
        Error::Timeout
    );
}

#[tokio::test]
async fn cancelling_the_turn_ends_the_request_rather_than_waiting_out_the_deadline() {
    let silent = fixture::serve(Reply::Silent).await;
    let cancel = CancellationToken::new();
    let asked = request(one_question(), DEFAULT_MODEL);
    let token = cancel.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
    });

    let failed = client(&silent).evaluate(&asked, &cancel).await.unwrap_err();

    assert_eq!(failed, Error::Cancelled);
    assert!(matches!(ToolError::from(failed), ToolError::Cancelled));

    // Already cancelled before the call: no socket is opened at all, so the
    // content is not sent for a judgement nobody is waiting for.
    let unwanted = fixture::serve(answer(ANSWERED)).await;
    let spent = CancellationToken::new();
    spent.cancel();

    assert_eq!(client(&unwanted).evaluate(&asked, &spent).await.unwrap_err(), Error::Cancelled);
    assert_eq!(unwanted.count(), 0, "a cancelled turn reaches no vendor");
}

#[tokio::test]
async fn a_redirect_is_a_failure_naming_its_status_and_the_target_is_never_asked() {
    let target = fixture::serve(answer(ANSWERED)).await;
    let moved = fixture::serve(redirect(&format!("{}/v1/systemone", target.base()))).await;
    let asked = request(one_question(), DEFAULT_MODEL);

    assert_eq!(
        client(&moved).evaluate(&asked, &CancellationToken::new()).await.unwrap_err(),
        Error::Unavailable { status: 302 }
    );
    assert_eq!(moved.count(), 1);
    assert_eq!(target.count(), 0, "the key never reached the redirect's host");
}

#[tokio::test]
async fn an_answer_too_large_to_hold_is_refused_whether_or_not_its_length_was_declared() {
    let oversized =
        format!(r#"{{"model":"x","answers":{{}},"pad":"{}"}}"#, "p".repeat(1024 * 1024));
    let asked = request(one_question(), DEFAULT_MODEL);

    let (declared, _endpoint) = evaluate(answer(&oversized), &asked).await;
    assert_eq!(declared.unwrap_err(), Error::TooLarge);

    // The same bytes with no `content-length` at all, which is the guard on
    // the accumulated stream rather than the one on the header.
    let undeclared = Reply::Canned(
        format!("HTTP/1.1 200 X\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{oversized}")
            .into_bytes(),
    );
    let (streamed, _endpoint) = evaluate(undeclared, &asked).await;
    assert_eq!(streamed.unwrap_err(), Error::TooLarge);
}

#[test]
fn a_base_url_is_accepted_only_where_the_key_would_not_travel_in_the_clear() {
    for refused in [
        "http://example.com",
        "ftp://x",
        "not a url",
        "http://127.0.0.1.evil.com",
        "http://127.0.0.1@evil.com",
        "http://localhost.evil.com",
    ] {
        assert_eq!(
            Settings::base_from(refused).unwrap_err(),
            Error::RefusedBase,
            "{refused} must be refused"
        );
    }
    // A query or a fragment is refused rather than silently dropped by the
    // join: a gateway is exactly where somebody puts a token in a query
    // string, and dropping it would send a request nobody described.
    for carried in
        ["https://eu.example/?token=secret", "https://eu.example/#tail", "http://127.0.0.1:1/?a=b"]
    {
        assert_eq!(
            Settings::base_from(carried).unwrap_err(),
            Error::RefusedBase,
            "{carried} must be refused"
        );
    }
    for accepted in
        ["http://127.0.0.1:1", "http://[::1]:1", "http://localhost:1", "https://eu.example"]
    {
        assert!(Settings::base_from(accepted).is_ok(), "{accepted} must be accepted");
    }
    assert!(
        !Error::RefusedBase.to_string().contains("evil"),
        "the refused URL is never echoed back"
    );
}

#[test]
fn a_base_url_carrying_a_path_prefix_keeps_it_when_the_endpoint_is_joined() {
    let base = Settings::base_from("https://eu.example/typesafe").expect("https is accepted");
    let settings = Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned())
        .expect("a checked base joins the endpoint path");

    assert_eq!(settings.endpoint.as_str(), "https://eu.example/typesafe/v1/systemone");
    assert_eq!(settings.host(), "eu.example");
}

#[tokio::test]
async fn every_limit_is_decided_before_a_socket_is_opened() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;
    let text = |what: &str| serde_json::json!(what);
    let noul = || Question::Noul { instructions: text("Urgent?"), criteria: None };
    let filler = |bytes: usize| State::Text("p".repeat(bytes));

    let mut fifty_one = BTreeMap::new();
    for index in 0..=MAX_QUESTIONS {
        fifty_one.insert(format!("q{index}"), noul());
    }

    let refusals: Vec<(&str, Result<Request, Error>)> = vec![
        (
            "one question past the cap",
            Request::checked(filler(1), fifty_one, DEFAULT_MODEL.to_owned()),
        ),
        (
            "a body one byte over by way of the state",
            Request::checked(filler(MAX_BODY), one_question(), DEFAULT_MODEL.to_owned()),
        ),
        (
            "a body one byte over by way of one question's instructions",
            Request::checked(
                State::Text("small".to_owned()),
                BTreeMap::from([(
                    "urgent".to_owned(),
                    Question::Noul { instructions: text(&"p".repeat(MAX_BODY)), criteria: None },
                )]),
                DEFAULT_MODEL.to_owned(),
            ),
        ),
        (
            "a choice with one option",
            Request::checked(
                filler(1),
                BTreeMap::from([(
                    "dept".to_owned(),
                    Question::Choice {
                        instructions: text("Which?"),
                        criteria: BTreeMap::from([("only".to_owned(), None)]),
                    },
                )]),
                DEFAULT_MODEL.to_owned(),
            ),
        ),
        (
            "a score with one level",
            Request::checked(
                filler(1),
                BTreeMap::from([(
                    "mood".to_owned(),
                    Question::Score {
                        instructions: text("How?"),
                        criteria: vec!["calm".to_owned()],
                    },
                )]),
                DEFAULT_MODEL.to_owned(),
            ),
        ),
        (
            "an id with a space in it",
            Request::checked(
                filler(1),
                BTreeMap::from([("a b".to_owned(), noul())]),
                DEFAULT_MODEL.to_owned(),
            ),
        ),
        (
            "no questions at all",
            Request::checked(filler(1), BTreeMap::new(), DEFAULT_MODEL.to_owned()),
        ),
    ];

    for (what, refused) in refusals {
        assert!(matches!(refused, Err(Error::InvalidRequest(_))), "{what} is refused: {refused:?}");
    }

    // A state that is neither text, object nor array never becomes a `State`
    // at all, so the refusal lands where arguments are parsed.
    assert!(serde_json::from_value::<State>(serde_json::json!(42)).is_err());
    assert!(serde_json::from_value::<State>(serde_json::json!(null)).is_err());
    assert!(serde_json::from_value::<State>(serde_json::json!({"diff": "x"})).is_ok());
    assert!(serde_json::from_value::<State>(serde_json::json!(["a", "b"])).is_ok());

    assert_eq!(endpoint.count(), 0, "not one refusal reached the vendor");
}

#[tokio::test]
async fn the_model_a_call_names_is_what_travels_and_an_alias_passes_through_untouched() {
    let preview = request(one_question(), PREVIEW_MODEL);
    assert_eq!(preview.model(), PREVIEW_MODEL);

    let (answered, endpoint) = evaluate(answer(ANSWERED), &preview).await;
    answered.expect("the canned answer parses");

    assert!(
        endpoint.first().contains(&format!(r#""model":"{PREVIEW_MODEL}""#)),
        "the alias reaches the wire unchanged: {}",
        endpoint.first()
    );

    // A versioned id is equally a free string: nothing here decides which ids
    // the vendor serves.
    let pinned = request(one_question(), "jev-1.13.0");
    let (answered, endpoint) = evaluate(answer(ANSWERED), &pinned).await;
    answered.expect("the canned answer parses");
    assert!(endpoint.first().contains(r#""model":"jev-1.13.0""#));
}

#[test]
fn a_known_answer_type_whose_payload_is_wrong_falls_through_rather_than_failing_the_call() {
    // The tolerance the module doc claims, settled rather than assumed: a
    // `type` this build knows but a payload it cannot read must not cost the
    // other answers in the same response. Serde's untagged fallback variant
    // does fall through here, so no hand-written `Deserialize` is needed —
    // and this test is what keeps that true.
    for renamed in [r#"{"type":"noul"}"#, r#"{"type":"score","value":1.6}"#] {
        let answer: Answer =
            serde_json::from_str(renamed).expect("a known tag with a bad payload still decodes");

        assert!(matches!(answer, Answer::Other(_)), "{renamed} became {answer:?}");
    }

    // And the whole response survives one of them, beside an answer that is
    // perfectly readable.
    let body = r#"{"model":"jev-1.13.0","answers":{"a":{"type":"noul"},"b":{"type":"noul","noul":0.4}},"usage":{"input_tokens":3,"output_tokens":1}}"#;
    let answered: super::Response =
        serde_json::from_str(body).expect("one bad answer is not fatal");

    assert!(matches!(answered.answers["a"], Answer::Other(_)));
    assert_eq!(answered.answers["b"], Answer::Noul { noul: 0.4 });
}

#[tokio::test]
async fn a_transport_failure_never_carries_the_url_it_failed_against() {
    // reqwest's `Display` appends `for url (<url>)` — userinfo included — and
    // this string is model-facing, so the URL is stripped before anything
    // reads it. A base whose port nothing listens on is the cheapest way to
    // make the client fail below HTTP.
    let dead = fixture::serve(answer(ANSWERED)).await;
    let port = dead.base().rsplit(':').next().expect("the base names a port").to_owned();
    drop(dead);

    let base = Settings::base_from(&format!("http://tok:pw@127.0.0.1:{port}"))
        .expect("loopback with userinfo is a legitimate base");
    let settings = Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned())
        .expect("a checked base joins the endpoint path")
        .within(Duration::from_secs(2));
    let failed = Client::new(settings)
        .expect("an HTTP client builds")
        .evaluate(&request(one_question(), DEFAULT_MODEL), &CancellationToken::new())
        .await
        .unwrap_err();

    let Error::Transport(message) = &failed else {
        panic!("a closed port fails below HTTP, not above it: {failed:?}");
    };
    let read_by_the_model = ToolError::from(failed.clone()).to_string();

    for said in [message.as_str(), read_by_the_model.as_str()] {
        assert!(!said.contains("tok"), "no userinfo: {said}");
        assert!(!said.contains("pw"), "no userinfo: {said}");
        assert!(!said.contains('@'), "no userinfo: {said}");
        assert!(!said.contains("127.0.0.1"), "no host: {said}");
        assert!(!said.contains(&port), "no port: {said}");
    }
}

#[test]
fn neither_the_body_nor_the_endpoint_can_reach_a_debug_rendering() {
    let asked = request(three_questions(), DEFAULT_MODEL);
    let rendered = format!("{asked:?}");

    assert!(!rendered.contains(STATE), "the state is not in a request's Debug: {rendered}");
    assert!(rendered.contains(&format!("bytes: {}", asked.body_len())));
    assert!(rendered.contains("questions: 3"));

    let base = Settings::base_from("https://tok:pw@eu.example").expect("https is accepted");
    let settings = Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned())
        .expect("a checked base joins the endpoint path");
    let rendered = format!("{settings:?}");

    assert!(!rendered.contains("tok"), "no userinfo in Debug: {rendered}");
    assert!(!rendered.contains(KEY), "no key in Debug: {rendered}");
    assert!(rendered.contains("eu.example"), "the host is what a reader gets: {rendered}");
}

#[test]
fn the_state_summary_is_computed_where_the_state_is_still_in_hand() {
    let of = |state: State| {
        Request::checked(state, one_question(), DEFAULT_MODEL.to_owned())
            .expect("the fixture request is within every limit")
            .state()
            .to_string()
    };

    assert_eq!(of(State::Text("anything".to_owned())), "text");
    assert_eq!(of(State::Array(vec![serde_json::json!(1); 12])), "array of 12");
    assert_eq!(
        of(State::Object(BTreeMap::from([
            ("diff".to_owned(), serde_json::json!("x")),
            ("policy".to_owned(), serde_json::json!("y")),
        ]))),
        "keys: diff, policy"
    );
}
