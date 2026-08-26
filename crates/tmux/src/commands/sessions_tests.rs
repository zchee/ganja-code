use super::*;
use crate::{
    SessionId,
    commands::{Invocation, words},
};

#[test]
fn new_session_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &NewSession::new()
                .attach_if_exists()
                .detached()
                .detach_others()
                .skip_update_environment()
                .print()
                .hangup_parent()
                .start_directory("/work")
                .environment("A=1")
                .format("#{session_name}")
                .client_flags("read-only")
                .window_name("first")
                .session_name("workers")
                .group("pool")
                .width("80")
                .height("24")
                .command(["sh", "-c", "true"])
        ),
        [
            "new-session",
            "-A",
            "-d",
            "-D",
            "-E",
            "-P",
            "-X",
            "-c",
            "/work",
            "-e",
            "A=1",
            "-F",
            "#{session_name}",
            "-f",
            "read-only",
            "-n",
            "first",
            "-s",
            "workers",
            "-t",
            "pool",
            "-x",
            "80",
            "-y",
            "24",
            "--",
            "sh",
            "-c",
            "true",
        ]
    );
}

#[test]
fn new_session_reads_an_environment_once_per_variable() {
    assert_eq!(
        words(
            &NewSession::new()
                .detached()
                .environment("A=1")
                .environment("B=2")
                .environment("C=3")
        ),
        ["new-session", "-d", "-e", "A=1", "-e", "B=2", "-e", "C=3"],
        "tmux reads -e once per variable, so the builder must not fold them together"
    );
}

#[test]
fn a_session_id_reaches_a_target_without_being_restrung() {
    let session = SessionId::new("$3").expect("a well-formed session id");
    assert_eq!(
        words(&HasSession::new().target(&session)),
        ["has-session", "-t", "$3"],
        "an id read out of a previous answer should pass straight into the next call"
    );
    assert_eq!(
        words(&KillSession::new().target(session)),
        ["kill-session", "-t", "$3"]
    );
}

#[test]
fn attach_session_carries_a_working_directory_and_its_client_flags() {
    assert_eq!(
        words(
            &AttachSession::new()
                .detach_others()
                .skip_update_environment()
                .read_only()
                .hangup_parent()
                .working_directory("/srv")
                .client_flags("no-output,pause-after=1")
                .target("workers")
        ),
        [
            "attach-session",
            "-d",
            "-E",
            "-r",
            "-x",
            "-c",
            "/srv",
            "-f",
            "no-output,pause-after=1",
            "-t",
            "workers",
        ]
    );
}

#[test]
fn rename_session_takes_its_new_name_as_a_positional() {
    assert_eq!(
        words(&RenameSession::new().target("$0").new_name("renamed")),
        ["rename-session", "-t", "$0", "--", "renamed"],
        "a name beginning with a dash must not be read back as a flag"
    );
}

#[test]
fn kill_session_can_spare_the_one_it_targets() {
    assert_eq!(
        words(&KillSession::new().all_others().target("keep")),
        ["kill-session", "-a", "-t", "keep"]
    );
    assert_eq!(
        words(&KillSession::new().clear_alerts().group()),
        ["kill-session", "-C", "-g"]
    );
}

#[test]
fn the_two_listings_share_their_vocabulary() {
    assert_eq!(
        words(
            &ListSessions::new()
                .reverse()
                .format("#{session_name}")
                .filter("#{session_attached}")
                .sort_order("activity")
        ),
        [
            "list-sessions",
            "-r",
            "-F",
            "#{session_name}",
            "-f",
            "#{session_attached}",
            "-O",
            "activity",
        ]
    );
    assert_eq!(
        words(
            &ListClients::new()
                .reverse()
                .format("#{client_name}")
                .filter("#{client_readonly}")
                .sort_order("size")
                .target("workers")
        ),
        [
            "list-clients",
            "-r",
            "-F",
            "#{client_name}",
            "-f",
            "#{client_readonly}",
            "-O",
            "size",
            "-t",
            "workers",
        ]
    );
}

#[test]
fn switch_client_walks_sessions_or_names_one() {
    assert_eq!(
        words(
            &SwitchClient::new()
                .next()
                .sort_order("creation")
                .client("/dev/ttys004")
        ),
        [
            "switch-client",
            "-n",
            "-O",
            "creation",
            "-c",
            "/dev/ttys004"
        ]
    );
    assert_eq!(
        words(
            &SwitchClient::new()
                .skip_update_environment()
                .last()
                .previous()
                .toggle_read_only()
                .keep_zoomed()
                .key_table("table2")
                .target("workers:1.0")
        ),
        [
            "switch-client",
            "-E",
            "-l",
            "-p",
            "-r",
            "-Z",
            "-T",
            "table2",
            "-t",
            "workers:1.0",
        ]
    );
}

