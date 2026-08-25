//! What the ChatGPT codex backend is told ganja is, and what it serves back.
//!
//! This is the instrument W2 of `.omc/plans/2026-08-25-ganja-code-identity-headers.md`
//! calls for: the baseline recording that gates the rename in W3. The point of
//! taking it *before* anything changes is that a roster measured after a change
//! proves nothing without the roster measured before it — a cohort demotion
//! looks exactly like a vendor-side rollout unless there is a recording to diff
//! against.
//!
//! # Running it
//!
//! Inert unless **both** are true: `GANJA_LIVE_TEST=1`, and this machine holds
//! a stored ChatGPT login that is what a session would actually authenticate
//! with. The second half is [`provider::wire_lists_models`]'s question, not a
//! second copy of it — which means an exported `OPENAI_API_KEY` makes the probe
//! skip, because a key outranks a login and such a session is not a seat at all.
//!
//! ```sh
//! # nextest, which needs to be told to reach an ignored test:
//! GANJA_LIVE_TEST=1 cargo nextest run -p ganja-core \
//!     -E 'binary(codex_identity_probe)' --run-ignored all --no-capture
//!
//! # or plain cargo:
//! GANJA_LIVE_TEST=1 cargo test -p ganja-core --test codex_identity_probe \
//!     -- --ignored --nocapture
//! ```
//!
//! `GANJA_MODEL` names the model the full turn is taken on; unset, it is
//! [`responses::SUBSCRIPTION_DEFAULT`]. `GANJA_PROBE_NOTE` stamps one operator
//! line into the recording's header — the binary and machine the P27/P28
//! `*-probe.txt` fixtures record by hand.
//!
//! The recording lands at `tests/fixtures/codex-identity-probe.txt`, overwriting
//! whatever was there, and the path is printed when it does. **The file is born
//! from a live run**: nothing in the tree creates it, so its absence means the
//! probe has not been taken rather than that it failed.
//!
//! # What it measures, and in what sense
//!
//! Three legs, because the three facts are true in different ways and one
//! instrument would be lying about at least one of them:
//!
//! 1. **The identity headers**, captured off a loopback socket this process
//!    owns. That this records the live turn's own bytes is not an argument from
//!    similarity: [`ResponsesProvider::from_stored`] *is*
//!    `at(configured(Backend::Codex), Login::new())`, so the live leg and this
//!    one are the same constructor, the same `Backend::Codex`, and the same
//!    credential source, differing in the base URL alone — and the header
//!    quartet is the backend's, which the base URL does not reach.
//!    A live request cannot be asked what it carried: the header set is built
//!    inside the provider and no accessor hands it back, which is the right
//!    shape for a credential-bearing request and the reason this leg exists.
//! 2. **One live turn** on the model under test, against the real backend.
//! 3. **A reachability ladder** over the roster, one minimal ask per model.
//!
//! ## Three limitations, stated rather than papered over
//!
//! - **There is no roster endpoint on this backend, so none is invented.** The
//!   listing [`provider::wire_model_listing`] answers for a seat is
//!   [`responses::SEAT_ROSTER`] — compile-time, no network, no catalog read
//!   (**D476**) — so recording it alone would record this build's own constant
//!   and call it a measurement. It *is* recorded, as what this build volunteers;
//!   what makes the recording diffable is leg 3 beside it, which asks the
//!   backend for each of those models in turn and writes down what came back.
//!   That is the only sense in which "the roster this seat is served" is
//!   measurable here, and a cohort demotion is precisely a row that moves from
//!   served to refused.
//! - **The wire reports no served-model field.** [`ProviderEvent`] carries no
//!   such variant, so what is recorded is the model *asked for* and whether the
//!   backend accepted it. A vendor silently substituting one model for another
//!   would not show up here, and is not claimed to.
//! - **A refusal body is verbatim to 400 characters and already masked.**
//!   `retry::refusal` is the one seam that turns an HTTP refusal into a message,
//!   and it redacts the body against the presented credential and trims it on a
//!   char boundary, appending `…`. Every refusal this repository has recorded
//!   from this backend is far shorter than that, so in practice the body is
//!   whole — but a longer one arrives cut, and the fixture says so.
//!
//!   **The order of those two is worth knowing**, because it decides what the
//!   leak check below has to look for: `retry::refusal` trims *first* and
//!   redacts the trimmed copy, and its redaction is an exact substring match.
//!   A body that quoted the credential across the 400-character cut therefore
//!   leaves a *prefix* of it behind — too short for the redaction to match, and
//!   too short for a search on the whole token to match either. That is why the
//!   check searches for a leading prefix as well as for the whole thing.
//!
//! # Why nothing here can write a credential down
//!
//! Both credential-bearing headers reach this process on leg 1 — that is what
//! makes the leg possible — and neither may reach the recording. Two mechanisms,
//! deliberately independent:
//!
//! - **Structural.** [`Redacted`] holds such a value with no `Display` that
//!   renders it and a `Debug` that cannot either, so the rendering path has
//!   nothing to print but a shape. Only an allowlist of identity header values
//!   is ever pushed into the text; every other header contributes its *name*,
//!   because a header set that silently dropped what it did not recognise would
//!   hide the next one somebody adds.
//! - **Asserted.** The finished text is searched, before it is written, for the
//!   bearer that was actually presented — whole, without its scheme, and as a
//!   leading prefix, for the straddle described above. This is
//!   `tests/secrets_env.rs`'s canary drill with the canary being the real
//!   credential: the search terms are never rendered, never logged, and never
//!   leave this process.
//!
//!   **What that assertion covers, exactly.** An OAuth access token is resolved
//!   afresh immediately before each request, so a renewal part-way through this
//!   probe would mean the ladder presented a token leg 1 never saw. The header
//!   leg therefore runs *twice* — once before the live legs and once after them,
//!   both against loopback — and every bearer either capture observed is
//!   searched for. A second renewal inside one run would still leave one
//!   unsearched; that residual is covered by the structural half rather than
//!   this one, since `retry::refusal` redacts each refusal against the
//!   credential that request actually presented, whichever it was.
//!
//! One test, one binary. It is the only test in the tree that writes a
//! tree-tracked fixture, its per-model ladder is a sequence rather than a set,
//! and its gate is the inverse of `tests/live.rs`'s openai test — that one needs
//! `OPENAI_API_KEY` exported and this one needs it absent, so the two can never
//! be satisfied by one run and housing them together would leave one silently
//! inert with no visible reason.

