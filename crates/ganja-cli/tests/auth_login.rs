//! `ganja auth login`'s flows, driven against an issuer this suite owns.
//!
//! Every assertion about a token is on its redacted tail. A test that printed a
//! whole one would put it in CI output, which is the failure the redaction
//! exists to prevent — so the canaries below are also hunted for by name.
//!
//! **The gate is the point of this file.** A device login has to show the code
//! and the address *before* it blocks on somebody having used them, and the
//! only way to assert an ordering from outside the process is to make the
//! second thing unable to finish until the first has been observed: the token
//! exchange is held open until this suite has read the code off the child's
//! standard error. A build that printed the code afterwards would deadlock
//! against its own login, which is what the deadline turns into a failure.

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

/// The access token every completed login in this file stores, shaped so that
/// only its last four characters may ever be printed.
const ACCESS: &str = "at-login-canary-7731";

/// The refresh token stored beside it, which nothing may print at all.
const REFRESH: &str = "rt-login-canary-9902";

/// What each provider's device-code endpoint answers with, one per provider so
/// a test cannot pass by reading another's.
const GROK_CODE: &str = "GROK-1234";
const COPILOT_CODE: &str = "COPI-5678";
const CHATGPT_CODE: &str = "CHAT-9012";

/// Where a person is told to go. Never fetched — it is printed and nothing
/// else — so it names an address rather than reaching one.
const VERIFICATION_URI: &str = "https://login.example/device";

/// How long a test waits for something the child was supposed to have printed.
///
/// Generous on purpose: a timeout here should mean "it never printed it", not
/// "the machine was busy".
const DEADLINE: Duration = Duration::from_secs(20);

/// What holds a token exchange open until this suite says otherwise.
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn closed() -> Arc<Self> {
        Arc::new(Self {
            open: Mutex::new(false),
            changed: Condvar::new(),
        })
    }

    /// Blocks the answer until [`Self::open`] has been called.
    fn hold(&self) {
        let mut open = self.open.lock().expect("the gate is not poisoned");
        while !*open {
            open = self.changed.wait(open).expect("the gate is not poisoned");
        }
    }

    fn open(&self) {
        *self.open.lock().expect("the gate is not poisoned") = true;
        self.changed.notify_all();
    }
}

/// Answers the three login shapes this build knows, at the paths their real
/// endpoints use, and says where it is listening.
///
/// A thread and the standard library's own listener rather than a runtime: this
/// suite drives a built binary, and all it needs of an HTTP server is that
/// there is one. Requests arrive one at a time, because a login makes one at a
/// time.
fn serve(gate: &Arc<Gate>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
    let url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );
    let gate = Arc::clone(gate);

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let Some(path) = request(&mut stream) else {
                continue;
            };
            let (status, body) = answer(&path, &gate);
            let response = format!(
                "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: \
                 {}\r\nconnection: close\r\n\r\n{body}",
                if status == 200 { "OK" } else { "Not Found" },
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    url
}

