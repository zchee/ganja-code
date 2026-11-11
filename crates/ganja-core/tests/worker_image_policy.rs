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
    let lines: Vec<_> = DOCKERFILE.lines().map(str::trim).collect();
    assert!(lines.contains(&"ARG BASE_IMAGE"), "base reference must be supplied without a default");
    assert_eq!(
        lines.iter().filter(|line| line.starts_with("FROM ")).copied().collect::<Vec<_>>(),
        ["FROM ${BASE_IMAGE}"],
        "no tag-only base may bypass the supplied digest"
    );
    for required in [
        "digest=\"${BASE_IMAGE##*@sha256:}\"",
        "test \"$digest\" != \"$BASE_IMAGE\"",
        "test \"${#digest}\" -eq 64",
        "case \"$digest\" in \"\"|*[!a-fA-F0-9]*) exit 1 ;; esac",
        "ARG GANJA_SHA256",
        "test \"${#GANJA_SHA256}\" -eq 64",
        "case \"$GANJA_SHA256\" in \"\"|*[!a-fA-F0-9]*) exit 1 ;; esac",
        "sha256sum --check --strict -",
        "org.opencontainers.image.base.name=\"${BASE_IMAGE}\"",
        "io.ganja.binary.sha256=\"${GANJA_SHA256}\"",
    ] {
        assert!(DOCKERFILE.contains(required), "missing supply check: {required}");
    }
}

#[test]
fn the_worker_image_keeps_only_runtime_directories_writable() {
    for required in [
        "USER 10001:10001",
        "HOME=/home/ganja",
        "COPY --chown=0:0 --chmod=0555 ganja /usr/local/bin/ganja",
        "COPY --chown=0:0 --chmod=0444 ganja.toml gitconfig acceptance-script.json /etc/ganja/",
        "chmod 0555 /etc/ganja /usr/local/bin/ganja",
        "chmod 0444 /etc/ganja/*",
        "chown -R 10001:10001 /work /var/lib/ganja /home/ganja",
        "find / -xdev \\( -type f -o -type d \\) -perm /0022 -exec chmod go-w {} +",
        "chmod 0755 /tmp /var/tmp",
        "find / -xdev -type f -perm /6000 -exec chmod a-s {} +",
        "remaining=\"$(find / -xdev -type f -perm /6000)\"",
        "test -z \"$remaining\"",
        "EXPOSE 8080",
        "ENTRYPOINT [\"ganja\", \"worker\", \"init\", \"--port\", \"8080\"]",
    ] {
        assert!(DOCKERFILE.contains(required), "missing image ownership rule: {required}");
    }
    let user = DOCKERFILE.find("USER 10001:10001").expect("runtime user");
    assert!(!DOCKERFILE[user..].lines().any(|line| line.starts_with("RUN ")));
}
