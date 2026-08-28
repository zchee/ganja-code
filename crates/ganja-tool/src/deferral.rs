//! Deferred tool schemas: what is *advertised* is a subset of what is
//! *registered*, and `tool_search` is the resident door back in.
//!
//! No upstream opencode counterpart at all. The behavior modeled is Claude
//! Code's ToolSearch — deferred names ride every request, schemas arrive on
//! demand, an activated tool is usable from the next step of the same turn —
//! with one deliberate departure the engine records as **D492** at its
//! filter: a direct call to a deferred tool executes, because a tool result
//! is information and a correct guess is not an error.
//!
//! [`Deferral`](crate::deferral::Deferral) is a value in the same spirit as
//! `SkillTool`'s roots: the
//! engine computes which names defer and hands a clone to each turn, and the
//! only shared state is the insert-only activated set behind an `Arc` — a
//! `tool_search` hit, an executed direct call, or resume seeding writes it,
//! nothing ever removes from it, so a tool this session has touched is never
//! un-advertised. Registration is untouched throughout: permission, hook and
//! transcript keys never change, and a call that resolves in the registry
//! executes through the code path it always had.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use ganja_permission::permission::MCP_PREFIX;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolCtx, ToolDefinition, ToolError, ToolOutput};

/// The tool's registry name. Never `mcp__`-prefixed and not in
/// `ASK_BY_DEFAULT`, so it runs unasked — it is read-only over what the
/// engine already holds.
pub const TOOL_SEARCH: &str = "tool_search";

/// What the model is told `tool_search` is for. Ganja's own text — there is
/// nothing upstream to port — and the first paragraph is model-facing
/// contract (the `select:` grammar, batch first), pinned by a test.
pub const DESCRIPTION: &str = "\
Fetch deferred tools' JSON schemas and activate those tools for the rest of \
the session. Prefer one batched `select:` call: `select:name1,name2` names \
deferred tools exactly (copy the names from the deferred-tools listing) and \
activates every named tool at once. Any other query is matched as keywords \
against the deferred tools' names and descriptions.

Each request names the deferred tools with a one-line description; the full \
schemas are withheld until fetched here. A match comes back with its name, \
description and full argument schema, is callable immediately, and its \
schema stays advertised for the rest of the session. Calling a deferred \
tool directly also works: the call executes, and executing activates the \
tool the same way. `max_results` caps keyword matches (default 5, at most \
20); a `select:` query ignores it and answers every name it lists.";

/// Longest run of characters the deferred listing spends on one tool's
/// description — an order of magnitude under the schema it stands in for.
const CLAMP: usize = 120;

/// Keyword matches returned when the model names no count.
const DEFAULT_RESULTS: usize = 5;

/// The most keyword matches one call may return.
const MAX_RESULTS: usize = 20;

/// Near-miss names offered for one failed `select:` entry.
const NEAR_MISSES: usize = 3;

