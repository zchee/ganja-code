<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# crates

## Purpose

Container for the three workspace members. The split is architectural, not cosmetic: `ganja-core` must stay usable without a terminal so the engine is testable headless and can later be served over a socket, `ganja-tui` owns every pixel and no engine logic, and `ganja-cli` is the thin binary that wires them together.

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `ganja-core/` | Engine: sessions, providers, tools, permissions, the serde protocol (see `ganja-core/AGENTS.md`) |
| `ganja-tui/` | ratatui frontend (see `ganja-tui/AGENTS.md`) |
| `ganja-cli/` | The `ganja` binary (see `ganja-cli/AGENTS.md`) |

## For AI Agents

### Working In This Directory

The dependency direction is one-way and enforced: `ganja-cli` → `ganja-tui` → `ganja-core`. Two rules follow.

- **`ganja-core` may never depend on a terminal crate.** CI asserts `cargo tree -p ganja-core -e normal` never mentions `ratatui`. If core needs to describe something the UI will draw, it does so in serde-serializable protocol types, not in ratatui types.
- **`ganja-tui` holds no engine logic.** It turns terminal events into `Command`s and engine `Event`s into frames. A transcript is built from engine events alone — the frontend never invents an entry — because that is what makes resumed sessions (P4) and remote clients (P7) replay identically.

`ganja-cli` depends on `ratatui` for exactly one reason: the raw-mode read that keeps a typed API key off the screen, through the same crossterm instance the UI drives so the two cannot disagree about terminal state.

### Testing Requirements

Run the workspace gates from the repository root; see `../AGENTS.md`. Per-crate: `cargo test -p ganja-core`, `-p ganja-tui`, `-p ganja-cli`.

### Common Patterns

Member manifests declare dependencies as `foo.workspace = true` and never carry a version. Where a feature is enabled at the member level (`tokio-util = { workspace = true, features = ["rt"] }`), the manifest comment says why that crate opts into that module.

## Dependencies

### Internal

`ganja-core` and `ganja-tui` are themselves declared as workspace dependencies (path deps) in the root manifest, so members reference each other the same way they reference crates.io.

<!-- MANUAL: -->
