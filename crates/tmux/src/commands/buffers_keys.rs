//! Buffers, keys, the prompt and the display: what a caller reaches for to
//! move text into and out of a server, to rebind it, and to put something in
//! front of a person.
//!
//! Synthesized, not ported — see [`crate::commands`] for the convention and
//! for how a flag's argument is typed.
//!
//! # What is in this family, and why
//!
//! Two of tmux(1)'s own sections, `BUFFERS` and `KEY BINDINGS`, plus the
//! prompt-and-display commands the manual files under `MISCELLANEOUS`
//! alongside things this crate types elsewhere. That is a seam this module
//! draws rather than inherits, so it is worth saying what it is: these
//! commands all carry *a caller's own text* — a paste buffer's contents, the
//! keys to send, the note on a binding, a prompt, a menu item, a popup title
//! — into tmux, or carry it back out. That is one job, and it is the job the
//! `--` fence and the `OsString` argument type exist for.
//!
//! Which is also why `clear-history` is here and not with the panes: tmux's
//! own manual files it under `BUFFERS`, because a pane's scrollback is a
//! buffer question, and [`crate::commands::panes`] says so from the other
//! side. The chooser is likewise split by *what is chosen* rather than by
//! its being a mode: `choose-buffer` is here because it chooses a buffer,
//! while `choose-client` and `choose-tree` are typed in
//! [`options_misc`][crate::commands::options_misc] with the rest of the
//! interactive modes.
//!
//! # Three flags the usage strings do not mention
//!
//! Flags come from the binary's own usage strings, as
//! [`crate::commands`]'s baseline note says. Three flags in this family are
//! **accepted by the parser and documented in the manual** while the
//! command's usage string omits them: `load-buffer -w`, `list-buffers -r`
//! and `choose-buffer -y`. They are typed here, because a usage string is a
//! hand-written sentence in tmux's own source and the parser is the thing
//! that decides — a letter tmux does not know is refused by name
//! (`unknown flag -Q`), and these three are not. Each is marked at its
//! method.
//!
//! # Layout
//!
//! Buffers first — the ones that carry text, then the ones that list and
//! choose it — then keys, then the prompt and the display.

use super::invocations;