/// The registry names to defer: whole servers, largest first, until the
/// advertised `mcp__*` total is at or under `threshold`.
///
/// Grouping is by the `<server>` segment of `mcp__<server>__<tool>`, split at
/// the **first** `__` after the prefix. A pathological server key whose
/// sanitized name itself contains `__` mis-groups harmlessly: the deferral
/// arithmetic blurs, and no permission or execution identity moves. Groups
/// sort by (tool count descending, name ascending) so the order is total and
/// a recompute is deterministic.
///
/// `activated` names are exempt *before* the arithmetic starts: they are
/// advertised regardless and count toward nothing, so a recompute — a
/// reconnect, a `/plugin` Reload — runs over never-touched names only and
/// can never cascade into withdrawing an activated tool.
///
/// The threshold is denominated in tools, not schema bytes, on purpose: it
/// is the unit a person can count in `/mcp`'s own listing and the unit
/// Claude Code's visible contract is shaped in. The byte-accurate signal
/// stays available in `definitions()` if field use ever wants a smarter
/// sort.
#[must_use]
pub fn candidates<'a>(
    names: impl IntoIterator<Item = &'a str>,
    threshold: usize,
    activated: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for name in names {
        let Some(rest) = name.strip_prefix(MCP_PREFIX) else {
            continue;
        };
        if activated.contains(name) {
            continue;
        }
        let server = rest.split_once("__").map_or(rest, |(server, _)| server);
        groups.entry(server).or_default().push(name);
    }

    let mut advertised: usize = groups.values().map(Vec::len).sum();
    if advertised <= threshold {
        return BTreeSet::new();
    }

    let mut ordered: Vec<(&str, Vec<&str>)> = groups.into_iter().collect();
    ordered
        .sort_by(|left, right| right.1.len().cmp(&left.1.len()).then_with(|| left.0.cmp(right.0)));

    let mut deferred = BTreeSet::new();
    for (server, members) in ordered {
        if advertised <= threshold {
            break;
        }
        advertised -= members.len();
        tracing::debug!(
            server,
            tools = members.len(),
            threshold,
            "deferring an MCP server's schemas"
        );
        deferred.extend(members.into_iter().map(str::to_owned));
    }

    deferred
}

/// Which registry names are deferred, and which of those this session has
/// activated.
///
/// The engine computes one per registry composition and every turn carries a
/// clone — a child turn inherits its parent's — so every consumer reads the
/// same advertised subset and every activation lands in the same
/// session-lifetime set. The candidates travel by value; the activated set is
/// the one shared handle, insert-only, with exactly three writer contexts:
/// [`ToolSearchTool`]'s hit, the engine's executed-call insert, and resume
/// seeding.
#[derive(Clone, Debug, Default)]
pub struct Deferral {
    /// Registry names currently deferred (whole servers, largest first).
    candidates: BTreeSet<String>,
    /// Names activated for this session — by a `tool_search` hit, by an
    /// executed direct call, or by resume seeding. Insert-only.
    activated: Arc<Mutex<BTreeSet<String>>>,
}

impl Deferral {
    /// Nothing deferred: the fixture and no-MCP default, under which every
    /// request is byte-identical to one built with no deferral at all.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The deferral over exactly `candidates`, activations landing in
    /// `activated` — the engine's own handle, shared so an insert made
    /// through any clone is read by every other.
    #[must_use]
    pub fn over(candidates: BTreeSet<String>, activated: Arc<Mutex<BTreeSet<String>>>) -> Self {
        Self { candidates, activated }
    }

    /// Whether anything at all defers — what decides that `tool_search`
    /// joins the roster.
    #[must_use]
    pub fn any(&self) -> bool {
        !self.candidates.is_empty()
    }

    /// Whether `name`'s schema rides the request: not a candidate, or
    /// activated.
    #[must_use]
    pub fn advertised(&self, name: &str) -> bool {
        !self.candidates.contains(name) || self.lock().contains(name)
    }

    /// Records that this session touched `name`; from the next step it is
    /// advertised for the rest of the session. Returns whether the name is
    /// newly activated — the caller's growth signal.
    pub fn activate(&self, name: &str) -> bool {
        self.lock().insert(name.to_owned())
    }

    /// A snapshot of the activated set, for the engine's durability
    /// watermark.
    #[must_use]
    pub fn activated(&self) -> BTreeSet<String> {
        self.lock().clone()
    }

    /// The one filter between the registry and the model: an
    /// order-preserving retain of what is advertised, used by the request
    /// build and by every other place "what the model sees" is computed.
    pub fn retain_advertised(&self, definitions: &mut Vec<ToolDefinition>) {
        if self.candidates.is_empty() {
            return;
        }
        let activated = self.lock();
        definitions.retain(|definition| {
            !self.candidates.contains(&definition.name) || activated.contains(&definition.name)
        });
    }

