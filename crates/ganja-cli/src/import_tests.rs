use super::{
    At, BUILTIN_IDS, Built, Config, Json, Report, guard, map_config, parse, permission,
    reachable_in_the_clear, tokens, validate,
};

/// One opencode config holding every shape the mapping has a rule for.
/// Shared with `tests/import_opencode.rs`, which drives the same file
/// through the built binary.
const FIXTURE: &str = include_str!("../tests/fixtures/opencode.jsonc");

/// The table rows as borrowed pairs, which is the shape the assertions
/// read in.
fn rows(rows: &[(String, String)]) -> Vec<(&str, &str)> {
    rows.iter()
        .map(|(left, right)| (left.as_str(), right.as_str()))
        .collect()
}

fn imported(text: &str) -> (Built, Report) {
    map_config(&parse(text).expect("the fixture is JSONC"))
}

/// The accept criterion: one config in, and the table plus the file it
/// produces, in full. Written out rather than spot-checked because the
/// table *is* the command's output — a row that quietly changed shape is a
/// user being told something different about their config.
#[test]
fn the_fixture_maps_agents_commands_permissions_and_leaves_the_rest_named() {
    let (built, report) = imported(FIXTURE);

    assert_eq!(
        rows(&report.mapped),
        vec![
            ("model", "model"),
            ("default_agent", "default_agent"),
            ("shell", "shell"),
            ("theme", "theme"),
            ("instructions", "instructions"),
            // The legacy `tools` map lands in `permission`, with `write`
            // naming the edit permission…
            ("tools.webfetch", "permission.webfetch"),
            // …and the explicit rules below winning the tools they name.
            ("permission.bash", "permission.bash"),
            ("permission.edit", "permission.edit"),
            ("permission.read", "permission.read"),
            ("agent.review.model", "agent.review.model"),
            ("agent.review.description", "agent.review.description"),
            ("agent.review.mode", "agent.review.mode"),
            ("agent.review.tools.edit", "agent.review.permission.edit"),
            (
                "agent.review.permission.webfetch",
                "agent.review.permission.webfetch"
            ),
            ("command.release.template", "command.release.template"),
            ("command.release.description", "command.release.description"),
            ("command.release.agent", "command.release.agent"),
            // A provider entry is upstream's shape flattened: the SDK
            // becomes the wire, and `options` becomes the entry itself.
            ("provider.local-llama.npm", "provider.local-llama.dialect"),
            ("provider.local-llama.options", "provider.local-llama"),
            (
                "provider.local-llama.options.baseURL",
                "provider.local-llama.base_url",
            ),
            (
                "provider.local-llama.options.headers",
                "provider.local-llama.headers",
            ),
            // An MCP entry is ganja's shape already, `type` included.
            ("mcp.fs.type", "mcp.fs.type"),
            ("mcp.fs.command", "mcp.fs.command"),
            ("mcp.fs.cwd", "mcp.fs.cwd"),
            ("mcp.fs.environment", "mcp.fs.environment"),
            ("mcp.fs.enabled", "mcp.fs.enabled"),
            ("mcp.fs.timeout", "mcp.fs.timeout"),
            ("mcp.docs.type", "mcp.docs.type"),
            ("mcp.docs.url", "mcp.docs.url"),
            ("mcp.docs.headers", "mcp.docs.headers"),
            // A builtin ganja ships too, one of opencode's that describes
            // itself whole, and a server of the user's own. The entry that
            // leans on a definition this build does not have is the only
            // one missing.
            ("lsp.rust.disabled", "lsp.rust.disabled"),
            ("lsp.deno.command", "lsp.deno.command"),
            ("lsp.deno.extensions", "lsp.deno.extensions"),
            ("lsp.nickel.command", "lsp.nickel.command"),
            ("lsp.nickel.extensions", "lsp.nickel.extensions"),
            ("lsp.nickel.env", "lsp.nickel.env"),
            ("lsp.nickel.initialization", "lsp.nickel.initialization"),
            ("snapshot", "snapshot"),
            // A `mode` entry becomes an agent that only the user can pick.
            ("mode.ship.prompt", "agent.ship.prompt"),
            ("mode.ship.hidden", "agent.ship.hidden"),
            ("mode.ship", "agent.ship.mode"),
        ]
    );

    assert_eq!(
        rows(&report.skipped),
        vec![
            ("$schema", "unpublished"),
            // Nothing but a token, so carrying it would name a model that
            // does not exist.
            ("small_model", "token"),
            // Ganja has both keys; neither holds what opencode puts in
            // them, so they are refused by name rather than half-mapped.
            ("keybinds", "incompatible"),
            ("tui", "incompatible"),
            ("instructions[1]", "token"),
            ("instructions[3]", "unsupported"),
            ("tools.write", "overridden"),
            ("tools.bash", "overridden"),
            ("agent.review.temperature", "unsupported"),
            ("agent.review.top_p", "unsupported"),
            ("agent.review.steps", "unsupported"),
            ("agent.review.color", "unsupported"),
            ("agent.review.variant", "unsupported"),
            ("agent.review.options", "unsupported"),
            ("command.release.variant", "unsupported"),
            ("command.release.subtask", "unsupported"),
            ("provider.anthropic", "refused"),
            ("provider.anthropic.options.apiKey", "credential"),
            ("provider.local-llama.models", "catalog"),
            ("provider.local-llama.options.organization", "unsupported"),
            ("mcp.docs.oauth", "unsupported"),
            ("mcp.legacy", "malformed"),
            ("lsp.typescript", "unsupported"),
            ("compaction", "deferred"),
            ("autoshare", "unsupported"),
            ("username", "unsupported"),
            ("definitely_not_an_opencode_key", "unknown"),
        ]
    );

    // Two orders are visible here and only one of them means anything. The
    // document's own keys precede its tables — which is why `snapshot`, added
    // last, is written sixth — and that is TOML's layout rule rather than a
    // decision of this module's. What the module *does* decide is the order
    // inside `[permission]`: `webfetch` before `bash` before `edit` before
    // `read`, none of it alphabetical, all of it the order the source spelled
    // and the order last-match-wins evaluation reads.
    let rendered = built.document().to_string();
    assert_eq!(
        rendered,
        r#"model = "anthropic/claude-sonnet-5"
default_agent = "plan"
theme = "tokyonight"
shell = "/bin/zsh"
instructions = ["AGENTS.md", "docs/{env:TEAM}/style.md"]
snapshot = false

[permission]
webfetch = "deny"
bash = { "git status" = "allow", "git *" = "ask", "*" = "deny" }
edit = "ask"
read = "allow"

[agent.review]
model = "anthropic/claude-haiku-4-5"
description = "reads a diff and complains"
mode = "subagent"
permission = { edit = "deny", webfetch = "allow" }

[agent.ship]
prompt = "You ship what is already green."
mode = "primary"
hidden = false

[command.release]
template = "cut a release for $ARGUMENTS"
description = "tag and push"
agent = "build"

[mcp.fs]
type = "local"
command = ["mcp-fs", "--root", "."]
cwd = "./servers"
environment = { MCP_FS_MODE = "ro" }
enabled = true
timeout = 45000

[mcp.docs]
type = "remote"
url = "https://mcp.example.invalid/mcp"
headers = { Authorization = "Bearer {env:DOCS_TOKEN}" }

[provider.local-llama]
dialect = "openai-chat-completions"
base_url = "http://127.0.0.1:11434/v1"
headers = { x-route = "gpu-0" }

[lsp.rust]
disabled = true

[lsp.deno]
command = ["deno", "lsp"]
extensions = [".ts", ".tsx"]

[lsp.nickel]
command = ["nls"]
extensions = [".ncl"]
env = { NICKEL_LOG = "info" }
initialization = { eval = { limit = 500 } }
"#
    );

    validate(&rendered).expect("what the importer writes has to load");
}