/// What one path answers with, and which of them the gate holds.
fn answer(path: &str, gate: &Gate) -> (u16, String) {
    match path {
        // xAI's own paths (`xai.ts:12`, `:20`).
        "/oauth2/device/code" => (200, device_code(GROK_CODE)),
        // GitHub's (`copilot.ts:19-24`).
        "/login/device/code" => (200, device_code(COPILOT_CODE)),
        // ChatGPT's, which is not RFC 8628 (`openai.ts:105`).
        "/api/accounts/deviceauth/usercode" => (
            200,
            format!(r#"{{"device_auth_id":"dai-1","user_code":"{CHATGPT_CODE}","interval":"1"}}"#),
        ),
        "/oauth2/token" => {
            gate.hold();
            (
                200,
                format!(
                    r#"{{"access_token":"{ACCESS}","refresh_token":"{REFRESH}","expires_in":3600}}"#
                ),
            )
        }
        // Copilot's token *is* the credential, so its response carries nothing
        // else (`copilot.ts:280-284`).
        "/login/oauth/access_token" => {
            gate.hold();
            (200, format!(r#"{{"access_token":"{ACCESS}"}}"#))
        }
        "/api/accounts/deviceauth/token" => {
            gate.hold();
            (
                200,
                r#"{"authorization_code":"ac-1","code_verifier":"cv-1"}"#.to_owned(),
            )
        }
        // The exchange the ChatGPT device grant ends in, already past the gate.
        "/oauth/token" => (
            200,
            format!(
                r#"{{"access_token":"{ACCESS}","refresh_token":"{REFRESH}","expires_in":3600}}"#
            ),
        ),
        _ => (404, "{}".to_owned()),
    }
}

/// One authorization, with no `verification_uri_complete` so that what is
/// printed can only have come from `verification_uri`.
fn device_code(user_code: &str) -> String {
    format!(
        r#"{{"device_code":"dc-secret","user_code":"{user_code}","verification_uri":"{VERIFICATION_URI}","interval":1,"expires_in":300}}"#
    )
}

/// Reads a request off `stream` and answers with the path it named.
///
/// The body is read and discarded so that the client is never writing into a
/// socket nobody is reading; what it holds is a device code, and this file has
/// no business asserting on one.
fn request(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return None,
        }
    }

    let head = String::from_utf8_lossy(&head).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if stream.read_exact(&mut body).is_err() {
        return None;
    }

    Some(head.split_whitespace().nth(1)?.to_owned())
}

fn data() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

fn stored_at(data: &TempDir) -> PathBuf {
    data.path().join("ganja").join("auth.json")
}

/// An invocation with its own data directory, its own issuer, and none of the
/// developer's exported keys.
fn ganja(data: &TempDir, issuer: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        .env("XDG_DATA_HOME", data.path())
        .env("GANJA_AUTH_ISSUER", issuer)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    command
}

/// The child's standard error, one line at a time, so a test can wait for a
/// line without waiting for the process.
fn watching(child: &mut Child) -> Receiver<String> {
    let stderr = child.stderr.take().expect("stderr was piped");
    let (sender, lines) = mpsc::channel();

    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            if sender.send(line).is_err() {
                return;
            }
        }
    });

    lines
}

/// Everything printed up to and including the line carrying `needle`.
///
/// Panics on the deadline rather than blocking forever, which is what makes a
/// build that prints the code *after* the exchange a failing test instead of a
/// hung suite.
fn printed_before(lines: &Receiver<String>, needle: &str) -> Vec<String> {
    let until = Instant::now() + DEADLINE;
    let mut seen = Vec::new();

    loop {
        let left = until.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "{needle:?} was not printed before the login blocked; what was: {seen:#?}"
        );

        match lines.recv_timeout(left) {
            Ok(line) => {
                let found = line.contains(needle);
                seen.push(line);
                if found {
                    return seen;
                }
            }
            Err(_) => panic!("{needle:?} was never printed; what was: {seen:#?}"),
        }
    }
}

/// What a finished login left on the two streams.
struct Finished {
    stdout: String,
    stderr: String,
    ok: bool,
}

/// Waits for `child`, collecting what it said. `stderr` is whatever the watcher
/// already read plus the rest of it.
fn finish(mut child: Child, lines: &Receiver<String>, mut seen: Vec<String>) -> Finished {
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout was piped")
        .read_to_string(&mut stdout)
        .expect("stdout reads");
    let ok = child.wait().expect("the child is waitable").success();
    // Drained after the exit, so nothing the login said on its way out is
    // missed by a test asserting on what it did *not* say.
    while let Ok(line) = lines.recv_timeout(Duration::from_millis(200)) {
        seen.push(line);
    }

    Finished {
        stdout,
        stderr: seen.join("\n"),
        ok,
    }
}

/// The stored credential file, parsed.
fn stored(data: &TempDir) -> serde_json::Value {
    let written = std::fs::read_to_string(stored_at(data)).expect("a login stored something");

    serde_json::from_str(&written).expect("the store is JSON")
}

/// Nothing anywhere may carry a whole token.
fn leaks_nothing(finished: &Finished) {
    for secret in [ACCESS, REFRESH, "dc-secret", "cv-1"] {
        assert!(
            !finished.stdout.contains(secret) && !finished.stderr.contains(secret),
            "a login printed {secret:?} whole"
        );
    }
}

