<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-12 -->

# workflows

## Purpose

CI definitions. Two workflows: `ci.yaml` — three jobs, the contract every phase is gated against (a phase is not done until these pass) — and `claude-live.yaml`, a scheduled probe that is deliberately *not* part of that contract.

## Key Files

| File | Description |
|------|-------------|
| `claude-live.yaml` | AC-13's live half on a weekly clock plus `workflow_dispatch`: installs the current `claude` from npm, pre-approves the `ANTHROPIC_API_KEY` repository secret in a seeded `.claude.json` (the sha256 arithmetic lives beside the secret; the test copies the file via `GANJA_LIVE_CLAUDE_SEED`), and runs `teammate_claude_live` under `GANJA_LIVE_TEST=1`. Scheduled rather than per-push because what drifts is claude's own release stream, which no push here witnesses; hard-fails by name when the secret is absent, because a run that tested nothing must not look like interop evidence. |
| `ci.yaml` | `lint` (rustfmt, clippy, rustdoc under `RUSTDOCFLAGS=-D warnings`, core-purity) on ubuntu; `deny` (cargo-deny advisories/licenses/bans/sources against `deny.toml`) on ubuntu only, because lockfile analysis cannot differ by OS; `test` matrixed over ubuntu and xcode runners — every lane blocking — with the upstream opencode checkout provisioned for the golden differential suite. The windows lanes (`windows-lint`, a `windows-2025` matrix entry) left on 2026-08-12: windows support is parked for now. |

## For AI Agents

### Working In This Directory

Five things in `ci.yaml` are load-bearing and should not be "cleaned up" without understanding them:

- **The boundary gates come in three shapes, and the shape is the point.** The *inverted greps* (`! cargo tree -p ganja-core -e normal | grep -q ratatui` and its `axum` sibling, the `ganja-provider` trio against `ratatui`/`crossterm`/`arboard`, the two bottom-crate leaf checks with `tail -n +2` stripping the header line, the frontend-lane quartet — `ganja-tui` ∌ `axum`, `ganja-serve` ∌ `ratatui`, `ganja-client` ∌ `axum`, `ganja-serve` ∌ `ganja-teammate-local` — and the header-stripped `tmux` leaf check against `ganja-`) forbid a named crate from a named tree — a plain `grep -c` exits non-zero on *zero* matches, which would fail exactly when the boundary holds. The *allowlist gates* assert an **exact set** and exist because a blocklist silently weakens when the crate set grows. There are seven internal ones plus two external ones; `tmux`'s is external-only because its internal `ganja-*` set must be empty under the inverted leaf check, not because that direction went unchecked. The strings below are byte-for-byte what `ci.yaml` tests against, trailing space included:

    ```
    ganja-tool             = "ganja-permission "
    ganja-team             = "ganja-protocol "
    ganja-storage          = "ganja-permission ganja-protocol "
    ganja-provider         = "ganja-permission ganja-protocol ganja-tool "
    ganja-core             = "ganja-permission ganja-protocol ganja-provider ganja-storage ganja-team ganja-tool "
    ganja-client           = "ganja-protocol "
    ganja-teammate-local   = "ganja-core ganja-permission ganja-protocol ganja-provider ganja-storage ganja-team ganja-tool "
    ganja-protocol, depth 1 = "serde serde_json uuid "
    tmux, depth 1 = "futures thiserror tokio "
    ```

  `ganja-core`'s list is the **six** beneath it: `ganja-team` joined in P25, `uuid` joined the protocol's externals in the same phase (D493, the id mint), and `ganja-storage` joined in W3 (**D540**) when the session store and the working-tree snapshots became a leaf of their own — each a deliberate edit to its line, which is the whole point of the form. `ganja-teammate-local`'s list is `ganja-core` plus the same six, because the crate sits *above* the engine the way a frontend does (**D539**) rather than beneath it — every backend it holds needs a tmux server, a shell to split into or somebody else's binary on `PATH`, none of which an engine may hold. `ganja-core`'s own list is unchanged by that crate's existence, which is the half of the split CI proves for free; the inverted `ganja-serve` ∌ `ganja-teammate-local` line proves the other half — the closure `ganja serve` ships is the closure a cloud worker will ship, so the machine-bound backends must never enter it. `ganja-storage`'s own list is the two bottom crates — the session store needs to know which worktree it is anchored on and what a stored record decodes to, and nothing else; `ganja-core` reaches it only through the re-export its `lib.rs` carries. The external gates exist because the leaf greps see only `ganja-` names (measured: a scratch `schemars.workspace = true` resolved cleanly and left the protocol leaf grep green — only the exact-set form can see a non-`ganja` widening); `tmux` gets the analogous depth-one guard over its three normal externals while its internal half remains the empty set asserted by the inverted grep. The third shape is the metadata-derived reverse-prohibition loop: the P26 user directive (2026-08-18) forbids every other workspace member from consuming `tmux`, so the gate enumerates `cargo metadata` at run time rather than freezing today's peer list and silently ceasing to cover members added later — the same loop covers `ganja-teammate-local` and `ganja-storage` with no edit, since it derives the member list rather than freezing it, and cannot false-positive on `ganja-teammate-local`'s own `tmux.rs` module because the check is `cargo tree`'s crate-name match, not a source grep. The version-policy `awk` step is the gate form of the workspace rule that member manifests never carry a dependency version.
