//! Command-line surface of the `ganja` binary.
//!
//! Every credential assertion is on the redacted tail. A test that printed a
//! whole key would put it in CI output, which is the failure the redaction
//! exists to prevent.

use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::Path,
    thread,
};

use assert_cmd::Command;
use ganja_core::{SessionId, SessionInfo, Storage, storage::VERSION};
use ganja_permission::Project;
use ganja_protocol::Usage;
use predicates::prelude::*;
use tempfile::TempDir;

/// A key shaped like the real thing, planted so the tests can prove it never
/// comes back out whole.
const CANARY: &str = "sk-canary-8842";

/// Builds an invocation with its own data directory and no inherited keys, so
/// that a developer's exported `ANTHROPIC_API_KEY` cannot decide whether these
/// pass.
fn ganja(data: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command.env("XDG_DATA_HOME", data.path());
    // `auth login`/`logout` now validate a name that is not a builtin against
    // the loaded config, so a developer's global `ganja.jsonc` would otherwise
    // decide whether a provider exists here. The data home does for a config
    // home too: what matters is that it is not theirs.
    command.env("XDG_CONFIG_HOME", data.path());
    // The other two doors to a global home, closed with it: an exported
    // `GANJA_CONFIG_HOME` outranks the pinned XDG dir, and an empty pinned
    // XDG dir falls through to `~/.ganja` via `HOME`.
    command.env("HOME", data.path());
    command.env_remove("GANJA_CONFIG_HOME");
    command.env_remove("GANJA_CONFIG");
    for variable in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        command.env_remove(variable);
    }
    // A developer who left a login redirected would otherwise decide what the
    // key path in this file does; the flows are driven in `auth_login.rs`,
    // which sets this itself.
    command.env_remove("GANJA_AUTH_ISSUER");

    command
}

fn data() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

fn stored_at(data: &TempDir) -> std::path::PathBuf {
    data.path().join("ganja").join("auth.json")
}

#[test]
fn version_flag_reports_the_binary_name_and_version() {
    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .arg("--version")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ganja")
                .and(predicate::str::contains(env!("CARGO_PKG_VERSION"))),
        );
}

#[test]
fn a_key_given_on_the_command_line_is_stored_and_reported_redacted() {
    let data = data();

    ganja(&data)
        .args(["auth", "login", "--provider", "anthropic", "--key", CANARY])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("anthropic")
                .and(predicate::str::contains("****8842"))
                .and(predicate::str::contains(CANARY).not()),
        );

    assert!(
        stored_at(&data).is_file(),
        "the key should have been stored"
    );

    ganja(&data)
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("anthropic")
                .and(predicate::str::contains("****8842"))
                .and(predicate::str::contains("auth.json"))
                .and(predicate::str::contains(CANARY).not()),
        );
}

/// `pass show … | ganja auth login` has to work, which means a key arriving on
/// a pipe is read rather than prompted for.
#[test]
fn a_piped_key_is_read_from_standard_input() {
    let data = data();

    ganja(&data)
        .args(["auth", "login", "--provider", "openai"])
        .write_stdin(format!("{CANARY}\n"))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("openai")
                .and(predicate::str::contains("****8842"))
                .and(predicate::str::contains(CANARY).not()),
        );

    ganja(&data)
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("openai"));
}

#[cfg(unix)]
#[test]
fn a_stored_key_is_written_where_only_its_owner_can_read_it() {
    use std::os::unix::fs::PermissionsExt as _;

    let data = data();
    ganja(&data)
        .args(["auth", "login", "--key", CANARY])
        .assert()
        .success();

    let mode = std::fs::metadata(stored_at(&data))
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o600, "got {mode:04o}");
}

#[test]
fn an_empty_key_is_refused_rather_than_stored() {
    let data = data();

    ganja(&data)
        .args(["auth", "login", "--key", "   "])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no key"));

    assert!(
        !stored_at(&data).exists(),
        "a refused login should write nothing"
    );
}

