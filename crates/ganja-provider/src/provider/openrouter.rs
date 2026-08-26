//! OpenRouter, which is a gateway speaking the Responses API [`super::responses`]
//! already speaks.
//!
//! Spec: upstream supports this provider natively
//! (`packages/opencode/src/provider/provider.ts:467`), so a builtin is a port
//! rather than an invention — but what upstream ports is a *loader*, not a
//! wire: its entry sets `autoload: false` and two attribution headers and then
//! hands the vendor to `@openrouter/ai-sdk-provider`, which speaks chat
//! completions. Ganja's session runs the Responses dialect against this vendor
//! instead (owner's directive, 2026-08-14), and OpenRouter publishes that
//! surface itself — so upstream's TypeScript settles *that* this provider
//! exists and the vendor's own reference settles *what its request carries*:
//! <https://openrouter.ai/docs/api_reference/responses/overview> and the
//! `basic-usage`, `reasoning`, `tool-calling` and `error-handling` pages
//! beneath it (read 2026-08-14).
//!
//! # What this vendor's request keeps, drops, and never had
//!
//! [`super::responses`] was written against one vendor, so every field in it is
//! an OpenAI-shaped assumption until something says otherwise. Each was
//! re-derived here rather than carried over:
//!
//! | field | here | why |
//! |---|---|---|
//! | `model`, `input`, `instructions`, `tools`, `stream` | kept | all four are the reference's own documented body fields (`overview`, `basic-usage`, `tool-calling`) |
//! | `store: false` | kept | the reference: "Requests that set `store: true` or a non-null `previous_response_id` are rejected with a `400` error". The value is the same as the ChatGPT backend's and the reason is not — there it is a backend requirement, here it is the only value a *stateless* API accepts |
//! | `previous_response_id` | never sent | rejected by the same sentence, and this build has never had a field for it: every request is rebuilt whole from ganja's own transcript |
//! | `include: ["reasoning.encrypted_content"]` | **dropped** | the reference documents no `include` parameter at all, and its own reasoning example shows `encrypted_content` arriving without one. Sending an undocumented field to a gateway is a guess, and this one would be spent on every request |
//! | a `reasoning` input item replayed | **dropped** | the reference documents no way to send sealed reasoning back, and its multi-turn example (`tool-calling`) carries none. The failure mode decides it: a replay this vendor refuses is a `400` on the *second* request of every reasoning turn, which is most agentic turns |
//! | `reasoning.summary: "auto"` | **dropped** | both halves of that default are the other vendor's — `super::responses::seals_reasoning` is a rule about *its* model ids, and `"auto"` is what *its* CLI sends. The reference documents no way to *ask* for a summary at all, and asks for none in its own examples — yet its settled response carries one, and its stream carries thinking regardless (see the reasoning rows below). A field nobody has to send is not one to invent a default for |
//! | `reasoning: {effort: …}` | **sent, when an effort is selected** (P20) | the reference publishes the four levels `minimal`/`low`/`medium`/`high` in a table of its own, so this is the one reasoning field it documents. It rides the ordinary effort splice — `crate::effort`'s `Wire::OpenRouter` lane synthesizes exactly `{"reasoning": {"effort": …}}`, and deliberately not the sibling's map, whose `summary` and `include` are the two rows above. No effort selected is still no `reasoning` key |
//! | `tool_choice: "auto"` | **sent, beside a non-empty roster** (P20) | every tool example in the reference spells it, and what the API assumes in its absence is the one thing that page does not say. The failure it would cause is the expensive kind — a roster advertised and never called — and `"auto"` is what an agent loop wants on every turn. Scoped to this backend: the other two send the Codex CLI's request, which carries no such field |
//! | `strict` on a tool definition | **dropped** | the reference prints `strict: null` on every tool it defines, which is that field's absent value; it documents no behavior for a boolean, and this build's schemas are generated from the argument structs rather than written to the strict subset, so a `true` would be a promise the roster cannot keep |
//! | the four subscription headers | never sent | they belong to a ChatGPT seat impersonating the Codex CLI; the reference asks for `Authorization` and `Content-Type` and nothing else |
//!
//! # What its stream says that the other vendor's does not
//!
//! Two readings, both P20, both about thinking a person can read — never about
//! state to replay, which the rows above still refuse in both directions:
//!
//! - **`response.reasoning.delta`** is this vendor's own name for a fragment of
//!   thinking (its reasoning page's streaming example). The dialect is
//!   documented as a drop-in for OpenAI's, and then this one event is not:
//!   unmapped, a reasoning turn here streamed a reply with nothing under it.
//!   [`super::responses`] maps it beside OpenAI's spelling, and a stream
//!   carrying both is one train of thought relayed twice — the first spelling to
//!   say anything wins for the whole response.
//! - **The settled item's `summary` array.** The reference's own response
//!   example carries `{type: "reasoning", id, encrypted_content, summary:
//!   [strings]}` on a request that asked for no summary, so on a turn that
//!   streamed nothing readable the closing frame is the only place thinking
//!   exists; it is read there when nothing was streamed, and never in addition
//!   to what was.
//!
//! One thing about tool streaming is **assumed rather than measured**: a call is
//! terminated on `response.output_item.done`, the OpenAI terminator this dialect
//! inherits by being a documented drop-in. The reference's own streaming example
//! watches `response.function_call_arguments.done` instead — for the finished
//! arguments, which this build accumulates from the deltas. Both frames arriving
//! produce exactly one call either way (pinned in
//! [`super::responses`]'s tests); a live turn showing the gateway omitting
//! `output_item.done` would be a one-arm fix, and is recorded here rather than
//! guessed at.
//!
//! Every "dropped" row above is a *refusal to guess*, not a finding that the
//! vendor refuses the field. The one thing that would settle them is a live
//! turn against the real service, which is why the opt-in live test exists; a
//! probe that shows the replay accepted turns three of these rows into one
//! `super::responses::Backend` predicate flipping to `true`.
//!
//! # Out of scope, named rather than forgotten
//!
//! - **OpenRouter's PKCE key-provisioning flow.** `ganja auth login openrouter`
//!   stores a key somebody fetched from the vendor's console, which is the path
//!   every key provider here already uses. Recorded as a follow-up.
//! - **The attribution headers** upstream's loader sets — `HTTP-Referer:
//!   https://opencode.ai/` and `X-Title: opencode` (`provider.ts:467-476`, and
//!   still set at that project's HEAD, `plugin/provider/openrouter.ts:13-24` at
//!   `e23586af`). They are *self*-attribution: they identify opencode to the
//!   vendor's public leaderboards. Sending them would file ganja's traffic
//!   under another project's name, and inventing ganja's own values is a
//!   decision about somebody's public listing rather than a port. Recorded, not
//!   sent.
//! - **A base-URL environment override.** `ANTHROPIC_BASE_URL` and
//!   `OPENAI_BASE_URL` exist because they are those vendors' own SDK variables;
//!   OpenRouter publishes exactly one variable, the key ([`API_KEY_ENV`], which
//!   is also the `env` the model catalog lists for this provider). A config
//!   `provider` entry already reaches an arbitrary endpoint, so a variable
//!   invented here would be a third way to say what two already say.