#[test]
fn detach_client_names_a_client_or_a_whole_session() {
    assert_eq!(
        words(
            &DetachClient::new()
                .all_others()
                .hangup_parent()
                .shell_command("echo bye")
                .session("workers")
                .target("/dev/ttys004")
        ),
        [
            "detach-client",
            "-a",
            "-P",
            "-E",
            "echo bye",
            "-s",
            "workers",
            "-t",
            "/dev/ttys004",
        ]
    );
}

#[test]
fn refresh_client_reads_its_pane_keyed_flags_once_per_pane() {
    assert_eq!(
        words(
            &RefreshClient::new()
                .pane_state("%0:pause")
                .pane_state("%1:off")
                .subscribe("one::#{pane_id}")
                .subscribe("two::#{session_name}")
                .pane_report("%0:report")
                .pane_report("%1:report")
                .target("/dev/ttys004")
        ),
        [
            "refresh-client",
            "-A",
            "%0:pause",
            "-A",
            "%1:off",
            "-B",
            "one::#{pane_id}",
            "-B",
            "two::#{session_name}",
            "-r",
            "%0:report",
            "-r",
            "%1:report",
            "-t",
            "/dev/ttys004",
        ],
        "tmux reads these once per pane, so the builder must keep every one of them"
    );
}

#[test]
fn refresh_client_moves_the_visible_portion_by_an_adjustment() {
    assert_eq!(
        words(
            &RefreshClient::new()
                .track_cursor()
                .down()
                .left()
                .clipboard()
                .right()
                .status_line()
                .up()
                .size("80x24")
                .client_flags("no-output")
                .adjustment("5")
        ),
        [
            "refresh-client",
            "-c",
            "-D",
            "-L",
            "-l",
            "-R",
            "-S",
            "-U",
            "-C",
            "80x24",
            "-f",
            "no-output",
            "--",
            "5",
        ]
    );
}

#[test]
fn the_three_commands_that_take_nothing_are_their_names_alone() {
    assert_eq!(words(&StartServer::new()), ["start-server"]);
    assert_eq!(words(&KillServer::new()), ["kill-server"]);
    assert_eq!(words(&LockServer::new()), ["lock-server"]);
}

#[test]
fn the_three_locks_and_the_suspend_name_what_they_act_on() {
    assert_eq!(
        words(&LockSession::new().target("workers")),
        ["lock-session", "-t", "workers"]
    );
    assert_eq!(
        words(&LockClient::new().target("/dev/ttys004")),
        ["lock-client", "-t", "/dev/ttys004"]
    );
    assert_eq!(
        words(&SuspendClient::new().target("/dev/ttys004")),
        ["suspend-client", "-t", "/dev/ttys004"]
    );
}

#[test]
fn show_messages_can_ask_for_jobs_or_terminals_instead() {
    assert_eq!(
        words(
            &ShowMessages::new()
                .jobs()
                .terminals()
                .target("/dev/ttys004")
        ),
        ["show-messages", "-J", "-T", "-t", "/dev/ttys004"]
    );
}

#[test]
fn server_access_names_a_user_behind_the_separator() {
    assert_eq!(
        words(
            &ServerAccess::new()
                .allow()
                .deny()
                .list()
                .read_only()
                .writable()
                .name("testgroup")
        ),
        [
            "server-access",
            "-a",
            "-d",
            "-l",
            "-r",
            "-w",
            "--",
            "testgroup",
        ]
    );
}

#[test]
fn list_commands_can_ask_about_one_command() {
    assert_eq!(words(&ListCommands::new()), ["list-commands"]);
    assert_eq!(
        words(
            &ListCommands::new()
                .format("#{command_list_name}")
                .command("split-window")
        ),
        [
            "list-commands",
            "-F",
            "#{command_list_name}",
            "--",
            "split-window",
        ]
    );
}

#[test]
fn the_family_names_itself_the_way_tmux_does() {
    assert_eq!(NewSession::NAME, "new-session");
    assert_eq!(NewSession::ALIAS, Some("new"));
    assert_eq!(KillServer::ALIAS, None, "tmux gives kill-server no alias");
    assert_eq!(ENTRIES.len(), 19, "the roster this wave named");
}
