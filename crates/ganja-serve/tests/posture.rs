//! The three security postures, pinned: a non-loopback bind with no password
//! is refused at startup, a request naming a directory this server does not
//! serve is `400`, and a configured password gates every route — `401` with
//! the Basic challenge, satisfied by the header or by `?auth_token=`.

mod support;

use base64::Engine as _;
use ganja_serve::{Listen, ServeError};
use support::{base_url, credentials, engine, loopback_config};

#[tokio::test]
async fn a_non_loopback_bind_with_no_password_is_refused_at_startup() {
    let mut config = loopback_config();
    config.listen = Listen::Tcp { hostname: "0.0.0.0".to_owned(), port: Some(0) };
    config.credentials = None;

    let refused = ganja_serve::serve(engine(), config).await;
    let error = match refused {
        Err(error) => error,
        Ok(handle) => panic!(
            "an unsecured non-loopback bind must not come up; it did, on {}",
            handle.address()
        ),
    };

    assert!(
        matches!(
            error,
            ServeError::UnsecuredNonLoopback { ref hostname } if hostname == "0.0.0.0"
        ),
        "the refusal names the posture: {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("without a password") && message.contains("GANJA_SERVER_PASSWORD"),
        "the message says why and what to set: {message}"
    );
}

async fn stop_prebound(
    server: ganja_serve::Handle,
    deadline: tokio::time::Instant,
) -> std::io::Result<()> {
    let mut shutdown = tokio::spawn(server.shutdown());
    match tokio::time::timeout_at(deadline, &mut shutdown).await {
        Ok(joined) => joined.map_err(std::io::Error::other)?,
        Err(error) => {
            shutdown.abort();
            let _ = tokio::time::timeout_at(deadline + std::time::Duration::from_secs(5), shutdown)
                .await
                .map_err(std::io::Error::other)?;
            Err(std::io::Error::other(error))
        }
    }
}

#[test]
fn a_prebound_non_loopback_listener_requires_credentials_despite_a_loopback_config() {
    let (refused, cleanup) = support::server_fixture(|| async {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.expect("network listener");
        let result = tokio::time::timeout(
            support::DEADLINE,
            ganja_serve::serve_on_tcp(engine(), loopback_config(), listener),
        )
        .await;
        let refused = matches!(&result, Ok(Err(ServeError::UnsecuredNonLoopback { hostname })) if hostname == "0.0.0.0");
        let cleanup = if let Ok(Ok(server)) = result {
            stop_prebound(server, tokio::time::Instant::now() + std::time::Duration::from_secs(5))
                .await
        } else {
            Ok(())
        };
        (refused, cleanup)
    });
    cleanup.expect("prebound fixture stopped before asserting refusal");
    assert!(refused, "actual non-loopback address must require credentials");
}

#[test]
fn a_prebound_tcp_listener_refuses_a_socket_classified_config() {
    let (refused, cleanup) = support::server_fixture(|| async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("TCP listener");
        let mut config = loopback_config();
        config.listen =
            Listen::Unix { path: std::env::temp_dir().join("not-a-tcp-transport.sock") };
        let result = tokio::time::timeout(
            support::DEADLINE,
            ganja_serve::serve_on_tcp(engine(), config, listener),
        )
        .await;
        let refused = matches!(&result, Ok(Err(ServeError::ExpectedTcp)));
        let cleanup = if let Ok(Ok(server)) = result {
            stop_prebound(server, tokio::time::Instant::now() + std::time::Duration::from_secs(5))
                .await
        } else {
            Ok(())
        };
        (refused, cleanup)
    });
    cleanup.expect("prebound fixture stopped before asserting refusal");
    assert!(refused, "TCP cannot use the credential-free socket routes");
}

