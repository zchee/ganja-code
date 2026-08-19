//! Sessions, the clients attached to them, and the server underneath both.
//!
//! Synthesized, not ported — see [`crate::commands`] for the convention and
//! for how a flag's argument is typed.
//!
//! # What is in this family, and why
//!
//! The roster is tmux(1)'s own `CLIENTS AND SESSIONS` section, with two
//! deliberate departures — the same "somebody else already drew the
//! boundary" rule [`panes`][crate::commands::panes] follows, corrected
//! where that boundary splits something this crate should not.
//!
//! - `source-file` is in that section and is **not** here. It reads a
//!   configuration file and runs the commands in it, which makes it a
//!   relative of `set-option` and `show-options` rather than of attaching
//!   and detaching; it stays named in
//!   [`EXCLUDED`][crate::commands::EXCLUDED] against the wave that takes
//!   the options.
//! - `lock-server` is filed under `MISCELLANEOUS` and **is** here, because
//!   the manual defines the other two locks in terms of it — `lock-client`
//!   reads "see the lock-server command" — and a trio the manual
//!   cross-references should not be split across two waves.
//!
//! # Where the flags came from
//!
//! From the binary's own usage strings, cross-checked against the
//! next-3.8 manual, and — where the two disagreed — from what the binary
//! actually accepts, probed against a private socket. Two of these
//! commands' usage strings are stale in opposite directions, so neither
//! source alone would have been right:
//!
//! - `list-clients` prints no `-r` in its usage line, but the manual
//!   documents one and the binary accepts it. It is typed, as
//!   [`ListClients::reverse`].
//! - `server-access` prints `[-t target-pane]` in its usage line, but the
//!   manual has no such flag and the binary answers `unknown flag -t`. It
//!   is **not** typed, because a method for it could only ever build argv
//!   tmux refuses.
//!
//! # Two transports name the same commands
//!
//! Several of these have a counterpart on a control-mode connection, and
//! `refresh-client` is the one that matters: it is how a control client
//! sets its size, its flags and its format subscriptions. A caller holding
//! a [`control_mode::Client`][crate::control_mode::Client] should send it
//! through that client rather than through a builder here — see
//! [`RefreshClient`]'s own doc for why the two are not interchangeable.
//!
//! # Layout
//!
//! Sessions first, then the clients attached to them, then the server both
//! stand on — outward from the thing a caller usually names.

use super::invocations;

