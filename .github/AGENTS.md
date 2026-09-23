<!-- Parent: ../AGENTS.md -->
# .github

GitHub configuration: two workflows, one issue form and the Renovate config. There is no PR template and no `dependabot.yml`.

## Layout

| Path | Holds |
|---|---|
| `workflows/ci.yaml` | The merge gate, on push to `main` and every PR: jobs `test`, `lint`, `deny` |
| `workflows/claude-live.yaml` | Weekly (`17 3 * * 1`, UTC) and `workflow_dispatch` run of `teammate_claude_live` against a real `claude` |
| `ISSUE_TEMPLATE/idea.yml` | The Idea form (label `idea`); the owner gives each idea a do, park or drop verdict |
| `renovate.json5` | The only dependency updater: `cargo` and `github-actions` managers, Monday before 09:00 Asia/Tokyo, `minimumReleaseAge` 5 days, `deps:` commit prefix |

## CI jobs (`ci.yaml`)

- `test/<os>`, matrix `ubuntu-26.04` and `xcode-27`, `fail-fast: false`, `shell: bash`. Steps in order:
  - checkout; `dtolnay/rust-toolchain@master` (`nightly`, component `rust-analyzer`); `Swatinem/rust-cache@v2`; `taiki-e/install-action@v2` (nextest);
  - `install tmux` (Homebrew on both OSes, Linuxbrew put on `GITHUB_PATH`); `oven-sh/setup-bun@v2` (bun `1.4.0`);
  - checkout of `anomalyco/opencode` `v1.18.22` into `upstream/opencode-v1.18.22`; `actions/cache@v6` of its `node_modules`; `bun install --frozen-lockfile`;
  - `test`; `doc-tests`.
- `lint`, `ubuntu-26.04`: checkout; toolchain `nightly` with `clippy, rustfmt`; rust-cache; `rustfmt`; `clippy`; `rustdoc`; `cache the dependency gate`; `install the dependency gate` (only on a cache miss); `dependency policy`.
- `deny`, `ubuntu-26.04`: checkout; toolchain `nightly`; `EmbarkStudios/cargo-deny-action@v2` with `command: check`, configured by the root `deny.toml`.
- `claude-live.yaml` has one job, `claude-live/ubuntu` on `ubuntu-26.04`: fail if the `ANTHROPIC_API_KEY` secret is empty, install `@anthropic-ai/claude-code` from npm (latest), write a seed file, then run the test with `GANJA_LIVE_TEST=1` and `GANJA_LIVE_CLAUDE_SEED`, 15-minute timeout. It uses the image's apt tmux.

## Commands

Run from the repository root. These match the CI steps.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
cargo install --git https://github.com/zchee/cargo-depgate --rev v0.1.1 --locked cargo-depgate   # once
cargo depgate check --config depgate.toml
cargo deny check                    # needs cargo-deny: cargo install --locked cargo-deny
GANJA_OPENCODE_DIR=/path/to/opencode-v1.18.22 GANJA_LSP_EDIT_BUDGET_MS=6000 \
  cargo nextest run --locked --workspace --profile ci
GANJA_OPENCODE_DIR=/path/to/opencode-v1.18.22 cargo test --workspace --doc
```

For `GANJA_OPENCODE_DIR`, check out `anomalyco/opencode` at tag `v1.18.22` and run `bun install --frozen-lockfile` in it. The golden differential test hard-fails without it. The `test` step also needs tmux 3.7c or newer and the `rust-analyzer` rustup component on `PATH`.

## Conventions

- `runs-on` is one of `ubuntu-26.04`, `xcode-27`, `windows-2025`; there is no Windows lane today, so `cfg(windows)` code gets no compile check.
- Pin actions at the major version (`actions/checkout@v7`). `dtolnay/rust-toolchain` is the exception: `@master` takes an explicit `toolchain:`.
- Every job repeats `toolchain: nightly` because the action does not read `rust-toolchain.toml`. Change both together.
- New files use `.yaml`. `ISSUE_TEMPLATE/idea.yml` keeps `.yml`; GitHub accepts either for issue forms.
- The depgate version appears twice in `ci.yaml`: `--rev v0.1.1` in the install step and `v0.1.1` in the cache key. Bump both together.
- Dependency rules live in the root `depgate.toml`, each with its reason in a comment; edit a rule there, not in `ci.yaml`.
- Renovate's own cargo is pinned by `constraints.rust` (`1.98.0`) in `renovate.json5`; bump it by hand when a dependency's MSRV passes it. Lockstep groups: `html5ever family`, `ripgrep internals`, `ratatui family`.

## Gotchas

- `test` and `clippy` run with `--locked`: a stale `Cargo.lock` fails CI instead of being rewritten.
- The upstream checkout goes in `upstream/`, which is gitignored and exists only on the runner.
- The `cargo-depgate` binary comes from `actions/cache@v6` keyed `cargo-depgate-<os>-<arch>-v0.1.1`; a cache hit skips the install.
- `.config/nextest.toml` sets `retries = 0`, and its `ci` profile terminates a test after 4 minutes and keeps running after a failure; three overrides (`tmux` `live`, two `pty_smoke` drills, `ganja-core` `cancel`) take every test thread.
- `concurrency` cancels an in-progress run on the same ref when a new one starts.
- Without `install tmux` the xcode runner has no tmux and every tmux-driving suite hard-fails; the ubuntu apt tmux is older than the 3.7c floor `crates/tmux/tests/inventory.rs` is held to.

## History

Decisions before 2026-09-23: `docs/decisions/github.md` and `docs/decisions/github-workflows.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