use std::{
    collections::BTreeSet,
    env, fmt, fs,
    sync::{Arc, Mutex},
};

use futures::StreamExt as _;
use ganja_core::{
    auth::openai::Login,
    protocol::{Message, Usage},
    provider::{
        self, ChatRequest, Provider, ProviderError, ProviderEvent, ResponsesProvider, openai,
        responses,
    },
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

/// Variable that has to be `1` before any of this talks to a vendor.
const LIVE_ENV: &str = "GANJA_LIVE_TEST";

/// One operator line stamped into the recording's header — the binary and the
/// machine, which no process can ask itself about honestly.
const NOTE_ENV: &str = "GANJA_PROBE_NOTE";

/// Where the recording lands, beside the P27/P28 `*-probe.txt` fixtures whose
/// convention it follows.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/codex-identity-probe.txt"
);

/// The prompt, chosen so the reply is one cheap token and the recording is not
/// a judgement about what a model felt like saying.
const PROMPT: &str = "Reply with exactly: pong";

/// The headers this plan is about, in the order the recording lists them.
///
/// Values are written down verbatim because each is a constant this build
/// chose: they are the measurement, not something measured about a person.
const IDENTITY: [&str; 3] = ["originator", "openai-beta", "user-agent"];

/// The headers that authenticate as somebody or name them.
///
/// Values are written down as a shape and never as bytes.
const CREDENTIAL: [&str; 2] = ["authorization", "chatgpt-account-id"];

/// How much of a token's head the leak check searches for on its own.
///
/// Long enough that no ordinary English or JSON in a refusal body could collide
/// with it, short enough to still be inside a body that quoted the credential
/// right at `retry::refusal`'s 400-character cut.
const PREFIX_SEARCH: usize = 24;

/// A value that reached this process and may not leave it.
///
/// The type is the redaction: there is no accessor that renders the bytes and
/// no `Display` at all, so a rendering path holds something it cannot print.
/// [`Redacted::expose`] exists for exactly one caller — the leak check, which
/// searches the finished text for what it must not contain.
struct Redacted(String);

impl Redacted {
    /// The bytes, for the one assertion that has to look for them.
    fn expose(&self) -> &str {
        &self.0
    }

    /// What a bearer header carries after the scheme, or the whole value where
    /// it carries no scheme — so the token is searched for on its own as well
    /// as inside the header it travelled in.
    fn without_scheme(&self) -> &str {
        self.0.strip_prefix("Bearer ").unwrap_or(&self.0)
    }