    /// The per-step block naming what is deferred right now — one clamped
    /// line per tool, recomputed each step so activated entries drop out,
    /// and an empty string (appended nowhere) when nothing is deferred.
    #[must_use]
    pub fn listing(&self, definitions: &[ToolDefinition]) -> String {
        if self.candidates.is_empty() {
            return String::new();
        }
        let activated = self.lock();
        let deferred: Vec<&ToolDefinition> = definitions
            .iter()
            .filter(|definition| {
                self.candidates.contains(&definition.name) && !activated.contains(&definition.name)
            })
            .collect();
        if deferred.is_empty() {
            return String::new();
        }

        let mut block = String::from(
            "<deferred_tools>\nThese tools are registered but their schemas are \
             not advertised; a schema is fetched through `tool_search` (batch \
             `select:name1,name2` preferred), and a direct call also executes \
             and activates the tool.\n",
        );
        for definition in deferred {
            let _ = writeln!(block, "- {}: {}", definition.name, clamped(&definition.description));
        }
        block.push_str("</deferred_tools>");

        block
    }

    fn lock(&self) -> MutexGuard<'_, BTreeSet<String>> {
        self.activated.lock().expect("the activated set is never poisoned")
    }
}

/// The first line of `description`, cut at [`CLAMP`] characters.
fn clamped(description: &str) -> String {
    let line = description.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(CLAMP).collect();
    if line.chars().count() > CLAMP {
        out.push('…');
    }

    out
}

/// The resident door back into the deferred set: fetches schemas by exact
/// name or by keyword, and a fetch activates.
///
/// Registered by the engine only while something is deferred, beside every
/// builtin and never itself deferred. The definitions snapshot is the
/// engine's, written at registry rebuild and shared rather than copied so a
/// reconnect's recomposition is what a later search reads.
pub struct ToolSearchTool {
    /// The composed registry's definitions, as last rebuilt.
    catalog: Arc<Mutex<Vec<ToolDefinition>>>,
    /// The same value the session's turns carry, so a hit lands in the same
    /// activated set the request build reads.
    deferral: Deferral,
}

impl ToolSearchTool {
    /// The tool over the engine's definitions snapshot and the session's
    /// deferral.
    #[must_use]
    pub fn over(catalog: Arc<Mutex<Vec<ToolDefinition>>>, deferral: Deferral) -> Self {
        Self { catalog, deferral }
    }
}

/// Arguments the model sends `tool_search`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// `select:name1,name2` with exact deferred names, or keywords matched
    /// against the deferred tools' names and descriptions.
    query: String,
    /// Most keyword matches returned (default 5, at most 20). A `select:`
    /// query ignores it.
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn id(&self) -> &str {
        TOOL_SEARCH
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        match args.get("query").and_then(serde_json::Value::as_str) {
            Some(query) => format!("{TOOL_SEARCH} {query}"),
            None => TOOL_SEARCH.to_owned(),
        }
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        let catalog =
            self.catalog.lock().expect("the definitions snapshot is never poisoned").clone();
        let deferred: Vec<ToolDefinition> = catalog
            .iter()
            .filter(|definition| !self.deferral.advertised(&definition.name))
            .cloned()
            .collect();

        if deferred.is_empty() {
            return Ok(ToolOutput {
                title: "tool_search: nothing is deferred".to_owned(),
                output:
                    "Nothing is deferred: every registered tool's schema is already advertised."
                        .to_owned(),
                metadata: json!({ "activated": [] }),
            });
        }

        let (matches, mut notes) = match args.query.strip_prefix("select:") {
            Some(named) => selected(named, &catalog, &deferred),
            None => (keyword_matches(&args.query, &deferred, cap(args.max_results)), Vec::new()),
        };
        if matches.is_empty() && notes.is_empty() {
            notes.push(format!(
                "No deferred tool matches {:?}. The deferred tools are named in each \
                 request's <deferred_tools> block; `select:` with an exact name always \
                 answers.",
                args.query
            ));
        }

        let mut activated = Vec::new();
        for definition in &matches {
            if self.deferral.activate(&definition.name) {
                tracing::debug!(tool = %definition.name, by = "search", "activated a deferred tool");
            }
            activated.push(definition.name.clone());
        }

        let mut output = String::new();
        if !matches.is_empty() {
            let _ = writeln!(
                output,
                "Activated {}; each is callable now, and its schema is advertised from \
                 the next step for the rest of the session.",
                match matches.len() {
                    1 => "1 tool".to_owned(),
                    n => format!("{n} tools"),
                }
            );
            for definition in &matches {
                let _ = write!(
                    output,
                    "\n## {}\n{}\n\nSchema: {}\n",
                    definition.name, definition.description, definition.schema
                );
            }
        }
        if !notes.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&notes.join("\n"));
        }

        Ok(ToolOutput {
            title: match activated.len() {
                0 => "tool_search: nothing activated".to_owned(),
                1 => format!("tool_search: activated {}", activated[0]),
                n => format!("tool_search: activated {n} tools"),
            },
            output,
            metadata: json!({ "activated": activated }),
        })
    }
}

