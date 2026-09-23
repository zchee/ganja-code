//! The `webfetch` tool: reads a page and hands the model its text.
//!
//! Spec: upstream `packages/opencode/src/tool/webfetch.ts` and `webfetch.txt`.
//!
//! Unlike the provider client, this one follows redirects. That client refuses
//! them because every provider request carries an API key in a header, and a
//! 3xx is an instruction to hand that header to a host of the server's
//! choosing. Nothing here carries a credential — the request is a bare `GET`
//! with a user agent and an `Accept` — so a redirect costs nothing to follow,
//! and refusing them would break the large fraction of the web that answers
//! `http` with a 301 to `https`.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use markup5ever_rcdom::{Node, NodeData};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, truncate};

/// Most bytes a response may carry before it is refused. Upstream's 5 MB.
const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;

/// How long a fetch runs when the call names no timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest timeout a call may ask for; longer requests are clamped to it.
const MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// What the endpoint is told the client is.
///
/// A browser string, as upstream sends: a great many sites answer an obviously
/// automated agent with a challenge page, and the point of the tool is to
/// return what a person looking at the URL would have seen.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// Elements whose text belongs to the machine rather than the reader.
const SKIPPED: [&str; 6] = ["script", "style", "noscript", "iframe", "object", "embed"];

/// Elements a plain-text reading puts a blank line around.
///
/// Not the CSS block set, which is a question about rendering boxes: these are
/// the elements that separate *prose*, so a heading does not run into the
/// paragraph under it and one list item does not run into the next. Anything
/// unlisted is treated as inline, which is the right default — a tag a text
/// rendering has never heard of is far more likely to be a `<span>` than a
/// `<section>`. `td`/`th` are absent on purpose: rows separate through `tr`,
/// and cells gluing within a row beats a table exploding into one paragraph
/// per cell.
const BLOCK: [&str; 38] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "button",
    "caption",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "label",
    "legend",
    "li",
    "main",
    "nav",
    "ol",
    "optgroup",
    "option",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "title",
    "tr",
    "ul",
];

/// The deepest chain of nodes [`to_markdown`] hands to htmd's converter.
///
/// That converter walks a tree by recursion: `walk_node` descends through
/// `walk_children` and back into `walk_node`, a frame pair per level, and an
/// element handler that re-walks its own subtree — a table's, a list's — adds
/// frames on top of that (htmd 0.5.5, `src/dom_walker.rs:16` and `:237`). It
/// carries no depth guard, and nobody has asked it for one: its tracker at
/// <https://github.com/letmutex/htmd/issues> held fourteen issues on
/// 2026-09-01, none of them about recursion or stack depth. The bound
/// therefore belongs to this caller, because the page is not this caller's:
/// `webfetch` fetches a URL a *model* chose after reading pages a stranger may
/// have written, so how deeply the document nests is an attacker's to decide,
/// and a stack that runs out ends the **process** rather than the call — the
/// one failure a tool result cannot carry back to the model.
///
/// The number is measured rather than argued. On the 2 MiB stack a
/// `spawn_blocking` thread gets, the walker survives a chain of 324 nodes of
/// nested `<div>` and dies at 356, survives 260 of `<ul><li>` and dies at 324,
/// and survives 244 of nested `<table>` and dies at 260 — the heaviest of the
/// three, because that handler re-walks what it buffers. So the ceiling is not
/// one number but a range ending near 250, and a cap has to sit under the
/// worst of it rather than the best. 128 is half that worst case and still
/// several times deeper than prose reaches; markup that nests past it was far
/// likelier built to nest than written to be read. Being wrong in this
/// direction costs the page's formatting, not the page.
const MAX_NESTING: usize = 128;

/// Most redirects one fetch will follow, which is reqwest's own default. Spelled
/// out because guarding each hop means policing the chain here rather than
/// leaving it to the client.
///
/// The policy reads it against `previous()`, which holds the URL the fetch
/// started at as well as every hop already followed, so a chain is stopped
/// once that list is longer than this — the comparison reqwest's own limit
/// makes. The response that asked for one hop more is what the model is
/// handed.
const MAX_REDIRECTS: usize = 10;

/// How the fetched page should be handed back.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Format {
    /// Tags stripped, text kept.
    Text,
    /// Prose, with the markup rendered as markdown.
    #[default]
    Markdown,
    /// The response body exactly as it arrived.
    Html,
}

/// What the model passes to `webfetch`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The URL to fetch content from
    url: String,
    /// The format to return the content in (text, markdown, or html). Defaults to markdown.
    #[serde(default)]
    format: Format,
    /// Optional timeout in seconds (max 120)
    #[serde(default)]
    timeout: Option<u64>,
}

