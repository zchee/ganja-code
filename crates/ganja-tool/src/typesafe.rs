//! The TypeSafe System One client: one POST, one set of typed judgements.
//!
//! **D564.** No upstream counterpart — opencode has nothing that speaks to
//! this vendor — so every sentence here is ganja's own. What is taken from
//! TypeSafe is interop fact: the endpoint path, the field names, the status
//! semantics and the three environment variables its own SDK reads
//! (`https://docs.typesafe.ai/api.md` and `/sdk/python/api/constants.md`,
//! read 2026-09-17/18 against `jev-1.13.0`).
//!
//! Jev is not a chat model and this is not a provider: one request carries a
//! [`State`] and a map of typed [`Question`]s, and the answer is a map of
//! calibrated probabilities. There is no streaming, no conversation and no
//! `Provider::stream` to implement.
//!
//! Two surfaces sit over this one client — the `evaluate` tool the model
//! calls and the `ganja evaluate` subcommand a hook calls — and both validate
//! through [`Request::checked`], so neither can send something the other
//! would have refused.
//!
//! # What this module refuses, and why
//!
//! - **A base URL that would put the key on the wire in the clear.**
//!   [`Settings::base_from`] accepts `https`, or `http` to loopback, deciding
//!   on a *parsed* host — the same predicate `ganja_provider::provider::
//!   reachable_in_the_clear` applies to a provider base URL, mirrored rather
//!   than imported because this crate's internal dependency set is exactly
//!   `ganja-permission`. `crates/ganja-core/tests/typesafe_base_url.rs`
//!   holds the two equal, from the one crate that can see both. The refused
//!   URL is never echoed: configuration is allowed to carry credentials in
//!   its userinfo.
//! - **Redirects.** The client is built with [`reqwest::redirect::Policy::
//!   none`], so a 3xx is a failure naming its status rather than a body to
//!   parse. A request here carries an API key in a header; `webfetch`, which
//!   carries none, follows redirects on purpose.
//! - **A second attempt.** One consent is one transmission. The tree's retry
//!   budget lives in the provider; a 429 costs the judgement and the model is
//!   told to continue without it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use schemars::JsonSchema;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::ToolError;

/// Where the API key is read from, spelled as the vendor's own SDK spells it
/// so a machine already set up for TypeSafe needs no second variable.
pub const KEY_ENV: &str = "TYPESAFE_API_KEY";

/// The optional base-URL override, the vendor's spelling. The cookbooks'
/// `TYPESAFE_ENDPOINT` is deliberately not read: one name, and it is the one
/// the SDK documents.
pub const BASE_ENV: &str = "TYPESAFE_BASE_URL";

/// The optional default-model override, the vendor's spelling.
pub const MODEL_ENV: &str = "TYPESAFE_DEFAULT_MODEL";

/// The vendor's `DEFAULT_BASE_URL`.
const DEFAULT_BASE: &str = "https://api.typesafe.ai";

/// The flagship alias, and what a session sends when nothing names a model.
pub const DEFAULT_MODEL: &str = "jev-latest";

/// The other selectable alias. Named here because both surfaces advertise it
/// by name and a model that is never told an alias exists cannot ask for it.
pub const PREVIEW_MODEL: &str = "jev-preview";

/// The endpoint path, joined onto the base.
const PATH: &str = "v1/systemone";

/// How long one exchange may take, the vendor SDK's `DEFAULT_TIMEOUT`.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Most questions one request may carry.
///
/// Ganja's own limit; the vendor documents none. One question is one output
/// line, and the vendor's own largest cookbook fan-out is thirteen.
pub const MAX_QUESTIONS: usize = 50;

/// Fewest questions one request may carry: a request asking nothing is a
/// transmission bought for no judgement.
pub const MIN_QUESTIONS: usize = 1;

/// Longest a question id may be.
const MAX_ID: usize = 64;

/// Largest serialized request body, state and questions together.
///
/// Ganja's own limit, about 65k tokens. The cap is over the **whole** body
/// rather than over `state` alone because `instructions` is
/// `string | object | array` and `criteria` are free maps and arrays: a model
/// can put project content in either, and a cap on `state` alone would leave
/// that content outside both this limit and the consent disclosure that
/// quotes the same number.
pub const MAX_BODY: usize = 256 * 1024;

