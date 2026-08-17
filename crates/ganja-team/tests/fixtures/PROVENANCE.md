# Where these documents came from

Everything under this directory was **written by Claude Code**, not by this
repository. It is committed as interop test data for `claude_format_interop.rs`
(AC-1b), which is the only test in this workspace that can tell whether
`ganja-team` reads and rewrites a foreign document byte-for-byte. A round-trip
test against documents this repo also wrote proves self-consistency and nothing
about interop, which is why these bytes are here at all.

`THIRD_PARTY_NOTICES.md` names this directory as Claude Code's output; this
file is the record that line was written from.

**Do not reformat anything under this directory.** Not with an editor's
save hook, not with `jq`, not with a formatter that thinks a JSON file wants a
trailing newline. Every byte is evidence, and re-indenting the fixture to make
a test pass would delete the only thing the test is for. If a change here is
ever genuinely needed, recapture rather than edit. The interop test is the
guard — anything that rewrites these files turns it red immediately, which is
the intended behavior and not a flake.

## Capture

| | |
|---|---|
| Captured | 2026-08-17 |
| Captured by | this repository's P25 work, on the machine that ran it |
| Source | `$CLAUDE_CONFIG_DIR/teams/<team>/`, with `CLAUDE_CONFIG_DIR` unset — so `~/.claude/teams/<team>/` |
| Claude Code installed at capture | **2.1.233** (2026-08-15) |
| Claude Code that wrote the files | 2.1.x, exact patch **not recoverable** — see below |

Only the three most recent installs survive under `~/.local/share/claude/versions`
(2.1.229, 2.1.232, 2.1.233), and both source directories predate all three. What
*is* verifiable is the thing that matters: a team file written by 2.1.233 on the
capture date (`~/.claude/teams/session-18fb916d/config.json`, and the live
session's own) carries the **identical key order** to what is captured here. The
shape below is therefore current, not merely historical.

The whole `~/.claude/teams/` tree — 35 team directories — was surveyed before
choosing. Two format eras are present on this machine:

- **modern**, every `session-*` directory, 2026-07-02 through the capture date;
- **legacy**, the older named directories (`ane-python`, `web-pages`, …),
  2026-03. Their member records order `prompt` before `color`, their lead
  records omit `backendType` entirely, and their team files carry a top-level
  `description` between `name` and `createdAt`.

**Only the modern era is captured**, deliberately. One serde declaration order
can round-trip exactly one of the two, so committing both would make AC-1b
permanently unsatisfiable; the era ganja must interop with is the one Claude
Code writes today. The legacy observations are recorded here rather than
fixtured, because they are evidence about the format even where they are not
test data — in particular, `description` is proof that a real Claude document
has carried an unknown key in a **non-tail** position, which is the limitation
`record.rs`'s module doc leaves open ("a question a captured document
answers"). `claude_format.rs` pins that limitation as behavior.

## Why two directories

The brief was one team holding a lead record, a teammate record and a
non-empty inbox. **No such modern directory exists**, and the reason is a
property of the design rather than of this machine: delivered messages are
pruned (§3.1), so a settled team's inboxes are all `[]`. Every modern team dir
either has members and empty inboxes, or a message and no `config.json`.

So two real directories are captured verbatim instead of one, and between them
they carry every document AC-1b needs:

| Directory | What it carries | Source mtime |
|---|---|---|
| `session-62633995/` | `config.json` with a real lead record and a real teammate record (`backendType: tmux`, `tmuxPaneId: %7`, `isActive: false`), and two real seeded-empty inboxes | 2026-08-10 |
| `session-44cd25e1/` | one real inbox holding one real message with the complete modern envelope (`msgV`, `msg_id`, `type`, `read`) | 2026-07-08 |

`session-2e5b719d` — the capture session's own live team — was deliberately not
touched: it changes underfoot.

## What was redacted, exactly

The law this capture was held to: **only two kinds of span may change** — prompt
text, and absolute paths pointing outside the fixture. The replacement is
byte-for-byte length-preserving ASCII alphanumerics; it never introduces a `"`
or a `\`; and a JSON escape sequence is redacted as a whole unit or not at all.
The reason is narrow: AC-1b compares bytes, so a redaction that changed a
length, or that introduced a character the encoder must escape, would fail the
test for a reason having nothing to do with ganja's serializer — and the first
person to investigate would look in the wrong crate.

Every redacted span was replaced with the cycled ASCII word `redacted`, so a
reader can see at a glance which bytes are not Claude's. Escape sequences took
the "not at all" branch: all 28 `\n` in the prompt are **the original bytes**,
which is deliberate — re-emitting an escape exactly as Claude wrote it is part
of what AC-1b is testing.

| File | JSON pointer | Span | Bytes replaced |
|---|---|---|---|
| `session-62633995/config.json` | `/members/0/cwd` | 49 bytes | 49 |
| `session-62633995/config.json` | `/members/1/prompt` | 3342 bytes | 3286 (56 bytes are the 28 preserved `\n` escapes) |
| `session-62633995/config.json` | `/members/1/cwd` | 49 bytes | 49 |

Nothing else in any file was touched. In particular:

- **`session-44cd25e1/inboxes/worker-mask.json` is entirely unredacted.** Its
  message body holds no absolute path (`~/gows`, `~/sdk/go1.26.5/bin/go` and
  `.omc/research/…` are all relative or tilde-rooted) and no credential, and a
  message body is not one of the two redactable span kinds. Leaving it whole is
  the law's answer, not an oversight.
- Team names, agent ids, session UUIDs, timestamps, colors, models, agent types
  and pane ids are Claude's own bytes.
- The two `[]` inboxes are two bytes each, exactly as `writeExclusive` seeded
  them, and carry no trailing newline — as no captured document does.

Verification after redaction: every file's byte length is unchanged, every file
still parses, and `jq keys_unsorted` over the whole document tree is identical
to the source. The only textual differences against the source are the three
values above.

## What the capture found

Recorded here because the finding outlived the test run that produced it, and
because a later reader deserves the evidence rather than the conclusion.

Key orders as they are actually on disk, against `record.rs`'s declaration
order **as it stood at capture** — the two `differs` verdicts are what
de0b6fa corrected, and `record.rs` has emitted these orders since:

| Shape | Verdict |
|---|---|
| `TeamFile` | matches: `name, createdAt, leadAgentId, leadSessionId, members` |
| lead `MemberRecord` | matches: `agentId, name, agentType, joinedAt, tmuxPaneId, cwd, subscriptions, backendType` |
| teammate `MemberRecord` | **differs**: `agentId, name, color, joinedAt, tmuxPaneId, subscriptions, agentType, model, prompt, planModeRequired, cwd, backendType, isActive` |
| `MailboxMessage` | **differs**: `from, text, summary, timestamp, msgV, msg_id, type, read` — the sender's fields, then the four write-time stamps appended |

The two orders a `MemberRecord` has to produce are **not reconcilable in one
declaration order**: a lead puts `agentType` third and `cwd` before
`subscriptions`; a teammate puts `subscriptions` before `agentType` and `cwd`
after `planModeRequired`. Claude builds them at two creation sites, and byte
identity for both needs two emit orders.

`color`'s position in a *modern* message is unwitnessed — no captured modern
message carries one. The legacy documents put it after `timestamp` and before
`read`.
