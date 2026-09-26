use std::collections::BTreeMap;
use std::time::Duration;

use fixture::{Endpoint, Reply, answer, canned, redirect};
use tokio_util::sync::CancellationToken;

use super::{
    Answer, Client, DEFAULT_MODEL, Error, Instructions, MAX_BASE, MAX_BODY, MAX_QUESTIONS,
    NoulCriteria, PREVIEW_MODEL, Question, Request, Settings, State, Usage,
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
                instructions: Instructions::Text("Which team should handle this?".to_owned()),
                criteria: BTreeMap::from([
                    ("billing".to_owned(), Some("payments".to_owned())),
                    ("technical".to_owned(), None),
                ]),
            },
        ),
        (
            "frustration".to_owned(),
            Question::Score {
                instructions: Instructions::Text("How frustrated is the customer?".to_owned()),
                criteria: vec!["calm".to_owned(), "angry".to_owned()],
            },
        ),
        (
            "urgent".to_owned(),
            Question::Noul {
                instructions: Instructions::Text("Does this convey urgency?".to_owned()),
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
        Question::Noul { instructions: Instructions::Text("Urgent?".to_owned()), criteria: None },
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

    let settings = Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned()).within(deadline);

    Client::new(settings).expect("an HTTP client builds")
}

/// An https base of exactly `len` bytes, padded in its path with a letter
/// that parsing leaves as written.
fn padded_base(len: usize) -> String {
    const HEAD: &str = "https://eu.example/";

    format!("{HEAD}{}", "a".repeat(len - HEAD.len()))
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
        head.lines().any(|line| line.eq_ignore_ascii_case("content-type: application/json")),
        "the body is declared as JSON: {head}"
    );
    assert_eq!(
        body,
        format!(
            r#"{{"state":"{STATE}","model":"{DEFAULT_MODEL}","questions":{{"dept":{{"type":"choice","instructions":"Which team should handle this?","criteria":{{"billing":"payments","technical":null}}}},"frustration":{{"type":"score","instructions":"How frustrated is the customer?","criteria":["calm","angry"]}},"urgent":{{"type":"noul","instructions":"Does this convey urgency?","criteria":{{"true":"time-sensitive"}}}}}}}}"#
        )
    );
    assert_eq!(answered.model, "jev-1.13.0");
    assert_eq!(answered.usage, Usage { input_tokens: 312, output_tokens: 48 });
    assert_eq!(answered.answers.len(), 3);
    assert_eq!(answered.answers["urgent"], Answer::Noul { noul: 0.92 });
    assert_eq!(endpoint.count(), 1);
}

#[tokio::test]
async fn the_headers_the_vendor_receives_are_exactly_these() {
    // The identification posture, asserted on the bytes the loopback vendor
    // received rather than on the builder calls: `User-Agent` names ganja
    // first and the SDK last, `X-TypeSafe-SDK` names the SDK alone, and
    // `X-TypeSafe-Runtime`, which would name the operating system and the
    // CPU architecture, is not sent. Every header name is listed, so one the
    // transport or the SDK starts adding shows up here too.
    let (answered, endpoint) =
        evaluate(answer(ANSWERED), &request(one_question(), DEFAULT_MODEL)).await;
    answered.expect("the canned answer parses");
    let seen = endpoint.first();
    let (head, _body) = seen.split_once("\r\n\r\n").expect("the request has a body");
    let headers: BTreeMap<String, String> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();

    assert_eq!(
        headers.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "accept",
            "authorization",
            "content-length",
            "content-type",
            "host",
            "user-agent",
            "x-typesafe-sdk",
        ],
        "every header the vendor receives, and no x-typesafe-runtime: {head}"
    );
    // One line per name: the map above would fold a header sent twice.
    assert_eq!(head.lines().skip(1).count(), headers.len(), "no header is sent twice: {head}");
    assert_eq!(headers["authorization"], format!("Bearer {KEY}"));
    assert_eq!(headers["accept"], "application/json");
    assert_eq!(headers["content-type"], "application/json");

    let sdk = &headers["x-typesafe-sdk"];
    assert!(sdk.starts_with("typesafe-sdk-rust/"), "the SDK names itself: {head}");
    assert_eq!(
        headers["user-agent"],
        format!("ganja-code/{} {sdk}", env!("CARGO_PKG_VERSION")),
        "ganja's product first, the SDK's last: {head}"
    );
    for platform in [std::env::consts::OS, std::env::consts::ARCH] {
        assert!(!head.contains(platform), "the platform is not named ({platform}): {head}");
    }
}