/// Largest response body that will be held. An answer is a small JSON object;
/// `webfetch`, which reads pages, allows twenty times this.
const MAX_RESPONSE: usize = 1024 * 1024;

/// Most of a vendor error body that reaches a message.
const MAX_DETAIL: usize = 2 * 1024;

/// Most of an error body that is read at all, before it is clamped.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// What a refusal says when the vendor's body could not be read at all.
const UNREADABLE: &str = "(the response body could not be read)";

/// What a refusal says when the vendor's body held nothing a reader could
/// turn into a sentence. A fixed sentence rather than the body itself: every
/// shape that reaches here has already declined to say which field is wrong,
/// and passing the raw JSON on would re-admit the request echo that
/// [`detail_of`] exists to strip.
const UNEXPLAINED: &str = "(TypeSafe gave no reason this build could read)";

/// What this client is, told to the vendor.
///
/// A literal rather than `ganja-provider`'s `GANJA_USER_AGENT`, for
/// `websearch`'s reason: this crate names `ganja-permission` and nothing else
/// of ours, and one product name is not worth an edge in that graph.
const USER_AGENT: &str = concat!("ganja-code/", env!("CARGO_PKG_VERSION"));

/// Everything one call needs that is not the call: where to send it, what to
/// send it as, and the credential that pays for it.
///
/// Read **once**, at construction, rather than at each call. The `evaluate`
/// tool's consent disclosure has to name the host the content would go to,
/// and a `describe` that re-read the environment would be describing a
/// different request from the one `run` would send.
#[derive(Clone)]
pub struct Settings {
    /// The API key. Held as a secret so it is wiped on drop and cannot reach
    /// a `Debug` rendering; exposed at exactly one line, in [`Client::send`].
    key: SecretString,
    /// The endpoint, already joined and already checked.
    endpoint: Url,
    /// The model id a request names when it names none of its own.
    model: String,
    /// The deadline over one whole exchange. A field rather than a constant
    /// only so a test can hold a listener to a bound it can actually wait
    /// for; every shipped construction is [`TIMEOUT`].
    timeout: Duration,
}

impl Settings {
    /// What this process's environment configures, or [`None`] where it
    /// configures nothing.
    ///
    /// An empty or whitespace-only variable is no value, under `websearch`'s
    /// rule: a variable exported blank by a shell profile would otherwise
    /// select this tool and then fail against the vendor.
    ///
    /// # Errors
    ///
    /// [`Error::RefusedBase`] when [`BASE_ENV`] is set to something that
    /// would put the key on the wire in the clear. A missing [`KEY_ENV`] is
    /// `Ok(None)` and not an error: not being configured is the ordinary
    /// case.
    pub fn from_env() -> Result<Option<Self>, Error> {
        let read = |name| std::env::var(name).ok().filter(|value| !value.trim().is_empty());

        let Some(key) = read(KEY_ENV) else {
            return Ok(None);
        };
        let base = match read(BASE_ENV) {
            Some(named) => Self::base_from(named.trim())?,
            None => Self::base_from(DEFAULT_BASE)?,
        };
        let model = read(MODEL_ENV)
            .map_or_else(|| DEFAULT_MODEL.to_owned(), |named| named.trim().to_owned());

        Ok(Some(Self::new(key, base, model)?))
    }

    /// Settings assembled from values rather than from the environment.
    ///
    /// `base` has already passed [`Settings::base_from`], which is the only
    /// way to make one.
    ///
    /// # Errors
    ///
    /// [`Error::RefusedBase`] where the endpoint path will not join onto
    /// `base`. No URL [`Settings::base_from`] accepts can reach this — every
    /// `https` and `http` URL can be a base — but the alternative spelling
    /// would quietly POST to the base path itself, which is a request nobody
    /// asked for sent to a URL nobody named.
    pub(crate) fn new(key: String, base: Url, model: String) -> Result<Self, Error> {
        let endpoint = base.join(PATH).map_err(|_unprintable| Error::RefusedBase)?;

        Ok(Self { key: SecretString::from(key), endpoint, model, timeout: TIMEOUT })
    }