#[test]
fn a_device_login_shows_the_code_and_the_address_before_it_waits_on_them() {
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let mut child = ganja(&data, &issuer)
        .args(["auth", "login", "--provider", "grok", "--method", "device"])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);

    // The exchange cannot answer until this returns, so everything collected
    // here reached the screen before the login blocked on it. "Waiting" is the
    // last thing said before the block, which is what makes the two assertions
    // below statements about order rather than about presence.
    let seen = printed_before(&lines, "Waiting for authorization");
    assert!(
        seen.iter().any(|line| line.contains(GROK_CODE)),
        "the code to type has to be shown before the wait: {seen:#?}"
    );
    assert!(
        seen.iter().any(|line| line.contains(VERIFICATION_URI)),
        "the address to type it at has to be shown before the wait: {seen:#?}"
    );

    gate.open();
    let finished = finish(child, &lines, seen);

    assert!(finished.ok, "the login should have succeeded: {finished:?}");
    assert!(
        finished.stdout.contains("****7731") && finished.stdout.contains("grok"),
        "a stored login reports its provider and its redacted tail: {:?}",
        finished.stdout
    );
    leaks_nothing(&finished);
}

/// ganja calls the provider `grok` and the file calls it `xai`, which is
/// upstream's name — so a shared `auth.json` keeps working.
#[test]
fn a_completed_grok_login_is_stored_as_an_xai_oauth_credential() {
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let mut child = ganja(&data, &issuer)
        .args(["auth", "login", "--provider", "grok", "--method", "device"])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);
    let seen = printed_before(&lines, GROK_CODE);
    gate.open();
    let finished = finish(child, &lines, seen);
    assert!(finished.ok, "the login should have succeeded: {finished:?}");

    let store = stored(&data);
    assert_eq!(store["xai"]["type"], "oauth");
    assert_eq!(store["xai"]["access"], ACCESS);
    assert_eq!(store["xai"]["refresh"], REFRESH);
    assert!(
        store.get("grok").is_none(),
        "the command-line name is not a line in the file: {store}"
    );
}

/// The listing is where a login and a pasted key have to be told apart, because
/// at least one provider stores both under the same name.
#[test]
fn the_listing_names_a_login_oauth_and_a_pasted_key_api() {
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let mut child = ganja(&data, &issuer)
        .args(["auth", "login", "--provider", "grok", "--method", "device"])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);
    let seen = printed_before(&lines, GROK_CODE);
    gate.open();
    assert!(finish(child, &lines, seen).ok, "the login should succeed");

    let keyed = ganja(&data, &issuer)
        .args([
            "auth",
            "login",
            "--provider",
            "anthropic",
            "--key",
            "sk-listing-4242",
        ])
        .output()
        .expect("the binary runs");
    assert!(keyed.status.success(), "storing a key should succeed");

    let listed = ganja(&data, &issuer)
        .args(["auth", "list"])
        .output()
        .expect("the binary runs");
    let table = String::from_utf8_lossy(&listed.stdout).into_owned();

    assert!(
        table.contains("TYPE"),
        "the listing needs the column: {table}"
    );
    let anthropic = row(&table, "anthropic");
    let xai = row(&table, "xai");
    assert!(
        anthropic.contains("api"),
        "a pasted key is an `api` credential: {anthropic:?}"
    );
    assert!(
        xai.contains("oauth"),
        "a login is an `oauth` credential: {xai:?}"
    );
    assert!(
        !anthropic.contains("oauth"),
        "the two must not read the same: {anthropic:?}"
    );
}

/// The row a listing gives `provider`, so an assertion cannot be satisfied by
/// another provider's.
fn row<'a>(table: &'a str, provider: &str) -> &'a str {
    table
        .lines()
        .find(|line| line.starts_with(provider))
        .unwrap_or_else(|| panic!("{provider} has no row in {table}"))
}

