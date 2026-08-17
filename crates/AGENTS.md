<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# crates

## Purpose

Container for the workspace members. The split is architectural, not cosmetic. Two axes cross here. **Up:** `ganja-core` must stay usable without a terminal so the engine is testable headless and can later be served over a socket, `ganja-tui` owns every pixel and no engine logic, and `ganja-cli` is the thin binary that wires them together. **Down:** the protocol, the permission engine, the tools and the vendor wires are crates of their own beneath the engine, so that what each depends on is a fact the compiler checks rather than a rule a reviewer remembers.

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `ganja-protocol/` | The types every side of the app speaks. Depends on nothing of ours (see `ganja-protocol/AGENTS.md`) |
| `ganja-permission/` | Which calls run unasked, and the worktree they run in (see `ganja-permission/AGENTS.md`) |
| `ganja-team/` | Claude Code's teams directory: member records and the file-backed mailboxes teammates are addressed through. Names only `ganja-protocol` (see `ganja-team/AGENTS.md`) |
| `ganja-tool/` | What the model can do besides talk, plus the read log and its watcher (see `ganja-tool/AGENTS.md`) |
| `ganja-provider/` | Talking to a model vendor: the wires, the credentials they present, and the catalog that sizes and prices what they serve (see `ganja-provider/AGENTS.md`) |
| `ganja-core/` | Engine: sessions, the agent loop, config, storage. Re-exports the four above under their old module names (see `ganja-core/AGENTS.md`) |
| `ganja-tui/` | ratatui frontend (see `ganja-tui/AGENTS.md`) |
| `ganja-serve/` | The engine over a socket: REST routes and the SSE event stream, over TCP and over a per-session Unix socket (see `ganja-serve/AGENTS.md`) |
| `ganja-client/` | The other end of that wire, and nothing else: the typed routes and the SSE reader `run --attach` and `sessions --live` drive. Names only `ganja-protocol` (see `ganja-client/AGENTS.md`) |
| `ganja-cli/` | The `ganja` binary (see `ganja-cli/AGENTS.md`) |
| `ganja-testkit/` | Dev-only scaffolding shared by `ganja-core`'s integration suites: scripted providers, recorder/blocking tools, drain and storage-seeding builders (see `ganja-testkit/AGENTS.md`) |

## For AI Agents

### Working In This Directory

The dependency direction is one-way, and every load-bearing edge of it is asserted in CI: frontends — `ganja-tui` and `ganja-serve` alike — sit on `ganja-core`, core sits on `ganja-provider`, `ganja-tool` and `ganja-team`, the first two of those sit on `ganja-permission` — while `ganja-protocol` is a leaf that core, the provider crate, the teams crate, the frontends and `ganja-client` consume directly and that tool and permission never touch, and the two bottom crates name nothing else of ours at all. Three rules follow.

- **`ganja-core` may never depend on a terminal crate.** CI asserts `cargo tree -p ganja-core -e normal` never mentions `ratatui`. If core needs to describe something the UI will draw, it does so in serde-serializable protocol types, not in ratatui types. `ganja-provider` is held to the same rule, plus `arboard`: a login that wants to ask a person something hands the question back to whoever called it rather than drawing a prompt.
- **Nothing below the engine may name the engine.** The assertion is a closed allowlist per crate rather than a `! grep ganja-core`, which names one crate and goes quiet the day a new one appears: `ganja-tool`'s internal set is exactly `ganja-permission`, `ganja-provider`'s is exactly `ganja-permission ganja-protocol ganja-tool`, `ganja-team`'s and `ganja-client`'s are each exactly `ganja-protocol`, and `ganja-permission` and `ganja-protocol` name nothing of ours at all. What a tool needs from its caller arrives as a value in `ToolCtx`, which is why that type is a bag of values rather than a session handle; what a wire needs arrives on its `ChatRequest`; and where a teams directory *is* arrives as a `TeamsRoot`, for the same reason. `ganja-core`'s own list is the closed five — `ganja-permission ganja-protocol ganja-provider ganja-team ganja-tool` — which is the one that has to be edited deliberately when a crate is split off, and the reason none of these is a blocklist.
- **`ganja-tui` holds no engine logic.** It turns terminal events into `Command`s and engine `Event`s into frames. A transcript is built from engine events alone — the frontend never invents an entry — because that is what makes resumed sessions and remote clients replay identically. It links `ganja-protocol` for the types it renders, `ganja-permission` for the project's stored rules it loads and hands to the engine, and `ganja-tool` for the one thing it genuinely runs in-process: the `@` file menu's glob walk.

`ganja-cli` depends on `ratatui` for exactly one reason: the raw-mode read that keeps a typed API key off the screen, through the same crossterm instance the UI drives so the two cannot disagree about terminal state.

### Testing Requirements

Run the workspace gates from the repository root; see `../AGENTS.md`. Per-crate: `cargo test -p ganja-core`, and the same for `-p ganja-protocol`, `-p ganja-permission`, `-p ganja-team`, `-p ganja-tool`, `-p ganja-provider`, `-p ganja-tui`, `-p ganja-serve`, `-p ganja-client`, `-p ganja-cli`.

### Common Patterns

Member manifests declare dependencies as `foo.workspace = true` and never carry a version. Where a feature is enabled at the member level (`tokio-util = { workspace = true, features = ["rt"] }`), the manifest comment says why that crate opts into that module.

## Dependencies

### Internal

Every member is declared as a workspace dependency (a path dep) in the root manifest with the reason it exists, so members reference each other the same way they reference crates.io — `foo.workspace = true`, never a path or a version in a member manifest.

`ganja-core` re-exports the crates beneath it under the module names they had before each split (`ganja_core::protocol`, `::permission`, `::project`, `::tool`, `::watch`, `::auth`, `::catalog`, and `::team` for the one crate that was born rather than split off), which is what let each split land without rewriting every caller. The facade is those module names and nothing more: the crate root names only the engine's own types, so a caller that wants one of the four crates alone depends on it directly rather than reach through the facade — `ganja-cli` does exactly that for `auth login`, which drives `ganja-provider`'s OAuth flows and has no engine at all.

`ganja_core::provider` is the one facade that is not a bare re-export, because the module did not move whole: the wires left, and the half that reads a `Config` — which provider a session runs as, which model it asks for — stayed, over a glob of `ganja_provider::provider`. Every path a caller already wrote still resolves, and `ganja-core/src/AGENTS.md` says which functions are on which side.

<!-- MANUAL: -->
