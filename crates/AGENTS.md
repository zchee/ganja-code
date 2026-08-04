<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# crates

## Purpose

Container for the workspace members. The split is architectural, not cosmetic. Two axes cross here. **Up:** `ganja-core` must stay usable without a terminal so the engine is testable headless and can later be served over a socket, `ganja-tui` owns every pixel and no engine logic, and `ganja-cli` is the thin binary that wires them together. **Down:** the protocol, the permission engine and the tools are crates of their own beneath the engine, so that what each depends on is a fact the compiler checks rather than a rule a reviewer remembers.

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `ganja-protocol/` | The types every side of the app speaks. Depends on nothing of ours (see `ganja-protocol/AGENTS.md`) |
| `ganja-permission/` | Which calls run unasked, and the worktree they run in (see `ganja-permission/AGENTS.md`) |
| `ganja-tool/` | What the model can do besides talk, plus the read log and its watcher (see `ganja-tool/AGENTS.md`) |
| `ganja-core/` | Engine: sessions, providers, the agent loop, config, storage. Re-exports the three above under their old module names (see `ganja-core/AGENTS.md`) |
| `ganja-tui/` | ratatui frontend (see `ganja-tui/AGENTS.md`) |
| `ganja-cli/` | The `ganja` binary (see `ganja-cli/AGENTS.md`) |
| `ganja-testkit/` | Dev-only scaffolding shared by `ganja-core`'s integration suites: scripted providers, recorder/blocking tools, drain and storage-seeding builders (see `ganja-testkit/AGENTS.md`) |

## For AI Agents

### Working In This Directory

The dependency direction is one-way, and every load-bearing edge of it is asserted in CI: frontends sit on `ganja-core`, core sits on `ganja-tool`, tool sits on `ganja-permission` — while `ganja-protocol` is a leaf that core and the frontends consume directly and that tool and permission never touch, and the two bottom crates name nothing else of ours at all. Three rules follow.

- **`ganja-core` may never depend on a terminal crate.** CI asserts `cargo tree -p ganja-core -e normal` never mentions `ratatui`. If core needs to describe something the UI will draw, it does so in serde-serializable protocol types, not in ratatui types.
- **Nothing below the engine may name the engine.** `! cargo tree -p ganja-tool -e normal | grep -q ganja-core` is the assertion; `ganja-permission` and `ganja-protocol` name nothing of ours at all. What a tool needs from its caller arrives as a value in `ToolCtx`, which is why that type is a bag of values rather than a session handle.
- **`ganja-tui` holds no engine logic.** It turns terminal events into `Command`s and engine `Event`s into frames. A transcript is built from engine events alone — the frontend never invents an entry — because that is what makes resumed sessions and remote clients replay identically. It links `ganja-protocol` for the types it renders, and `ganja-tool` for the one thing it genuinely runs in-process: the `@` file menu's glob walk.

`ganja-cli` depends on `ratatui` for exactly one reason: the raw-mode read that keeps a typed API key off the screen, through the same crossterm instance the UI drives so the two cannot disagree about terminal state.

### Testing Requirements

Run the workspace gates from the repository root; see `../AGENTS.md`. Per-crate: `cargo test -p ganja-core`, and the same for `-p ganja-protocol`, `-p ganja-permission`, `-p ganja-tool`, `-p ganja-tui`, `-p ganja-cli`.

### Common Patterns

Member manifests declare dependencies as `foo.workspace = true` and never carry a version. Where a feature is enabled at the member level (`tokio-util = { workspace = true, features = ["rt"] }`), the manifest comment says why that crate opts into that module.

## Dependencies

### Internal

Every member is declared as a workspace dependency (a path dep) in the root manifest with the reason it exists, so members reference each other the same way they reference crates.io — `foo.workspace = true`, never a path or a version in a member manifest.

`ganja-core` re-exports the three crates beneath it under the module names they had before the split (`ganja_core::protocol`, `::permission`, `::project`, `::tool`, `::watch`), which is what let the split land without rewriting every caller. New code that wants only one of them should depend on it directly rather than reach through the facade.

<!-- MANUAL: -->