invocations! {
    /// Creates a session, and the server under it if none is running.
    ///
    /// The shape a caller who wants a session and not a terminal asks for
    /// is `-d -P -F`: detached so this process's terminal is not taken
    /// over, and printing so the new session's name comes back from the
    /// call that made it rather than from a second call that would already
    /// be racing whoever else is creating sessions.
    ///
    /// ```
    /// use tmux::commands::NewSession;
    ///
    /// let argv = NewSession::new()
    ///     .detached()
    ///     .print()
    ///     .format("#{session_name}")
    ///     .start_directory("/tmp")
    ///     .environment("TERM=screen-256color")
    ///     .environment("GANJA_AGENT_ID=w1")
    ///     .session_name("workers")
    ///     .command(["sh", "-c", "exec my-agent"])
    ///     .args();
    ///
    /// let words: Vec<_> = argv.iter().map(|word| word.to_string_lossy()).collect();
    /// assert_eq!(
    ///     words,
    ///     [
    ///         "new-session", "-d", "-P", "-F", "#{session_name}", "-c", "/tmp",
    ///         "-e", "TERM=screen-256color", "-e", "GANJA_AGENT_ID=w1",
    ///         "-s", "workers", "--", "sh", "-c", "exec my-agent",
    ///     ]
    /// );
    /// ```
    ///
    /// With [`attach_if_exists`][Self::attach_if_exists] the command stops
    /// being a create and becomes a create-or-attach, which is what makes
    /// it safe to run twice; the two flags that only mean something under
    /// it — [`detach_others`][Self::detach_others] and
    /// [`hangup_parent`][Self::hangup_parent] — are `attach-session`'s `-d`
    /// and `-x` under new letters, and carry the names they have there.
    NewSession = "new-session", Some("new") => {
        /// `-A`: attaches to the session of that name if it already exists
        /// instead of failing.
        attach_if_exists: switch "-A";
        /// `-d`: creates the session without attaching this terminal to it.
        detached: switch "-d";
        /// `-D`: under [`attach_if_exists`][Self::attach_if_exists],
        /// detaches every other client from the session, as
        /// [`AttachSession::detach_others`] does.
        detach_others: switch "-D";
        /// `-E`: leaves the `update-environment` option unapplied.
        skip_update_environment: switch "-E";
        /// `-P`: prints the new session, in [`format`][Self::format].
        print: switch "-P";
        /// `-X`: under [`attach_if_exists`][Self::attach_if_exists], sends
        /// `SIGHUP` to the parent of the client it displaces, as
        /// [`AttachSession::hangup_parent`] does.
        hangup_parent: switch "-X";
        /// `-c start-directory`: the working directory new windows inherit.
        start_directory: value "-c";
        /// `-e environment`: one `NAME=VALUE` pair, and callable once per
        /// variable.
        environment: repeat "-e";
        /// `-F format`: what [`print`][Self::print] answers with; the
        /// default is `#{session_name}:`.
        format: text "-F";
        /// `-f flags`: a comma-separated list of client flags, such as
        /// `read-only,ignore-size`.
        client_flags: text "-f";
        /// `-n window-name`: names the session's first window.
        window_name: value "-n";
        /// `-s session-name`: names the session.
        session_name: value "-s";
        /// `-t group-name`: the session group to join, which may be a
        /// group, a session already in one, or a name for a new group.
        /// tmux's usage line calls this a target-session; the manual calls
        /// it what it is.
        group: value "-t";
        /// `-x width`: columns, or `-` for the current client's width.
        width: value "-x";
        /// `-y height`: lines, or `-` for the current client's height.
        height: value "-y";
        /// The program and its arguments, run directly rather than through
        /// a shell, behind the `--` that keeps them out of the flags.
        command: trailing;
    }

    /// Attaches this terminal to an existing session, or — from inside
    /// tmux — switches the current client to it.
    ///
    /// The session must already exist: creating one is
    /// [`NewSession`]'s job, and [`NewSession::attach_if_exists`] is how a
    /// caller asks for either without knowing which it will get.
    AttachSession = "attach-session", Some("attach") => {
        /// `-d`: detaches every other client attached to the session.
        detach_others: switch "-d";
        /// `-E`: leaves the `update-environment` option unapplied.
        skip_update_environment: switch "-E";
        /// `-r`: the client is read-only and does not size the others —
        /// tmux's own shorthand for
        /// [`client_flags`][Self::client_flags]`("read-only,ignore-size")`.
        read_only: switch "-r";
        /// `-x`: sends `SIGHUP` to the client's parent process as well as
        /// detaching it, which typically ends it.
        hangup_parent: switch "-x";
        /// `-c working-directory`: the session's working directory, which
        /// new windows inherit.
        working_directory: value "-c";
        /// `-f flags`: a comma-separated list of client flags; a leading
        /// `!` turns one off on an already-attached client.
        client_flags: text "-f";
        /// `-t target-session`: the session to attach to.
        target: value "-t";
    }

    /// Asks whether a session exists, and answers by exit status alone.
    ///
    /// A missing session is an error rather than an empty answer, so it
    /// arrives as [`Error::ClientRefused`][crate::Error::ClientRefused]
    /// carrying tmux's own words — which is the shape a caller should
    /// match on rather than parsing anything.
    HasSession = "has-session", Some("has") => {
        /// `-t target-session`: the session to ask about.
        target: value "-t";
    }

    /// Renames a session.
    RenameSession = "rename-session", Some("rename") => {
        /// `-t target-session`: the session to rename.
        target: value "-t";
        /// The session's new name. Required by tmux, which says so itself
        /// when it is missing.
        new_name: positional;
    }

    /// Destroys a session, its windows, and the clients' attachment to it.
    KillSession = "kill-session", None => {
        /// `-a`: kills every session *except* the target.
        all_others: switch "-a";
        /// `-C`: clears the alerts — bell, activity, silence — in every
        /// window linked to the session instead.
        clear_alerts: switch "-C";
        /// `-g`: kills every session in the target's session group.
        group: switch "-g";
        /// `-f filter`: with [`all_others`][Self::all_others], kills only
        /// the sessions the filter is true for.
        filter: text "-f";
        /// `-t target-session`: the session to kill, or to spare under
        /// [`all_others`][Self::all_others].
        target: value "-t";
    }

    /// Lists the server's sessions.
    ///
    /// Answers with lines, one per session, in whatever
    /// [`format`][Self::format] asked for — which is why the crate parses
    /// none of it: what the columns mean was the caller's decision.
    ListSessions = "list-sessions", Some("ls") => {
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        reverse: switch "-r";
        /// `-F format`: what each line looks like.
        format: text "-F";
        /// `-f filter`: shows only the sessions it is true for.
        filter: text "-f";
        /// `-O sort-order`: one of `index`, `name`, `creation` or
        /// `activity`.
        sort_order: text "-O";
    }

    /// Locks every client attached to a session.
    LockSession = "lock-session", Some("locks") => {
        /// `-t target-session`: the session whose clients to lock.
        target: value "-t";
    }

    /// Points a client at another session.
    ///
    /// A `-t` naming a pane — a target holding `:`, `.` or `%` — moves the
    /// client's session, window and pane together, which is the one case
    /// [`keep_zoomed`][Self::keep_zoomed] exists for.
    SwitchClient = "switch-client", Some("switchc") => {
        /// `-E`: leaves the `update-environment` option unapplied.
        skip_update_environment: switch "-E";
        /// `-l`: moves to the last session instead of a named one.
        last: switch "-l";
        /// `-n`: moves to the next session.
        next: switch "-n";
        /// `-p`: moves to the previous one.
        previous: switch "-p";
        /// `-r`: toggles the client's read-only and ignore-size flags,
        /// rather than setting them as [`AttachSession::read_only`] does.
        toggle_read_only: switch "-r";
        /// `-Z`: keeps the window zoomed if it was, for a `-t` naming a
        /// pane.
        keep_zoomed: switch "-Z";
        /// `-c target-client`: the client to move; the current one if
        /// omitted.
        client: value "-c";
        /// `-O sort-order`: which field [`next`][Self::next] and
        /// [`previous`][Self::previous] walk — one of `name`, `size`,
        /// `creation` or `activity`.
        sort_order: text "-O";
        /// `-T key-table`: the table the client's next key is looked up
        /// in, after which it returns to its default.
        key_table: text "-T";
        /// `-t target-session`: where the client goes; may name a pane.
        target: value "-t";
    }

    /// Detaches a client, or every client attached to a session.
    DetachClient = "detach-client", Some("detach") => {
        /// `-a`: detaches every client *except* the one
        /// [`target`][Self::target] names.
        all_others: switch "-a";
        /// `-P`: sends `SIGHUP` to the client's parent process, which
        /// typically ends it.
        hangup_parent: switch "-P";
        /// `-E shell-command`: runs this in the client's place, as one
        /// string tmux hands to `sh`.
        shell_command: value "-E";
        /// `-s target-session`: detaches every client attached to this
        /// session.
        session: value "-s";
        /// `-t target-client`: the client to detach.
        target: value "-t";
    }

    /// Suspends a client by sending it `SIGTSTP`.
    SuspendClient = "suspend-client", Some("suspendc") => {
        /// `-t target-client`: the client to suspend.
        target: value "-t";
    }

    /// Redraws a client, and sets what a control-mode client reports.
    ///
    /// **On a control-mode connection this is the wrong door.** A
    /// [`control_mode::Client`][crate::control_mode::Client] must send its
    /// own `refresh-client` through [`exec`][crate::control_mode::Client::exec]
    /// so the arguments cross that transport's quoting ladder and its
    /// validators; a builder here renders argv for a separate `tmux`
    /// invocation, which is a *different client* and would set a size and
    /// subscriptions on nobody. The two share only the command's name,
    /// which both take from tmux.
    ///
    /// From outside a client this is still useful — `-t` names one — and
    /// that is the case this builder is for.
    RefreshClient = "refresh-client", Some("refresh") => {
        /// `-c`: returns to following the cursor, undoing the four
        /// directional moves.
        track_cursor: switch "-c";
        /// `-D`: moves the visible portion down by
        /// [`adjustment`][Self::adjustment] rows.
        down: switch "-D";
        /// `-L`: moves it left by [`adjustment`][Self::adjustment]
        /// columns.
        left: switch "-L";
        /// `-l`: asks the client for its clipboard and stores the answer
        /// in a new paste buffer.
        clipboard: switch "-l";
        /// `-R`: moves the visible portion right.
        right: switch "-R";
        /// `-S`: updates the status line and nothing else.
        status_line: switch "-S";
        /// `-U`: moves the visible portion up.
        up: switch "-U";
        /// `-A pane:state`: what a control-mode client wants done with a
        /// pane's output — `on`, `off`, `continue` or `pause` after the
        /// pane id. Callable once per pane.
        pane_state: repeat "-A";
        /// `-B name:what:format`: a format subscription, reported back as
        /// `%subscription-changed` at most once a second; `what` may be
        /// empty, a pane or window id, or `%*`/`@*`. A name alone removes
        /// the subscription. Callable once per subscription.
        subscribe: repeat "-B";
        /// `-C size`: the size of a control-mode client, or of one window
        /// for it — `80x24`, or `@0:80x24`.
        size: value "-C";
        /// `-f flags`: a comma-separated list of client flags, as
        /// [`AttachSession::client_flags`] takes.
        client_flags: text "-f";
        /// `-r pane:report`: information a control-mode client is
        /// answering with, such as a reply to OSC 10 — the pane id, a
        /// colon, then the escape sequence. Callable once per pane.
        pane_report: repeat "-r";
        /// `-t target-client`: the client to refresh.
        target: value "-t";
        /// How far the four directional flags move; one if omitted.
        adjustment: positional;
    }

    /// Lists the clients attached to the server.
    ListClients = "list-clients", Some("lsc") => {
        /// `-r`: reverses [`sort_order`][Self::sort_order].
        ///
        /// tmux next-3.8 prints no `-r` in this command's usage line and
        /// accepts one all the same; the manual documents it, and the
        /// binary was asked directly.
        reverse: switch "-r";
        /// `-F format`: what each line looks like.
        format: text "-F";
        /// `-f filter`: shows only the clients it is true for.
        filter: text "-f";
        /// `-O sort-order`: one of `name`, `size`, `creation` or
        /// `activity`.
        sort_order: text "-O";
        /// `-t target-session`: lists only the clients attached to this
        /// session.
        target: value "-t";
    }

    /// Locks one client, as [`LockServer`] locks all of them.
    LockClient = "lock-client", Some("lockc") => {
        /// `-t target-client`: the client to lock.
        target: value "-t";
    }

    /// Shows the server's recent messages, or what it is doing.
    ShowMessages = "show-messages", Some("showmsgs") => {
        /// `-J`: shows the server's jobs instead of its messages.
        jobs: switch "-J";
        /// `-T`: shows its terminals instead.
        terminals: switch "-T";
        /// `-t target-client`: whose messages to show.
        target: value "-t";
    }

    /// Starts the server without creating a session.
    ///
    /// On its own this is nearly a no-op, since a server with no sessions
    /// exits again unless `exit-empty` is off or the configuration file
    /// creates one. It earns its place as the first command of a sequence.
    StartServer = "start-server", Some("start") => {}

    /// Kills the server, its clients, and every session on it.
    KillServer = "kill-server", None => {}

    /// Locks every client on the server, using the `lock-command` option.
    LockServer = "lock-server", Some("lock") => {}

    /// Reads or changes which users and groups may reach this server.
    ///
    /// The list is empty by default and the socket's file permissions keep
    /// everybody else out, so this only matters where those permissions
    /// have already been widened — which the manual warns against, and
    /// this crate repeats rather than softens.
    ///
    /// tmux next-3.8 prints `[-t target-pane]` in this command's usage
    /// line and then answers `unknown flag -t`; there is deliberately no
    /// method for it, since one could only build argv tmux refuses.
    ServerAccess = "server-access", None => {
        /// `-a`: grants access to the named user or group.
        allow: switch "-a";
        /// `-d`: revokes it, detaching any client it was the only warrant
        /// for.
        deny: switch "-d";
        /// `-g`: reads the name as a group rather than a user.
        group: switch "-g";
        /// `-l`: lists the current permissions — `U` for a user, `G` for a
        /// group, `R` or `W` for read-only or writable.
        list: switch "-l";
        /// `-r`: makes matching clients read-only.
        read_only: switch "-r";
        /// `-w`: makes them writable.
        writable: switch "-w";
        /// The user or group the flags apply to.
        name: positional;
    }

    /// Lists the syntax of one command, or of every command tmux has.
    ///
    /// The answer this crate's own inventory test reads: it is what says
    /// which commands the tmux on this machine actually has, against which
    /// [`REGISTRY`][crate::commands::REGISTRY] and
    /// [`EXCLUDED`][crate::commands::EXCLUDED] must between them account
    /// for all of them.
    ListCommands = "list-commands", Some("lscm") => {
        /// `-F format`: what each line looks like.
        format: text "-F";
        /// One command's name, when the whole listing is not wanted.
        command: positional;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionId, commands::Invocation};

    /// Every assertion below reads argv as text, for the reason the pane
    /// family's own helper states: these are about which words tmux is
    /// handed, and the one test about bytes lives beside the accumulator.
    fn words<I: Invocation>(invocation: &I) -> Vec<String> {
        invocation
            .args()
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

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
            words(
                &KillSession::new()
                    .all_others()
                    .filter("#{==:#{session_attached},0}")
                    .target("keep")
            ),
            [
                "kill-session",
                "-a",
                "-f",
                "#{==:#{session_attached},0}",
                "-t",
                "keep",
            ]
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
                    .group()
                    .list()
                    .read_only()
                    .writable()
                    .name("testgroup")
            ),
            [
                "server-access",
                "-a",
                "-d",
                "-g",
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
}