#[tokio::test]
async fn the_disclosed_byte_count_is_the_byte_count_the_vendor_receives() {
    // The SDK encodes the body when it sends it; the number the consent
    // dialog quotes and the cap is checked against is measured beforehand,
    // with a different encoder. Every shape here is one where two JSON
    // encoders could plausibly disagree: escapes, control characters, the
    // two Unicode line separators, non-ASCII text, integers at the edges,
    // and floats that are written in exponent form.
    let states = [
        State::Text(STATE.to_owned()),
        State::Text(
            "quote \" backslash \\ slash / tab \t nl \n cr \r bell \u{7} del \u{7f}".to_owned(),
        ),
        State::Text("line\u{2028}para\u{2029} 日本語 🦀 é".to_owned()),
        State::Object(BTreeMap::from([
            ("floats".to_owned(), serde_json::json!([0.1, 1e-7, 1.5e300, -0.0, 123_456.789, 2.0])),
            ("ints".to_owned(), serde_json::json!([0, -1, u64::MAX, i64::MIN])),
            ("nested".to_owned(), serde_json::json!({"a": [null, true, false, {"b": "c"}]})),
        ])),
        State::Array(vec![serde_json::json!("x"), serde_json::json!(3.25), serde_json::json!({})]),
    ];
    let questions = {
        let mut asked = three_questions();
        asked.insert(
            "shaped".to_owned(),
            Question::Noul {
                instructions: Instructions::Object(BTreeMap::from([(
                    "ask".to_owned(),
                    serde_json::json!("Is it \"urgent\"?\n"),
                )])),
                criteria: Some(NoulCriteria { yes: None, no: None }),
            },
        );
        asked
    };

    for state in states {
        let asked = Request::checked(state.clone(), questions.clone(), DEFAULT_MODEL.to_owned())
            .expect("every fixture state is within every limit");
        let (answered, endpoint) = evaluate(answer(ANSWERED), &asked).await;
        answered.expect("the canned answer parses");
        let seen = endpoint.first();
        let (_head, body) = seen.split_once("\r\n\r\n").expect("the request has a body");

        assert_eq!(
            asked.body_len(),
            body.len(),
            "the disclosure says {} bytes, the vendor got {} for {state:?}:\n{body}",
            asked.body_len(),
            body.len()
        );
    }
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
async fn an_answer_the_sdk_skipped_unread_is_never_lost_to_reading_it_back() {
    // The SDK passes over an answer of a type it does not model without
    // evaluating it, so it accepts bodies a whole-value parse would refuse.
    // Each one here but the last is such a body, and in each the skipped
    // answer still reaches the model: whole where a value can hold it, by its
    // type where not.
    let usage = r#""usage":{"input_tokens":1,"output_tokens":0}"#;
    let cases = [
        (
            "a number beyond an f64",
            format!(
                r#"{{"model":"jev-1.13.0","answers":{{"urgent":{{"type":"quanta","quanta":[1e400]}},"tail":{{"type":"noul","noul":0.3}}}},{usage}}}"#
            ),
            serde_json::json!({"type": "quanta"}),
        ),
        (
            "a lone surrogate",
            format!(
                r#"{{"model":"jev-1.13.0","answers":{{"urgent":{{"type":"quanta","note":"\ud800"}},"tail":{{"type":"noul","noul":0.3}}}},{usage}}}"#
            ),
            serde_json::json!({"type": "quanta"}),
        ),
        (
            // The SDK keeps the last of a repeated `answers` member; a serde
            // derive refuses the repetition outright.
            "a repeated answers member",
            format!(
                r#"{{"model":"jev-1.13.0","answers":{{"stale":{{"type":"noul","noul":0.9}}}},"answers":{{"urgent":{{"type":"quanta","quanta":[0.1]}},"tail":{{"type":"noul","noul":0.3}}}},{usage}}}"#
            ),
            serde_json::json!({"type": "quanta", "quanta": [0.1]}),
        ),
        (
            // One id answered twice, both of a type the SDK skips. The
            // answers hold one per id, and the raw read keeps the last.
            "one id answered twice, both skipped",
            format!(
                r#"{{"model":"jev-1.13.0","answers":{{"urgent":{{"type":"quanta","quanta":[0.1]}},"urgent":{{"type":"quanta","quanta":[0.2]}},"tail":{{"type":"noul","noul":0.3}}}},{usage}}}"#
            ),
            serde_json::json!({"type": "quanta", "quanta": [0.2]}),
        ),
    ];

    for (what, body, kept) in cases {
        let (answered, _endpoint) =
            evaluate(answer(&body), &request(one_question(), DEFAULT_MODEL)).await;
        let answered =
            answered.unwrap_or_else(|error| panic!("{what}: the SDK decodes it: {error}"));

        assert_eq!(
            answered.answers,
            BTreeMap::from([
                ("tail".to_owned(), Answer::Noul { noul: 0.3 }),
                ("urgent".to_owned(), Answer::Other(kept)),
            ]),
            "{what}"
        );
    }
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
async fn every_401_and_403_is_a_rejection_whatever_error_type_the_vendor_names() {
    // The SDK tells a 403 for a request without a key
    // (`authentication_error`) from a 403 for a key that lacks a permission
    // (`permission_denied`). This client keeps both, and every other 401 and
    // 403, a rejection: the judge reads a 401 as "switch off" and a 403 as
    // "skip this segment", and which 403s belong with the 401 is not decided
    // here.
    for (status, body) in [
        (401_u16, r#"{"detail":{"error_type":"authentication_error","message":"Bad key."}}"#),
        (401, ""),
        (403, r#"{"detail":{"error_type":"authentication_error","message":"No key."}}"#),
        (403, r#"{"detail":{"error_type":"permission_denied","message":"Not allowed."}}"#),
        (403, r#"{"detail":"Forbidden"}"#),
        (403, ""),
    ] {
        let (answered, endpoint) =
            evaluate(canned(status, body), &request(one_question(), DEFAULT_MODEL)).await;

        assert_eq!(answered.unwrap_err(), Error::Rejected { status }, "HTTP {status} {body:?}");
        assert_eq!(endpoint.count(), 1, "HTTP {status} {body:?} was not retried");
    }

    // The SDK counts `authentication_error` as a credential failure under any
    // status. That does not widen what a rejection is: a 5xx naming it is
    // still a vendor that might answer later.
    let (answered, _endpoint) = evaluate(
        canned(500, r#"{"detail":{"error_type":"authentication_error"}}"#),
        &request(one_question(), DEFAULT_MODEL),
    )
    .await;
    assert_eq!(answered.unwrap_err(), Error::Unavailable { status: 500 });
}

#[tokio::test]
async fn a_refusal_whose_body_breaks_off_loses_its_status_and_is_a_transport_failure() {
    // Accepted from the SDK rather than decided here: a response whose body
    // cannot be read is a broken connection to it, whatever status line came
    // first. So a 401 or a 422 cut off mid-body is `Transport` (exit 5,
    // "unavailable"), not `Rejected` or `Invalid` (exit 4, "refused").
    for status in [401_u16, 422] {
        let cut_short = Reply::Canned(
            format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\
                 connection: close\r\n\r\n{{\"detail\":"
            )
            .into_bytes(),
        );
        let (answered, endpoint) =
            evaluate(cut_short, &request(one_question(), DEFAULT_MODEL)).await;

        let Err(Error::Transport(said)) = &answered else {
            panic!("HTTP {status} with a body cut short is a transport failure: {answered:?}");
        };
        assert!(!said.contains(&status.to_string()), "the status is lost: {said}");
        assert_eq!(endpoint.count(), 1, "HTTP {status} was not retried");
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
        // Userinfo, on a base that is otherwise fine: the key travels as the
        // bearer token, so nothing would send these as their author meant.
        "https://tok:pw@eu.example",
        "https://tok@eu.example",
        "http://user:pw@127.0.0.1:1",
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
    // A host an HTTP request cannot carry, and a base past `MAX_BASE`. `url`
    // admits these characters in a host and the HTTP client does not; an
    // unexpanded template is where they come from. The length is measured
    // once parsed: a `{` in a path is percent-encoded to three bytes, so the
    // last row is inside `MAX_BASE` as written and past it as handed on.
    let too_long = padded_base(MAX_BASE + 1);
    let too_long_once_parsed = format!("https://eu.example/{}", "{".repeat(MAX_BASE / 3));
    assert!(too_long_once_parsed.len() <= MAX_BASE, "the last row is inside the bound as written");
    for uncarried in [
        "https://{{host}}/v1",
        "https://${host}",
        "https://a\"b.example",
        "https://a`b.example",
        too_long.as_str(),
        too_long_once_parsed.as_str(),
    ] {
        assert_eq!(
            Settings::base_from(uncarried).unwrap_err(),
            Error::RefusedBase,
            "{uncarried} must be refused"
        );
    }
    let longest = padded_base(MAX_BASE);
    for accepted in [
        "http://127.0.0.1:1",
        "http://[::1]:1",
        "http://localhost:1",
        "https://eu.example",
        // The characters refused in a host are percent-encoded in a path, and
        // a base of exactly `MAX_BASE` bytes is inside the bound.
        "https://eu.example/{{prefix}}/v1",
        "https://eu.example/a\"b`c",
        longest.as_str(),
    ] {
        assert!(Settings::base_from(accepted).is_ok(), "{accepted} must be accepted");
    }
    assert!(
        !Error::RefusedBase.to_string().contains("evil"),
        "the refused URL is never echoed back"
    );
}

#[tokio::test]
async fn a_base_url_carrying_a_path_prefix_keeps_it_when_the_endpoint_is_joined() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;

    for prefixed in
        [format!("{}/typesafe", endpoint.base()), format!("{}/typesafe/", endpoint.base())]
    {
        let base = Settings::base_from(&prefixed).expect("loopback is accepted");
        let settings = Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned());
        assert_eq!(settings.host(), "127.0.0.1");

        Client::new(settings)
            .expect("an HTTP client builds")
            .evaluate(&request(one_question(), DEFAULT_MODEL), &CancellationToken::new())
            .await
            .expect("the canned answer parses");
    }

    for seen in endpoint.requests() {
        assert!(
            seen.starts_with("POST /typesafe/v1/systemone HTTP/1.1\r\n"),
            "the prefix is kept, with one slash before the endpoint path: {seen}"
        );
    }
    assert_eq!(endpoint.count(), 2);
}

/// **D567.** The public door refuses what [`Settings::from_env`] refuses, on
/// the same two rules and in the same order, so a caller holding values
/// rather than variables — a judge built from configuration, a test that
/// must never read a developer's exported key — cannot build settings the
/// environment could not have.
#[test]
fn settings_from_parts_refuse_a_cleartext_base_and_an_unusable_model_and_accept_loopback() {
    let refused = [
        ("http://example.com", "jev-1.13.0", Error::RefusedBase, "plain http off loopback"),
        ("http://127.0.0.1.evil.com", "jev-1.13.0", Error::RefusedBase, "a domain, not loopback"),
        ("https://eu.example/?token=t", "jev-1.13.0", Error::RefusedBase, "a query to drop"),
        ("https://tok@eu.example", "jev-1.13.0", Error::RefusedBase, "userinfo the SDK refuses"),
        ("not a url", "jev-1.13.0", Error::RefusedBase, "no URL at all"),
        ("https://api.typesafe.ai", "jev latest", Error::RefusedModel, "a space"),
        ("https://api.typesafe.ai", "", Error::RefusedModel, "an empty id"),
        (
            "https://api.typesafe.ai",
            "jev-latest · 0 B · 0 question(s)",
            Error::RefusedModel,
            "a second disclosure forged into the title",
        ),
        // Both wrong: the base is decided first, as `from_env` decides it.
        ("http://example.com", "jev latest", Error::RefusedBase, "both refused"),
    ];
    for (base, model, expected, what) in refused {
        let refusal = Settings::from_parts(KEY.to_owned(), base, model.to_owned())
            .expect_err(&format!("{what} is refused: {base} / {model:?}"));

        assert_eq!(refusal, expected, "{what}: {base} / {model:?}");
    }

    // The base as the SDK is handed it. The SDK joins the endpoint path onto
    // it, and `a_base_url_carrying_a_path_prefix_keeps_it_when_the_endpoint_is_joined`
    // holds that join to the request line the vendor receives.
    let accepted = [
        ("http://127.0.0.1:8080", "127.0.0.1", "http://127.0.0.1:8080/"),
        ("http://[::1]:1", "[::1]", "http://[::1]:1/"),
        ("http://localhost:1", "localhost", "http://localhost:1/"),
        ("https://eu.example/typesafe", "eu.example", "https://eu.example/typesafe"),
        // What a host may not hold, a path holds percent-encoded.
        ("https://eu.example/{{prefix}}", "eu.example", "https://eu.example/%7B%7Bprefix%7D%7D"),
    ];
    for (base, host, handed) in accepted {
        let settings = Settings::from_parts(KEY.to_owned(), base, "jev-1.13.0".to_owned())
            .unwrap_or_else(|refusal| panic!("{base} is accepted: {refusal}"));

        assert_eq!(settings.host(), host, "{base}");
        assert_eq!(settings.base.as_str(), handed, "{base}: the base is carried as checked");
        assert_eq!(settings.model(), "jev-1.13.0", "{base}: the model is carried as given");
    }
}

/// A base URL an HTTP request cannot carry is refused where the other
/// base-URL rules are, so the sentence a person reads names
/// [`super::BASE_ENV`], not the key. [`Client::new`] never answers
/// `RefusedBase`: it is where the key is judged, and a base that reached it
/// would be reported against the key.
#[test]
fn a_base_an_http_request_cannot_carry_is_refused_as_the_base_and_never_as_the_key() {
    let too_long = padded_base(MAX_BASE + 1);
    for base in ["https://{{host}}/v1", too_long.as_str()] {
        let refused = Settings::from_parts(KEY.to_owned(), base, DEFAULT_MODEL.to_owned())
            .and_then(Client::new)
            .err()
            .unwrap_or_else(|| panic!("{base} builds no client"));

        assert_eq!(refused, Error::RefusedBase, "{base} is the base's refusal, not the key's");
        let said = refused.to_string();
        assert!(said.contains(super::BASE_ENV), "the base's variable is named: {said}");
        assert!(!said.contains(super::KEY_ENV), "the key is not blamed: {said}");
    }

    // And no more than that is refused: the same characters in a path, and a
    // base of exactly `MAX_BASE` bytes, each build a client.
    let longest = padded_base(MAX_BASE);
    for base in [
        "https://eu.example/{{prefix}}/v1",
        "https://eu.example/${prefix}",
        "https://eu.example/a\"b`c",
        longest.as_str(),
    ] {
        let built = Settings::from_parts(KEY.to_owned(), base, DEFAULT_MODEL.to_owned())
            .and_then(Client::new);

        assert!(built.is_ok(), "{base} builds a client: {:?}", built.err());
    }
}

#[tokio::test]
async fn every_limit_is_decided_before_a_socket_is_opened() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;
    let text = |what: &str| Instructions::Text(what.to_owned());
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

#[tokio::test]
async fn a_known_answer_type_whose_payload_is_wrong_fails_the_response_and_names_the_field() {
    // The SDK's rule: an answer that names a `type` this build knows and then
    // contradicts it is not a shape to guess at, so the whole response is
    // malformed — and the model is told to carry on without the judgement
    // rather than handed half of one. What differs from an unknown `type` is
    // that nothing here is new: it is the vendor's own shape, broken.
    let body = r#"{"model":"jev-1.13.0","answers":{"a":{"type":"noul"},"b":{"type":"noul","noul":0.4}},"usage":{"input_tokens":3,"output_tokens":1}}"#;
    let asked = request(
        BTreeMap::from([
            (
                "a".to_owned(),
                Question::Noul {
                    instructions: Instructions::Text("A?".to_owned()),
                    criteria: None,
                },
            ),
            (
                "b".to_owned(),
                Question::Noul {
                    instructions: Instructions::Text("B?".to_owned()),
                    criteria: None,
                },
            ),
        ]),
        DEFAULT_MODEL,
    );
    let (answered, endpoint) = evaluate(answer(body), &asked).await;

    let Err(Error::Malformed(why)) = answered else {
        panic!("a known type with a broken payload fails the response: {answered:?}");
    };
    assert_eq!(why.matches("answers.a").count(), 1, "the field is named, once: {why}");
    assert!(!why.contains(endpoint.base()), "and the endpoint is not: {why}");
    assert!(
        ToolError::from(Error::Malformed(why))
            .to_string()
            .contains("continue without this judgement"),
        "the model is told to carry on"
    );
}

#[tokio::test]
async fn a_transport_failure_never_carries_the_url_it_failed_against() {
    // What the model reads of a failure below HTTP is one fixed sentence: no
    // URL (a gateway may carry a token in its path), and none of the
    // transport's error chain, which can hold text a server or a proxy chose
    // — a certificate's names, an HTTP/2 GOAWAY's debug data. Two failures:
    // a port nothing listens on, and a TLS handshake answered in plain HTTP,
    // whose chain is rustls's own words.
    let dead = fixture::serve(answer(ANSWERED)).await;
    let closed = dead.base().rsplit(':').next().expect("the base names a port").to_owned();
    drop(dead);

    let plaintext = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("loopback binds");
    let speaking = plaintext.local_addr().expect("a bound socket has an address").port();
    let _server = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;

        while let Ok((mut socket, _)) = plaintext.accept().await {
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
        }
    }));

    for (scheme, port) in [("http", closed), ("https", speaking.to_string())] {
        let base = Settings::base_from(&format!("{scheme}://127.0.0.1:{port}/gateway-tok3n"))
            .expect("loopback is a legitimate base");
        let settings = Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned())
            .within(Duration::from_secs(2));
        let failed = Client::new(settings)
            .expect("an HTTP client builds")
            .evaluate(&request(one_question(), DEFAULT_MODEL), &CancellationToken::new())
            .await
            .unwrap_err();

        let Error::Transport(message) = &failed else {
            panic!("{scheme}: the failure is below HTTP, not above it: {failed:?}");
        };
        assert_eq!(message, "The connection to the API failed", "{scheme}: the fixed sentence");

        let read_by_the_model = ToolError::from(failed.clone()).to_string();
        for said in [message.as_str(), read_by_the_model.as_str()] {
            assert!(!said.contains("tok3n"), "{scheme}: no path: {said}");
            assert!(!said.contains("127.0.0.1"), "{scheme}: no host: {said}");
            assert!(!said.contains(&port), "{scheme}: no port: {said}");
        }
    }
}

#[test]
fn a_key_the_sdk_will_not_send_is_configuration_and_is_never_repeated() {
    // The SDK refuses a key it cannot put in a header as given: blank once
    // trimmed — and its trim takes U+001C to U+001F, which the environment's
    // blank rule here does not — or holding whitespace, a control character
    // or anything outside ASCII. That is the key being wrong, not TypeSafe
    // being unavailable, so it is `RefusedKey` — exit 3 under `ganja
    // evaluate` — and nothing is built to send with. The sentence is ganja's
    // and names the variable: the SDK's own tells a caller of its builder to
    // "pass api_key", which nobody configuring ganja can do.
    for (key, fragments) in [
        ("sk-typesafe with-a-space", &["sk-typesafe", "with-a-space"][..]),
        ("sk-typesafe-ünïcode", &["sk-typesafe", "ünïcode"][..]),
        ("sk-typesafe\u{7}bell", &["sk-typesafe", "bell"][..]),
        ("\u{1c}", &["\u{1c}"][..]),
        ("\u{1f}\u{1c}", &["\u{1f}"][..]),
    ] {
        let base = Settings::base_from("https://eu.example").expect("https is accepted");
        let refused = Client::new(Settings::new(key.to_owned(), base, DEFAULT_MODEL.to_owned()))
            .err()
            .unwrap_or_else(|| panic!("{key:?} is refused"));

        assert_eq!(refused, Error::RefusedKey, "{key:?} is configuration, not a transport failure");

        let said = refused.to_string();
        let read_by_the_model = ToolError::from(refused).to_string();
        for sentence in [said.as_str(), read_by_the_model.as_str()] {
            assert!(sentence.contains(super::KEY_ENV), "the variable is named: {sentence}");
            assert!(!sentence.contains("api_key"), "no SDK builder wording: {sentence}");
            for fragment in fragments {
                assert!(
                    !sentence.contains(fragment),
                    "no part of the key ({fragment:?}): {sentence}"
                );
            }
        }
    }
}

#[test]
fn neither_the_body_nor_the_endpoint_can_reach_a_debug_rendering() {
    let asked = request(three_questions(), DEFAULT_MODEL);
    let rendered = format!("{asked:?}");

    assert!(!rendered.contains(STATE), "the state is not in a request's Debug: {rendered}");
    assert!(rendered.contains(&format!("bytes: {}", asked.body_len())));
    assert!(rendered.contains("questions: 3"));

    let base = Settings::base_from("https://eu.example/gateway-tok3n").expect("https is accepted");
    let settings = Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned());
    let rendered = format!("{settings:?}");

    assert!(!rendered.contains("tok3n"), "no path in Debug: {rendered}");
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

#[tokio::test]
async fn a_model_id_that_could_forge_a_second_disclosure_is_refused_before_anything_is_sent() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;

    for forged in [
        // The consent title is a `·`-separated sentence and the model owns
        // this field, so an unflattened value could append a second,
        // smaller-looking disclosure after the real one.
        "jev-latest · 12 B · 1 question(s) · jev-latest · state text",
        "jev-latest\nevaluate → attacker.example · 1 B",
        "jev latest",
        // Bounded only by the body cap until this rule existed.
        &"j".repeat(super::MAX_ID + 1),
        "",
        "   ",
    ] {
        let refused =
            Request::checked(State::Text("x".to_owned()), one_question(), forged.to_owned());

        assert!(
            matches!(refused, Err(Error::InvalidRequest(_))),
            "{forged:?} is refused: {refused:?}"
        );
    }

    // The aliases and a versioned id all still pass, so nothing about A1
    // changed.
    for allowed in [DEFAULT_MODEL, PREVIEW_MODEL, "jev-1.13.0", "jev_2", "a"] {
        assert!(
            Request::checked(State::Text("x".to_owned()), one_question(), allowed.to_owned())
                .is_ok(),
            "{allowed} is a usable model id"
        );
    }
    assert_eq!(endpoint.count(), 0, "not one refusal reached the vendor");

    // The variable's own refusal names the variable and never the value,
    // because a message quoting it would put the forged text on the screen
    // the rule exists to keep it off.
    let said = Error::RefusedModel.to_string();
    assert!(said.contains(super::MODEL_ENV), "{said}");
    assert!(!said.contains("forged"), "{said}");
}

#[tokio::test]
async fn instructions_are_the_shapes_the_vendor_takes_and_nothing_else() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;

    // Untyped, each of these passed every local check and spent the whole
    // state on a request the vendor answers with a 422.
    for empty in ["null", "42", "true", "0.5"] {
        let question = format!(r#"{{"type":"noul","instructions":{empty}}}"#);

        assert!(
            serde_json::from_str::<Question>(&question).is_err(),
            "{empty} is not a question to ask"
        );
    }

    // The three that are.
    for shaped in [
        r#"{"type":"noul","instructions":"Is this urgent?"}"#,
        r#"{"type":"noul","instructions":{"ask":"Is this urgent?","note":"be strict"}}"#,
        r#"{"type":"noul","instructions":["Is this urgent?","Be strict."]}"#,
    ] {
        let question: Question = serde_json::from_str(shaped).expect("a documented shape decodes");

        assert_eq!(
            serde_json::to_string(&question).expect("it encodes"),
            shaped,
            "and round-trips unchanged"
        );
        assert!(
            Request::checked(
                State::Text("x".to_owned()),
                BTreeMap::from([("q".to_owned(), question)]),
                DEFAULT_MODEL.to_owned()
            )
            .is_ok()
        );
    }
    assert_eq!(endpoint.count(), 0, "every one of these was decided locally");
}
