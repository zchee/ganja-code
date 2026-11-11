use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

#[test]
fn routing_consumes_outer_headers_and_preserves_the_inner_password_and_body() {
    let input = b"POST /session HTTP/1.1\r\nHost: localhost\r\nate-target-actor: ganja/worker\r\nx-ganja-proxy-token: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\nAuthorization: Basic secret-canary\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\nbody";
    let (actor, request, remaining) = route_request(
        input,
        &HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
    )
    .expect("route a valid worker request");
    assert_eq!(remaining, 0);
    assert_eq!(actor, ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() });
    assert_eq!(request, b"POST /session HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic secret-canary\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody");
}

#[test]
fn a_pipelined_followup_is_not_forwarded_with_the_first_request() {
    let input = b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nate-target-actor: ganja/worker\r\nx-ganja-proxy-token: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\r\nbodyGET /other HTTP/1.1\r\nate-target-actor: ganja/other\r\nx-ganja-proxy-token: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\r\n";
    let (_, request, remaining) = route_request(
        input,
        &HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
    )
    .expect("first request");
    assert_eq!(remaining, 0);
    assert_eq!(request, b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody");
}

#[test]
fn ambiguous_or_absent_routing_is_refused_before_opening_an_actor() {
    for input in [
        b"GET / HTTP/1.1\r\nHost: localhost\r\nx-ganja-proxy-token: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\r\n".as_slice(),
        b"GET / HTTP/1.1\r\nate-target-actor: g/a\r\nate-target-actor: g/b\r\nx-ganja-proxy-token: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\r\n",
        b"GET / HTTP/1.1\r\nate-target-actor: g/a/b\r\nx-ganja-proxy-token: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\r\n",
        b"CONNECT a:80 HTTP/1.1\r\nate-target-actor: g/a\r\nx-ganja-proxy-token: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\r\n",
    ] {
        assert_eq!(
            route_request(input, &HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")).expect_err("refuse invalid routing").kind,
            ProvisionErrorKind::Refused
        );
    }
}

#[derive(Debug)]
struct SocketTunnel {
    address: std::net::SocketAddr,
    opens: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Tunnel for SocketTunnel {
    async fn control(&self) -> Result<Channel, ProvisionError> {
        Ok(tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy())
    }

    async fn actor(&self, _actor: &ObjectRef) -> Result<TunnelIo, ProvisionError> {
        self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(TcpStream::connect(self.address).await.expect("upstream")))
    }
}

async fn socket_proxy() -> (ActorProxy, TcpListener, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("upstream listener");
    let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tunnel = Arc::new(SocketTunnel {
        address: listener.local_addr().expect("address"),
        opens: Arc::clone(&opens),
    });
    let proxy = ActorProxy::bind("ganja", tunnel).await.expect("proxy");
    proxy
        .admit(&ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() })
        .expect("admit");
    (proxy, listener, opens)
}

#[tokio::test]
async fn neither_buffered_nor_late_followup_bytes_reach_the_worker() {
    for late in [false, true] {
        let (proxy, listener, _) = socket_proxy().await;
        let token = proxy.token_header();
        let token = token.to_str().expect("token");
        let first = format!(
            "POST /first HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nate-target-actor: ganja/worker\r\nx-ganja-proxy-token: {token}\r\n\r\nbody"
        );
        let followup = format!(
            "GET /second HTTP/1.1\r\nate-target-actor: ganja/worker\r\nx-ganja-proxy-token: {token}\r\n\r\n"
        );
        let mut client = TcpStream::connect(proxy.address).await.expect("local connection");
        let request = if late {
            first.strip_suffix("body").expect("body suffix").to_owned()
        } else {
            format!("{first}{followup}")
        };
        client.write_all(request.as_bytes()).await.expect("first request");
        let (mut upstream, _) = listener.accept().await.expect("upstream connection");
        let mut received = read_head(&mut upstream).await.expect("upstream head");
        if late {
            client.write_all(b"body").await.expect("stream first body");
        }
        let end = received.windows(4).position(|bytes| bytes == b"\r\n\r\n").expect("head end") + 4;
        while received.len() < end + 4 {
            received.push(upstream.read_u8().await.expect("body byte"));
        }
        assert_eq!(&received[end..], b"body");
        assert!(!String::from_utf8_lossy(&received).contains("x-ganja-proxy-token"));
        if late {
            client.write_all(followup.as_bytes()).await.expect("late followup");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), upstream.read_u8()).await.is_err(),
            "no followup may reach upstream"
        );
        upstream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("response");
        upstream.shutdown().await.expect("close response");
        let mut response = Vec::new();
        if let Err(error) = client.read_to_end(&mut response).await {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
        assert!(response.ends_with(b"ok"));
    }
}

