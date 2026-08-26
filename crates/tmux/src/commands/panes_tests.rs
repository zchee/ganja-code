use super::*;
use crate::{PaneId, WindowId, commands::words};

#[test]
fn split_window_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &SplitWindow::new()
                .before()
                .detached()
                .full_size()
                .horizontal()
                .stdin()
                .keep_open()
                .print()
                .vertical()
                .zoom()
                .start_directory("/work")
                .environment("A=1")
                .format("#{pane_id}")
                .size("20%")
                .message("done")
                .percentage("30")
                .style("bg=black")
                .active_border_style("fg=green")
                .inactive_border_style("fg=grey")
                .target("%2")
                .command(["sh", "-c", "true"])
        ),
        [
            "split-window",
            "-b",
            "-d",
            "-f",
            "-h",
            "-I",
            "-k",
            "-P",
            "-v",
            "-Z",
            "-c",
            "/work",
            "-e",
            "A=1",
            "-F",
            "#{pane_id}",
            "-l",
            "20%",
            "-m",
            "done",
            "-p",
            "30",
            "-s",
            "bg=black",
            "-S",
            "fg=green",
            "-R",
            "fg=grey",
            "-t",
            "%2",
            "--",
            "sh",
            "-c",
            "true",
        ]
    );
}

#[test]
fn new_pane_carries_a_floating_panes_position_and_size() {
    assert_eq!(
        words(
            &NewPane::new()
                .detached()
                .width("60%")
                .height("40%")
                .x_position("10")
                .y_position("2")
                .target("%0")
        ),
        [
            "new-pane", "-d", "-x", "60%", "-y", "40%", "-X", "10", "-Y", "2", "-t", "%0",
        ]
    );
}

#[test]
fn select_pane_can_mark_a_pane_and_title_it() {
    assert_eq!(
        words(&SelectPane::new().mark().title("build").target("%3")),
        ["select-pane", "-m", "-T", "build", "-t", "%3"]
    );
    assert_eq!(
        words(&SelectPane::new().clear_marked()),
        ["select-pane", "-M"]
    );
}

#[test]
fn select_pane_moves_by_direction() {
    assert_eq!(
        words(&SelectPane::new().down().left().right().up().keep_zoomed()),
        ["select-pane", "-D", "-L", "-R", "-U", "-Z"]
    );
    assert_eq!(
        words(&SelectPane::new().last().disable_input().enable_input()),
        ["select-pane", "-l", "-d", "-e"]
    );
}

#[test]
fn last_pane_names_the_window_it_looks_in() {
    assert_eq!(
        words(&LastPane::new().keep_zoomed().disable_input().target("@1")),
        ["last-pane", "-Z", "-d", "-t", "@1"]
    );
    assert_eq!(words(&LastPane::new().enable_input()), ["last-pane", "-e"]);
}

#[test]
fn kill_pane_can_spare_the_one_it_targets() {
    assert_eq!(
        words(&KillPane::new().all_others().target("%1")),
        ["kill-pane", "-a", "-t", "%1",]
    );
}

#[test]
fn list_panes_asks_the_whole_server_in_a_caller_format() {
    assert_eq!(
        words(
            &ListPanes::new()
                .all()
                .session()
                .reverse()
                .format("#{pane_id} #{pane_pid}")
                .filter("#{pane_active}")
                .sort_order("creation")
                .target("work")
        ),
        [
            "list-panes",
            "-a",
            "-s",
            "-r",
            "-F",
            "#{pane_id} #{pane_pid}",
            "-f",
            "#{pane_active}",
            "-O",
            "creation",
            "-t",
            "work",
        ]
    );
}

#[test]
fn break_pane_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &BreakPane::new()
                .after()
                .before()
                .detached()
                .print()
                .format("#{window_id}")
                .window_name("scratch")
                .source("%4")
                .target("work:9")
        ),
        [
            "break-pane",
            "-a",
            "-b",
            "-d",
            "-P",
            "-F",
            "#{window_id}",
            "-n",
            "scratch",
            "-s",
            "%4",
            "-t",
            "work:9",
        ]
    );
}

