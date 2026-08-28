use super::*;

fn ctx() -> ToolCtx {
    ToolCtx::fixture(std::path::PathBuf::from("/"))
}

fn definition(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        schema: serde_json::json!({
            "type": "object",
            "properties": { "input": { "type": "string" } },
        }),
    }
}

fn shared(names: &[&str]) -> Arc<Mutex<BTreeSet<String>>> {
    Arc::new(Mutex::new(names.iter().map(|&name| name.to_owned()).collect()))
}

#[test]
fn nothing_defers_at_or_under_the_threshold() {
    let names = ["mcp__a__one", "mcp__a__two", "mcp__b__one", "read", "bash"];

    assert!(candidates(names, 3, &BTreeSet::new()).is_empty());
    assert!(candidates(names, usize::MAX, &BTreeSet::new()).is_empty());
}

#[test]
fn whole_servers_defer_largest_first_until_the_total_fits() {
    let names = [
        "mcp__big__a",
        "mcp__big__b",
        "mcp__big__c",
        "mcp__big__d",
        "mcp__big__e",
        "mcp__mid__a",
        "mcp__mid__b",
        "mcp__mid__c",
        "mcp__small__a",
        "mcp__small__b",
        "read",
    ];

    let at_five = candidates(names, 5, &BTreeSet::new());
    assert_eq!(
        at_five,
        ["mcp__big__a", "mcp__big__b", "mcp__big__c", "mcp__big__d", "mcp__big__e"]
            .map(str::to_owned)
            .into(),
        "deferring the biggest server alone brings 10 down to 5"
    );

    let at_four = candidates(names, 4, &BTreeSet::new());
    assert_eq!(at_four.len(), 8, "big and mid defer; small stays whole");
    assert!(!at_four.contains("mcp__small__a"));
    assert!(!at_four.contains("mcp__small__b"));
}

#[test]
fn equal_sized_servers_defer_in_name_order() {
    let names = ["mcp__beta__x", "mcp__beta__y", "mcp__alpha__x", "mcp__alpha__y"];

    let deferred = candidates(names, 2, &BTreeSet::new());

    assert_eq!(
        deferred,
        ["mcp__alpha__x", "mcp__alpha__y"].map(str::to_owned).into(),
        "the tie breaks toward the name that sorts first"
    );
}

#[test]
fn activated_names_are_exempt_before_the_arithmetic_starts() {
    let names = ["mcp__a__one", "mcp__a__two", "mcp__a__three", "mcp__a__four", "mcp__a__five"];
    let activated: BTreeSet<String> = ["mcp__a__one", "mcp__a__two"].map(str::to_owned).into();

    assert!(
        candidates(names, 3, &activated).is_empty(),
        "three never-touched names fit a threshold of three"
    );

    let deferred = candidates(names, 2, &activated);
    assert_eq!(deferred.len(), 3, "only the never-touched names defer");
    assert!(!deferred.contains("mcp__a__one"));
    assert!(!deferred.contains("mcp__a__two"));
}

/// The exact plugin-namespaced spelling that lives in the tree
/// (`provider/toolname.rs`'s own pinned name), grouped under the first
/// `__` after the prefix — and a tool whose *own* name carries `__`,
/// where first and last separators disagree.
#[test]
fn a_server_key_is_everything_up_to_the_first_separator() {
    let names = [
        "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result",
        "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research",
        "mcp__docs__search__v2",
    ];

    let deferred = candidates(names, 1, &BTreeSet::new());

    assert_eq!(
        deferred,
        [
            "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result",
            "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research",
        ]
        .map(str::to_owned)
        .into(),
        "the two-plugin-tool server defers whole; `docs` (one tool, its name's \
             own `__` notwithstanding) stays advertised"
    );
}

#[test]
fn advertised_is_not_a_candidate_or_activated() {
    let deferral = Deferral::over(
        ["mcp__s__deferred", "mcp__s__touched"].map(str::to_owned).into(),
        shared(&["mcp__s__touched"]),
    );

    assert!(!deferral.advertised("mcp__s__deferred"));
    assert!(deferral.advertised("mcp__s__touched"));
    assert!(deferral.advertised("read"), "a non-candidate always rides");
}

#[test]
fn retain_advertised_preserves_registration_order() {
    let deferral =
        Deferral::over(["mcp__s__a", "mcp__s__b"].map(str::to_owned).into(), shared(&[]));
    let mut definitions = vec![
        definition("read", "reads"),
        definition("mcp__s__a", "deferred"),
        definition("bash", "runs"),
        definition("mcp__s__b", "deferred"),
        definition("mcp__t__c", "kept"),
    ];

    deferral.retain_advertised(&mut definitions);

    assert_eq!(
        definitions.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        ["read", "bash", "mcp__t__c"],
        "the deferred leave; everything else keeps its order"
    );
}