/// The keyword cap: what the model asked for, bounded.
fn cap(max_results: Option<usize>) -> usize {
    max_results.unwrap_or(DEFAULT_RESULTS).min(MAX_RESULTS)
}

/// Resolves one `select:` list: exact deferred names match, an
/// already-advertised name is said to be, and a name matching nothing is
/// answered with its nearest deferred neighbours. All of it is information
/// the model reads — a miss never fails the call.
fn selected(
    named: &str,
    catalog: &[ToolDefinition],
    deferred: &[ToolDefinition],
) -> (Vec<ToolDefinition>, Vec<String>) {
    let mut matches = Vec::new();
    let mut notes = Vec::new();
    for name in named.split(',').map(str::trim).filter(|name| !name.is_empty()) {
        if let Some(definition) = deferred.iter().find(|definition| definition.name == name) {
            matches.push(definition.clone());
        } else if catalog.iter().any(|definition| definition.name == name) {
            notes.push(format!("`{name}` is already advertised; call it directly."));
        } else {
            let near = near_misses(name, deferred);
            if near.is_empty() {
                notes.push(format!("No deferred tool is named `{name}`."));
            } else {
                notes.push(format!(
                    "No deferred tool is named `{name}`; closest deferred names: {}.",
                    near.join(", ")
                ));
            }
        }
    }

    (matches, notes)
}

/// The still-deferred names nearest one failed `select:` entry.
fn near_misses(name: &str, deferred: &[ToolDefinition]) -> Vec<String> {
    let pattern = Pattern::parse(name, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored: Vec<(u32, &str)> = deferred
        .iter()
        .filter_map(|definition| {
            pattern
                .score(Utf32Str::new(&definition.name, &mut buffer), &mut matcher)
                .map(|score| (score, definition.name.as_str()))
        })
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));

    scored.into_iter().take(NEAR_MISSES).map(|(_, name)| name.to_owned()).collect()
}

/// The deferred tools ranked against a keyword query, best first, at most
/// `cap` of them. Ties break on the tool's own name so a query always
/// produces the same list.
fn keyword_matches(query: &str, deferred: &[ToolDefinition], cap: usize) -> Vec<ToolDefinition> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored: Vec<(u32, &ToolDefinition)> = deferred
        .iter()
        .filter_map(|definition| {
            let haystack = format!("{} {}", definition.name, definition.description);
            pattern
                .score(Utf32Str::new(&haystack, &mut buffer), &mut matcher)
                .map(|score| (score, definition))
        })
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.name.cmp(&right.1.name)));

    scored.into_iter().take(cap).map(|(_, definition)| definition.clone()).collect()
}

#[cfg(test)]
#[path = "deferral_tests.rs"]
mod tests;
