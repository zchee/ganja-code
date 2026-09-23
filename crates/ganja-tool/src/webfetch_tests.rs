use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::{Double, MAX_RESPONSE_SIZE, WebfetchTool};
use crate::{Tool, ToolCtx, ToolError};

/// A loopback endpoint answering one connection with canned bytes.
///
/// Served over a real socket rather than through a mock, so the request
/// that is asserted on is the one the tool actually built and sent.
struct Endpoint {
    /// Where the tool should be pointed.
    url: String,
    /// The socket it listens on, which is what a resolver double answers
    /// with.
    address: SocketAddr,
    /// The request the endpoint was sent, once it has had one.
    seen: Arc<std::sync::Mutex<String>>,
    /// Kept so the server outlives the test talking to it.
    _server: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    fn seen(&self) -> String {
        self.seen.lock().expect("the request log is never poisoned").clone()
    }
}

/// Serves `response`, or nothing at all when it is [`None`], which is how
/// a server that accepts and then goes quiet is spelled.
async fn serve(response: Option<Vec<u8>>) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let address = listener.local_addr().expect("a bound socket has an address");
    let url = format!("http://{address}");
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let log = Arc::clone(&seen);

    let server = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };

        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while let Ok(read) = socket.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        *log.lock().expect("the request log is never poisoned") =
            String::from_utf8_lossy(&request).into_owned();

        let Some(response) = response else {
            // Held open and never answered, so the caller's own deadline is
            // the only thing that can end the exchange.
            tokio::time::sleep(Duration::from_secs(60)).await;
            return;
        };
        let _ = socket.write_all(&response).await;
        let _ = socket.flush().await;
    });

    Endpoint { url, address, seen, _server: server }
}

/// A 200 carrying `body` as `content_type`.
fn response(content_type: &str, body: &str) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: {content_type}\r\n\
             content-length: {}\r\n\r\n",
        body.len()
    );
    out.push_str(body);

    out.into_bytes()
}

/// A 302 pointing at `url`.
fn redirect_to(url: &str) -> Vec<u8> {
    redirect_saying(url, "")
}

/// A 302 pointing at `url`, carrying `body` as the page a client that does
/// not follow it would be shown.
fn redirect_saying(url: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 302 Found\r\nconnection: close\r\nlocation: {url}\r\n\
         content-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A resolver double: each name answers the lists given, one list per
/// lookup, and `public` names the listeners that stand in for public hosts.
///
/// A name's answers carry the listener's port, and a URL that names no port
/// connects to the port its answer carries — so one test can hold a public
/// stand-in and a private listener on the same loopback address and still
/// tell which of them a connection reached.
fn resolving(names: &[(&str, Vec<Vec<SocketAddr>>)], public: &[SocketAddr]) -> Double {
    Double {
        answers: Mutex::new(
            names
                .iter()
                .map(|(name, answers)| ((*name).to_owned(), answers.iter().cloned().collect()))
                .collect::<HashMap<_, _>>(),
        ),
        public: public.to_vec(),
        proxy: None,
    }
}

/// Asserts `error` is the refusal the tool gives for `host`, word for word.
fn assert_refused(error: &ToolError, host: &str) {
    let ToolError::Failed(message) = error else {
        panic!("{host} should be refused as a failure: {error:?}");
    };
    assert_eq!(
        message,
        &super::refusal(host).to_string(),
        "a refusal is the same sentence wherever it is raised"
    );
}

fn ctx() -> ToolCtx {
    ToolCtx::fixture(PathBuf::from("."))
}

const PAGE: &str = "<html><head><title>t</title><style>body{color:red}</style>\
                        <script>var x = 1 < 2;</script></head>\
                        <body><h1>Ganja </h1><p>ports &amp; tests</p></body></html>";

/// Every test below that actually fetches something fetches it over
/// loopback, and loopback is one of the addresses the shipped tool refuses
/// — so each of them says `allowing_private` out loud. That is not a
/// convenience: it is the guard being live in every one of them. The tests
/// that ask the tool what it *is*, rather than fetching anything, use
/// `new` because that is the tool a session gets.
#[tokio::test]
async fn a_local_private_or_reserved_address_is_refused_before_anything_is_opened() {
    // Nothing here is listening, and nothing needs to be: a refusal that
    // opened a socket first would not be this refusal.
    let refused = [
        ("http://127.0.0.1/", "loopback"),
        ("http://10.1.2.3/", "an RFC 1918 ten"),
        ("http://172.16.0.1/", "an RFC 1918 172"),
        ("http://192.168.1.1/", "an RFC 1918 192"),
        ("http://169.254.169.254/latest/meta-data/", "link-local"),
        ("http://0.0.0.0/", "the unspecified address"),
        ("http://0.1.2.3/", "this network, past its unspecified address"),
        ("http://100.64.0.1/", "the bottom of the shared address space"),
        ("http://100.127.255.255/", "the top of the shared address space"),
        ("http://192.0.0.1/", "an IETF protocol assignment"),
        ("http://198.18.0.1/", "the bottom of the benchmarking range"),
        ("http://198.19.255.255/", "the top of the benchmarking range"),
        ("http://224.0.0.1/", "v4 multicast"),
        ("http://240.0.0.1/", "a reserved v4"),
        ("http://255.255.255.255/", "the limited broadcast address"),
        ("http://[::1]/", "loopback, written as v6"),
        ("http://[::ffff:127.0.0.1]/", "loopback, wrapped in a v6"),
        ("http://[fd00::1]/", "a unique-local v6"),
        ("http://[fe80::1]/", "a link-local v6"),
        ("http://[fec0::1]/", "a site-local v6"),
        ("http://[ff02::1]/", "v6 multicast"),
        ("http://[64:ff9b::a00:1]/", "a ten, through NAT64"),
        ("http://[64:ff9b::6464:6401]/", "the shared address space, through NAT64"),
        ("http://[2002:a00:1::]/", "a ten, through 6to4"),
        ("http://[2002:6464:6401::]/", "the shared address space, through 6to4"),
        ("http://[::a00:1]/", "a ten, written IPv4-compatible"),
        ("http://[::ffff:0:a00:1]/", "a ten, in SIIT's translated form"),
        ("http://[64:ff9b:1:a00:1::]/", "a ten, behind the local-use NAT64 prefix"),
        ("http://[2001:2::1]/", "v6 benchmarking"),
    ];

    for (url, what) in refused {
        let error = WebfetchTool::new()
            .run(serde_json::json!({ "url": url }), &ctx())
            .await
            .expect_err(&format!("{what} is refused: {url}"));

        let ToolError::Failed(message) = &error else {
            panic!("{what} should be refused as a failure: {error:?}");
        };
        assert!(
            message.contains("on this machine, on a private network or in a reserved range"),
            "{what} should say why: {message}"
        );
        assert!(
            !message.contains("latest/meta-data"),
            "a refusal names the host and not the whole URL, which can carry a \
                 credential in its path or query: {message}"
        );
    }
}

/// The same URL, refused and then fetched — because the only thing that
/// changed is the session saying it wanted this.
#[tokio::test]
async fn the_same_private_url_is_fetched_once_the_session_allows_it() {
    let endpoint = serve(Some(response("text/plain", "an intranet page"))).await;

    let refused = WebfetchTool::new()
        .run(serde_json::json!({ "url": endpoint.url }), &ctx())
        .await
        .expect_err("loopback is refused by the tool as it ships");
    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("private network")),
        "got {refused:?}"
    );
    assert!(endpoint.seen().is_empty(), "and refused without connecting: {}", endpoint.seen());

    let out = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url }), &ctx())
        .await
        .expect("the endpoint answers a session that asked for this");
    assert_eq!(out.output, "an intranet page");
}

