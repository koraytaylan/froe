//! The second Lucene definition the fixture carries, and the content its
//! rules are written for.
//!
//! The definition Sling ships — an `nt:base` rule over strings and binaries
//! with one catch-all regular expression — is the classic "everything
//! fulltext" shape, and the skeleton of AEM's own `cqPageLucene`,
//! `damAssetLucene` and `lucene` definitions. It exercises none of the
//! branches plan 0010's document maker has for `evaluatePathRestrictions`,
//! ordered doc values, null checks, facets and aggregates. This module is
//! the fixture's coverage of those, posted through Sling like every other
//! shape so the bytes are authentically Oak's.

use super::*;

/// The second Lucene definition's name under `/oak:index`.
///
/// A name of its own rather than a second `lucene`: plan 0008's transport
/// phases select and compare per definition, so the fixture may carry more
/// than one, and the reindex phase's query comparison has to be able to say
/// *which* index Oak planned against.
pub(crate) const LUCENE_VARIANT_DEFINITION: &str = "interopLucene";

/// The subtree the variant definition indexes, and the only subtree its
/// queries are restricted to.
///
/// Both `includedPaths` and `queryPaths` name it. `queryPaths` is what
/// makes Oak's own fulltext planner offer the index to a query — it offers
/// one with `queryPaths` only to a query restricted at or below them — and
/// `includedPaths` is what keeps the index small enough that the planner
/// prefers it over the fixture's repository-wide `lucene`.
pub(crate) const LUCENE_VARIANT_SUBTREE: &str = "/content/interop/variant";

/// The node type the variant's one indexing rule is written for.
///
/// **Not** `nt:base`: Oak's own `IndexingRule.validateRuleDefinition`
/// refuses an `nt:base` rule that carries a `nullCheckEnabled` property
/// definition, and the `IS NULL` query this fixture exists to answer needs
/// one.
pub(crate) const LUCENE_VARIANT_NODE_TYPE: &str = "nt:unstructured";

/// The word every variant node's analyzed text carries, so one `CONTAINS`
/// returns the whole set.
pub(crate) const VARIANT_SHARED_WORD: &str = "vandrelith";

/// The label every item's multi-valued `variantTags` carries, so a facet
/// over that dimension has one count the whole item set fixes.
pub(crate) const VARIANT_SHARED_TAG: &str = "sharedtag";

/// The page child the second `relativeNode` aggregate include names, and
/// nothing else does.
pub(crate) const VARIANT_RELATIVE_CHILD: &str = "meta";

/// The page child whose type no indexing rule covers.
pub(crate) const VARIANT_UNRULED_CHILD: &str = "extra";

/// The subtree `excludedPaths` names, inside the included one.
pub(crate) const VARIANT_EXCLUDED_SUBTREE: &str = "/content/interop/variant/pages/excluded";

/// The prefix `valueExcludedPrefixes` refuses on the faceted category, so
/// half the items contribute no category field of any kind.
pub(crate) const VARIANT_EXCLUDED_CATEGORY: &str = "beta";

/// The node type of the page child whose **own type** declares an
/// aggregate and which no indexing rule covers.
///
/// `oak:Unstructured` extends `nt:base` rather than `nt:unstructured`, so
/// neither of the definition's two rules registers under it, and it
/// carries residual properties so a title can be set on it.
pub(crate) const VARIANT_UNRULED_NODE_TYPE: &str = "oak:Unstructured";

/// The node type of the page child the second `relativeNode` include
/// names, and of the second indexing rule.
pub(crate) const VARIANT_AGGREGATED_NODE_TYPE: &str = "sling:Folder";

/// How many items the variant subtree holds.
pub(crate) const VARIANT_ITEMS: u32 = 6;

/// How many of them carry `variantOptional`, the property the `IS NULL`
/// query is about.
pub(crate) const VARIANT_ITEMS_WITH_OPTIONAL: u32 = 4;

