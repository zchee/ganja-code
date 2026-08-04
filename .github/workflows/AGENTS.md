<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# workflows

## Purpose

CI definitions. One workflow, three jobs, and it is the contract every phase is gated against: a phase is not done until these pass.

## Key Files

| File | Description |
|------|-------------|
| `ci.yaml` | `lint` (rustfmt, clippy, core-purity) on ubuntu; `deny` (cargo-deny advisories/licenses/bans/sources against `deny.toml`) on ubuntu only, because lockfile analysis cannot differ by OS; `test` matrixed over ubuntu and xcode runners, with the upstream opencode checkout provisioned for the golden differential suite. |

## For AI Agents

### Working In This Directory

Four things in `ci.yaml` are load-bearing and should not be "cleaned up" without understanding them:

- **The core-purity gate is inverted**: `! cargo tree -p ganja-core -e normal | grep -q ratatui`. A plain `grep -c` exits non-zero on *zero* matches, which would fail the build exactly when the core is pure. Its sibling `! cargo tree -p ganja-tool -e normal | grep -q ganja-core` is inverted for the same reason and asserts the other load-bearing direction: nothing beneath the engine may reach the engine.
- **Clippy runs at the default lint level** with `-D warnings`, not `pedantic`. That was a deliberate P0 decision.
- **The toolchain is nightly**, matching `rust-toolchain.toml`.
- **The upstream spec checkout lands in `upstream/`, not `.omc/`.** `.omc/` is local tooling state that is never committed, so the workflow cannot assume it exists and must not create it. The checkout is `anomalyco/opencode` at `v1.18.11` with `persist-credentials: false`, its `node_modules` are cached and installed with `bun install --frozen-lockfile`, and the path is handed to the tests as `GANJA_OPENCODE_DIR`. The golden suite hard-fails when that is missing, so removing any of those steps turns a real comparison into a green run that compared against nothing.
- **The `test` job runs under `cargo nextest run` (profile `ci`), with `cargo test --doc` beside it.** nextest gives each test its own process, which is what the env-mutating suites rely on; it does not run doctests, so the companion step is not optional even though there are none yet. Config is the root `.config/nextest.toml` — the `terminate-after` there is the only thing that stops a pty test wedged in the kernel from hanging the run.

### Testing Requirements

Reproduce a CI failure locally by running the same gates from the repository root (see `../../AGENTS.md`); the `test` job is `cargo nextest run --workspace` plus `cargo test --workspace --doc`. To reproduce it exactly, check the upstream tag out somewhere, run `bun install` in it, and point `GANJA_OPENCODE_DIR` at it.

## Dependencies

### External

`actions/checkout@v7`, `dtolnay/rust-toolchain@nightly`, `Swatinem/rust-cache@v2`, `taiki-e/install-action@v2` (installs `cargo-nextest`), `actions/cache@v6`, `oven-sh/setup-bun@v2` (bun 1.3.14), `EmbarkStudios/cargo-deny-action@v2` (cargo-deny, configured by the root `deny.toml`).

<!-- MANUAL: -->