#[test]
fn an_activation_through_one_clone_is_read_by_every_other() {
    let deferral = Deferral::over(["mcp__s__a"].map(str::to_owned).into(), shared(&[]));
    let clone = deferral.clone();

    assert!(!deferral.advertised("mcp__s__a"));
    assert!(clone.activate("mcp__s__a"), "the first insert is growth");
    assert!(!clone.activate("mcp__s__a"), "the second is not");
    assert!(deferral.advertised("mcp__s__a"));
    assert!(deferral.activated().contains("mcp__s__a"));
}

#[test]
fn the_listing_names_the_deferred_and_shrinks_on_activation() {
    let deferral =
        Deferral::over(["mcp__s__a", "mcp__s__b"].map(str::to_owned).into(), shared(&[]));
    let definitions = vec![
        definition("read", "reads a file"),
        definition("mcp__s__a", "does the first thing"),
        definition("mcp__s__b", "does the second thing"),
    ];

    let listing = deferral.listing(&definitions);
    assert!(listing.starts_with("<deferred_tools>\n"));
    assert!(listing.contains("`tool_search`"), "the header names the door");
    assert!(listing.contains("- mcp__s__a: does the first thing"));
    assert!(listing.contains("- mcp__s__b: does the second thing"));
    assert!(!listing.contains("read"), "advertised tools are not listed");

    deferral.activate("mcp__s__a");
    let shrunk = deferral.listing(&definitions);
    assert!(!shrunk.contains("mcp__s__a"));
    assert!(shrunk.contains("mcp__s__b"));

    deferral.activate("mcp__s__b");
    assert_eq!(deferral.listing(&definitions), "", "everything activated appends nothing");
}

#[test]
fn a_description_is_clamped_to_one_line() {
    let long = "x".repeat(200);
    let deferral =
        Deferral::over(["mcp__s__a", "mcp__s__b"].map(str::to_owned).into(), shared(&[]));
    let definitions = vec![
        definition("mcp__s__a", &format!("{long}\nsecond line never shows")),
        definition("mcp__s__b", "short"),
    ];

    let listing = deferral.listing(&definitions);
    let line =
        listing.lines().find(|line| line.starts_with("- mcp__s__a:")).expect("the tool is listed");

    assert!(!line.contains("second line"), "only the first line rides");
    assert_eq!(
        line.chars().count(),
        "- mcp__s__a: ".chars().count() + CLAMP + 1,
        "the description is cut at the clamp, plus the mark that says so"
    );
    assert!(line.ends_with('…'));
}

#[test]
fn an_empty_deferral_lists_nothing_and_filters_nothing() {
    let deferral = Deferral::none();
    let mut definitions = vec![definition("mcp__s__a", "big server tool")];

    assert_eq!(deferral.listing(&definitions), "");
    assert!(!deferral.any());
    deferral.retain_advertised(&mut definitions);
    assert_eq!(definitions.len(), 1, "nothing is a candidate, nothing leaves");
}

fn search_over(definitions: Vec<ToolDefinition>, deferral: &Deferral) -> ToolSearchTool {
    ToolSearchTool::over(Arc::new(Mutex::new(definitions)), deferral.clone())
}

