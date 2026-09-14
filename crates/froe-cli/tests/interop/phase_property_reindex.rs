//! The `property_reindex` phase: froe's offline rebuild against Oak's own.
//!
//! This is the strongest oracle available for a reindex. Not "the index
//! froe wrote reads back", which a self-consistent mistake satisfies, but
//! "Oak rebuilt these same definitions over these same bytes, and the two
//! stores render identically".
//!
//! Both rebuilds must therefore run over the *very same store*. A booted
//! Sling writes content of its own before any request arrives — discovery,
//! job and distribution nodes under `/var`, each keyed by that instance's
//! fresh identifier — so a rebuild on an un-booted copy could never match a
//! rebuild on a booted one. The phase boots Sling on a copy, has Oak
//! rebuild, extracts *that* store, and gives froe a copy of it.

use super::*;

/// The counter definition's name, kept as a constant because the
/// canonicality check is specifically about its `:cnt` values.
const COUNTER_DEFINITION: &str = "counter";

/// Every definition in the fixture that froe rebuilds, discovered rather
/// than listed.
///
/// A hardcoded list would quietly stop covering a definition the fixture
/// gained, and would be wrong the first time Sling's own defaults changed.
/// The predicate is froe's own: the modelled type is one of the three this
/// plan rebuilds, and the definition is a direct child of `/oak:index`.
fn rebuildable_definitions(store: &Path) -> Vec<String> {
    let repository = froe::Repository::open(store).expect("open the fixture");
    let oak_index = repository
        .node_at_path("/oak:index")
        .expect("resolve /oak:index")
        .expect("the fixture has an /oak:index");
    let mut names: Vec<String> = oak_index
        .child_node_entries()
        .expect("read /oak:index")
        .into_iter()
        .filter(|(name, _)| !name.starts_with(':'))
        .filter(|(name, node)| {
            let path = format!("/oak:index/{name}");
            froe::index::IndexDefinition::read(node, &path).is_ok_and(|definition| {
                matches!(
                    definition.index_type,
                    Some(
                        froe::index::IndexType::Property
                            | froe::index::IndexType::Reference
                            | froe::index::IndexType::Counter
                    )
                )
            })
        })
        .map(|(name, _)| name)
        .collect();
    names.sort();
    assert!(
        names.len() >= 3,
        "the fixture should carry several rebuildable definitions, found {names:?}"
    );
    names
}

/// How many times the phase will re-run Oak's rebuild to get a canonical
/// counter before failing.
const CANONICAL_ATTEMPTS: usize = 3;

/// The approximate counters Oak seeds from a random number. Two rebuilds of
/// the same content disagree on them by construction, which is a recorded
/// deviation rather than a difference this phase can assert away.
const RANDOMIZED_PROPERTY_PREFIX: &str = ":count_";

/// Phase: froe's reindex against Oak's own reindex of the same store.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn property_reindex() {
    let work = work_root().join("property-reindex");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create the phase's work directory");

    let definitions = rebuildable_definitions(&oak_store());
    eprintln!(
        "  rebuilding {}: {}",
        definitions.len(),
        definitions.join(", ")
    );

    let (oak_rebuilt, attempt) = oak_rebuild_with_a_canonical_counter(&work, &definitions);
    record_canonical_verdict(attempt);

    // froe rebuilds a copy of the store Oak rebuilt, from the same
    // bookkeeping Oak started from.
    let froe_store = work.join("froe");
    copy_store(&oak_rebuilt, &froe_store);
    let counts = extracted_reindex_counts(&oak_rebuilt, &definitions);
    let resets: Vec<definition_edits::BookkeepingReset<'_>> = counts
        .iter()
        .map(|(name, count)| definition_edits::BookkeepingReset {
            name,
            // One below Oak's value, so froe's single increment lands back
            // on exactly it whatever the attempt count was.
            reindex_count: count - 1,
        })
        .collect();
    definition_edits::reset_reindex_bookkeeping(&froe_store, &resets);

    let before = digest_store(&froe_store);
    eprintln!("  froe index reindex --yes");
    let summary = froe(&[
        "index",
        "reindex",
        froe_store.to_str().unwrap(),
        "--yes",
        "--work-directory",
        work.to_str().unwrap(),
    ]);
    eprintln!("{summary}");

    assert_every_definition_renders_identically(&oak_rebuilt, &froe_store, &definitions);
    assert_digest_delta(
        &before,
        &digest_store(&froe_store),
        ExpectedDigestDelta::Subtrees(&["/oak:index"]),
        "property_reindex",
    );
    assert_check_passes(&froe_store);
    assert_oak_answers_queries_from_froes_index(&froe_store);
    assert_a_rerun_is_identical(&froe_store, &work, &definitions);
    assert_the_counter_reset_lets_oak_rebuild_from_scratch(&work, &oak_rebuilt);

    eprintln!("  property_reindex phase passed");
}

