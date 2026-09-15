//! The `lucene_reindex` phase's live oracle: Oak answering queries from
//! froe's own Lucene index, compared against Oak answering them from its
//! own rebuild of the same store.
//!
//! An enumeration comparison says the two indexes have the same contents.
//! It does not say Oak's query planner *offers* the index, chooses it, and
//! reads it the same way — a shape error subtle enough to survive an
//! enumeration would show up here and nowhere else. So every statement goes
//! through Oak's own query engine on both stores, and every statement's
//! `EXPLAIN` is compared too.
//!
//! Every statement meant for the variant definition is restricted at or
//! below its `queryPaths`, because Oak's own fulltext planner offers an
//! index carrying them only to a query restricted that way — an unrestricted
//! query would silently be answered by the fixture's repository-wide
//! `lucene`, and the comparison would then prove nothing about the
//! definition it was written for.

use super::*;

/// One statement, and what about its answer is a claim.
pub(crate) struct QuerySample {
    /// The JCR-SQL2 statement.
    pub(crate) statement: &'static str,
    /// The column to print instead of each row's path, for a facet query
    /// whose rows name no node.
    pub(crate) column: Option<&'static str>,
    /// Whether the row **order** is part of the answer.
    ///
    /// Only an `ORDER BY` fixes it. For every other statement the order is
    /// Lucene's own document order, which the two indexes do not share —
    /// Oak's editor walks a node's children in map order and froe's rebuild
    /// walks them sorted — so comparing it would compare the walk.
    pub(crate) ordered: bool,
    /// The index the plan must name, which is the other half of the claim:
    /// equal rows from a traversal would be equal rows proving nothing.
    pub(crate) index: &'static str,
}

/// The statements, one per branch the variant definition was posted for.
pub(crate) const QUERY_SAMPLES: [QuerySample; 10] = [
    // Node-scope fulltext, which is `:fulltext` and — through the
    // aggregate — a page's `jcr:content` text on the page's own document.
    QuerySample {
        statement: "SELECT * FROM [nt:unstructured] WHERE \
                    ISDESCENDANTNODE('/content/interop/variant') AND CONTAINS(*, 'vandrelith')",
        column: None,
        ordered: false,
        index: "lucene:interopLucene",
    },
    // Property fulltext, which is `full:variantText`.
    QuerySample {
        statement: "SELECT * FROM [nt:unstructured] WHERE \
                    ISDESCENDANTNODE('/content/interop/variant') AND \
                    CONTAINS([variantText], 'alphacorn')",
        column: None,
        ordered: false,
        index: "lucene:interopLucene",
    },
    // The ordered doc value, read as a sort field rather than as a term.
    QuerySample {
        statement: "SELECT * FROM [nt:unstructured] WHERE \
                    ISDESCENDANTNODE('/content/interop/variant') AND [variantRank] IS NOT NULL \
                    ORDER BY [variantRank] DESC",
        column: None,
        ordered: true,
        index: "lucene:interopLucene",
    },
    // `nullCheckEnabled`, which is the `:nullProps` marker term.
    QuerySample {
        statement: "SELECT * FROM [nt:unstructured] WHERE \
                    ISDESCENDANTNODE('/content/interop/variant') AND [variantOptional] IS NULL",
        column: None,
        ordered: false,
        index: "lucene:interopLucene",
    },
    // `:ancestors`, which `evaluatePathRestrictions` is what writes.
    QuerySample {
        statement: "SELECT * FROM [nt:unstructured] WHERE \
                    ISDESCENDANTNODE('/content/interop/variant/pages')",
        column: None,
        ordered: false,
        index: "lucene:interopLucene",
    },
    // A path restriction one level deep, beside a property term.
    QuerySample {
        statement: "SELECT * FROM [nt:unstructured] WHERE \
                    ISCHILDNODE('/content/interop/variant/items') AND [variantCategory] = 'alpha'",
        column: None,
        ordered: false,
        index: "lucene:interopLucene",
    },
    // The facet, read through its own column.
    QuerySample {
        statement: "SELECT [rep:facet(variantCategory)] FROM [nt:unstructured] WHERE \
                    ISDESCENDANTNODE('/content/interop/variant') AND CONTAINS(*, 'vandrelith')",
        column: Some("rep:facet(variantCategory)"),
        ordered: false,
        index: "lucene:interopLucene",
    },
    // The relative property definition, whose field carries the relative
    // path as its name and whose value lives on a child node.
    QuerySample {
        statement: "SELECT * FROM [nt:unstructured] WHERE \
                    ISDESCENDANTNODE('/content/interop/variant') AND \
                    CONTAINS([jcr:content/jcr:title], 'pagecontentcorn')",
        column: None,
        ordered: false,
        index: "lucene:interopLucene",
    },
    // The multi-valued facet dimension, which is the one the persisted
    // configuration records as `multivalued`.
    QuerySample {
        statement: "SELECT [rep:facet(variantTags)] FROM [nt:unstructured] WHERE \
                    ISDESCENDANTNODE('/content/interop/variant') AND CONTAINS(*, 'vandrelith')",
        column: Some("rep:facet(variantTags)"),
        ordered: false,
        index: "lucene:interopLucene",
    },
    // The repository-wide definition, which is the one the binary is on.
    QuerySample {
        statement: "SELECT * FROM [nt:base] WHERE CONTAINS(*, 'throwaway')",
        column: None,
        ordered: false,
        index: "lucene:lucene",
    },
];

