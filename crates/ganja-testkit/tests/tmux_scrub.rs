//! [`ganja_testkit::tmux::PrivateServer`] is born outside whatever tmux runs
//! the suite.
//!
//! The scrub it pins is load-bearing twice over: with `$TMUX` inherited, the
//! client refuses to start a nested server at all, and a `$TMUX_PANE` that
//! reached the server's global environment would hand every pane it makes
//! the id of a pane on the *developer's* server. Mutates the environment to
//! stage exactly that inheritance — one test, one binary.

#[test]
fn a_private_server_is_born_outside_whatever_tmux_runs_the_suite() {
    ganja_testkit::tmux::require_tmux();
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment while this writes it.
    unsafe {
        std::env::set_var("TMUX", "/nonexistent/tmux.sock,0,0");
        std::env::set_var("TMUX_PANE", "%99");
    }

    // Without the scrub this line is where the test dies: a client that
    // inherits `$TMUX` refuses to nest.
    let server = ganja_testkit::tmux::PrivateServer::start(&["sleep", "30"], &[], &[]);

    for name in ["TMUX", "TMUX_PANE"] {
        assert!(
            !server.global_has(name),
            "{name} was in this process's environment and must not be in the server's"
        );
    }
}