    /// The token's first [`PREFIX_SEARCH`] characters, for the one leak the two
    /// whole-value searches structurally cannot catch.
    ///
    /// `retry::refusal` trims a refusal body to 400 characters and only then
    /// redacts it by exact substring, so a credential quoted across that cut
    /// survives as a prefix that neither its own redaction nor a search for the
    /// whole token matches. [`None`] where the token is shorter than the prefix,
    /// in which case the whole-value searches already cover every substring of
    /// it worth finding.
    fn prefix(&self) -> Option<&str> {
        let token = self.without_scheme();

        token
            .char_indices()
            .nth(PREFIX_SEARCH)
            .map(|(end, _)| &token[..end])
    }
}

impl fmt::Debug for Redacted {
    /// The shape, so that a `{:?}` somebody adds later still cannot leak.
    ///
    /// The length is deliberate and is not a leak: it distinguishes a header
    /// that arrived empty from one that did not arrive, which is a difference
    /// the recording has to be able to state.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<redacted: {} bytes>", self.0.len())
    }
}

/// One request the loopback endpoint was asked to serve.
struct Recorded {
    /// Request line and headers, verbatim. Never rendered: it carries the
    /// bearer.
    head: String,
}

impl Recorded {
    /// The value of `name`, compared case-insensitively the way a header name
    /// is. [`None`] where the request did not carry it at all, which is a
    /// different answer from carrying it empty.
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|line| {
            let (found, value) = line.split_once(':')?;
            found
                .trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    /// Every header name the request carried, sorted, so the recording is the
    /// same file twice for the same request.
    fn header_names(&self) -> BTreeSet<String> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| Some(line.split_once(':')?.0.trim().to_ascii_lowercase()))
            .filter(|name| !name.is_empty())
            .collect()
    }
}

/// A loopback endpoint that records what it was asked and answers plausibly.
struct Endpoint {
    /// What the provider is pointed at.
    base_url: String,
    seen: Arc<Mutex<Vec<Recorded>>>,
    /// Kept so the server outlives the test talking to it.
    _server: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    /// The newest request served, with the recorder emptied behind it.
    ///
    /// Newest rather than only: a failure before the first byte is retried
    /// (**D475**), and a retry is the same headers again. Emptied because this
    /// is called twice — the second header capture must read its own request
    /// rather than the first capture's. Panics rather than skips when nothing
    /// arrived, because a probe that measured nothing must say so — the golden
    /// suite's hard-fail posture, for its reason.
    fn take_latest(&self) -> Recorded {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let latest = seen.pop();
        seen.clear();

        latest.unwrap_or_else(|| {
            panic!(
                "a header leg sent nothing at all, so there are no headers to \
                 record; the usual cause is a stored credential this build \
                 could not renew"
            )
        })
    }
}

/// Starts an endpoint that answers every connection for as long as the test
/// holds it.
///
/// The body is a complete two-frame Responses stream rather than an empty one:
/// a turn that fails before its first byte is retried, and three identical
/// recordings of one request would be noise in the count.
async fn serve() -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let address = listener
        .local_addr()
        .expect("a bound socket has an address");
    let seen: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));

    let recorded = Arc::clone(&seen);
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);

            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                recorded
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request);

                let body = [
                    r#"data: {"type":"response.created","response":{"id":"resp_probe"}}"#,
                    r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ]
                .join("\n\n")
                    + "\n\n";
                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nconnection: close\r\n\
                             content-type: text/event-stream\r\n\r\n{body}"
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = socket.flush().await;
                // Dropping the socket ends a close-delimited body.
            });
        }
    });

    Endpoint {
        base_url: format!("http://{address}/backend-api/codex"),
        seen,
        _server: server,
    }
}

/// Reads one whole request: head to the blank line, then whatever
/// `content-length` promised.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];

    while !buffer.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => buffer.push(byte[0]),
        }
    }
    let head = String::from_utf8_lossy(&buffer).into_owned();

    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or_default();
    // Drained rather than kept: the body is this build's own request shape,
    // which `tests/responses_wire.rs` already pins, and reading it to the end
    // is what lets the response be written without the peer seeing a reset.
    let mut body = vec![0_u8; length];
    if length > 0 && socket.read_exact(&mut body).await.is_err() {
        return None;
    }

    Some(Recorded { head })
}

