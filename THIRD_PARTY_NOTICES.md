# Third-Party Notices

## opencode

This repository is a behavioral port to Rust of [opencode](https://github.com/anomalyco/opencode), pinned at reference tag **v1.18.13** (released 2026-08-04).

What is ported from the upstream specification:
- **Behavior**: streaming chat, tool calling, permission gating, session persistence, configuration loading
- **Tool prompt texts**: descriptions in `crates/ganja-tool/src/` derive from upstream `packages/opencode/src/tool/*.txt` and from prompt strings embedded in the tool sources under `packages/opencode/src/tool/` (MIT licensed, attributed below)
- **Theme definitions**: color schemes and styling adapted from `packages/tui/src/theme/`. The files under `crates/ganja-tui/assets/themes/` — `opencode.json`, `tokyonight.json`, `gruvbox.json`, `aura.json` — are verbatim copies of the upstream `packages/tui/src/theme/assets/` files of the same names.
- **System prompt texts**: the files under `crates/ganja-core/src/prompt/` are verbatim copies of upstream files — `anthropic.txt`, `gpt.txt`, `default.txt`, `plan.txt` and `build-switch.txt` from `packages/opencode/src/session/prompt/`, `explore.txt` from `packages/opencode/src/agent/prompt/`, and `initialize.txt` from `packages/opencode/src/command/template/initialize.txt`. `crates/ganja-tool/src/{task,websearch,skill,question}.txt` are likewise verbatim copies, of `packages/opencode/src/tool/{task,websearch,skill,question}.txt`; they sit beside the tools they describe, which is where upstream keeps them. `websearch.txt` is copied verbatim and its `{{year}}` token substituted at runtime, which is what upstream does with the same file. The `Instructions from: {path}` header and the `<env>` block wording derive from `packages/opencode/src/session/{instruction.ts,system.ts}`. The `build`, `plan`, `general` and `explore` agent descriptions in `crates/ganja-core/src/agent.rs` are verbatim from `packages/opencode/src/agent/agent.ts`, and the permission-denial message in `crates/ganja-core/src/session.rs` derives from `packages/core/src/v1/permission.ts`.

The implementation is original Rust code using idiomatic patterns; it is **not a code translation** but rather a faithful behavioral port of the upstream TypeScript/JavaScript specification.

**Deferred:** the cursor stub (`crates/ganja-provider/src/provider/cursor.rs`) incorporates no third-party material — it is an identity and a refusal, written here. The attribution obligation for cursor's protocol attaches when a real wire ports the opencode-cursor plugin's protocol knowledge, and lands in this file with it.

---

## Scope of this file

These notices cover material **incorporated into this repository's own sources**: ported behavior, prompt texts, and theme definitions.

Rust dependencies are not vendored here — they are resolved by Cargo and recorded in `Cargo.lock`. Their license notices are therefore not reproduced in this file; a complete, generated attribution list belongs with the distributed binaries and is produced at packaging time (e.g. `cargo about`). Listing individual crates here would give an arbitrary and misleading picture of a tree that carries hundreds of them.

---

## MIT License

The following is the complete license text from upstream opencode:

```
MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
