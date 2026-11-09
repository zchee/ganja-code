use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

#[test]
fn routing_consumes_outer_headers_and_preserves_the_inner_password_and_body() {
    let input = b"POST /session HTTP/1.1\r\nHost: localhost\r\nate-target-actor: ganja/worker\r\nx-ganja-proxy-token: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\nAuthorization: Basic secret-canary\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\nbody";
    let (actor, request) = route_request(
        input,
        &HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
    )
    .expect("route a valid worker request");
    assert_eq!(actor, ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() });
    assert_eq!(request, b"POST /session HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic secret-canary\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody");
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

#[tokio::test]
async fn connect_sends_routing_outside_worker_auth_and_preserves_buffered_bytes() {
    let (client, mut server) = tokio::io::duplex(4096);
    let fixture = tokio::spawn(async move {
        let request = read_head(&mut server).await.expect("read CONNECT");
        assert_eq!(request, b"CONNECT worker:80 HTTP/1.1\r\nHost: worker:80\r\nate-target-actor: ganja/worker\r\n\r\n");
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