/// A public address is what the tool is *for*, and the check has to let it
/// through — including the addresses that sit just outside each blocked
/// range, which is where an off-by-one in a mask would show.
///
/// Written against literals so nothing here consults a resolver or opens a
/// connection.
#[test]
fn a_public_address_is_not_what_the_guard_refuses() {
    let allowed = [
        ("http://8.8.8.8/", "an ordinary public v4"),
        ("http://9.255.255.255/", "just below the ten"),
        ("http://11.0.0.0/", "just above the ten"),
        ("http://172.15.255.255/", "just below the 172 range"),
        ("http://172.32.0.1/", "just above the 172 range"),
        ("http://192.167.255.255/", "just below the 192 range"),
        ("http://192.169.0.1/", "just above the 192 range"),
        ("http://169.253.0.1/", "just below link-local"),
        ("http://100.63.255.255/", "just below the shared address space"),
        ("http://100.128.0.0/", "just above the shared address space"),
        ("http://192.0.1.1/", "just above the protocol assignments"),
        ("http://198.17.255.255/", "just below the benchmarking range"),
        ("http://198.20.0.0/", "just above the benchmarking range"),
        ("http://[2001:4860:4860::8888]/", "a public v6"),
        ("http://[2003:a00:1::]/", "a public v6 just past 6to4, whose next bits read as a ten"),
        ("http://[fe00::1]/", "just below the unique-local prefix"),
        ("http://[64:ff9b::0808:0808]/", "a public v4, through NAT64"),
        ("http://[2002:808:808::]/", "a public v4, through 6to4"),
        ("http://[2001:2:1::1]/", "just past v6 benchmarking"),
    ];

    for (url, what) in allowed {
        let parsed = reqwest::Url::parse(url).expect("the fixture is a URL");
        WebfetchTool::new()
            .guard()
            .admit(&parsed)
            .unwrap_or_else(|error| panic!("{what} should be allowed: {url}: {error:?}"));
    }
}

/// A redirect is a URL somebody else chose, so the policy runs the same
/// check on it that the first hop got.
///
/// This pins the check the policy applies to an address written into the
/// `Location` header, which no resolver is ever asked about. A redirect to a
/// *name*, with the guard on, is driven end to end through a resolver double
/// further down.
#[test]
fn a_redirect_target_on_a_private_network_is_refused_by_the_check_the_policy_applies() {
    let hop =
        reqwest::Url::parse("http://169.254.169.254/latest/meta-data/").expect("a URL parses");
    let refused = WebfetchTool::new().guard().admit(&hop).expect_err("a hop into link-local");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("private network")),
        "got {refused:?}"
    );
}

/// And the policy that does the refusing still follows the redirects it
/// should — the whole web answers `http` with a 301 to `https`, and a guard
/// that broke that would be worse than no guard.
#[tokio::test]
async fn a_redirect_is_followed_to_the_page_it_names() {
    let target = serve(Some(response("text/plain", "arrived"))).await;
    let hop = serve(Some(redirect_to(&target.url))).await;

    let out = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": hop.url }), &ctx())
        .await
        .expect("the hop and its target both answer");

    assert_eq!(out.output, "arrived");
    assert!(
        target.seen().starts_with("GET /"),
        "the second endpoint is the one that served the body: {}",
        target.seen()
    );
}

/// A name that answers a public address when its hop is checked and a
/// private one when the connection is made is refused at the second answer:
/// the connection's own lookup is checked, not trusted to agree with the
/// first.
#[tokio::test]
async fn a_name_answering_privately_by_the_time_its_connection_is_made_is_refused() {
    let public = serve(Some(response("text/plain", "the public page"))).await;
    let private = serve(Some(response("text/plain", "the private page"))).await;
    let tool = WebfetchTool::new().resolving_through(resolving(
        &[("rebind.test", vec![vec![public.address], vec![private.address]])],
        &[public.address],
    ));

    let refused = tool
        .run(serde_json::json!({ "url": "http://rebind.test/" }), &ctx())
        .await
        .expect_err("the answer the connection would use is private");

    assert_refused(&refused, "rebind.test");
    assert!(private.seen().is_empty(), "the private address was never reached: {}", private.seen());
    assert!(
        public.seen().is_empty(),
        "nor the public one, which only the check was answered with: {}",
        public.seen()
    );
}

