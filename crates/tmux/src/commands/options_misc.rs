//! Options, hooks, the environment, the interactive modes, and the four
//! commands tmux's own manual files under `MISCELLANEOUS`.
//!
//! Synthesized, not ported — see [`crate::commands`] for the convention and
//! for how a flag's argument is typed.
//!
//! # What is in this family, and why
//!
//! Four rosters that share one property: none of them is about making,
//! moving or destroying a pane, a window, a session or a buffer, which is
//! what the other three families are for. The options and their two
//! window-scoped synonyms; the hooks, which tmux's manual documents beside
//! the options because their flags *are* the options' flags; the session
//! environment; the five commands that put a pane into an interactive mode
//! and wait for a person — named in [`panes`][crate::commands::panes]'s own
//! doc as belonging here rather than there; and `MISCELLANEOUS`, which is
//! tmux's own word for the four that are left.
//!
//! The modes are the reason this family is not simply "options": a mode
//! command is spelled like every other command and can be sent by any
//! caller, but what it does is hand a pane to somebody with a keyboard.
//! This crate ships the words; whether a caller has a person to hand a pane
//! to is the caller's question, as [`crate::server`]'s doc says of `$TMUX`.
//!
//! # Where tmux's two accounts of a flag disagree
//!
//! Everywhere else in this crate the binary's own usage string
//! (`tmux list-commands <name>`) settles what a command's flags are. In
//! this family it is settled six times by asking the parser as well, because
//! the usage string and the manual shipped beside the same binary do not
//! agree about the modes. The rule applied below is: **a flag is a method
//! when the parser accepts it and at least one of those two documents names
//! it**, and the six exceptions are these.
//!
//! - `choose-tree -i`: the manual's synopsis lists it, the usage string does
//!   not, and the parser answers `unknown flag -i`. Not a method — the
//!   manual is describing [`ChooseClient::information`], whose command does
//!   accept it.
//! - `choose-tree -y` and `choose-client -y`: the usage strings omit them,
//!   the manual documents both as disabling confirmation prompts, and the
//!   parser takes them. Methods:
//!   [`ChooseTree::no_confirm`]/[`ChooseClient::no_confirm`].
//! - `choose-client -i`: the usage string lists it and the manual's synopsis
//!   does not, though its prose explains exactly what it does. A method.
//! - `customize-mode -y` and `run-shell -s`: the parser accepts both and
//!   neither document names either, so there is nothing to write a doc line
//!   from — and a method whose doc would be a guess is worse than no
//!   method. Left to [`Server::run`][crate::Server::run], which carries
//!   them today.
//! - `customize-mode`'s trailing `[template]`: the manual's synopsis has
//!   one, and the parser refuses any positional at all
//!   (`too many arguments (need at most 0)`). Not a method.
//!
//! # Layout
//!
//! Options, then hooks, then the environment, then the modes, then the four
//! miscellaneous commands — the order tmux's own manual introduces them in,
//! which is also the order somebody reaching for one of them arrives by.

use super::invocations;