    /// The same settings under a different deadline.
    ///
    /// The one caller is the test that asks what happens when the vendor
    /// accepts a connection and never answers: ten seconds of real time is
    /// not a wait a suite can afford, and a paused clock cannot serve —
    /// tokio auto-advances it while real socket I/O is pending, so the
    /// timeout would fire against an answering listener too and the test
    /// could not fail.
    #[cfg(test)]
    pub(crate) fn within(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The base URL `text` spells, or a refusal.
    ///
    /// Plain HTTP is allowed to loopback and nowhere else: the bytes never
    /// reach a network there, which is what this crate's own fixtures rely
    /// on. Everything else has to be `https`, because the key travels in a
    /// header on every request.
    ///
    /// The host is compared as a **parsed host**, never as text. Every cheap
    /// spelling of this check is bypassable — `http://127.0.0.1.evil.com`
    /// beats a prefix match, `http://127.0.0.1@evil.com` beats a substring
    /// match, `http://localhost.evil.com` beats a starts-with — and all three
    /// are ordinary hosts belonging to whoever registered them.
    ///
    /// A trailing slash is added where the path lacks one, so that joining
    /// the endpoint path onto a base carrying a prefix keeps the prefix.
    ///
    /// A query or a fragment is **refused** rather than carried. Joining the
    /// endpoint path drops both silently, so a base that carried one would
    /// send a request the person who configured it did not describe — and a
    /// gateway is exactly where a token gets put in a query string.
    ///
    /// # Errors
    ///
    /// [`Error::RefusedBase`] when `text` is not a URL, carries a query or a
    /// fragment, or is one this module will not put a credential on. The
    /// value is never echoed.
    pub fn base_from(text: &str) -> Result<Url, Error> {
        let mut parsed = Url::parse(text).map_err(|_unprintable| Error::RefusedBase)?;

        if !reachable_in_the_clear(&parsed) {
            return Err(Error::RefusedBase);
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(Error::RefusedBase);
        }
        if !parsed.path().ends_with('/') {
            let joined = format!("{}/", parsed.path());
            parsed.set_path(&joined);
        }

        Ok(parsed)
    }

    /// The host a request would be sent to, for the consent disclosure.
    #[must_use]
    pub fn host(&self) -> &str {
        self.endpoint.host_str().unwrap_or_default()
    }

    /// The model a request names when it names none of its own.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Written by hand, and this is not decoration. The endpoint is a parsed
/// base URL, and a base URL is allowed to carry a credential in its userinfo
/// — so a derived `Debug` would put one in any message that formats these
/// settings. The host is what a reader needs and all they get.
impl std::fmt::Debug for Settings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Settings")
            .field("host", &self.host())
            .field("model", &self.model)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// Whether `url` may be spoken to at all, given that the request carries a
/// secret.
///
/// The mirror of `ganja_provider::provider::reachable_in_the_clear`, held to
/// it by `crates/ganja-core/tests/typesafe_base_url.rs` over one shared
/// table, in both directions, from the one crate that can see both. Copied
/// rather than shared because this crate may not name that crate. The query
/// and fragment clause below is the one place the copy is deliberately
/// stricter, and that test says so rather than sharing the table with it.
fn reachable_in_the_clear(url: &Url) -> bool {
    // `Url` has already done the parsing that makes this safe: whatever sits
    // before an `@` is userinfo and never reaches `host()`, and a host that
    // merely contains an address is a domain, not that address.
    let loopback = match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        // Only the exact name. RFC 6761 reserves everything under
        // `.localhost` for loopback too, but that is a promise about
        // resolvers rather than one the resolver on this machine has to keep,
        // and a suffix match is the shape of bypass this refuses.
        Some(Host::Domain(name)) => name == "localhost",
        None => false,
    };

    url.scheme() == "https" || (url.scheme() == "http" && loopback)
}

/// Everything one evaluation can go wrong as.
///
/// `#[non_exhaustive]` because the vendor's own status table is theirs to
/// extend and a new arm here must not be a breaking change for the two
/// surfaces above.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The configured base URL would put the key on the wire in the clear.
    /// The URL is deliberately absent from this message.
    #[error(
        "{BASE_ENV} must be https, or http to loopback; anything else puts the API key on the \
         wire in the clear"
    )]
    RefusedBase,
    /// The request did not pass [`Request::checked`], so nothing was sent.
    #[error("{0}")]
    InvalidRequest(String),
    /// The vendor refused the request outright: 401 and 403 mean the
    /// credential, any other 4xx that is not 422 means the request itself.
    /// Either way there is nothing a second attempt would change.
    ///
    /// Two statuses in this range are exceptions on paper — a 408 request
    /// timeout and a 425 too early could both survive a later attempt — and
    /// they are treated as refusals anyway, because this client deliberately
    /// has no second attempt to give them. One consent is one transmission.
    #[error("TypeSafe refused the request with HTTP {status}")]
    Rejected {
        /// The status that came back.
        status: u16,
    },
    /// HTTP 422: the body failed the vendor's own validation, and `detail`
    /// is what it said about which field.
    #[error("TypeSafe rejected the questions (HTTP 422): {detail}")]
    Invalid {
        /// The vendor's explanation, clamped to [`MAX_DETAIL`] bytes.
        detail: String,
    },
    /// A failure a later attempt might not hit: 429, 529, any other 5xx, and
    /// **any 3xx** — with redirects disabled a 302 is a failure naming its
    /// status, never a body to parse.
    #[error("TypeSafe is unavailable (HTTP {status})")]
    Unavailable {
        /// The status that came back.
        status: u16,
    },
    /// The deadline passed with no complete answer.
    #[error("TypeSafe did not answer in time")]
    Timeout,
    /// The answer was larger than this client will hold.
    #[error("the TypeSafe response exceeds the {} KiB limit", MAX_RESPONSE / 1024)]
    TooLarge,
    /// The exchange failed below HTTP: no client, no connection, a reset.
    #[error("the request to TypeSafe did not complete: {0}")]
    Transport(String),
    /// A 2xx whose body is not an answer this build can read.
    #[error("the TypeSafe response could not be read: {0}")]
    Malformed(String),
    /// The turn was cancelled while the request was in flight.
    #[error("the call was cancelled")]
    Cancelled,
}