/// Fetches a URL.
pub struct WebfetchTool {
    /// Whether a URL resolving to an address on this machine, on a private
    /// network or in a reserved range is fetched rather than refused. See
    /// [`WebfetchTool::allowing_private`].
    allow_private: bool,
    /// Where a clamped page spills its whole text, when it must not go where
    /// [`truncate::clamp`] would put it.
    ///
    /// Only a test ever sets this, through `WebfetchTool::spilling_into` —
    /// gated `#[cfg(test)]`, so there is no item here to link. The seam is
    /// `shell.rs`'s (`ShellTool::spill_dir`) and exists for its reason: a test
    /// spilling into the resolved data directory would fill a real person's
    /// `~/.local/share` with fixtures, and one that merely avoided naming a
    /// directory would pass on the pathless notice without proving a file was
    /// written. Every other build leaves it empty.
    spill_dir: Option<PathBuf>,
    /// What a test put in place of the system resolver, set only through
    /// `WebfetchTool::resolving_through`. The type is `#[cfg(test)]` too, so a
    /// shipped build has no way to hold one: it resolves through the system,
    /// under the guard.
    #[cfg(test)]
    double: Option<Arc<Double>>,
}

impl WebfetchTool {
    /// The tool as it ships: an address on this machine, on a private network
    /// or in a reserved range is refused.
    ///
    /// A deliberate divergence — upstream fetches whatever it is given. The
    /// URL here is one a *model* chose, and a model chooses it after reading
    /// files and pages that a stranger may have written, so "fetch this and
    /// tell me what it says" is a working read of a metadata service, a
    /// database admin port, or a router's console. The provider client already
    /// refuses to speak plainly to anything but loopback for the same class of
    /// reason; this is that judgement applied where the address, and not the
    /// credential, is what matters (deviation:
    /// `webfetch-refuses-private-addresses`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            allow_private: false,
            spill_dir: None,
            #[cfg(test)]
            double: None,
        }
    }

    /// The tool with that refusal lifted, for a session whose config asked for
    /// it.
    ///
    /// A real need — an intranet wiki, a service on the developer's own
    /// machine — and one nobody can serve from here, because which private
    /// addresses are legitimate is a question only the person running the
    /// session can answer.
    #[must_use]
    pub fn allowing_private() -> Self {
        Self { allow_private: true, ..Self::new() }
    }

    /// The same tool, spilling into `dir` rather than the resolved data
    /// directory. See `WebfetchTool::spill_dir`.
    #[cfg(test)]
    fn spilling_into(self, dir: &Path) -> Self {
        Self { spill_dir: Some(dir.to_owned()), ..self }
    }

    /// The same tool, resolving through `double` rather than the system. See
    /// `WebfetchTool::double`.
    #[cfg(test)]
    fn resolving_through(self, double: Double) -> Self {
        Self { double: Some(Arc::new(double)), ..self }
    }

    /// A fresh guard for one fetch.
    fn guard(&self) -> Guard {
        Guard {
            allow_private: self.allow_private,
            destinations: Mutex::default(),
            connected: Mutex::default(),
            proxied: AtomicBool::new(false),
            #[cfg(test)]
            double: self.double.clone(),
        }
    }
}

impl Default for WebfetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebfetchTool {
    fn id(&self) -> &str {
        "webfetch"
    }