/// A redirect to a name that resolves into a private range is refused before
/// it is followed, and what the first hop said is not handed back.
#[tokio::test]
async fn a_redirect_to_a_name_resolving_privately_is_refused_and_the_first_hop_is_not_returned() {
    let private = serve(Some(response("text/plain", "the private page"))).await;
    let first = serve(Some(redirect_saying("http://inside.test/", "moved along"))).await;
    let tool = WebfetchTool::new().resolving_through(resolving(
        &[("first.test", vec![vec![first.address]]), ("inside.test", vec![vec![private.address]])],
        &[first.address],
    ));

    let refused = tool
        .run(serde_json::json!({ "url": "http://first.test/" }), &ctx())
        .await
        .expect_err("the redirect leads into a private range");

    assert_refused(&refused, "inside.test");
    assert!(
        !refused.to_string().contains("moved along"),
        "the first hop's page is not what comes back: {refused}"
    );
    assert!(first.seen().starts_with("GET / "), "the first hop was fetched: {}", first.seen());
    assert!(private.seen().is_empty(), "the private address was never reached: {}", private.seen());
}

/// The same after a redirect: a hop's name that answers publicly when the
/// hop is admitted and privately when its connection is made is refused at
/// the connection, so no hop connects through a lookup the guard never saw.
#[tokio::test]
async fn a_redirect_to_a_name_answering_privately_by_the_time_its_connection_is_made_is_refused() {
    let public = serve(Some(response("text/plain", "the public page"))).await;
    let private = serve(Some(response("text/plain", "the private page"))).await;
    let first = serve(Some(redirect_saying("http://rebind.test/", "moved along"))).await;
    let tool = WebfetchTool::new().resolving_through(resolving(
        &[
            ("first.test", vec![vec![first.address]]),
            ("rebind.test", vec![vec![public.address], vec![private.address]]),
        ],
        &[first.address, public.address],
    ));

    let refused = tool
        .run(serde_json::json!({ "url": "http://first.test/" }), &ctx())
        .await
        .expect_err("the answer the redirect's connection would use is private");

    assert_refused(&refused, "rebind.test");
    assert!(first.seen().starts_with("GET / "), "the first hop was fetched: {}", first.seen());
    assert!(private.seen().is_empty(), "the private address was never reached: {}", private.seen());
    assert!(public.seen().is_empty(), "nor the check's public answer: {}", public.seen());
}

/// The connection's check applies only to a name the hop was admitted under,
/// so however a `Location` spells its name, the connection must look it up
/// under that same name. A spelling that reached the connection any other way
/// would resolve unchecked, and the private listener would answer.
#[tokio::test]
async fn every_spelling_of_a_redirect_s_name_is_checked_at_its_connection() {
    // The `Location` sent, and the name the URL parser makes of it.
    let spellings = [
        ("http://REBIND.Test/", "rebind.test"),
        ("HTTP://REBIND.TEST/", "rebind.test"),
        ("http://rebind.test./", "rebind.test."),
        ("//rebind.test/", "rebind.test"),
        ("http://user:pw@rebind.test/", "rebind.test"),
        ("http://reb%69nd.test/", "rebind.test"),
        ("http://B\u{dc}CHER.test/", "xn--bcher-kva.test"),
        ("http://xn--BCHER-kva.test/", "xn--bcher-kva.test"),
        // The private listener's own port, so a connection that skipped the
        // check would reach it whichever answer it used.
        ("http://rebind.test:PRIVATE_PORT/", "rebind.test"),
    ];

    for (location, name) in spellings {
        let public = serve(Some(response("text/plain", "the public page"))).await;
        let private = serve(Some(response("text/plain", "the private page"))).await;
        let location = location.replace("PRIVATE_PORT", &private.address.port().to_string());
        let first = serve(Some(redirect_saying(&location, "moved along"))).await;
        let tool = WebfetchTool::new().resolving_through(resolving(
            &[
                ("first.test", vec![vec![first.address]]),
                (name, vec![vec![public.address], vec![private.address]]),
            ],
            &[first.address, public.address],
        ));

        let refused = tool
            .run(serde_json::json!({ "url": "http://first.test/" }), &ctx())
            .await
            .expect_err(&format!("{location}: the answer its connection would use is private"));

        let ToolError::Failed(message) = &refused else {
            panic!("{location}: refused as a failure: {refused:?}");
        };
        assert_eq!(
            message,
            &super::refusal(name).to_string(),
            "{location}: refused at the connection's lookup, naming the host"
        );
        assert!(private.seen().is_empty(), "{location}: private reached: {}", private.seen());
        assert!(public.seen().is_empty(), "{location}: public reached: {}", public.seen());
    }
}

/// A connection whose own lookup fails, or finds no address, is reported by
/// host, as a hop's check reports it, rather than in reqwest's sentence, which
/// names the whole URL and whatever its query carries.
#[tokio::test]
async fn a_failed_or_empty_connection_lookup_is_reported_as_the_host_not_resolving() {
    let public = serve(Some(response("text/plain", "the public page"))).await;
    // The hop's check gets the public answer, and the connection then gets
    // none.
    let empty = WebfetchTool::new().resolving_through(resolving(
        &[("gone.test", vec![vec![public.address], vec![]])],
        &[public.address],
    ));
    // With the guard lifted, the connection's lookup is the only one, and the
    // double knows no such name.
    let failed = WebfetchTool::allowing_private().resolving_through(resolving(&[], &[]));

    for (tool, what) in [(empty, "finds no address"), (failed, "fails")] {
        let error = tool
            .run(serde_json::json!({ "url": "http://gone.test/page?token=abc" }), &ctx())
            .await
            .expect_err(&format!("a connection whose lookup {what} fails the fetch"));

        let ToolError::Failed(message) = &error else {
            panic!("a lookup that {what} is a failure: {error:?}");
        };
        assert_eq!(message, "gone.test did not resolve", "a lookup that {what}");
    }
    assert!(public.seen().is_empty(), "nothing was connected to: {}", public.seen());
}