use crate::provider::{
    CredentialSource, ProviderError, ResponsesProvider, require_key, responses::Backend,
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
///
/// The id the model catalog publishes this vendor's rows under, which is what
/// makes them resolve at all: `catalog::model` answers per provider, so a
/// spelling of ganja's own would silently cost this provider its sizing, its
/// pricing and its auto-compaction. Upstream's `ProviderV2.ID.openrouter` is
/// the same string, so a shared `auth.json` needs no alias either — see the
/// test below, which pins both halves.
pub const ID: &str = "openrouter";

/// Environment variable carrying the credential.
///
/// The vendor's own name for it, and the one the catalog lists in this
/// provider's `env` — so a shell already set up for the vendor's `curl`
/// examples needs no further configuration. `auth::KEY_VARS` is where it earns
/// its precedence over a stored key, exactly as the two older key providers do.
pub const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Where this vendor's API lives, which is also the `api` its catalog row
/// publishes. The path this wire appends is `/responses`, so the whole URL is
/// the reference's own `https://openrouter.ai/api/v1/responses`.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Models this vendor publishes that no Responses request may name
/// (`provider.ts:1625-1631`).
///
/// Upstream deletes two ids from openrouter's roster — `gpt-5-chat-latest` and
/// `openai/gpt-5-chat` — with the reason in its own comment: "These chat
/// aliases are invalid for the special handling in the built-in providers
/// below", where that handling is the Responses routing its built-in providers
/// do. Ganja refuses at the wire instead of hiding rows, for the reason
/// [`super::responses::CHAT_COMPLETIONS_ONLY`] gives: the fetched catalog is
/// upstream's own file and a filter over the compiled-in snapshot would stop
/// applying the first time anybody refreshed.
///
/// Only one id is here because the other, `gpt-5-chat-latest`, is upstream's
/// for this provider *and* for openai, and is already refused unconditionally
/// by that constant.
///
/// **The rule outlived the file it was ported from**, which is what makes it
/// worth keeping: the pinned spec's version is one arm of a loop over every
/// provider's models, and at that project's HEAD (`e23586af`) it has moved into
/// a dedicated `plugin/provider/openrouter.ts:13-24` that disables the same two
/// ids by name. A carve-out that survived being refactored is a rule the
/// vendor's own client still enforces, not a stale line.
///
/// **Neither id appears in this vendor's current rows** (checked against the
/// catalog's own `api.json`, 2026-08-14 — what it publishes today is
/// `openai/gpt-chat-latest` and `openai/gpt-5.2-chat`), so this arm is dormant
/// as it stands. It is upstream's exact identity list all the same, and
/// deliberately not a broader guess: a gateway's roster comes and goes, and
/// deciding for it which of *today's* chat models its own translation layer
/// cannot serve is the invention this port does not make.
pub(super) const CHAT_COMPLETIONS_ONLY: [&str; 1] = ["openai/gpt-5-chat"];

/// The tools this vendor runs on its own side, by the name a config asks for
/// them under (**D489**).
///
/// Spec: `docs/guides/features/server-tools`, its roster table read 2026-08-14.
/// The wire spelling is [`SERVER_TOOL_PREFIX`] and the name, which is how the
/// reference publishes each of them (`openrouter:web_search` and so on); a
/// config names the half after the colon, because the half before it is this
/// provider's identity and repeating it in every entry would be noise.
///
/// **An identity list, deliberately.** A config naming anything outside it is
/// refused at load rather than forwarded: an unknown `openrouter:whatever` is a
/// row the gateway would reject *mid-turn*, and a typo somebody has to read a
/// 400 to find is worse than one the config file names back at them. The cost is
/// that a tool this vendor adds tomorrow needs a line here — which is the same
/// bargain every curated list in the config makes, and this one bills per call.
///
/// The roster is what the reference publishes and **not** what this build
/// guarantees to render richly: `fusion`, `advisor` and `subagent` run whole
/// model panels behind one call, and what a transcript shows for one of those is
/// the row and its result, the same as for a search.
pub const SERVER_TOOLS: [&str; 10] = [
    "web_search",
    "datetime",
    "image_generation",
    "web_fetch",
    "apply_patch",
    "shell",
    "fusion",
    "advisor",
    "subagent",
    "experimental__search_models",
];

/// What every server tool's `type` starts with, on the way out and on the way
/// back.
///
/// Both directions matter and the reference documents both: a request asks for
/// `{"type": "openrouter:web_search"}`, and on the Responses API "the call
/// becomes an `openrouter:shell` output item"
/// (`docs/guides/features/server-tools/shell`, read 2026-08-14). That sentence
/// is what lets [`super::responses`] recognize one of these on the way in
/// without guessing at a shape — see its own decode arm for what it does and
/// does not read out of the item.
pub const SERVER_TOOL_PREFIX: &str = "openrouter:";

/// Whether `name` is a server tool this build will ask for.
///
/// The one door config validation goes through, so the roster is stated once.
#[must_use]
pub fn serves_server_tool(name: &str) -> bool {
    SERVER_TOOLS.contains(&name)
}

/// The provider against OpenRouter's own endpoint, authenticated by the key
/// [`API_KEY_ENV`] or the credential store carries.
///
/// The lookup is [`super::key_for`]'s, so the precedence is the one every key
/// provider here has: an exported key outranks a stored one, and a session with
/// neither dies at startup naming the variable rather than at the first prompt.
///
/// # Errors
///
/// Returns [`ProviderError::Auth`] when there is no key to send, and
/// [`ProviderError::Transport`] when no HTTP client can be built.
pub fn from_env() -> Result<ResponsesProvider, ProviderError> {
    ResponsesProvider::built(
        CredentialSource::Key(require_key(ID, API_KEY_ENV)?),
        DEFAULT_BASE_URL.to_owned(),
        Backend::OpenRouter,
    )
}

#[cfg(test)]
#[path = "openrouter_tests.rs"]
mod tests;
