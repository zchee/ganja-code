<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-12 -->

# workflows

## Purpose

CI definitions. Two workflows: `ci.yaml` — three jobs, the contract every phase is gated against (a phase is not done until these pass) — and `claude-live.yaml`, a scheduled probe that is deliberately *not* part of that contract.

## Key Files

| File | Description |
|------|-------------|
| `claude-live.yaml` | AC-13's live half on a weekly clock plus `workflow_dispatch`: installs the current `claude` from npm, pre-approves the `ANTHROPIC_API_KEY` repository secret in a seeded `.claude.json` (the sha256 arithmetic lives beside the secret; the test copies the file via `GANJA_LIVE_CLAUDE_SEED`), and runs `teammate_claude_live` under `GANJA_LIVE_TEST=1`. Scheduled rather than per-push because what drifts is claude's own release stream, which no push here witnesses; hard-fails by name when the secret is absent, because a run that tested nothing must not look like interop evidence. |
| `ci.yaml` | `lint` (rustfmt, clippy, rustdoc under `RUSTDOCFLAGS=-D warnings`, and the dependency-policy gate `cargo depgate check` over the root `depgate.toml`) on ubuntu; `deny` (cargo-deny advisories/licenses/bans/sources against `deny.toml`) on ubuntu only, because lockfile analysis cannot differ by OS; `test` matrixed over ubuntu and xcode runners — every lane blocking — with the upstream opencode checkout provisioned for the golden differential suite. The windows lanes (`windows-lint`, a `windows-2025` matrix entry) left on 2026-08-12: windows support is parked for now. |

## For AI Agents

### Working In This Directory

Six things in `ci.yaml` are load-bearing and should not be "cleaned up" without understanding them:

- **The dependency boundaries are one policy file, and the file is the point.** The seventeen shell steps that used to stand in `lint` (nineteen assertions of inverted `cargo tree | grep` bans, exact internal/direct sets, leaf checks, a metadata-derived "nothing consumes `tmux`" loop and a version-policy `awk`; retired 2026-08-31) are now the root `depgate.toml`, evaluated by `cargo depgate check --config depgate.toml` over one `cargo metadata` resolve. Nineteen rules: five `deny` (`ganja-core` ∌ `ratatui*`/`axum*`; `ganja-provider` ∌ `ratatui*`/`crossterm*`/`arboard*`; `ganja-tui` ∌ `axum*`; `ganja-serve` ∌ `ratatui*` and `ganja-teammate-local`; `ganja-client` ∌ `axum*`), seven exact `internal` allowlists — an exact set fails closed where a blocklist silently weakens as the crate set grows — three `leaf` rules (`ganja-permission`, `ganja-protocol`, `tmux`), two exact depth-1 `direct` sets (`ganja-protocol` = serde, serde_json, uuid; `tmux` = futures, thiserror, tokio — the leaf shape sees only internal names, so an external widening needs its own rule), the `sealed` rule that no other workspace member consumes `tmux` (the member set is resolved at gate time, so a new member is covered with no edit — and it cannot false-positive on `ganja-teammate-local`'s own `tmux.rs` module, because the check matches crate names in the resolved graph, not source text), and the `manifest` rule that member manifests never carry a dependency version. Each rule's rationale and the retired step it replaces are comments in `depgate.toml` itself; treat a rule edit like the deliberate allowlist edit it replaces. The tool is installed from `zchee/cargo-depgate` pinned by `--rev` (a tag is mutable and that repo publishes no GitHub Release, so the sha is the pin) and cached under a key carrying the same sha plus OS and arch; the dedicated `actions/cache` entry deliberately duplicates rust-cache's `~/.cargo/bin` caching, because rust-cache's key churns with the lockfile while this one moves only with the rev.
- **Clippy runs at the default lint level** with `-D warnings`, not `pedantic`. That was a deliberate P0 decision.
- **The toolchain is the non-dated `nightly` channel**, and the channel is spelled out in every job because `dtolnay/rust-toolchain` does not read `rust-toolchain.toml`; moving the pin means moving it in both places, deliberately, with a gate run.
- **The upstream spec checkout lands in `upstream/`, not `.omc/`.** `.omc/` is local tooling state that is never committed, so the workflow cannot assume it exists and must not create it. The checkout is `anomalyco/opencode` at `v1.18.22` with `persist-credentials: false`, its `node_modules` are cached and installed with `bun install --frozen-lockfile`, and the path is handed to the tests as `GANJA_OPENCODE_DIR`. The golden suite hard-fails when that is missing, so removing any of those steps turns a real comparison into a green run that compared against nothing.
- **Windows support is parked (2026-08-12).** The `windows-lint` job and the `windows-2025` matrix entry are removed, not paused: the `cfg(windows)` code still in the tree has no compile signal while this holds, so treat it as unverified. The lanes' history — the earned-blocking posture, bun's cache-poisoned `node_modules` on windows, the separable `cargo check` compile gate — lives in git; a revival starts observational again rather than inheriting the old standing. The job-level `defaults: run: shell: bash` stays for a different reason now: naming bash adds pipefail, which the runners' implicit `bash -e` default does not.
- **The `test` job runs under `cargo nextest run` (profile `ci`), with `cargo test --doc` beside it.** nextest gives each test its own process, which is what the env-mutating suites rely on; it does not run doctests, so the companion step is not optional — and it stopped being hypothetical: there are ten now, `ganja-team`'s runnable mailbox example among them. Config is the root `.config/nextest.toml` — the `terminate-after` there is the only thing that stops a pty test wedged in the kernel from hanging the run.

### Testing Requirements

Reproduce a CI failure locally by running the same gates from the repository root (see `../../AGENTS.md`); the `test` job is `cargo nextest run --workspace` plus `cargo test --workspace --doc`. To reproduce it exactly, check the upstream tag out somewhere, run `bun install` in it, and point `GANJA_OPENCODE_DIR` at it.

## Dependencies

### External

`actions/checkout@v7`, `dtolnay/rust-toolchain@master` (the `@master` form is the one that takes an explicit `toolchain:`, which is how the channel pin is spelled), `Swatinem/rust-cache@v2`, `taiki-e/install-action@v2` (installs `cargo-nextest`), `actions/cache@v6`, `oven-sh/setup-bun@v2` (bun 1.3.14), `EmbarkStudios/cargo-deny-action@v2` (cargo-deny, configured by the root `deny.toml`), and `zchee/cargo-depgate` (the dependency-policy gate, `cargo install`ed pinned by `--rev` and cached).

<!-- MANUAL: -->
