<!-- Parent: ../AGENTS.md -->

# tests

## Purpose

Integration binaries for what unit tests cannot touch: process-wide environment. `theme_paths.rs` holds exactly one test — it mutates `XDG_CONFIG_HOME` and `XDG_DATA_HOME` to prove theme discovery and the persisted pick resolve through real XDG paths end to end.

## For AI Agents

- **One test per file in any binary that mutates process-wide state** — the same rule `ganja-core/tests/AGENTS.md` states and for the same reason: nextest gives each test a process, but a plain `cargo test` runs a binary's tests on parallel threads, and environment variables are process-wide. A second env-mutating test goes in a NEW file, not a second `fn`.
- Everything else about themes is unit-tested beside the code in `../src/theme/`; reach for this directory only when the environment itself is the subject.