/// The one value in an opencode config that must never travel. Its own
/// test, because the assertion is about what is *absent* — and an absence
/// is exactly what a refactor takes away without noticing.
#[test]
fn an_api_key_is_never_written_and_is_pointed_at_the_credential_store() {
    // Two entries, because the key's row has to survive both endings an
    // entry can have: one that was refused whole, and one that was
    // written. A key carried into a config file this command produced
    // would be the one place it could sit in the clear.
    let (built, report) = imported(
        r#"{"provider": {
                "anthropic": {"options": {"apiKey": "sk-canary-8842"}},
                "local-llama": {
                    "npm": "@ai-sdk/openai-compatible",
                    "options": {"apiKey": "sk-canary-4471", "baseURL": "https://a.test/v1"}
                }
            }}"#,
    );

    assert_eq!(
        rows(&report.skipped),
        vec![
            ("provider.anthropic", "refused"),
            ("provider.anthropic.options.apiKey", "credential"),
            ("provider.local-llama.options.apiKey", "credential"),
        ]
    );

    let rendered = built.document().to_string();
    for canary in ["sk-canary-8842", "sk-canary-4471"] {
        assert!(
            !rendered.contains(canary),
            "a key reached the written config: {rendered}"
        );
    }
    assert!(
        rendered.contains("local-llama") && !rendered.contains("key_env"),
        "the entry is written, and nothing invents the variable holding its \
             key: {rendered}"
    );

    let credentials: Vec<&String> = report
        .warnings
        .iter()
        .filter(|warning| warning.contains("ganja auth login"))
        .collect();
    assert_eq!(credentials.len(), 2, "{:?}", report.warnings);
    for warning in credentials {
        assert!(
            !warning.contains("sk-canary-8842") && !warning.contains("sk-canary-4471"),
            "the warning must not repeat the key: {warning}"
        );
    }
}