#[tokio::test]
async fn pooled_clients_use_two_admitted_connections_for_two_requests() {
    let (proxy, listener, opens) = socket_proxy().await;
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("connection");
            let head = read_head(&mut socket).await.expect("request");
            let head = String::from_utf8(head).expect("HTTP head");
            assert!(!head.contains("x-ganja-proxy-token"));
            assert!(!head.contains("ate-target-actor"));
            assert!(head.contains("Connection: close"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]")
                .await
                .expect("response");
        }
    });
    let mut headers = http::HeaderMap::new();
    headers.insert(TOKEN_HEADER, proxy.token_header());
    headers.insert("ate-target-actor", HeaderValue::from_static("ganja/worker"));
    let client = ganja_client::Client::new(&proxy.base_url(), None)
        .expect("typed client")
        .with_headers(headers);
    assert!(client.sessions().await.expect("first request").is_empty());
    assert!(client.sessions().await.expect("second request").is_empty());
    server.await.expect("server");
    assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn upgrades_and_transfer_encoded_requests_are_refused_without_an_upstream() {
    let (proxy, _listener, opens) = socket_proxy().await;
    for header in
        ["Upgrade: websocket", "Connection: keep-alive, Upgrade", "Transfer-Encoding: chunked"]
    {
        let token = proxy.token_header();
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\n{header}\r\nate-target-actor: ganja/worker\r\nx-ganja-proxy-token: {}\r\n\r\n",
            token.to_str().expect("token")
        );
        let mut client = TcpStream::connect(proxy.address).await.expect("client");
        client.write_all(request.as_bytes()).await.expect("request");
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.expect("refusal");
        assert!(response.starts_with(b"HTTP/1.1 502"));
    }
    assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_interim_continue_response_flows_before_the_request_body() {
    let (proxy, listener, _) = socket_proxy().await;
    let token = proxy.token_header();
    let mut client = TcpStream::connect(proxy.address).await.expect("client");
    let head = format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nExpect: 100-continue\r\nate-target-actor: ganja/worker\r\nx-ganja-proxy-token: {}\r\n\r\n",
        token.to_str().expect("token")
    );
    client.write_all(head.as_bytes()).await.expect("request head");
    let (mut upstream, _) = listener.accept().await.expect("upstream");
    let request = read_head(&mut upstream).await.expect("forwarded head");
    assert!(String::from_utf8_lossy(&request).contains("Expect: 100-continue"));
    upstream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.expect("interim response");
    let mut interim = [0; 25];
    tokio::time::timeout(Duration::from_millis(500), client.read_exact(&mut interim))
        .await
        .expect("interim response before upload")
        .expect("read interim");
    assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    client.write_all(b"body").await.expect("upload body");
    let mut body = [0; 4];
    upstream.read_exact(&mut body).await.expect("forwarded body");
    assert_eq!(&body, b"body");
    upstream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .await
        .expect("final response");
    upstream.shutdown().await.expect("response end");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
        .await
        .expect("response deadline")
        .expect("response");
    assert!(response.ends_with(b"ok"));
}

#[tokio::test]
async fn an_early_final_response_ends_an_incomplete_large_upload() {
    let (proxy, listener, _) = socket_proxy().await;
    let token = proxy.token_header();
    let mut client = TcpStream::connect(proxy.address).await.expect("client");
    let head = format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10485760\r\nate-target-actor: ganja/worker\r\nx-ganja-proxy-token: {}\r\n\r\npartial body",
        token.to_str().expect("token")
    );
    client.write_all(head.as_bytes()).await.expect("partial upload");
    let (mut upstream, _) = listener.accept().await.expect("upstream");
    read_head(&mut upstream).await.expect("request head");
    upstream
        .write_all(
            b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("early final response");
    upstream.shutdown().await.expect("response end");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_millis(500), client.read_to_end(&mut response))
        .await
        .expect("early final response must not wait for upload")
        .expect("response");
    assert!(response.starts_with(b"HTTP/1.1 413"));
}

#[tokio::test]
async fn the_connect_request_targets_port_8080() {
    let (client, mut server) = tokio::io::duplex(4096);
    let fixture = tokio::spawn(async move {
        let request = read_head(&mut server).await.expect("read CONNECT");
        assert_eq!(request, b"CONNECT worker:8080 HTTP/1.1\r\nHost: worker:8080\r\nate-target-actor: ganja/worker\r\n\r\n");
        server
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nbuffered")
            .await
            .expect("respond to CONNECT");
    });
    let actor = ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() };
    let mut tunnel = connect_actor(Box::new(client), &actor).await.expect("open CONNECT");
    let mut received = Vec::new();
    tunnel.read_to_end(&mut received).await.expect("read buffered body");
    assert_eq!(received, b"buffered");
    fixture.await.expect("join router fixture");
}

#[tokio::test]
async fn connect_failures_never_expose_the_remote_error_body() {
    for status in [401, 403, 502] {
        let (client, mut server) = tokio::io::duplex(4096);
        let fixture = tokio::spawn(async move {
            read_head(&mut server).await.expect("read CONNECT");
            server
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Refused\r\nContent-Length: 18\r\n\r\nprivate-key-canary"
                    )
                    .as_bytes(),
                )
                .await
                .expect("reject CONNECT");
        });
        let actor = ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() };
        let result = connect_actor(Box::new(client), &actor).await;
        let Err(error) = result else { panic!("refused CONNECT must fail") };
        assert!(!format!("{error:?} {error}").contains("private-key-canary"));
        assert_eq!(
            error.kind,
            if status == 502 { ProvisionErrorKind::Transient } else { ProvisionErrorKind::Refused }
        );
        fixture.await.expect("join rejecting fixture");
    }
}
