//! A loopback HTTP server the client suites speak real bytes to.
//!
//! Hand-rolled rather than borrowed from `ganja-serve`, because linking the
//! server into the client's tests would put `axum` in this crate's graph and
//! the point of the crate is that it is not there. What a suite needs is a
//! socket that answers exactly the bytes it was told to answer — including
//! malformed ones no real server would send — which a stub does better than a
//! router anyway.
//!
//! Not a test binary — `tests/support/` is a directory module, compiled only
//! into the binaries that declare `mod support;`.

// Compiled once per binary, and each binary uses its own subset.
#![allow(dead_code)]

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::TcpListener,
};

/// How long any single wait may take before the fixture is declared broken.
pub const DEADLINE: Duration = Duration::from_secs(30);

/// The health body a well-behaved server answers with, spelled once for
/// every suite that opens with it.
pub const HEALTHY: &str =
    r#"{"healthy":true,"version":"0.1.0","session_id":"01998ad0-0000-7000-8000-00000000d505"}"#;

/// One request the stub received, as the assertions read it.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub accept: Option<String>,
    pub body: String,
}

/// What the stub answers with.
#[derive(Clone, Debug)]
pub enum Reply {
    /// A JSON document, with the status it carries.
    Json { status: u16, body: String },
    /// `204`, the way every accepting route here answers.
    Accepted,
    /// An event stream: the chunks are written in order, and the connection
    /// then closes — which is how a stream ends when nothing evicted it.
    Stream { chunks: Vec<String> },
}

impl Reply {
    /// A `200` carrying `body`.
    pub fn ok(body: impl Into<String>) -> Self {
        Self::Json {
            status: 200,
            body: body.into(),
        }
    }
}

/// A server that answers whatever the closure it was built with returns.
///
/// Listens on a loopback port ([`Stub::answering`]) or on a Unix socket
/// ([`Stub::on_socket`]); the same responder serves both, because the client
/// under test speaks the same bytes to both — which is the whole point of
/// the socket form.
pub struct Stub {
    address: String,
    /// The socket file, when this stub listens on one — unlinked on drop.
    socket: Option<PathBuf>,
    received: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
    }
}

impl Stub {
    /// Binds a loopback port and answers every request with `answer`.
    pub async fn answering(
        answer: impl Fn(&Recorded) -> Reply + Send + Sync + 'static,
    ) -> Arc<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("a loopback port is bindable");
        let address = format!(
            "http://{}",
            listener.local_addr().expect("the bound address reads")
        );
        let received = Arc::new(Mutex::new(Vec::new()));

        let answer = Arc::new(answer);
        let task = tokio::spawn({
            let received = Arc::clone(&received);
            async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    let answer = Arc::clone(&answer);
                    let received = Arc::clone(&received);
                    tokio::spawn(async move {
                        serve_one(socket, answer.as_ref(), &received).await;
                    });
                }
            }
        });

        Arc::new(Self {
            address,
            socket: None,
            received,
            task,
        })
    }

    /// Binds a Unix socket in the temp directory and answers every request
    /// with `answer`. The path is short — the temp root plus a few bytes —
    /// so it fits `sun_path` on every platform this runs on, and unique per
    /// process and per call so parallel suites cannot collide.
    #[cfg(unix)]
    pub async fn on_socket(
        answer: impl Fn(&Recorded) -> Reply + Send + Sync + 'static,
    ) -> Arc<Self> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ganja-client-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("a socket is bindable");
        let received = Arc::new(Mutex::new(Vec::new()));

        let answer = Arc::new(answer);
        let task = tokio::spawn({
            let received = Arc::clone(&received);
            async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    let answer = Arc::clone(&answer);
                    let received = Arc::clone(&received);
                    tokio::spawn(async move {
                        serve_one(socket, answer.as_ref(), &received).await;
                    });
                }
            }
        });

        Arc::new(Self {
            address: path.display().to_string(),
            socket: Some(path),
            received,
            task,
        })
    }

    /// A stub that answers the same document to everything.
    pub async fn always(reply: Reply) -> Arc<Self> {
        Self::answering(move |_| reply.clone()).await
    }

    /// Where it listens, in the spelling a client takes.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// A client pointed at this stub, in whichever address form it listens
    /// on.
    pub fn client(&self) -> ganja_client::Client {
        match &self.socket {
            #[cfg(unix)]
            Some(socket) => {
                ganja_client::Client::on_socket(socket).expect("the stub's socket is usable")
            }
            #[cfg(not(unix))]
            Some(_) => unreachable!("a socket stub is never built off unix"),
            None => ganja_client::Client::new(&self.address, None)
                .expect("the stub's address is usable"),
        }
    }

    /// Every request it has received, in order.
    pub fn received(&self) -> Vec<Recorded> {
        self.received
            .lock()
            .expect("the request log is never poisoned")
            .clone()
    }

    /// The one request it received, which is what most suites assert about.
    pub fn only_request(&self) -> Recorded {
        let received = self.received();
        assert_eq!(received.len(), 1, "expected one request: {received:?}");

        received.into_iter().next().expect("just counted")
    }
}

/// Reads one request off `socket`, records it, and writes the answer.
async fn serve_one<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: S,
    answer: &(dyn Fn(&Recorded) -> Reply + Send + Sync),
    received: &Mutex<Vec<Recorded>>,
) {
    let Some(request) = read_request(&mut socket).await else {
        return;
    };
    received
        .lock()
        .expect("the request log is never poisoned")
        .push(request.clone());

    match answer(&request) {
        Reply::Json { status, body } => {
            let head = format!(
                "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reason(status),
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
        }
        Reply::Accepted => {
            let _ = socket
                .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                .await;
        }
        Reply::Stream { chunks } => {
            // No content-length and no chunked encoding: the body ends when
            // the connection does, which is what an SSE response is.
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      cache-control: no-cache, no-transform\r\nconnection: close\r\n\r\n",
                )
                .await;
            for chunk in chunks {
                if socket.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                // A pause between writes, so a frame really does arrive in its
                // own read and a splitter that only worked on whole buffers
                // would show.
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }

    let _ = socket.shutdown().await;
}

/// The request head plus its body, or [`None`] when the peer left first.
async fn read_request<S: AsyncRead + Unpin>(socket: &mut S) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let head = loop {
        if let Some(end) = find_head_end(&buffer) {
            break end;
        }
        let mut chunk = [0u8; 1024];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let text = String::from_utf8_lossy(&buffer[..head]).into_owned();
    let mut lines = text.lines();
    let mut request = lines.next()?.split_whitespace();
    let method = request.next()?.to_owned();
    let path = request.next()?.to_owned();

    let mut authorization = None;
    let mut accept = None;
    let mut length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "authorization" => authorization = Some(value.to_owned()),
            "accept" => accept = Some(value.to_owned()),
            "content-length" => length = value.parse().unwrap_or(0),
            _ => {}
        }
    }

    let mut body: Vec<u8> = buffer[head + 4..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 1024];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Some(Recorded {
        method,
        path,
        authorization,
        accept,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Status",
    }
}

/// One SSE frame, spelled the way `ganja-serve` writes it.
pub fn frame(name: &str, data: &str) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}