/// How many page-like trees it holds.
pub(crate) const VARIANT_PAGES: u32 = 3;

/// The content the variant definition's branches need.
///
/// One node per typed property the document maker has a branch for — a
/// long, a double, a date and a boolean beside the strings — a page-like
/// tree with `jcr:content` children for the aggregate, and a property
/// present on some nodes and absent on others for `nullCheckEnabled`.
///
/// The words are nonsense on purpose: a term that occurs nowhere else in
/// the repository makes a query's row set attributable to this content
/// rather than to whatever else the image ships.
pub(crate) fn populate_lucene_variant_content(port: u16) {
    sling_post(port, LUCENE_VARIANT_SUBTREE, "sling:Folder", "Variant");
    populate_variant_items(port);
    populate_variant_pages(port);
}

/// The one-node-per-typed-property half: a long, a double, a date, a
/// boolean, a multi-valued string and a property four of the six carry.
fn populate_variant_items(port: u16) {
    sling_post(
        port,
        &format!("{LUCENE_VARIANT_SUBTREE}/items"),
        "sling:Folder",
        "Variant Items",
    );
    let words = [
        "alphacorn",
        "betacorn",
        "gammacorn",
        "deltacorn",
        "epsiloncorn",
        "zetacorn",
    ];
    for item in 1..=VARIANT_ITEMS {
        let index = item as usize - 1;
        let path = format!("{LUCENE_VARIANT_SUBTREE}/items/item{item}");
        let title = format!("Variant Item {item}");
        // A word the tokenizer keeps whole and the word-delimiter filter
        // splits — an underscore joins words for UAX#29 and delimits for
        // the filter — so `indexOriginalTerm` has an original to keep
        // beside the parts, and the token-count cap has something to cut.
        let text = format!("{VARIANT_SHARED_WORD} {} wifi_router{item}", words[index]);
        let rank = (item * 10).to_string();
        let score = format!("{item}.5");
        let date = format!("2026-03-0{item}T12:00:00.000Z");
        let flag = if item % 2 == 0 { "true" } else { "false" };
        let category = if item % 2 == 0 { "beta" } else { "alpha" };
        let mut fields: Vec<(&str, &str)> = vec![
            ("jcr:primaryType", LUCENE_VARIANT_NODE_TYPE),
            ("jcr:title", &title),
            ("variantText", &text),
            ("variantRank@TypeHint", "Long"),
            ("variantRank", &rank),
            ("variantScore@TypeHint", "Double"),
            ("variantScore", &score),
            ("variantDate@TypeHint", "Date"),
            ("variantDate", &date),
            ("variantFlag@TypeHint", "Boolean"),
            ("variantFlag", flag),
            ("variantCategory", category),
        ];
        let optional = format!("present{item}");
        if item <= VARIANT_ITEMS_WITH_OPTIONAL {
            fields.push(("variantOptional", &optional));
        }
        // The multi-valued string the faceted, analyzed and node-scope
        // branches all see as an array: one label every item shares, so a
        // facet count is a number this fixture fixes, and one of its own.
        // A binary on a node with **no** `jcr:mimeType`, which is the gate
        // Oak's own extraction stops at: neither side indexes it, so the
        // comparison covers the branch rather than excluding it.
        fields.push(("variantBlob@TypeHint", "Binary"));
        fields.push(("variantBlob", "binarycorn without a declared type"));
        let own_tag = format!("tag{item}");
        fields.push(("variantTags@TypeHint", "String[]"));
        fields.push(("variantTags", VARIANT_SHARED_TAG));
        fields.push(("variantTags", &own_tag));
        sling_post_fields(port, &path, &fields);
    }
}