/// The mapping's shape: what upstream spells across `npm` and `options`
/// becomes one flat entry, and `endpoint` wins over `baseURL` because
/// upstream's own read does.
#[test]
fn a_provider_entry_this_build_has_a_wire_for_is_carried_flattened() {
    let (built, report) = imported(
        r#"{"provider": {"gateway": {
                "npm": "@ai-sdk/anthropic",
                "name": "Gateway Inc",
                "options": {
                    "baseURL": "https://ignored.test",
                    "endpoint": "https://gateway.test/v1",
                    "headers": {"x-route": "eu"}
                }
            }}}"#,
    );

    assert_eq!(
        built.document().to_string(),
        "[provider.gateway]\n\
             dialect = \"anthropic-messages\"\n\
             base_url = \"https://gateway.test/v1\"\n\
             headers = { x-route = \"eu\" }\n"
    );
    assert_eq!(
        rows(&report.mapped),
        vec![
            ("provider.gateway.npm", "provider.gateway.dialect"),
            ("provider.gateway.options", "provider.gateway"),
            (
                "provider.gateway.options.baseURL",
                "provider.gateway.base_url"
            ),
            (
                "provider.gateway.options.endpoint",
                "provider.gateway.base_url"
            ),
            (
                "provider.gateway.options.headers",
                "provider.gateway.headers"
            ),
        ]
    );
    assert_eq!(
        rows(&report.skipped),
        vec![("provider.gateway.name", "catalog")]
    );
}

/// The row that used to be refused as unsupported: the vendor's own SDK
/// drives the Responses API, which a config-named endpoint now speaks as
/// its own dialect.
#[test]
fn a_responses_sdk_entry_is_carried_under_the_responses_dialect() {
    let (built, report) = imported(
        r#"{"provider": {"proxy": {
                "npm": "@ai-sdk/openai",
                "options": {"baseURL": "https://responses.test/v1"}
            }}}"#,
    );

    assert_eq!(
        built.document().to_string(),
        "[provider.proxy]\n\
             dialect = \"openai-responses\"\n\
             base_url = \"https://responses.test/v1\"\n"
    );
    assert_eq!(
        rows(&report.mapped),
        vec![
            ("provider.proxy.npm", "provider.proxy.dialect"),
            ("provider.proxy.options", "provider.proxy"),
            ("provider.proxy.options.baseURL", "provider.proxy.base_url"),
        ]
    );
    assert!(rows(&report.skipped).is_empty(), "{:?}", report.skipped);
}

/// Nothing is completed on a config's behalf. Each of these describes an
/// endpoint this build could not talk to, and each is named rather than
/// half-written.
#[test]
fn a_provider_entry_this_build_cannot_talk_to_is_named_rather_than_guessed_at() {
    let cases = [
        // No SDK this build has a wire for: the dialect is not derivable,
        // and guessing it sends one API's body to the other's endpoint.
        (
            r#"{"provider": {"x": {"npm": "@ai-sdk/google",
                   "options": {"baseURL": "https://a.test"}}}}"#,
            "unsupported",
            "@ai-sdk/google",
        ),
        (
            r#"{"provider": {"x": {"options": {"baseURL": "https://a.test"}}}}"#,
            "unsupported",
            "which API",
        ),
        // opencode takes the endpoint from the SDK it loads; there is
        // nothing here to take one from.
        (
            r#"{"provider": {"x": {"npm": "@ai-sdk/openai-compatible"}}}"#,
            "unsupported",
            "no endpoint",
        ),
        // ganja's own config refuses this at load, so writing it would
        // produce a file the next launch will not read.
        (
            r#"{"provider": {"x": {"npm": "@ai-sdk/openai-compatible",
                   "options": {"baseURL": "http://gateway.test/v1"}}}}"#,
            "refused",
            "https",
        ),
    ];

    for (source, reason, said) in cases {
        let (built, report) = imported(source);

        assert!(built.is_empty(), "{source} should have been left out");
        assert_eq!(
            rows(&report.skipped),
            vec![("provider.x", reason)],
            "{source}"
        );
        assert!(
            report.warnings.iter().any(|warning| warning.contains(said)),
            "{source}: {:?}",
            report.warnings
        );
    }
}

/// Upstream expands these textually before parsing; ganja expands nothing,
/// so the two cases have to be told apart — a value that *is* a token would
/// otherwise become a literal `{env:…}` model id, and a value that merely
/// contains one would vanish.
#[test]
fn a_value_that_is_only_a_token_is_left_out_and_one_that_contains_it_is_carried() {
    let mut report = Report::default();

    assert_eq!(guard(&mut report, "model", "{env:MODEL}"), None);
    assert_eq!(guard(&mut report, "shell", " {file:/etc/shell} "), None);
    assert_eq!(
        guard(&mut report, "instructions[0]", "docs/{env:TEAM}/x.md"),
        Some("docs/{env:TEAM}/x.md".to_owned())
    );
    assert_eq!(
        guard(&mut report, "model", "anthropic/claude-sonnet-5"),
        Some("anthropic/claude-sonnet-5".to_owned())
    );

    assert_eq!(
        rows(&report.skipped),
        vec![("model", "token"), ("shell", "token")]
    );
    assert_eq!(report.warnings.len(), 3, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("{env:MODEL}"));
    assert!(report.warnings[2].contains("{env:TEAM}"));
}