/// What the model reads when an evaluation fails.
///
/// The sentences are the point: every arm says whether to try again, and the
/// ones that cannot succeed say to carry on without the judgement rather than
/// leaving the model to decide that a probability it never got was load
/// bearing.
impl From<Error> for ToolError {
    fn from(error: Error) -> Self {
        match error {
            Error::Cancelled => Self::Cancelled,
            Error::InvalidRequest(message) => Self::InvalidArgs(message),
            // Two sentences, because the two cases have different remedies
            // and a model that reads "check your API key" for a 404 will go
            // and tell the user something false.
            Error::Rejected { status } if matches!(status, 401 | 403) => Self::Failed(format!(
                "TypeSafe refused the credential (HTTP {status}); check that {KEY_ENV} holds a \
                 valid key. This was rejected, so do not retry — continue without this \
                 judgement."
            )),
            Error::Rejected { status } => Self::Failed(format!(
                "TypeSafe refused the request itself (HTTP {status}), not the credential; check \
                 that {BASE_ENV} names the right endpoint and that the questions are shaped as \
                 the tool's schema describes. This was rejected, so do not retry — continue \
                 without this judgement."
            )),
            Error::Invalid { detail } => Self::Failed(format!(
                "TypeSafe rejected the questions (HTTP 422): {detail}. Correct the questions, or \
                 continue without this judgement."
            )),
            Error::Unavailable { status } => Self::Failed(format!(
                "TypeSafe is unavailable (HTTP {status}); continue without this judgement."
            )),
            Error::Timeout => Self::Failed(
                "TypeSafe did not answer in time; continue without this judgement.".to_owned(),
            ),
            other => Self::Failed(format!("{other}; continue without this judgement.")),
        }
    }
}

/// The content a judgement is made about.
///
/// Typed as the vendor types it — `string | object | array` — rather than as
/// a bare `serde_json::Value`, so that the schema the model is shown states
/// the constraint instead of schemars' always-true schema for a `Value`. A
/// number, a boolean or a null is not a state and does not decode.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum State {
    /// Plain text: a message, a diff, a file.
    Text(String),
    /// Structured data: a record, an application's current state.
    Object(BTreeMap<String, serde_json::Value>),
    /// An ordered sequence: a chat log, a list of records.
    Array(Vec<serde_json::Value>),
}

/// What a yes and a no mean, for a [`Question::Noul`].
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoulCriteria {
    /// What a value near 1 means.
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<String>,
    /// What a value near 0 means.
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<String>,
}

