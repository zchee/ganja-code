use super::*;
use crate::commands::words;

#[test]
fn set_option_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &SetOption::new()
                .append()
                .expand_formats()
                .global()
                .only_if_unset()
                .pane()
                .quiet()
                .server()
                .unset()
                .unset_in_panes()
                .window()
                .target("%1")
                .option("status-left")
                .value("#{session_name}")
        ),
        [
            "set-option",
            "-a",
            "-F",
            "-g",
            "-o",
            "-p",
            "-q",
            "-s",
            "-u",
            "-U",
            "-w",
            "-t",
            "%1",
            "--",
            "status-left",
            "#{session_name}",
        ]
    );
}

#[test]
fn show_options_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &ShowOptions::new()
                .inherited()
                .global()
                .hooks()
                .pane()
                .quiet()
                .server()
                .value_only()
                .window()
                .target("%1")
                .option("pane-border-status")
        ),
        [
            "show-options",
            "-A",
            "-g",
            "-H",
            "-p",
            "-q",
            "-s",
            "-v",
            "-w",
            "-t",
            "%1",
            "--",
            "pane-border-status",
        ]
    );
}

#[test]
fn the_window_scoped_pair_carries_its_own_smaller_flag_set() {
    assert_eq!(
        words(
            &SetWindowOption::new()
                .append()
                .expand_formats()
                .global()
                .only_if_unset()
                .quiet()
                .unset()
                .target("@1")
                .option("pane-border-status")
                .value("top")
        ),
        [
            "set-window-option",
            "-a",
            "-F",
            "-g",
            "-o",
            "-q",
            "-u",
            "-t",
            "@1",
            "--",
            "pane-border-status",
            "top",
        ]
    );
    assert_eq!(
        words(
            &ShowWindowOptions::new()
                .global()
                .value_only()
                .target("@1")
                .option("pane-border-status")
        ),
        [
            "show-window-options",
            "-g",
            "-v",
            "-t",
            "@1",
            "--",
            "pane-border-status",
        ],
        "this command has no -A, so an inherited read goes through show-options -w"
    );
}

#[test]
fn set_hook_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &SetHook::new()
                .append()
                .global()
                .pane()
                .run_now()
                .unset()
                .window()
                .target("%1")
                .hook("pane-exited")
                .command("display-message gone")
        ),
        [
            "set-hook",
            "-a",
            "-g",
            "-p",
            "-R",
            "-u",
            "-w",
            "-t",
            "%1",
            "--",
            "pane-exited",
            "display-message gone",
        ]
    );
}

#[test]
fn show_hooks_can_list_subscriptions_instead() {
    assert_eq!(
        words(
            &ShowHooks::new()
                .global()
                .pane()
                .window()
                .target("%1")
                .hook("pane-exited")
        ),
        [
            "show-hooks",
            "-g",
            "-p",
            "-w",
            "-t",
            "%1",
            "--",
            "pane-exited",
        ]
    );
}

#[test]
fn set_environment_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &SetEnvironment::new()
                .expand_formats()
                .global()
                .hidden()
                .removed()
                .unset()
                .target("work")
                .variable("GANJA_AGENT_ID")
                .value("w1")
        ),
        [
            "set-environment",
            "-F",
            "-g",
            "-h",
            "-r",
            "-u",
            "-t",
            "work",
            "--",
            "GANJA_AGENT_ID",
            "w1",
        ]
    );
}

#[test]
fn show_environment_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &ShowEnvironment::new()
                .global()
                .hidden()
                .shell()
                .target("work")
                .variable("GANJA_AGENT_ID")
        ),
        [
            "show-environment",
            "-g",
            "-h",
            "-s",
            "-t",
            "work",
            "--",
            "GANJA_AGENT_ID",
        ]
    );
}

#[test]
fn copy_mode_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &CopyMode::new()
                .page_down()
                .exit_at_bottom()
                .hide_position()
                .mouse()
                .cancel()
                .scroll_to_mouse()
                .page_up()
                .source("%2")
                .target("%1")
        ),
        [
            "copy-mode",
            "-d",
            "-e",
            "-H",
            "-M",
            "-q",
            "-S",
            "-u",
            "-s",
            "%2",
            "-t",
            "%1",
        ]
    );
}

#[test]
fn clock_mode_names_only_a_pane() {
    assert_eq!(
        words(&ClockMode::new().target("%1")),
        ["clock-mode", "-t", "%1"]
    );
}

#[test]
fn customize_mode_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &CustomizeMode::new()
                .without_information()
                .zoom()
                .format("#{option_name}")
                .filter("#{m:pane-*,#{option_name}}")
                .target("%1")
        ),
        [
            "customize-mode",
            "-N",
            "-Z",
            "-F",
            "#{option_name}",
            "-f",
            "#{m:pane-*,#{option_name}}",
            "-t",
            "%1",
        ]
    );
}