/// A redirect to a name that resolves publicly is followed as it always was,
/// and the page is stamped as one the guard checked.
#[tokio::test]
async fn a_redirect_to_a_name_resolving_publicly_is_followed_and_stamped_as_checked() {
    let target = serve(Some(response("text/plain", "arrived"))).await;
    let first = serve(Some(redirect_to("http://second.test/"))).await;
    let tool = WebfetchTool::new().resolving_through(resolving(
        &[("first.test", vec![vec![first.address]]), ("second.test", vec![vec![target.address]])],
        &[first.address, target.address],
    ));

    let out = tool
        .run(serde_json::json!({ "url": "http://first.test/" }), &ctx())
        .await
        .expect("both hops resolve publicly");

    assert_eq!(out.output, "arrived");
    assert_eq!(out.metadata, serde_json::json!({ "private_allowed": false, "truncated": false }));
    assert!(target.seen().starts_with("GET / "), "the second hop served it: {}", target.seen());
}

/// With the guard lifted every one of those names is fetched: a name that
/// resolves privately, a redirect to one, and a name that changes its answer.
/// Nothing is checked, so nothing is looked up twice — the one lookup is the
/// connection's, and it gets the name's first answer.
#[tokio::test]
async fn once_the_session_allows_private_addresses_every_such_name_is_fetched() {
    let private = serve(Some(response("text/plain", "a private page"))).await;
    let redirected = serve(Some(response("text/plain", "a private page, redirected to"))).await;
    let first = serve(Some(redirect_to("http://inside.test/"))).await;
    let public = serve(Some(response("text/plain", "the first answer"))).await;
    let rebound = serve(Some(response("text/plain", "the second answer"))).await;
    let tool = WebfetchTool::allowing_private().resolving_through(resolving(
        &[
            ("private.test", vec![vec![private.address]]),
            ("first.test", vec![vec![first.address]]),
            ("inside.test", vec![vec![redirected.address]]),
            ("rebind.test", vec![vec![public.address], vec![rebound.address]]),
        ],
        &[],
    ));

    for (url, page) in [
        ("http://private.test/", "a private page"),
        ("http://first.test/", "a private page, redirected to"),
        ("http://rebind.test/", "the first answer"),
    ] {
        let out = tool
            .run(serde_json::json!({ "url": url }), &ctx())
            .await
            .unwrap_or_else(|error| panic!("{url} is fetched once allowed: {error:?}"));

        assert_eq!(out.output, page, "{url}");
        assert_eq!(out.metadata["private_allowed"], true, "{url}: {}", out.metadata);
    }
    assert!(rebound.seen().is_empty(), "the second answer was never asked for: {}", rebound.seen());
}

/// A guarded page is stamped `false` only when it came from an address the
/// guard checked. Sent through a proxy, the proxy resolved the target and the
/// guard saw none of its answers, so the stamp is `null` — which a reader
/// requiring an explicit `false` reads as "may have been a private page".
#[tokio::test]
async fn a_page_is_stamped_as_checked_only_when_it_came_from_an_address_the_guard_checked() {
    let site = serve(Some(response("text/plain", "straight from the site"))).await;
    let direct = WebfetchTool::new()
        .resolving_through(resolving(&[("site.test", vec![vec![site.address]])], &[site.address]))
        .run(serde_json::json!({ "url": "http://site.test/" }), &ctx())
        .await
        .expect("the site answers");
    assert_eq!(direct.output, "straight from the site");
    assert_eq!(
        direct.metadata,
        serde_json::json!({ "private_allowed": false, "truncated": false }),
        "the connection went to the answer the guard checked"
    );

    let site = serve(Some(response("text/plain", "straight from the site"))).await;
    let proxy = serve(Some(response("text/plain", "through the proxy"))).await;
    let mut double = resolving(&[("site.test", vec![vec![site.address]])], &[site.address]);
    double.proxy = Some(reqwest::Proxy::http(&proxy.url).expect("the proxy's URL is a URL"));
    let proxied = WebfetchTool::new()
        .resolving_through(double)
        .run(serde_json::json!({ "url": "http://site.test/" }), &ctx())
        .await
        .expect("the proxy answers");

    assert_eq!(proxied.output, "through the proxy");
    assert_eq!(
        proxied.metadata,
        serde_json::json!({ "private_allowed": null, "truncated": false }),
        "the proxy resolved the target, so the guard did not decide where the page came from"
    );
    assert!(
        proxy.seen().starts_with("GET http://site.test/ "),
        "the request went to the proxy, naming the target: {}",
        proxy.seen()
    );
    assert!(site.seen().is_empty(), "and never to the site itself: {}", site.seen());
}

/// The guard polices where a fetch goes, not how it gets there. A proxy is
/// the person's own configuration, and one on this machine is where a proxy
/// usually is, so its name is resolved without the guard — while the target
/// it is asked for is still admitted first.
#[tokio::test]
async fn a_proxy_whose_name_resolves_to_this_machine_is_still_used() {
    let site = serve(Some(response("text/plain", "straight from the site"))).await;
    let proxy = serve(Some(response("text/plain", "through the proxy"))).await;
    let mut double = resolving(
        &[("site.test", vec![vec![site.address]]), ("proxy.test", vec![vec![proxy.address]])],
        &[site.address],
    );
    double.proxy = Some(
        reqwest::Proxy::http(format!("http://proxy.test:{}", proxy.address.port()))
            .expect("the proxy's URL is a URL"),
    );

    let out = WebfetchTool::new()
        .resolving_through(double)
        .run(serde_json::json!({ "url": "http://site.test/" }), &ctx())
        .await
        .expect("a proxy on loopback is still reached");

    assert_eq!(out.output, "through the proxy");
    assert_eq!(out.metadata["private_allowed"], serde_json::Value::Null, "{}", out.metadata);
    assert!(site.seen().is_empty(), "the site itself was never reached: {}", site.seen());
}

