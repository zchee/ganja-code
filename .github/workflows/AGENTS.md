<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-12 -->

# workflows

## Purpose

CI definitions. One workflow, three jobs, and it is the contract every phase is gated against: a phase is not done until these pass.

## Key Files

| File | Description |
|------|-------------|
| `ci.yaml` | `lint` (rustfmt, clippy, core-purity) on ubuntu; `deny` (cargo-deny advisories/licenses/bans/sources against `deny.toml`) on ubuntu only, because lockfile analysis cannot differ by OS; `test` matrixed over ubuntu and xcode runners — every lane blocking — with the upstream opencode checkout provisioned for the golden differential suite. The windows lanes (`windows-lint`, a `windows-2025` matrix entry) left on 2026-08-12: windows support is parked for now. |

## For AI Agents

### Working In This Directory

Five things in `ci.yaml` are load-bearing and should not be "cleaned up" without understanding them:

- **The boundary gates come in two shapes, and the shape is the point.** The *inverted greps* (`! cargo tree -p ganja-core -e normal | grep -q ratatui`, its `axum` sibling, the two bottom-crate leaf checks with `tail -n +2` stripping the header line, and the frontend-lane pair for `ganja-tui`/`ganja-serve`) forbid a named crate from a named tree — a plain `grep -c` exits non-zero on *zero* matches, which would fail exactly when the boundary holds. The *allowlist gates* assert an **exact set** and exist because a blocklist silently weakens when the crate set grows: `ganja-tool`'s internal deps are exactly `ganja-permission`, `ganja-core`'s are exactly the three beneath it, and `ganja-protocol`'s direct externals are exactly `serde serde_json` (measured: a scratch `schemars.workspace = true` resolved cleanly and left the leaf grep green — only the exact-set form can see a non-`ganja` widening). A new internal crate joining an allowlist is a deliberate edit to that line, never an accident. The version-policy `awk` step is the gate form of the workspace rule that member manifests never carry a dependency version.
- **Clippy runs at the default lint level** with `-D warnings`, not `pedantic`. That was a deliberate P0 decision.
- **The toolchain is a date-pinned nightly** (`nightly-2026-08-07`), and the date is spelled out in every job because `dtolnay/rust-toolchain` does not read `rust-toolchain.toml`; moving the pin means moving it in both places, deliberately, with a gate run.
- **The upstream spec checkout lands in `upstream/`, not `.omc/`.** `.omc/` is local tooling state that is never committed, so the workflow cannot assume it exists and must not create it. The checkout is `anomalyco/opencode` at `v1.18.13` with `persist-credentials: false`, its `node_modules` are cached and installed with `bun install --frozen-lockfile`, and the path is handed to the tests as `GANJA_OPENCODE_DIR`. The golden suite hard-fails when that is missing, so removing any of those steps turns a real comparison into a green run that compared against nothing.
- **Windows support is parked (2026-08-12).** The `windows-lint` job and the `windows-2025` matrix entry are removed, not paused: the `cfg(windows)` code still in the tree has no compile signal while this holds, so treat it as unverified. The lanes' history — the earned-blocking posture, bun's cache-poisoned `node_modules` on windows, the separable `cargo check` compile gate — lives in git; a revival starts observational again rather than inheriting the old standing. The job-level `defaults: run: shell: bash` stays for a different reason now: naming bash adds pipefail, which the runners' implicit `bash -e` default does not.
- **The `test` job runs under `cargo nextest run` (profile `ci`), with `cargo test --doc` beside it.** nextest gives each test its own process, which is what the env-mutating suites rely on; it does not run doctests, so the companion step is not optional even though there are none yet. Config is the root `.config/nextest.toml` — the `terminate-after` there is the only thing that stops a pty test wedged in the kernel from hanging the run.

### Testing Requirements

Reproduce a CI failure locally by running the same gates from the repository root (see `../../AGENTS.md`); the `test` job is `cargo nextest run --workspace` plus `cargo test --workspace --doc`. To reproduce it exactly, check the upstream tag out somewhere, run `bun install` in it, and point `GANJA_OPENCODE_DIR` at it.

## Dependencies

### External

`actions/checkout@v7`, `dtolnay/rust-toolchain@nightly`, `Swatinem/rust-cache@v2`, `taiki-e/install-action@v2` (installs `cargo-nextest`), `actions/cache@v6`, `oven-sh/setup-bun@v2` (bun 1.3.14), `EmbarkStudios/cargo-deny-action@v2` (cargo-deny, configured by the root `deny.toml`).

<!-- MANUAL: -->