    fn description(&self) -> &str {
        include_str!("webfetch.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let url = args.get("url").and_then(serde_json::Value::as_str).unwrap_or_default();

        format!("fetch {url}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        // Only the two schemes the tool is for. Anything else — `file`, `data`,
        // a bare host with no scheme at all — is either a way to read the disk
        // through a tool that is gated as a network call, or a typo.
        if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
            return Err(ToolError::InvalidArgs(
                "URL must start with http:// or https://".to_owned(),
            ));
        }

        let timeout = args
            .timeout
            .map_or(DEFAULT_TIMEOUT, |seconds| Duration::from_secs(seconds).min(MAX_TIMEOUT));

        let spill = self.spill_dir.as_deref();

        tokio::select! {
            fetched = fetch(&args, timeout, Arc::new(self.guard()), spill) => fetched,
            () = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
        }
    }
}

/// Whether `address` is one this tool refuses by default.
///
/// The set is the one a request from this process can reach and a request from
/// the internet cannot: loopback (`127.0.0.0/8`, `::1`), the RFC 1918 ranges
/// (`10/8`, `172.16/12`, `192.168/16`) with their IPv6 counterpart
/// (`fc00::/7`), and link-local (`169.254/16`, `fe80::/10`) — which is where
/// every cloud's instance metadata service lives.
///
/// The unspecified addresses are in the set too, though nothing named them:
/// `0.0.0.0` and `::` route to this machine on every stack that matters, so
/// leaving them out would make the loopback line above decorative.
///
/// So are these special-purpose ranges, because each is either somebody's
/// private network or no single host on the internet at all:
///
/// - `0.0.0.0/8`, "this host on this network" (RFC 1122), which a stack
///   routes the way it routes `0.0.0.0`;
/// - `100.64.0.0/10`, the shared address space (RFC 6598) — a carrier's NAT,
///   and the range overlay networks such as Tailscale number their peers in;
/// - `192.0.0.0/24`, IETF protocol assignments (RFC 6890);
/// - `198.18.0.0/15`, benchmarking (RFC 2544), a lab network by definition;
/// - `240.0.0.0/4`, reserved (RFC 1112), and the limited broadcast address
///   `255.255.255.255` at its top (RFC 919);
/// - multicast, `224.0.0.0/4` (RFC 5771) and `ff00::/8` (RFC 4291), which
///   names a group on some network rather than one host;
/// - `fec0::/10`, IPv6 site-local — deprecated by RFC 3879, and still routed
///   by a stack that never dropped it;
/// - the rest of `::/8`, which RFC 4291 reserves and which holds no host on
///   the internet: the IPv4-compatible `::a.b.c.d` (deprecated by RFC 4291),
///   SIIT's `::ffff:0:a.b.c.d` (RFC 2765) and the local-use NAT64 prefix
///   `64:ff9b:1::/48` (RFC 8215), which may carry a private IPv4 address where
///   the well-known prefix below may not;
/// - `2001:2::/48`, IPv6 benchmarking (RFC 5180), the twin of `198.18.0.0/15`.
///
/// An IPv6 address that carries an IPv4 one is refused as the address it
/// carries: v4-mapped `::ffff:0:0/96` (RFC 4291), NAT64's well-known prefix
/// `64:ff9b::/96` (RFC 6052) and 6to4's `2002::/16` (RFC 3056). A translator or
/// relay turns the last two into a packet to that v4 address, so
/// `64:ff9b::a00:1` reaches exactly what `10.0.0.1` would, and
/// `64:ff9b::808:808` reaches a public host and is not refused.
fn blocked(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_broadcast()
                || address.is_multicast()
                // The rest have no stable predicate — `is_shared`,
                // `is_benchmarking` and `is_reserved` are still unstable, and
                // `0/8` and `192.0.0/24` have none at all — so they are spelled
                // as octets here rather than paid for with a crate-wide feature
                // gate.
                || matches!(
                    address.octets(),
                    [0, ..]
                        | [100, 64..=127, ..]
                        | [192, 0, 0, _]
                        | [198, 18..=19, ..]
                        | [240..=255, ..]
                )
        }
        IpAddr::V6(address) => match address.segments() {
            // An address written as `::ffff:127.0.0.1` is the v4 address it
            // wraps, and refusing it as one is the whole point of unwrapping
            // here — as it is for the NAT64 and 6to4 forms, whose v4 address a
            // translator or relay connects to one hop further on.
            [0, 0, 0, 0, 0, 0xffff, high, low]
            | [0x64, 0xff9b, 0, 0, 0, 0, high, low]
            | [0x2002, high, low, ..] => {
                blocked(IpAddr::V4(Ipv4Addr::from_bits((u32::from(high) << 16) | u32::from(low))))
            }
            // The rest of `::/8`, the local-use NAT64 `/48` among it. After
            // the carriers above, so a public address behind the well-known
            // NAT64 prefix or a v4-mapped one still passes.
            [first, ..] if (first & 0xff00) == 0 => true,
            // Benchmarking.
            [0x2001, 0x0002, 0, ..] => true,
            // Site-local has no stable predicate, so its prefix is matched
            // here.
            [first, ..] => {
                address.is_loopback()
                    || address.is_unspecified()
                    || address.is_multicast()
                    || address.is_unique_local()
                    || address.is_unicast_link_local()
                    || (first & 0xffc0) == 0xfec0
            }
        },
    }
}

/// One fetch's address guard: whether private addresses are refused, which
/// names the fetch was pointed at, what each of those resolved to when a
/// connection was made, and whether a connection looked up any other name.
///
/// Every answer of every lookup of a name this fetch was pointed at is
/// checked, not the first: a name answering with one public address and one
/// private one is the oldest way around a check that stops at the head of the
/// list, and one refused answer refuses the lookup whole rather than leaving
/// the connection to try the rest. A configured proxy's own name is looked up
/// through [`Resolver`] too, and resolves unchecked; see
/// [`Guard::connecting`].
struct Guard {
    /// Whether the session lifted the guard, in which case nothing is checked.
    allow_private: bool,
    /// The names this fetch was pointed at — the URL's host and each
    /// redirect's — lowercased, recorded by [`Guard::admit`] before a hop is
    /// requested.
    destinations: Mutex<HashSet<String>>,
    /// What [`Resolver`] answered for each of those names when a connection
    /// was made, which is what [`Guard::vouches_for`] reads. Whole sockets
    /// rather than addresses, because a proxy can share an address with an
    /// answer and still be another listener.
    connected: Mutex<HashMap<String, Vec<SocketAddr>>>,
    /// Whether a connection looked up a name this fetch was not pointed at —
    /// in this build, a proxy's — so no page it returns is stamped as one the
    /// guard checked.
    proxied: AtomicBool,
    /// What a test put in place of the system resolver. See
    /// `WebfetchTool::double`.
    #[cfg(test)]
    double: Option<Arc<Double>>,
}