/// What one booted store answered.
pub(crate) struct QueryAnswers {
    pub(crate) results: Vec<Vec<String>>,
    pub(crate) plans: Vec<Vec<String>>,
}

/// Every sample's rows and plan, from a Sling that is already up.
pub(crate) fn collect_query_answers(port: u16) -> QueryAnswers {
    assert_eq!(
        sling::sling_query(port, "SELECT * FROM [rep:root]"),
        vec!["/".to_owned()],
        "the query probe does not answer; every comparison built on it would be vacuous"
    );
    let results = QUERY_SAMPLES
        .iter()
        .map(|sample| {
            let mut rows = match sample.column {
                Some(column) => sling::sling_query_column(port, sample.statement, column),
                None => sling::sling_query(port, sample.statement),
            };
            if !sample.ordered {
                rows.sort();
            }
            rows
        })
        .collect();
    let plans = QUERY_SAMPLES
        .iter()
        .map(|sample| sling::sling_query(port, &format!("EXPLAIN {}", sample.statement)))
        .collect();
    QueryAnswers { results, plans }
}

/// Where the oracle's answers are kept between the two halves of the phase.
fn oracle_answers_path() -> PathBuf {
    work_root().join("lucene-reindex-oracle-answers.txt")
}

/// Writes the oracle's answers, one section per sample.
pub(crate) fn write_oracle_answers(answers: &QueryAnswers) {
    let mut rendered = String::new();
    for rows in &answers.results {
        rendered.push_str("=== rows\n");
        rendered.push_str(&rows.join("\n"));
        rendered.push('\n');
    }
    for plan in &answers.plans {
        rendered.push_str("=== plan\n");
        rendered.push_str(&plan.join("\n"));
        rendered.push('\n');
    }
    std::fs::write(oracle_answers_path(), rendered).expect("record the oracle's answers");
}

/// Reads them back.
fn read_oracle_answers() -> QueryAnswers {
    let rendered =
        std::fs::read_to_string(oracle_answers_path()).expect("the oracle recorded its answers");
    let mut results = Vec::new();
    let mut plans = Vec::new();
    for section in rendered.split("=== ").skip(1) {
        let (kind, body) = section.split_once('\n').expect("a section has a body");
        let lines: Vec<String> = body
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if kind == "rows" {
            results.push(lines);
        } else {
            plans.push(lines);
        }
    }
    QueryAnswers { results, plans }
}

/// Oak boots on froe's store, accepts the indexes as written, and answers
/// every sample with the same rows and the same plan it answers from its
/// own rebuild.
pub(crate) fn assert_oak_answers_queries_from_froes_index(
    froe_store: &Path,
    port: u16,
    extract_to: &Path,
) {
    let from_oak = read_oracle_answers();
    let from_froe = query_a_booted_store_then(
        froe_store,
        "froe-lucene-reindex-froe",
        port,
        AfterQuerying::ProvokeALaneCycleAndExtract(extract_to),
    );
    assert_answers_agree(&from_oak, &from_froe);
}

/// The dictionary froe removes and never builds, which Oak's own suggester
/// is left to rebuild — `docs/index.md` §5.6.
const SUGGESTER_NODE: &str = ":suggest-data";

/// The word the lane-cycle provocation writes, which nothing else holds.
const LANE_CYCLE_WORD: &str = "suggestercorn";

/// Oak's suggester rebuilds the dictionary froe removed, on the first
/// cycle of the lane that writes the index.
///
/// The claim is froe's, not Oak's: the rebuild removes `:suggest-data`
/// because the dictionary is Lucene's own suggester artifact rather than
/// an index froe writes, and what makes that safe rather than lossy is
/// that Oak builds it again. `DEFAULT_SUGGESTER_UPDATE_FREQUENCY_MINUTES`
/// is ten in the pinned build, and the gate it feeds reads the timestamp
/// of a node that is no longer there — so the first cycle is when it
/// happens, and this is where that is checked rather than assumed.
pub(crate) fn assert_oak_rebuilt_the_suggester(after_boot: &Path, definition: &str) {
    let node = format!("/oak:index/{definition}/{SUGGESTER_NODE}");
    let rendered = digest_store(after_boot);
    assert!(
        rendered.lines().any(|line| line.starts_with(&node)),
        "{node} is still absent after Oak ran a cycle of the lane, so removing it in the          rebuild loses the suggester rather than handing it back to Oak"
    );
    eprintln!("    Oak's own suggester rebuilt {node} on the first cycle");
}