#[test]
fn a_prebound_listener_preserves_basic_epoch_directory_readiness_and_route_posture() {
    let (result, cleanup) = support::server_fixture(|| async {
        let deadline = tokio::time::Instant::now() + support::DEADLINE;
        let mut server = None;
        let result: Result<Result<(), Box<dyn std::error::Error + Send + Sync>>, _> =
            tokio::time::timeout_at(deadline, async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let mut config = loopback_config();
                config.credentials = Some(credentials());
                let epoch = config.journal.epoch().to_owned();
                server = Some(ganja_serve::serve_on_tcp(engine(), config, listener).await?);
                let base = base_url(server.as_ref().expect("prebound server exists"));
                let client = reqwest::Client::builder().timeout(support::DEADLINE).build()?;
                for (path, password, directory, status) in [
                    ("/global/health", None, None, 401),
                    ("/global/health", Some("wrong"), None, 401),
                    ("/global/health", Some("hunter2"), None, 200),
                    ("/global/health", Some("hunter2"), Some("/another-directory"), 400),
                    ("/readyz", None, None, 200),
                    ("/bootstrap", Some("hunter2"), None, 404),
                ] {
                    let mut request = client.get(format!("{base}{path}"));
                    if let Some(password) = password {
                        request = request.basic_auth("ganja", Some(password));
                    }
                    if let Some(directory) = directory {
                        request = request.header(ganja_serve::DIRECTORY_HEADER, directory);
                    }
                    let response = request.send().await?;
                    if response.status() != status {
                        return Err(format!(
                            "{path} answered {}, expected {status}",
                            response.status()
                        )
                        .into());
                    }
                    if response
                        .headers()
                        .get(ganja_serve::EPOCH_HEADER)
                        .and_then(|value| value.to_str().ok())
                        != Some(epoch.as_str())
                    {
                        return Err(format!("journal epoch missing or changed on {path}").into());
                    }
                    if status == 401
                        && response
                            .headers()
                            .get("www-authenticate")
                            .and_then(|value| value.to_str().ok())
                            != Some("Basic realm=\"Secure Area\"")
                    {
                        return Err("Basic challenge missing or changed".into());
                    }
                }
                Ok(())
            })
            .await;
        let cleanup = if let Some(server) = server {
            stop_prebound(server, tokio::time::Instant::now() + std::time::Duration::from_secs(5))
                .await
        } else {
            Ok(())
        };
        (result, cleanup)
    });
    cleanup.expect("stop prebound serve within cleanup reserve");
    result.expect("bounded route checks").expect("prebound TCP preserves engine posture");
}

#[test]
fn a_held_server_drain_ends_all_tasks_before_the_cleanup_verdict() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (result, engine) = support::server_fixture(|| async {
        let instance = engine();
        let engine = std::sync::Arc::downgrade(&instance);
        let result: Result<_, Box<dyn std::error::Error + Send + Sync>> = async {
            let deadline = tokio::time::Instant::now() + support::DEADLINE;
            let held = tokio::time::timeout_at(deadline, async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let address = listener.local_addr()?;
                let server =
                    ganja_serve::serve_on_tcp(instance, loopback_config(), listener).await?;
                let mut stream = tokio::net::TcpStream::connect(address).await?;
                stream.write_all(b"POST /permission/held/reply HTTP/1.1\r\nHost: localhost\r\nContent-Length: 64\r\nContent-Type: application/json\r\nExpect: 100-continue\r\n\r\n").await?;
                let mut response = Vec::new();
                while !response.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut bytes = [0; 1024];
                    let size = stream.read(&mut bytes).await?;
                    if size == 0 {
                        return Err("connection ended before requesting the held body".into());
                    }
                    response.extend_from_slice(&bytes[..size]);
                }
                if !response.starts_with(b"HTTP/1.1 100 Continue") {
                    return Err("server must begin reading the deliberately incomplete body".into());
                }
                // Continue proves an accepted task is waiting for this withheld body.
                let cleanup = stop_prebound(
                    server,
                    tokio::time::Instant::now() + std::time::Duration::from_millis(50),
                )
                .await;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>((cleanup, stream.into_std()?))
            })
            .await??;
            Ok(held)
        }
        .await;
        (result, engine)
    });
    let (cleanup, _connection) = result.expect("accepted request body held before cleanup");
    let error = cleanup.expect_err("the deliberately held drain must exhaust cleanup");
    assert!(
        error.get_ref().is_some_and(|error| error.is::<tokio::time::error::Elapsed>()),
        "cleanup must time out while draining: {error}"
    );
    assert!(
        engine.upgrade().is_none(),
        "all actual server tasks must end before the cleanup verdict"
    );
}