impl Guard {
    /// Checks one hop before it is requested: the URL a fetch starts at, and
    /// every redirect before it is followed, because a hop is a URL somebody
    /// else chose.
    ///
    /// An address written into the URL is checked as written. No resolver is
    /// ever asked about it, so this is the only check it gets — and an exact
    /// one, since a literal cannot answer differently the second time. A name
    /// is resolved and every answer checked, and the connection's own lookup is
    /// checked again by [`Resolver`]. The check here is not what the connection
    /// uses; it is what a hop sent through a proxy still gets, since the proxy
    /// resolves that name and no lookup of ours is ever asked for it.
    ///
    /// Recording the name is what arms [`Resolver`]'s check of the
    /// connection's own lookup: [`Guard::connecting`] checks only names
    /// recorded here, so every hop must pass through this before it is
    /// requested.
    ///
    /// Blocks on the system resolver for a name.
    fn admit(&self, url: &reqwest::Url) -> Result<(), ToolError> {
        if self.allow_private {
            return Ok(());
        }

        let host = host_of(url)?;
        if let Ok(address) = host.parse::<IpAddr>() {
            return self.check(&host, &[SocketAddr::new(address, 0)]);
        }

        self.destinations
            .lock()
            .expect("the destination set is never poisoned")
            .insert(host.to_ascii_lowercase());
        let answers = self
            .lookup(&host)
            .map_err(|_error| ToolError::Failed(format!("{host} did not resolve")))?;
        if answers.is_empty() {
            return Err(ToolError::Failed(format!("{host} did not resolve")));
        }

        self.check(&host, &answers)
    }

    /// Checks a connection's own lookup of `host` before its answers are
    /// handed to the connector, and records them.
    ///
    /// Only a name this fetch was pointed at is checked. reqwest, as this
    /// workspace builds it, resolves nothing else but the proxy the person
    /// running the session configured, and that proxy is theirs to name: a
    /// proxy on this machine or on the office network is exactly where one
    /// usually is, and refusing it would refuse every fetch sent through it.
    /// Such a lookup marks the fetch proxied instead, so nothing it returns is
    /// stamped as checked ([`Guard::vouches_for`]).
    fn connecting(&self, host: &str, answers: &[SocketAddr]) -> Result<(), ToolError> {
        if self.allow_private {
            return Ok(());
        }

        let name = host.to_ascii_lowercase();
        if !self.destinations.lock().expect("the destination set is never poisoned").contains(&name)
        {
            // This branch is also where a destination would land if its
            // spelling here ever stopped matching what `admit` recorded, and
            // that would fail open, so it leaves a trace. Debug-escaped,
            // because a name is somebody else's text.
            tracing::debug!(
                name = ?name,
                "a name this fetch was not pointed at resolved without the address check; the \
                 fetch is treated as proxied"
            );
            self.proxied.store(true, Ordering::Release);

            return Ok(());
        }

        self.check(host, answers)?;
        self.connected
            .lock()
            .expect("the connection record is never poisoned")
            .entry(name)
            .or_default()
            .extend(answers.iter().copied());

        Ok(())
    }

    /// Refuses `host` if any of `answers` is an address the guard refuses.
    fn check(&self, host: &str, answers: &[SocketAddr]) -> Result<(), ToolError> {
        if !self.allow_private && answers.iter().any(|answer| self.refuses(*answer)) {
            return Err(refusal(host));
        }

        Ok(())
    }

    /// Whether [`blocked`] names `answer`'s address.
    ///
    /// A test's double may stand a few loopback sockets up as public hosts,
    /// and only those, only in a test build, are let through.
    fn refuses(&self, answer: SocketAddr) -> bool {
        #[cfg(test)]
        if self.double.as_ref().is_some_and(|double| double.public.contains(&answer)) {
            return false;
        }

        blocked(answer.ip())
    }

    /// Resolves `host` through the system, or through a test's double.
    ///
    /// Blocks: `getaddrinfo` has no asynchronous form, which is why
    /// [`Resolver`] calls this on a blocking thread.
    fn lookup(&self, host: &str) -> std::io::Result<Vec<SocketAddr>> {
        #[cfg(test)]
        if let Some(double) = &self.double {
            return double.lookup(host);
        }

        Ok((host, 0).to_socket_addrs()?.collect())
    }