/// A proxy that carries only some hops — `HTTP_PROXY` without `HTTPS_PROXY`,
/// or an exception list — can share an address with an answer that a direct
/// hop to the same name connected to. The page it served is still not stamped
/// as checked: it came from another port than a direct connection to that
/// answer uses.
#[tokio::test]
async fn a_page_a_proxy_served_after_a_direct_hop_to_the_same_name_is_not_stamped_as_checked() {
    let proxy = serve(Some(response("text/plain", "through the proxy"))).await;
    let site = serve(Some(redirect_saying("http://site.test:9/", "moved along"))).await;
    let mut double = resolving(&[("site.test", vec![vec![site.address]])], &[site.address]);
    let proxy_url = proxy.url.clone();
    // Only the second hop, the one naming port 9, is sent through the proxy,
    // which listens on the site's own loopback address.
    double.proxy =
        Some(reqwest::Proxy::custom(move |url| (url.port() == Some(9)).then(|| proxy_url.clone())));

    let out = WebfetchTool::new()
        .resolving_through(double)
        .run(serde_json::json!({ "url": "http://site.test/" }), &ctx())
        .await
        .expect("the proxy answers the second hop");

    assert_eq!(out.output, "through the proxy");
    assert!(site.seen().starts_with("GET / "), "the first hop went direct: {}", site.seen());
    assert!(
        proxy.seen().starts_with("GET http://site.test:9/ "),
        "the second went to the proxy, naming the target: {}",
        proxy.seen()
    );
    assert_eq!(
        out.metadata,
        serde_json::json!({ "private_allowed": null, "truncated": false }),
        "the proxy resolved the second hop's target, whatever address it shares with the site"
    );
}

/// The same with a proxy that is named rather than written as an address, and
/// that listens on the very port the proxied hop names, so its socket matches
/// an answer a direct hop connected to. Its name was looked up on the way,
/// and that lookup is what marks the fetch as proxied.
#[tokio::test]
async fn a_page_a_named_proxy_served_on_the_hop_s_own_port_is_not_stamped_as_checked() {
    let proxy = serve(Some(response("text/plain", "through the proxy"))).await;
    let port = proxy.address.port();
    let site =
        serve(Some(redirect_saying(&format!("http://site.test:{port}/"), "moved along"))).await;
    let mut double = resolving(
        &[("site.test", vec![vec![site.address]]), ("proxy.test", vec![vec![proxy.address]])],
        &[site.address],
    );
    let proxy_url = format!("http://proxy.test:{port}");
    double.proxy = Some(reqwest::Proxy::custom(move |url| {
        (url.port() == Some(port)).then(|| proxy_url.clone())
    }));

    let out = WebfetchTool::new()
        .resolving_through(double)
        .run(serde_json::json!({ "url": "http://site.test/" }), &ctx())
        .await
        .expect("the proxy answers the second hop");

    assert_eq!(out.output, "through the proxy");
    assert!(site.seen().starts_with("GET / "), "the first hop went direct: {}", site.seen());
    assert!(
        proxy.seen().starts_with(&format!("GET http://site.test:{port}/ ")),
        "the second went to the proxy, naming the target: {}",
        proxy.seen()
    );
    assert_eq!(
        out.metadata,
        serde_json::json!({ "private_allowed": null, "truncated": false }),
        "a fetch that looked up a proxy's name is not stamped as checked"
    );
}

/// A hop sent through a proxy is still admitted first, and that is the only
/// check it gets here: a name resolving into a private range on this machine
/// is refused before the proxy is asked for it.
#[tokio::test]
async fn a_proxied_fetch_of_a_name_resolving_privately_here_is_refused_before_the_proxy_is_asked() {
    let private = serve(Some(response("text/plain", "the private page"))).await;
    let proxy = serve(Some(response("text/plain", "through the proxy"))).await;
    let mut double = resolving(&[("inside.test", vec![vec![private.address]])], &[]);
    double.proxy = Some(reqwest::Proxy::http(&proxy.url).expect("the proxy's URL is a URL"));

    let refused = WebfetchTool::new()
        .resolving_through(double)
        .run(serde_json::json!({ "url": "http://inside.test/" }), &ctx())
        .await
        .expect_err("the name resolves into a private range here");

    assert_refused(&refused, "inside.test");
    assert!(proxy.seen().is_empty(), "the proxy was never asked: {}", proxy.seen());
    assert!(private.seen().is_empty(), "nor the private address reached: {}", private.seen());
}

#[tokio::test]
async fn an_html_page_asked_for_as_text_comes_back_without_its_markup() {
    let endpoint = serve(Some(response("text/html; charset=utf-8", PAGE))).await;

    let out = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url, "format": "text" }), &ctx())
        .await
        .expect("the endpoint answers");

    // The title, the heading and the paragraph are three blocks, so they
    // are three blocks here; the stripper this replaced ran them into one
    // line because it had no idea which tags were which.
    assert_eq!(out.output, "t\n\nGanja\n\nports & tests");
    assert!(
        !out.output.contains("color:red") && !out.output.contains("var x"),
        "a stylesheet and a script are not prose: {:?}",
        out.output
    );
    assert!(out.title.contains("text/html"), "the title names what was served: {}", out.title);
}

#[tokio::test]
async fn the_request_says_who_it_is_and_what_it_would_like_back() {
    let endpoint = serve(Some(response("text/plain", "hi"))).await;

    WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url }), &ctx())
        .await
        .expect("the endpoint answers");

    let seen = endpoint.seen().to_lowercase();
    assert!(
        seen.contains("user-agent: mozilla/5.0"),
        "a bare agent gets a challenge page instead of the content: {seen}"
    );
    assert!(
        seen.contains("text/markdown;q=1.0"),
        "markdown is the default, and the request should say so: {seen}"
    );
}

/// A page with the constructs a stripper cannot represent: a heading is a
/// line of text to it, and a link's target is not text at all.
const STRUCTURED: &str = "<html><body><h1>Ganja</h1><p>See \
                              <a href=\"https://example.com/docs\">the docs</a>.</p>\
                              <ul><li>one</li><li>two</li></ul>\
                              <style>body{color:red}</style>\
                              <script>var x = 1 < 2;</script></body></html>";