/// One narrow judgement to make about the state.
///
/// The tagged form is the vendor's: `{"type": "noul", ...}`. Unknown keys are
/// refused rather than forwarded, so a misspelled field is a refusal the
/// model can act on instead of a silently ignored instruction — which the
/// test `a_tagged_question_round_trips_and_refuses_an_unknown_key` proves
/// serde will actually do for an internally tagged enum of struct variants.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Question {
    /// A yes/no question, answered with the probability of yes.
    Noul {
        /// The question to evaluate.
        instructions: serde_json::Value,
        /// Optional descriptions of what a yes and a no mean.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// One option out of a named set, answered with the full distribution.
    Choice {
        /// What to decide.
        instructions: serde_json::Value,
        /// Option to rubric; `null` where an option needs no extra detail.
        criteria: BTreeMap<String, Option<String>>,
    },
    /// A position on ordered levels, answered with a weighted value.
    Score {
        /// What to rate.
        instructions: serde_json::Value,
        /// The levels, in order, lowest first.
        criteria: Vec<String>,
    },
}

impl Question {
    /// The wire name of this question's type.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }

    /// Why this question cannot be asked, where it cannot.
    fn refusal(&self, id: &str) -> Option<String> {
        match self {
            Self::Noul { .. } => None,
            Self::Choice { criteria, .. } if criteria.len() < 2 => Some(format!(
                "question `{id}` is a choice with {} option(s); a choice needs at least two.",
                criteria.len()
            )),
            Self::Score { criteria, .. } if criteria.len() < 2 => Some(format!(
                "question `{id}` is a score with {} level(s); a score needs at least two.",
                criteria.len()
            )),
            Self::Choice { .. } | Self::Score { .. } => None,
        }
    }
}

/// One evaluation, already validated and already serialized.
///
/// The body is built once, in [`Request::checked`], and both the size limit
/// and the consent disclosure read that one number — so what the dialog says
/// would be sent is the byte count of what is sent.
#[derive(Clone)]
pub struct Request {
    /// Exactly the bytes the POST carries.
    body: Vec<u8>,
    /// The model id this request names.
    model: String,
    /// How many questions it asks.
    questions: usize,
    /// What the state is, in the shape the consent disclosure says it.
    state: StateKind,
}

/// Written by hand for the reason [`Settings`]'s is: the body is the project
/// content a model chose, and a derived `Debug` would spill up to
/// [`MAX_BODY`] of it into any message that formats a request.
impl std::fmt::Debug for Request {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Request")
            .field("bytes", &self.body.len())
            .field("questions", &self.questions)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// What a [`State`] is, without the state.
///
/// Computed in [`Request::checked`], where the state is in hand, so that the
/// consent disclosure never re-parses what it is about to describe — and so
/// that what the dialog says is the shape of what was actually validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateKind {
    /// Plain text.
    Text,
    /// An array, and how many elements it has.
    Array(usize),
    /// An object, and its top-level keys in order.
    Keys(Vec<String>),
}

impl StateKind {
    /// What `state` is.
    fn of(state: &State) -> Self {
        match state {
            State::Text(_) => Self::Text,
            State::Array(elements) => Self::Array(elements.len()),
            State::Object(fields) => Self::Keys(fields.keys().cloned().collect()),
        }
    }
}

/// The disclosure's own words for it: `text`, `array of 12`, `keys: a, b, c`.
///
/// A caller that has a width to fit cuts the result; nothing is cut here,
/// because where to cut is the surface's question and this type does not know
/// which surface is asking.
impl std::fmt::Display for StateKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text => formatter.write_str("text"),
            Self::Array(elements) => write!(formatter, "array of {elements}"),
            Self::Keys(names) => write!(formatter, "keys: {}", names.join(", ")),
        }
    }
}