    /// Whether the guard decided the socket a response to `url` came from:
    /// `peer` is `url`'s own address, or one [`Resolver`] answered for its
    /// name and checked, on the port a direct connection to that answer uses.
    ///
    /// Otherwise somebody else resolved the target — a proxy, which the
    /// person running the session may have configured in the environment or
    /// in the system's own settings — and what the guard checked is not what
    /// the page came from. The port is compared as well as the address
    /// because a proxy may carry only some hops of a fetch and share an
    /// address with an answer a direct hop connected to; and a fetch that
    /// looked up a proxy's name ([`Guard::connecting`]) is never vouched for.
    /// An unknown peer is that case too.
    fn vouches_for(&self, url: &reqwest::Url, peer: Option<SocketAddr>) -> bool {
        let (Some(peer), Ok(host), Some(port)) = (peer, host_of(url), url.port_or_known_default())
        else {
            return false;
        };
        if self.proxied.load(Ordering::Acquire) {
            return false;
        }
        // The socket the connector builds from an answer, as hyper-util's
        // `set_port` builds it: a port the URL names replaces the answer's,
        // and so does the URL's default when the answer carries none.
        let explicit = url.port().is_some();
        let peer_address = peer.ip().to_canonical();
        let reached = |answer: SocketAddr| {
            let answer_port = if explicit || answer.port() == 0 { port } else { answer.port() };

            answer.ip().to_canonical() == peer_address && answer_port == peer.port()
        };

        if let Ok(address) = host.parse::<IpAddr>() {
            return reached(SocketAddr::new(address, 0));
        }

        self.connected
            .lock()
            .expect("the connection record is never poisoned")
            .get(&host.to_ascii_lowercase())
            .is_some_and(|answers| answers.iter().copied().any(reached))
    }
}

/// The resolver every name a fetch connects to resolves through, so a
/// connection to a name uses the addresses [`Guard`] was shown.
///
/// Installed in place of reqwest's own, which is a second `getaddrinfo` that
/// nothing here checks: a name that answers one way when its hop is admitted
/// and another a moment later, when the connection is made, is refused at
/// the second answer rather than trusted on the first.
struct Resolver(Arc<Guard>);

impl reqwest::dns::Resolve for Resolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let guard = Arc::clone(&self.0);
        let host = name.as_str().to_owned();

        Box::pin(async move {
            let answers = {
                let guard = Arc::clone(&guard);
                let host = host.clone();
                tokio::task::spawn_blocking(move || guard.lookup(&host)).await
            };
            let answers = match answers {
                Ok(Ok(answers)) if !answers.is_empty() => answers,
                // Said as `Guard::admit` says it: reqwest renders a failed or
                // empty lookup as its own sentence naming the whole URL.
                _ => return Err(ToolError::Failed(format!("{host} did not resolve")).into()),
            };
            guard.connecting(&host, &answers)?;

            Ok(Box::new(answers.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// What a test puts in place of the network under the guard.
///
/// Only `WebfetchTool::resolving_through` stores one, and both are
/// `#[cfg(test)]`, so no shipped build can resolve anything but through the
/// system.
#[cfg(test)]
struct Double {
    /// Each name's answers, one list per lookup, handed out in order; the
    /// last list is handed out again once the others are gone.
    answers: Mutex<HashMap<String, std::collections::VecDeque<Vec<SocketAddr>>>>,
    /// The only sockets a guarded fetch counts as a public host.
    ///
    /// Loopback is all a hermetic test can listen on, and the guard refuses
    /// every loopback address, so a test names the exact listener that plays
    /// the public site. Every other loopback socket, another listener on
    /// another port included, is refused as the shipped guard refuses it.
    public: Vec<SocketAddr>,
    /// A proxy every request is sent through. Without one, none is, whatever
    /// the machine running the test has configured.
    proxy: Option<reqwest::Proxy>,
}

#[cfg(test)]
impl Double {
    fn lookup(&self, host: &str) -> std::io::Result<Vec<SocketAddr>> {
        let mut answers = self.answers.lock().expect("the double's answers are never poisoned");
        let Some(queue) = answers.get_mut(&host.to_ascii_lowercase()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{host} is not a name this test resolves"),
            ));
        };

        let answer = if queue.len() > 1 { queue.pop_front() } else { queue.front().cloned() };
        Ok(answer.unwrap_or_default())
    }
}

/// What the model is told when an address is refused.
///
/// The host and nothing else. A URL a model was handed can carry a token in
/// its path or its query — that is what the provider client's own redaction
/// exists for — and a refusal is not a reason to put one in a transcript.
fn refusal(host: &str) -> ToolError {
    ToolError::Failed(format!(
        "{host} resolves to an address on this machine, on a private network or in a reserved \
         range, which webfetch does not reach. Set webfetch.allow_private in the config to \
         allow it."
    ))
}

/// `url`'s host, without the brackets a URL writes an IPv6 literal in.
fn host_of(url: &reqwest::Url) -> Result<String, ToolError> {
    let host =
        url.host_str().ok_or_else(|| ToolError::Failed("the URL names no host".to_owned()))?;

    Ok(host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host).to_owned())
}

/// Gets the URL and renders the body in the format the call asked for.
///
/// `spill` is where a clamped page's whole text goes when a test named a
/// directory, and [`None`] in every shipped build (`WebfetchTool::spill_dir`).
async fn fetch(
    args: &Args,
    timeout: Duration,
    guard: Arc<Guard>,
    spill: Option<&Path>,
) -> Result<ToolOutput, ToolError> {
    let allow_private = guard.allow_private;
    let url = reqwest::Url::parse(&args.url)
        .map_err(|error| ToolError::InvalidArgs(format!("the URL is not a URL: {error}")))?;
    // Off the reactor: a name's lookup blocks, and this one happens before any
    // request is in flight, where a thread is affordable.
    {
        let guard = Arc::clone(&guard);
        let url = url.clone();
        tokio::task::spawn_blocking(move || guard.admit(&url)).await.map_err(|error| {
            ToolError::Failed(format!("the address check did not run: {error}"))
        })??;
    }

    let client = client(&guard)?;
    let request = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, accept(args.format))
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9");

    // One deadline over the whole exchange — connect, headers, body and
    // rendering — so neither a dribbling server nor a pathological page can
    // hold the call forever.
    tokio::time::timeout(timeout, async {
        // Every error reqwest renders names a URL — the one asked for, or the
        // hop it failed on — and a URL can carry a token in its query, as a
        // redirect an SSO server chose carries a `code=`. So an error the
        // tool did not raise itself reaches the model without it, as a
        // `refusal` does.
        let response = request
            .send()
            .await
            .map_err(|error| {
                ToolError::Failed(raised_inside(&error).unwrap_or_else(|| {
                    format!("the request did not complete: {}", error.without_url())
                }))
            })?
            .error_for_status()
            .map_err(|error| {
                ToolError::Failed(format!(
                    "the endpoint refused the request: {}",
                    error.without_url()
                ))
            })?;
        let private_allowed = if allow_private {
            Some(true)
        } else {
            guard.vouches_for(response.url(), response.remote_addr()).then_some(false)
        };

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = collect(response).await?;
        let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        let title = format!("{} ({content_type})", args.url);

        // The protocol carries no attachments yet, so an image is reported
        // rather than returned; upstream hands the bytes back as a data URL.
        if mime.starts_with("image/") {
            let mut metadata = stamped(private_allowed, None);
            metadata["mime"] = mime.as_str().into();
            metadata["bytes"] = body.len().into();

            return Ok(ToolOutput {
                title,
                output: format!(
                    "Image fetched successfully ({mime}, {} bytes). This tool cannot hand image \
                     bytes to the model yet.",
                    body.len()
                ),
                metadata,
            });
        }

        // Owned before entering the blocking task because that task may
        // outlive this async stack frame after cancellation or a timeout.
        let content = String::from_utf8_lossy(&body).into_owned();
        let html = mime.contains("text/html");
        let rendered = if html && matches!(args.format, Format::Markdown | Format::Text) {
            let format = args.format;
            tokio::task::spawn_blocking(move || match format {
                // Upstream converts HTML to markdown with turndown; this is
                // `htmd`, which preserves headings, links and lists where the
                // plain-text rendering deliberately keeps the stripper.
                Format::Markdown => to_markdown(&content),
                Format::Text => strip_tags(&content),
                Format::Html => unreachable!("HTML is not sent to the renderer"),
            })
            .await
            .map_err(|error| ToolError::Failed(format!("the page renderer did not run: {error}")))?
        } else {
            content
        };
        let clamped = match spill {
            Some(dir) => truncate::clamp_with(&rendered, dir),
            None => truncate::clamp(&rendered),
        };

        Ok::<_, ToolError>(ToolOutput {
            title,
            metadata: stamped(private_allowed, Some(&clamped)),
            output: clamped.text,
        })
    })
    .await
    .map_err(|_elapsed| ToolError::Failed("Request timed out".to_owned()))?
}