/// **R17.** The markdown format is a markdown rendering, which is what
/// upstream's turndown call produces and what the tag stripper standing in
/// for it could not.
#[tokio::test]
async fn an_html_page_asked_for_as_markdown_comes_back_as_markdown() {
    let endpoint = serve(Some(response("text/html; charset=utf-8", STRUCTURED))).await;

    let out = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url, "format": "markdown" }), &ctx())
        .await
        .expect("the endpoint answers");

    assert!(out.output.contains("# Ganja"), "a heading should be a heading: {:?}", out.output);
    assert!(
        out.output.contains("[the docs](https://example.com/docs)"),
        "a link should keep the target a reader would follow: {:?}",
        out.output
    );
    // Asserted by shape rather than by exact marker and spacing, which are
    // the converter's style options and not the claim being made.
    let bulleted: Vec<&str> =
        out.output.lines().filter(|line| line.trim_start().starts_with(['-', '*', '+'])).collect();
    assert!(
        bulleted.len() == 2 && bulleted[0].contains("one") && bulleted[1].contains("two"),
        "a list should be a list: {:?}",
        out.output
    );

    // The other half of the claim: none of the above is something the
    // stripper this replaced could ever have emitted, so the assertions
    // are about the conversion and not about the page.
    let stripped = super::strip_tags(STRUCTURED);
    assert!(
        !stripped.contains("# Ganja")
            && !stripped.contains("example.com/docs")
            && !stripped.lines().any(|line| line.trim_start().starts_with(['-', '*', '+'])),
        "the stripper has no markdown to lose: {stripped:?}"
    );
}

#[tokio::test]
async fn neither_rendering_hands_the_model_a_script_or_a_stylesheet() {
    for format in ["markdown", "text"] {
        let endpoint = serve(Some(response("text/html", STRUCTURED))).await;

        let out = WebfetchTool::allowing_private()
            .run(serde_json::json!({ "url": endpoint.url, "format": format }), &ctx())
            .await
            .expect("the endpoint answers");

        assert!(
            !out.output.contains("color:red") && !out.output.contains("var x"),
            "{format} handed over machinery as prose: {:?}",
            out.output
        );
    }
}

#[tokio::test]
async fn a_body_that_is_not_html_is_handed_over_as_it_arrived() {
    let endpoint = serve(Some(response("text/plain", "plain <b>text</b>"))).await;

    let out = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url, "format": "markdown" }), &ctx())
        .await
        .expect("the endpoint answers");

    assert_eq!(out.output, "plain <b>text</b>");
}

#[tokio::test]
async fn html_asked_for_as_html_keeps_its_markup() {
    let endpoint = serve(Some(response("text/html", PAGE))).await;

    let out = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url, "format": "html" }), &ctx())
        .await
        .expect("the endpoint answers");

    assert_eq!(out.output, PAGE);
}

/// **D567.** A page cut to fit says so in its metadata, and says how many
/// trailing bytes of the output are the spill hint rather than the page — so
/// a reader drops exactly those and never searches for a sentence the page
/// could have carried itself. The notice stays in what is left (review N11):
/// it is the preview's, and a reader that dropped the hint must still see
/// the page was cut.
#[tokio::test]
async fn a_clamped_page_reports_the_bytes_its_hint_appended_and_keeps_its_notice() {
    let spill = tempfile::tempdir().expect("a scratch directory");
    let page = "line\n".repeat(crate::truncate::MAX_LINES + 10);
    let endpoint = serve(Some(response("text/plain", &page))).await;

    let out = WebfetchTool::allowing_private()
        .spilling_into(spill.path())
        .run(serde_json::json!({ "url": endpoint.url }), &ctx())
        .await
        .expect("the endpoint answers");

    let spilled = std::fs::read_dir(spill.path())
        .expect("the spill directory was created")
        .map(|entry| entry.expect("a readable directory entry").path())
        .collect::<Vec<_>>();
    let [file] = spilled.as_slice() else {
        panic!("exactly one spill file: {spilled:?}");
    };
    let appended = format!("\n\n{}", crate::truncate::hint(file));

    assert_eq!(out.metadata["truncated"], true, "{}", out.metadata);
    assert_eq!(out.metadata["private_allowed"], true, "{}", out.metadata);
    assert_eq!(out.metadata["hint_len"], appended.len(), "{}", out.metadata);
    let hint_len = usize::try_from(out.metadata["hint_len"].as_u64().expect("a count"))
        .expect("a count that fits");
    let (kept, tail) = out.output.split_at(out.output.len() - hint_len);
    assert_eq!(tail, appended, "the counted tail is byte for byte what the clamp appended");
    assert!(
        kept.ends_with("\n\n...11 lines truncated..."),
        "the notice is left with the page, outside the count: {:?}",
        &kept[kept.len().saturating_sub(64)..]
    );
    assert_eq!(std::fs::read_to_string(file).expect("the spill is readable"), page);
}

/// **D567.** A page cut with nowhere to spill is still reported as cut, and
/// its `hint_len` is zero: nothing was appended past the notice, so a reader
/// dropping `hint_len` bytes drops nothing and keeps the notice.
#[tokio::test]
async fn a_page_clamped_with_nowhere_to_spill_reports_that_nothing_was_appended() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    // A regular file where the spill directory would have to be created, so
    // no spill file can be written: the degraded path, reached hermetically.
    let blocked = scratch.path().join("blocked");
    std::fs::write(&blocked, "not a directory").expect("the fixture writes");
    let page = "line\n".repeat(crate::truncate::MAX_LINES + 10);
    let endpoint = serve(Some(response("text/plain", &page))).await;

    let out = WebfetchTool::allowing_private()
        .spilling_into(&blocked)
        .run(serde_json::json!({ "url": endpoint.url }), &ctx())
        .await
        .expect("the endpoint answers");

    assert_eq!(
        out.metadata,
        serde_json::json!({ "private_allowed": true, "truncated": true, "hint_len": 0 })
    );
    assert!(
        out.output.ends_with("\n\n...11 lines truncated..."),
        "the notice is the last thing in the output: {:?}",
        &out.output[out.output.len().saturating_sub(64)..]
    );
    assert!(!out.output.contains("Full output saved to:"), "no file is claimed");
}