/// The statements the phase puts through Oak's own query engine.
///
/// One per storage strategy, because each is answered by different code:
/// the unique strategy reads `entry`, the mirror strategy walks the trie,
/// and the reference index is keyed by identifier.
const QUERY_SAMPLES: [&str; 3] = [
    "SELECT * FROM [nt:base] WHERE [jcr:primaryType] = 'sling:Folder'",
    "SELECT * FROM [nt:base] WHERE ISDESCENDANTNODE('/content/interop')",
    "SELECT * FROM [nt:base] WHERE [sling:resourceType] IS NOT NULL",
];

/// Oak boots on froe's store, accepts the indexes as written, and answers
/// the same queries with the same rows it answers from its own rebuild.
///
/// This is the claim a byte comparison cannot make. Two renderings can
/// agree while the index is unusable — a key encoded so the query engine
/// never looks it up would render identically on both sides only if froe
/// and Oak made the same mistake, which they cannot, but a subtler shape
/// error might. Putting statements through the live engine on both stores
/// closes that.
fn assert_oak_answers_queries_from_froes_index(froe_store: &Path) {
    let from_oak = read_oracle_answers();
    let from_froe = query_a_booted_store(froe_store, "froe-reindex-froe", 8092);

    for (statement, (oak_rows, froe_rows)) in QUERY_SAMPLES
        .iter()
        .zip(from_oak.results.iter().zip(from_froe.results.iter()))
    {
        assert_eq!(
            froe_rows, oak_rows,
            "{statement}: Oak answered differently from froe's index than from its own"
        );
    }
    // Only the plans with no counter-derived number: a mirror-strategy
    // plan prints an `estimatedCost:` the strategy derives from the
    // randomized `:count_*` counters froe omits by recorded deviation, and
    // index selection between competing mirror indexes can differ for the
    // same reason.
    for (statement, (oak_plan, froe_plan)) in DETERMINISTIC_PLAN_SAMPLES
        .iter()
        .zip(from_oak.plans.iter().zip(from_froe.plans.iter()))
    {
        assert_eq!(
            froe_plan, oak_plan,
            "{statement}: Oak chose a different plan over froe's index than over its own"
        );
    }
}

/// The statements whose `EXPLAIN` text carries no counter-derived number.
const DETERMINISTIC_PLAN_SAMPLES: [&str; 1] =
    ["SELECT * FROM [nt:base] WHERE [jcr:uuid] IS NOT NULL"];

/// What one booted store answered.
struct QueryAnswers {
    results: Vec<Vec<String>>,
    plans: Vec<Vec<String>>,
}

