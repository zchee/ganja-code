//! Static acceptance-image inputs and their effective shell permission rules.

use ganja_core::Config;
use ganja_core::permission::{Decision, Permissions};
use serde_json::{Value, json};

const DOCKERFILE: &str = include_str!("../../ganja-teammate-cloud/worker/Dockerfile");
const POLICY: &str = include_str!("../../ganja-teammate-cloud/worker/ganja.toml");
const SCRIPT: &str = include_str!("../../ganja-teammate-cloud/worker/acceptance-script.json");

fn permissions() -> Permissions {
    let config: Config = toml::from_str(POLICY).expect("real image policy is valid configuration");
    let mut permissions = Permissions::default();
    permissions.set_baseline(config.permission.rules());
    permissions
}

fn script_commands() -> Vec<String> {
    let script: Value = serde_json::from_str(SCRIPT).expect("real acceptance script is valid JSON");
    let mut commands = Vec::new();
    for turn in script["turns"].as_array().expect("script turns") {
        for call in turn["tool_calls"].as_array().into_iter().flatten() {
            assert_eq!(call["name"], "bash", "the fixed acceptance only needs shell commands");
            let command = call["args"]["command"].as_str().expect("shell command");
            // This fixed script uses only AND lists. Validate every extracted
            // command with the existing POSIX lexer, not an ad-hoc quote parser.
            for command in command.split(" && ") {
                let words = shlex::split(command).expect("complete quoted shell command");
                assert!(!words.is_empty());
                assert!(
                    !words.iter().any(|word| matches!(word.as_str(), "&&" | ";" | "|")),
                    "unsupported command separator in the fixed script: {command}"
                );
                commands.push(command.to_owned());
            }
        }
    }
    assert!(!commands.is_empty(), "the acceptance must execute commands");
    commands
}

#[test]
fn the_acceptance_policy_allows_each_required_command() {
    let permissions = permissions();
    let commands = script_commands();
    assert_eq!(commands.len(), 4, "append, stage, commit and push remain required");
    for command in &commands {
        assert_eq!(
            permissions.gate("bash", &json!({"command": command, "workdir": "/work"})).action,
            Decision::Allow,
            "required command: {command}"
        );
    }
    let script: Value = serde_json::from_str(SCRIPT).expect("script");
    for turn in script["turns"].as_array().expect("turns") {
        for call in turn["tool_calls"].as_array().into_iter().flatten() {
            assert_eq!(permissions.gate("bash", &call["args"]).action, Decision::Allow);
        }
    }
}

#[test]
fn the_acceptance_policy_rejects_unrelated_commands() {
    let permissions = permissions();
    for command in [
        "git status",
        "printf unrelated",
        "git add -- other.txt",
        "git push other HEAD",
        "git add -- NOTES.md && git status",
    ] {
        assert_eq!(
            permissions.gate("bash", &json!({"command": command})).action,
            Decision::Deny,
            "unrelated command: {command}"
        );
    }
    assert_eq!(permissions.gate("write", &json!({"filePath": "NOTES.md"})).action, Decision::Deny);
}

#[test]
fn the_acceptance_policy_rejects_unapproved_git_configuration() {
    let permissions = permissions();
    for command in [
        "git -c color.ui=false status",
        "git -c commit.gpgsign=true commit -m 'docs: record hosted worker acceptance'",
        "git -c commit.gpgsign=false -c color.ui=false commit -m 'docs: record hosted worker acceptance'",
        "git -c commit.gpgsign=false status",
        "git -c commit.gpgsign=false commit -m unrelated",
    ] {
        assert_eq!(
            permissions.gate("bash", &json!({"command": command})).action,
            Decision::Deny,
            "unapproved configuration: {command}"
        );
    }
}

#[test]
fn the_acceptance_policy_rejects_git_alias_execution() {
    let permissions = permissions();
    for command in [
        "git acceptance",
        "git -c alias.acceptance=status acceptance",
        "git -c commit.gpgsign=false -c alias.acceptance=status acceptance",
    ] {
        assert_eq!(
            permissions.gate("bash", &json!({"command": command})).action,
            Decision::Deny,
            "alias command: {command}"
        );
    }
}