impl Request {
    /// The one validator both surfaces use.
    ///
    /// Limits are ganja's own, since the vendor documents none: one to
    /// [`MAX_QUESTIONS`] questions, ids of `[A-Za-z0-9_.-]{1,64}`, at least
    /// two options for a choice and two levels for a score (the vendor
    /// requires the latter), and a serialized body within [`MAX_BODY`].
    ///
    /// `model` is a free string: the two aliases [`DEFAULT_MODEL`] and
    /// [`PREVIEW_MODEL`] are what the surfaces advertise, and a versioned id
    /// such as `jev-1.13.0` is equally valid, so nothing here decides which
    /// ids the vendor serves.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] naming the limit that was passed. Nothing is
    /// sent, and nothing needs to be: every one of these is decidable here.
    pub fn checked(
        state: State,
        questions: BTreeMap<String, Question>,
        model: String,
    ) -> Result<Self, Error> {
        let refuse = |message: String| Err(Error::InvalidRequest(message));

        if questions.len() < MIN_QUESTIONS {
            return refuse("an evaluation needs at least one question.".to_owned());
        }
        if questions.len() > MAX_QUESTIONS {
            return refuse(format!(
                "an evaluation carries at most {MAX_QUESTIONS} questions; this one carries {}.",
                questions.len()
            ));
        }
        for (id, question) in &questions {
            if !is_id(id) {
                return refuse(format!(
                    "question id `{id}` is not usable; ids are 1 to {MAX_ID} characters of \
                     letters, digits, `_`, `.` or `-`."
                ));
            }
            if let Some(message) = question.refusal(id) {
                return refuse(message);
            }
        }
        if model.trim().is_empty() {
            return refuse("the model id is empty.".to_owned());
        }

        let kind = StateKind::of(&state);
        let wire = Wire { state: &state, questions: &questions, model: &model };
        let body = serde_json::to_vec(&wire)
            .map_err(|error| Error::InvalidRequest(format!("the request is not JSON: {error}")))?;

        if body.len() > MAX_BODY {
            return refuse(format!(
                "the request body is {} bytes; the limit over state and questions together is \
                 {MAX_BODY} bytes. Send less state, or shorter instructions.",
                body.len()
            ));
        }

        Ok(Self { body, model, questions: questions.len(), state: kind })
    }

    /// How many bytes this request would put on the wire.
    ///
    /// The consent disclosure's number and the cap's number, which is why
    /// there is only one of them.
    #[must_use]
    pub fn body_len(&self) -> usize {
        self.body.len()
    }

    /// The model id this request names.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// How many questions this request asks.
    #[must_use]
    pub fn questions(&self) -> usize {
        self.questions
    }

    /// What this request's state is, for the consent disclosure.
    #[must_use]
    pub fn state(&self) -> &StateKind {
        &self.state
    }
}

/// The request body's field order, fixed here so the wire shape is one
/// decision rather than a consequence of [`Request`]'s field order.
#[derive(Serialize)]
struct Wire<'request> {
    /// The content to judge.
    state: &'request State,
    /// What to judge about it.
    questions: &'request BTreeMap<String, Question>,
    /// Which model judges.
    model: &'request str,
}

/// Whether `id` is a usable question id.
fn is_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// One answer, under the id its question was asked by.
///
/// Parsed tolerantly: a `type` this build does not know becomes
/// [`Answer::Other`] rather than failing the whole response, because the
/// vendor's own migration notes show answer shapes being renamed between
/// versions and one unknown answer must not cost the others.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    /// A yes/no probability, 0 (no) to 1 (yes). Near 0.5 is undecided.
    Noul {
        /// The probability of yes.
        noul: f64,
    },
    /// The chosen option, with the distribution it was chosen from.
    Choice {
        /// The highest-probability option.
        choice: String,
        /// Every option mapped to its probability.
        probabilities: BTreeMap<String, f64>,
        /// How certain the model is, derived from the distribution.
        confidence: f64,
    },
    /// A probability-weighted position on the levels, which can land between
    /// them.
    Score {
        /// The weighted value.
        score: f64,
        /// Each level index mapped back to its description.
        legend: BTreeMap<String, String>,
        /// Each level index mapped to its probability.
        probabilities: BTreeMap<String, f64>,
        /// How certain the model is, derived from the distribution.
        confidence: f64,
    },
    /// An answer shape this build does not know, kept whole.
    #[serde(untagged)]
    Other(serde_json::Value),
}

impl Answer {
    /// The wire name of this answer's type, including the one an
    /// [`Answer::Other`] carried.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
            Self::Other(value) => {
                value.get("type").and_then(serde_json::Value::as_str).unwrap_or("unknown")
            }
        }
    }
}

/// What one request cost.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// Tokens the state and questions came to.
    #[serde(default)]
    pub input_tokens: u64,
    /// Tokens the answers came to. TypeSafe charges nothing for these.
    #[serde(default)]
    pub output_tokens: u64,
}