#[test]
fn join_pane_moves_a_pane_into_a_split() {
    assert_eq!(
        words(
            &JoinPane::new()
                .before()
                .detached()
                .full_size()
                .horizontal()
                .vertical()
                .size("30%")
                .source("%5")
                .target("%0")
        ),
        [
            "join-pane",
            "-b",
            "-d",
            "-f",
            "-h",
            "-v",
            "-l",
            "30%",
            "-s",
            "%5",
            "-t",
            "%0",
        ]
    );
}

#[test]
fn move_pane_renders_both_of_the_jobs_it_does() {
    assert_eq!(
        words(&MovePane::new().source("%5").target("%0").size("50%")),
        ["move-pane", "-s", "%5", "-t", "%0", "-l", "50%"]
    );
    assert_eq!(
        words(
            &MovePane::new()
                .before()
                .detached()
                .full_size()
                .horizontal()
                .vertical()
        ),
        ["move-pane", "-b", "-d", "-f", "-h", "-v"]
    );
}

#[test]
fn swap_pane_takes_a_direction_when_it_has_no_source() {
    assert_eq!(
        words(&SwapPane::new().detached().next().previous().keep_zoomed()),
        ["swap-pane", "-d", "-D", "-U", "-Z"]
    );
    assert_eq!(
        words(&SwapPane::new().source("%1").target("%2")),
        ["swap-pane", "-s", "%1", "-t", "%2"]
    );
}

#[test]
fn resize_pane_adjusts_or_sizes_absolutely() {
    assert_eq!(
        words(
            &ResizePane::new()
                .mouse()
                .trim()
                .zoom()
                .down("2")
                .left("3")
                .right("4")
                .up("5")
                .width("10%")
                .height("20")
                .target("%1")
        ),
        [
            "resize-pane",
            "-M",
            "-T",
            "-Z",
            "-D",
            "2",
            "-L",
            "3",
            "-R",
            "4",
            "-U",
            "5",
            "-x",
            "10%",
            "-y",
            "20",
            "-t",
            "%1",
        ]
    );
}

#[test]
fn respawn_pane_can_replace_a_dead_command() {
    assert_eq!(
        words(
            &RespawnPane::new()
                .kill_running()
                .start_directory("/srv")
                .environment("A=1")
                .environment("B=2")
                .target("%2")
                .command(["sh", "-c", "true"])
        ),
        [
            "respawn-pane",
            "-k",
            "-c",
            "/srv",
            "-e",
            "A=1",
            "-e",
            "B=2",
            "-t",
            "%2",
            "--",
            "sh",
            "-c",
            "true",
        ]
    );
}

#[test]
fn capture_pane_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &CapturePane::new()
                .alternate_screen()
                .escape_octal()
                .escape_sequences()
                .line_flags()
                .hyperlinks()
                .join_wrapped()
                .line_numbers()
                .mode_screen()
                .keep_trailing_spaces()
                .stdout()
                .pending()
                .quiet()
                .ignore_trailing()
                .buffer("scratch")
                .end_line("-")
                .start_line("-")
                .target("%0")
        ),
        [
            "capture-pane",
            "-a",
            "-C",
            "-e",
            "-F",
            "-H",
            "-J",
            "-L",
            "-M",
            "-N",
            "-p",
            "-P",
            "-q",
            "-T",
            "-b",
            "scratch",
            "-E",
            "-",
            "-S",
            "-",
            "-t",
            "%0",
        ]
    );
}

#[test]
fn pipe_pane_fences_its_shell_command_off() {
    assert_eq!(
        words(
            &PipePane::new()
                .input()
                .output()
                .toggle()
                .target("%0")
                .shell_command("cat >>/tmp/log")
        ),
        [
            "pipe-pane",
            "-I",
            "-O",
            "-o",
            "-t",
            "%0",
            "--",
            "cat >>/tmp/log",
        ]
    );
    assert_eq!(
        words(&PipePane::new().target("%0")),
        ["pipe-pane", "-t", "%0"],
        "with no command tmux closes the pane's pipe, so the fence has nothing to fence"
    );
}