/// The page-like half, whose `jcr:content`, `meta` and `inner` children
/// are what the aggregate includes and the relative definitions reach.
fn populate_variant_pages(port: u16) {
    sling_post(
        port,
        &format!("{LUCENE_VARIANT_SUBTREE}/pages"),
        "sling:Folder",
        "Variant Pages",
    );
    // Inside `includedPaths` and named by `excludedPaths`: every node here
    // is one neither index carries.
    sling_post_fields(
        port,
        VARIANT_EXCLUDED_SUBTREE,
        &[
            ("jcr:primaryType", LUCENE_VARIANT_NODE_TYPE),
            ("jcr:title", "Excludedcorn Page"),
            ("variantText", "vandrelith excludedcorn"),
        ],
    );
    let page_words = ["pagecornone", "pagecorntwo", "pagecornthree"];
    for page in 1..=VARIANT_PAGES {
        let index = page as usize - 1;
        let path = format!("{LUCENE_VARIANT_SUBTREE}/pages/page{page}");
        let title = format!("Variant Page {page}");
        sling_post_fields(
            port,
            &path,
            &[
                ("jcr:primaryType", LUCENE_VARIANT_NODE_TYPE),
                ("jcr:title", &title),
            ],
        );
        let text = format!("{VARIANT_SHARED_WORD} {}", page_words[index]);
        // A word of its own, so the relative property definition's query
        // is answerable by that field alone: the page's own `jcr:title`
        // carries none of it.
        let content_title = format!("Pagecontentcorn {page}");
        sling_post_fields(
            port,
            &format!("{path}/jcr:content"),
            &[
                ("jcr:primaryType", LUCENE_VARIANT_NODE_TYPE),
                ("jcr:title", &content_title),
                ("variantText", &text),
            ],
        );
        // The child only the second `relativeNode` include names, so what
        // that include writes is attributable to it alone. Its type is
        // **not** the rule's, so the second indexing rule is the one that
        // covers it — which is how the fixture asks whose rule an
        // aggregated node's `excludeFromAggregation` is read from.
        sling_post_fields(
            port,
            &format!("{path}/{VARIANT_RELATIVE_CHILD}"),
            &[
                ("jcr:primaryType", VARIANT_AGGREGATED_NODE_TYPE),
                ("jcr:title", "Metacorn Meta"),
                ("variantText", "Metatextcorn"),
            ],
        );
        // A child of the aggregated node whose own type declares an
        // aggregate: whether its text reaches the page is whether Oak
        // re-aggregates, which `reaggregateLimit` bounds.
        sling_post_fields(
            port,
            &format!("{path}/{VARIANT_RELATIVE_CHILD}/inner"),
            &[
                ("jcr:primaryType", LUCENE_VARIANT_NODE_TYPE),
                ("jcr:title", "Innercorn Inner"),
            ],
        );
        // A child whose own type declares an aggregate and which **no
        // indexing rule covers**: Oak looks a re-aggregation up in the
        // definition's `aggregates` map by the matched node's own type,
        // so the grandchild below reaches the page all the same.
        sling_post_fields(
            port,
            &format!("{path}/{VARIANT_UNRULED_CHILD}"),
            &[
                ("jcr:primaryType", VARIANT_UNRULED_NODE_TYPE),
                ("jcr:title", "Extracorn Extra"),
            ],
        );
        sling_post_fields(
            port,
            &format!("{path}/{VARIANT_UNRULED_CHILD}/leaf"),
            &[
                ("jcr:primaryType", VARIANT_UNRULED_NODE_TYPE),
                ("jcr:title", "Leafcorn Leaf"),
            ],
        );
        // A child of the aggregated node, so that node carries a
        // **hidden** `:childOrder` with a value in it: the relative
        // catch-all pattern below is matched against every property name
        // the node state yields, and whether a hidden one is among them is
        // a question only Oak's own rebuild answers.
        sling_post_fields(
            port,
            &format!("{path}/jcr:content/par"),
            &[
                ("jcr:primaryType", LUCENE_VARIANT_NODE_TYPE),
                ("jcr:title", "Variant Page Paragraph"),
            ],
        );
    }
}

