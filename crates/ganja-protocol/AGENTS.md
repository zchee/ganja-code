<!-- Parent: ../AGENTS.md -->
# ganja-protocol

The serde types every side of ganja exchanges: the `Command`s a frontend sends, the `Event`s the engine streams back, the `Message`/`Part` model a session is stored as, and the teammate frames in `team.rs`. It holds data and id minting only, never engine behaviour. It is a leaf at the bottom of the workspace graph, so a frontend or a test can name an `Event` without building the engine.

## Boundary

- `depgate.toml`: `[rules."ganja-protocol"] leaf = true` and `direct = ["serde", "serde_json", "uuid"]`. The direct dependency set is exact: adding any normal dependency fails CI until that rule is edited deliberately.
- `crates/ganja-core/src/lib.rs` re-exports it whole: `pub use ganja_protocol as protocol;`.
- Doc comments here may describe engine-side items in prose but never as intra-doc links, because a link needs a dependency the leaf rule refuses.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `Command`, `Event`, `Message`, `MessageTime`, `Part`, `PartBody`, `ToolState`, `Usage`, `FinishReason`, `Mention`, `RevertInfo`, `PermissionReply`, `PermissionMode`, the question and held-call types, the id newtypes, `now`, `uuidv7`, `is_uuidv7`. Spec: upstream `session/message-v2.ts`. |
| `src/team.rs` | Teammate mailbox frames (`Frame`, `Tagged`, the fifteen payload structs), `LeadFrame`, `PeerPayload`, `PeerMessageId`, and the render-only `TeamView`/`MemberView`. |
| `src/lib_tests.rs`, `src/team_tests.rs` | Unit tests, including pinned wire shapes. |

## Commands

```sh
cargo nextest run -p ganja-protocol
cargo tree -p ganja-protocol -e normal        # shows the three direct dependencies
cargo depgate check --config depgate.toml     # the leaf and direct rules, from the repository root
```

## Conventions

- **Every type round-trips through serde, and stored sessions are these values written verbatim.** Changing a field's name, type or tag breaks every stored session; adding a new optional field or a new `PartBody` variant does not.
- **Unknown fields: `lib.rs` tolerates them, `team.rs` refuses them.** The session and command/event types in `lib.rs` carry no `deny_unknown_fields`, so a newer stored row still decodes. In `team.rs`, every frame payload except the passthrough `TeamPermissionUpdate`, plus `PeerPayload`, `MemberView` and `TeamView`, is `deny_unknown_fields`; a half-understood frame fails to decode.
- **A new optional `Message` field uses `#[serde(default, skip_serializing_if = ...)]`** so a stored session without it stays byte-identical. Follow `request_only`, `compaction_summary` and `command`.
- **Tags:** `PartBody`, `Command` and `Event` are internally tagged on `type` in `snake_case`; `ToolState` is tagged on `status`. `team.rs` keeps each frame family's casing exactly as Claude Code writes it and never normalises it, because a real `claude` process reads and writes the same mailbox.
- **Ids are bare lowercase hyphenated UUIDv7 strings with no prefix.** `uuidv7` mints them through `Uuid::now_v7` (RFC 9562 monotonic counter), so string order is creation order; every id type's `ascending()` calls it. `is_uuidv7` accepts only that exact 36-character lowercase form, and the store uses it to detect rows minted by an older build.
- **Behaviour stays out.** Types are plain data. The exceptions are identity helpers: id minting, `Part::as_text`/`as_text_mut`/`streamed_mut`, `Message::from_command`/`request_only_user`, and `Event::session_id`, which matches every `Event` variant; a new variant needs an arm there.

## Gotchas

- **Display-only parts, which no wire sends:** `PartBody::ReasoningText` (readable thinking; every wire drops it when encoding a request, and it is excluded from `Part::as_text`) and `PartBody::ServerTool` (work a vendor already ran; never executed, gated or replayed). `PartBody::Peer` is drawn and also sent, rendered into the user turn.
- **`Message.request_only` marks a message minted for one request.** It is never written to the transcript and gets a fresh id on every request, so no stateful wire may treat its id as conversation state.
- **The `reasoning` tag prefix (`REASONING_TAG`) is a contract.** A new reasoning variant keeps the prefix; a reader that cannot decode such a part keeps the message and substitutes a stateless `PartBody::Reasoning` (the reader is `crates/ganja-storage/src/storage.rs`).
- **`LeadFrame` has no serde derives, no `From` and no `Deref`.** `LeadFrame::parse` is its only constructor, because a `Deserialize` impl would be a second constructor that never checked the sender.
- `PermissionResponse` fields are private; build one with `PermissionResponse::success` or `PermissionResponse::error`, and check a decoded one with `PermissionResponse::is_consistent`.
- `Command::SetDeadline { until }` is a Unix-epoch timestamp in milliseconds, the same unit as `MessageTime`. No event answers it; what a deadline means lives in the engine.

## Tests

Unit tests live in sibling files wired through `#[path]`; no inline test module exists. The crate has no `tests/` directory and no test needs setup.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-protocol.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