/// **D567.** Every result says whether private addresses were allowed and
/// whether it was cut, on both branches a fetch can answer through. A reader
/// may then require an explicit `private_allowed: false` and treat a missing
/// key as `true`, so a branch that lost its stamp fails toward caution.
///
/// Loopback is the only address a hermetic test may fetch, and the guarded
/// tool refuses it, so the `false` is asserted here on the one function both
/// branches stamp through, fed the flag the tool holds. A guarded fetch
/// writing it end to end goes through a resolver double further down.
#[tokio::test]
async fn every_fetched_result_says_whether_private_addresses_were_allowed_and_whether_it_was_cut() {
    let endpoint = serve(Some(response("text/plain", "an intranet page"))).await;
    let page = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url }), &ctx())
        .await
        .expect("the endpoint answers");
    assert_eq!(
        page.metadata,
        serde_json::json!({ "private_allowed": true, "truncated": false }),
        "a page that fit carries no hint_len"
    );

    let endpoint = serve(Some(response("image/png", "png bytes"))).await;
    let image = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url }), &ctx())
        .await
        .expect("the endpoint answers");
    assert_eq!(
        image.metadata,
        serde_json::json!({
            "mime": "image/png",
            "bytes": "png bytes".len(),
            "private_allowed": true,
            "truncated": false,
        }),
        "the image branch is stamped too"
    );

    let guarded = WebfetchTool::new();
    assert_eq!(
        super::stamped(Some(guarded.allow_private), None),
        serde_json::json!({ "private_allowed": false, "truncated": false }),
        "the tool as it ships stamps an explicit false"
    );
    assert_eq!(
        super::stamped(
            Some(guarded.allow_private),
            Some(&crate::truncate::Truncated {
                text: String::new(),
                truncated: true,
                hint_len: 213,
            })
        ),
        serde_json::json!({ "private_allowed": false, "truncated": true, "hint_len": 213 }),
    );
}

#[tokio::test]
async fn a_response_over_the_size_cap_is_refused() {
    // Declares a length nobody would want to buffer, so the refusal lands
    // before the body is read at all.
    let oversized = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n",
        MAX_RESPONSE_SIZE + 1
    );
    let endpoint = serve(Some(oversized.into_bytes())).await;

    let refused = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url }), &ctx())
        .await
        .expect_err("5MB is the limit");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("exceeds 5MB limit")),
        "got {refused:?}"
    );
}

#[tokio::test]
async fn an_endpoint_that_never_answers_ends_at_the_timeout() {
    let endpoint = serve(None).await;

    let started = std::time::Instant::now();
    let refused = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url, "timeout": 1 }), &ctx())
        .await
        .expect_err("nothing ever came back");
    let elapsed = started.elapsed();

    assert!(
        matches!(&refused, ToolError::Failed(message) if message == "Request timed out"),
        "got {refused:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5),
        "the deadline should be the thing that ended it, took {elapsed:?}"
    );
}

#[tokio::test]
async fn a_cancel_ends_a_fetch_that_is_still_waiting() {
    let endpoint = serve(None).await;
    let context = ctx();
    let cancel = context.cancel.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let refused = WebfetchTool::allowing_private()
        .run(serde_json::json!({ "url": endpoint.url, "timeout": 120 }), &context)
        .await
        .expect_err("the turn ended before the page did");

    assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
}

#[tokio::test]
async fn a_scheme_that_is_not_http_is_refused_before_anything_is_opened() {
    for url in
        ["file:///etc/passwd", "data:text/html,<b>hi</b>", "ftp://example.com/x", "example.com"]
    {
        let refused = match WebfetchTool::allowing_private()
            .run(serde_json::json!({ "url": url }), &ctx())
            .await
        {
            Err(refused) => refused,
            Ok(out) => panic!("{url} was fetched rather than refused: {out:?}"),
        };

        assert!(
            matches!(&refused, ToolError::InvalidArgs(message) if message.contains("http://")),
            "{url} got {refused:?}"
        );
    }
}

#[tokio::test]
async fn a_call_without_a_url_is_refused() {
    let refused = WebfetchTool::allowing_private()
        .run(serde_json::json!({}), &ctx())
        .await
        .expect_err("there is nothing to fetch");

    assert!(matches!(refused, ToolError::InvalidArgs(_)), "got {refused:?}");
}

#[test]
fn the_one_line_description_names_the_url() {
    assert_eq!(
        WebfetchTool::new().describe(&serde_json::json!({ "url": "https://example.com/a" })),
        "fetch https://example.com/a"
    );
}

#[test]
fn the_prompt_and_schema_are_what_the_model_is_given() {
    let schema = serde_json::to_value(WebfetchTool::new().schema()).expect("a schema is JSON");

    assert_eq!(WebfetchTool::new().id(), "webfetch");
    assert!(WebfetchTool::new().description().contains("Fetches content from a specified URL"));
    assert_eq!(schema["required"], serde_json::json!(["url"]));
    assert!(
        schema.to_string().contains("markdown"),
        "the schema should spell out the formats it accepts: {schema}"
    );
}

#[test]
fn entity_references_survive_the_stripper() {
    assert_eq!(
        super::strip_tags("<p>a &amp; b &lt;c&gt; &#39;d&#39; &x; &#x41;</p>"),
        "a & b <c> 'd' &x; A"
    );
    assert_eq!(
        super::strip_tags("<p>Tom & Jerry &amp; friends</p>"),
        "Tom & Jerry & friends",
        "a bare ampersand must not swallow the reference that follows it"
    );
}

#[test]
fn a_script_holding_markup_characters_does_not_swallow_the_page() {
    assert_eq!(
        super::strip_tags(
            "<p>before</p><script>for (i = 0; i < n; i++) { a = '</div>' }</script><p>after</p>"
        ),
        "before\n\nafter",
        "`i < n` inside a script is arithmetic, not the start of a tag"
    );
    assert_eq!(
        super::strip_tags("<p>kept</p><script>never closed"),
        "kept",
        "an unterminated script takes the rest of the document, as a browser's parser does"
    );
}

/// The reason the hand-rolled entity table went: it held six names, and
/// the two here are ordinary typography that any prose page carries.
#[test]
fn a_named_entity_outside_the_handful_still_decodes() {
    assert_eq!(super::strip_tags("<p>He said &mdash; &rsquo;yes&rsquo;</p>"), "He said — ’yes’");
}