#[tokio::test]
async fn a_select_returns_the_schema_and_activates() {
    let deferral = Deferral::over(["mcp__s__t"].map(str::to_owned).into(), shared(&[]));
    let tool = search_over(
        vec![definition("read", "reads"), definition("mcp__s__t", "the deferred one")],
        &deferral,
    );

    let out = tool
        .run(serde_json::json!({ "query": "select:mcp__s__t" }), &ctx())
        .await
        .expect("a select over a deferred name answers");

    assert_eq!(out.title, "tool_search: activated mcp__s__t");
    assert!(out.output.contains("## mcp__s__t"));
    assert!(out.output.contains("the deferred one"));
    assert!(out.output.contains(r#""input""#), "the full schema rides the result: {}", out.output);
    assert!(deferral.advertised("mcp__s__t"), "the hit activated it");
}

#[tokio::test]
async fn a_batch_select_activates_every_name_in_one_call() {
    let deferral = Deferral::over(
        ["mcp__s__a", "mcp__s__b", "mcp__s__c"].map(str::to_owned).into(),
        shared(&[]),
    );
    let tool = search_over(
        vec![
            definition("mcp__s__a", "first"),
            definition("mcp__s__b", "second"),
            definition("mcp__s__c", "third"),
        ],
        &deferral,
    );

    let out = tool
        .run(serde_json::json!({ "query": "select:mcp__s__a, mcp__s__b, mcp__s__c" }), &ctx())
        .await
        .expect("the batch answers");

    assert_eq!(out.title, "tool_search: activated 3 tools");
    for name in ["mcp__s__a", "mcp__s__b", "mcp__s__c"] {
        assert!(deferral.advertised(name), "{name} activated in the one call");
    }
}

#[tokio::test]
async fn keywords_rank_by_relevance_and_the_cap_holds() {
    let deferral = Deferral::over(
        ["mcp__s__notebook_edit", "mcp__s__notebook_read", "mcp__s__unrelated"]
            .map(str::to_owned)
            .into(),
        shared(&[]),
    );
    let tool = search_over(
        vec![
            definition("mcp__s__notebook_edit", "edits a jupyter notebook"),
            definition("mcp__s__notebook_read", "reads a jupyter notebook"),
            definition("mcp__s__unrelated", "sends a message"),
        ],
        &deferral,
    );

    let out = tool
        .run(serde_json::json!({ "query": "jupyter notebook", "max_results": 1 }), &ctx())
        .await
        .expect("keywords answer");

    assert_eq!(out.title, "tool_search: activated mcp__s__notebook_edit");
    assert!(!out.output.contains("mcp__s__notebook_read"), "max_results capped the matches to one");
    assert!(!out.output.contains("mcp__s__unrelated"));
}

#[tokio::test]
async fn an_empty_deferred_set_answers_that_nothing_is_deferred() {
    let deferral = Deferral::over(["mcp__s__t"].map(str::to_owned).into(), shared(&["mcp__s__t"]));
    let tool = search_over(vec![definition("mcp__s__t", "already touched")], &deferral);

    let out = tool
        .run(serde_json::json!({ "query": "anything" }), &ctx())
        .await
        .expect("an empty set is an answer, not an error");

    assert_eq!(out.title, "tool_search: nothing is deferred");
    assert!(out.output.contains("already advertised"));
}

#[tokio::test]
async fn a_failed_select_answers_with_near_misses() {
    let deferral = Deferral::over(
        ["mcp__github__create_issue", "mcp__github__list_issues"].map(str::to_owned).into(),
        shared(&[]),
    );
    let tool = search_over(
        vec![
            definition("read", "reads"),
            definition("mcp__github__create_issue", "opens an issue"),
            definition("mcp__github__list_issues", "lists issues"),
        ],
        &deferral,
    );

    let out = tool
        .run(serde_json::json!({ "query": "select:mcp__github__issues, read" }), &ctx())
        .await
        .expect("a miss is information, never an error");

    assert_eq!(out.title, "tool_search: nothing activated");
    assert!(
        out.output.contains("No deferred tool is named `mcp__github__issues`"),
        "{}",
        out.output
    );
    assert!(
        out.output.contains("mcp__github__list_issues"),
        "the near-misses name the neighbours: {}",
        out.output
    );
    assert!(out.output.contains("`read` is already advertised"), "{}", out.output);
    assert!(!deferral.advertised("mcp__github__create_issue"));
}

/// The two numbers the description promises: five matches when the model
/// names no count, and never more than twenty however many it asks for.
/// Activation is sticky, so the second call ranks what the first left.
#[tokio::test]
async fn keyword_matches_default_to_five_and_clamp_at_twenty() {
    let names: Vec<String> = (0..25).map(|n| format!("mcp__s__thing_{n:02}")).collect();
    let deferral = Deferral::over(names.iter().cloned().collect(), shared(&[]));
    let definitions: Vec<ToolDefinition> =
        names.iter().map(|name| definition(name, "does a thing")).collect();
    let tool = search_over(definitions, &deferral);

    let defaulted =
        tool.run(serde_json::json!({ "query": "thing" }), &ctx()).await.expect("keywords answer");
    assert_eq!(
        defaulted.metadata["activated"]
            .as_array()
            .expect("the metadata lists what was activated")
            .len(),
        5,
        "no count asked for is five"
    );

    let clamped = tool
        .run(serde_json::json!({ "query": "thing", "max_results": 50 }), &ctx())
        .await
        .expect("keywords answer");
    assert_eq!(
        clamped.metadata["activated"]
            .as_array()
            .expect("the metadata lists what was activated")
            .len(),
        20,
        "fifty asked for is twenty, ranked over the twenty still deferred"
    );
}

/// The first paragraph is model-facing contract: the `select:` grammar
/// and the batch-first phrasing. A change here is a change to what every
/// model is taught, so it is pinned byte-for-byte.
#[test]
fn the_description_opens_with_the_select_grammar() {
    let first = DESCRIPTION.split("\n\n").next().expect("the description has paragraphs");

    assert_eq!(
        first,
        "Fetch deferred tools' JSON schemas and activate those tools for the rest of \
             the session. Prefer one batched `select:` call: `select:name1,name2` names \
             deferred tools exactly (copy the names from the deferred-tools listing) and \
             activates every named tool at once. Any other query is matched as keywords \
             against the deferred tools' names and descriptions."
    );
}