/// What one asked-for model did.
enum Outcome {
    /// The backend served the turn.
    Served {
        /// Why the model stopped.
        finish: String,
        /// What it cost, where the turn was billed.
        usage: Option<Usage>,
        /// What it said, trimmed — one cheap token by construction.
        reply: String,
    },
    /// The backend refused, with its own words.
    Refused(String),
}

impl Outcome {
    /// The one-word verdict a ladder row leads with.
    fn verdict(&self) -> &'static str {
        match self {
            Self::Served { .. } => "served",
            Self::Refused(_) => "refused",
        }
    }
}

/// Takes one turn and reports what happened, never what it authenticated with.
async fn turn(provider: &dyn Provider, model: &str) -> Outcome {
    let streamed = match provider
        .stream(
            ChatRequest {
                effort_options: Default::default(),
                model: model.to_owned(),
                system: Some("Answer with a single word.".to_owned()),
                messages: vec![Message::user(PROMPT)],
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
    {
        Ok(stream) => stream.collect::<Vec<ProviderEvent>>().await,
        // A refusal that arrived before the stream did: the status and the
        // body the backend answered with, which is the negative case this
        // recording exists to hold.
        Err(error) => return Outcome::Refused(described(&error)),
    };

    if let Some(error) = streamed.iter().find_map(|event| match event {
        ProviderEvent::Failed(error) => Some(error),
        _ => None,
    }) {
        return Outcome::Refused(described(error));
    }

    Outcome::Served {
        finish: streamed
            .iter()
            .find_map(|event| match event {
                ProviderEvent::Finish(reason) => Some(format!("{reason:?}")),
                _ => None,
            })
            .unwrap_or_else(|| "no finish reason arrived".to_owned()),
        usage: streamed.iter().find_map(|event| match event {
            ProviderEvent::Usage(usage) => Some(*usage),
            _ => None,
        }),
        reply: streamed
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>()
            .trim()
            .to_owned(),
    }
}

/// A refusal in the recording's own words: the status first, because that is
/// what a cohort demotion is read off.
fn described(error: &ProviderError) -> String {
    match error {
        ProviderError::Status { status, message } => format!("HTTP {status}: {message}"),
        other => format!("{other}"),
    }
}

/// The credential situation this probe needs, or [`false`] with the reason.
///
/// The seat half is [`provider::wire_lists_models`]'s own question rather than
/// a second reading of the store: it answers `true` for exactly the session
/// this probe is about — a stored ChatGPT login with no exported key outranking
/// it — and it answers it without handing anything back that could be printed.
fn seated() -> bool {
    if env::var(LIVE_ENV).as_deref() != Ok("1") {
        eprintln!("skipping: {LIVE_ENV} is not 1");
        return false;
    }
    if !provider::wire_lists_models(openai::ID) {
        eprintln!(
            "skipping: this machine holds no ChatGPT login that a session would \
             use — run `ganja auth login`, and unset OPENAI_API_KEY if it is \
             exported, because a key outranks a login and such a session is not \
             a seat"
        );
        return false;
    }

    true
}

#[tokio::test]
#[ignore = "talks to the ChatGPT codex backend; needs GANJA_LIVE_TEST=1 and a stored ChatGPT login"]
async fn a_chatgpt_seat_records_the_identity_it_presents_and_the_models_it_is_served() {
    if !seated() {
        return;
    }

    let model = env::var("GANJA_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| responses::SUBSCRIPTION_DEFAULT.to_owned());

    // ---- 1. The headers, off a socket in this process. --------------------
    //
    // The real refresher, so this leg takes the same credential path a live
    // turn does — including a renewal, if the stored token needs one. Only the
    // endpoint differs, and the header quartet is the backend's rather than the
    // endpoint's.
    let endpoint = serve().await;
    let loopback = ResponsesProvider::at(
        &endpoint.base_url,
        Arc::new(Login::new().expect("a login builds wherever an HTTP client does")),
    )
    .expect("loopback may carry a token");
    let _ = turn(&loopback, &model).await;
    let sent = endpoint.take_latest();

    let bearer = Redacted(
        sent.header("authorization")
            .expect("a request to this backend authenticates"),
    );
    let account = sent.header("chatgpt-account-id").map(Redacted);

    // ---- 2. One live turn, against the vendor. ----------------------------
    let live = ResponsesProvider::from_stored().expect("a stored login builds the provider");
    let taken = turn(&live, &model).await;

    // ---- 3. The ladder, one ask per offered model. ------------------------
    let offered = provider::wire_model_listing(openai::ID)
        .await
        .expect("a seat is what the gate established")
        .expect("the seat arm reaches nothing that could fail");
    let mut ladder = Vec::new();
    for listed in &offered.models {
        // Sequential on purpose: a ladder is an ordered account of one seat,
        // and rows that raced would record the backend's concurrency rather
        // than its offering.
        let outcome = if listed.id == model {
            // Already asked, at leg 2. Asking twice would bill twice and could
            // disagree with itself.
            None
        } else {
            Some(turn(&live, &listed.id).await)
        };
        ladder.push((listed.id.clone(), listed.name.clone(), outcome));
    }

    // ---- 4. The headers again, so a renewal mid-run cannot go unsearched. -
    //
    // An access token is resolved afresh before each request, so the ladder may
    // have presented one this leg's first capture never saw. Aimed at loopback
    // rather than the vendor: this leg is about which credential is current,
    // and asking the backend again would bill a model ask to answer it.
    let _ = turn(&loopback, &model).await;
    let after = endpoint.take_latest();

    let recording = render(
        &bearer,
        account.as_ref(),
        &sent,
        &model,
        &taken,
        &offered,
        &ladder,
    );

    // ---- The leak check, before a byte of this reaches the disk. ----------
    //
    // The search terms are the credentials that were really presented, so this
    // is not a check that some placeholder is absent. They are never rendered:
    // the only thing that happens to them here is a substring search.
    //
    // Three shapes per value, because they fail to catch different things: the
    // header whole, the token without its scheme, and the token's head — that
    // last one for `retry::refusal`'s trim-then-redact order, which can leave a
    // prefix behind that no whole-value search would find.
    let mut searched: Vec<String> = Vec::new();
    for captured in [&sent, &after] {
        for name in CREDENTIAL {
            let Some(value) = captured.header(name).filter(|value| !value.is_empty()) else {
                continue;
            };
            let value = Redacted(value);

            searched.push(value.expose().to_owned());
            searched.push(value.without_scheme().to_owned());
            if let Some(prefix) = value.prefix() {
                searched.push(prefix.to_owned());
            }
        }
    }

    assert!(
        !bearer.expose().is_empty(),
        "the leak check needs a credential to look for, and this request \
         carried an empty one"
    );
    for secret in &searched {
        assert!(
            // The message names no secret, for the reason this assertion
            // exists: a panic is rendered.
            !recording.contains(secret.as_str()),
            "the recording renders a credential this probe presented"
        );
    }

    fs::write(FIXTURE, &recording).expect("the fixture directory is writable");
    eprintln!("recorded to {FIXTURE}");
}

/// The recording, in the order it is always in.
///
/// Blocks are delimited so that the acceptance artifact W3 diffs — the ladder —
/// can be compared on its own, without the provenance header's own volatility
/// counting as a difference.
fn render(
    bearer: &Redacted,
    account: Option<&Redacted>,
    sent: &Recorded,
    model: &str,
    taken: &Outcome,
    offered: &provider::WireModels,
    ladder: &[(String, String, Option<Outcome>)],
) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "\
# What the ChatGPT codex backend is told ganja is, and what it serves back.
#
# The baseline W3 of .omc/plans/2026-08-25-ganja-code-identity-headers.md diffs
# against: a roster measured after the rename proves nothing without the roster
# measured before it. Recorded by `tests/codex_identity_probe.rs`, whose module
# doc holds the command, the gate and the three things this cannot measure.
#
# Build: ganja-code {version}
# Note:  {note}

== identity headers ==
# Captured off a loopback socket this process owns. The quartet below is decided
# by Backend::Codex and not by the endpoint, so these are the bytes the live
# turn sent, read somewhere they can be read. The two credential-bearing values
# are written as presence alone: a type that cannot render them decides that,
# and a token's length changes on every renewal, which this block should not
# diff on.
",
        version = env!("CARGO_PKG_VERSION"),
        note = env::var(NOTE_ENV)
            .ok()
            .filter(|note| !note.trim().is_empty())
            .unwrap_or_else(|| format!("not recorded; set {NOTE_ENV} to stamp one")),
    ));

    for name in CREDENTIAL {
        let value = match name {
            "authorization" => Some(bearer),
            _ => account,
        };
        out.push_str(&format!("{name}: {}\n", shape(value)));
    }
    for name in IDENTITY {
        match sent.header(name) {
            Some(value) => out.push_str(&format!("{name}: {value}\n")),
            None => out.push_str(&format!("{name}: <not sent>\n")),
        }
    }

    let named: BTreeSet<String> = CREDENTIAL
        .iter()
        .chain(IDENTITY.iter())
        .map(|name| (*name).to_owned())
        .collect();
    let others: Vec<String> = sent
        .header_names()
        .difference(&named)
        .cloned()
        .collect::<Vec<_>>();
    out.push_str(&format!(
        "\n# Also sent, names only: a header set that dropped what it did not\n\
         # recognise would hide the next one somebody adds.\nalso sent: {}\n",
        if others.is_empty() {
            "<nothing else>".to_owned()
        } else {
            others.join(", ")
        }
    ));

    out.push_str(&format!(
        "\n== the live turn ==\n\
         # Against the real backend. The wire reports no served-model field, so\n\
         # what is recorded is the model asked for and whether it was accepted.\n\
         # Offered is not servable: SUBSCRIPTION_DEFAULT is deliberately outside\n\
         # the roster below, so unless GANJA_MODEL named a roster member this\n\
         # turn is a sixth ask rather than one of the five.\n\
         model asked: {model}\n\
         outcome: {}\n",
        taken.verdict()
    ));
    out.push_str(&detailed(taken));

    out.push_str(&format!(
        "\n== what this build offers a seat ==\n\
         # responses::SEAT_ROSTER, compile-time and reached by no network\n\
         # (D476). Recorded as what ganja volunteers, never as a measurement:\n\
         # the ladder below is the measured half.\n\
         notice: {}\n\
         default: {}\n",
        offered.notice,
        responses::SUBSCRIPTION_DEFAULT,
    ));
    for listed in &offered.models {
        out.push_str(&format!("offered: {} ({})\n", listed.id, listed.name));
    }

    out.push_str(
        "\n== the ladder ==\n\
         # One ask per offered model, in the roster's own order. This is the\n\
         # acceptance artifact: a cohort demotion is a row that moves from\n\
         # served to refused. A refusal body is verbatim to 400 characters and\n\
         # arrives already masked against the presented credential; a longer one\n\
         # is cut on a char boundary and ends in an ellipsis. Line breaks inside\n\
         # one are flattened to spaces, so a row stays a row.\n",
    );
    for (id, _, outcome) in ladder {
        match outcome {
            Some(outcome) => {
                out.push_str(&format!("{id}: {}\n", outcome.verdict()));
                if let Outcome::Refused(refusal) = outcome {
                    out.push_str(&format!("  {}\n", flattened(refusal)));
                }
            }
            None => out.push_str(&format!("{id}: see the live turn above\n")),
        }
    }

    out
}