#[test]
fn logging_out_forgets_the_key_and_says_so_when_there_was_none() {
    let data = data();
    ganja(&data)
        .args(["auth", "login", "--provider", "openai", "--key", CANARY])
        .assert()
        .success();

    ganja(&data)
        .args(["auth", "logout", "--provider", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("forgot"));

    ganja(&data)
        .args(["auth", "logout", "--provider", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no stored"));

    ganja(&data)
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no credentials"));
}

/// A stored key that an exported variable outranks is the one way a successful
/// login can change nothing, so the listing shows which is in use and the
/// login says so.
///
/// **Both rows, and the outranked one saying what beat it.** The listing used
/// to print only the winner, which made a credential it holds invisible to the
/// command whose whole job is saying what it holds — so the stored tail is
/// asserted here rather than asserted absent, and the marker is what keeps two
/// rows from reading as two credentials in use.
#[test]
fn an_environment_variable_outranks_the_stored_key_and_is_pointed_out() {
    let data = data();
    ganja(&data)
        .args(["auth", "login", "--provider", "anthropic", "--key", CANARY])
        .assert()
        .success();

    ganja(&data)
        .env("ANTHROPIC_API_KEY", "sk-environment-4242")
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ANTHROPIC_API_KEY")
                .and(predicate::str::contains("****4242"))
                .and(predicate::str::contains("****8842"))
                .and(predicate::str::contains("shadowed by ANTHROPIC_API_KEY"))
                .and(predicate::str::contains(CANARY).not()),
        );

    ganja(&data)
        .env("ANTHROPIC_API_KEY", "sk-environment-4242")
        .args(["auth", "login", "--provider", "anthropic", "--key", CANARY])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("ANTHROPIC_API_KEY").and(predicate::str::contains(
                "used in preference to the stored key",
            )),
        );
}

/// The listing has to say which *kind* of credential a row is, because a login
/// and a pasted key are stored under the same provider name for at least one
/// provider and behave nothing alike.
#[test]
fn the_listing_says_what_kind_of_credential_each_row_is() {
    let data = data();
    ganja(&data)
        .args(["auth", "login", "--provider", "anthropic", "--key", CANARY])
        .assert()
        .success();

    ganja(&data)
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("TYPE")
                .and(predicate::str::is_match(r"anthropic\s+api\s+\*{4}8842").expect("a pattern")),
        );
}

/// The deferral's blanket refusal narrowed when the login landed ahead of the
/// wire: what is refused now is the key cursor has nowhere to send, in every
/// spelling that could store one — named alongside the login it does have,
/// which is the half that makes it actionable.
///
/// The bare invocation is deliberately absent from this list: it *runs* the
/// OAuth login now, and that flow is driven end to end in `auth_login.rs`,
/// where the suite owns the issuer — no test may poll the real endpoints.
#[test]
fn a_cursor_key_is_refused_naming_the_login_cursor_has_and_stores_nothing() {
    let data = data();

    for arguments in [
        vec!["auth", "login", "--provider", "cursor", "--method", "api"],
        vec![
            "auth",
            "login",
            "--provider",
            "cursor",
            "--key",
            "not-a-real-key",
        ],
    ] {
        ganja(&data).args(&arguments).assert().failure().stderr(
            predicate::str::contains("cursor has no `api` login")
                .and(predicate::str::contains("`browser`")),
        );
    }

    assert!(
        !stored_at(&data).exists(),
        "a refused login must leave no credential file behind"
    );
}

#[test]
fn a_login_method_a_provider_does_not_have_is_refused_and_the_ones_it_has_are_named() {
    let data = data();

    // Only pairings this build really lacks belong here. A provider that has
    // the method would not be refused — it would start the login, and a browser
    // login binds a socket on a port fixed by somebody else's client
    // registration, which is not a thing a test may hold.
    for (provider, method, instead) in [
        // There is no Anthropic OAuth flow in the pin at all.
        ("anthropic", "device", "`api`"),
        ("anthropic", "browser", "`api`"),
        // Copilot's only OAuth flow is the device grant (`copilot.ts:182-185`).
        ("github-copilot", "browser", "`device` and `api`"),
    ] {
        ganja(&data)
            .args(["auth", "login", "--provider", provider, "--method", method])
            .assert()
            .failure()
            .stderr(
                predicate::str::contains(format!("{provider} has no `{method}` login"))
                    .and(predicate::str::contains(format!("it has {instead}"))),
            );
    }

    assert!(
        !stored_at(&data).exists(),
        "a refused login should write nothing"
    );
}