/// One evaluation's answers.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Response {
    /// The model that actually performed the evaluation, which for an alias
    /// is the version it resolved to.
    pub model: String,
    /// One answer per question, under the ids the questions were asked by.
    pub answers: BTreeMap<String, Answer>,
    /// What the request cost.
    #[serde(default)]
    pub usage: Usage,
}

/// The one door to the vendor.
pub struct Client {
    /// Where to send, what to send as, and what pays for it.
    settings: Settings,
    /// Built once: a client per call would discard the connection pool and
    /// re-verify TLS on every judgement.
    http: reqwest::Client,
}

impl Client {
    /// A client over `settings`.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when no HTTP client can be built, which in
    /// practice means the TLS backend failed to initialize.
    pub fn new(settings: Settings) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            // A request here carries the API key in a header, so a redirect
            // is somebody else's host asking for it.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            // `without_url` here too, though a builder error carries none
            // today: one rule for every reqwest error in this module beats
            // one rule and an exception nobody re-checks.
            .map_err(|error| {
                Error::Transport(format!("no HTTP client: {}", error.without_url()))
            })?;

        Ok(Self { settings, http })
    }

    /// What this client was configured with.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Sends `request` and returns what came back. **One attempt.**
    ///
    /// The whole exchange — connect, headers and body — is under one
    /// deadline, and `cancel` ends it at any point, so a turn the user
    /// abandoned does not go on holding a socket open.
    ///
    /// # Errors
    ///
    /// Any [`Error`] but [`Error::InvalidRequest`] and
    /// [`Error::RefusedBase`], both of which are decided before a [`Request`]
    /// exists.
    pub async fn evaluate(
        &self,
        request: &Request,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        // Before the socket, not only during it. A turn cancelled while this
        // call sat in a queue would otherwise still open a connection and
        // still send the content, and the consent was for a judgement the
        // caller no longer wants. Once the bytes are out the race is real and
        // unavoidable: cancelling then abandons the answer, it does not
        // unsend the request.
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let started = Instant::now();
        let (status, answered) = tokio::select! {
            sent = tokio::time::timeout(self.settings.timeout, self.send(request)) => {
                sent.unwrap_or(Err(Error::Timeout))
            }
            () = cancel.cancelled() => Err(Error::Cancelled),
        }?;

        // Never the key, never the state, never a URL: a base URL is
        // configuration and configuration may carry a credential in its
        // userinfo.
        tracing::debug!(
            status,
            latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            input_tokens = answered.usage.input_tokens,
            "typesafe answered"
        );

        Ok(answered)
    }

    /// One request, from the first byte out to the parsed answer, and the
    /// status it came back with.
    async fn send(&self, request: &Request) -> Result<(u16, Response), Error> {
        let sent = self
            .http
            .post(self.settings.endpoint.clone())
            // The one line the key is exposed at.
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.settings.key.expose_secret()),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .body(request.body.clone())
            .send()
            .await
            // `without_url` before anything reads this. reqwest's `Display`
            // appends `for url (<url>)`, userinfo and all, and this string is
            // model-facing — the same reason `ganja-provider`'s auth flows
            // strip it (`auth/grok.rs`, `auth/device.rs`, `auth/cursor.rs`,
            // `auth/mcp_oauth.rs`).
            .map_err(|error| Error::Transport(error.without_url().to_string()))?;

        // The status first. A body read out of a 401 is an error page, and
        // handing the caller a parse failure for a rejected credential would
        // be the one answer it cannot act on.
        let status = sent.status().as_u16();
        if !sent.status().is_success() {
            // An error body that will not read is a fact, not an empty
            // string: rendered as one, a 422 would reach the model as
            // "(HTTP 422): ." and read like a bug in ganja.
            let said = match collect(sent, MAX_ERROR_BODY).await {
                Ok(body) => String::from_utf8_lossy(&body).into_owned(),
                Err(_unreadable) => UNREADABLE.to_owned(),
            };

            return Err(refusal(status, &said));
        }

        let body = collect(sent, MAX_RESPONSE).await?;
        let answered =
            serde_json::from_slice(&body).map_err(|error| Error::Malformed(error.to_string()))?;

        Ok((status, answered))
    }
}