/// Posts the second Lucene definition, **after** the content it indexes
/// exists, so Oak rebuilds it from that content in one lane cycle.
///
/// Every feature here is one the offline rebuild has a branch for and the
/// fixture's own `/oak:index/lucene` does not exercise:
///
/// * `evaluatePathRestrictions`, which is what puts `:ancestors` and
///   `:depth` on every document;
/// * an `ordered` property definition in the **new-format** place, under
///   `indexRules` — the old `orderedProps` list is read only by the
///   old-format rule construction, for definitions without `indexRules`;
/// * `nullCheckEnabled` under a rule whose node type is not `nt:base`,
///   which Oak's own rule validation is the reason for;
/// * a `facets` property, which makes the rebuild persist a `facets`
///   configuration into the visible definition;
/// * an `aggregates` rule, so a page's `jcr:content` text reaches the
///   page's own document;
/// * `analyzed` and `nodeScopeIndex` together, which is what makes Oak
///   select `oakCodec` for the index rather than Lucene's default;
/// * a **multi-valued** string, faceted and ordered at once — the facet
///   configuration's only multi-valued dimension and the ordered branch's
///   skipped one;
/// * `facets` on a **long**, which Oak's own type test writes no facet
///   field for;
/// * a **relative** property definition, the shape AEM's own definitions
///   are written in;
/// * a `relativeNode` aggregate include, which writes `fullnode:<path>`;
/// * `useInSuggest` and `useInSpellcheck`, and a `boost` other than the
///   default.
///
/// The type hints are load-bearing exactly as they are for the property
/// index above: Oak reads `includedPaths` and `queryPaths` as strings and
/// `compatVersion` as a long, and a value that arrived as the wrong type
/// produces a definition that looks right and plans as though it were not
/// there.
pub(crate) fn populate_lucene_variant_definition(port: u16) {
    let root = format!("/oak:index/{LUCENE_VARIANT_DEFINITION}");
    let rule = format!("{root}/indexRules/{LUCENE_VARIANT_NODE_TYPE}");
    let properties = format!("{rule}/properties");

    // The subtree first, flagged last: `reindex` on the definition node is
    // what starts Oak's rebuild, and a rebuild that started before the
    // rules landed would index nothing.
    sling_post_fields(
        port,
        &root,
        &[
            ("jcr:primaryType", "oak:QueryIndexDefinition"),
            ("type", "lucene"),
            // The codec named outright, which is what AEM's own
            // definitions carry and what `Codec.forName` resolves to the
            // composition froe writes.
            ("codec", "oakCodec"),
            ("async", "async"),
            ("compatVersion@TypeHint", "Long"),
            ("compatVersion", "2"),
            ("evaluatePathRestrictions@TypeHint", "Boolean"),
            ("evaluatePathRestrictions", "true"),
            ("includedPaths@TypeHint", "String[]"),
            ("includedPaths", LUCENE_VARIANT_SUBTREE),
            ("queryPaths@TypeHint", "String[]"),
            ("queryPaths", LUCENE_VARIANT_SUBTREE),
            ("excludedPaths@TypeHint", "String[]"),
            ("excludedPaths", VARIANT_EXCLUDED_SUBTREE),
            // The token-count cap, which stops the stream rather than
            // draining it — and so changes the end state every analyzed
            // field of this definition reports.
            ("maxFieldLength@TypeHint", "Long"),
            ("maxFieldLength", "4"),
            // `:suggest` analyzed with the definition's own chain instead
            // of the suggest helper's newline tokenizer, which is the
            // other of the two chains that field can take.
            ("suggestAnalyzed@TypeHint", "Boolean"),
            ("suggestAnalyzed", "true"),
        ],
    );
    // `indexOriginalTerm` is a property **of** the `analyzers` node; a
    // child of it is what froe refuses.
    sling_post_fields(
        port,
        &format!("{root}/analyzers"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("indexOriginalTerm@TypeHint", "Boolean"),
            ("indexOriginalTerm", "true"),
        ],
    );
    sling_post_fields(
        port,
        &format!("{root}/indexRules"),
        &[("jcr:primaryType", "nt:unstructured")],
    );
    sling_post_fields(
        port,
        &rule,
        &[
            ("jcr:primaryType", "nt:unstructured"),
            // The rule-level flag, beside the `:nodeName` property
            // definition the fixture's own `lucene` reaches by pattern.
            ("indexNodeName@TypeHint", "Boolean"),
            ("indexNodeName", "true"),
            // The **rule's** own type list, which gates the fulltext loop
            // and nothing else: the typed fields of the long, the double,
            // the date and the boolean below are written all the same,
            // because the list that gates *them* is each property
            // definition's own and defaults to every type.
            ("includePropertyTypes@TypeHint", "String[]"),
            ("includePropertyTypes", "String"),
        ],
    );
    sling_post_fields(port, &properties, &[("jcr:primaryType", "nt:unstructured")]);

    for (child, fields) in variant_property_definitions() {
        sling_post_fields(port, &format!("{properties}/{child}"), &fields);
    }

    populate_aggregated_node_rule(port, &root);

    sling_post_fields(
        port,
        &format!("{root}/aggregates"),
        &[("jcr:primaryType", "nt:unstructured")],
    );
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{LUCENE_VARIANT_NODE_TYPE}"),
        &[("jcr:primaryType", "nt:unstructured")],
    );
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{LUCENE_VARIANT_NODE_TYPE}/include0"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("path", "jcr:content"),
        ],
    );
    // The same node again through a `relativeNode` include, and a second
    // relative include over a path **no other include names**. The pair
    // separates the two questions a single relative include cannot: what
    // field a relative include writes, and whether a node named by two
    // includes is aggregated once or twice.
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{LUCENE_VARIANT_NODE_TYPE}/include1"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("path", "jcr:content"),
            ("relativeNode@TypeHint", "Boolean"),
            ("relativeNode", "true"),
        ],
    );
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{LUCENE_VARIANT_NODE_TYPE}/include2"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("path", VARIANT_RELATIVE_CHILD),
            ("relativeNode@TypeHint", "Boolean"),
            ("relativeNode", "true"),
        ],
    );
    // A two-step path whose last step is `*`, which is the shape AEM's
    // own `jcr:content/*` includes are written in.
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{LUCENE_VARIANT_NODE_TYPE}/include3"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("path", "jcr:content/*"),
        ],
    );
    // The `primaryType` constraint, which is enforced on the last step
    // alone — once where it holds and once where it does not, so a
    // constraint that is never read and one that always refuses are both
    // visible.
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{LUCENE_VARIANT_NODE_TYPE}/include4"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("path", "jcr:content"),
            ("primaryType", LUCENE_VARIANT_NODE_TYPE),
        ],
    );
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{LUCENE_VARIANT_NODE_TYPE}/include5"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("path", "jcr:content"),
            ("primaryType", "nt:file"),
        ],
    );
    // A plain include over the child whose type no rule covers, and that
    // type's own aggregate beside it.
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{LUCENE_VARIANT_NODE_TYPE}/include6"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("path", VARIANT_UNRULED_CHILD),
        ],
    );
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{VARIANT_UNRULED_NODE_TYPE}"),
        &[("jcr:primaryType", "nt:unstructured")],
    );
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{VARIANT_UNRULED_NODE_TYPE}/include0"),
        &[("jcr:primaryType", "nt:unstructured"), ("path", "leaf")],
    );
    // The aggregate of the **aggregated** node's own type. Whether its
    // include reaches the page is whether Oak re-aggregates.
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{VARIANT_AGGREGATED_NODE_TYPE}"),
        &[("jcr:primaryType", "nt:unstructured")],
    );
    sling_post_fields(
        port,
        &format!("{root}/aggregates/{VARIANT_AGGREGATED_NODE_TYPE}/include0"),
        &[("jcr:primaryType", "nt:unstructured"), ("path", "inner")],
    );

    sling_post_fields(
        port,
        &root,
        &[("reindex@TypeHint", "Boolean"), ("reindex", "true")],
    );
}

/// The second indexing rule, over the node type the page's aggregated
/// `meta` child carries.
///
/// Its one property definition is `excludeFromAggregation`, and the first
/// rule's definition of the same name is not — so whether that child's
/// `jcr:title` reaches the page's `:fulltext` says **whose** rule Oak reads
/// an aggregated property's definition from: the document's own rule, or
/// the rule that covers the aggregated node.
fn populate_aggregated_node_rule(port: u16, root: &str) {
    let rule = format!("{root}/indexRules/{VARIANT_AGGREGATED_NODE_TYPE}");
    sling_post_fields(port, &rule, &[("jcr:primaryType", "nt:unstructured")]);
    sling_post_fields(
        port,
        &format!("{rule}/properties"),
        &[("jcr:primaryType", "nt:unstructured")],
    );
    sling_post_fields(
        port,
        &format!("{rule}/properties/title"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("name", "jcr:title"),
            ("excludeFromAggregation@TypeHint", "Boolean"),
            ("excludeFromAggregation", "true"),
        ],
    );
    // A relative definition on the **second** rule, which is reached at
    // two altitudes: on this node's own document, and — if Oak evaluates
    // a rule's property includes when it re-aggregates — on the document
    // of the page that aggregates it.
    sling_post_fields(
        port,
        &format!("{rule}/properties/innerTitle"),
        &[
            ("jcr:primaryType", "nt:unstructured"),
            ("name", "inner/jcr:title"),
            ("propertyIndex@TypeHint", "Boolean"),
            ("propertyIndex", "true"),
            ("analyzed@TypeHint", "Boolean"),
            ("analyzed", "true"),
        ],
    );
}

/// The variant rule's property definitions, one per branch under test.
///
/// Split at the seam the thousand-line gate's per-function limit found:
/// the analyzed and node-scope definitions first, then the ones written
/// for a declared type.
fn variant_property_definitions() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let mut definitions = variant_analyzed_property_definitions();
    definitions.extend(variant_typed_property_definitions());
    definitions
}

/// The definitions whose fields are analyzed, node-scope indexed or both.
fn variant_analyzed_property_definitions() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)>
{
    vec![
        // Analyzed *and* node-scope: `full:variantText` for a property
        // `CONTAINS`, and `:fulltext` for a node-scope one.
        (
            "text",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "variantText"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("analyzed@TypeHint", "Boolean"),
                ("analyzed", "true"),
                ("nodeScopeIndex@TypeHint", "Boolean"),
                ("nodeScopeIndex", "true"),
                // The two suggestion branches, whose fields — the merged
                // `:suggest` and the shingled `:spellcheck` — no other
                // definition in the fixture produces.
                ("useInSuggest@TypeHint", "Boolean"),
                ("useInSuggest", "true"),
                ("useInSpellcheck@TypeHint", "Boolean"),
                ("useInSpellcheck", "true"),
            ],
        ),
        (
            "title",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "jcr:title"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("analyzed@TypeHint", "Boolean"),
                ("analyzed", "true"),
                ("nodeScopeIndex@TypeHint", "Boolean"),
                ("nodeScopeIndex", "true"),
                // A boost other than the default, which is what a norm
                // records: without one every norm in the fixture is the
                // same byte whatever the boost branch does.
                ("boost@TypeHint", "Double"),
                ("boost", "2.0"),
            ],
        ),
        // The relative property definition, the shape AEM's own
        // definitions are written in: the value lives on a child and the
        // field carries the relative path as its name.
        (
            "contentTitle",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "jcr:content/jcr:title"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("analyzed@TypeHint", "Boolean"),
                ("analyzed", "true"),
                ("ordered@TypeHint", "Boolean"),
                ("ordered", "true"),
            ],
        ),
        // The relative **pattern**, which reaches the same child through
        // its name expression rather than an exact name — the second of
        // the two shapes AEM's definitions use, and the one that can only
        // ever match through the aggregate walk.
        (
            "contentAny",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "jcr:content/.*"),
                ("isRegexp@TypeHint", "Boolean"),
                ("isRegexp", "true"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("analyzed@TypeHint", "Boolean"),
                ("analyzed", "true"),
            ],
        ),
        // A definition of the **document's** rule naming an aggregated
        // node's property by its relative path, and excluding it from
        // aggregation. Which rule Oak reads that flag from — this one, or
        // the rule covering the aggregated node — is what this asks; it
        // contributes no field of its own, indexing nothing.
        (
            "metaTextExcluded",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "meta/variantText"),
                ("excludeFromAggregation@TypeHint", "Boolean"),
                ("excludeFromAggregation", "true"),
            ],
        ),
        // A relative definition whose ancestor step is `*`, which the
        // include matcher treats as every child — the one element form
        // neither of the two above reaches.
        (
            "anyChildTitle",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "*/jcr:title"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
            ],
        ),
        // The multi-valued string: an array of labels, faceted, analyzed
        // and node-scope indexed at once.
        (
            "tags",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "variantTags"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("analyzed@TypeHint", "Boolean"),
                ("analyzed", "true"),
                ("nodeScopeIndex@TypeHint", "Boolean"),
                ("nodeScopeIndex", "true"),
                ("facets@TypeHint", "Boolean"),
                ("facets", "true"),
                // Ordered too, which a multi-valued property is skipped
                // for: the branch that writes no doc value at all.
                ("ordered@TypeHint", "Boolean"),
                ("ordered", "true"),
            ],
        ),
    ]
}

