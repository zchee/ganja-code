use super::*;
use crate::{PaneId, commands::words};

#[test]
fn set_buffer_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &SetBuffer::new()
                .append()
                .clipboard()
                .buffer("work")
                .new_name("kept")
                .target("/dev/ttys001")
                .data("hello")
        ),
        [
            "set-buffer",
            "-a",
            "-w",
            "-b",
            "work",
            "-n",
            "kept",
            "-t",
            "/dev/ttys001",
            "--",
            "hello",
        ]
    );
}

#[test]
fn set_buffer_fences_data_that_looks_like_a_flag() {
    assert_eq!(
        words(&SetBuffer::new().buffer("work").data("-n not-a-rename")),
        ["set-buffer", "-b", "work", "--", "-n not-a-rename"],
        "without the fence, a buffer whose contents begin with a flag letter would rename it"
    );
}

#[test]
fn load_buffer_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &LoadBuffer::new()
                .clipboard()
                .buffer("work")
                .target("/dev/ttys001")
                .path("/tmp/clip.txt")
        ),
        [
            "load-buffer",
            "-w",
            "-b",
            "work",
            "-t",
            "/dev/ttys001",
            "--",
            "/tmp/clip.txt",
        ],
        "-w is documented and accepted, and only the usage string omits it"
    );
}

#[test]
fn save_buffer_renders_every_flag_it_has() {
    assert_eq!(
        words(&SaveBuffer::new().append().buffer("work").path("-")),
        ["save-buffer", "-a", "-b", "work", "--", "-"],
        "the fence is what lets `-` mean standard output rather than a truncated flag"
    );
}

#[test]
fn show_buffer_and_delete_buffer_name_one_buffer_each() {
    assert_eq!(
        words(&ShowBuffer::new().buffer("work")),
        ["show-buffer", "-b", "work"]
    );
    assert_eq!(words(&ShowBuffer::new()), ["show-buffer"]);
    assert_eq!(
        words(&DeleteBuffer::new().buffer("work")),
        ["delete-buffer", "-b", "work"]
    );
}

#[test]
fn paste_buffer_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &PasteBuffer::new()
                .delete()
                .bracketed()
                .keep_linefeeds()
                .unsanitized()
                .buffer("work")
                .separator("\r\n")
                .target("%1")
        ),
        [
            "paste-buffer",
            "-d",
            "-p",
            "-r",
            "-S",
            "-b",
            "work",
            "-s",
            "\r\n",
            "-t",
            "%1",
        ]
    );
}

#[test]
fn list_buffers_asks_in_a_caller_format() {
    assert_eq!(
        words(
            &ListBuffers::new()
                .reverse()
                .format("#{buffer_name} #{buffer_size}")
                .filter("#{m:work*,#{buffer_name}}")
                .sort_order("creation")
        ),
        [
            "list-buffers",
            "-r",
            "-F",
            "#{buffer_name} #{buffer_size}",
            "-f",
            "#{m:work*,#{buffer_name}}",
            "-O",
            "creation",
        ],
        "-r is documented and accepted, and only the usage string omits it"
    );
}

#[test]
fn choose_buffer_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &ChooseBuffer::new()
                .no_preview()
                .reverse()
                .skip_confirmations()
                .zoom()
                .format("#{buffer_name}")
                .filter("#{buffer_size}")
                .key_format("#{line}")
                .sort_order("size")
                .target("%2")
                .template("paste-buffer -b '%%'")
        ),
        [
            "choose-buffer",
            "-N",
            "-r",
            "-y",
            "-Z",
            "-F",
            "#{buffer_name}",
            "-f",
            "#{buffer_size}",
            "-K",
            "#{line}",
            "-O",
            "size",
            "-t",
            "%2",
            "--",
            "paste-buffer -b '%%'",
        ],
        "-y is documented and accepted, and only the usage string omits it"
    );
}

#[test]
fn clear_history_can_free_the_hyperlinks_too() {
    assert_eq!(
        words(&ClearHistory::new().hyperlinks().target("%0")),
        ["clear-history", "-H", "-t", "%0"]
    );
    assert_eq!(words(&ClearHistory::new()), ["clear-history"]);
}

#[test]
fn send_keys_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &SendKeys::new()
                .expand_formats()
                .hex()
                .to_client()
                .literal()
                .mouse()
                .reset()
                .copy_mode()
                .client("/dev/ttys001")
                .repeat_count("3")
                .target("%1")
                .key("C-a")
        ),
        [
            "send-keys",
            "-F",
            "-H",
            "-K",
            "-l",
            "-M",
            "-R",
            "-X",
            "-c",
            "/dev/ttys001",
            "-N",
            "3",
            "-t",
            "%1",
            "--",
            "C-a",
        ]
    );
}