#[tokio::test]
async fn a_request_naming_another_directory_is_refused_with_400() {
    let handle =
        ganja_serve::serve(engine(), loopback_config()).await.expect("a loopback server comes up");
    let base = base_url(&handle);
    let served =
        std::env::current_dir().expect("the working directory resolves").display().to_string();

    // The right directory — by header, by query, or unstated — is served.
    for request in [
        reqwest::Client::new().get(format!("{base}/global/health")),
        reqwest::Client::new()
            .get(format!("{base}/global/health"))
            .header(ganja_serve::DIRECTORY_HEADER, served.clone()),
        reqwest::Client::new().get(format!("{base}/global/health?directory={served}")),
    ] {
        let response = request.send().await.expect("the route answers");
        assert_eq!(response.status(), 200, "the served directory is served");
    }

    // Another directory — even a real one — is a refusal, never a silent
    // answer about the wrong worktree.
    let elsewhere = std::env::temp_dir().display().to_string();
    let by_header = reqwest::Client::new()
        .get(format!("{base}/global/health"))
        .header(ganja_serve::DIRECTORY_HEADER, elsewhere.clone())
        .send()
        .await
        .expect("the route answers");
    assert_eq!(by_header.status(), 400);
    let body = by_header.json::<serde_json::Value>().await.expect("the refusal is tagged JSON");
    assert_eq!(body["type"], "invalid_request");
    assert!(
        body["message"].as_str().is_some_and(|message| message.contains(&elsewhere)),
        "the refusal names the directory it refused: {body}"
    );

    let by_query = reqwest::get(format!("{base}/global/health?directory={elsewhere}"))
        .await
        .expect("the route answers");
    assert_eq!(by_query.status(), 400, "the query spelling is the same refusal");

    handle.shutdown().await.expect("a clean stop");
}

#[tokio::test]
async fn a_configured_password_gates_every_route() {
    let mut config = loopback_config();
    config.credentials = Some(credentials());
    let handle = ganja_serve::serve(engine(), config).await.expect("a loopback server comes up");
    let base = base_url(&handle);

    // No credential: the Basic challenge, empty-bodied.
    let refused = reqwest::get(format!("{base}/global/health")).await.expect("the route answers");
    assert_eq!(refused.status(), 401);
    assert_eq!(
        refused.headers().get("www-authenticate").and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"Secure Area\"")
    );
    assert!(refused.bytes().await.expect("a body").is_empty());

    // The wrong password is the same refusal.
    let wrong = reqwest::Client::new()
        .get(format!("{base}/global/health"))
        .basic_auth("ganja", Some("hunter3"))
        .send()
        .await
        .expect("the route answers");
    assert_eq!(wrong.status(), 401);

    // The Basic header opens the door.
    let by_header = reqwest::Client::new()
        .get(format!("{base}/global/health"))
        .basic_auth("ganja", Some("hunter2"))
        .send()
        .await
        .expect("the route answers");
    assert_eq!(by_header.status(), 200);

    // So does the query escape hatch an EventSource is limited to; the event
    // stream is the route it exists for, so that is the one it is proven on.
    let token = base64::engine::general_purpose::STANDARD.encode("ganja:hunter2");
    let by_token =
        reqwest::get(format!("{base}/event?auth_token={token}")).await.expect("the route answers");
    assert_eq!(by_token.status(), 200);
    assert_eq!(
        by_token.headers().get("content-type").and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    drop(by_token);

    handle.shutdown().await.expect("a clean stop");
}