#[test]
fn display_panes_carries_a_template_for_the_chosen_pane() {
    assert_eq!(
        words(
            &DisplayPanes::new()
                .no_block()
                .ignore_keys()
                .duration("2000")
                .target("/dev/ttys001")
                .template("select-pane -t '%%'")
        ),
        [
            "display-panes",
            "-b",
            "-N",
            "-d",
            "2000",
            "-t",
            "/dev/ttys001",
            "--",
            "select-pane -t '%%'",
        ]
    );
}

#[test]
fn new_window_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &NewWindow::new()
                .after()
                .before()
                .detached()
                .kill_existing()
                .print()
                .select_existing()
                .start_directory("/work")
                .environment("A=1")
                .format("#{window_id}")
                .window_name("build")
                .target("work:2")
                .command(["make", "-j8"])
        ),
        [
            "new-window",
            "-a",
            "-b",
            "-d",
            "-k",
            "-P",
            "-S",
            "-c",
            "/work",
            "-e",
            "A=1",
            "-F",
            "#{window_id}",
            "-n",
            "build",
            "-t",
            "work:2",
            "--",
            "make",
            "-j8",
        ]
    );
}

#[test]
fn kill_window_can_spare_the_one_it_targets() {
    assert_eq!(
        words(&KillWindow::new().all_others().target("@1")),
        ["kill-window", "-a", "-t", "@1"]
    );
}

#[test]
fn list_windows_asks_the_whole_server_in_a_caller_format() {
    assert_eq!(
        words(
            &ListWindows::new()
                .all()
                .reverse()
                .format("#{window_id}")
                .filter("#{window_active}")
                .sort_order("index")
                .target("work")
        ),
        [
            "list-windows",
            "-a",
            "-r",
            "-F",
            "#{window_id}",
            "-f",
            "#{window_active}",
            "-O",
            "index",
            "-t",
            "work",
        ]
    );
}

#[test]
fn select_window_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &SelectWindow::new()
                .last()
                .next()
                .previous()
                .toggle()
                .target("@2")
        ),
        ["select-window", "-l", "-n", "-p", "-T", "-t", "@2"]
    );
}

#[test]
fn next_and_previous_window_can_look_for_an_alert() {
    assert_eq!(
        words(&NextWindow::new().with_alert().target("work")),
        ["next-window", "-a", "-t", "work"]
    );
    assert_eq!(
        words(&PreviousWindow::new().with_alert().target("work")),
        ["previous-window", "-a", "-t", "work"]
    );
}

#[test]
fn last_window_names_only_a_session() {
    assert_eq!(
        words(&LastWindow::new().target("work")),
        ["last-window", "-t", "work"]
    );
}

#[test]
fn rename_window_puts_the_new_name_behind_the_fence() {
    assert_eq!(
        words(&RenameWindow::new().target("@1").new_name("-not-a-flag")),
        ["rename-window", "-t", "@1", "--", "-not-a-flag"],
        "a name beginning with a dash is a name, and the fence is what says so"
    );
}

#[test]
fn move_window_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &MoveWindow::new()
                .after()
                .before()
                .detached()
                .kill_existing()
                .renumber()
                .source("@1")
                .target("other:3")
        ),
        [
            "move-window",
            "-a",
            "-b",
            "-d",
            "-k",
            "-r",
            "-s",
            "@1",
            "-t",
            "other:3",
        ]
    );
}

#[test]
fn swap_window_names_both_windows() {
    assert_eq!(
        words(&SwapWindow::new().detached().source("@1").target("@2")),
        ["swap-window", "-d", "-s", "@1", "-t", "@2"]
    );
}

