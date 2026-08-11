//! `ganja mcp`: what the listing says about servers this build actually dialled.
//!
//! # Why the server here is a socket and not a process
//!
//! The engine's own MCP suite (`ganja-core/tests/mcp.rs`) drives a reference
//! server built on `@modelcontextprotocol/sdk`, and pays for that with two hard
//! prerequisites: `bun`, and an upstream checkout with its dependencies
//! installed. What is under test *here* is a listing — which servers were
//! configured, where connecting to each got, and which tools came back — and
//! reaching that needs a peer that speaks the protocol, not a peer that is
//! somebody else's implementation of it. A loopback endpoint answering
//! `initialize` and `tools/list` is exactly that peer, and it leaves this crate
//! with no prerequisite it did not already have and no child process to leak.
//!
//! The remote transport's own correctness is not this file's claim: that is
//! pinned against a real socket beside the engine, in the same shape this
//! endpoint is written in.

use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A project of this test's own, pinned by a checkout marker.
///
/// Load-bearing twice over: the config tiers are discovered by walking up from
/// the working directory, so a run that inherited the runner's would be
/// listing whichever servers *this* checkout configures, and the marker is
/// what stops the walk before it reaches them.
fn project() -> TempDir {
    let directory = TempDir::new().expect("a temporary directory is creatable");
    std::fs::create_dir(directory.path().join(".git")).expect("the checkout marker is creatable");

    directory
}

/// An invocation of `ganja mcp` in `project`, reading `config` and nothing else.
///
/// Every credential is removed rather than merely unused: an MCP server is a
/// peer of the session and not of the model provider, so this command must run
/// for somebody who has never logged in, and a test that inherited a
/// developer's exported key could not tell that apart.
fn listing(project: &TempDir, data: &TempDir, config: Option<&std::path::Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        .current_dir(project.path())
        .env("XDG_DATA_HOME", data.path())
        // The global config home is pinned with the data home — a developer's
        // real `ganja.jsonc` can declare MCP servers of its own, which is
        // exactly what "reading `config` and nothing else" must exclude.
        .env("HOME", data.path())
        .env("XDG_CONFIG_HOME", data.path().join("config"))
        .env_remove("GANJA_CONFIG_HOME")
        .arg("mcp");
    for variable in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        command.env_remove(variable);
    }
    match config {
        Some(path) => command.env("GANJA_CONFIG", path),
        None => command.env_remove("GANJA_CONFIG"),
    };

    command
}

/// Writes `config` where `GANJA_CONFIG` can name it, and answers the path.
fn config_file(directory: &TempDir, config: &Value) -> std::path::PathBuf {
    let path = directory.path().join("ganja.json");
    std::fs::write(&path, config.to_string()).expect("the config file is writable");

    path
}

/// A configured server of each kind there is an outcome for: one that answers,
/// one that cannot be started, and one that was told not to be.
///
/// The three together are what make the listing's claim checkable — a report
/// that said the same thing about every server would be indistinguishable from
/// a correct one if only a connected server were configured.
#[test]
fn the_listing_names_each_server_its_standing_and_the_tools_it_lends() {
    let project = project();
    let data = TempDir::new().expect("a temporary directory is creatable");
    let url = endpoint();
    let path = config_file(
        &project,
        &json!({
            "mcp": {
                "hub": { "type": "remote", "url": url },
                // A program no machine has, so the connect fails at the spawn
                // rather than at a timeout this test would have to wait out.
                "broken": { "type": "local", "command": ["ganja-no-such-program-8842"] },
                "off": { "type": "local", "command": ["never-run"], "enabled": false },
            }
        }),
    );

    listing(&project, &data, Some(&path))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("SERVER")
                .and(predicate::str::contains("STATUS"))
                .and(predicate::str::contains("ADDRESS"))
                // Reached, and lending the tool it advertised under the name
                // the model would call it by.
                .and(predicate::str::contains("hub"))
                .and(predicate::str::contains("connected"))
                .and(predicate::str::contains(&url))
                .and(predicate::str::contains("mcp__hub__ping"))
                // Not reached, and the reason is quoted rather than swallowed.
                .and(predicate::str::contains("failed"))
                .and(predicate::str::contains("ganja-no-such-program-8842"))
                // Never dialled, which is not an error.
                .and(predicate::str::contains("disabled")),
        );
}

/// The TOOLS column names how many tools a connected server lends, and says
/// nothing countable for one that cannot be lending any.
#[test]
fn the_listing_names_a_tool_count_beside_each_servers_standing() {
    let project = project();
    let data = TempDir::new().expect("a temporary directory is creatable");
    let url = endpoint();
    let path = config_file(
        &project,
        &json!({
            "mcp": {
                "hub": { "type": "remote", "url": url },
                "broken": { "type": "local", "command": ["ganja-no-such-program-8842"] },
                "off": { "type": "local", "command": ["never-run"], "enabled": false },
            }
        }),
    );

    listing(&project, &data, Some(&path))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("TOOLS")
                // The one server that lent a tool (`hub`, the endpoint above
                // advertises exactly one: `ping`) is the only row that can
                // print a count of "1".
                .and(predicate::str::is_match(r"hub\s+connected\s+1\s+").unwrap()),
        );
}

