//! The far side of a socket crossing, as a `ganja-core` test can build one
//! (**D532**, **D534**).
//!
//! `ganja-core` may not link `ganja-serve` — the engine grows no HTTP server,
//! and CI gates it — so the peer these suites send to is a few dozen lines of
//! HTTP/1.1 over a real `UnixListener` rather than the shipped router. What
//! it answers is fixed by the test; what it **received** is what the test
//! asserts on, which is the whole point: these suites pin what this side puts
//! on the wire and what it does with the answer, and `ganja-serve`'s own
//! suites pin the router.
//!
//! Blocking sockets on their own OS thread, deliberately: the requests under
//! test are `reqwest`'s on the test's runtime, and a stub that never touches
//! that runtime cannot deadlock it however a test drives it.

#![allow(dead_code, reason = "each suite uses a different part of the stub")]

use std::{
    collections::VecDeque,
    io::{BufRead as _, BufReader, Read as _, Write as _},
    os::unix::{fs::PermissionsExt as _, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// How long a test waits for the stub to record something before declaring
/// the fixture broken.
pub const EVENTUALLY: Duration = Duration::from_secs(10);

/// The team name every stub peer answers `GET /team` with, so a sender's
/// `Sent.to` composition is a thing a test can spell.
pub const FAR_TEAM: &str = "far-team";

/// The lead name every stub peer answers with — `MemberName`-legal, because
/// the sender puts it in a URL.
pub const FAR_LEAD: &str = "team-lead";

/// One request the stub took in, as the test reads it back.
#[derive(Clone, Debug)]
pub struct Taken {
    /// The request line's path.
    pub route: String,
    /// The body, parsed — every route here takes JSON or nothing.
    pub body: serde_json::Value,
}

/// What a stub answers `POST /team/{lead}/message` with.
#[derive(Clone, Debug)]
pub enum Answer {
    /// `SocketDelivered` with no `held` field — an accept, and byte-for-byte
    /// what a refuse answers too.
    Accepted,
    /// `SocketDelivered` carrying `held`, with the cause spelled as the wire
    /// spells it (`"explicit"`, `"mode_mismatch"`, …).
    Held(&'static str),
    /// A verbatim body, for the skew cases.
    Raw(String),
}

impl Answer {
    fn body(&self) -> String {
        match self {
            Self::Accepted => {
                format!(r#"{{"to":"{FAR_LEAD}","note":"received; it is in the lead's inbox"}}"#)
            }
            Self::Held(cause) => format!(
                r#"{{"to":"{FAR_LEAD}","note":"held for review","held":{{"cause":{cause}}}}}"#
            ),
            Self::Raw(body) => body.clone(),
        }
    }
}

/// A session socket in the one shape `vet_address` admits: a real socket at a
/// session-stem name inside a `0700` directory of this uid's.
///
/// Answers the two routes the crossing drives (`GET /team`, `POST
/// /team/{lead}/message`) and the one a settlement rides (`POST
/// /peer/receipt`), records every request, and answers everything else
/// `404`.
pub struct FarSide {
    directory: tempfile::TempDir,
    path: PathBuf,
    taken: Arc<Mutex<Vec<Taken>>>,
    answers: Arc<Mutex<VecDeque<Answer>>>,
    running: Arc<AtomicBool>,
}

impl FarSide {
    /// A peer that accepts everything.
    #[must_use]
    pub fn accepting() -> Self {
        Self::answering(Vec::new())
    }

    /// A peer whose message answers are `answers` in order, falling back to
    /// [`Answer::Accepted`] once they run out.
    #[must_use]
    pub fn answering(answers: Vec<Answer>) -> Self {
        Self::at(
            tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .expect("a private directory"),
            "0198c1a2",
            answers,
        )
    }

    /// The same, under a caller-chosen stem — for the tests that need two
    /// peers to be told apart by the stem a chain carries.
    #[must_use]
    pub fn named(stem: &str, answers: Vec<Answer>) -> Self {
        Self::at(
            tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .expect("a private directory"),
            stem,
            answers,
        )
    }

    fn at(directory: tempfile::TempDir, stem: &str, answers: Vec<Answer>) -> Self {
        let path = directory.path().join(format!("{stem}.sock"));
        let listener = UnixListener::bind(&path).expect("a socket binds");
        listener
            .set_nonblocking(false)
            .expect("a blocking listener");

        let taken: Arc<Mutex<Vec<Taken>>> = Arc::default();
        let answers = Arc::new(Mutex::new(VecDeque::from(answers)));
        let running = Arc::new(AtomicBool::new(true));

        let served = Arc::clone(&taken);
        let queued = Arc::clone(&answers);
        let alive = Arc::clone(&running);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if !alive.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(stream) = stream else {
                    return;
                };
                serve_one(stream, &served, &queued);
            }
        });

        Self {
            directory,
            path,
            taken,
            answers,
            running,
        }
    }

    /// Where this peer answers.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory it lives in — what a session binding beside it uses.
    #[must_use]
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }

    /// The address a model would write.
    #[must_use]
    pub fn address(&self) -> String {
        format!("uds:{}", self.path.display())
    }

    /// Everything it has taken in so far.
    #[must_use]
    pub fn taken(&self) -> Vec<Taken> {
        self.taken
            .lock()
            .expect("the log is never poisoned")
            .clone()
    }

    /// Every request it took on `route`.
    #[must_use]
    pub fn taken_on(&self, route: &str) -> Vec<Taken> {
        self.taken()
            .into_iter()
            .filter(|taken| taken.route == route)
            .collect()
    }

    /// The one message body it took, or a panic naming what it did take.
    #[must_use]
    pub fn message(&self) -> serde_json::Value {
        let messages = self.taken_on(&format!("/team/{FAR_LEAD}/message"));
        assert_eq!(messages.len(), 1, "exactly one message: {:?}", self.taken());

        messages[0].body.clone()
    }

    /// Waits until at least `count` requests have landed on `route`, or
    /// panics.
    pub fn wait_for(&self, route: &str, count: usize) -> Vec<Taken> {
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            let taken = self.taken_on(route);
            if taken.len() >= count {
                return taken;
            }
            assert!(
                Instant::now() < deadline,
                "{route} took {} of {count} before the deadline",
                taken.len()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Gives anything already in flight time to land, then answers what did.
    #[must_use]
    pub fn settled(&self, grace: Duration) -> Vec<Taken> {
        std::thread::sleep(grace);

        self.taken()
    }
}

impl Drop for FarSide {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // One connection so the accept loop wakes and sees the flag.
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
    }
}

/// One HTTP/1.1 exchange: the request line, headers until the blank line,
/// exactly `Content-Length` body bytes, then one answer and a close.
fn serve_one(
    mut stream: std::os::unix::net::UnixStream,
    taken: &Arc<Mutex<Vec<Taken>>>,
    answers: &Arc<Mutex<VecDeque<Answer>>>,
) {
    let mut reader = BufReader::new(stream.try_clone().expect("a stream clones"));
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() || request.trim().is_empty() {
        return;
    }
    let route = request
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();

    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() {
            return;
        }
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    if length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let parsed = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    taken
        .lock()
        .expect("the log is never poisoned")
        .push(Taken {
            route: route.clone(),
            body: parsed,
        });

    let answer = if route == "/team" {
        Some(format!(
            r#"{{"team":"{FAR_TEAM}","lead":"{FAR_LEAD}","members":[]}}"#
        ))
    } else if route == format!("/team/{FAR_LEAD}/message") {
        Some(
            answers
                .lock()
                .expect("the answers are never poisoned")
                .pop_front()
                .unwrap_or(Answer::Accepted)
                .body(),
        )
    } else if route == "/peer/receipt" {
        Some("{}".to_owned())
    } else {
        None
    };

    let response = match answer {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        None => {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
        }
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