#[test]
fn link_window_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &LinkWindow::new()
                .after()
                .before()
                .detached()
                .kill_existing()
                .source("@1")
                .target("other:3")
        ),
        [
            "link-window",
            "-a",
            "-b",
            "-d",
            "-k",
            "-s",
            "@1",
            "-t",
            "other:3",
        ]
    );
}

#[test]
fn unlink_window_can_destroy_the_last_link() {
    assert_eq!(
        words(&UnlinkWindow::new().destroy().target("@1")),
        ["unlink-window", "-k", "-t", "@1"]
    );
}

#[test]
fn respawn_window_can_replace_a_dead_command() {
    assert_eq!(
        words(
            &RespawnWindow::new()
                .kill_running()
                .start_directory("/srv")
                .environment("A=1")
                .target("@1")
                .command(["true"])
        ),
        [
            "respawn-window",
            "-k",
            "-c",
            "/srv",
            "-e",
            "A=1",
            "-t",
            "@1",
            "--",
            "true",
        ]
    );
}

#[test]
fn resize_window_takes_its_adjustment_as_a_positional() {
    assert_eq!(
        words(
            &ResizeWindow::new()
                .smallest()
                .largest()
                .down()
                .left()
                .right()
                .up()
                .width("120")
                .height("40")
                .target("@1")
                .adjustment("5")
        ),
        [
            "resize-window",
            "-a",
            "-A",
            "-D",
            "-L",
            "-R",
            "-U",
            "-x",
            "120",
            "-y",
            "40",
            "-t",
            "@1",
            "--",
            "5",
        ]
    );
}

#[test]
fn rotate_window_turns_either_way() {
    assert_eq!(
        words(
            &RotateWindow::new()
                .downward()
                .upward()
                .keep_zoomed()
                .target("@1")
        ),
        ["rotate-window", "-D", "-U", "-Z", "-t", "@1"]
    );
}

#[test]
fn find_window_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &FindWindow::new()
                .match_contents()
                .ignore_case()
                .match_name()
                .regex()
                .match_title()
                .zoom()
                .target("%0")
                .pattern("^build")
        ),
        [
            "find-window",
            "-C",
            "-i",
            "-N",
            "-r",
            "-T",
            "-Z",
            "-t",
            "%0",
            "--",
            "^build",
        ]
    );
}

#[test]
fn the_two_bare_layout_moves_name_only_a_window() {
    assert_eq!(
        words(&NextLayout::new().target("@1")),
        ["next-layout", "-t", "@1"]
    );
    assert_eq!(
        words(&PreviousLayout::new().target("@1")),
        ["previous-layout", "-t", "@1"]
    );
}

#[test]
fn select_layout_takes_a_layout_name_or_none() {
    assert_eq!(
        words(
            &SelectLayout::new()
                .spread()
                .next()
                .undo()
                .previous()
                .target("%0")
                .layout("main-vertical")
        ),
        [
            "select-layout",
            "-E",
            "-n",
            "-o",
            "-p",
            "-t",
            "%0",
            "--",
            "main-vertical",
        ]
    );
    assert_eq!(words(&SelectLayout::new()), ["select-layout"]);
}

#[test]
fn a_target_takes_an_id_read_out_of_a_previous_answer() {
    let pane = PaneId::new("%7").expect("a well-formed pane id");
    let window = WindowId::new("@3").expect("a well-formed window id");
    assert_eq!(
        words(&KillPane::new().target(&pane)),
        ["kill-pane", "-t", "%7"],
        "an id that has to be restrung to be used again is an id in name only"
    );
    assert_eq!(
        words(&KillWindow::new().target(window)),
        ["kill-window", "-t", "@3"]
    );
}

#[test]
fn every_command_in_this_family_is_in_the_registry_once() {
    assert_eq!(ENTRIES.len(), 34, "the roster this module settled on");
    for entry in ENTRIES {
        assert!(
            crate::commands::REGISTRY.contains(entry),
            "{} is declared here but not gathered into the register",
            entry.name
        );
    }
}
