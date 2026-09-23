<!-- Parent: ../AGENTS.md -->

# assets

## Purpose

Data files compiled into the binary with `include_str!`. Nothing here is code.

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `themes/` | The four upstream themes shipped verbatim — `opencode.json` (the default), `tokyonight.json`, `gruvbox.json`, `aura.json`. MIT, attributed in the root `THIRD_PARTY_NOTICES.md`. |

## For AI Agents

- **These are verbatim upstream copies** from `packages/tui/src/theme/assets/` at v1.18.22. Do not reformat, re-indent, sort keys, or "fix" them — a byte diff against upstream must stay empty, because that is what the notices claim.
- Porting another theme takes four coordinated changes: the verbatim file here, an `include_str!` row in `../src/theme/registry.rs`, a filename added to `THIRD_PARTY_NOTICES.md`, and a per-theme snapshot in `../src/snapshots/`.