/// A hand scanner that ends a tag at the first `>` ends this one inside
/// the attribute and hands the rest of it over as prose — `b">text`.
/// Knowing where a tag ends is the parser's job, and now it does it.
#[test]
fn a_greater_than_inside_an_attribute_does_not_end_its_own_tag() {
    assert_eq!(super::strip_tags("<p title=\"a > b\">text</p>"), "text");
}

/// Text is text. htmd's own rendering would reach the model as
/// `src/main\_test.rs` and `\[1\]` and `\*why\*`, because it escapes
/// every text node for a markdown syntax a plain-text answer does not
/// have; that is why this path walks the tree instead of rendering it.
#[test]
fn markdown_punctuation_in_prose_is_handed_over_unescaped() {
    let text = super::strip_tags(
        "<p>Run cargo build --release, edit src/main_test.rs, \
             and see the note [1] about *why*.</p>",
    );

    assert_eq!(
        text,
        "Run cargo build --release, edit src/main_test.rs, and see the note [1] about *why*."
    );
    assert!(!text.contains('\\'), "a plain-text reading has no syntax to escape for: {text:?}");
}

/// Titles and rows remain blocks while cells within one row remain inline.
#[test]
fn a_title_and_table_keep_their_intended_plain_text_boundaries() {
    let text = super::strip_tags(
        "<html><head><title>Page title</title></head><body><table>\
             <tr><td>A</td><td>B</td></tr><tr><td>C</td><td>D</td></tr>\
             </table></body></html>",
    );

    assert_eq!(text, "Page title\n\nAB\n\nCD");
}

/// A passing result proves both the text walk and the tree's teardown
/// return without consuming one call stack frame per nesting level.
#[test]
fn deeply_nested_inline_elements_do_not_overflow_the_stack() {
    let html = format!("{}text{}", "<i>".repeat(100_000), "</i>".repeat(100_000));

    assert_eq!(super::strip_tags(&html), "text");
}

/// The 100,000-level fixture completes in a real thread with a 2 MiB stack.
#[test]
fn deeply_nested_inline_elements_fit_a_two_mebibyte_thread_stack() {
    let rendered = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let html = format!("{}text{}", "<i>".repeat(100_000), "</i>".repeat(100_000));

            super::strip_tags(&html)
        })
        .expect("the bounded-stack webfetch test thread starts")
        .join()
        .expect("the bounded-stack webfetch test thread returns");

    assert_eq!(rendered, "text");
}

/// A deep template-contents chain is detached without rendering its text.
#[test]
fn deeply_nested_template_contents_do_not_overflow_the_stack() {
    const DEPTH: usize = 10_000;
    let html = format!("{}text{}", "<template>".repeat(DEPTH), "</template>".repeat(DEPTH));

    assert_eq!(super::strip_tags(&html), "");
}

/// A `<div>` chain of `chain` levels wrapped around one heading.
///
/// The heading is what tells the two renderings apart: htmd's walker writes it
/// as `# Deep`, while the text fallback writes the word alone, so a test can
/// name which arm ran without reaching for the log.
fn nested_divs(chain: usize) -> String {
    format!("{}<h1>Deep</h1>{}", "<div>".repeat(chain), "</div>".repeat(chain))
}

/// The depth the guard would measure for `html`, through the guard's own scan.
///
/// html5ever wraps what it parses in a document, an `<html>` and a `<body>`, so
/// a fixture's chain is shorter than the depth the walker descends. Measuring
/// rather than restating that arithmetic keeps these tests honest if the parser
/// ever wraps differently.
fn measured_depth(html: &str) -> usize {
    let tree = htmd::HtmlToMarkdown::new().html_to_tree(html).expect("the fixture parses");
    let depth = super::nesting_depth(&tree);
    super::drop_tree_iteratively(tree);

    depth
}

/// The scan counts every node the walker descends through, so ten more levels
/// of markup are ten more levels of depth whatever the parser wrapped them in.
#[test]
fn the_depth_scan_grows_one_for_one_with_the_chain_it_measures() {
    assert_eq!(measured_depth(&nested_divs(20)) - measured_depth(&nested_divs(10)), 10);
}

/// A document the walker can safely descend is still converted by it.
#[test]
fn a_document_within_the_cap_is_converted_by_the_markdown_walker() {
    let html = nested_divs(super::MAX_NESTING / 2);

    assert!(
        measured_depth(&html) <= super::MAX_NESTING,
        "the fixture is meant to sit under the cap"
    );
    assert_eq!(super::to_markdown(&html), "# Deep");
}

/// Past the cap the page still comes back — as its text, since the words are a
/// better answer than a refusal over a formatting choice the model did not make.
#[test]
fn a_document_nested_past_the_cap_is_rendered_as_text_rather_than_walked() {
    let html = nested_divs(super::MAX_NESTING + 1);

    assert!(measured_depth(&html) > super::MAX_NESTING, "the fixture is meant to clear the cap");
    assert_eq!(super::to_markdown(&html), "Deep");
}

/// A passing result proves the guard rather than the stack decides what htmd's
/// recursive walker is handed. Without it this fixture ends the **process**: a
/// stack overflow is not an unwind, so no test failure could report it.
///
/// Sixteen times the cap, which is roughly six times the shallowest depth the
/// walker was measured to die at, and no deeper: html5ever is itself quadratic
/// on a nested-`<div>` chain — it rescans the whole open-element stack for each
/// open tag — so the 100,000-level fixture the neighbouring `strip_tags` tests
/// use never finishes parsing here, and would be measuring that parser rather
/// than this guard.
#[test]
fn a_document_deep_enough_to_overflow_the_walker_answers_on_a_two_mebibyte_stack() {
    let rendered = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| super::to_markdown(&nested_divs(super::MAX_NESTING * 16)))
        .expect("the bounded-stack webfetch test thread starts")
        .join()
        .expect("the bounded-stack webfetch test thread returns");

    assert_eq!(rendered, "Deep");
}
