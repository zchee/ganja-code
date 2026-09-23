use std::collections::BTreeSet;

use super::Screen;
use crate::Config;

/// Default off holds by construction: a config no tier wrote anything into
/// resolves to the screen that names no source, which is also what
/// `Screen::default()` is.
#[test]
fn a_config_that_names_no_source_screens_nothing() {
    let screen = Config::default().evaluate_screen();

    assert_eq!(screen, Screen::default());
    assert!(!screen.webfetch, "webfetch is not screened unless named");
    assert!(!screen.websearch, "websearch is not screened unless named");
    assert_eq!(screen.mcp, BTreeSet::new(), "no MCP server is screened unless named");
}