/// What every fetched result's metadata says about how it was fetched and
/// what the clamp did to it, whichever branch answered.
///
/// `private_allowed` is written on **every** result: `true` when the session
/// lifted the guard, `false` only when the address and port the page came
/// from are ones the guard checked ([`Guard::vouches_for`]), and `null`
/// otherwise — as when a proxy fetched it, so no answer the guard saw was the
/// one connected to, or when the address the page came from is unknown.
/// A reader deciding by it can therefore require an explicit `false` and treat
/// anything else as the other answer: a branch that forgot the stamp then
/// fails toward "this may have been a private page" rather than away from it,
/// and so does a proxied fetch. `truncated` is written on every result too, so
/// no reader has to decide what its absence means, and `hint_len` — the bytes
/// [`truncate::clamp`] appended after its notice
/// ([`truncate::Truncated::hint_len`]) — whenever it is true, so a reader
/// separates the page from the spill hint by count rather than by searching for
/// a sentence the page could have carried itself. Both are
/// [`truncate::Truncated::stamp`]'s; `clamped` is [`None`] for the one branch
/// that never clamps, an image.
fn stamped(
    private_allowed: Option<bool>,
    clamped: Option<&truncate::Truncated>,
) -> serde_json::Value {
    let mut metadata =
        serde_json::json!({ "private_allowed": private_allowed, "truncated": false });
    if let Some(clamped) = clamped {
        clamped.stamp(&mut metadata);
    }

    metadata
}