/// ganja calls the provider `grok`; the credential file calls it `xai`, which
/// is what an opencode install reading the same file expects. The key path has
/// to land there too, not only the login.
#[test]
fn a_grok_key_is_stored_under_the_name_the_credential_file_uses() {
    let data = data();

    ganja(&data)
        .args(["auth", "login", "--provider", "grok"])
        .write_stdin(format!("{CANARY}\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("****8842").and(predicate::str::contains(CANARY).not()));

    let written = fs::read_to_string(stored_at(&data)).expect("the key was stored");
    let parsed: serde_json::Value = serde_json::from_str(&written).expect("the store is JSON");
    assert_eq!(parsed["xai"]["type"], "api");
    assert!(
        parsed.get("grok").is_none(),
        "the command-line name is not a line in the file: {parsed}"
    );

    // And forgetting it reaches the same entry.
    ganja(&data)
        .args(["auth", "logout", "--provider", "grok"])
        .assert()
        .success()
        .stdout(predicate::str::contains("forgot"));
    ganja(&data)
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no credentials"));
}

/// The variable that redirects a login decides where a device code and then a
/// pair of tokens are sent, so anything that could name a host off this machine
/// is refused — and refused rather than ignored, because quietly using the real
/// issuer instead is the one outcome whoever set it cannot have wanted.
#[test]
fn a_redirected_login_that_could_leave_the_machine_is_refused() {
    let data = data();

    for origin in [
        "https://auth.x.ai",
        // Userinfo: everything before the `@` is discarded by a resolver, so
        // this names `elsewhere.example`.
        "http://127.0.0.1:80@elsewhere.example",
        "http://localhost.elsewhere.example:8080",
    ] {
        ganja(&data)
            .env("GANJA_AUTH_ISSUER", origin)
            .args(["auth", "login", "--provider", "grok", "--method", "device"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("GANJA_AUTH_ISSUER"));
    }

    assert!(
        !stored_at(&data).exists(),
        "a refused login should write nothing"
    );
}

/// A cache home of this test's own.
///
/// Load-bearing rather than tidy: the listing adopts whatever catalog is
/// cached under the cache home, so a run that inherited the developer's would
/// be asserting on whatever their last session happened to fetch.
fn cache() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// An invocation whose catalog is only what the test put in front of it: this
/// cache home, nothing fetched, and none of the developer's `GANJA_*` settings.
///
/// The variables are spelled out rather than imported from
/// `ganja_core::catalog` for the reason the pty suite spells its own out: what
/// a command-line test pins is the contract somebody types, and the contract
/// is the name.
fn offline(cache: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        .env("XDG_CACHE_HOME", cache.path())
        // The listing now asks the config whether a provider it has no rows
        // for is one a session could still run as, so the config home is as
        // much a part of what a test puts in front of it as the cache home is.
        // The same directory for both: what matters is that neither is the
        // developer's, and neither name collides inside it.
        .env("XDG_CONFIG_HOME", cache.path())
        // The cursor listing reads the credential store before it dials, so
        // the data home is part of what a test puts in front of the command
        // too — without this, a developer's real cursor login would send
        // `models cursor` to the network.
        .env("XDG_DATA_HOME", cache.path())
        .env_remove("GANJA_CONFIG")
        .env("GANJA_DISABLE_MODELS_FETCH", "1")
        .env_remove("GANJA_MODELS_URL")
        .env_remove("GANJA_MODELS_PATH");

    command
}

/// The same, with fetching on and pointed at `url` instead of at the published
/// endpoint — which this suite must never reach.
fn online(cache: &TempDir, url: &str) -> Command {
    let mut command = offline(cache);
    command
        .env("GANJA_MODELS_URL", url)
        .env_remove("GANJA_DISABLE_MODELS_FETCH");

    command
}

/// Where the default endpoint's catalog is cached, under a cache home.
fn cached_at(cache: &TempDir) -> std::path::PathBuf {
    cache.path().join("ganja").join("models.json")
}

/// A catalog in the shape the endpoint publishes, naming a provider no build
/// of this binary carries — so a row of it on screen can only have come from
/// the file it was written into.
const PLANTED: &str = r#"{
  "planted": {
    "id": "planted",
    "models": {
      "planted-one": {
        "id": "planted-one",
        "name": "Planted One",
        "cost": { "input": 3.0, "output": 15.0 },
        "limit": { "context": 321000, "output": 4000 }
      }
    }
  }
}"#;

/// Two more of the same, served in order, so a second fetch can be told apart
/// from a cache that was merely adopted.
const SERVED: [&str; 2] = [
    r#"{ "alpha": { "models": { "alpha-one": {
        "cost": { "input": 1.0, "output": 2.0 },
        "limit": { "context": 111000, "output": 1000 } } } } }"#,
    r#"{ "beta": { "models": { "beta-one": {
        "cost": { "input": 1.0, "output": 2.0 },
        "limit": { "context": 222000, "output": 2000 } } } } }"#,
];