/// Logging out has to reach the entry under the name it was *stored* as, which
/// for grok is not the name that was typed.
#[test]
fn logging_out_of_grok_forgets_the_credential_filed_under_xai() {
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let mut child = ganja(&data, &issuer)
        .args(["auth", "login", "--provider", "grok", "--method", "device"])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);
    let seen = printed_before(&lines, GROK_CODE);
    gate.open();
    assert!(finish(child, &lines, seen).ok, "the login should succeed");
    assert_eq!(stored(&data)["xai"]["type"], "oauth");

    let out = ganja(&data, &issuer)
        .args(["auth", "logout", "--provider", "grok"])
        .output()
        .expect("the binary runs");
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("forgot"),
        "logout has to say it did something"
    );
    assert!(
        stored(&data).get("xai").is_none(),
        "the entry filed under xai is what had to go: {}",
        stored(&data)
    );
}

/// The address question is upstream's second prompt and is asked only when the
/// first one's answer needs it (`copilot.ts:208`).
#[test]
fn the_enterprise_address_is_asked_for_only_when_the_deployment_is_one() {
    for (answers, enterprise) in [
        ("1\n", None),
        ("2\ncompany.ghe.com\n", Some("company.ghe.com")),
    ] {
        let gate = Gate::closed();
        let issuer = serve(&gate);
        let data = data();

        let mut child = ganja(&data, &issuer)
            .args([
                "auth",
                "login",
                "--provider",
                "github-copilot",
                "--method",
                "device",
            ])
            .stdin(Stdio::piped())
            .spawn()
            .expect("the binary runs");
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(answers.as_bytes())
            .expect("the answers are writable");
        let lines = watching(&mut child);
        let seen = printed_before(&lines, COPILOT_CODE);
        gate.open();
        let finished = finish(child, &lines, seen);
        assert!(finished.ok, "the login should have succeeded: {finished:?}");

        let asked = finished.stderr.contains("GitHub Enterprise URL or domain");
        assert_eq!(
            asked,
            enterprise.is_some(),
            "the address question belongs to the enterprise answer alone: {:?}",
            finished.stderr
        );

        let store = stored(&data);
        assert_eq!(store["github-copilot"]["type"], "oauth");
        // The token *is* the credential, so it is stored twice and never
        // expires (`copilot.ts:294-298`).
        assert_eq!(store["github-copilot"]["access"], ACCESS);
        assert_eq!(store["github-copilot"]["refresh"], ACCESS);
        assert_eq!(store["github-copilot"]["expires"], 0);
        assert_eq!(
            store["github-copilot"].get("enterpriseUrl"),
            enterprise.map(serde_json::Value::from).as_ref(),
            "the deployment decides what is stored beside the token"
        );
        leaks_nothing(&finished);
    }
}

/// `--enterprise-url` answers both questions at once, which is what makes a
/// Copilot login runnable with nobody at the keyboard.
#[test]
fn naming_the_enterprise_deployment_up_front_asks_nothing() {
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let mut child = ganja(&data, &issuer)
        .args([
            "auth",
            "login",
            "--provider",
            "github-copilot",
            "--method",
            "device",
            // Every spelling names the same deployment (`copilot.ts:15-17`).
            "--enterprise-url",
            "https://company.ghe.com/",
        ])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);
    let seen = printed_before(&lines, COPILOT_CODE);
    gate.open();
    let finished = finish(child, &lines, seen);

    assert!(finished.ok, "the login should have succeeded: {finished:?}");
    assert!(
        !finished.stderr.contains("deployment type"),
        "a deployment given up front is not asked about: {:?}",
        finished.stderr
    );
    assert_eq!(
        stored(&data)["github-copilot"]["enterpriseUrl"],
        "company.ghe.com"
    );
}

/// ChatGPT's device flow is not RFC 8628 — the pending signal is a status and
/// the *server* mints the PKCE verifier — so it is worth driving whole.
#[test]
fn a_chatgpt_device_login_stores_an_oauth_credential_under_openai() {
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let mut child = ganja(&data, &issuer)
        .args([
            "auth",
            "login",
            "--provider",
            "openai",
            "--method",
            "device",
        ])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);
    let seen = printed_before(&lines, CHATGPT_CODE);
    gate.open();
    let finished = finish(child, &lines, seen);

    assert!(finished.ok, "the login should have succeeded: {finished:?}");
    assert_eq!(stored(&data)["openai"]["type"], "oauth");
    assert_eq!(stored(&data)["openai"]["access"], ACCESS);
    leaks_nothing(&finished);
}

