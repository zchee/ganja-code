//! `ganja` — a terminal-first AI coding agent.

use anyhow::Result;
use clap::Parser;

/// Terminal-first AI coding agent.
#[derive(Debug, Parser)]
#[command(name = "ganja", version, about)]
struct Cli {}

#[tokio::main]
async fn main() -> Result<()> {
    // Parsing is for its side effects: `--help` and `--version` print and exit,
    // and unknown arguments are rejected before the terminal is taken over.
    let _cli = Cli::parse();

    ganja_tui::run().await
}