#[test]
fn send_keys_takes_one_key_per_call_in_the_order_they_were_given() {
    assert_eq!(
        words(
            &SendKeys::new()
                .target("%1")
                .key("C-c")
                .key("q")
                .key("Enter")
        ),
        ["send-keys", "-t", "%1", "--", "C-c", "q", "Enter"],
        "tmux sends the keys first to last, so the builder must not reorder or fold them"
    );
}

#[cfg(unix)]
#[test]
fn a_literal_line_outside_utf8_survives_into_argv_byte_for_byte() {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let line = std::ffi::OsString::from_vec(b"-echo \x80\xfe not text".to_vec());
    let argv = SendKeys::new().target("%1").literal().key(line).args();
    assert_eq!(
        argv.iter()
            .map(|word| word.as_bytes())
            .collect::<Vec<&[u8]>>(),
        [
            &b"send-keys"[..],
            b"-t",
            b"%1",
            b"-l",
            b"--",
            b"-echo \x80\xfe not text",
        ],
        "a literal line is a caller's own bytes, and this layer hands them to execve unread"
    );
}

#[test]
fn send_prefix_can_send_the_secondary_one() {
    assert_eq!(
        words(&SendPrefix::new().secondary().target("%1")),
        ["send-prefix", "-2", "-t", "%1"]
    );
    assert_eq!(words(&SendPrefix::new()), ["send-prefix"]);
}

#[test]
fn bind_key_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &BindKey::new()
                .root_table()
                .repeats()
                .note("split the pane")
                .key_table("copy-mode")
                .key("C-s")
                .command(["split-window", "-h"])
        ),
        [
            "bind-key",
            "-n",
            "-r",
            "-N",
            "split the pane",
            "-T",
            "copy-mode",
            "--",
            "C-s",
            "split-window",
            "-h",
        ],
        "the key comes before the command, and the fence keeps both out of the flags"
    );
}

#[test]
fn unbind_key_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &UnbindKey::new()
                .all()
                .root_table()
                .quiet()
                .key_table("copy-mode")
                .key("C-s")
        ),
        [
            "unbind-key",
            "-a",
            "-n",
            "-q",
            "-T",
            "copy-mode",
            "--",
            "C-s",
        ]
    );
}

#[test]
fn list_keys_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &ListKeys::new()
                .first_only()
                .include_unnoted()
                .notes()
                .reverse()
                .format("#{key}")
                .sort_order("modifier")
                .prefix_string("bind ")
                .key_table("prefix")
                .key("c")
        ),
        [
            "list-keys",
            "-1",
            "-a",
            "-N",
            "-r",
            "-F",
            "#{key}",
            "-O",
            "modifier",
            "-P",
            "bind ",
            "-T",
            "prefix",
            "--",
            "c",
        ]
    );
}

#[test]
fn command_prompt_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &CommandPrompt::new()
                .one_key()
                .background()
                .keep_updating()
                .backspace_cancels()
                .expand_template()
                .on_change()
                .one_key_name()
                .literal()
                .numeric()
                .inputs("main")
                .prompts("name:")
                .target("/dev/ttys001")
                .prompt_type("command")
                .template("rename-window '%%'")
        ),
        [
            "command-prompt",
            "-1",
            "-b",
            "-C",
            "-e",
            "-F",
            "-i",
            "-k",
            "-l",
            "-N",
            "-I",
            "main",
            "-p",
            "name:",
            "-t",
            "/dev/ttys001",
            "-T",
            "command",
            "--",
            "rename-window '%%'",
        ]
    );
}

#[test]
fn confirm_before_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &ConfirmBefore::new()
                .background()
                .default_yes()
                .confirm_key("k")
                .prompt("kill this pane?")
                .target("/dev/ttys001")
                .command("kill-pane")
        ),
        [
            "confirm-before",
            "-b",
            "-y",
            "-c",
            "k",
            "-p",
            "kill this pane?",
            "-t",
            "/dev/ttys001",
            "--",
            "kill-pane",
        ]
    );
}