/// How a credential-bearing header is written down.
///
/// Presence rather than length, and the difference belongs at this seam alone:
/// [`Redacted`]'s own `Debug` renders the length so that an empty value can be
/// told from an absent one wherever it is printed, but a token's length changes
/// every time it is renewed — and this block is one of the two W3 diffs, where a
/// number that moves on its own is noise standing between a reader and the line
/// that actually changed.
fn shape(value: Option<&Redacted>) -> &'static str {
    match value {
        None => "<not sent>",
        Some(value) if value.expose().is_empty() => "<redacted: empty>",
        Some(_) => "<redacted: present>",
    }
}

/// One row, whatever the vendor put line breaks through.
///
/// The ladder is one row per model and is the block W3 diffs; a refusal body
/// that arrived with newlines in it would split its own row and turn a one-line
/// difference into a shapeless one. Carriage returns go with them, so a body
/// written for a wire cannot leave a stray one mid-row.
fn flattened(body: &str) -> String {
    body.replace(['\n', '\r'], " ")
}

/// The live turn's own rows, which a ladder row does not carry.
fn detailed(taken: &Outcome) -> String {
    match taken {
        Outcome::Served {
            finish,
            usage,
            reply,
        } => {
            let billed = usage.map_or_else(
                || "not billed".to_owned(),
                |usage| {
                    format!(
                        "input={} output={} cache_read={} reasoning={}",
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens,
                        usage.reasoning_tokens,
                    )
                },
            );

            format!("finish: {finish}\nusage: {billed}\nreply: {reply:?}\n")
        }
        Outcome::Refused(refusal) => format!("refusal: {}\n", flattened(refusal)),
    }
}