#[test]
fn every_token_in_a_value_is_found_and_none_is_invented() {
    let cases: [(&str, Vec<&str>); 6] = [
        ("plain", vec![]),
        ("{env:A}", vec!["{env:A}"]),
        ("{file:./a.md}", vec!["{file:./a.md}"]),
        ("x{env:A}y{file:b}z", vec!["{env:A}", "{file:b}"]),
        // An opener with no close is not a token, and must not eat the rest.
        ("{env:A", vec![]),
        ("${SHELL}", vec![]),
    ];

    for (value, expected) in cases {
        assert_eq!(tokens(value), expected, "scanning {value:?}");
    }
}

/// Order is the whole semantics of `permission`: evaluation is
/// last-match-wins, so a rule that moved is a rule that stopped applying.
#[test]
fn the_legacy_tools_map_keeps_its_positions_and_loses_the_tools_named_twice() {
    let source = parse(
        r#"{
              "tools": {"webfetch": false, "patch": true, "bash": true},
              "permission": {"bash": {"git *": "allow"}, "read": "allow"}
            }"#,
    )
    .expect("the fixture is JSONC");
    let mut report = Report::default();

    let permission = permission(
        &mut report,
        &At::root(),
        source.get("tools"),
        source.get("permission"),
    )
    .expect("both halves fold into one value");

    assert_eq!(
        permission,
        Json::Object(vec![
            ("webfetch".to_owned(), Json::String("deny".to_owned())),
            // `patch` names the edit permission, the way upstream folds it.
            ("edit".to_owned(), Json::String("allow".to_owned())),
            (
                "bash".to_owned(),
                Json::Object(vec![("git *".to_owned(), Json::String("allow".to_owned()))])
            ),
            ("read".to_owned(), Json::String("allow".to_owned())),
        ])
    );
    assert_eq!(rows(&report.skipped), vec![("tools.bash", "overridden")]);
}