#[test]
fn switch_mode_puts_its_template_behind_the_fence() {
    assert_eq!(
        words(
            &SwitchMode::new()
                .kill_on_exit()
                .sessions()
                .windows()
                .zoom()
                .format("#{session_name}")
                .target("%1")
                .template("switch-client -t '%%'")
        ),
        [
            "switch-mode",
            "-k",
            "-s",
            "-w",
            "-Z",
            "-F",
            "#{session_name}",
            "-t",
            "%1",
            "--",
            "switch-client -t '%%'",
        ]
    );
}

#[test]
fn choose_client_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &ChooseClient::new()
                .without_preview()
                .reverse()
                .no_confirm()
                .zoom()
                .format("#{client_name}")
                .filter("#{client_readonly}")
                .key_format("#{line}")
                .sort_order("activity")
                .target("%1")
                .template("detach-client -t '%%'")
        ),
        [
            "choose-client",
            "-N",
            "-r",
            "-y",
            "-Z",
            "-F",
            "#{client_name}",
            "-f",
            "#{client_readonly}",
            "-K",
            "#{line}",
            "-O",
            "activity",
            "-t",
            "%1",
            "--",
            "detach-client -t '%%'",
        ]
    );
}

#[test]
fn choose_tree_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &ChooseTree::new()
                .all_session_groups()
                .without_preview()
                .reverse()
                .collapsed_sessions()
                .collapsed_windows()
                .no_confirm()
                .zoom()
                .format("#{window_name}")
                .filter("#{window_active}")
                .key_format("#{line}")
                .sort_order("index")
                .target("%1")
                .template("switch-client -t '%%'")
        ),
        [
            "choose-tree",
            "-G",
            "-N",
            "-r",
            "-s",
            "-w",
            "-y",
            "-Z",
            "-F",
            "#{window_name}",
            "-f",
            "#{window_active}",
            "-K",
            "#{line}",
            "-O",
            "index",
            "-t",
            "%1",
            "--",
            "switch-client -t '%%'",
        ]
    );
}

#[test]
fn source_file_takes_one_path_per_call_in_that_order() {
    assert_eq!(
        words(
            &SourceFile::new()
                .expand_formats()
                .parse_only()
                .quiet()
                .verbose()
                .target("%1")
                .path("/etc/tmux.conf")
                .path("~/.tmux.conf")
        ),
        [
            "source-file",
            "-F",
            "-n",
            "-q",
            "-v",
            "-t",
            "%1",
            "--",
            "/etc/tmux.conf",
            "~/.tmux.conf",
        ],
        "tmux reads `path ...`, so a second file is another word rather than a replacement"
    );
}

#[test]
fn run_shell_fences_its_command_off_from_the_flags() {
    assert_eq!(
        words(
            &RunShell::new()
                .background()
                .tmux_command()
                .stderr_to_stdout()
                .start_directory("/work")
                .delay("2")
                .target("%1")
                .shell_command("myscript.sh #{1} #{2}")
                .arguments(["-foo", "bar"])
        ),
        [
            "run-shell",
            "-b",
            "-C",
            "-E",
            "-c",
            "/work",
            "-d",
            "2",
            "-t",
            "%1",
            "--",
            "myscript.sh #{1} #{2}",
            "-foo",
            "bar",
        ],
        "an argument beginning with a dash is an argument, and the fence is what says so"
    );
}

#[test]
fn if_shell_keeps_its_three_positionals_in_call_order() {
    assert_eq!(
        words(
            &IfShell::new()
                .background()
                .as_format()
                .target("%1")
                .shell_command("#{pane_dead}")
                .command("display-message dead")
                .otherwise("display-message alive")
        ),
        [
            "if-shell",
            "-b",
            "-F",
            "-t",
            "%1",
            "--",
            "#{pane_dead}",
            "display-message dead",
            "display-message alive",
        ]
    );
}

#[test]
fn wait_for_renders_every_flag_it_has() {
    assert_eq!(
        words(&WaitFor::new().lock().signal().unlock().name("build-done")),
        ["wait-for", "-L", "-S", "-U", "--", "build-done"]
    );
}

#[cfg(unix)]
#[test]
fn an_environment_value_outside_utf8_survives_into_argv_byte_for_byte() {
    use std::{
        ffi::OsString,
        os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    };

    let value = OsString::from_vec(b"/tmp/a\x80b".to_vec());
    let argv = SetEnvironment::new()
        .variable("GANJA_PATH")
        .value(value)
        .args();
    assert_eq!(
        argv.last().map(|word| word.as_bytes()),
        Some(&b"/tmp/a\x80b"[..]),
        "an environment value is whatever the caller holds, and a path is not obliged to be \
             UTF-8"
    );
}

#[test]
fn every_command_in_this_family_is_in_the_registry_once() {
    assert_eq!(ENTRIES.len(), 18, "the roster this module settled on");
    for entry in ENTRIES {
        assert!(
            crate::commands::REGISTRY.contains(entry),
            "{} is declared here but not gathered into the register",
            entry.name
        );
    }
}