invocations! {
    /// Sets one option, at whichever scope the flags name.
    ///
    /// tmux infers the scope from the option's own name where it can, so
    /// [`window`][Self::window] and [`server`][Self::server] are needed
    /// mainly for a user option — one whose name begins with `@` — which
    /// belongs to nothing in particular and must therefore be told where it
    /// lives.
    SetOption = "set-option", Some("set") => {
        /// `-a`: appends to the existing value rather than replacing it,
        /// for a string or a style option.
        append: switch "-a";
        /// `-F`: expands formats in the value before storing it.
        expand_formats: switch "-F";
        /// `-g`: the global session or window option.
        global: switch "-g";
        /// `-o`: refuses to set an option that is already set.
        only_if_unset: switch "-o";
        /// `-p`: a pane option.
        pane: switch "-p";
        /// `-q`: says nothing about an unknown or ambiguous option.
        quiet: switch "-q";
        /// `-s`: a server option.
        server: switch "-s";
        /// `-u`: unsets the option, so this scope inherits it again — or,
        /// with [`global`][Self::global], restores tmux's own default.
        unset: switch "-u";
        /// `-U`: [`unset`][Self::unset], and for a pane option unsets it on
        /// every pane in the window as well.
        unset_in_panes: switch "-U";
        /// `-w`: a window option.
        window: switch "-w";
        /// `-t target-pane`: whose options to set.
        target: value "-t";
        /// The option's name; one element of an array option is
        /// `name[key]`.
        option: positional;
        /// The new value. Omitted, a flag or choice option toggles.
        value: positional;
    }

    /// Reads options back: one, or every option at a scope.
    ///
    /// [`inherited`][Self::inherited] is what makes this a *read* rather
    /// than a listing of what somebody happened to set here: without it an
    /// option a pane inherits from the window, the session or the global
    /// scope is simply absent from the answer, which reads identically to
    /// an option that is off.
    ///
    /// ```
    /// use tmux::commands::ShowOptions;
    ///
    /// let argv = ShowOptions::new()
    ///     .window()
    ///     .quiet()
    ///     .value_only()
    ///     .inherited()
    ///     .target("%3")
    ///     .option("pane-border-status")
    ///     .args();
    ///
    /// let words: Vec<_> = argv.iter().map(|word| word.to_string_lossy()).collect();
    /// assert_eq!(
    ///     words,
    ///     [
    ///         "show-options", "-w", "-q", "-v", "-A", "-t", "%3",
    ///         "--", "pane-border-status",
    ///     ]
    /// );
    /// ```
    ///
    /// That is the shape a real consumer already asks a live server: the
    /// window-scoped value of one option for one pane, quietly, inherited
    /// values included, so that "unset" and "off" can be told apart before
    /// anything is written. tmux reads `-w -q -v -A` and `-wqvA` alike; this
    /// layer always spells them apart, because a builder has no reason to
    /// fold what it did not have to parse.
    ShowOptions = "show-options", Some("show") => {
        /// `-A`: includes options inherited from a parent scope, each
        /// marked with an asterisk.
        inherited: switch "-A";
        /// `-g`: the global session or window options.
        global: switch "-g";
        /// `-H`: includes hooks, which are left out by default.
        hooks: switch "-H";
        /// `-p`: the pane options.
        pane: switch "-p";
        /// `-q`: says nothing when the option is unset.
        quiet: switch "-q";
        /// `-s`: the server options.
        server: switch "-s";
        /// `-v`: prints the value alone, without the option's name.
        value_only: switch "-v";
        /// `-w`: the window options.
        window: switch "-w";
        /// `-F format`: what each line looks like.
        format: text "-F";
        /// `-t target-pane`: whose options to show.
        target: value "-t";
        /// One option to show; every option at the scope if omitted.
        option: positional;
    }

    /// Sets a window option: [`SetOption`] with the window scope built in.
    ///
    /// tmux keeps this as a command of its own rather than an alias, and so
    /// does this crate — but it carries fewer flags than [`SetOption`] does,
    /// because the scope selectors it would have to contradict are not
    /// offered.
    SetWindowOption = "set-window-option", Some("setw") => {
        /// `-a`: appends to the existing value rather than replacing it.
        append: switch "-a";
        /// `-F`: expands formats in the value before storing it.
        expand_formats: switch "-F";
        /// `-g`: the global window option.
        global: switch "-g";
        /// `-o`: refuses to set an option that is already set.
        only_if_unset: switch "-o";
        /// `-q`: says nothing about an unknown or ambiguous option.
        quiet: switch "-q";
        /// `-u`: unsets the option, so the window inherits it again.
        unset: switch "-u";
        /// `-t target-window`: whose option to set.
        target: value "-t";
        /// The option's name.
        option: positional;
        /// The new value. Omitted, a flag or choice option toggles.
        value: positional;
    }

    /// Reads window options back: [`ShowOptions`] scoped to a window.
    ///
    /// Note what is missing beside [`ShowOptions`]: this command has no `-A`
    /// of its own, so a caller who needs to tell an inherited value from an
    /// unset one asks [`ShowOptions::window`] instead.
    ShowWindowOptions = "show-window-options", Some("showw") => {
        /// `-g`: the global window options.
        global: switch "-g";
        /// `-v`: prints the value alone, without the option's name.
        value_only: switch "-v";
        /// `-F format`: what each line looks like.
        format: text "-F";
        /// `-t target-window`: whose options to show.
        target: value "-t";
        /// One option to show; every window option if omitted.
        option: positional;
    }

    /// Sets a hook — a command tmux runs when something happens.
    ///
    /// The scope flags are [`SetOption`]'s, because a hook *is* an option:
    /// the same name reaches it through `set-option`, and
    /// [`ShowOptions::hooks`] lists it among the others.
    ///
    /// Three of the flags do something other than setting: [`fire`] raises a
    /// user event rather than storing anything, [`run_now`] runs the named
    /// hook immediately, and [`monitor`] installs a subscription that
    /// re-evaluates a format once a second.
    ///
    /// [`fire`]: Self::fire
    /// [`run_now`]: Self::run_now
    /// [`monitor`]: Self::monitor
    SetHook = "set-hook", None => {
        /// `-a`: appends to the hook already stored under this name.
        append: switch "-a";
        /// `-E`: fires the user event [`hook`][Self::hook] names, whose
        /// name must begin with `@`.
        fire: switch "-E";
        /// `-g`: the global hook.
        global: switch "-g";
        /// `-p`: a pane hook.
        pane: switch "-p";
        /// `-R`: runs the named hook immediately.
        run_now: switch "-R";
        /// `-T`: with [`monitor`][Self::monitor], runs the hook only while
        /// the subscription's format is true.
        when_true: switch "-T";
        /// `-u`: unsets the hook, or removes the subscription
        /// [`monitor`][Self::monitor] names.
        unset: switch "-u";
        /// `-w`: a window hook.
        window: switch "-w";
        /// `-B name:what:format`: a monitor subscription, in the same
        /// syntax `refresh-client -B` takes — `name` is the hook to run and
        /// must begin with `@`, `what` selects the session, pane, window,
        /// all panes or all windows, and `format` is expanded once a
        /// second.
        ///
        /// Set twice, the last wins: tmux reads one subscription per call,
        /// so a second `-B` in the same argv replaces rather than adds.
        monitor: text "-B";
        /// `-t target-pane`: whose hook to set.
        target: value "-t";
        /// The hook's name, or the user event's under
        /// [`fire`][Self::fire].
        hook: positional;
        /// The tmux command the hook runs.
        command: positional;
    }

    /// Reads hooks back, or the subscriptions behind them.
    ///
    /// The flags are [`ShowOptions`]'s, minus the scope selectors a hook
    /// cannot have.
    ShowHooks = "show-hooks", None => {
        /// `-B`: shows the subscriptions [`SetHook::monitor`] installed
        /// rather than the hooks themselves.
        subscriptions: switch "-B";
        /// `-g`: the global hooks.
        global: switch "-g";
        /// `-p`: the pane hooks.
        pane: switch "-p";
        /// `-w`: the window hooks.
        window: switch "-w";
        /// `-F format`: what each line looks like.
        format: text "-F";
        /// `-t target-pane`: whose hooks to show.
        target: value "-t";
        /// One hook to show; every hook at the scope if omitted.
        hook: positional;
    }

    /// Sets, unsets or hides one variable in a session's environment.
    ///
    /// This is the environment a *new* process started by that session
    /// inherits, which is why it is a session's property rather than a
    /// pane's — and why [`removed`][Self::removed] exists at all: a
    /// variable can be recorded here precisely so that it is taken *out* of
    /// what a new process is handed.
    SetEnvironment = "set-environment", Some("setenv") => {
        /// `-F`: expands formats in the value before storing it.
        expand_formats: switch "-F";
        /// `-g`: the global environment.
        global: switch "-g";
        /// `-h`: marks the variable hidden — tmux itself may read it, a new
        /// process may not.
        hidden: switch "-h";
        /// `-r`: records that the variable is to be removed from the
        /// environment of a new process.
        removed: switch "-r";
        /// `-u`: unsets the variable.
        unset: switch "-u";
        /// `-t target-session`: whose environment to change.
        target: value "-t";
        /// The variable's name.
        variable: positional;
        /// Its value, which is bytes: this layer hands argv to execve, so a
        /// value that is not UTF-8 arrives as it was given.
        value: positional;
    }

    /// Reads a session's environment back.
    ShowEnvironment = "show-environment", Some("showenv") => {
        /// `-g`: the global environment.
        global: switch "-g";
        /// `-h`: shows hidden variables, which are left out by default.
        hidden: switch "-h";
        /// `-s`: prints Bourne shell commands rather than `NAME=value`
        /// lines.
        shell: switch "-s";
        /// `-t target-session`: whose environment to show.
        target: value "-t";
        /// One variable to show; the whole environment if omitted. A
        /// variable marked [`removed`][SetEnvironment::removed] is printed
        /// with a leading `-`.
        variable: positional;
    }

    /// Puts a pane into copy mode, where its history can be scrolled and
    /// selected.
    ///
    /// [`cancel`][Self::cancel] is the odd one out: it leaves every mode
    /// rather than entering this one, which is what makes a single binding
    /// able to toggle.
    CopyMode = "copy-mode", None => {
        /// `-d`: scrolls one page down as it enters.
        page_down: switch "-d";
        /// `-e`: leaves copy mode when scrolling reaches the bottom of the
        /// history, unless a selection is present.
        exit_at_bottom: switch "-e";
        /// `-H`: hides the position indicator in the top right.
        hide_position: switch "-H";
        /// `-k`: kills the pane when the mode is left.
        kill_on_exit: switch "-k";
        /// `-M`: begins a mouse drag, for a binding on one.
        mouse: switch "-M";
        /// `-q`: cancels copy mode, and every other mode with it.
        cancel: switch "-q";
        /// `-S`: scrolls when bound to a mouse drag event; see the
        /// `scroll-to-mouse` option.
        scroll_to_mouse: switch "-S";
        /// `-u`: scrolls one page up as it enters.
        page_up: switch "-u";
        /// `-s src-pane`: copies from this pane instead of the target.
        source: value "-s";
        /// `-t target-pane`: the pane to put into the mode.
        target: value "-t";
    }

    /// Displays a large clock in a pane.
    ClockMode = "clock-mode", None => {
        /// `-t target-pane`: the pane to put into the mode.
        target: value "-t";
    }

    /// Puts a pane into customize mode, where options and key bindings are
    /// browsed and changed from a list.
    CustomizeMode = "customize-mode", None => {
        /// `-k`: kills the pane when the mode is left.
        kill_on_exit: switch "-k";
        /// `-N`: starts without the option information.
        without_information: switch "-N";
        /// `-Z`: zooms the pane.
        zoom: switch "-Z";
        /// `-F format`: what each item in the list looks like.
        format: text "-F";
        /// `-f filter`: a format; an item it evaluates to zero for is not
        /// shown, and a filter that would empty the list is ignored.
        filter: text "-f";
        /// `-t target-pane`: the pane to put into the mode.
        target: value "-t";
    }

    /// Puts a pane into switch mode, where a session or window is chosen
    /// from a list narrowed by typing.
    SwitchMode = "switch-mode", None => {
        /// `-k`: kills the pane when the mode is left.
        kill_on_exit: switch "-k";
        /// `-s`: lists sessions, which is also the default.
        sessions: switch "-s";
        /// `-w`: lists windows.
        windows: switch "-w";
        /// `-Z`: zooms the pane.
        zoom: switch "-Z";
        /// `-F format`: what each item in the list looks like.
        format: text "-F";
        /// `-t target-pane`: the pane to put into the mode.
        target: value "-t";
        /// The command run for the chosen item, with `%%` standing for it
        /// and `%1` for every further mention; the default is
        /// `switch-client -Zt '%%'`. tmux spells this argument `command`.
        template: positional;
    }

    /// Puts a pane into client mode, where an attached client is chosen
    /// from a list.
    ///
    /// Works only while at least one client is attached, which is a thing a
    /// caller driving a detached server should know before reaching for it.
    ChooseClient = "choose-client", None => {
        /// `-h`: hides the pane the mode is in.
        hide_pane: switch "-h";
        /// `-i`: shows client information instead of the preview.
        information: switch "-i";
        /// `-k`: kills the pane when the mode is left.
        kill_on_exit: switch "-k";
        /// `-N`: starts without the preview.
        ///
        /// tmux reads a *second* `-N` as "with the larger preview", which
        /// this builder cannot spell: a switch asked for twice is one flag
        /// here, by the rule [`crate::commands`] states. A caller who wants
        /// the larger preview passes the words to
        /// [`Server::run`][crate::Server::run] itself.
        without_preview: switch "-N";
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        reverse: switch "-r";
        /// `-y`: skips any confirmation prompt.
        no_confirm: switch "-y";
        /// `-Z`: zooms the pane.
        zoom: switch "-Z";
        /// `-F format`: what each item in the list looks like.
        format: text "-F";
        /// `-f filter`: a format; an item it evaluates to zero for is not
        /// shown.
        filter: text "-f";
        /// `-K key-format`: what each shortcut key looks like.
        key_format: text "-K";
        /// `-O sort-order`: one of `name`, `size`, `creation` or
        /// `activity`.
        sort_order: text "-O";
        /// `-t target-pane`: the pane to put into the mode.
        target: value "-t";
        /// The command run for the chosen client, with `%%` standing for
        /// its name; the default is `detach-client -t '%%'`.
        template: positional;
    }

    /// Puts a pane into tree mode, where a session, window or pane is
    /// chosen from a tree.
    ///
    /// Works only while at least one client is attached.
    ChooseTree = "choose-tree", None => {
        /// `-G`: includes every session in a session group, rather than the
        /// first alone.
        all_session_groups: switch "-G";
        /// `-h`: hides the pane the mode is in.
        hide_pane: switch "-h";
        /// `-k`: kills the pane when the mode is left.
        kill_on_exit: switch "-k";
        /// `-N`: starts without the preview; a second `-N` would ask for
        /// the larger one, which this builder cannot spell — see
        /// [`ChooseClient::without_preview`].
        without_preview: switch "-N";
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        reverse: switch "-r";
        /// `-s`: starts with sessions collapsed.
        collapsed_sessions: switch "-s";
        /// `-w`: starts with windows collapsed.
        collapsed_windows: switch "-w";
        /// `-y`: skips any confirmation prompt.
        no_confirm: switch "-y";
        /// `-Z`: zooms the pane.
        zoom: switch "-Z";
        /// `-F format`: what each item in the tree looks like.
        format: text "-F";
        /// `-f filter`: a format; an item it evaluates to zero for is not
        /// shown.
        filter: text "-f";
        /// `-K key-format`: what each shortcut key looks like.
        key_format: text "-K";
        /// `-O sort-order`: one of `index`, `name`, `activity` or `z`.
        sort_order: text "-O";
        /// `-t target-pane`: the pane to put into the mode.
        target: value "-t";
        /// The command run for the chosen item, with `%%` standing for it;
        /// the default is `switch-client -t '%%'`.
        template: positional;
    }

    /// Runs the commands in one or more files.
    ///
    /// [`path`][Self::path] is callable once per file, in the order tmux
    /// should read them — and each may be a `glob(7)` pattern, which is why
    /// a single call can still name more files than words.
    SourceFile = "source-file", Some("source") => {
        /// `-F`: expands formats in each path.
        expand_formats: switch "-F";
        /// `-n`: parses the file without running anything in it.
        parse_only: switch "-n";
        /// `-q`: says nothing when a path does not exist.
        quiet: switch "-q";
        /// `-v`: prints the parsed commands, with line numbers where it
        /// can.
        verbose: switch "-v";
        /// `-t target-pane`: the pane the commands run against.
        target: value "-t";
        /// One file to read, callable once per file.
        path: positional;
    }

    /// Runs a shell command — or, with
    /// [`tmux_command`][Self::tmux_command], a tmux one — without making a
    /// window for it.
    ///
    /// The command is one string handed to `/bin/sh`, not a program and its
    /// arguments: what [`arguments`][Self::arguments] adds is available to
    /// the string as `#{1}`, `#{2}` and so on, which is a substitution
    /// rather than an argv.
    RunShell = "run-shell", Some("run") => {
        /// `-b`: runs it in the background.
        background: switch "-b";
        /// `-C`: runs the argument as a tmux command rather than through
        /// `/bin/sh`.
        tmux_command: switch "-C";
        /// `-E`: redirects the command's standard error onto its standard
        /// output instead of discarding it.
        stderr_to_stdout: switch "-E";
        /// `-c start-directory`: the directory to run it in.
        start_directory: value "-c";
        /// `-d delay`: seconds to wait before starting.
        delay: value "-d";
        /// `-t target-pane`: where output is displayed when
        /// [`tmux_command`][Self::tmux_command] is not given.
        target: value "-t";
        /// The command, as one string.
        shell_command: positional;
        /// The values the command reads as `#{1}`, `#{2}` and so on.
        arguments: trailing;
    }

    /// Runs one tmux command or another, according to a shell command's
    /// exit status.
    IfShell = "if-shell", Some("if") => {
        /// `-b`: runs the shell command in the background.
        background: switch "-b";
        /// `-F`: does not run the first argument at all, and takes it as
        /// true when it is neither empty nor zero once formats are
        /// expanded.
        as_format: switch "-F";
        /// `-t target-pane`: the pane whose formats the shell command is
        /// expanded against.
        target: value "-t";
        /// The shell command whose success decides, or the format under
        /// [`as_format`][Self::as_format].
        shell_command: positional;
        /// The tmux command run when it succeeds.
        command: positional;
        /// The tmux command run when it does not.
        otherwise: positional;
    }

    /// Waits on, signals or locks a channel — tmux's own rendezvous between
    /// commands.
    ///
    /// With no flag at all, the client is held until a `wait-for -S` on the
    /// same channel wakes it, which is what makes this the one command in
    /// this crate whose whole point is not returning yet.
    WaitFor = "wait-for", Some("wait") => {
        /// `-E`: waits for the next event of that name — a hook, a
        /// notification, or a user `@` event.
        event: switch "-E";
        /// `-l`: lists the waiters on the channel.
        list: switch "-l";
        /// `-L`: locks the channel, so anything else locking it waits.
        lock: switch "-L";
        /// `-S`: wakes whatever is waiting on the channel.
        signal: switch "-S";
        /// `-U`: unlocks the channel.
        unlock: switch "-U";
        /// `-v`: prints the event's payload keys, whether or not
        /// [`format`][Self::format] is true.
        payload_keys: switch "-v";
        /// `-F format`: with [`event`][Self::event], a format that must
        /// also be true.
        format: text "-F";
        /// `-w waiter`: wakes this one waiter immediately.
        waiter: value "-w";
        /// The channel, or the event's name under [`event`][Self::event].
        name: positional;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Invocation;

    /// Every assertion below reads argv as text, for the reason
    /// [`panes`][crate::commands::panes]'s own tests give; the one test here
    /// that is about bytes says so in its name.
    fn words<I: Invocation>(invocation: &I) -> Vec<String> {
        invocation
            .args()
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

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
                    .format("#{option_name}")
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
                "-F",
                "#{option_name}",
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
                    .format("#{option_value}")
                    .target("@1")
                    .option("pane-border-status")
            ),
            [
                "show-window-options",
                "-g",
                "-v",
                "-F",
                "#{option_value}",
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
                    .fire()
                    .global()
                    .pane()
                    .run_now()
                    .when_true()
                    .unset()
                    .window()
                    .monitor("@alert:pane:#{pane_dead}")
                    .target("%1")
                    .hook("pane-exited")
                    .command("display-message gone")
            ),
            [
                "set-hook",
                "-a",
                "-E",
                "-g",
                "-p",
                "-R",
                "-T",
                "-u",
                "-w",
                "-B",
                "@alert:pane:#{pane_dead}",
                "-t",
                "%1",
                "--",
                "pane-exited",
                "display-message gone",
            ]
        );
    }

    #[test]
    fn a_second_subscription_replaces_the_first_because_tmux_reads_one() {
        assert_eq!(
            words(
                &SetHook::new()
                    .monitor("@one:pane:#{pane_id}")
                    .monitor("@two:pane:#{pane_id}")
            ),
            ["set-hook", "-B", "@two:pane:#{pane_id}"],
            "a live server keeps only the last -B of a call, so the builder must not send two"
        );
    }

    #[test]
    fn show_hooks_can_list_subscriptions_instead() {
        assert_eq!(
            words(
                &ShowHooks::new()
                    .subscriptions()
                    .global()
                    .pane()
                    .window()
                    .format("#{hook}")
                    .target("%1")
                    .hook("pane-exited")
            ),
            [
                "show-hooks",
                "-B",
                "-g",
                "-p",
                "-w",
                "-F",
                "#{hook}",
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
                    .kill_on_exit()
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
                "-k",
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
                    .kill_on_exit()
                    .without_information()
                    .zoom()
                    .format("#{option_name}")
                    .filter("#{m:pane-*,#{option_name}}")
                    .target("%1")
            ),
            [
                "customize-mode",
                "-k",
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
                    .hide_pane()
                    .information()
                    .kill_on_exit()
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
                "-h",
                "-i",
                "-k",
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
                    .hide_pane()
                    .kill_on_exit()
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
                "-h",
                "-k",
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
            words(
                &WaitFor::new()
                    .event()
                    .list()
                    .lock()
                    .signal()
                    .unlock()
                    .payload_keys()
                    .format("#{pane_id}")
                    .waiter("w1")
                    .name("build-done")
            ),
            [
                "wait-for",
                "-E",
                "-l",
                "-L",
                "-S",
                "-U",
                "-v",
                "-F",
                "#{pane_id}",
                "-w",
                "w1",
                "--",
                "build-done",
            ]
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
}