- **Clippy runs at the default lint level** with `-D warnings`, not `pedantic`. That was a deliberate P0 decision.
- **The toolchain is the non-dated `nightly` channel**, and the channel is spelled out in every job because `dtolnay/rust-toolchain` does not read `rust-toolchain.toml`; moving the pin means moving it in both places, deliberately, with a gate run.
- **The upstream spec checkout lands in `upstream/`, not `.omc/`.** `.omc/` is local tooling state that is never committed, so the workflow cannot assume it exists and must not create it. The checkout is `anomalyco/opencode` at `v1.18.22` with `persist-credentials: false`, its `node_modules` are cached and installed with `bun install --frozen-lockfile`, and the path is handed to the tests as `GANJA_OPENCODE_DIR`. The golden suite hard-fails when that is missing, so removing any of those steps turns a real comparison into a green run that compared against nothing.
- **Windows support is parked (2026-08-12).** The `windows-lint` job and the `windows-2025` matrix entry are removed, not paused: the `cfg(windows)` code still in the tree has no compile signal while this holds, so treat it as unverified. The lanes' history — the earned-blocking posture, bun's cache-poisoned `node_modules` on windows, the separable `cargo check` compile gate — lives in git; a revival starts observational again rather than inheriting the old standing. The job-level `defaults: run: shell: bash` stays for a different reason now: naming bash adds pipefail, which the runners' implicit `bash -e` default does not.
- **The `test` job runs under `cargo nextest run` (profile `ci`), with `cargo test --doc` beside it.** nextest gives each test its own process, which is what the env-mutating suites rely on; it does not run doctests, so the companion step is not optional — and it stopped being hypothetical: there are ten now, `ganja-team`'s runnable mailbox example among them. Config is the root `.config/nextest.toml` — the `terminate-after` there is the only thing that stops a pty test wedged in the kernel from hanging the run.

### Testing Requirements

Reproduce a CI failure locally by running the same gates from the repository root (see `../../AGENTS.md`); the `test` job is `cargo nextest run --workspace` plus `cargo test --workspace --doc`. To reproduce it exactly, check the upstream tag out somewhere, run `bun install` in it, and point `GANJA_OPENCODE_DIR` at it.

## Dependencies

### External

`actions/checkout@v7`, `dtolnay/rust-toolchain@master` (the `@master` form is the one that takes an explicit `toolchain:`, which is how the channel pin is spelled), `Swatinem/rust-cache@v2`, `taiki-e/install-action@v2` (installs `cargo-nextest`), `actions/cache@v6`, `oven-sh/setup-bun@v2` (bun 1.3.14), `EmbarkStudios/cargo-deny-action@v2` (cargo-deny, configured by the root `deny.toml`).

<!-- MANUAL: -->