/// A login replacing a credential of the other kind is the hazard the shared
/// `openai` storage key creates, and the only place anybody can be warned about
/// it is here.
#[test]
fn a_chatgpt_login_says_what_the_stored_key_it_replaces_was() {
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let keyed = ganja(&data, &issuer)
        .args([
            "auth",
            "login",
            "--provider",
            "openai",
            "--key",
            "sk-replaced-1177",
        ])
        .output()
        .expect("the binary runs");
    assert!(keyed.status.success());

    let mut child = ganja(&data, &issuer)
        .args([
            "auth",
            "login",
            "--provider",
            "openai",
            "--method",
            "device",
        ])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);
    let seen = printed_before(&lines, CHATGPT_CODE);
    gate.open();
    let finished = finish(child, &lines, seen);

    assert!(finished.ok, "the login should have succeeded: {finished:?}");
    assert!(
        finished.stderr.contains("replaces the api credential")
            && finished.stderr.contains("****1177"),
        "the login has to name what it is about to overwrite: {:?}",
        finished.stderr
    );
    // And it really is gone, rather than shadowed.
    assert_eq!(stored(&data)["openai"]["type"], "oauth");
}

/// The same hazard the other way round: a pasted key replaces a login.
#[test]
fn a_pasted_key_says_what_the_stored_login_it_replaces_was() {
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let mut child = ganja(&data, &issuer)
        .args([
            "auth",
            "login",
            "--provider",
            "openai",
            "--method",
            "device",
        ])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);
    let seen = printed_before(&lines, CHATGPT_CODE);
    gate.open();
    assert!(finish(child, &lines, seen).ok, "the login should succeed");

    let keyed = ganja(&data, &issuer)
        .args([
            "auth",
            "login",
            "--provider",
            "openai",
            "--key",
            "sk-replacing-3355",
        ])
        .output()
        .expect("the binary runs");
    let said = String::from_utf8_lossy(&keyed.stderr).into_owned();

    assert!(keyed.status.success());
    assert!(
        said.contains("replaces the oauth credential") && said.contains("****7731"),
        "storing a key has to name the login it overwrites: {said:?}"
    );
    assert_eq!(stored(&data)["openai"]["type"], "api");
}

/// The wait is what a person has to be able to get out of, and getting out of
/// it must leave nothing behind.
///
/// Unix only: the way out is the interrupt signal, and `kill` is how this suite
/// sends one without the crate growing a dependency to do it.
#[cfg(unix)]
#[test]
fn an_interrupted_login_says_it_was_cancelled_and_stores_nothing() {
    // Never opened: the exchange hangs, which is what a login waiting on
    // somebody else's browser looks like from here.
    let gate = Gate::closed();
    let issuer = serve(&gate);
    let data = data();

    let mut child = ganja(&data, &issuer)
        .args(["auth", "login", "--provider", "grok", "--method", "device"])
        .spawn()
        .expect("the binary runs");
    let lines = watching(&mut child);
    let seen = printed_before(&lines, GROK_CODE);

    let sent = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(sent.success(), "the interrupt should have been delivered");

    let finished = finish(child, &lines, seen);
    assert!(
        !finished.ok,
        "a cancelled login is not a successful one: {finished:?}"
    );
    assert!(
        finished.stderr.contains("cancelled") && finished.stderr.contains("nothing was stored"),
        "a cancelled login has to say both: {:?}",
        finished.stderr
    );
    assert!(
        !stored_at(&data).exists(),
        "a cancelled login must leave no credential file behind"
    );
}

impl std::fmt::Debug for Finished {
    /// Hand-written so a failure prints what was said rather than one escaped
    /// line, and so that adding a field here can never start printing a token —
    /// [`leaks_nothing`] is what proves neither stream carries one.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "exited {}\n--- stdout\n{}\n--- stderr\n{}",
            if self.ok { "0" } else { "non-zero" },
            self.stdout,
            self.stderr
        )
    }
}