/// Which failure a non-2xx status is.
fn refusal(status: u16, body: &str) -> Error {
    match status {
        // With redirects disabled a 3xx is a failure naming its status. It is
        // `Unavailable` rather than `Rejected` because the vendor moving an
        // endpoint is exactly the case a later attempt survives.
        300..=399 => Error::Unavailable { status },
        422 => Error::Invalid { detail: detail_of(body) },
        429 => Error::Unavailable { status },
        400..=499 => Error::Rejected { status },
        _ => Error::Unavailable { status },
    }
}

/// What the vendor said about a refusal, as a sentence.
///
/// Three shapes, all measured against the live API on 2026-09-18: a 401
/// answers `{"detail": {"error_type": …, "message": …}}`, a 422 answers
/// `{"detail": [{"loc": […], "msg": …, "input": …}]}`, and anything else is
/// passed through as the text it was.
///
/// The 422 array is reduced to its `loc` and `msg` rather than rendered
/// whole, because each element's `input` is **the request body echoed back**
/// — so the unreduced form would put the state into a message the model
/// reads, and would spend the clamp's budget on content the model already
/// sent.
fn detail_of(body: &str) -> String {
    let said = match serde_json::from_str::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(fields)) => match fields.get("detail") {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(serde_json::Value::Array(faults)) => {
                faults.iter().map(fault).collect::<Vec<_>>().join("; ")
            }
            Some(serde_json::Value::Object(named)) => said_by(named),
            // Every remaining shape — a number, a bool, a nested array — says
            // nothing a reader can use, and the one thing it might carry is
            // the echo. None of it travels.
            Some(_) | None => UNEXPLAINED.to_owned(),
        },
        // Not the documented envelope, either because it is not an object or
        // because it is not JSON at all.
        Ok(_) | Err(_) => {
            let text = body.trim();

            // A body that is not the documented envelope at all is passed on
            // as the text it was, because a proxy's plain-text 502 is exactly
            // the sentence a reader wants — unless it is JSON, in which case
            // it is a shape this build does not know and may well be carrying
            // the request back.
            if text.is_empty() || text.starts_with(['{', '[']) {
                UNEXPLAINED.to_owned()
            } else {
                text.to_owned()
            }
        }
    };

    clamp(&said, MAX_DETAIL)
}

/// The sentence an object-shaped `detail` carries, if it carries one.
///
/// `message` is the vendor's own spelling, measured against a live 401 on
/// 2026-09-18. Anything else falls to the fixed sentence rather than being
/// rendered whole: this is the shape most likely to grow an `input` field.
fn said_by(named: &serde_json::Map<String, serde_json::Value>) -> String {
    named
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| UNEXPLAINED.to_owned(), str::to_owned)
}

/// One element of a 422's `detail` array, as the field it names and what is
/// wrong with it.
///
/// **Never the element whole.** Each one carries `input`, which is the
/// request body echoed back, so a fallback that rendered the element would
/// put the state into what the model reads. An element with a `msg` and no
/// usable `loc` is that message alone; an element with neither — the shape a
/// bare `{"input": …}` is — is the fixed sentence.
fn fault(fault: &serde_json::Value) -> String {
    let what = fault.get("msg").and_then(serde_json::Value::as_str);
    let Some(what) = what else {
        return UNEXPLAINED.to_owned();
    };
    let Some(serde_json::Value::Array(where_)) = fault.get("loc") else {
        return what.to_owned();
    };
    let named = where_
        .iter()
        .map(|part| match part {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".");

    if named.is_empty() { what.to_owned() } else { format!("{named}: {what}") }
}

/// `text` cut to at most `limit` bytes, on a character boundary.
fn clamp(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }

    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}…", &text[..end])
}

/// Reads a response body, refusing one too big to be worth holding.
///
/// The declared length is checked first, so an oversized answer costs nothing
/// to refuse, and the body is measured as it streams, so one that lies about
/// its length — or declares none at all — is refused at the same boundary
/// rather than after it has been buffered whole.
async fn collect(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, Error> {
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(Error::TooLarge);
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| Error::Transport(error.without_url().to_string()))?;

        if body.len() + chunk.len() > limit {
            return Err(Error::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

// `pub(crate)` for one item inside it: the loopback fixture, which
// `evaluate_tests.rs` drives the tool above this client against. A second copy
// of that server would be a second thing to keep in step with the vendor.
#[cfg(test)]
#[path = "typesafe_tests.rs"]
pub(crate) mod tests;