#[test]
fn the_two_prompt_history_commands_name_one_type_each() {
    assert_eq!(
        words(&ClearPromptHistory::new().prompt_type("search")),
        ["clear-prompt-history", "-T", "search"]
    );
    assert_eq!(
        words(&ShowPromptHistory::new().prompt_type("command")),
        ["show-prompt-history", "-T", "command"]
    );
    assert_eq!(
        words(&ShowPromptHistory::new()),
        ["show-prompt-history"],
        "no type is every type, which is a thing to say by omission"
    );
}

#[test]
fn display_message_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &DisplayMessage::new()
                .list_formats()
                .keep_updating()
                .forward_stdin()
                .literal()
                .ignore_keys()
                .print()
                .verbose()
                .client("/dev/ttys001")
                .delay("0")
                .format("#{session_name}")
                .target("%1")
                .message("hello")
        ),
        [
            "display-message",
            "-a",
            "-C",
            "-I",
            "-l",
            "-N",
            "-p",
            "-v",
            "-c",
            "/dev/ttys001",
            "-d",
            "0",
            "-F",
            "#{session_name}",
            "-t",
            "%1",
            "--",
            "hello",
        ],
        "tmux refuses -F together with a message; this layer renders what it was asked for \
             and leaves that judgment to tmux"
    );
}

#[test]
fn display_message_reads_a_format_off_a_pane_id() {
    let pane = PaneId::new("%9").expect("a well-formed pane id");
    assert_eq!(
        words(
            &DisplayMessage::new()
                .print()
                .target(&pane)
                .message("#{pane_pid}")
        ),
        ["display-message", "-p", "-t", "%9", "--", "#{pane_pid}"]
    );
}

#[test]
fn display_menu_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &DisplayMenu::new()
                .mouse()
                .click_to_choose()
                .border_lines("rounded")
                .starting_choice("1")
                .client("/dev/ttys001")
                .selected_style("bg=blue")
                .style("bg=black")
                .border_style("fg=grey")
                .title("Panes")
                .x_position("C")
                .y_position("S")
                .target("%1")
                .items(["-Disabled", "", "", "Split", "s", "split-window"])
        ),
        [
            "display-menu",
            "-M",
            "-O",
            "-b",
            "rounded",
            "-C",
            "1",
            "-c",
            "/dev/ttys001",
            "-H",
            "bg=blue",
            "-s",
            "bg=black",
            "-S",
            "fg=grey",
            "-T",
            "Panes",
            "-x",
            "C",
            "-y",
            "S",
            "-t",
            "%1",
            "--",
            "-Disabled",
            "",
            "",
            "Split",
            "s",
            "split-window",
        ],
        "a disabled item's name is spelled exactly like a flag, which is what the fence is for"
    );
}

#[test]
fn display_popup_renders_every_flag_it_has() {
    assert_eq!(
        words(
            &DisplayPopup::new()
                .no_border()
                .close_existing()
                .close_on_exit()
                .any_key()
                .stay_open()
                .border_lines("heavy")
                .client("/dev/ttys001")
                .start_directory("/work")
                .environment("A=1")
                .environment("B=2")
                .height("50%")
                .style("bg=black")
                .border_style("fg=grey")
                .title("build")
                .width("80%")
                .x_position("C")
                .y_position("C")
                .target("%1")
                .command(["sh", "-c", "make"])
        ),
        [
            "display-popup",
            "-B",
            "-C",
            "-E",
            "-k",
            "-N",
            "-b",
            "heavy",
            "-c",
            "/dev/ttys001",
            "-d",
            "/work",
            "-e",
            "A=1",
            "-e",
            "B=2",
            "-h",
            "50%",
            "-s",
            "bg=black",
            "-S",
            "fg=grey",
            "-T",
            "build",
            "-w",
            "80%",
            "-x",
            "C",
            "-y",
            "C",
            "-t",
            "%1",
            "--",
            "sh",
            "-c",
            "make",
        ],
        "-e is one pair per call, and keeps the order the caller enumerated"
    );
}

#[test]
fn display_popup_spells_close_on_success_as_the_manual_does() {
    assert_eq!(
        words(&DisplayPopup::new().close_on_success().command(["make"])),
        ["display-popup", "-EE", "--", "make"],
        "a flag set twice is set once here, so the doubled -E has to arrive as one word"
    );
}

#[test]
fn every_command_in_this_family_is_in_the_registry_once() {
    assert_eq!(ENTRIES.len(), 21, "the roster this module settled on");
    for entry in ENTRIES {
        assert!(
            crate::commands::REGISTRY.contains(entry),
            "{} is declared here but not gathered into the register",
            entry.name
        );
    }
}