#[test]
fn the_worker_image_requires_pinned_supply_inputs() {
    check_supply_inputs(DOCKERFILE);
}

fn active_instructions(source: &str) -> Vec<(String, String)> {
    let mut instructions = Vec::new();
    let mut logical = String::new();
    for line in source.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let continued = line.ends_with('\\');
        let part = line.strip_suffix('\\').unwrap_or(line).trim_end();
        if !logical.is_empty() {
            logical.push(' ');
        }
        logical.push_str(part);
        if !continued {
            let (name, arguments) =
                logical.split_once(char::is_whitespace).expect("instruction arguments");
            instructions.push((name.to_ascii_uppercase(), arguments.trim().to_owned()));
            logical.clear();
        }
    }
    assert!(logical.is_empty(), "unterminated Dockerfile continuation");
    instructions
}

fn instruction_position(instructions: &[(String, String)], name: &str, arguments: &str) -> usize {
    instructions
        .iter()
        .position(|(kind, body)| kind == name && body == arguments)
        .unwrap_or_else(|| panic!("missing active {name} {arguments}"))
}

// These are the fixed image's AND lists, not arbitrary shell programs. Compare
// whole commands in active RUN instructions so comments or quoted copies of a
// command cannot stand in for the command that enforces the property.
fn run_sequence(instructions: &[(String, String)], commands: &[&str]) -> usize {
    let positions: Vec<_> = commands
        .iter()
        .map(|required| {
            instructions
                .iter()
                .enumerate()
                .filter(|(_, (name, _))| name == "RUN")
                .find_map(|(instruction, (_, body))| {
                    body.split(" && ")
                        .position(|command| command == *required)
                        .map(|command| (instruction, command))
                })
                .unwrap_or_else(|| panic!("missing active RUN command: {required}"))
        })
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0].0 == pair[1].0 && pair[0].1 < pair[1].1),
        "commands must run in order in one AND list: {commands:?}"
    );
    positions.first().expect("nonempty command sequence").0
}

fn check_supply_inputs(dockerfile: &str) {
    let instructions = active_instructions(dockerfile);
    let base_arg = instruction_position(&instructions, "ARG", "BASE_IMAGE");
    let from = instruction_position(&instructions, "FROM", "${BASE_IMAGE}");
    assert!(base_arg < from, "base must be a required build argument");
    assert_eq!(instructions.iter().filter(|(name, _)| name == "FROM").count(), 1);
    let base_check = run_sequence(
        &instructions,
        &[
            "digest=\"${BASE_IMAGE##*@sha256:}\"",
            "test \"$digest\" != \"$BASE_IMAGE\"",
            "test \"${#digest}\" -eq 64",
            "case \"$digest\" in \"\"|*[!a-fA-F0-9]*) exit 1 ;; esac",
        ],
    );
    assert!(
        instructions[from + 1..base_check]
            .iter()
            .any(|(name, body)| name == "ARG" && body == "BASE_IMAGE")
    );
    assert_eq!(instructions.iter().position(|(name, _)| name == "RUN"), Some(base_check));
    let binary = instruction_position(
        &instructions,
        "COPY",
        "--chown=0:0 --chmod=0555 ganja /usr/local/bin/ganja",
    );
    let hash_arg = instruction_position(&instructions, "ARG", "GANJA_SHA256");
    let hash_check = run_sequence(
        &instructions,
        &[
            "test \"${#GANJA_SHA256}\" -eq 64",
            "case \"$GANJA_SHA256\" in \"\"|*[!a-fA-F0-9]*) exit 1 ;; esac",
            "printf '%s  /usr/local/bin/ganja\\n' \"$GANJA_SHA256\" | sha256sum --check --strict -",
        ],
    );
    assert!(base_check < binary && binary < hash_check && hash_arg < hash_check);
    let label = instruction_position(
        &instructions,
        "LABEL",
        "org.opencontainers.image.base.name=\"${BASE_IMAGE}\" io.ganja.binary.sha256=\"${GANJA_SHA256}\"",
    );
    assert!(hash_check < label);
}

#[test]
fn the_worker_image_keeps_only_runtime_directories_writable() {
    check_ownership(DOCKERFILE);
}