/// A bare action replaces everything under it rather than merging into it,
/// which is upstream's `mergeDeep` refusing to recurse into a string.
#[test]
fn a_bare_permission_action_wins_over_the_whole_legacy_tools_map() {
    let (built, report) =
        imported(r#"{"tools": {"bash": true, "webfetch": false}, "permission": "ask"}"#);

    assert_eq!(
        built.permission,
        Some(Json::String("ask".to_owned())),
        "{:?}",
        built.permission
    );
    assert_eq!(
        rows(&report.skipped),
        vec![
            ("tools.bash", "overridden"),
            ("tools.webfetch", "overridden")
        ]
    );
}

/// Reading a config nobody wrote for this importer means every unknown key
/// is a row, never a failure — and a value of the wrong type is a row too,
/// because refusing the whole file over one line would make the command
/// useless exactly when it is most wanted.
#[test]
fn an_unknown_key_and_a_value_of_the_wrong_type_are_reported_rather_than_fatal() {
    let (built, report) =
        imported(r#"{"model": 42, "shell": "/bin/sh", "sparkles": {"deep": [1]}, "agent": "no"}"#);

    assert_eq!(built.shell.as_deref(), Some("/bin/sh"));
    assert_eq!(rows(&report.mapped), vec![("shell", "shell")]);
    assert_eq!(
        rows(&report.skipped),
        vec![
            ("model", "malformed"),
            ("sparkles", "unknown"),
            ("agent", "malformed"),
        ]
    );
}

/// A command with no template could not be loaded back, so it is not
/// written at all rather than written broken.
#[test]
fn a_command_without_a_template_is_not_written() {
    let (built, report) = imported(
        r#"{"command": {"ship": {"description": "no template"}, "cut": {"template": "cut $1"}}}"#,
    );

    assert_eq!(
        built
            .command
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["cut"]
    );
    assert_eq!(rows(&report.skipped), vec![("command.ship", "malformed")]);
}

/// A config whose every key is one ganja has no home for writes nothing,
/// and the rows say why rather than the command claiming success.
#[test]
fn a_config_of_nothing_but_skipped_keys_produces_no_file() {
    let (built, report) = imported(r#"{"plugin": [], "share": "auto", "autoupdate": false}"#);

    assert!(built.is_empty());
    assert!(report.mapped.is_empty(), "{:?}", report.mapped);
    assert_eq!(report.skipped.len(), 3);
}

/// An entry with no `type` describes no server ganja could connect to, and
/// the shape it usually takes is somebody switching off a server another
/// tier defined — which is why the warning says there is nothing here to
/// switch off, rather than leaving a user to conclude it worked.
#[test]
fn an_mcp_entry_with_no_type_writes_no_server_and_says_what_it_was() {
    let (built, report) = imported(r#"{"mcp": {"legacy": {"enabled": false}}}"#);

    assert!(built.mcp.is_empty());
    assert_eq!(rows(&report.skipped), vec![("mcp.legacy", "malformed")]);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].contains("nothing to switch off"),
        "{}",
        report.warnings[0]
    );
}

/// A server is written whole or not at all: ganja needs the `command` (or
/// the `url`) and refuses an entry without one, so an import that wrote the
/// rest of it would produce a file the next launch will not read.
#[test]
fn an_mcp_entry_missing_the_field_that_names_the_server_is_left_out() {
    let cases: [(&str, &str); 4] = [
        (r#"{"type": "local"}"#, "malformed"),
        (r#"{"type": "local", "command": []}"#, "malformed"),
        (r#"{"type": "remote"}"#, "malformed"),
        (r#"{"type": "remote", "url": 8080}"#, "malformed"),
    ];

    for (entry, reason) in cases {
        let (built, report) = imported(&format!(r#"{{"mcp": {{"one": {entry}}}}}"#));

        assert!(built.mcp.is_empty(), "{entry} was written");
        assert_eq!(rows(&report.skipped), vec![("mcp.one", reason)], "{entry}");
        assert!(!report.warnings.is_empty(), "{entry} was left out silently");
    }
}

/// A command is one invocation rather than a list of independent entries,
/// so an argument that is only a token cannot be dropped the way an
/// instruction path is: what would run is a different program.
#[test]
fn a_command_argument_that_is_only_a_token_leaves_the_whole_server_out() {
    let (built, report) = imported(
        r#"{"mcp": {"fs": {"type": "local", "cwd": "./here", "command": ["node", "{env:SERVER}"]}}}"#,
    );

    assert!(built.mcp.is_empty());
    // The rows the entry's other fields earned go with it: a `mapped` row
    // under a server that was never written would name a setting its author
    // still believes is in force.
    assert!(report.mapped.is_empty(), "{:?}", report.mapped);
    assert_eq!(rows(&report.skipped), vec![("mcp.fs", "token")]);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("{env:SERVER}")),
        "{:?}",
        report.warnings
    );
}

/// The rule ganja applies to a provider's base URL, applied to the one
/// place an MCP entry puts a token. The value is never repeated back: a URL
/// may carry a credential in its userinfo.
#[test]
fn a_remote_server_ganja_would_not_send_headers_to_is_left_out_unquoted() {
    let (built, report) = imported(
        r#"{"mcp": {"docs": {"type": "remote", "url": "http://mcp.example.invalid/mcp",
               "headers": {"Authorization": "Bearer canary-4417"}}}}"#,
    );

    assert!(built.mcp.is_empty());
    assert_eq!(rows(&report.skipped), vec![("mcp.docs", "refused")]);
    for said in report.warnings.iter().chain(
        report
            .skipped
            .iter()
            .map(|(key, _)| key)
            .chain(report.mapped.iter().map(|(key, _)| key)),
    ) {
        assert!(
            !said.contains("mcp.example.invalid") && !said.contains("canary-4417"),
            "the refusal repeated what it refused: {said}"
        );
    }
}

/// Which endpoints travel. Conservative on purpose: this decides whether to
/// carry an entry, and `ganja_core` parses the URL properly and is the
/// authority — a "no" here costs a row somebody can act on, where a wrong
/// "yes" costs a config file that does not load.
#[test]
fn only_an_endpoint_this_can_prove_is_https_or_this_machine_is_carried() {
    let cases: [(&str, bool); 13] = [
        ("https://mcp.example.invalid/mcp", true),
        ("https://mcp.example.invalid:8443/mcp", true),
        // A scheme is case-insensitive, and ganja's reader lowercases it.
        ("HTTPS://mcp.example.invalid/mcp", true),
        ("http://localhost:3000/mcp", true),
        ("http://127.0.0.1/mcp", true),
        ("http://user:p@ss@127.0.0.1:9/mcp", true),
        ("http://[::1]:8080/mcp", true),
        ("http://example.invalid/mcp", false),
        // A hostname somebody else can register, which is the whole reason
        // the host is parsed rather than matched as text.
        ("http://127.0.0.1.example.invalid/mcp", false),
        // Short-form IPv4 is a loopback address ganja's URL reader resolves
        // and this one does not, so it is left out rather than guessed at.
        ("http://127.1/mcp", false),
        ("ws://localhost/mcp", false),
        ("mcp-fs", false),
        ("", false),
    ];

    for (url, carried) in cases {
        assert_eq!(reachable_in_the_clear(url), carried, "judging {url:?}");
    }
}

/// Ganja types the budget as a `NonZeroU64`, so anything that is not a
/// positive whole number is a row rather than an import that fails on one
/// line of somebody else's config — and so is anything above what a TOML
/// integer holds, which would otherwise reach the file as a float.
#[test]
fn an_mcp_timeout_is_carried_only_when_it_is_a_positive_whole_number() {
    let cases: [(&str, bool); 8] = [
        ("45000", true),
        ("1", true),
        ("9223372036854775807", true),
        ("9223372036854775808", false),
        ("0", false),
        ("-1", false),
        ("1.5", false),
        (r#""45000""#, false),
    ];

    for (spelled, carried) in cases {
        let (built, report) = imported(&format!(
            r#"{{"mcp": {{"fs": {{"type": "local", "command": ["s"], "timeout": {spelled}}}}}}}"#
        ));

        assert_eq!(
            built.document().to_string().contains("timeout = "),
            carried,
            "carrying {spelled}"
        );
        assert_eq!(
            report.skipped.is_empty(),
            carried,
            "reporting {spelled}: {:?}",
            report.skipped
        );
    }
}

/// Ganja refuses an unknown field inside an MCP entry by name, so one
/// carried across would be a config file that does not load. It is reported
/// where it was written and dropped.
#[test]
fn a_field_an_mcp_entry_does_not_have_is_reported_rather_than_written() {
    let (built, report) = imported(
        r#"{"mcp": {"fs": {"type": "local", "command": ["s"], "sparkles": true,
               "url": "https://elsewhere.invalid"}}}"#,
    );

    let rendered = built.document().to_string();
    assert!(!rendered.contains("sparkles"), "{rendered}");
    assert!(
        !rendered.contains("elsewhere.invalid"),
        "a remote field on a local server is not a field: {rendered}"
    );
    assert_eq!(
        rows(&report.skipped),
        vec![("mcp.fs.sparkles", "unknown"), ("mcp.fs.url", "unknown")]
    );
    validate(&rendered).expect("what survived is still a config ganja reads");
}

/// Decoding is not the whole of what the loader accepts, so neither is
/// this check: `McpServer::check` runs over every entry after the decode,
/// which is the one authority the loader and `ganja mcp add` also call.
///
/// Both shapes below decode perfectly — an empty `command` is a legal
/// `Vec<String>`, a zero `output_limit` a legal `u64` — and both are
/// files the next launch would refuse, which is the one thing a writer
/// exists to prevent.
/// The refusal names what went wrong and where, and never the line it
/// happened on.
///
/// `toml_edit`'s own `Display` reproduces the offending line with a caret
/// under it, and the line a translated document fails on may be an `mcp`
/// entry's `headers` — the one place a bearer token is spelled, whose values
/// this build withholds even from `ganja mcp get`. The message is built from
/// the accessors instead, so the bytes cannot ride out through a terminal
/// somebody is sharing or a log somebody keeps.
#[test]
fn a_document_that_will_not_decode_is_named_by_position_and_never_by_its_bytes() {
    // One line carrying both the credential and the mistake, which is what
    // separates the two renderings: `Display` reproduces the line and hands
    // over the token beside the error, and the accessors hand over neither.
    let document = concat!(
        "[mcp]\n",
        "vendor = { type = \"remote\", url = \"https://example.test\", ",
        "headers = { Authorization = \"Bearer NEVER-PRINT-ME\" }, enabled = 1 }\n",
    );

    let refused = validate(document).expect_err("`enabled` is a boolean, so this does not decode");
    let said = refused.to_string();

    assert!(said.contains("line 2"), "{said}");
    assert!(said.contains("column"), "{said}");
    assert!(!said.contains("NEVER-PRINT-ME"), "{said}");
}

#[test]
fn a_written_mcp_entry_that_decodes_but_would_not_load_is_still_refused() {
    for (document, named) in [
        (
            "[mcp.fs]\ntype = \"local\"\ncommand = []\n",
            "empty command",
        ),
        (
            "[mcp.fs]\ntype = \"local\"\ncommand = [\"s\"]\noutput_limit = 0\n",
            "output_limit of 0",
        ),
    ] {
        let refused =
            validate(document).expect_err("the next launch would refuse this, so this run does");
        let said = refused.to_string();
        assert!(said.contains(named) && said.contains("\"fs\""), "{said}");
    }
}

/// `true` means something narrower here than it does upstream, and a user
/// who is not told that will conclude their language servers are running.
#[test]
fn lsp_true_is_carried_and_names_the_servers_this_build_actually_ships() {
    let (built, report) = imported(r#"{"lsp": true}"#);

    assert_eq!(built.lsp, Some(Json::Bool(true)));
    assert_eq!(rows(&report.mapped), vec![("lsp", "lsp")]);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    for shipped in BUILTIN_IDS {
        assert!(
            report.warnings[0].contains(shipped),
            "{}",
            report.warnings[0]
        );
    }

    // `false` is the same answer on both sides — no language server at all
    // — so it travels without anything to say about it.
    let (built, report) = imported(r#"{"lsp": false}"#);
    assert_eq!(built.lsp, Some(Json::Bool(false)));
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

/// opencode ships a definition for thirty-eight language servers and ganja
/// ships two, so what decides an entry naming one of the other thirty-six
/// is how much of that definition it was relying on — not the name.
///
/// An entry that names its own `command` *and* its own `extensions` is a
/// whole server description already: ganja loads it, starts it, and asks it
/// about the files it said, which is what it did upstream. An entry naming
/// less was leaning on the builtin, and the builtin is not in this build.
#[test]
fn an_upstream_language_server_travels_when_it_describes_itself_whole() {
    let cases: [(&str, Vec<&str>); 6] = [
        // Upstream lets a command stand alone because the extensions come
        // from the builtin; here they would come from nowhere.
        (
            r#"{"typescript": {"command": ["typescript-language-server", "--stdio"]}}"#,
            vec![],
        ),
        (r#"{"typescript": {"extensions": [".ts"]}}"#, vec![]),
        // Nothing to switch off: this build ships no `typescript`.
        (r#"{"typescript": {"disabled": true}}"#, vec![]),
        // Leaning on nothing, so it travels under its own name.
        (
            r#"{"deno": {"command": ["deno", "lsp"], "extensions": [".ts"]}}"#,
            vec!["deno"],
        ),
        // A builtin this build *does* ship needs neither field.
        (r#"{"gopls": {"disabled": true}}"#, vec!["gopls"]),
        (
            r#"{"typescript": {"command": ["tsserver"], "extensions": [".ts"]},
                   "vue": {"command": ["vls"]}}"#,
            vec!["typescript"],
        ),
    ];

    for (entries, kept) in cases {
        let (built, report) = imported(&format!(r#"{{"lsp": {entries}}}"#));

        let names: Vec<&str> = built
            .lsp
            .as_ref()
            .and_then(Json::as_object)
            .map(|entries| entries.iter().map(|(name, _)| name.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(names, kept, "mapping {entries}");

        // Whatever did not travel is named, in the row and in the line that
        // says what to do about it.
        for (key, _) in &report.skipped {
            let name = key.rsplit('.').next().expect("a row names a key");
            assert!(
                report.warnings.iter().any(|warning| warning.contains(name)),
                "{name} was dropped without a word: {:?}",
                report.warnings
            );
        }
        assert!(
            report
                .skipped
                .iter()
                .all(|(_, reason)| reason == "unsupported"),
            "mapping {entries}: {:?}",
            report.skipped
        );
    }
}

/// The two shapes ganja refuses at load, refused here instead — and never
/// repaired: a command invented on an entry's behalf would start a program
/// nobody chose.
#[test]
fn a_language_server_ganja_could_not_start_is_left_out_rather_than_completed() {
    let cases: [(&str, Vec<&str>); 6] = [
        // A server this build ships no definition for has nothing to
        // inherit its extensions from.
        (r#"{"nickel": {"command": ["nls"]}}"#, vec![]),
        // Only a disabled entry may leave out its command.
        (r#"{"nickel": {"extensions": [".ncl"]}}"#, vec![]),
        (r#"{"rust": {}}"#, vec![]),
        (r#"{"rust": {"disabled": true}}"#, vec!["rust"]),
        (r#"{"nickel": {"disabled": true}}"#, vec!["nickel"]),
        // An empty extension list is legal and means every file, which is
        // why it is not refused the way an empty command is.
        (
            r#"{"nickel": {"command": ["nls"], "extensions": []}}"#,
            vec!["nickel"],
        ),
    ];

    for (entries, kept) in cases {
        let (built, _) = imported(&format!(r#"{{"lsp": {entries}}}"#));

        let names: Vec<&str> = built
            .lsp
            .as_ref()
            .and_then(Json::as_object)
            .map(|entries| entries.iter().map(|(name, _)| name.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(names, kept, "mapping {entries}");
    }
}

/// An empty map is not "no servers": ganja merges a map *over* the servers
/// it ships, so `{}` would start them. Writing nothing is the only spelling
/// that means what a map nothing survived means.
#[test]
fn an_lsp_map_nothing_survives_writes_no_key_at_all() {
    let (built, report) = imported(r#"{"lsp": {"pyright": {"command": ["pyright"]}}}"#);

    assert_eq!(built.lsp, None);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("empty map would start")),
        "{:?}",
        report.warnings
    );
}

/// Absent means on, on both sides, so only `false` is worth carrying — and
/// it has to be carried, because absent would switch snapshots back on and
/// `/undo` would then restore files its author had told opencode not to
/// track.
#[test]
fn snapshot_travels_as_the_boolean_it_is() {
    let cases: [(&str, Option<bool>); 3] = [
        (r#"{"snapshot": false}"#, Some(false)),
        (r#"{"snapshot": true}"#, Some(true)),
        (r#"{"snapshot": "yes"}"#, None),
    ];

    for (source, expected) in cases {
        let (built, _) = imported(source);

        assert_eq!(built.snapshot, expected, "mapping {source}");
    }
}

/// Comments and trailing commas are legal in every opencode config file,
/// whatever its extension says.
#[test]
fn comments_and_trailing_commas_are_part_of_the_dialect() {
    let (built, _) = imported(
        r#"{
              // the model this project talks to
              "model": "anthropic/claude-sonnet-5",
              /* and nothing else */
            }"#,
    );

    assert_eq!(built.model.as_deref(), Some("anthropic/claude-sonnet-5"));
}

#[test]
fn a_file_holding_nothing_is_an_empty_config_rather_than_an_error() {
    for text in ["", "   \n  ", "// nothing but a comment\n"] {
        assert_eq!(
            parse(text).expect("an empty config file is legal"),
            Json::Object(Vec::new()),
            "parsing {text:?}"
        );
    }
}

#[test]
fn a_malformed_file_says_where_it_stopped() {
    let error = parse(r#"{"model": }"#).expect_err("a broken config file is fatal");

    assert!(error.to_string().contains("line 1"), "{error}");
}

/// The writer is the only thing between a value and a file that has to
/// parse again, so the characters that would end the literal early get
/// their own case.
///
/// Which escape each one takes is `toml_edit`'s business rather than this
/// module's, so what is asserted is the property that was always the point:
/// a value written and read back is the value that went in. Each case is
/// spelled twice on the way in — once as a value, once as the permission
/// pattern that is the only place a user's own text becomes a *key* — since
/// a quote ends a key as readily as it ends a value.
#[test]
fn a_written_string_comes_back_as_itself() {
    let cases = [
        "plain",
        "say \"hi\"",
        r"back\slash",
        "two\nlines",
        "a\tb",
        "bell\u{7}",
        // The file is UTF-8, so text outside ASCII travels as itself rather
        // than as `\u` pairs — either way it has to survive.
        "ずっと",
    ];

    for value in cases {
        let quoted = serde_json::to_string(value).expect("a JSON string literal");
        let (built, _) = imported(&format!(
            r#"{{"agent": {{"build": {{"prompt": {quoted}}}}},
                 "permission": {{"bash": {{{quoted}: "allow"}}}}}}"#
        ));

        let rendered = built.document().to_string();
        let read: Config = toml_edit::de::from_str(&rendered)
            .unwrap_or_else(|error| panic!("writing {value:?} produced {rendered}: {error}"));

        assert_eq!(
            read.agent["build"].prompt.as_deref(),
            Some(value),
            "a value carrying {value:?} came back changed: {rendered}"
        );
        assert_eq!(
            read.permission
                .rules()
                .into_iter()
                .map(|rule| rule.pattern)
                .collect::<Vec<_>>(),
            vec![value.to_owned()],
            "a key carrying {value:?} came back changed: {rendered}"
        );
    }
}

/// TOML has no way to write a `null`, and nothing may be invented in its
/// place — so the key is left out and named, which is the answer this
/// command gives to everything else it cannot carry.
///
/// Both places one can reach: a permission rule, and a value inside an
/// `initialization` block, which travels as the document it is.
#[test]
fn a_null_is_left_out_and_named_rather_than_written_as_something_else() {
    let (built, report) = imported(
        r#"{
              "permission": {"bash": null, "edit": "ask"},
              "lsp": {"nickel": {"command": ["nls"], "extensions": [".ncl"],
                      "initialization": {"eval": null, "limit": 500}}}
            }"#,
    );

    assert_eq!(
        rows(&report.skipped),
        vec![
            ("permission.bash", "null"),
            ("lsp.nickel.initialization.eval", "null"),
        ]
    );

    let rendered = built.document().to_string();
    assert!(!rendered.contains("null"), "{rendered}");
    assert_eq!(
        rendered,
        "[permission]\n\
             edit = \"ask\"\n\
             \n\
             [lsp.nickel]\n\
             command = [\"nls\"]\n\
             extensions = [\".ncl\"]\n\
             initialization = { limit = 500 }\n"
    );
    validate(&rendered).expect("what survived is still a config ganja reads");
}

/// The boundary of what TOML holds, from both sides.
///
/// `i64::MAX` is a number the destination has room for and travels as the
/// digits that were read. One more than that is a number it has no room for,
/// and the only two things that could happen to it are a reported row and a
/// silently different value — a failed integer parse falling through to a
/// float writes `9.223372036854776e18`, which then reaches a language server
/// as though somebody had asked for it. So the row is asserted *and* the
/// float spelling is asserted absent, since a test that only checked the row
/// would still pass if the number were carried wrongly beside it.
///
/// The untyped `initialization` block is the one place an out-of-range number
/// can reach: every number ganja gives a type to goes through
/// `positive_integer`, which already refuses what it cannot parse as a
/// positive `i64`.
#[test]
fn a_whole_number_wider_than_toml_holds_is_named_rather_than_rounded() {
    let (built, report) = imported(
        r#"{
              "lsp": {"nickel": {"command": ["nls"], "extensions": [".ncl"],
                      "initialization": {"big": 9223372036854775807,
                                         "bigger": 9223372036854775808,
                                         "fraction": 1.5}}}
            }"#,
    );

    assert_eq!(
        rows(&report.skipped),
        vec![("lsp.nickel.initialization.bigger", "range")],
        "the one it cannot hold, and only that one"
    );

    let rendered = built.document().to_string();
    assert!(
        rendered.contains("big = 9223372036854775807"),
        "the largest whole number TOML holds travels as its own digits:\n{rendered}"
    );
    assert!(
        rendered.contains("fraction = 1.5"),
        "a number written as a fraction still goes through a float:\n{rendered}"
    );
    assert!(
        !rendered.contains("9.223372036854776e18") && !rendered.contains("bigger"),
        "nothing is written in place of the one that would not fit:\n{rendered}"
    );
    validate(&rendered).expect("what survived is still a config ganja reads");
}

/// Everything the importer can emit has to survive the reader that will
/// pick it up, including the values that carry escapes.
#[test]
fn what_the_importer_writes_is_what_ganja_reads() {
    let (built, _) = imported(
        r#"{
              "model": "anthropic/claude-sonnet-5",
              "agent": {"build": {"prompt": "line\none\t\"quoted\"", "disable": false}},
              "permission": {"bash": {"echo \"hi\"": "allow"}},
              "command": {"ship": {"template": "ship $ARGUMENTS"}}
            }"#,
    );

    let rendered = built.document().to_string();
    validate(&rendered).expect("the escaped values survive the round trip");
}