/// The definitions written for a declared type, and the two the markers
/// and the facet branches are about.
fn variant_typed_property_definitions() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    vec![
        // The ordered doc value, with the rule's own declared type — which
        // is the type the doc value is written under, not the property's.
        (
            "rank",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "variantRank"),
                ("type", "Long"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("ordered@TypeHint", "Boolean"),
                ("ordered", "true"),
                // Faceted on a **non-string** property, which Oak's own
                // facet branch tests the type tag for: the configuration
                // is consulted and no facet field is added.
                ("facets@TypeHint", "Boolean"),
                ("facets", "true"),
            ],
        ),
        (
            "score",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "variantScore"),
                ("type", "Double"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("ordered@TypeHint", "Boolean"),
                ("ordered", "true"),
            ],
        ),
        (
            "date",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "variantDate"),
                ("type", "Date"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("ordered@TypeHint", "Boolean"),
                ("ordered", "true"),
            ],
        ),
        (
            "flag",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "variantFlag"),
                ("type", "Boolean"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                // A **sorted** doc value, where every other ordered
                // property in the fixture writes a numeric one.
                ("ordered@TypeHint", "Boolean"),
                ("ordered", "true"),
            ],
        ),
        // The one the `IS NULL` query is about. Its rule's node type is
        // `nt:unstructured` precisely because of it.
        (
            "optional",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "variantOptional"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("nullCheckEnabled@TypeHint", "Boolean"),
                ("nullCheckEnabled", "true"),
                ("notNullCheckEnabled@TypeHint", "Boolean"),
                ("notNullCheckEnabled", "true"),
            ],
        ),
        (
            "category",
            vec![
                ("jcr:primaryType", "nt:unstructured"),
                ("name", "variantCategory"),
                ("propertyIndex@TypeHint", "Boolean"),
                ("propertyIndex", "true"),
                ("facets@TypeHint", "Boolean"),
                ("facets", "true"),
                // The value pattern's prefix form, which gates the typed
                // field, the doc value and the facet alike.
                ("valueExcludedPrefixes@TypeHint", "String[]"),
                ("valueExcludedPrefixes", VARIANT_EXCLUDED_CATEGORY),
                // A string sorted doc value beside the numeric ones.
                ("ordered@TypeHint", "Boolean"),
                ("ordered", "true"),
            ],
        ),
    ]
}