invocations! {
    /// Sets a buffer's contents from a caller's own bytes.
    ///
    /// The counterpart of [`ShowBuffer`], and the way text reaches a server
    /// without a file: the data rides argv, so it is bytes rather than a
    /// line, and the `--` this layer emits is what keeps data beginning with
    /// `-` from being read as a flag.
    SetBuffer = "set-buffer", Some("setb") => {
        /// `-a`: appends to the buffer rather than overwriting it.
        append: switch "-a";
        /// `-w`: also sends the buffer to
        /// [`target`][Self::target]'s clipboard, through the xterm escape
        /// sequence, where that is possible.
        clipboard: switch "-w";
        /// `-b buffer-name`: the buffer to set; the most recent
        /// automatically named one if omitted.
        buffer: value "-b";
        /// `-n new-buffer-name`: renames the buffer, which is the only way
        /// an automatically named one becomes explicitly named.
        new_name: value "-n";
        /// `-t target-client`: whose clipboard
        /// [`clipboard`][Self::clipboard] writes to. A client, not a pane.
        target: value "-t";
        /// The contents, behind the `--`.
        data: positional;
    }

    /// Loads a buffer from a file.
    ///
    /// tmux reads `-` as standard input, which this crate's
    /// [`Server`][crate::Server] closes for every call it makes: a
    /// `load-buffer -- -` through this layer therefore loads nothing and
    /// creates no buffer at all. Use [`SetBuffer`] to put a caller's own
    /// bytes in.
    LoadBuffer = "load-buffer", Some("loadb") => {
        /// `-w`: also sends the buffer to
        /// [`target`][Self::target]'s clipboard.
        ///
        /// Documented in tmux's manual and accepted by its parser; the
        /// command's own usage string omits it. See this module's doc.
        clipboard: switch "-w";
        /// `-b buffer-name`: the buffer to load into.
        buffer: value "-b";
        /// `-t target-client`: whose clipboard
        /// [`clipboard`][Self::clipboard] writes to.
        target: value "-t";
        /// The file to read, which is a path and so not obliged to be UTF-8.
        path: positional;
    }

    /// Saves a buffer to a file.
    ///
    /// tmux writes `-` to its own standard output, which is what
    /// [`Captured`][crate::server::Captured] carries back — so
    /// `save-buffer -- -` is [`ShowBuffer`] with an append flag.
    SaveBuffer = "save-buffer", Some("saveb") => {
        /// `-a`: appends to the file rather than overwriting it.
        append: switch "-a";
        /// `-b buffer-name`: the buffer to save.
        buffer: value "-b";
        /// The file to write, or `-` for standard output.
        path: positional;
    }

    /// Prints a buffer's contents.
    ///
    /// The answer arrives as bytes through
    /// [`Captured`][crate::server::Captured], because a buffer holds
    /// whatever was put in it.
    ShowBuffer = "show-buffer", Some("showb") => {
        /// `-b buffer-name`: the buffer to show; the most recent
        /// automatically named one if omitted.
        buffer: value "-b";
    }

    /// Inserts a buffer's contents into a pane, as if typed.
    PasteBuffer = "paste-buffer", Some("pasteb") => {
        /// `-d`: deletes the buffer once it has been pasted.
        delete: switch "-d";
        /// `-p`: wraps the paste in bracketed-paste control codes, when the
        /// application in the pane has asked for them.
        bracketed: switch "-p";
        /// `-r`: replaces no linefeeds, which is
        /// [`separator`][Self::separator] set to a linefeed.
        keep_linefeeds: switch "-r";
        /// `-S`: pastes control characters as they are, rather than through
        /// `vis(3)`.
        unsanitized: switch "-S";
        /// `-b buffer-name`: the buffer to paste.
        buffer: value "-b";
        /// `-s separator`: what each linefeed becomes; a carriage return if
        /// omitted.
        separator: value "-s";
        /// `-t target-pane`: the pane to paste into.
        target: value "-t";
    }

    /// Deletes a buffer.
    DeleteBuffer = "delete-buffer", Some("deleteb") => {
        /// `-b buffer-name`: the buffer to delete; the most recent
        /// automatically named one if omitted.
        buffer: value "-b";
    }

    /// Lists the server's buffers.
    ListBuffers = "list-buffers", Some("lsb") => {
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        ///
        /// Documented in tmux's manual and accepted by its parser; the
        /// command's own usage string omits it. See this module's doc.
        reverse: switch "-r";
        /// `-F format`: the format of each line.
        format: text "-F";
        /// `-f filter`: a format; a buffer it evaluates false for is left
        /// out.
        filter: text "-f";
        /// `-O sort-order`: `name`, `size` or `creation`.
        sort_order: text "-O";
    }

    /// Puts a pane into buffer mode, where a person chooses one.
    ///
    /// Needs an attached client, and runs
    /// [`template`][Self::template] — `paste-buffer -p -b '%%'` if none is
    /// given — with `%%` standing for the chosen buffer's name.
    ChooseBuffer = "choose-buffer", None => {
        /// `-k`: kills the pane when the mode is left.
        kill_on_exit: switch "-k";
        /// `-N`: starts with the preview off.
        no_preview: switch "-N";
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        reverse: switch "-r";
        /// `-y`: answers the mode's confirmation prompts itself.
        ///
        /// Documented in tmux's manual and accepted by its parser; the
        /// command's own usage string omits it. See this module's doc.
        skip_confirmations: switch "-y";
        /// `-Z`: zooms the pane.
        zoom: switch "-Z";
        /// `-F format`: the format of each line.
        format: text "-F";
        /// `-f filter`: a format; a buffer it evaluates false for is left
        /// out, unless that would empty the list.
        filter: text "-f";
        /// `-K key-format`: the format of each line's shortcut key.
        key_format: text "-K";
        /// `-O sort-order`: `creation`, `name` or `size`.
        sort_order: text "-O";
        /// `-t target-pane`: the pane to put into the mode.
        target: value "-t";
        /// The command run for the chosen buffer, with `%%` standing for its
        /// name.
        template: positional;
    }

    /// Frees a pane's scrollback.
    ///
    /// Filed with the buffers because tmux's own manual files it there: a
    /// pane's history is a buffer, and the window commands are
    /// [`crate::commands::panes`]'s.
    ClearHistory = "clear-history", Some("clearhist") => {
        /// `-H`: frees the pane's hyperlinks too, which clearing the history
        /// alone does not.
        hyperlinks: switch "-H";
        /// `-t target-pane`: the pane whose history to free.
        target: value "-t";
    }

    /// Sends keys to a pane, or to a client.
    ///
    /// Each [`key`][Self::key] is a key name such as `C-a` or `NPage`;
    /// anything tmux does not recognise as one is sent as its characters.
    /// [`literal`][Self::literal] turns that lookup off altogether, which is
    /// the flag a caller typing a *line* wants — with it, `Enter` is five
    /// characters rather than the return key.
    ///
    /// The shape a real consumer proved against a live server types the line
    /// literally and then sends the return key as its own call, so that no
    /// text a person supplied can be read as a key name:
    ///
    /// ```
    /// use tmux::commands::SendKeys;
    ///
    /// let typed = SendKeys::new()
    ///     .target("%3")
    ///     .literal()
    ///     .key("--not-a-flag 'and not a key name'")
    ///     .args();
    ///
    /// let words: Vec<_> = typed.iter().map(|word| word.to_string_lossy()).collect();
    /// assert_eq!(
    ///     words,
    ///     ["send-keys", "-t", "%3", "-l", "--", "--not-a-flag 'and not a key name'"]
    /// );
    ///
    /// let entered = SendKeys::new().target("%3").key("Enter").args();
    /// let words: Vec<_> = entered.iter().map(|word| word.to_string_lossy()).collect();
    /// assert_eq!(words, ["send-keys", "-t", "%3", "--", "Enter"]);
    /// ```
    SendKeys = "send-keys", Some("send") => {
        /// `-F`: expands formats in the keys, where that applies.
        expand_formats: switch "-F";
        /// `-H`: reads each key as the hexadecimal number of an ASCII
        /// character.
        hex: switch "-H";
        /// `-K`: sends the keys to [`client`][Self::client], where they are
        /// looked up in that client's key table, rather than to a pane.
        to_client: switch "-K";
        /// `-l`: sends the keys as literal UTF-8 characters, looking no key
        /// name up.
        literal: switch "-l";
        /// `-M`: passes a mouse event through; only valid from a binding on
        /// one.
        mouse: switch "-M";
        /// `-R`: resets the terminal state.
        reset: switch "-R";
        /// `-X`: sends a copy-mode command rather than a key.
        copy_mode: switch "-X";
        /// `-c target-client`: the client [`to_client`][Self::to_client]
        /// sends to.
        client: value "-c";
        /// `-N repeat-count`: how many times to send what follows.
        repeat_count: value "-N";
        /// `-t target-pane`: the pane to send to.
        target: value "-t";
        /// One key, one key name, or — under [`literal`][Self::literal] —
        /// one run of characters. Callable once per key, which tmux sends
        /// first to last.
        key: positional;
    }

    /// Sends the prefix key to a pane, as if it had been pressed.
    SendPrefix = "send-prefix", None => {
        /// `-2`: sends the secondary prefix key instead.
        secondary: switch "-2";
        /// `-t target-pane`: the pane to send to.
        target: value "-t";
    }

    /// Binds a key to a command.
    ///
    /// A binding lives in a key table: the `prefix` table by default, the
    /// `root` table under [`root_table`][Self::root_table], or whatever
    /// [`key_table`][Self::key_table] names. Called with no
    /// [`command`][Self::command], [`repeats`][Self::repeats] and
    /// [`note`][Self::note] alter a binding that already exists.
    BindKey = "bind-key", Some("bind") => {
        /// `-n`: binds in the `root` table, which is
        /// [`key_table`][Self::key_table] set to `root`: the key then works
        /// without the prefix.
        root_table: switch "-n";
        /// `-r`: lets the key repeat, within `repeat-time`.
        repeats: switch "-r";
        /// `-N note`: the note [`ListKeys::notes`] shows; an empty string
        /// clears it.
        note: value "-N";
        /// `-T key-table`: the table to bind in.
        key_table: value "-T";
        /// The key to bind, behind the `--` — which is what lets a key
        /// spelled with a leading `-` be bound at all.
        key: positional;
        /// The command and its arguments, after the key.
        command: trailing;
    }

    /// Removes a key binding.
    UnbindKey = "unbind-key", Some("unbind") => {
        /// `-a`: removes every binding, rather than the one
        /// [`key`][Self::key] names.
        all: switch "-a";
        /// `-n`: unbinds from the `root` table, as
        /// [`BindKey::root_table`] binds into it.
        root_table: switch "-n";
        /// `-q`: says nothing when there was nothing to unbind.
        quiet: switch "-q";
        /// `-T key-table`: the table to unbind from.
        key_table: value "-T";
        /// The key to unbind.
        key: positional;
    }

    /// Lists key bindings.
    ///
    /// Two forms: the default one prints each binding as the
    /// [`BindKey`] command that would recreate it, and
    /// [`notes`][Self::notes] prints each key with its note instead.
    ListKeys = "list-keys", Some("lsk") => {
        /// `-1`: lists only the first matching key.
        first_only: switch "-1";
        /// `-a`: under [`notes`][Self::notes], prints the command for keys
        /// carrying no note rather than passing over them.
        include_unnoted: switch "-a";
        /// `-N`: prints each key with its note, and only the keys of the
        /// `root` and `prefix` tables unless
        /// [`key_table`][Self::key_table] names one.
        notes: switch "-N";
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        reverse: switch "-r";
        /// `-F format`: the format of each line.
        format: text "-F";
        /// `-O sort-order`: `key`, `modifier` or `name`, the last being the
        /// table's.
        sort_order: text "-O";
        /// `-P prefix-string`: printed before each key.
        prefix_string: value "-P";
        /// `-T key-table`: the table to list.
        key_table: value "-T";
        /// One key to list, rather than all of them.
        key: positional;
    }

    /// Opens the command prompt in a client.
    ///
    /// [`template`][Self::template] is the command the prompt runs, with
    /// `%%` and `%1` standing for the first answer, `%2` for the second, and
    /// so on to `%9`; `%%%` is `%%` with its quotation marks escaped.
    CommandPrompt = "command-prompt", None => {
        /// `-1`: takes one key press, so the answer is one character.
        one_key: switch "-1";
        /// `-b`: shows the prompt in the background, and the invoking client
        /// stays alive until it is dismissed.
        background: switch "-b";
        /// `-C`: keeps the panes updating while the prompt is up.
        keep_updating: switch "-C";
        /// `-e`: makes backspace cancel an empty prompt.
        backspace_cancels: switch "-e";
        /// `-F`: expands [`template`][Self::template] as a format.
        expand_template: switch "-F";
        /// `-i`: runs the command on every change to the input, rather than
        /// when the prompt is left.
        on_change: switch "-i";
        /// `-k`: [`one_key`][Self::one_key], with the press translated to a
        /// key name.
        one_key_name: switch "-k";
        /// `-l`: reads [`inputs`][Self::inputs] and
        /// [`prompts`][Self::prompts] literally, splitting neither at its
        /// commas.
        literal: switch "-l";
        /// `-N`: accepts numeric key presses only.
        numeric: switch "-N";
        /// `-P`: opens the prompt inside a pane rather than on the status
        /// line.
        in_pane: switch "-P";
        /// `-I inputs`: the initial text of each prompt, comma-separated
        /// unless [`literal`][Self::literal].
        inputs: value "-I";
        /// `-p prompts`: the prompts themselves, shown in order and
        /// comma-separated unless [`literal`][Self::literal].
        prompts: value "-p";
        /// `-t target-client`: the client to open the prompt in.
        target: value "-t";
        /// `-T prompt-type`: what to complete on Tab — `command` or
        /// `search`.
        prompt_type: text "-T";
        /// The command the prompt runs, as one string.
        template: positional;
    }

    /// Asks for confirmation, then runs a command.
    ConfirmBefore = "confirm-before", Some("confirm") => {
        /// `-b`: shows the prompt in the background, and the invoking client
        /// stays alive until it is dismissed.
        background: switch "-b";
        /// `-y`: makes Enter alone run the command.
        default_yes: switch "-y";
        /// `-c confirm-key`: the key that confirms; `y` if omitted.
        confirm_key: value "-c";
        /// `-p prompt`: what to ask; built from the command if omitted. It
        /// may carry the same `#` sequences `status-left` does.
        prompt: value "-p";
        /// `-t target-client`: the client to ask.
        target: value "-t";
        /// The command to run once confirmed, as one string.
        command: positional;
    }

    /// Forgets the status prompt's history.
    ClearPromptHistory = "clear-prompt-history", Some("clearphist") => {
        /// `-T prompt-type`: which history to clear — see
        /// [`CommandPrompt::prompt_type`]. All of them if omitted.
        prompt_type: text "-T";
    }

    /// Prints the status prompt's history.
    ShowPromptHistory = "show-prompt-history", Some("showphist") => {
        /// `-T prompt-type`: which history to print — see
        /// [`CommandPrompt::prompt_type`]. All of them if omitted.
        prompt_type: text "-T";
    }

    /// Displays a message, on a client's status line or on standard output.
    ///
    /// With [`print`][Self::print] the answer comes back through
    /// [`Captured`][crate::server::Captured], which is how a caller reads a
    /// format the server evaluated: `display-message -p '#{session_name}'`.
    /// The message is a format unless [`literal`][Self::literal] says
    /// otherwise — and it is either the positional
    /// [`message`][Self::message] or [`format`][Self::format], never both,
    /// which tmux refuses in its own words.
    DisplayMessage = "display-message", Some("display") => {
        /// `-a`: lists the format variables and their values instead.
        list_formats: switch "-a";
        /// `-C`: keeps the pane updating while the message is up.
        keep_updating: switch "-C";
        /// `-I`: forwards this call's standard input to the empty pane
        /// [`target`][Self::target] names — which for a call made through
        /// [`Server`][crate::Server] is closed, and so forwards nothing.
        forward_stdin: switch "-I";
        /// `-l`: prints the message unchanged rather than as a format.
        literal: switch "-l";
        /// `-N`: ignores key presses, closing only when
        /// [`delay`][Self::delay] runs out.
        ignore_keys: switch "-N";
        /// `-p`: prints to standard output rather than to a status line.
        print: switch "-p";
        /// `-v`: prints verbose logging as the format is parsed.
        verbose: switch "-v";
        /// `-c target-client`: whose status line to write to.
        client: value "-c";
        /// `-d delay`: milliseconds to stay up; zero waits for a key, and
        /// `display-time` decides if this is omitted.
        delay: value "-d";
        /// `-F format`: the message, as an alternative to
        /// [`message`][Self::message] rather than an addition to it.
        format: text "-F";
        /// `-t target-pane`: the pane the format reads from.
        target: value "-t";
        /// The message, which is a format unless
        /// [`literal`][Self::literal].
        message: positional;
    }

    /// Displays a menu on a client.
    ///
    /// The menu is [`items`][Self::items]: a flat run of name, key and
    /// command, three words per entry. A name beginning with `-` is a
    /// disabled entry and an empty name is a separator — which is the whole
    /// reason this layer's `--` fence matters here, since a disabled entry's
    /// name is spelled exactly like a flag.
    DisplayMenu = "display-menu", Some("menu") => {
        /// `-M`: handles mouse events; by default only a menu opened from a
        /// mouse binding does.
        mouse: switch "-M";
        /// `-O`: keeps the menu up when a mouse button is released over no
        /// item, so an item must be clicked to be chosen.
        click_to_choose: switch "-O";
        /// `-b border-lines`: the characters the border is drawn with; the
        /// values `popup-border-lines` takes.
        border_lines: text "-b";
        /// `-C starting-choice`: the item selected to begin with, unless the
        /// menu is bound to a mouse key.
        starting_choice: value "-C";
        /// `-c target-client`: the client to display on.
        client: value "-c";
        /// `-H selected-style`: the style of the selected item.
        selected_style: text "-H";
        /// `-s style`: the style of the menu.
        style: text "-s";
        /// `-S border-style`: the style of its border.
        border_style: text "-S";
        /// `-T title`: the menu's title, as a format.
        title: text "-T";
        /// `-x position`: a column, one of tmux's position letters, or a
        /// format.
        x_position: value "-x";
        /// `-y position`: a line, one of tmux's position letters, or a
        /// format.
        y_position: value "-y";
        /// `-t target-pane`: the target any command the menu runs is run
        /// against.
        target: value "-t";
        /// The menu: name, key and command per entry, laid end to end
        /// behind the `--`.
        items: trailing;
    }

    /// Displays a popup running a command over a client's panes.
    ///
    /// Run from inside a popup this modifies that popup, and tmux then reads
    /// only [`border_lines`][Self::border_lines],
    /// [`no_border`][Self::no_border], [`close_existing`][Self::close_existing],
    /// the two close-on-exit flags, [`any_key`][Self::any_key],
    /// [`stay_open`][Self::stay_open], [`style`][Self::style] and
    /// [`border_style`][Self::border_style], ignoring the rest.
    DisplayPopup = "display-popup", Some("popup") => {
        /// `-B`: draws no border, which makes
        /// [`border_lines`][Self::border_lines] moot.
        no_border: switch "-B";
        /// `-C`: closes whatever popup the client already has.
        close_existing: switch "-C";
        /// `-E`: closes the popup when the command exits.
        close_on_exit: switch "-E";
        /// `-E -E`: closes it only when the command exits successfully.
        ///
        /// Spelled `-EE` because that is the option tmux's own manual names,
        /// and because a flag set twice is set once here — see
        /// [`crate::commands`]. It is an alternative to
        /// [`close_on_exit`][Self::close_on_exit], not an addition.
        close_on_success: switch "-EE";
        /// `-k`: lets any key dismiss the popup, rather than only Escape or
        /// `C-c`.
        any_key: switch "-k";
        /// `-N`: undoes the [`close_on_exit`][Self::close_on_exit],
        /// [`close_on_success`][Self::close_on_success] or
        /// [`any_key`][Self::any_key] a previous call set on this popup.
        stay_open: switch "-N";
        /// `-b border-lines`: the characters the border is drawn with; the
        /// values `popup-border-lines` takes.
        border_lines: text "-b";
        /// `-c target-client`: the client to display on.
        client: value "-c";
        /// `-d start-directory`: the command's working directory. tmux
        /// spells this `-d` here and `-c` on
        /// [`SplitWindow`][crate::commands::SplitWindow], where `-c` is
        /// already the client.
        start_directory: value "-d";
        /// `-e environment`: one `NAME=VALUE` pair, and callable once per
        /// variable.
        environment: repeat "-e";
        /// `-h height`: lines, or a percentage such as `50%`; half the
        /// terminal if omitted.
        height: value "-h";
        /// `-s style`: the style of the popup.
        style: text "-s";
        /// `-S border-style`: the style of its border.
        border_style: text "-S";
        /// `-T title`: the popup's title, as a format.
        title: text "-T";
        /// `-w width`: columns, or a percentage; half the terminal if
        /// omitted.
        width: value "-w";
        /// `-x position`: a column, one of tmux's position letters, or a
        /// format.
        x_position: value "-x";
        /// `-y position`: a line, one of tmux's position letters, or a
        /// format.
        y_position: value "-y";
        /// `-t target-pane`: the pane the popup's formats read from.
        target: value "-t";
        /// The program and its arguments. Two or more words are execvp'd
        /// directly, behind the `--` that keeps them out of the flags; **one
        /// word is handed to the person's login shell**, which parses it and
        /// sources their startup files first — so anything not written by the
        /// caller itself travels as a program plus arguments, never one
        /// string (the workspace's D502 lesson). `default-command` if
        /// omitted.
        command: trailing;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PaneId, commands::Invocation};

    /// Every assertion below reads argv as text, because every assertion
    /// below is about which words tmux is handed; the two that are about
    /// bytes say so, and build their values out of them.
    fn words<I: Invocation>(invocation: &I) -> Vec<String> {
        invocation
            .args()
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

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
                    .kill_on_exit()
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
                "-k",
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
                    .in_pane()
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
                "-P",
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
}