fn check_ownership(dockerfile: &str) {
    let instructions = active_instructions(dockerfile);
    let (user, (_, identity)) = instructions
        .iter()
        .enumerate()
        .rev()
        .find(|(_, (name, _))| name == "USER")
        .expect("active runtime USER");
    let (uid, gid) = identity.split_once(':').expect("explicit numeric uid and gid");
    assert_ne!(uid.parse::<u32>().expect("numeric uid"), 0);
    assert_ne!(gid.parse::<u32>().expect("numeric gid"), 0);
    assert!(
        !instructions[user + 1..].iter().any(|(name, _)| matches!(name.as_str(), "RUN" | "COPY"))
    );
    assert_eq!(instructions.iter().filter(|(name, _)| name == "COPY").count(), 2);
    let binary = instruction_position(
        &instructions,
        "COPY",
        "--chown=0:0 --chmod=0555 ganja /usr/local/bin/ganja",
    );
    let config = instruction_position(
        &instructions,
        "COPY",
        "--chown=0:0 --chmod=0444 ganja.toml gitconfig acceptance-script.json /etc/ganja/",
    );
    let permissions = run_sequence(
        &instructions,
        &[
            "chmod 0555 /etc/ganja /usr/local/bin/ganja",
            "chmod 0444 /etc/ganja/*",
            "find / -xdev \\( -type f -o -type d \\) -perm /0022 -exec chmod go-w {} +",
            &format!("chown -R {uid}:{gid} /work /var/lib/ganja /home/ganja"),
            "chmod 0755 /tmp /var/tmp",
            "find / -xdev -type f -perm /6000 -exec chmod a-s {} +",
            "remaining=\"$(find / -xdev -type f -perm /6000)\"",
            "test -z \"$remaining\"",
        ],
    );
    assert!(binary < permissions && config < permissions && permissions < user);
    assert!(instructions.iter().any(|(name, body)| name == "ENV"
        && body.split_whitespace().any(|value| value == "HOME=/home/ganja")));
    instruction_position(&instructions, "EXPOSE", "8080");
    instruction_position(
        &instructions,
        "ENTRYPOINT",
        "[\"ganja\", \"worker\", \"init\", \"--port\", \"8080\"]",
    );
}

fn comment_instruction(source: &str, prefix: &str) -> String {
    let mut commenting = false;
    source
        .lines()
        .map(|line| {
            commenting |= line.trim_start().starts_with(prefix);
            let output = if commenting { format!("# {line}") } else { line.to_owned() };
            commenting &= line.trim_end().ends_with('\\');
            output
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn disabled_or_reordered_image_instructions_are_refused() {
    let strip =
        "    && find / -xdev \\( -type f -o -type d \\) -perm /0022 -exec chmod go-w {} + \\\n";
    let chown = "    && chown -R 10001:10001 /work /var/lib/ganja /home/ganja \\\n";
    let cases = [
        ("commented USER", comment_instruction(DOCKERFILE, "USER ")),
        ("commented checksum", comment_instruction(DOCKERFILE, "RUN test \"${#GANJA_SHA256}\"")),
        (
            "commented cleanup",
            comment_instruction(DOCKERFILE, "&& find / -xdev -type f -perm /6000"),
        ),
        (
            "write strip after chown",
            DOCKERFILE.replace(&format!("{strip}{chown}"), &format!("{chown}{strip}")),
        ),
        ("RUN after USER", DOCKERFILE.replace("USER 10001:10001", "USER 10001:10001\nRUN true")),
        ("tag-only FROM", DOCKERFILE.replace("FROM ${BASE_IMAGE}", "FROM debian:trixie-slim")),
    ];
    let mut accepted = Vec::new();
    for (name, source) in cases {
        assert_ne!(source, DOCKERFILE, "fixture {name} must alter the real Dockerfile");
        if std::panic::catch_unwind(|| {
            check_supply_inputs(&source);
            check_ownership(&source);
        })
        .is_ok()
        {
            accepted.push(name);
        }
    }
    assert!(accepted.is_empty(), "disabled or reordered instructions accepted: {accepted:?}");
}