/// Answers each connection with the next of `bodies`, repeating the last, and
/// says where it is listening.
///
/// A thread and the standard library's own listener rather than a runtime:
/// this suite drives a built binary, and the only thing it needs of an HTTP
/// server is that there is one.
fn serve(bodies: &'static [&'static str]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
    let url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );

    thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { return };

            read_head(&mut stream);
            let body = bodies[index.min(bodies.len() - 1)];
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                 {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    url
}

/// Reads a request head off `stream` and discards it, so the client is never
/// writing into a socket nobody is reading.
fn read_head(stream: &mut TcpStream) {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];

    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
}

/// A port nothing is listening on, which is what an endpoint that refuses a
/// connection looks like from here.
///
/// Bound and released rather than picked, because a number chosen by hand is a
/// number somebody else's service may already have.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");

    listener
        .local_addr()
        .expect("a bound socket has an address")
        .port()
}

#[test]
fn models_lists_the_catalog_and_marks_one_default_per_provider() {
    offline(&cache()).arg("models").assert().success().stdout(
        predicate::str::contains("PROVIDER")
            .and(predicate::str::contains("$/MTOK IN"))
            .and(predicate::str::contains("claude-opus-4-8*"))
            // The star follows `catalog::DEFAULTS`, and openai's is the newest
            // row again: the key wire speaks Responses, which is the endpoint
            // that model needed to serve tools.
            .and(predicate::str::contains("gpt-5.6*"))
            .and(predicate::str::contains("claude-haiku-4-5"))
            // The context window is compacted rather than spelled out.
            .and(predicate::str::contains("1.0M"))
            .and(predicate::str::contains("200.0k")),
    );
}

/// The cache is a layer somebody installs, not one a lookup reaches for — so
/// the listing has to install it, or it answers from the snapshot compiled
/// into the binary however recently a session fetched something newer.
#[test]
fn a_cached_catalog_is_what_the_listing_reflects() {
    let cache = cache();
    let file = cached_at(&cache);
    fs::create_dir_all(file.parent().expect("the cache file has a directory"))
        .expect("the cache directory is creatable");
    fs::write(&file, PLANTED).expect("the cache file is writable");

    offline(&cache).arg("models").assert().success().stdout(
        predicate::str::contains("planted-one")
            .and(predicate::str::contains("321.0k"))
            // A fetched catalog replaces the table rather than joining it,
            // which is also what makes this assertion mean anything.
            .and(predicate::str::contains("claude-sonnet-5").not()),
    );
}

/// Fetching switched off is an ordinary answer to "refresh", not a failure:
/// the tier below still has everything the question needed.
#[test]
fn a_refresh_with_fetching_switched_off_still_lists_and_says_why() {
    offline(&cache())
        .args(["models", "--refresh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-opus-4-8*"))
        .stderr(predicate::str::contains("GANJA_DISABLE_MODELS_FETCH"));
}

/// Neither is an endpoint that cannot be reached. A `--refresh` that exited
/// non-zero over a refused connection would make a table that is merely stale
/// look like no table at all.
#[test]
fn a_refresh_the_network_refuses_degrades_to_the_table_already_in_hand() {
    let cache = cache();
    let nowhere = format!("http://127.0.0.1:{}", closed_port());

    online(&cache, &nowhere)
        .args(["models", "--refresh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-opus-4-8*"))
        .stderr(predicate::str::contains("not refreshed"));
}