/// Boots Sling on `store` and runs every sample through the query probe.
fn query_a_booted_store(store: &Path, container: &str, port: u16) -> QueryAnswers {
    let volume = PodmanVolume::new(&format!("{container}-volume"));
    let bootstrap =
        PodmanContainer::run_detached(&format!("{container}-bootstrap"), port, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(store, &volume.name);

    let sling = PodmanContainer::run_detached(container, port, &volume.name);
    wait_for_sling(port, container);
    assert_oak_consumed_store_as_written(container, "property_reindex");
    // Oak must accept froe's index rather than rebuild it; otherwise every
    // row below is Oak's own rebuild answering itself.
    assert_oak_did_not_reindex(container, "property_reindex");

    // The probe is already in the store — installed before Oak rebuilt —
    // so this step only reads.
    let answers = collect_query_answers(port);
    drop(sling);
    answers
}

/// The reset scenario: froe removes the counter's data, Oak rebuilds it.
///
/// The counter is the one definition froe deliberately does *not* rebuild
/// when its lane cannot be resolved, because Oak's replay after a lost
/// checkpoint doubles a rebuilt counter whether or not froe ran. The claim
/// is therefore about Oak, and only Oak can make it: after froe's reset,
/// Oak's own cycle logs the lost checkpoint, advances `reindexCount` by
/// exactly one, and rebuilds the counter from scratch.
fn assert_the_counter_reset_lets_oak_rebuild_from_scratch(work: &Path, oak_rebuilt: &Path) {
    let store = work.join("reset");
    copy_store(oak_rebuilt, &store);

    let lane_checkpoint = counter_lane_checkpoint(&store);
    let Some(checkpoint) = lane_checkpoint else {
        eprintln!("  the counter's lane names no checkpoint; the reset scenario needs one");
        return;
    };
    eprintln!("  removing the counter's lane checkpoint {checkpoint}");
    froe(&[
        "checkpoint",
        "remove",
        store.to_str().unwrap(),
        &checkpoint,
        "--yes",
    ]);

    let visible_before = visible_definition_properties(&store, COUNTER_DEFINITION);
    let count_before = counter_reindex_count(&store);
    eprintln!("  froe index reindex --yes --from-head --index /oak:index/counter");
    let summary = froe(&[
        "index",
        "reindex",
        store.to_str().unwrap(),
        "--yes",
        "--from-head",
        "--index",
        &format!("/oak:index/{COUNTER_DEFINITION}"),
        "--work-directory",
        work.to_str().unwrap(),
    ]);
    eprintln!("{summary}");
    assert!(
        summary.contains("reset"),
        "the counter should have been reset rather than rebuilt: {summary}"
    );

    let hidden: Vec<String> = hidden_children_rendering(&store, &[COUNTER_DEFINITION.to_owned()])
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(
        hidden.is_empty(),
        "the reset left hidden children behind: {hidden:?}"
    );
    assert_eq!(
        visible_definition_properties(&store, COUNTER_DEFINITION),
        visible_before,
        "the reset changed a visible property, which it must never do"
    );

    eprintln!("  booting Sling so Oak's own lane rebuilds the counter");
    let volume = PodmanVolume::new("froe-reindex-reset-volume");
    let bootstrap =
        PodmanContainer::run_detached("froe-reindex-reset-bootstrap", 8093, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-reindex-reset", 8093, &volume.name);
    wait_for_sling(8093, "froe-reindex-reset");

    let path = format!("/oak:index/{COUNTER_DEFINITION}");
    let after = sling::sling_wait_until_reindexed(8093, &path, count_before - 1);
    assert_eq!(
        after,
        count_before + 1,
        "Oak's replay should advance reindexCount by exactly one"
    );
    let logs = container_logs("froe-reindex-reset");
    assert!(
        logs.contains("Failed to retrieve previously indexed checkpoint"),
        "Oak should have logged the lost checkpoint that makes this a from-scratch \
         rebuild rather than an incremental one"
    );
    drop(sling);

    let extracted = work.join("reset-extracted");
    store_from_volume(&volume.name, &extracted);
    let rebuilt = non_canonical_counter_nodes(&extracted);
    assert!(
        rebuilt.is_empty(),
        "Oak's from-scratch counter is not canonical: {rebuilt:?}"
    );
}

/// The checkpoint the counter's lane names, if any.
fn counter_lane_checkpoint(store: &Path) -> Option<String> {
    let repository = froe::Repository::open(store).expect("open the store");
    let root = repository.content_root().expect("content root");
    let lanes = froe::index::lanes::AsyncLanes::read(&root).expect("read /:async");
    let path = format!("/oak:index/{COUNTER_DEFINITION}");
    let node = repository.node_at_path(&path).ok()??;
    let definition = froe::index::IndexDefinition::read(&node, &path).ok()?;
    let lane = definition.lane.as_deref()?;
    lanes.lane(lane).and_then(|lane| lane.checkpoint.clone())
}

/// The counter definition's visible properties, rendered.
fn visible_definition_properties(store: &Path, name: &str) -> String {
    let whole = digest_store_excluding_properties(store, &[], &[RANDOMIZED_PROPERTY_PREFIX]);
    let path = format!("/oak:index/{name}");
    whole
        .lines()
        .find(|line| line.starts_with(&format!("{path}\t")) || *line == path)
        .unwrap_or("")
        .to_owned()
}

/// The counter definition's `reindexCount`.
fn counter_reindex_count(store: &Path) -> i64 {
    let repository = froe::Repository::open(store).expect("open the store");
    let path = format!("/oak:index/{COUNTER_DEFINITION}");
    let node = repository
        .node_at_path(&path)
        .expect("resolve the counter")
        .expect("the counter is in the store");
    froe::index::IndexDefinition::read(&node, &path)
        .expect("model the counter")
        .reindex
        .count
}

/// Boots Sling on a copy, flags every definition, waits for Oak to finish,
/// stops, extracts — retrying while the extracted counter is not canonical.
///
/// A lane cycle running between Oak's rebuild and the stop maintains the
/// counter incrementally, and Oak's counter editor removes a `:cnt` that
/// reached zero without removing the node it sat on. A deletion in that
/// window therefore leaves a `:cnt`-less mirror node no rebuild produces,
/// and comparing against it would fail for a reason that is not froe's.
/// Plan 0006's reader reports those nodes, so the phase can tell the
/// difference and retry rather than compare — or fail naming the condition,
/// which it must never skip past.
fn oak_rebuild_with_a_canonical_counter(work: &Path, definitions: &[String]) -> (PathBuf, usize) {
    for attempt in 1..=CANONICAL_ATTEMPTS {
        eprintln!("  Oak rebuild attempt {attempt} of {CANONICAL_ATTEMPTS}");
        let extracted = work.join(format!("oak-{attempt}"));
        let _ = std::fs::remove_dir_all(&extracted);
        oak_rebuild_once(&extracted, definitions);
        match non_canonical_counter_nodes(&extracted) {
            nodes if nodes.is_empty() => return (extracted, attempt),
            nodes => eprintln!(
                "  the extracted counter is not canonical ({} node(s) without a :cnt, \
                 e.g. {}); repeating",
                nodes.len(),
                nodes.first().map_or("", String::as_str),
            ),
        }
    }
    panic!(
        "Oak's counter index was not canonical after {CANONICAL_ATTEMPTS} attempts: a lane \
         cycle between the rebuild and the stop left mirror nodes without a :cnt, which no \
         rebuild produces. The comparison is skipped rather than weakened — investigate \
         rather than raising the attempt count."
    );
}

/// One boot-flag-wait-stop-extract cycle.
fn oak_rebuild_once(extracted: &Path, definitions: &[String]) {
    let source = oak_store();
    let volume = PodmanVolume::new("froe-interop-reindex");
    let bootstrap = PodmanContainer::run_detached("froe-reindex-bootstrap", 8091, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&source, &volume.name);

    let sling = PodmanContainer::run_detached("froe-reindex-oak", 8091, &volume.name);
    wait_for_sling(8091, "froe-reindex-oak");

    // The query probe goes in *before* anything is flagged, so it becomes
    // part of the store both sides share: Oak's rebuild indexes it and
    // froe's rebuild indexes it, symmetrically. Installing it afterwards
    // would mean writing to a store whose indexes are the thing under
    // comparison — and a write to a freshly reindexed store is its own
    // experiment, not this one.
    eprintln!("  installing the query probe before flagging anything");
    sling::sling_install_query_probe(8091);

    let mut before: Vec<(String, i64)> = Vec::new();
    for name in definitions {
        let path = format!("/oak:index/{name}");
        let rendered = sling_get_json(8091, &path);
        before.push((
            name.clone(),
            sling::json_number(&rendered, "reindexCount").unwrap_or(0),
        ));
        eprintln!("  flagging {path} for reindex");
        sling::sling_set_property(
            8091,
            &path,
            &sling::SlingProperty {
                name: "reindex",
                value: "true",
                type_hint: Some("Boolean"),
            },
        );
    }
    for (name, count) in &before {
        let path = format!("/oak:index/{name}");
        eprintln!("  waiting for Oak to finish rebuilding {path}");
        let after = sling::sling_wait_until_reindexed(8091, &path, *count);
        eprintln!("    reindexCount {count} -> {after}");
    }

    // The oracle's answers are collected here, from the session that just
    // rebuilt: booting a second Sling on the extracted copy would prove the
    // same thing at the cost of another full boot, and this host's memory
    // watchdog has killed the phase at exactly that point three times.
    eprintln!("  collecting the oracle's query answers before stopping Sling");
    let answers = collect_query_answers(8091);
    write_oracle_answers(&answers);

    drop(sling);
    store_from_volume(&volume.name, extracted);
}

/// Every sample's rows and every deterministic sample's plan, from a Sling
/// that is already up.
fn collect_query_answers(port: u16) -> QueryAnswers {
    assert_eq!(
        sling::sling_query(port, "SELECT * FROM [rep:root]"),
        vec!["/".to_owned()],
        "the query probe does not answer; every comparison built on it would be vacuous"
    );
    let results = QUERY_SAMPLES
        .iter()
        .map(|statement| {
            let mut rows = sling::sling_query(port, statement);
            rows.sort();
            rows
        })
        .collect();
    let plans = DETERMINISTIC_PLAN_SAMPLES
        .iter()
        .map(|statement| sling::sling_query(port, &format!("EXPLAIN {statement}")))
        .collect();
    QueryAnswers { results, plans }
}

/// Where the oracle's answers are kept between the two halves of the phase.
fn oracle_answers_path() -> PathBuf {
    work_root().join("property-reindex-oracle-answers.txt")
}

/// Writes the oracle's answers, one section per sample.
fn write_oracle_answers(answers: &QueryAnswers) {
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

/// The counter's mirror nodes that carry no count at all.
///
/// Read through plan 0006's counter reader, whose `count: None` is exactly
/// this condition: a node Oak's editor emptied without removing.
fn non_canonical_counter_nodes(store: &Path) -> Vec<String> {
    let repository = froe::Repository::open(store).expect("open the extracted store");
    let path = format!("/oak:index/{COUNTER_DEFINITION}");
    let Some(node) = repository.node_at_path(&path).expect("resolve the counter") else {
        return Vec::new();
    };
    let definition =
        froe::index::IndexDefinition::read(&node, &path).expect("model the counter definition");
    let index = froe::index::counter::CounterIndex::open(node, &definition);
    index
        .entries()
        .expect("read the counter's entries")
        .into_iter()
        .filter(|entry| entry.count.is_none())
        .map(|entry| entry.path)
        .collect()
}

/// Each rebuilt definition's `reindexCount` in the extracted store.
fn extracted_reindex_counts(store: &Path, definitions: &[String]) -> Vec<(String, i64)> {
    let repository = froe::Repository::open(store).expect("open the extracted store");
    definitions
        .iter()
        .map(|name| {
            let path = format!("/oak:index/{name}");
            let node = repository
                .node_at_path(&path)
                .expect("resolve the definition")
                .unwrap_or_else(|| panic!("{path} is not in the extracted store"));
            let model = froe::index::IndexDefinition::read(&node, &path).expect("model");
            (name.clone(), model.reindex.count)
        })
        .collect()
}

/// The assertion the phase exists for.
fn assert_every_definition_renders_identically(
    oak: &Path,
    froe_store: &Path,
    definitions: &[String],
) {
    for name in definitions {
        let subtree = format!("/oak:index/{name}");
        eprintln!("  comparing {subtree} against Oak's own rebuild");
        let oak_rendering = digest_of_subtree(oak, &subtree);
        let froe_rendering = digest_of_subtree(froe_store, &subtree);
        if oak_rendering != froe_rendering {
            let difference = first_difference(&oak_rendering, &froe_rendering);
            panic!(
                "{subtree}: froe's rebuild differs from Oak's own rebuild of the same \
                 store.\n{difference}"
            );
        }
    }
}

/// One definition's subtree, with the randomized counters excluded.
fn digest_of_subtree(store: &Path, subtree: &str) -> String {
    let whole = digest_store_excluding_properties(store, &[], &[RANDOMIZED_PROPERTY_PREFIX]);
    whole
        .lines()
        .filter(|line| {
            line.starts_with(subtree)
                && line[subtree.len()..]
                    .chars()
                    .next()
                    .is_none_or(|character| character == '/' || character == '\t')
        })
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first line the two renderings disagree on, with both sides.
fn first_difference(left: &str, right: &str) -> String {
    for (index, (one, other)) in left.lines().zip(right.lines()).enumerate() {
        if one != other {
            return format!("  line {}:\n    oak:  {one}\n    froe: {other}", index + 1);
        }
    }
    format!(
        "  the renderings share a prefix; oak has {} lines and froe {}",
        left.lines().count(),
        right.lines().count()
    )
}

/// `froe check` at the new head.
fn assert_check_passes(store: &Path) {
    eprintln!("  froe check at the reindexed head");
    let report = froe(&["check", store.to_str().unwrap()]);
    assert!(
        report.contains("good") || report.contains("consistent"),
        "the reindexed store does not check out: {report}"
    );
}

/// A second run renders the hidden children identically.
///
/// The definition node itself is excluded: `reindexCount` increments on
/// every run, so comparing it would be comparing the counter, not the index.
fn assert_a_rerun_is_identical(store: &Path, work: &Path, definitions: &[String]) {
    let hidden_before = hidden_children_rendering(store, definitions);
    let counts = extracted_reindex_counts(store, definitions);
    let resets: Vec<definition_edits::BookkeepingReset<'_>> = counts
        .iter()
        .map(|(name, count)| definition_edits::BookkeepingReset {
            name,
            reindex_count: *count,
        })
        .collect();
    definition_edits::reset_reindex_bookkeeping(store, &resets);
    eprintln!("  froe index reindex --yes, a second time");
    froe(&[
        "index",
        "reindex",
        store.to_str().unwrap(),
        "--yes",
        "--work-directory",
        work.to_str().unwrap(),
    ]);
    assert_eq!(
        hidden_children_rendering(store, definitions),
        hidden_before,
        "a second reindex over the same content wrote a different index"
    );
}

/// Every rebuilt definition's hidden children, rendered.
fn hidden_children_rendering(store: &Path, definitions: &[String]) -> String {
    let whole = digest_store_excluding_properties(store, &[], &[RANDOMIZED_PROPERTY_PREFIX]);
    whole
        .lines()
        .filter(|line| {
            definitions
                .iter()
                .any(|name| line.starts_with(&format!("/oak:index/{name}/:")))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Writes the canonicality verdict where `write_run_record` can read it,
/// even when this phase runs alone in its own process.
fn record_canonical_verdict(attempt: usize) {
    let path = work_root().join("canonical-index-property.txt");
    let _ = std::fs::write(
        &path,
        format!("canonical on attempt {attempt} of {CANONICAL_ATTEMPTS}\n"),
    );
}
