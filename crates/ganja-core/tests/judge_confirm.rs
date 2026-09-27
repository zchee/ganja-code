//! The judge's confirmation (**D570**): what a 403 naming the credential
//! does, end to end. Such a 403 sends one fixed confirmation that carries no
//! text of any result, and the judge turns off only when that confirmation
//! is refused as a credential failure too. An answered confirmation settles
//! the key for the process; anything else settles nothing, and a process
//! sends at most three. At most one is in flight at a time.
//!
//! The vendor is the loopback double in `judge_support`, so every count here
//! is read off a real socket. Every text a test screens carries a marker of
//! its own, and a confirmation is told apart as the request that carries
//! none of them: the tests never name what a confirmation says, only what it
//! must not say. Nothing here reads the environment or installs a global
//! subscriber.

mod judge_support;

use std::time::{Duration, Instant};

use ganja_core::judge::{MODEL, SENTENCE, SUFFIX};
use ganja_core::tool::ToolOutput;
use ganja_testkit::LogCapture;
use judge_support::{
    PATIENCE, Reply, Seen, Vendor, fetched, mcp_only, page, result, tuning, webfetch_only,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// What the vendor answers a request whose key it refuses: a 403 whose
/// `detail.error_type` names the credential.
const NAMES_THE_CREDENTIAL: &str =
    r#"{"detail":{"error_type":"authentication_error","message":"Not authenticated."}}"#;

/// A 403 for a key that lacks a permission: it names something other than
/// the credential.
const PERMISSION_DENIED: &str =
    r#"{"detail":{"error_type":"permission_denied","message":"Not allowed."}}"#;

/// One row of a table whose reply the double builds afresh for each
/// request: a name, and the reply.
type ReplyCase = (&'static str, fn() -> Reply);

/// One row of the confirmation-outcome table: a name, the reply the
/// confirmation gets, and whether the result's deadline is what ends it.
type ConfirmationCase = (&'static str, fn() -> Reply, bool);

/// The vendor's 403 naming the credential.
fn credential_403() -> Reply {
    Reply::Raw { status: 403, body: NAMES_THE_CREDENTIAL.to_owned() }
}

/// The requests `vendor` received whose segment carries none of `markers`:
/// the confirmations, since every text a test screens carries a marker.
fn confirmations(vendor: &Vendor, markers: &[&str]) -> Vec<Seen> {
    vendor
        .seen()
        .into_iter()
        .filter(|seen| !markers.iter().any(|marker| seen.content.contains(marker)))
        .collect()
}

/// `metadata.screen` of a result the judge recorded.
fn record(output: &ToolOutput) -> &Value {
    output.metadata.get("screen").expect("the result carries its screening record")
}

/// The segment indices one list of a record holds.
fn indices(record: &Value, list: &str) -> Vec<u64> {
    record["segments"][list]
        .as_array()
        .unwrap_or_else(|| panic!("segments.{list} is a list: {record}"))
        .iter()
        .filter_map(Value::as_u64)
        .collect()
}

/// Lines the judge logged, and nothing anyone else logged.
fn judge_lines(log: &LogCapture) -> Vec<String> {
    log.logged()
        .lines()
        .filter(|line| line.contains("ganja_core::judge"))
        .map(str::to_owned)
        .collect()
}

/// T1: a 403 naming the credential whose confirmation is refused as a
/// credential failure too — a 403 naming the credential, or a 401 — turns the
/// judge off for the process, with one warning saying the key was refused and
/// confirmed. The segments of a twelve-segment result not yet issued never
/// leave, the result is annotated nothing, and the next result sends nothing.
#[tokio::test]
async fn a_credential_403_whose_confirmation_is_refused_too_turns_the_judge_off() {
    let tests: [ReplyCase; 2] = [
        ("the confirmation gets a 403 naming the credential", credential_403),
        ("the confirmation gets a 401", || Reply::Status(401)),
    ];
    for (name, confirmation) in tests {
        let (log, _guard) = LogCapture::install(tracing::Level::WARN);
        let vendor = Vendor::start(move |seen| {
            if seen.content.contains("harbour") { credential_403() } else { confirmation() }
        })
        .await;
        let judge = vendor.judge(webfetch_only(), tuning(20_000, 3, 60_000));
        let cancel = CancellationToken::new();
        let text = page("harbour", 12);
        let mut output = fetched(&text);

        judge.annotate("webfetch", &mut output, &cancel).await;

        assert_eq!(output.output, text, "{name}: nothing is annotated");
        assert!(output.metadata.get("screen").is_none(), "{name}: an off result records nothing");
        assert!(output.title.ends_with(SUFFIX), "{name}: its title says text left");
        let screened = vendor.seen().iter().filter(|seen| seen.content.contains("harbour")).count();
        assert!(
            (1..=4).contains(&screened),
            "{name}: only the first four-at-a-time wave was issued; the rest never left: \
             {screened} of 12"
        );
        assert_eq!(confirmations(&vendor, &["harbour"]).len(), 1, "{name}: one confirmation");

        let requests = vendor.requests();
        let mut next = fetched(&page("harbour", 3));
        judge.annotate("webfetch", &mut next, &cancel).await;
        assert_eq!(vendor.requests(), requests, "{name}: the next result sends nothing");
        assert!(!next.title.ends_with(SUFFIX), "{name}: and says nothing left");

        let warnings = judge_lines(&log);
        assert_eq!(warnings.len(), 1, "{name}: one warning however many said so: {warnings:?}");
        assert!(
            warnings[0].contains("refused the credential")
                && warnings[0].contains("confirmation")
                && warnings[0].contains("screening is off"),
            "{name}: the line says the key was refused and confirmed: {warnings:?}"
        );
        assert!(!warnings[0].contains(judge_support::KEY), "{name}: and never carries the key");
    }
}

/// T9: a result in flight keeps the marker one of its segments earned when
/// another of its segments gets a 403 naming the credential from a judge
/// that is already off. That 403 is a plain refusal, with no confirmation,
/// and does not make the result an off result, which would drop the marker.
#[tokio::test]
async fn a_credential_403_meeting_a_judge_already_off_keeps_the_results_marker() {
    let vendor = Vendor::start(|seen| {
        if seen.content.contains("fire-text") {
            Reply::Answer { fire: true }
        } else if seen.content.contains("late-text") {
            // Sent while the judge is on; held until the test releases it,
            // once the judge is off.
            Reply::Held(Box::new(credential_403()))
        } else if seen.content.contains("off-text") {
            Reply::Status(401)
        } else {
            Reply::Answer { fire: false }
        }
    })
    .await;
    let judge = vendor.judge(webfetch_only(), tuning(20_000, 3, 60_000));
    let cancel = CancellationToken::new();
    let text = format!(
        "fire-text block 000 {}\n\nlate-text block 001 {}\n\n",
        "p".repeat(2100),
        "p".repeat(2100)
    );
    let mut marked = fetched(&text);
    let mut switching = fetched("off-text: the key is refused on this one.");

    // The order is driven, not timed: both segments of the marked result are
    // at the vendor, the 401 result runs to completion and so turns the
    // judge off, and only then is the held 403 answered.
    tokio::time::timeout(PATIENCE, async {
        tokio::join!(judge.annotate("webfetch", &mut marked, &cancel), async {
            vendor.wait_for_requests(2, PATIENCE).await;
            judge.annotate("webfetch", &mut switching, &cancel).await;
            vendor.release();
        })
    })
    .await
    .expect("both results finish once the held 403 is released");

    assert!(switching.metadata.get("screen").is_none(), "the 401 turned the judge off");
    assert_eq!(vendor.requests(), 3, "two segments and the 401; no confirmation");
    assert!(marked.output.ends_with(SENTENCE), "the marker stays");
    let screened = record(&marked);
    assert_eq!(indices(screened, "fired_indices"), [0], "the segment that fired");
    assert_eq!(indices(screened, "refused"), [1], "the late 403 is a plain refusal");
}

/// T2: a 403 naming the credential whose confirmation is answered is a
/// refusal of that segment, recorded and never counted against the vendor;
/// the judge stays on and screens the next result; and a later 403 naming the
/// credential sends no second confirmation, because the key is settled.
#[tokio::test]
async fn an_answered_confirmation_keeps_the_judge_on_and_is_never_sent_twice() {
    let vendor = Vendor::start(|seen| {
        if seen.content.contains("refused-text") {
            credential_403()
        } else {
            Reply::Answer { fire: false }
        }
    })
    .await;
    let judge = vendor.judge(webfetch_only(), tuning(20_000, 3, 60_000));
    let cancel = CancellationToken::new();
    let markers = ["refused-text", "ordinary-text"];

    let mut first = fetched("refused-text: the harbour timetable for the summer season.");
    judge.annotate("webfetch", &mut first, &cancel).await;
    assert_eq!(indices(record(&first), "refused"), [0], "that segment is refused");
    assert!(first.title.ends_with(SUFFIX));
    assert_eq!(vendor.requests(), 2, "its screening request and one confirmation");
    assert_eq!(confirmations(&vendor, &markers).len(), 1);

    let mut ordinary = fetched("ordinary-text: the ferry leaves at nine and returns at six.");
    judge.annotate("webfetch", &mut ordinary, &cancel).await;
    assert_eq!(record(&ordinary)["segments"]["answered"], 1, "the judge stays on");
    assert_eq!(vendor.requests(), 3);

    let mut again = fetched("refused-text: the winter timetable, with fewer crossings.");
    judge.annotate("webfetch", &mut again, &cancel).await;
    assert_eq!(indices(record(&again), "refused"), [0], "a plain refusal");
    assert_eq!(vendor.requests(), 4, "the second 403 sent its screening request alone");
    assert_eq!(confirmations(&vendor, &markers).len(), 1, "no second confirmation");
}

/// T3: a 403 that does not name the credential is refused with no
/// confirmation, and the judge stays on.
#[tokio::test]
async fn a_403_that_does_not_name_the_credential_is_refused_with_no_confirmation() {
    let tests: [(&str, &str); 3] = [
        ("a 403 naming a missing permission", PERMISSION_DENIED),
        ("a 403 with no body", ""),
        ("a 403 whose detail is a bare string", r#"{"detail":"Forbidden"}"#),
    ];
    let markers = ["plain-refusal", "ordinary-text"];

    for (name, body) in tests {
        let body = body.to_owned();
        let vendor = Vendor::start(move |seen| {
            if seen.content.contains("plain-refusal") {
                Reply::Raw { status: 403, body: body.clone() }
            } else {
                Reply::Answer { fire: false }
            }
        })
        .await;
        let judge = vendor.judge(webfetch_only(), tuning(20_000, 3, 60_000));
        let cancel = CancellationToken::new();

        let mut refused = fetched("plain-refusal: a list of the lighthouses on the coast.");
        judge.annotate("webfetch", &mut refused, &cancel).await;
        let mut next = fetched("ordinary-text: the museum opens at ten on weekdays.");
        judge.annotate("webfetch", &mut next, &cancel).await;

        assert_eq!(indices(record(&refused), "refused"), [0], "{name}");
        assert_eq!(record(&next)["segments"]["answered"], 1, "{name}: the judge stays on");
        assert_eq!(vendor.requests(), 2, "{name}");
        assert_eq!(confirmations(&vendor, &markers).len(), 0, "{name}: no confirmation");
    }
}

/// T4: a 401 or a 404 on a screening request turns the judge off at once,
/// with no confirmation: the one request the result sent is the only one
/// the vendor ever sees.
#[tokio::test]
async fn a_401_or_404_on_a_screening_request_turns_the_judge_off_with_no_confirmation() {
    let tests: [ReplyCase; 3] = [
        ("a 401 naming the credential", || Reply::Raw {
            status: 401,
            body: NAMES_THE_CREDENTIAL.to_owned(),
        }),
        ("a 401 whose body names nothing", || Reply::Status(401)),
        ("a 404", || Reply::Status(404)),
    ];

    for (name, reply) in tests {
        let vendor = Vendor::start(move |_| reply()).await;
        let judge = vendor.judge(webfetch_only(), tuning(20_000, 3, 60_000));
        let cancel = CancellationToken::new();

        let mut output = fetched("off-text: the bridge closes for repairs in March.");
        judge.annotate("webfetch", &mut output, &cancel).await;
        let mut next = fetched("off-text: the bridge reopens in May.");
        judge.annotate("webfetch", &mut next, &cancel).await;

        assert!(output.metadata.get("screen").is_none(), "{name}: the judge is off");
        assert_eq!(vendor.requests(), 1, "{name}: no confirmation, and nothing after");
        assert_eq!(confirmations(&vendor, &["off-text"]).len(), 0, "{name}");
    }
}

/// T5, per outcome: a confirmation that is neither answered nor refused as a
/// credential failure settles nothing. The segment that asked is refused —
/// also when the result's deadline catches the confirmation in flight — so
/// the breaker does not move: it opens after one failed result here, and the
/// next result is screened. A 404 on the confirmation does not turn the judge
/// off: only a screening request's 404 does. Every row but the deadline's
/// ends well before the deadline, so its refusal is the confirmation's
/// answer and not the deadline's.
#[tokio::test]
async fn a_confirmation_that_settles_nothing_leaves_the_judge_on() {
    let deadline = Duration::from_millis(5_000);
    let tests: [ConfirmationCase; 10] = [
        ("the deadline while it is in flight", || Reply::Never, true),
        ("a transport failure", || Reply::Hangup, false),
        (
            "a reply over the size cap",
            || Reply::Raw { status: 200, body: format!("\"{}\"", "x".repeat(1_100_000)) },
            false,
        ),
        ("an unreadable 2xx", || Reply::Raw { status: 200, body: "not json".to_owned() }, false),
        ("a 429", || Reply::Status(429), false),
        ("a 5xx", || Reply::Status(503), false),
        ("a 404", || Reply::Status(404), false),
        (
            "a 422",
            || Reply::Raw {
                status: 422,
                body: r#"{"detail":[{"loc":["body","state"],"msg":"invalid"}]}"#.to_owned(),
            },
            false,
        ),
        ("another 4xx", || Reply::Status(400), false),
        (
            "a 403 that does not name the credential",
            || Reply::Raw { status: 403, body: PERMISSION_DENIED.to_owned() },
            false,
        ),
    ];
    let markers = ["asking-text", "ordinary-text"];

    for (name, confirmation, waits_out_the_deadline) in tests {
        let vendor = Vendor::start(move |seen| {
            if seen.content.contains("asking-text") {
                credential_403()
            } else if seen.content.contains("ordinary-text") {
                Reply::Answer { fire: false }
            } else {
                confirmation()
            }
        })
        .await;
        let judge = vendor.judge(webfetch_only(), tuning(5_000, 1, 60_000));
        let cancel = CancellationToken::new();

        let started = Instant::now();
        let mut asking = fetched("asking-text: the ferry timetable for the spring.");
        judge.annotate("webfetch", &mut asking, &cancel).await;
        let took = started.elapsed();
        let mut next = fetched("ordinary-text: the ferry timetable for the autumn.");
        judge.annotate("webfetch", &mut next, &cancel).await;

        assert_eq!(indices(record(&asking), "refused"), [0], "{name}");
        assert_eq!(indices(record(&asking), "unanswered"), Vec::<u64>::new(), "{name}");
        assert_eq!(confirmations(&vendor, &markers).len(), 1, "{name}: one confirmation");
        assert_eq!(
            record(&next)["segments"]["answered"],
            1,
            "{name}: the breaker did not move, and the judge stays on"
        );
        assert_eq!(took >= deadline, waits_out_the_deadline, "{name}: took {took:?}");
    }
}

/// T5, the waiter: two segments of one result meet a 403 naming the
/// credential together; one sends the confirmation, which never comes back,
/// and the other waits at the door for it. The deadline catches both, and
/// both are refused: the breaker, which opens after one failed result here,
/// does not move, and the next result is screened.
#[tokio::test]
async fn the_deadline_catching_a_confirmation_and_its_waiter_refuses_both() {
    let vendor = Vendor::start(|seen| {
        if seen.content.contains("waiting-text") {
            credential_403()
        } else if seen.content.contains("ordinary-text") {
            Reply::Answer { fire: false }
        } else {
            Reply::Never
        }
    })
    .await;
    let judge = vendor.judge(webfetch_only(), tuning(1_500, 1, 60_000));
    let cancel = CancellationToken::new();

    let mut waiting = fetched(&page("waiting-text", 2));
    judge.annotate("webfetch", &mut waiting, &cancel).await;
    let mut next = fetched("ordinary-text: the lighthouse is open to visitors on Sundays.");
    judge.annotate("webfetch", &mut next, &cancel).await;

    assert_eq!(indices(record(&waiting), "refused"), [0, 1], "both are refused");
    assert_eq!(indices(record(&waiting), "unanswered"), Vec::<u64>::new());
    assert_eq!(
        confirmations(&vendor, &["waiting-text", "ordinary-text"]).len(),
        1,
        "one confirmation: the other segment waited for it"
    );
    assert_eq!(record(&next)["segments"]["answered"], 1, "the breaker did not move");
}

/// T5, the cap: confirmations that settle nothing leave later 403s naming
/// the credential free to ask again, three times in a process and never a
/// fourth. Every such segment is refused, and the judge stays on.
#[tokio::test]
async fn after_three_unsettled_confirmations_no_more_are_sent() {
    let vendor = Vendor::start(|seen| {
        if seen.content.contains("capped-text") { credential_403() } else { Reply::Status(500) }
    })
    .await;
    let judge = vendor.judge(webfetch_only(), tuning(20_000, 3, 60_000));
    let cancel = CancellationToken::new();

    for round in 1..=5_usize {
        let mut output = fetched(&format!("capped-text: round {round} of the tide tables."));
        judge.annotate("webfetch", &mut output, &cancel).await;

        assert_eq!(indices(record(&output), "refused"), [0], "round {round}");
        let sent = confirmations(&vendor, &["capped-text"]).len();
        assert_eq!(sent, round.min(3), "round {round}: confirmations so far");
    }
    assert_eq!(vendor.requests(), 5 + 3, "five screening requests and three confirmations");
}

/// T6: the confirmation is the same bytes whichever result asked. Two
/// triggers that differ in their tool — a search and an MCP server's tool,
/// whose name the server chose — and in their text send byte-identical
/// confirmations that carry neither, with the questions and the model a
/// screening request carries.
#[tokio::test]
async fn the_confirmation_is_the_same_bytes_whichever_result_asked_and_carries_none_of_it() {
    let vendor = Vendor::start(|seen| {
        if seen.content.contains("alpha-text") || seen.content.contains("beta-text") {
            credential_403()
        } else {
            Reply::Status(500)
        }
    })
    .await;
    let mut screen = mcp_only(&["hub"]);
    screen.websearch = true;
    let judge = vendor.judge(screen, tuning(20_000, 3, 60_000));
    let cancel = CancellationToken::new();

    let alpha_text = "alpha-text: the tide tables for the northern pier";
    let mut alpha = result("search", alpha_text, json!({ "truncated": false }));
    judge.annotate("websearch", &mut alpha, &cancel).await;
    let beta_text = "beta-text: the keepers of the lighthouse since 1850";
    let mut beta = result("lookup", beta_text, json!({ "server": "hub", "truncated": false }));
    judge.annotate("mcp__hub__lookup", &mut beta, &cancel).await;

    let sent = confirmations(&vendor, &["alpha-text", "beta-text"]);
    assert_eq!(sent.len(), 2, "one confirmation per trigger, since neither settled anything");
    assert_eq!(sent[0].raw, sent[1].raw, "byte-identical");
    for carried in ["websearch", "mcp__hub__lookup", alpha_text, beta_text, "tide", "lighthouse"] {
        assert!(!sent[0].raw.contains(carried), "the confirmation carries {carried:?}");
    }
    let screening = vendor
        .seen()
        .into_iter()
        .find(|seen| seen.content.contains("alpha-text"))
        .expect("the first trigger's screening request arrived");
    assert_eq!(sent[0].body["questions"], screening.body["questions"], "the same questions");
    assert_eq!(sent[0].model, MODEL, "the same model");
}

/// T7: eight segments of two results meet a 403 naming the credential
/// together, and exactly one confirmation reaches the vendor. The others
/// wait for it and take its outcome — answered, so every one of them is a
/// refusal — rather than sending their own.
#[tokio::test]
async fn segments_that_meet_the_403_together_send_one_confirmation() {
    let vendor = Vendor::start(|seen| {
        if seen.content.contains("together") {
            // Late enough that all eight are sent before any comes back.
            Reply::After(Duration::from_millis(300), Box::new(credential_403()))
        } else {
            Reply::Held(Box::new(Reply::Answer { fire: false }))
        }
    })
    .await;
    let judge = vendor.judge(webfetch_only(), tuning(20_000, 3, 60_000));
    let cancel = CancellationToken::new();
    let (mut a, mut b) = (fetched(&page("together-a", 4)), fetched(&page("together-b", 4)));

    tokio::time::timeout(PATIENCE, async {
        tokio::join!(
            judge.annotate("webfetch", &mut a, &cancel),
            judge.annotate("webfetch", &mut b, &cancel),
            async {
                vendor.wait_for_requests(9, PATIENCE).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert_eq!(
                    confirmations(&vendor, &["together"]).len(),
                    1,
                    "one confirmation in flight, however many segments met the 403"
                );
                vendor.release();
            },
        )
    })
    .await
    .expect("both results finish once the confirmation is answered");

    assert_eq!(confirmations(&vendor, &["together"]).len(), 1, "and one in all");
    assert_eq!(vendor.requests(), 9, "eight screening requests and one confirmation");
    for (name, output) in [("a", &a), ("b", &b)] {
        assert_eq!(indices(record(output), "refused"), [0, 1, 2, 3], "result {name}");
    }
}