/// `--refresh` fetches, and it fetches *past* the debounce a background
/// refresh honours — otherwise asking for the newest catalog would answer with
/// whatever was fetched in the last five minutes.
#[test]
fn a_forced_refresh_fetches_past_the_cache_it_just_wrote() {
    let cache = cache();
    let url = serve(&SERVED);

    // Nothing is cached yet, so these rows can only have come off the socket.
    online(&cache, &url)
        .args(["models", "--refresh"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("alpha-one")
                .and(predicate::str::contains("claude-sonnet-5").not()),
        );

    // The cache that fetch wrote is far fresher than the debounce, so a second
    // fetch happened because this run forced one.
    online(&cache, &url)
        .args(["models", "--refresh"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("beta-one").and(predicate::str::contains("alpha-one").not()),
        );

    // And with fetching off there is nowhere else the second catalog could be
    // read from: what the fetch wrote, the next run adopts.
    offline(&cache)
        .env("GANJA_MODELS_URL", &url)
        .arg("models")
        .assert()
        .success()
        .stdout(predicate::str::contains("beta-one"));
}

#[test]
fn a_named_provider_is_the_only_one_the_listing_carries() {
    let cache = cache();

    offline(&cache)
        .args(["models", "anthropic"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("PROVIDER")
                .and(predicate::str::contains("claude-opus-4-8*"))
                .and(predicate::str::contains("gpt-5.6").not()),
        );

    offline(&cache)
        .args(["models", "openai"])
        .assert()
        .success()
        // The starred row is openai's default, `gpt-5.6`; the previous default
        // stays listed beside it, unstarred — it is still what a ChatGPT seat
        // runs.
        .stdout(
            predicate::str::contains("gpt-5.6*")
                .and(predicate::str::contains("gpt-5.4 "))
                .and(predicate::str::contains("claude").not()),
        );
}

/// A header over no rows would read as "this provider serves nothing", which
/// is indistinguishable from the typo it usually is.
#[test]
fn a_provider_this_table_does_not_carry_is_named_rather_than_listed_as_empty() {
    offline(&cache())
        .args(["models", "gemini"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("PROVIDER").not())
        .stderr(
            predicate::str::contains("gemini")
                .and(predicate::str::contains("anthropic"))
                .and(predicate::str::contains("openai")),
        );
}

/// A config file declaring one endpoint, in a project directory of its own.
///
/// Written as `ganja.json` in a directory carrying a checkout marker, which is
/// where discovery's project tier looks — the same file a person would write.
fn declaring_project() -> TempDir {
    let directory = project();
    fs::write(
        directory.path().join("ganja.json"),
        r#"{"provider": {"local-llama": {
             "dialect": "openai-chat-completions",
             "base_url": "http://127.0.0.1:11434/v1",
             "key_env": "LOCAL_LLAMA_KEY"
           }}}"#,
    )
    .expect("the config file is writable");

    directory
}

/// A provider a session **can** run as, with no rows in the table, is not the
/// typo the refusal above is about: it is the uncataloged tier, and what it
/// needs is the consequence spelled out rather than an error or a bare header.
#[test]
fn a_selectable_provider_with_no_rows_says_what_it_gives_up_instead_of_failing() {
    let cache = cache();
    let project = declaring_project();

    // A configured endpoint: selectable because this project's own config
    // declares it, and cataloged by nothing.
    offline(&cache)
        .current_dir(project.path())
        .args(["models", "local-llama"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("PROVIDER")
                .and(predicate::str::contains("no catalog rows"))
                .and(predicate::str::contains("sizing and cost display are off"))
                .and(predicate::str::contains("GANJA_MODEL")),
        );

    // And a builtin in the same tier — shipped, selectable, deliberately
    // unpriced — answers the same way, because the tier is a fact about the
    // table rather than about where the provider came from.
    offline(&cache)
        .current_dir(project.path())
        .args(["models", "fake"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no catalog rows"));

    // Cursor left this tier's note behind: its wire carries a roster of its
    // own, so `models cursor` is the live listing — or its refusal — and is
    // pinned in its own test below.

    // The typo keeps the refusal it always had: this project declares no such
    // endpoint, so nothing could run as it.
    offline(&cache)
        .current_dir(project.path())
        .args(["models", "local-lama"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("PROVIDER").not());
}

/// The cursor roster is the wire's to serve, so `models cursor` asks the
/// stored login before it asks anything else — and with none stored, what
/// comes back is the wire's own refusal naming the repair, with the catalog
/// machinery never consulted. No network is dialled on this path: the
/// credential read fails first, which is what lets an offline test pin it.
#[test]
fn the_cursor_listing_without_a_login_is_refused_naming_the_login() {
    offline(&cache())
        .args(["models", "cursor"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("PROVIDER").not())
        .stderr(predicate::str::contains("ganja auth login cursor"));
}

/// A ChatGPT seat's roster is the binary's, not the table's (**D476**), so
/// `models openai` on a stored login prints the pinned five and a header that
/// says pinned — the wording is the whole point, because cursor's "live from
/// the wire; uncataloged" would be false twice over about this list.
///
/// Offline in the strong sense: fetching is off, every home is this test's, and
/// the seat arm reaches nothing anyway. What proves membership is not the
/// catalog's is `gpt-5.6-sol` — no row of this build's table carries it, and it
/// is listed regardless.
#[test]
fn the_openai_listing_on_a_chatgpt_login_is_the_pinned_roster_under_a_pinned_header() {
    let cache = cache();
    write_chatgpt_login(&cache);

    offline(&cache)
        // The environment key outranks a stored login, so a developer's
        // exported one would send this command to the platform arm and the
        // catalog listing.
        .env_remove("OPENAI_API_KEY")
        .args(["models", "openai"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("openai models, pinned to what a ChatGPT subscription is")
                .and(predicate::str::contains("--refresh does not apply"))
                .and(predicate::str::contains("live from the wire").not())
                .and(predicate::str::contains("gpt-5.5"))
                .and(predicate::str::contains("gpt-5.6-sol"))
                .and(predicate::str::contains("gpt-5.6-terra"))
                .and(predicate::str::contains("gpt-5.6-luna"))
                .and(predicate::str::contains("gpt-5.3-codex-spark"))
                // The catalog header, and the two rows a seat is not offered:
                // this listing is the roster alone.
                .and(predicate::str::contains("PROVIDER").not())
                .and(predicate::str::contains("gpt-5.4 ").not()),
        );
}

/// A stored ChatGPT credential under a temporary data home, in the shape
/// `ganja auth login` writes one. The tokens are inert: the listing path this
/// arranges presents them to nobody.
fn write_chatgpt_login(data: &TempDir) {
    let path = stored_at(data);
    fs::create_dir_all(path.parent().expect("the store lives in a directory"))
        .expect("the store directory is creatable");
    fs::write(
        &path,
        r#"{"openai": {"type": "oauth", "refresh": "rt-seat-fixture",
             "access": "at-seat-fixture", "expires": 4102444800000}}"#,
    )
    .expect("the fixture writes");

    // The store refuses a credential file other users can read.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("the fixture is made private");
    }
}

/// A key for an endpoint the config declares is stored under the id its entry
/// was written under, which is exactly where a session reads it.
#[test]
fn a_config_declared_provider_can_be_logged_into_and_out_of_by_its_own_name() {
    let data = data();
    let project = declaring_project();

    ganja(&data)
        .current_dir(project.path())
        .args([
            "auth",
            "login",
            "--provider",
            "local-llama",
            "--key",
            CANARY,
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("local-llama")
                .and(predicate::str::contains("****8842"))
                .and(predicate::str::contains(CANARY).not()),
        );

    ganja(&data)
        .current_dir(project.path())
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("local-llama"));

    // An OAuth flow is a set of endpoints written per provider, and a config
    // entry supplies none of them — so naming one is refused rather than
    // attempted against nothing.
    ganja(&data)
        .current_dir(project.path())
        .args([
            "auth",
            "login",
            "--provider",
            "local-llama",
            "--method",
            "device",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("key"));

    ganja(&data)
        .current_dir(project.path())
        .args(["auth", "logout", "--provider", "local-llama"])
        .assert()
        .success()
        .stdout(predicate::str::contains("forgot the stored local-llama"));

    // A name this project's config does not declare is refused with both tiers
    // named, because somebody who mistyped their own entry has to see what
    // they actually wrote.
    ganja(&data)
        .current_dir(project.path())
        .args(["auth", "login", "--provider", "local-lama", "--key", CANARY])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("local-lama")
                .and(predicate::str::contains("local-llama"))
                .and(predicate::str::contains("anthropic")),
        );
}

/// A project with nothing stored yet is what every project is on its first
/// run, and the store is created lazily — so "there is no store directory" and
/// "there are no sessions" are the same situation, and it has to read as an
/// invitation rather than as a failure.
///
/// The working directory is pinned as well as the data home because `sessions`
/// resolves its store from the directory it was run in. Inheriting the
/// runner's would make this a question about *this* checkout's project, which
/// is empty here only because the data home happens to be redirected too;
/// naming both is what makes the empty store structural rather than incidental.
#[test]
fn listing_sessions_in_a_project_with_none_invites_rather_than_fails() {
    let data = data();
    let project = TempDir::new().expect("a temporary directory is creatable");

    ganja(&data)
        .current_dir(project.path())
        .arg("sessions")
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions here yet"));
}

/// A project directory pinned by a checkout marker, so the store a run writes
/// into is this directory's rather than that of whatever the temporary
/// directory happens to sit inside.
fn project() -> TempDir {
    let directory = TempDir::new().expect("a temporary directory is creatable");
    fs::create_dir(directory.path().join(".git")).expect("the checkout marker is creatable");

    directory
}

/// The store a run in `project` with its state under `data` reads.
///
/// Composed rather than asked of `Project::data_dir`, which resolves the data
/// home from *this* process's environment while the run under test is given
/// its own. The slug is the part that has to agree between the two, and that
/// is what `Project` answers here.
fn session_storage(data: &TempDir, project: &Path) -> Storage {
    Storage::open(
        data.path()
            .join("ganja")
            .join("project")
            .join(Project::resolve(project).slug())
            .join("storage"),
    )
}

/// Stores a session, which is a delegated one when `parent` names the session
/// whose `task` call spawned it.
fn store(storage: &Storage, id: &str, parent: Option<&str>) {
    storage
        .save_info(&SessionInfo {
            effort: None,
            id: SessionId::from(id.to_owned()),
            version: VERSION,
            title: Some(format!("work of {id}")),
            created: 1,
            updated: 1,
            usage: Usage::default(),
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            parent: parent.map(|parent| SessionId::from(parent.to_owned())),
            revert: None,
        })
        .expect("a session stores");
}

/// A subagent's session is rendered on the tool-call row that spawned it, and
/// resuming into one would open a delegated turn with nothing on screen saying
/// what asked for it — so the listing shows roots, exactly as the picker in
/// `ganja-tui` does.
#[test]
fn a_session_a_task_call_spawned_is_not_listed_beside_the_one_that_asked_for_it() {
    let data = data();
    let project = project();
    let storage = session_storage(&data, project.path());
    store(&storage, "ses_root", None);
    store(&storage, "ses_delegated", Some("ses_root"));

    ganja(&data)
        .current_dir(project.path())
        .arg("sessions")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ses_root")
                .and(predicate::str::contains("ses_delegated").not()),
        );
}

/// And a project holding nothing but delegated sessions has nothing to list,
/// which has to read as the same invitation an empty store does rather than as
/// a table with no rows under it.
#[test]
fn a_project_whose_every_session_is_delegated_reads_as_one_with_none() {
    let data = data();
    let project = project();
    let storage = session_storage(&data, project.path());
    store(&storage, "ses_delegated", Some("ses_root"));

    ganja(&data)
        .current_dir(project.path())
        .arg("sessions")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("no sessions here yet")
                .and(predicate::str::contains("SESSION").not()),
        );
}

/// A first run has nothing to say on stderr.
///
/// The binary creates its log directory before the appender opens it, because
/// the appender prunes old files as it opens — it *reads* the directory first,
/// and a directory nothing has created yet makes it complain. The complaint is
/// harmless and looks anything but, and it lands on the one run where a user
/// has the least context for judging it: the first one in a new project.
///
/// A fresh data home is what makes this a first run, so the assertion is on
/// the run's own silence rather than on any string — nothing this binary means
/// to say belongs on stderr when nothing went wrong.
#[test]
fn a_first_run_in_a_fresh_data_home_says_nothing_on_stderr() {
    let data = data();
    let project = TempDir::new().expect("a temporary directory is creatable");

    ganja(&data)
        .current_dir(project.path())
        .arg("sessions")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

/// Configuration mistakes have to be reported before the terminal is put into
/// raw mode, or the message is drawn over and lost.
#[test]
fn an_unknown_provider_is_refused_before_the_terminal_is_taken_over() {
    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .env("GANJA_PROVIDER", "definitely-not-a-provider")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("GANJA_PROVIDER")
                .and(predicate::str::contains("definitely-not-a-provider")),
        );
}

/// A provider with no credential anywhere is refused with the command that
/// fixes it, which is the whole point of storing keys.
#[test]
fn a_provider_without_a_credential_is_refused_and_says_how_to_fix_it() {
    let data = data();

    ganja(&data)
        .env("GANJA_PROVIDER", "anthropic")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("ANTHROPIC_API_KEY")
                .and(predicate::str::contains("ganja auth login")),
        );
}

#[test]
fn an_unknown_subcommand_is_refused() {
    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .arg("definitely-not-a-subcommand")
        .assert()
        .failure();
}