/// The comparison itself, over two collected sets of answers.
pub(crate) fn assert_answers_agree(from_oak: &QueryAnswers, from_froe: &QueryAnswers) {
    for (sample, (oak_rows, froe_rows)) in QUERY_SAMPLES
        .iter()
        .zip(from_oak.results.iter().zip(from_froe.results.iter()))
    {
        assert_eq!(
            without_instance_scoped_rows(froe_rows),
            without_instance_scoped_rows(oak_rows),
            "{}: Oak answered differently from froe's index than from its own",
            sample.statement
        );
        assert!(
            !without_instance_scoped_rows(oak_rows).is_empty(),
            "{}: the statement returns nothing on either store, so comparing the two \
             proves nothing",
            sample.statement
        );
    }
    for (sample, (oak_plan, froe_plan)) in QUERY_SAMPLES
        .iter()
        .zip(from_oak.plans.iter().zip(from_froe.plans.iter()))
    {
        assert_eq!(
            without_estimates(froe_plan),
            without_estimates(oak_plan),
            "{}: Oak chose a different plan over froe's index than over its own",
            sample.statement
        );
        let rendered = oak_plan.join("\n");
        assert!(
            rendered.contains(sample.index),
            "{}: Oak's plan does not name {}, so the rows above say nothing about that \
             index:\n{rendered}",
            sample.statement,
            sample.index
        );
    }
    eprintln!(
        "  {} query pairs equal, every plan naming the same index",
        QUERY_SAMPLES.len()
    );
}

/// What a boot does once its answers are collected.
#[derive(Clone, Copy)]
pub(crate) enum AfterQuerying<'a> {
    /// Nothing: the container is dropped where it stands.
    Stop,
    /// Content the index covers is written and waited for, so the lane
    /// runs a cycle over it, and the store is then extracted to the path —
    /// which is how the phase sees what Oak wrote **back**.
    ProvokeALaneCycleAndExtract(&'a Path),
}

/// Boots Sling on `store` and runs every sample through the query probe.
pub(crate) fn query_a_booted_store(store: &Path, container: &str, port: u16) -> QueryAnswers {
    query_a_booted_store_then(store, container, port, AfterQuerying::Stop)
}

/// The same, with something to do before the container goes.
pub(crate) fn query_a_booted_store_then(
    store: &Path,
    container: &str,
    port: u16,
    after: AfterQuerying<'_>,
) -> QueryAnswers {
    let volume = PodmanVolume::new(&format!("{container}-volume"));
    let bootstrap =
        PodmanContainer::run_detached(&format!("{container}-bootstrap"), port, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(store, &volume.name);

    let sling = PodmanContainer::run_detached(container, port, &volume.name);
    wait_for_sling(port, container);
    assert_oak_consumed_store_as_written(container, "lucene_reindex");
    // Oak must accept froe's index rather than rebuild it; otherwise every
    // row below is Oak's own rebuild answering itself.
    assert_oak_did_not_reindex(container, "lucene_reindex");
    assert_oak_reported_no_index_failure(container, "lucene_reindex");

    let answers = collect_query_answers(port);
    if let AfterQuerying::ProvokeALaneCycleAndExtract(extract_to) = after {
        provoke_a_lane_cycle(port);
        sling.stop();
        store_from_volume(&volume.name, extract_to);
    }
    drop(sling);
    answers
}

/// Writes one node the variant definition indexes and waits until that
/// definition answers for it, which is the lane having run a cycle that
/// wrote the index — and closed the writer, which is where Oak decides
/// about the suggester.
fn provoke_a_lane_cycle(port: u16) {
    let path = format!("{LUCENE_VARIANT_SUBTREE}/items/cycle");
    sling::sling_post_fields(
        port,
        &path,
        &[
            ("jcr:primaryType", LUCENE_VARIANT_NODE_TYPE),
            ("variantText", LANE_CYCLE_WORD),
        ],
    );
    let statement = format!(
        "SELECT * FROM [nt:unstructured] WHERE ISDESCENDANTNODE('{LUCENE_VARIANT_SUBTREE}')          AND CONTAINS([variantText], '{LANE_CYCLE_WORD}')"
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if sling::sling_query(port, &statement).contains(&path) {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    panic!(
        "the async lane never indexed {path} within 120s, so no cycle ran and what Oak's          suggester does after one is untested"
    );
}
