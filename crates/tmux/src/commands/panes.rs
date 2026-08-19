//! Windows and panes: what tmux's own manual files under that heading,
//! minus the interactive modes.
//!
//! Synthesized, not ported — see [`crate::commands`] for the convention and
//! for how a flag's argument is typed.
//!
//! # What is in this family, and why
//!
//! The roster is tmux(1)'s own `WINDOWS AND PANES` section, which is a
//! boundary somebody else already drew and defended: it puts `clear-history`
//! under `BUFFERS` (a pane's scrollback is a buffer question) and
//! `list-clients` under `CLIENTS AND SESSIONS`, so this module does not have
//! to invent a rule for either. Five of that section's commands are left out
//! all the same — `choose-client`, `choose-tree`, `copy-mode`,
//! `customize-mode` and `switch-mode` — because they put a pane into an
//! interactive mode and wait for a person, which makes them relatives of the
//! prompt and the chooser rather than of splitting and resizing; they are
//! typed in [`options_misc`][crate::commands::options_misc], whose own doc
//! says why it holds them.
//!
//! # Layout
//!
//! Panes first, then windows, then layouts — the order somebody reaching for
//! one of these reads in, rather than the manual's alphabetical order, which
//! interleaves the three.

use super::invocations;

invocations! {
    /// Splits `target-pane` in two and runs a command in the new half.
    ///
    /// The shape a real consumer proved against a live server is `-d -P -F`
    /// with a working directory, some environment, and the program behind
    /// the separator: detached so a person's focus does not move, `-P -F` so
    /// the new pane's id comes back from the same call that made it — a
    /// second call to look it up would already be racing whatever recycled
    /// the id — and `--` so a program named like a flag cannot be read as
    /// one.
    ///
    /// ```
    /// use tmux::commands::SplitWindow;
    ///
    /// let argv = SplitWindow::new()
    ///     .detached()
    ///     .print()
    ///     .format("#{pane_id}")
    ///     .start_directory("/tmp")
    ///     .environment("TERM=screen-256color")
    ///     .environment("GANJA_AGENT_ID=w1")
    ///     .command(["sh", "-c", "exec my-agent"])
    ///     .args();
    ///
    /// let words: Vec<_> = argv.iter().map(|word| word.to_string_lossy()).collect();
    /// assert_eq!(
    ///     words,
    ///     [
    ///         "split-window", "-d", "-P", "-F", "#{pane_id}", "-c", "/tmp",
    ///         "-e", "TERM=screen-256color", "-e", "GANJA_AGENT_ID=w1",
    ///         "--", "sh", "-c", "exec my-agent",
    ///     ]
    /// );
    /// ```
    ///
    /// A one-word command is handed to the person's login shell, which
    /// sources their startup files; whether that matters is the consumer's
    /// judgment and not this crate's, but it is the reason a caller who has
    /// deliberately enumerated an environment passes a program and its
    /// arguments rather than one string.
    SplitWindow = "split-window", Some("splitw") => {
        /// `-b`: puts the new pane left of, or above, the target.
        before: switch "-b";
        /// `-d`: leaves the current pane active.
        detached: switch "-d";
        /// `-f`: spans the full window height or width instead of splitting
        /// the active pane.
        full_size: switch "-f";
        /// `-h`: splits horizontally. Neither this nor `-v` means `-v`.
        horizontal: switch "-h";
        /// `-I`: creates an empty pane and forwards this process's standard
        /// input into it.
        stdin: switch "-I";
        /// `-k`: keeps the pane open once the command exits, until a key is
        /// pressed.
        keep_open: switch "-k";
        /// `-P`: prints the new pane, in [`format`][Self::format].
        print: switch "-P";
        /// `-v`: splits vertically, which is also the default.
        vertical: switch "-v";
        /// `-W`: waits for the command to exit and returns its status.
        wait: switch "-W";
        /// `-Z`: zooms the window, or keeps it zoomed if it already was.
        zoom: switch "-Z";
        /// `-B border-lines`: the border lines for a floating pane.
        border_lines: text "-B";
        /// `-c start-directory`: the new pane's working directory.
        start_directory: value "-c";
        /// `-e environment`: one `NAME=VALUE` pair, and callable once per
        /// variable.
        environment: repeat "-e";
        /// `-F format`: what [`print`][Self::print] answers with.
        format: text "-F";
        /// `-l size`: lines, columns, or a percentage such as `20%`.
        size: value "-l";
        /// `-m message`: [`keep_open`][Self::keep_open], with this pane's
        /// own `remain-on-exit-format`.
        message: value "-m";
        /// `-p percentage`: shorthand for a percentage [`size`][Self::size].
        percentage: value "-p";
        /// `-s style`: the style of the pane's content.
        style: text "-s";
        /// `-S active-border-style`: the border style while the pane is
        /// active.
        active_border_style: text "-S";
        /// `-R inactive-border-style`: the border style while it is not.
        inactive_border_style: text "-R";
        /// `-T title`: the pane's title.
        title: value "-T";
        /// `-t target-pane`: the pane to split.
        target: value "-t";
        /// The program and its arguments. Two or more words are execvp'd
        /// directly, behind the `--` that keeps them out of the flags; **one
        /// word is handed to the person's login shell**, which parses it and
        /// sources their startup files first — so anything not written by the
        /// caller itself travels as a program plus arguments, never one
        /// string (the workspace's D502 lesson).
        command: trailing;
    }

    /// Creates a floating pane over `target-pane`.
    ///
    /// Everything [`SplitWindow`] means, plus a position and a size, plus
    /// the modal behavior a floating pane can have. With
    /// [`split_like`][Self::split_like] it behaves as `split-window` does,
    /// which is how one binding serves both.
    NewPane = "new-pane", Some("newp") => {
        /// `-b`: puts the new pane left of, or above, the target.
        before: switch "-b";
        /// `-C`: closes a modal pane when the mouse is clicked outside it.
        close_modal_on_click: switch "-C";
        /// `-d`: leaves the current pane active.
        detached: switch "-d";
        /// `-f`: spans the full window height or width.
        full_size: switch "-f";
        /// `-h`: splits horizontally.
        horizontal: switch "-h";
        /// `-I`: creates an empty pane fed from this process's standard
        /// input.
        stdin: switch "-I";
        /// `-k`: keeps the pane open once the command exits.
        keep_open: switch "-k";
        /// `-L`: behaves like [`SplitWindow`] instead of floating.
        split_like: switch "-L";
        /// `-M`: follows a mouse drag, for a binding on one.
        mouse: switch "-M";
        /// `-O`: makes the pane modal — always active, and blocking every
        /// other pane while it lives.
        modal: switch "-O";
        /// `-P`: prints the new pane, in [`format`][Self::format].
        print: switch "-P";
        /// `-v`: splits vertically.
        vertical: switch "-v";
        /// `-W`: waits for the command to exit and returns its status.
        wait: switch "-W";
        /// `-Z`: zooms the window.
        zoom: switch "-Z";
        /// `-B border-lines`: the floating pane's border lines.
        border_lines: text "-B";
        /// `-c start-directory`: the new pane's working directory.
        start_directory: value "-c";
        /// `-e environment`: one `NAME=VALUE` pair, callable once per
        /// variable.
        environment: repeat "-e";
        /// `-F format`: what [`print`][Self::print] answers with.
        format: text "-F";
        /// `-l size`: lines, columns, or a percentage.
        size: value "-l";
        /// `-m message`: [`keep_open`][Self::keep_open] with its own message.
        message: value "-m";
        /// `-p percentage`: shorthand for a percentage [`size`][Self::size].
        percentage: value "-p";
        /// `-s style`: the style of the pane's content.
        style: text "-s";
        /// `-S active-border-style`: the border style while active.
        active_border_style: text "-S";
        /// `-R inactive-border-style`: the border style while inactive.
        inactive_border_style: text "-R";
        /// `-T title`: the pane's title.
        title: value "-T";
        /// `-x width`: columns, or a percentage of the window.
        width: value "-x";
        /// `-y height`: lines, or a percentage of the window.
        height: value "-y";
        /// `-X x-position`: the upper-left corner's column.
        x_position: value "-X";
        /// `-Y y-position`: the upper-left corner's line.
        y_position: value "-Y";
        /// `-t target-pane`: the pane to float over.
        target: value "-t";
        /// The program and its arguments, behind the `--`.
        command: trailing;
    }

    /// Makes a pane the active one in its window, or marks it.
    ///
    /// The marked pane is what `-s` defaults to for [`JoinPane`],
    /// [`MovePane`], [`SwapPane`] and [`SwapWindow`], which is why
    /// [`mark`][Self::mark] belongs to a command that otherwise only moves
    /// the cursor.
    SelectPane = "select-pane", Some("selectp") => {
        /// `-D`: selects the pane below the target instead.
        down: switch "-D";
        /// `-d`: disables input to the pane.
        disable_input: switch "-d";
        /// `-e`: enables input to the pane.
        enable_input: switch "-e";
        /// `-L`: selects the pane to the left of the target.
        left: switch "-L";
        /// `-l`: selects the last-used pane, as [`LastPane`] does.
        last: switch "-l";
        /// `-M`: clears the marked pane.
        clear_marked: switch "-M";
        /// `-m`: marks this pane, clearing any other mark — there is one.
        mark: switch "-m";
        /// `-R`: selects the pane to the right of the target.
        right: switch "-R";
        /// `-U`: selects the pane above the target.
        up: switch "-U";
        /// `-Z`: keeps the window zoomed if it was.
        keep_zoomed: switch "-Z";
        /// `-T title`: sets the pane's title.
        title: value "-T";
        /// `-t target-pane`: the pane to select.
        target: value "-t";
    }

    /// Selects the previously selected pane.
    LastPane = "last-pane", Some("lastp") => {
        /// `-d`: disables input to the pane.
        disable_input: switch "-d";
        /// `-e`: enables input to the pane.
        enable_input: switch "-e";
        /// `-Z`: keeps the window zoomed if it was.
        keep_zoomed: switch "-Z";
        /// `-t target-window`: the window whose last pane to select.
        target: value "-t";
    }

    /// Destroys a pane, and its window with it if nothing else remains.
    KillPane = "kill-pane", Some("killp") => {
        /// `-a`: kills every pane in the window *except* the target.
        all_others: switch "-a";
        /// `-f filter`: with [`all_others`][Self::all_others], kills only
        /// the panes the filter is true for.
        filter: text "-f";
        /// `-t target-pane`: the pane to kill, or to spare under
        /// [`all_others`][Self::all_others].
        target: value "-t";
    }

    /// Lists panes: this window's, this session's, or the whole server's.
    ///
    /// Answers with lines, one per pane, in whatever
    /// [`format`][Self::format] asked for — which is why the crate parses
    /// none of it: what the columns mean was the caller's decision.
    ListPanes = "list-panes", Some("lsp") => {
        /// `-a`: every pane on the server, ignoring the target.
        all: switch "-a";
        /// `-s`: reads the target as a session rather than a window.
        session: switch "-s";
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        reverse: switch "-r";
        /// `-F format`: what each line looks like.
        format: text "-F";
        /// `-f filter`: shows only the panes it is true for.
        filter: text "-f";
        /// `-O sort-order`: one of `name`, `index`, `size`, `creation` or
        /// `activity`.
        sort_order: text "-O";
        /// `-t target-window`: the window, or the session under
        /// [`session`][Self::session].
        target: value "-t";
    }

    /// Moves a pane out of its window and into a window of its own.
    ///
    /// The inverse of [`JoinPane`], and — with [`floating`][Self::floating]
    /// — the way a tiled pane is lifted out of the layout instead.
    BreakPane = "break-pane", Some("breakp") => {
        /// `-a`: puts the new window at the next index after the target.
        after: switch "-a";
        /// `-b`: puts it at the index before.
        before: switch "-b";
        /// `-d`: leaves the current window current.
        detached: switch "-d";
        /// `-P`: prints the new window, in [`format`][Self::format].
        print: switch "-P";
        /// `-W`: lifts the pane out of the tiled layout and floats it
        /// instead of making a window.
        floating: switch "-W";
        /// `-F format`: what [`print`][Self::print] answers with; the
        /// default is `#{session_name}:#{window_index}.#{pane_index}`.
        format: text "-F";
        /// `-n window-name`: names the new window.
        window_name: value "-n";
        /// `-s src-pane`: the pane to break off.
        source: value "-s";
        /// `-t dst-window`: where the new window goes.
        target: value "-t";
        /// `-x width`: a floating pane's width.
        width: value "-x";
        /// `-y height`: a floating pane's height.
        height: value "-y";
        /// `-X x-position`: a floating pane's left edge.
        x_position: value "-X";
        /// `-Y y-position`: a floating pane's top edge.
        y_position: value "-Y";
    }

    /// Splits a pane and moves an existing pane into the space.
    ///
    /// [`BreakPane`] undone. With no [`source`][Self::source] and a marked
    /// pane present, the marked one is used.
    JoinPane = "join-pane", Some("joinp") => {
        /// `-b`: joins the source left of, or above, the destination.
        before: switch "-b";
        /// `-d`: leaves the current pane active.
        detached: switch "-d";
        /// `-f`: spans the full window height or width.
        full_size: switch "-f";
        /// `-h`: splits horizontally.
        horizontal: switch "-h";
        /// `-v`: splits vertically.
        vertical: switch "-v";
        /// `-l size`: lines, columns, or a percentage.
        size: value "-l";
        /// `-s src-pane`: the pane to move.
        source: value "-s";
        /// `-t dst-pane`: the pane to split for it.
        target: value "-t";
    }

    /// Joins a pane, or moves a floating one.
    ///
    /// [`JoinPane`] until one of the movement flags is given, at which point
    /// it moves the target floating pane instead — the same command name for
    /// two jobs is tmux's arrangement, kept rather than split in two here.
    MovePane = "move-pane", Some("movep") => {
        /// `-b`: joins the source left of, or above, the destination.
        before: switch "-b";
        /// `-d`: leaves the current pane active.
        detached: switch "-d";
        /// `-f`: spans the full window height or width.
        full_size: switch "-f";
        /// `-h`: splits horizontally.
        horizontal: switch "-h";
        /// `-M`: begins a mouse drag, for a binding on one.
        mouse: switch "-M";
        /// `-v`: splits vertically.
        vertical: switch "-v";
        /// `-D lines`: moves a floating pane down, one line if unsaid.
        down: value "-D";
        /// `-l size`: lines, columns, or a percentage.
        size: value "-l";
        /// `-L columns`: moves a floating pane left.
        left: value "-L";
        /// `-P position`: one of tmux's named positions, such as `centre`,
        /// `top-left` or `forward-loop`.
        position: text "-P";
        /// `-R columns`: moves a floating pane right.
        right: value "-R";
        /// `-s src-pane`: the pane to move.
        source: value "-s";
        /// `-t dst-pane`: the pane to split for it.
        target: value "-t";
        /// `-U lines`: moves a floating pane up.
        up: value "-U";
        /// `-X x-position`: moves it to an absolute column.
        x_position: value "-X";
        /// `-Y y-position`: moves it to an absolute line.
        y_position: value "-Y";
        /// `-z z-index`: moves it in the stack; zero is the front.
        z_index: value "-z";
    }

    /// Exchanges two panes without moving either one's position.
    SwapPane = "swap-pane", Some("swapp") => {
        /// `-d`: leaves the active pane where it was.
        detached: switch "-d";
        /// `-D`: swaps with the next pane when no source is named.
        next: switch "-D";
        /// `-U`: swaps with the previous pane when no source is named.
        previous: switch "-U";
        /// `-Z`: keeps the window zoomed if it was.
        keep_zoomed: switch "-Z";
        /// `-s src-pane`: one of the two; the marked pane if omitted.
        source: value "-s";
        /// `-t dst-pane`: the other.
        target: value "-t";
    }

    /// Resizes a pane, by an adjustment or to a size.
    ResizePane = "resize-pane", Some("resizep") => {
        /// `-M`: begins a mouse resize, for a binding on one.
        mouse: switch "-M";
        /// `-T`: trims the lines below the cursor, pulling history up to
        /// replace them.
        trim: switch "-T";
        /// `-Z`: toggles the active pane between zoomed and not.
        zoom: switch "-Z";
        /// `-D lines`: grows downward; a floating pane's bottom border, and
        /// then a negative value is allowed.
        down: value "-D";
        /// `-L columns`: grows leftward.
        left: value "-L";
        /// `-R columns`: grows rightward.
        right: value "-R";
        /// `-U lines`: grows upward.
        up: value "-U";
        /// `-x width`: an absolute width, in columns or as `10%`.
        width: value "-x";
        /// `-y height`: an absolute height, in lines or as `10%`.
        height: value "-y";
        /// `-t target-pane`: the pane to resize.
        target: value "-t";
    }

    /// Runs a command again in a pane whose own has exited.
    RespawnPane = "respawn-pane", Some("respawnp") => {
        /// `-E`: leaves the pane with nothing running.
        empty: switch "-E";
        /// `-k`: kills whatever is still running first, instead of refusing.
        kill_running: switch "-k";
        /// `-c start-directory`: a new working directory for the pane.
        start_directory: value "-c";
        /// `-e environment`: one `NAME=VALUE` pair, callable once per
        /// variable.
        environment: repeat "-e";
        /// `-t target-pane`: the pane to respawn.
        target: value "-t";
        /// The program and its arguments; the previous one if omitted.
        command: trailing;
    }

    /// Copies a pane's visible contents, or its history, into a buffer or
    /// onto standard output.
    ///
    /// With [`stdout`][Self::stdout] the capture comes back through
    /// [`Captured`][crate::server::Captured] — which is bytes, because
    /// [`escape_sequences`][Self::escape_sequences] and a pane's own output
    /// put things there that are not text.
    CapturePane = "capture-pane", Some("capturep") => {
        /// `-a`: captures the alternate screen; there is no history on it.
        alternate_screen: switch "-a";
        /// `-C`: escapes non-printable bytes as octal `\xxx`.
        escape_octal: switch "-C";
        /// `-e`: keeps the escape sequences for colour and attributes.
        escape_sequences: switch "-e";
        /// `-F`: prefixes each line with its flags.
        line_flags: switch "-F";
        /// `-H`: captures only the hyperlinks in the given lines.
        hyperlinks: switch "-H";
        /// `-J`: keeps trailing spaces and joins wrapped lines; implies
        /// [`ignore_trailing`][Self::ignore_trailing].
        join_wrapped: switch "-J";
        /// `-L`: prefixes each line with its number.
        line_numbers: switch "-L";
        /// `-M`: captures the mode's screen when the pane is in one.
        mode_screen: switch "-M";
        /// `-N`: keeps the trailing spaces at each line's end.
        keep_trailing_spaces: switch "-N";
        /// `-p`: writes to standard output instead of a buffer.
        stdout: switch "-p";
        /// `-P`: captures only an as-yet incomplete escape sequence.
        pending: switch "-P";
        /// `-q`: says nothing when there is no alternate screen.
        quiet: switch "-q";
        /// `-R`: dumps the internal grid, for diagnostics.
        grid_dump: switch "-R";
        /// `-T`: ignores trailing positions holding no character.
        ignore_trailing: switch "-T";
        /// `-b buffer-name`: the buffer to capture into.
        buffer: value "-b";
        /// `-E end-line`: the last line; `-` is the end of the visible pane.
        end_line: value "-E";
        /// `-S start-line`: the first line; zero is the top of the visible
        /// pane, a negative number is history, and `-` is its start.
        start_line: value "-S";
        /// `-t target-pane`: the pane to capture.
        target: value "-t";
    }

    /// Connects a pane to a shell command, in either direction.
    ///
    /// Called with no [`shell_command`][Self::shell_command], it closes
    /// whatever pipe the pane already had — a pane carries at most one.
    PipePane = "pipe-pane", Some("pipep") => {
        /// `-I`: connects the command's standard output to the pane, as if
        /// typed into it.
        input: switch "-I";
        /// `-O`: connects the pane's output to the command's standard input,
        /// which is also the default.
        output: switch "-O";
        /// `-o`: only opens a pipe if there is none, so one binding toggles.
        toggle: switch "-o";
        /// `-t target-pane`: the pane to pipe.
        target: value "-t";
        /// The shell command, as one string tmux hands to `sh`; it may carry
        /// the same `#` sequences `status-left` does.
        shell_command: positional;
    }

    /// Shows each pane's number over the window and waits for a choice.
    DisplayPanes = "display-panes", Some("displayp") => {
        /// `-k`: kills the pane when the mode is left.
        kill_on_exit: switch "-k";
        /// `-N`: stays up until the time runs out rather than until a key.
        ignore_keys: switch "-N";
        /// `-Z`: starts unzoomed; the mode is zoomed by default.
        unzoomed: switch "-Z";
        /// `-d duration`: milliseconds to stay up; zero means until a key.
        duration: value "-d";
        /// `-s source-window`: shows this window's panes instead of the
        /// target's.
        source_window: value "-s";
        /// `-t target-pane`: the pane to put into the mode.
        target: value "-t";
        /// The command run for the chosen pane, with `%%` standing for its
        /// id; the default is `select-pane -t '%%'`.
        template: positional;
    }

    /// Creates a window and runs a command in it.
    NewWindow = "new-window", Some("neww") => {
        /// `-a`: inserts at the next index after the target.
        after: switch "-a";
        /// `-b`: inserts at the index before it.
        before: switch "-b";
        /// `-d`: leaves the current window current.
        detached: switch "-d";
        /// `-E`: creates the first pane with nothing running in it.
        empty: switch "-E";
        /// `-k`: destroys whatever already occupies the target index.
        kill_existing: switch "-k";
        /// `-P`: prints the new window, in [`format`][Self::format].
        print: switch "-P";
        /// `-S`: selects the existing window of that name instead of making
        /// a second one.
        select_existing: switch "-S";
        /// `-c start-directory`: the new window's working directory.
        start_directory: value "-c";
        /// `-e environment`: one `NAME=VALUE` pair, callable once per
        /// variable.
        environment: repeat "-e";
        /// `-F format`: what [`print`][Self::print] answers with; the
        /// default is `#{session_name}:#{window_index}`.
        format: text "-F";
        /// `-n window-name`: names the new window.
        window_name: value "-n";
        /// `-t target-window`: where it goes.
        target: value "-t";
        /// The program and its arguments. Two or more words are execvp'd
        /// directly, behind the `--` that keeps them out of the flags; **one
        /// word is handed to the person's login shell**, which parses it and
        /// sources their startup files first — so anything not written by the
        /// caller itself travels as a program plus arguments, never one
        /// string (the workspace's D502 lesson).
        command: trailing;
    }

    /// Destroys a window, unlinking it from every session holding it.
    KillWindow = "kill-window", Some("killw") => {
        /// `-a`: kills every window in the session *except* the target.
        all_others: switch "-a";
        /// `-f filter`: with [`all_others`][Self::all_others], kills only
        /// the windows the filter is true for.
        filter: text "-f";
        /// `-t target-window`: the window to kill, or to spare.
        target: value "-t";
    }

    /// Lists windows: this session's, or the whole server's.
    ListWindows = "list-windows", Some("lsw") => {
        /// `-a`: every window on the server.
        all: switch "-a";
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        reverse: switch "-r";
        /// `-F format`: what each line looks like.
        format: text "-F";
        /// `-f filter`: shows only the windows it is true for.
        filter: text "-f";
        /// `-O sort-order`: one of `index`, `name`, `size`, `creation` or
        /// `activity`.
        sort_order: text "-O";
        /// `-t target-session`: the session to list.
        target: value "-t";
    }

    /// Makes a window the current one.
    SelectWindow = "select-window", Some("selectw") => {
        /// `-l`: the last-used window, as [`LastWindow`] does.
        last: switch "-l";
        /// `-n`: the next one, as [`NextWindow`] does.
        next: switch "-n";
        /// `-p`: the previous one, as [`PreviousWindow`] does.
        previous: switch "-p";
        /// `-T`: falls back to the last-used window when the target is
        /// already current, so one binding toggles.
        toggle: switch "-T";
        /// `-t target-window`: the window to select.
        target: value "-t";
    }

    /// Moves to the next window in the session.
    NextWindow = "next-window", Some("next") => {
        /// `-a`: the next window carrying an alert.
        with_alert: switch "-a";
        /// `-t target-session`: the session to move within.
        target: value "-t";
    }

    /// Moves to the previous window in the session.
    PreviousWindow = "previous-window", Some("prev") => {
        /// `-a`: the previous window carrying an alert.
        with_alert: switch "-a";
        /// `-t target-session`: the session to move within.
        target: value "-t";
    }

    /// Selects the previously selected window.
    LastWindow = "last-window", Some("last") => {
        /// `-t target-session`: the session whose last window to select.
        target: value "-t";
    }

    /// Renames a window.
    RenameWindow = "rename-window", Some("renamew") => {
        /// `-t target-window`: the window to rename.
        target: value "-t";
        /// The new name.
        new_name: positional;
    }

    /// Moves a window to another index, or another session.
    MoveWindow = "move-window", Some("movew") => {
        /// `-a`: moves to the next index after the destination.
        after: switch "-a";
        /// `-b`: moves to the index before it.
        before: switch "-b";
        /// `-d`: leaves the current window current.
        detached: switch "-d";
        /// `-k`: destroys whatever already occupies the destination.
        kill_existing: switch "-k";
        /// `-r`: renumbers every window in the session, respecting
        /// `base-index`.
        renumber: switch "-r";
        /// `-s src-window`: the window to move.
        source: value "-s";
        /// `-t dst-window`: where it goes.
        target: value "-t";
    }

    /// Exchanges two windows.
    SwapWindow = "swap-window", Some("swapw") => {
        /// `-d`: leaves the current window current.
        detached: switch "-d";
        /// `-s src-window`: one of the two; the marked pane's window if
        /// omitted.
        source: value "-s";
        /// `-t dst-window`: the other.
        target: value "-t";
    }

    /// Links one window into a second place, so two sessions hold the same
    /// one.
    LinkWindow = "link-window", Some("linkw") => {
        /// `-a`: links at the next index after the destination.
        after: switch "-a";
        /// `-b`: links at the index before it.
        before: switch "-b";
        /// `-d`: leaves the newly linked window unselected.
        detached: switch "-d";
        /// `-k`: destroys whatever already occupies the destination, rather
        /// than failing.
        kill_existing: switch "-k";
        /// `-s src-window`: the window to link.
        source: value "-s";
        /// `-t dst-window`: where to link it.
        target: value "-t";
    }

    /// Removes a window from one session, leaving it in the others.
    ///
    /// A window may not be linked to no sessions, so unlinking the last link
    /// needs [`destroy`][Self::destroy].
    UnlinkWindow = "unlink-window", Some("unlinkw") => {
        /// `-k`: destroys the window when this was its only link.
        destroy: switch "-k";
        /// `-t target-window`: the link to remove.
        target: value "-t";
    }

    /// Runs a command again in a window whose own has exited.
    RespawnWindow = "respawn-window", Some("respawnw") => {
        /// `-E`: leaves the window with one pane and nothing running.
        empty: switch "-E";
        /// `-k`: kills whatever is still running first, instead of refusing.
        kill_running: switch "-k";
        /// `-c start-directory`: a new working directory for the window.
        start_directory: value "-c";
        /// `-e environment`: one `NAME=VALUE` pair, callable once per
        /// variable.
        environment: repeat "-e";
        /// `-t target-window`: the window to respawn.
        target: value "-t";
        /// The program and its arguments; the previous one if omitted.
        command: trailing;
    }

    /// Resizes a window, by an adjustment or to a size.
    ///
    /// tmux sets the window's `window-size` option to `manual` as a side
    /// effect, which is a thing a caller should know before reaching for it.
    ResizeWindow = "resize-window", Some("resizew") => {
        /// `-a`: sizes to the smallest session holding the window.
        smallest: switch "-a";
        /// `-A`: sizes to the largest.
        largest: switch "-A";
        /// `-D`: shrinks downward by the [`adjustment`][Self::adjustment].
        down: switch "-D";
        /// `-L`: leftward.
        left: switch "-L";
        /// `-R`: rightward.
        right: switch "-R";
        /// `-U`: upward.
        up: switch "-U";
        /// `-x width`: an absolute width.
        width: value "-x";
        /// `-y height`: an absolute height.
        height: value "-y";
        /// `-t target-window`: the window to resize.
        target: value "-t";
        /// How far the direction flags move, in lines or cells; one if
        /// omitted.
        adjustment: positional;
    }

    /// Rotates the panes within a window through their positions.
    RotateWindow = "rotate-window", Some("rotatew") => {
        /// `-D`: rotates toward numerically higher positions.
        downward: switch "-D";
        /// `-U`: rotates toward numerically lower ones.
        upward: switch "-U";
        /// `-Z`: keeps the window zoomed if it was.
        keep_zoomed: switch "-Z";
        /// `-t target-window`: the window to rotate.
        target: value "-t";
    }

    /// Searches window names, titles and visible contents.
    ///
    /// Needs an attached client, and does not search history — a pane's
    /// scrollback is [`CapturePane`]'s question.
    FindWindow = "find-window", Some("findw") => {
        /// `-C`: matches the visible contents only.
        match_contents: switch "-C";
        /// `-i`: ignores case.
        ignore_case: switch "-i";
        /// `-N`: matches the window name only.
        match_name: switch "-N";
        /// `-r`: reads the pattern as a regular expression rather than a
        /// glob.
        regex: switch "-r";
        /// `-T`: matches the window title only.
        match_title: switch "-T";
        /// `-Z`: zooms the pane that matched.
        zoom: switch "-Z";
        /// `-t target-pane`: where to show the results.
        target: value "-t";
        /// What to look for: a glob, or a regular expression under
        /// [`regex`][Self::regex].
        pattern: positional;
    }

    /// Moves a window to the next layout and refits its panes.
    NextLayout = "next-layout", Some("nextl") => {
        /// `-t target-window`: the window to relayout.
        target: value "-t";
    }

    /// Moves a window to the previous layout.
    PreviousLayout = "previous-layout", Some("prevl") => {
        /// `-t target-window`: the window to relayout.
        target: value "-t";
    }

    /// Applies a layout to a window.
    SelectLayout = "select-layout", Some("selectl") => {
        /// `-E`: spreads the current pane and its neighbours out evenly.
        spread: switch "-E";
        /// `-n`: the next layout, as [`NextLayout`] does.
        next: switch "-n";
        /// `-o`: undoes the most recent layout change.
        undo: switch "-o";
        /// `-p`: the previous layout, as [`PreviousLayout`] does.
        previous: switch "-p";
        /// `-t target-pane`: a pane in the window to lay out.
        target: value "-t";
        /// The layout: one of tmux's five names, or a layout string it
        /// printed earlier. The last preset layout if omitted.
        layout: positional;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PaneId, WindowId, commands::Invocation};

    /// Every assertion below reads argv as text, because every assertion
    /// below is about which words tmux is handed rather than about bytes;
    /// the one test that *is* about bytes lives in the parent module beside
    /// the accumulator it exercises.
    fn words<I: Invocation>(invocation: &I) -> Vec<String> {
        invocation
            .args()
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

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
                    .wait()
                    .zoom()
                    .border_lines("rounded")
                    .start_directory("/work")
                    .environment("A=1")
                    .format("#{pane_id}")
                    .size("20%")
                    .message("done")
                    .percentage("30")
                    .style("bg=black")
                    .active_border_style("fg=green")
                    .inactive_border_style("fg=grey")
                    .title("worker")
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
                "-W",
                "-Z",
                "-B",
                "rounded",
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
                "-T",
                "worker",
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
                    .modal()
                    .close_modal_on_click()
                    .split_like()
                    .mouse()
                    .width("60%")
                    .height("40%")
                    .x_position("10")
                    .y_position("2")
                    .target("%0")
            ),
            [
                "new-pane", "-d", "-O", "-C", "-L", "-M", "-x", "60%", "-y", "40%", "-X", "10",
                "-Y", "2", "-t", "%0",
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
            words(
                &KillPane::new()
                    .all_others()
                    .filter("#{==:#{pane_dead},1}")
                    .target("%1")
            ),
            ["kill-pane", "-a", "-f", "#{==:#{pane_dead},1}", "-t", "%1",]
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
                    .floating()
                    .format("#{window_id}")
                    .window_name("scratch")
                    .source("%4")
                    .target("work:9")
                    .width("80")
                    .height("24")
                    .x_position("0")
                    .y_position("0")
            ),
            [
                "break-pane",
                "-a",
                "-b",
                "-d",
                "-P",
                "-W",
                "-F",
                "#{window_id}",
                "-n",
                "scratch",
                "-s",
                "%4",
                "-t",
                "work:9",
                "-x",
                "80",
                "-y",
                "24",
                "-X",
                "0",
                "-Y",
                "0",
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
                    .mouse()
                    .before()
                    .detached()
                    .full_size()
                    .horizontal()
                    .vertical()
                    .down("2")
                    .left("3")
                    .position("centre")
                    .right("4")
                    .up("1")
                    .x_position("10")
                    .y_position("5")
                    .z_index("0")
            ),
            [
                "move-pane",
                "-M",
                "-b",
                "-d",
                "-f",
                "-h",
                "-v",
                "-D",
                "2",
                "-L",
                "3",
                "-P",
                "centre",
                "-R",
                "4",
                "-U",
                "1",
                "-X",
                "10",
                "-Y",
                "5",
                "-z",
                "0",
            ]
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
                    .empty()
                    .kill_running()
                    .start_directory("/srv")
                    .environment("A=1")
                    .environment("B=2")
                    .target("%2")
                    .command(["sh", "-c", "true"])
            ),
            [
                "respawn-pane",
                "-E",
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
                    .grid_dump()
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
                "-R",
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
                    .kill_on_exit()
                    .ignore_keys()
                    .unzoomed()
                    .duration("2000")
                    .source_window("@1")
                    .target("%0")
                    .template("select-pane -t '%%'")
            ),
            [
                "display-panes",
                "-k",
                "-N",
                "-Z",
                "-d",
                "2000",
                "-s",
                "@1",
                "-t",
                "%0",
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
                    .empty()
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
                "-E",
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
            words(
                &KillWindow::new()
                    .all_others()
                    .filter("#{==:#{window_name},scratch}")
                    .target("@1")
            ),
            [
                "kill-window",
                "-a",
                "-f",
                "#{==:#{window_name},scratch}",
                "-t",
                "@1",
            ]
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
                    .empty()
                    .kill_running()
                    .start_directory("/srv")
                    .environment("A=1")
                    .target("@1")
                    .command(["true"])
            ),
            [
                "respawn-window",
                "-E",
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
}
