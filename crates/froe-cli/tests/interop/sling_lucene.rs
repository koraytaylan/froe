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
        let text = format!("{VARIANT_SHARED_WORD} {}", words[index]);
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
        sling_post_fields(port, &path, &fields);
    }

    sling_post(
        port,
        &format!("{LUCENE_VARIANT_SUBTREE}/pages"),
        "sling:Folder",
        "Variant Pages",
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
        let content_title = format!("Variant Page Content {page}");
        sling_post_fields(
            port,
            &format!("{path}/jcr:content"),
            &[
                ("jcr:primaryType", LUCENE_VARIANT_NODE_TYPE),
                ("jcr:title", &content_title),
                ("variantText", &text),
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
///   select `oakCodec` for the index rather than Lucene's default.
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
            ("async", "async"),
            ("compatVersion@TypeHint", "Long"),
            ("compatVersion", "2"),
            ("evaluatePathRestrictions@TypeHint", "Boolean"),
            ("evaluatePathRestrictions", "true"),
            ("includedPaths@TypeHint", "String[]"),
            ("includedPaths", LUCENE_VARIANT_SUBTREE),
            ("queryPaths@TypeHint", "String[]"),
            ("queryPaths", LUCENE_VARIANT_SUBTREE),
        ],
    );
    sling_post_fields(
        port,
        &format!("{root}/indexRules"),
        &[("jcr:primaryType", "nt:unstructured")],
    );
    sling_post_fields(port, &rule, &[("jcr:primaryType", "nt:unstructured")]);
    sling_post_fields(port, &properties, &[("jcr:primaryType", "nt:unstructured")]);

    for (child, fields) in variant_property_definitions() {
        sling_post_fields(port, &format!("{properties}/{child}"), &fields);
    }

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

    sling_post_fields(
        port,
        &root,
        &[("reindex@TypeHint", "Boolean"), ("reindex", "true")],
    );
}

/// The variant rule's property definitions, one per branch under test.
fn variant_property_definitions() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
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
            ],
        ),
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
            ],
        ),
    ]
}