/// The client one fetch runs through, guarded unless the session lifted it.
///
/// Two things keep the guard from being advice:
///
/// - every name a connection is made to resolves through [`Resolver`], so a
///   direct connection uses only addresses the guard checked, on the first
///   hop and on every redirect alike. A configured proxy's own name resolves
///   there unchecked ([`Guard::connecting`]), and the proxy resolves the
///   target;
/// - every redirect is admitted by [`Guard::admit`] before it is followed,
///   because a hop is a URL somebody else chose. That is the only check an
///   address written into the `Location` header gets, and for a name it is
///   the check a hop sent through a proxy still gets.
///
/// That per-hop check resolves a name synchronously, inside the policy reqwest
/// calls on its own thread. A blocking call in an async program, and here on
/// purpose: the policy is not an async fn.
///
/// The policy and the resolver are installed either way, so the guarded path
/// and the lifted one follow redirects and resolve names through the same code
/// and the same hop cap.
fn client(guard: &Arc<Guard>) -> Result<reqwest::Client, ToolError> {
    let admitting = Arc::clone(guard);
    let redirects = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.stop();
        }

        match admitting.admit(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(refused) => attempt.error(refused),
        }
    });
    let builder =
        reqwest::Client::builder().redirect(redirects).dns_resolver(Resolver(Arc::clone(guard)));
    #[cfg(test)]
    let builder = match guard.double.as_ref() {
        Some(double) => match double.proxy.clone() {
            Some(proxy) => builder.proxy(proxy),
            None => builder.no_proxy(),
        },
        None => builder,
    };

    builder.build().map_err(|error| ToolError::Failed(format!("no HTTP client: {error}")))
}

/// The tool's own message, when the error reqwest reports carries one.
///
/// A redirect refused by [`Guard::admit`], and a lookup [`Resolver`] refused
/// or could not make, are raised inside the client, which renders them as its
/// own sentence naming the whole URL — the one thing [`refusal`] exists not to
/// put in a transcript. So the sentence the tool raised is handed back as it
/// was raised.
fn raised_inside(error: &reqwest::Error) -> Option<String> {
    std::iter::successors(std::error::Error::source(error), |cause| cause.source())
        .find_map(|cause| cause.downcast_ref::<ToolError>())
        .map(ToString::to_string)
}

/// Reads the body, refusing one too big to be worth holding.
///
/// The declared length is checked first so an oversized response costs nothing
/// to refuse, and the body is measured as it streams so one that lies about
/// its length — or declares none at all — is refused at the same boundary
/// rather than after it has been buffered whole.
async fn collect(response: reqwest::Response) -> Result<Vec<u8>, ToolError> {
    let too_large = || ToolError::Failed("Response too large (exceeds 5MB limit)".to_owned());

    if response.content_length().is_some_and(|length| length > MAX_RESPONSE_SIZE as u64) {
        return Err(too_large());
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| ToolError::Failed(format!("the response body stopped: {error}")))?;

        if body.len() + chunk.len() > MAX_RESPONSE_SIZE {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// What the request says it would like back, weighted towards `format`.
fn accept(format: Format) -> &'static str {
    match format {
        Format::Markdown => {
            "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
        }
        Format::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        Format::Html => {
            "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
        }
    }
}

/// `html` rendered as markdown.
///
/// The elements in [`SKIPPED`] are dropped whole, as they are by
/// [`strip_tags`]: the converter treats a `<script>` as an ordinary block and
/// would hand its source to the model as prose, which is the one thing both
/// renderings agree must not happen (deviation:
/// webfetch-markdown-skips-the-same-elements-the-stripper-does).
///
/// A conversion that fails falls back to the text rendering. Returning nothing
/// because a renderer gave up would be a worse answer than the page's words in
/// plain text, and the model asked for the page rather than for markdown. A
/// document nested past [`MAX_NESTING`] takes that same arm, and takes it for
/// the same reason: [`strip_tags`] reaches every word of the page through an
/// explicit worklist rather than the call stack, so it answers at any depth,
/// and refusing the call outright would cost the model the page over a
/// formatting choice it did not make. That arm parses a second time, which is
/// the price of leaving the ordinary path exactly as it was.
fn to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder().skip_tags(SKIPPED.to_vec()).build();
    // `convert` is these two halves called back to back, and splitting them is
    // what puts the depth check between the parse and the recursive walk. The
    // check reads the parsed tree rather than the bytes because a scan for
    // `<`/`</` disagrees with the parser exactly where it would matter: a
    // `</div>` inside an attribute value, or a `<div>` inside a `<script>`, is
    // text to html5ever and a tag to a scanner, so a counter over the source
    // is something the very document that overflows the walker can walk past.
    let tree = match converter.html_to_tree(html) {
        Ok(tree) => tree,
        Err(error) => {
            tracing::warn!(
                %error,
                "the page would not convert to markdown; handing over its text instead"
            );

            return strip_tags(html);
        }
    };
    let depth = nesting_depth(&tree);
    // Rendered while the tree is still owned here, so the walk and the drop
    // stay in the order the borrow demands.
    let rendered = (depth <= MAX_NESTING).then(|| converter.tree_to_markdown(&tree));
    drop_tree_iteratively(tree);

    rendered.unwrap_or_else(|| {
        tracing::warn!(
            depth,
            cap = MAX_NESTING,
            "the page nests deeper than the markdown converter may recurse over; handing over \
             its text instead"
        );

        strip_tags(html)
    })
}