/// Each server's standing is its own, which is the whole claim of the listing.
///
/// Asserted by counting rather than by containment: "connected" appearing
/// somewhere proves nothing about the two servers that are not, and a listing
/// that reported one standing for everybody would satisfy every containment
/// assertion above.
#[test]
fn a_standing_is_reported_per_server_and_not_once_for_all_of_them() {
    let project = project();
    let data = TempDir::new().expect("a temporary directory is creatable");
    let url = endpoint();
    let path = config_file(
        &project,
        &json!({
            "mcp": {
                "hub": { "type": "remote", "url": url },
                "broken": { "type": "local", "command": ["ganja-no-such-program-8842"] },
                "off": { "type": "local", "command": ["never-run"], "enabled": false },
            }
        }),
    );

    let output = listing(&project, &data, Some(&path))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed = String::from_utf8(output).expect("the listing is UTF-8");

    let counted = |word: &str| listed.matches(word).count();
    assert_eq!(counted("connected"), 1, "{listed}");
    assert_eq!(counted("disabled"), 1, "{listed}");
    assert_eq!(counted("failed"), 1, "{listed}");
    // A tool belongs to the server that lent it, so the one connected server's
    // row is the only one carrying any.
    assert_eq!(counted("mcp__"), 1, "{listed}");
}

/// A project configuring none is every project until somebody configures one,
/// so it has to read as an invitation rather than as a table with no rows —
/// the shape `sessions` answers the same question in.
#[test]
fn a_project_with_no_configured_servers_invites_rather_than_lists() {
    let project = project();
    let data = TempDir::new().expect("a temporary directory is creatable");

    listing(&project, &data, None)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("no MCP servers configured")
                .and(predicate::str::contains("SERVER").not()),
        )
        // Nothing was dialled and nothing went wrong, so there is nothing to
        // say here either.
        .stderr(predicate::str::is_empty());
}

/// An invocation of `ganja mcp login <server>` in `project`, isolated the
/// same way [`listing`] is — a login neither reads nor writes a developer's
/// real credential store or config home.
fn login(
    project: &TempDir,
    data: &TempDir,
    config: Option<&std::path::Path>,
    server: &str,
) -> Command {
    let mut command = listing(project, data, config);
    command.args(["login", server]);

    command
}

/// The three ways `ganja mcp login` refuses without reaching a network: the
/// name is not configured, it names a local server (oauth is remote-only),
/// or it is remote but names no `oauth` — each is a config-validation
/// question `mcp_login_command` answers before anything about the flow
/// itself. What discovery, registration and the exchange do once a login
/// actually starts is pinned in `ganja-provider`'s own unit tests and in
/// `ganja-core/tests/mcp_oauth.rs`'s end-to-end run through the credential
/// store — this file's own job is the listing and this validation, not the
/// wire.
#[test]
fn a_login_is_refused_by_name_before_it_reaches_a_network() {
    let project = project();
    let data = TempDir::new().expect("a temporary directory is creatable");
    let path = config_file(
        &project,
        &json!({
            "mcp": {
                "hub": { "type": "remote", "url": "https://mcp.example/mcp" },
                "fs": { "type": "local", "command": ["ganja-no-such-program-8842"] },
            }
        }),
    );

    login(&project, &data, Some(&path), "nowhere")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"nowhere\" is not configured"));

    login(&project, &data, Some(&path), "fs")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"fs\"").and(predicate::str::contains("local server")));

    login(&project, &data, Some(&path), "hub")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no `oauth` configured"));
}

/// A loopback endpoint speaking streamable HTTP, answered by its URL.
///
/// A thread per connection and the standard library's own listener, because
/// this suite drives a built binary and the only thing it needs of an HTTP
/// server is that there is one.
///
/// The connection is served in a **loop** rather than answered once: the
/// transport posts the `initialize` request and then the `initialized`
/// notification, and a server that hung up in between would look to it exactly
/// like a connection that failed — which is the one outcome this test must be
/// able to tell apart from success.
fn endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
    let url = format!(
        "http://{}/mcp",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            thread::spawn(move || serve(stream));
        }
    });

    url
}

/// Answers every request that arrives on one connection until it closes.
fn serve(mut stream: TcpStream) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];

    loop {
        // The body length is read out of the headers rather than guessed,
        // because a POST arrives in as many reads as it likes.
        let (head, body) = loop {
            if let Some(request) = whole(&buffer) {
                break request;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        buffer.drain(..head.len() + 4 + body.len());

        let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let response = match answer(&request) {
            Some(answer) => {
                let body = answer.to_string();
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\n\r\n{body}",
                    body.len()
                )
            }
            // A notification is acknowledged and nothing more.
            None => "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_owned(),
        };
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

/// One whole request out of `buffer` — its head and its body — or [`None`]
/// while the bytes are still arriving.
fn whole(buffer: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(buffer).ok()?;
    let (head, rest) = text.split_once("\r\n\r\n")?;
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    if rest.len() < length {
        return None;
    }

    Some((head.to_owned(), rest[..length].to_owned()))
}

/// What the endpoint answers one JSON-RPC request with, or [`None`] for a
/// notification.
///
/// `tools/call` is deliberately absent: a listing never calls anything, and a
/// handler for a request that cannot arrive would be a claim this test does not
/// make.
fn answer(request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request.get("method")?.as_str()?;

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "hub", "version": "0.0.0" },
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "ping",
                "description": "Answers over HTTP.",
                "inputSchema": { "type": "object", "properties": {} },
            }],
        }),
        _ => json!({}),
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}