/// The longest chain of nodes in `tree`, counted the way htmd's walker
/// descends it — every node and not only every element, since that walker
/// recurses into each child whatever kind of node it is.
///
/// Iterative for the reason [`MAX_NESTING`] exists at all: a recursive
/// measurement would overflow on exactly the documents worth measuring.
fn nesting_depth(tree: &Rc<Node>) -> usize {
    let mut deepest = 0;
    let mut work = vec![(Rc::clone(tree), 0_usize)];

    while let Some((node, depth)) = work.pop() {
        deepest = deepest.max(depth);
        work.extend(node.children.borrow().iter().map(|child| (Rc::clone(child), depth + 1)));
    }

    deepest
}

/// The text a reader would see in `html`.
///
/// html5ever's parse tree, reached through the same converter [`to_markdown`]
/// already runs on — but walked here rather than rendered. htmd's own
/// rendering cannot be used for a plain-text answer: it markdown-escapes every
/// text node on the way out, so a page saying `src/main_test.rs` would reach
/// the model as `src/main\_test.rs`, and no option it exposes turns that off.
/// Walking the tree the parser already built costs one traversal and keeps a
/// plain-text answer plain, while still getting the full entity table and the
/// tag grammar a hand scanner cannot have — an attribute holding a `>` no
/// longer ends its own tag and spills into the text.
///
/// The elements in [`SKIPPED`] take their contents with them, so a page's
/// scripts and stylesheets do not reach the model as if they were prose, and
/// those in [`BLOCK`] are separated so prose stays readable. Text itself is
/// verbatim: whatever escaping a plain-text reading did would be escaping for
/// a syntax it does not have.
fn strip_tags(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 2);

    match htmd::HtmlToMarkdown::new().html_to_tree(html) {
        Ok(tree) => {
            push_text(&tree, &mut text);
            drop_tree_iteratively(tree);
        }
        // The parser is handed the whole string at once and has nothing to
        // fail at; the `Result` is the sink API's shape rather than a case
        // that arises. Saying nothing still beats panicking inside a tool
        // call, or handing the model the page's markup as if it were prose.
        Err(error) => tracing::warn!(%error, "the page would not parse"),
    }

    text.trim().to_owned()
}

/// Appends the text under `node`, minus what [`SKIPPED`] drops.
fn push_text(node: &Rc<Node>, out: &mut String) {
    enum Work {
        Enter(Rc<Node>),
        EndBlock,
    }

    // Children go on the stack reversed, so popping visits them in order.
    fn enter_children(work: &mut Vec<Work>, node: &Rc<Node>) {
        work.extend(node.children.borrow().iter().rev().map(|child| Work::Enter(Rc::clone(child))));
    }

    let mut work = vec![Work::Enter(Rc::clone(node))];
    while let Some(item) = work.pop() {
        let Work::Enter(node) = item else {
            end_block(out);
            continue;
        };

        match &node.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            NodeData::Element { name, .. } => {
                let tag = &*name.local;
                if SKIPPED.contains(&tag) {
                    continue;
                }
                // A line break is one line break, not the blank line a block
                // earns; markup that lays out an address or a verse with `<br>`
                // would otherwise come back double-spaced.
                if tag == "br" {
                    out.push('\n');
                    continue;
                }

                if BLOCK.contains(&tag) {
                    end_block(out);
                    work.push(Work::EndBlock);
                }
                enter_children(&mut work, &node);
            }
            // A document, a doctype, a comment, a processing instruction:
            // nothing a reader sees, though a document's children are the page.
            _ => enter_children(&mut work, &node),
        }
    }
}

/// Detaches every owning tree edge before each node is dropped, so teardown
/// consumes heap worklist space rather than one call stack frame per level.
///
/// This cannot cover [`to_markdown`], whose converter builds and drops its tree
/// entirely inside `convert`, beyond this module's reach.
fn drop_tree_iteratively(root: Rc<Node>) {
    let mut work = vec![root];
    while let Some(node) = work.pop() {
        work.extend(std::mem::take(&mut *node.children.borrow_mut()));
        if let NodeData::Element { template_contents, .. } = &node.data
            && let Some(contents) = template_contents.borrow_mut().take()
        {
            work.push(contents);
        }
    }
}

/// Closes the line the text is on, so what follows starts a block of its own.
///
/// Whatever whitespace is already at the end goes with it: the newline and
/// indentation between two tags belong to the markup's layout, not to the
/// reader's.
fn end_block(out: &mut String) {
    while out.ends_with([' ', '\t', '\n', '\r']) {
        out.pop();
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
}

#[cfg(test)]
#[path = "webfetch_tests.rs"]
mod tests;
