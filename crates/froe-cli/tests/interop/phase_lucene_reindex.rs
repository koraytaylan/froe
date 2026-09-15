//! The `lucene_reindex` phase: froe's offline Lucene rebuild against Oak's
//! own rebuild of the same bytes.
//!
//! The design is `property_reindex`'s, for the same reason: a booted Sling
//! writes content of its own before any request arrives, so the only way
//! two rebuilds can be compared is to make them rebuild **one store**. The
//! phase boots Sling on a copy of the fixture, flags every Lucene
//! definition, waits for the `async` lane to finish, stops, extracts *that*
//! store, and gives froe a copy of it.
//!
//! Three things are specific to Lucene.
//!
//! **The rebuild is asynchronous.** Oak clears `reindex` and advances
//! `reindexCount` on the lane, not in the commit that flags it, so the wait
//! is the same one `generate` uses and the hidden `:status` — invisible to
//! JCR and to Sling — is asserted afterwards on the extracted store.
//!
//! **The extracted index must be canonical.** A lane cycle between Oak's
//! rebuild and the stop updates a document through Oak's index writer as a
//! delete and an add, and the judge's `enumerate` reads live documents
//! only. An index carrying deletions therefore enumerates a subset of what
//! it holds, which no from-scratch rebuild produces. The phase reads every
//! `segments_N` with plan 0008's own segment reader, repeats the cycle
//! while any segment carries a deletion, and fails naming the condition
//! rather than comparing a weakened pair.
//!
//! **One difference is declared.** froe extracts no text, so under
//! `--binary-text marker` it indexes Oak's own `TextExtractionError` where
//! Oak indexed a binary's extracted text. `lucene_enumeration.rs` removes
//! that at the posting level from **both** sides before any statistic is
//! derived, and the phase asserts the removal covers exactly the documents
//! the fixture's binaries are on.

use std::collections::BTreeMap;

use froe::index::path_filter::PathVerdict;

use super::*;

/// The phase's name, as every assertion reports it.
const PHASE: &str = "lucene_reindex";

/// How many boot-flag-wait-stop-extract cycles the phase will run to get an
/// index with no deletions before failing.
const CANONICAL_ATTEMPTS: usize = 3;

/// Where Oak rebuilds.
const ORACLE_PORT: u16 = 8094;

/// Where Oak answers from froe's own rebuild.
const FROE_PORT: u16 = 8095;

/// Where Oak rebuilds from scratch after froe's reset.
pub(crate) const RESET_PORT: u16 = 8096;

/// Phase: froe's Lucene rebuild against Oak's own rebuild of the same store.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn lucene_reindex() {
    let work = work_root().join("lucene-reindex");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create the phase's work directory");

    let judge = Judge::compile();
    let definitions = lucene_definitions(&oak_store());
    let names = definition_names(&definitions);
    eprintln!(
        "  rebuilding {}: {}",
        definitions.len(),
        definitions.join(", ")
    );

    let rebuilt = oak_rebuild_with_a_canonical_index(&work, &names);
    record_canonical_verdict(rebuilt.attempt);
    assert_oak_finished_every_rebuild(&rebuilt.store, &names);

    let froe_store = work.join("froe");
    copy_store(&rebuilt.store, &froe_store);
    reset_to_what_oak_started_from(&froe_store, &rebuilt.store, &names);

    let before = digest_store(&froe_store);
    eprintln!("  froe index reindex --yes --binary-text marker");
    let summary = froe(&[
        "index",
        "reindex",
        froe_store.to_str().expect("utf-8"),
        "--yes",
        "--binary-text",
        "marker",
        "--work-directory",
        work.to_str().expect("utf-8"),
    ]);
    eprintln!("{summary}");

    let froe_dump = work.join("froe-dump");
    froe(&[
        "index",
        "dump",
        froe_store.to_str().expect("utf-8"),
        "--output",
        froe_dump.to_str().expect("utf-8"),
    ]);

    let exclusions = assert_every_definition_enumerates_identically(
        judge,
        &Comparison {
            store: &rebuilt.store,
            oak_dump: &rebuilt.dump,
            froe_dump: &froe_dump,
            work: &work,
            definitions: &definitions,
        },
    );
    // Asserted over the whole fixture rather than per definition: the
    // variant indexes a subtree with no binary in it on purpose, and a
    // per-definition assertion would make that a failure instead of the
    // coverage it is.
    assert!(
        exclusions > 0,
        "no definition in the fixture carries a binary at all, so the one declared \
         difference is never exercised and the exclusion proves nothing"
    );
    assert_every_definition_node_matches(&rebuilt.store, &froe_store, &names);
    assert_digest_delta(
        &before,
        &digest_store(&froe_store),
        ExpectedDigestDelta::Subtrees(&["/oak:index"]),
        PHASE,
    );
    assert_check_passes_at_head(&froe_store, PHASE);
    // froe's own index checker over the index froe just wrote. Oak accepts
    // it above and Lucene's `CheckIndex` calls it clean, and an operator
    // who runs `froe index check` after a rebuild would still be the first
    // to find a writer the checker disagrees with.
    eprintln!("  froe index check over the rebuilt indexes");
    froe(&["index", "check", froe_store.to_str().expect("utf-8")]);
    let after_boot = work.join("froe-after-boot");
    lucene_reindex_queries::assert_oak_answers_queries_from_froes_index(
        &froe_store,
        FROE_PORT,
        &after_boot,
    );
    lucene_reindex_queries::assert_oak_rebuilt_the_suggester(
        &after_boot,
        LUCENE_VARIANT_DEFINITION,
    );
    lucene_reindex_reset::assert_the_lane_reset_lets_oak_rebuild_from_scratch(
        judge,
        &work,
        &rebuilt.store,
    );

    eprintln!("  lucene_reindex phase passed");
}

/// What one Oak rebuild produced: the store, its dump, and which attempt it
/// took to get an index with no deletions.
struct OakRebuild {
    store: PathBuf,
    dump: PathBuf,
    attempt: usize,
}

/// The four paths one enumeration comparison is over.
pub(crate) struct Comparison<'a> {
    /// The store both dumps came from, which is where the binaries are
    /// counted: a booted Sling writes content of its own — compiled script
    /// classes under `/var` among it — so the fixture would name a
    /// different set from the store under comparison.
    pub(crate) store: &'a Path,
    pub(crate) oak_dump: &'a Path,
    pub(crate) froe_dump: &'a Path,
    pub(crate) work: &'a Path,
    pub(crate) definitions: &'a [String],
}

/// Each definition's name under `/oak:index`, from its path.
fn definition_names(definitions: &[String]) -> Vec<String> {
    definitions
        .iter()
        .map(|path| {
            path.rsplit_once('/')
                .expect("a definition path names a definition")
                .1
                .to_owned()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Oak's own rebuild
// ---------------------------------------------------------------------------

/// Boots, flags, waits, stops, extracts and dumps — repeating while the
/// extracted index carries a deletion.
fn oak_rebuild_with_a_canonical_index(work: &Path, names: &[String]) -> OakRebuild {
    for attempt in 1..=CANONICAL_ATTEMPTS {
        eprintln!("  Oak rebuild attempt {attempt} of {CANONICAL_ATTEMPTS}");
        let store = work.join(format!("oak-{attempt}"));
        let dump = work.join(format!("oak-{attempt}-dump"));
        let _ = std::fs::remove_dir_all(&store);
        let _ = std::fs::remove_dir_all(&dump);
        oak_rebuild_once(&store, names);
        froe(&[
            "index",
            "dump",
            store.to_str().expect("utf-8"),
            "--output",
            dump.to_str().expect("utf-8"),
        ]);
        match segments_carrying_deletions(&dump) {
            deletions if deletions.is_empty() => {
                return OakRebuild {
                    store,
                    dump,
                    attempt,
                };
            }
            deletions => eprintln!(
                "  the extracted index is not canonical ({} segment(s) carry deletions, \
                 e.g. {}); repeating",
                deletions.len(),
                deletions.first().map_or("", String::as_str)
            ),
        }
    }
    panic!(
        "Oak's Lucene index still carried deletions after {CANONICAL_ATTEMPTS} attempts: a \
         lane cycle between the rebuild and the stop updated a document as a delete and an \
         add, and the judge's enumerate reads live documents only, so the comparison would \
         be against a subset of what the index holds. The comparison is skipped rather than \
         weakened — investigate rather than raising the attempt count."
    );
}

/// One boot-flag-wait-stop-extract cycle.
fn oak_rebuild_once(extracted: &Path, names: &[String]) {
    let source = oak_store();
    let volume = PodmanVolume::new("froe-interop-lucene-reindex");
    let bootstrap =
        PodmanContainer::run_detached("froe-lucene-reindex-bootstrap", ORACLE_PORT, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&source, &volume.name);

    let sling = PodmanContainer::run_detached("froe-lucene-reindex-oak", ORACLE_PORT, &volume.name);
    wait_for_sling(ORACLE_PORT, "froe-lucene-reindex-oak");

    // Before anything is flagged, so the probe is part of the store both
    // sides share: Oak's rebuild indexes it and froe's rebuild indexes it,
    // symmetrically. Installing it afterwards would be a write to a store
    // whose indexes are the thing under comparison.
    eprintln!("  installing the query probe before flagging anything");
    sling::sling_install_query_probe(ORACLE_PORT);

    let mut before: Vec<(String, i64)> = Vec::new();
    for name in names {
        let path = format!("/oak:index/{name}");
        let rendered = sling_get_json(ORACLE_PORT, &path);
        before.push((
            name.clone(),
            sling::json_number(&rendered, "reindexCount").unwrap_or(0),
        ));
        eprintln!("  flagging {path} for reindex");
        sling::sling_set_property(
            ORACLE_PORT,
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
        eprintln!("  waiting for Oak's lane to finish rebuilding {path}");
        let after = sling::sling_wait_until_reindexed(ORACLE_PORT, &path, *count);
        eprintln!("    reindexCount {count} -> {after}");
    }

    drop(sling);
    store_from_volume(&volume.name, extracted);

    // The oracle's answers come from a **fresh boot on the extracted
    // store**, symmetrically with froe's. The session that rebuilt is
    // still running Sling, and Sling writes content of its own while it is
    // up — content that is gone again by the time the store is extracted,
    // so answers collected mid-session name rows neither store holds.
    // `property_reindex` failed intermittently on exactly that before both
    // sides were collected the same way.
    eprintln!("  collecting the oracle's query answers from the extracted store");
    let answers = lucene_reindex_queries::query_a_booted_store(
        extracted,
        "froe-lucene-reindex-oracle",
        ORACLE_PORT,
    );
    lucene_reindex_queries::write_oracle_answers(&answers);
}

/// Every segment in every dumped definition that carries a deletion.
///
/// Read with plan 0008's own `segments_N` reader over the dump's files,
/// rather than inferred from a file listing: a `.del` is not the only way a
/// deletion is recorded, and the count is what the claim is about.
fn segments_carrying_deletions(dump: &Path) -> Vec<String> {
    let mut carrying = Vec::new();
    for directory in dumped_index_directories(dump) {
        for (segment, deletions) in deletions_per_segment(&directory.path) {
            if deletions > 0 {
                carrying.push(format!(
                    "{}: segment {segment} carries {deletions} deletion(s)",
                    directory.index_path
                ));
            }
        }
    }
    carrying
}

/// One dumped index directory: which definition it is, and where its files
/// are.
struct DumpedIndex {
    index_path: String,
    path: PathBuf,
}

/// Every index directory under one `froe index dump` output.
fn dumped_index_directories(dump: &Path) -> Vec<DumpedIndex> {
    let dumps = dump.join("index-dumps");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dumps).expect("read the dump's index-dumps") {
        let entry = entry.expect("read an entry");
        let details = entry.path().join("index-details.txt");
        if !details.is_file() {
            continue;
        }
        let index_path = read_details(&details)
            .get("indexPath")
            .cloned()
            .expect("a dumped index names its definition");
        found.push(DumpedIndex {
            index_path,
            path: entry.path().join("data"),
        });
    }
    assert!(
        !found.is_empty(),
        "the dump under {} holds no index directory",
        dumps.display()
    );
    found.sort_by(|left, right| left.index_path.cmp(&right.index_path));
    found
}

/// Each segment's name and deletion count, from the directory's commit file.
fn deletions_per_segment(directory: &Path) -> Vec<(String, i32)> {
    use froe::index::lucene::Reader;
    use froe::index::lucene::segments::{is_commit_file_name, read_commit_file};

    let open = |name: &str| -> Reader<std::fs::File> {
        let path = directory.join(name);
        let length = std::fs::metadata(&path)
            .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
            .len();
        let file = std::fs::File::open(&path)
            .unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
        Reader::new(file, name, length)
    };

    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| is_commit_file_name(name))
        .collect();
    names.sort();
    let commit_name = names
        .last()
        .unwrap_or_else(|| panic!("{} holds no commit file", directory.display()))
        .clone();
    let mut reader = open(&commit_name);
    let commit = read_commit_file(&mut reader, &commit_name, |info| Ok(open(info)))
        .unwrap_or_else(|error| panic!("read {commit_name}: {error}"));
    commit
        .segments
        .iter()
        .map(|segment| (segment.name.clone(), segment.deletion_count))
        .collect()
}

/// Asserts the hidden `:status` Oak's lane wrote is newer than the one the
/// fixture carried.
///
/// The `reindex` flag and `reindexCount` are visible through JCR and the
/// wait above is on them; `:status` is not, and it is what the rebuild
/// itself writes. A definition whose flag cleared without its writer ever
/// closing would pass the wait and fail here.
fn assert_oak_finished_every_rebuild(store: &Path, names: &[String]) {
    for name in names {
        let path = format!("/oak:index/{name}/:status");
        let fresh = froe(&["node", store.to_str().expect("utf-8"), &path]);
        let original = froe(&["node", oak_store().to_str().expect("utf-8"), &path]);
        let timestamp = |rendered: &str| -> String {
            rendered
                .lines()
                .find(|line| line.contains("reindexCompletionTimestamp"))
                .unwrap_or_else(|| panic!("{path} names no reindexCompletionTimestamp"))
                .to_owned()
        };
        assert_ne!(
            timestamp(&fresh),
            timestamp(&original),
            "{path} carries the fixture's own completion timestamp, so Oak's lane never \
             rebuilt this definition"
        );
    }
    eprintln!("  Oak's lane rebuilt every definition and wrote a fresh :status");
}

/// Puts each definition's bookkeeping back to what Oak started from.
pub(crate) fn reset_to_what_oak_started_from(
    froe_store: &Path,
    oak_rebuilt: &Path,
    names: &[String],
) {
    let counts = extracted_reindex_counts(oak_rebuilt, names);
    let resets: Vec<definition_edits::BookkeepingReset<'_>> = counts
        .iter()
        .map(|(name, count)| definition_edits::BookkeepingReset {
            name,
            // One below Oak's value, so froe's single increment lands back
            // on exactly it whatever the attempt count was.
            reindex_count: count - 1,
        })
        .collect();
    definition_edits::reset_reindex_bookkeeping(froe_store, &resets);
}

/// Each definition's `reindexCount` in a store.
fn extracted_reindex_counts(store: &Path, names: &[String]) -> Vec<(String, i64)> {
    let repository = froe::Repository::open(store).expect("open the store");
    names
        .iter()
        .map(|name| {
            let path = format!("/oak:index/{name}");
            let node = repository
                .node_at_path(&path)
                .expect("resolve the definition")
                .unwrap_or_else(|| panic!("{path} is not in the store"));
            let model = froe::index::IndexDefinition::read(&node, &path).expect("model");
            (name.clone(), model.reindex.count)
        })
        .collect()
}

/// Writes the canonicality verdict where `write_run_record` can read it.
fn record_canonical_verdict(attempt: usize) {
    let path = work_root().join("canonical-index-lucene.txt");
    let _ = std::fs::write(
        &path,
        format!("canonical on attempt {attempt} of {CANONICAL_ATTEMPTS}\n"),
    );
}

// ---------------------------------------------------------------------------
// The content oracle
// ---------------------------------------------------------------------------

/// Oak's `TEXT_EXTRACTION_ERROR`, as the analyzed term it becomes.
///
/// The marker is one run of letters, so the standard tokenizer keeps it
/// whole and the lower-case filter folds it: one term, and one that occurs
/// nowhere else in the fixture.
const MARKER_TERM: &str = "textextractionerror";

/// Both indexes enumerated by the judge, re-keyed by document path, and
/// compared — with the declared binary difference removed from each first.
pub(crate) fn assert_every_definition_enumerates_identically(
    judge: &Judge,
    comparison: &Comparison<'_>,
) -> usize {
    let mut exclusions = 0usize;
    let oak = dumped_index_directories(comparison.oak_dump);
    let froe_directories = dumped_index_directories(comparison.froe_dump);
    let by_path: BTreeMap<&str, &Path> = froe_directories
        .iter()
        .map(|directory| (directory.index_path.as_str(), directory.path.as_path()))
        .collect();

    for definition in comparison.definitions {
        let oak_directory = oak
            .iter()
            .find(|directory| &directory.index_path == definition)
            .unwrap_or_else(|| panic!("Oak's dump has no directory for {definition}"));
        let froe_directory = by_path
            .get(definition.as_str())
            .unwrap_or_else(|| panic!("froe's dump has no directory for {definition}"));

        eprintln!("  comparing {definition} against Oak's own rebuild");
        eprintln!("    judge: checkindex over froe's rebuild");
        judge.run(
            "LuceneJudge",
            &["checkindex", "/index"],
            vec![Mount::read_only(*froe_directory, "/index")],
        );

        // The definition's path, flattened into one directory name: a
        // podman mount argument is colon-separated, so a `:` in a host path
        // is read as the start of the container path.
        let flattened: String = definition
            .chars()
            .map(|character| {
                if character.is_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let enumerations = comparison.work.join(format!("enumerations{flattened}"));
        std::fs::create_dir_all(&enumerations).expect("create the enumeration directory");
        let oak_enumeration = enumerate(judge, &oak_directory.path, &enumerations, "oak.txt");
        let froe_enumeration = enumerate(judge, froe_directory, &enumerations, "froe.txt");

        let mut theirs = lucene_enumeration::Enumeration::parse(&oak_enumeration);
        let mut ours = lucene_enumeration::Enumeration::parse(&froe_enumeration);
        let excluded = assert_the_binary_exclusion_is_exactly_the_binaries(
            &ours,
            comparison.store,
            definition,
        );
        exclusions += excluded.len();
        theirs.exclude_binary_text(&excluded);
        ours.exclude_binary_text(&excluded);

        match lucene_enumeration::compare("oak", &theirs, "froe", &ours) {
            Ok(lines) => eprintln!(
                "    {definition}: {} documents, {lines} enumerated lines, identical outside \
                 {} declared binary exclusion(s)",
                ours.document_count(),
                excluded.len()
            ),
            Err(difference) => panic!(
                "{definition}: froe's rebuild enumerates differently from Oak's own rebuild \
                 of the same store.\n{difference}"
            ),
        }
        assert_a_wrong_position_increment_is_caught(&theirs, &ours);
    }
    exclusions
}

/// One `enumerate` run, returning what it wrote.
fn enumerate(judge: &Judge, index: &Path, into: &Path, name: &str) -> String {
    judge.run(
        "Corpus",
        &["enumerate", "/index", &format!("/out/{name}")],
        vec![
            Mount::read_only(index, "/index"),
            Mount::writable(into, "/out"),
        ],
    );
    std::fs::read_to_string(into.join(name)).expect("read the enumeration the judge wrote")
}

/// The documents froe's own index says a binary went to, checked against
/// the nodes the store says carry one.
///
/// Two independent derivations of the same set. froe's is the marker term
/// in the index it wrote — the one term `--binary-text marker` adds.
/// The store's is every node the definition includes that carries a binary
/// property with a `jcr:mimeType`, plus, for each aggregate include that
/// names a relative node, the including node under `fullnode:<path>`. They
/// must agree exactly: a marker on a document the store says holds no
/// binary would be froe indexing something Oak never did, and a
/// binary-bearing node with no marker would be an exclusion that hides a
/// real difference.
fn assert_the_binary_exclusion_is_exactly_the_binaries(
    ours: &lucene_enumeration::Enumeration,
    store: &Path,
    definition: &str,
) -> lucene_enumeration::BinaryExclusion {
    let declared = ours.documents_carrying_the_marker(&hexadecimal(MARKER_TERM.as_bytes()));
    let from_the_store = binary_bearing_documents(store, definition);
    assert_eq!(
        declared, from_the_store,
        "{definition}: the documents froe marked as carrying a binary's text are not the \
         documents the store says carry a binary. An exclusion wider than the binaries \
         would hide a real difference, and one narrower would fail on a difference that is \
         declared."
    );
    declared
}

/// Every document the definition makes that a binary's text reaches, and
/// the field it reaches it under.
///
/// `:fulltext` for the node's own binary, which is the shape this fixture
/// has. A `relativeNode` aggregate include would put an aggregated child's
/// binary text under `fullnode:<include path>` on the *including* node's
/// document instead, and a non-relative include would add it to that
/// node's own `:fulltext`; neither is reachable here, and the assertion
/// below refuses to guess rather than silently under-computing the set if
/// the fixture ever gains that combination.
fn binary_bearing_documents(store: &Path, definition: &str) -> lucene_enumeration::BinaryExclusion {
    let repository = froe::Repository::open(store).expect("open the fixture");
    let root = repository.content_root().expect("content root");
    let node = repository
        .node_at_path(definition)
        .expect("resolve the definition")
        .unwrap_or_else(|| panic!("{definition} is not in the store"));
    let model =
        froe::index::IndexDefinition::read(&node, definition).expect("model the definition");
    let mut warnings = Vec::new();
    let rules = froe::index::lucene::documents::rules::IndexingRules::read(
        &node,
        definition,
        &root,
        &mut warnings,
    )
    .expect("read the definition's indexing rules");

    let mut affected = lucene_enumeration::BinaryExclusion::new();
    let mut aggregating = false;
    let mut walk = vec![("/".to_owned(), root)];
    while let Some((path, node)) = walk.pop() {
        if let Ok(Some(rule)) = rules.applicable_rule(&node) {
            aggregating |= rule.aggregate.has_node_aggregates();
            if model.path_filter.filter(&path) == PathVerdict::Include
                && carries_indexed_binary(&node, rule)
            {
                affected.insert((path.clone(), lucene_enumeration::FULLTEXT_FIELD.to_owned()));
            }
        }
        for (name, child) in node.child_node_entries().expect("read children") {
            if name.starts_with(':') {
                continue;
            }
            let child_path = if path == "/" {
                format!("/{name}")
            } else {
                format!("{path}/{name}")
            };
            if model.path_filter.filter(&child_path) == PathVerdict::Exclude {
                continue;
            }
            walk.push((child_path, child));
        }
    }
    assert!(
        !aggregating || affected.is_empty(),
        "{definition} both aggregates node content and indexes a binary, so a binary's text \
         reaches an including node's document as well as its own. The exclusion this phase \
         computes covers only the node's own :fulltext, and would hide the including \
         node's difference rather than declare it — extend it before this fixture combines \
         the two."
    );
    affected
}

/// Whether this node's own document carries a binary's text.
///
/// The three conditions Oak's own per-property pass applies, in its order:
/// the rule resolves a property definition for the name, that definition is
/// fulltext-enabled, the rule's `includePropertyTypes` admits `BINARY`
/// (empty means all, which is the rule-level default whatever the
/// definition says), and the node carries a `jcr:mimeType` — without which
/// Oak's own extraction stops before Tika is reached.
fn carries_indexed_binary(
    node: &froe::content::node::NodeState<'_>,
    rule: &froe::index::lucene::documents::rules::IndexingRule,
) -> bool {
    let properties = node.properties().expect("read properties");
    if !properties
        .iter()
        .any(|property| property.name == "jcr:mimeType")
    {
        return false;
    }
    let admits_binaries = rule.include_property_types.is_empty()
        || rule
            .include_property_types
            .iter()
            .any(|name| name.eq_ignore_ascii_case("Binary"));
    if !admits_binaries {
        return false;
    }
    properties.iter().any(|property| {
        property.property_type == froe::PropertyType::Binary
            && rule.config_of(&property.name).is_some_and(
                froe::index::lucene::documents::rules::PropertyDefinition::fulltext_enabled,
            )
    })
}

/// Hexadecimal, as the judge renders a term.
fn hexadecimal(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut rendered, byte| {
        let _ = write!(rendered, "{byte:02x}");
        rendered
    })
}

/// The negative control: the comparison has to **fail** on a wrong
/// position, and fail naming `:fulltext`.
///
/// A position increment the word delimiter got wrong moves every token
/// after it in the field it is on, which is what this perturbs: one
/// `:fulltext` posting's positions are shifted by one on a copy of froe's
/// enumeration, and the same comparison the phase just passed must refuse
/// it. Perturbing the enumeration rather than the analyzer is what makes
/// this a control the phase can run: the analyzer is compiled into the
/// binary under test, and a run that rebuilt froe with a defect would be
/// testing a different binary than the one that produced the index above.
/// The defect itself is neutralized against the analysis module's own
/// hand-computed vectors, which is where a wrong increment is caught
/// first; this proves the *comparison* would not let one through.
fn assert_a_wrong_position_increment_is_caught(
    theirs: &lucene_enumeration::Enumeration,
    ours: &lucene_enumeration::Enumeration,
) {
    let mut perturbed = ours.render();
    let target = perturbed
        .iter()
        .position(|line| {
            line.starts_with(&format!(
                "posting\t{}\t",
                lucene_enumeration::FULLTEXT_FIELD
            )) && line.split('\t').count() > POSITIONS_FROM
        })
        .expect("the index carries a :fulltext posting with positions");
    perturbed[target] = shift_first_position(&perturbed[target]);
    let expected = theirs.render();
    let difference = expected
        .iter()
        .zip(&perturbed)
        .position(|(one, other)| one != other)
        .expect("the perturbed enumeration must differ from Oak's");
    assert!(
        perturbed[difference].starts_with(&format!(
            "posting\t{}\t",
            lucene_enumeration::FULLTEXT_FIELD
        )),
        "a wrong position increment must make the comparison fail on {}, and it named {:?} \
         instead",
        lucene_enumeration::FULLTEXT_FIELD,
        perturbed[difference]
    );
    eprintln!(
        "    negative control: one shifted :fulltext position is refused at line {}",
        difference + 1
    );
}

/// Where a rendered posting's `position:start:end` triples begin.
///
/// `posting`, the field name, the term, the document ordinal and the
/// frequency come first — and the field name is itself `:fulltext`, so a
/// scan for the first field carrying a colon finds the name rather than a
/// position.
const POSITIONS_FROM: usize = 5;

/// One posting line with its first position advanced by one.
fn shift_first_position(line: &str) -> String {
    let mut fields: Vec<String> = line.split('\t').map(str::to_owned).collect();
    let mut parts = fields[POSITIONS_FROM].split(':');
    let position: u32 = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("a position is a number: {:?}", fields[POSITIONS_FROM]));
    let rest: Vec<&str> = parts.collect();
    fields[POSITIONS_FROM] = format!("{}:{}", position + 1, rest.join(":"));
    fields.join("\t")
}

// ---------------------------------------------------------------------------
// The definition node
// ---------------------------------------------------------------------------

/// Properties whose value is fresh on every rebuild by design, and which
/// are therefore not part of the comparison.
///
/// `uid` is a decimal epoch-millisecond string Oak's own reader uses to
/// tell one index from the next, and the two timestamps beside it are when
/// each run happened. Everything else in `:status` — `indexedNodes` above
/// all — is the claim. `reindexCount` is excluded inside the
/// `:index-definition` clone alone, where it records the count the run
/// found rather than the count it left.
const FRESH_BY_DESIGN: [&str; 3] = ["uid=", "lastUpdated=", "reindexCompletionTimestamp="];

/// The definition node itself, compared as task 0712 step 3 compares the
/// property family's.
///
/// This is the oracle for everything the rebuild writes *beside* the index:
/// the `facets` configuration the document maker's dimensions persist, the
/// `seed`, the removal of `refresh` and `indexImportState`, the `:version`,
/// the `:status` node's indexed-node count and the `:index-definition`
/// clone of the pre-run visible state.
fn assert_every_definition_node_matches(oak: &Path, froe_store: &Path, names: &[String]) {
    for name in names {
        let subtree = format!("/oak:index/{name}");
        eprintln!("  comparing {subtree}'s own nodes against Oak's");
        assert_suggest_data_is_oaks_alone(oak, froe_store, name);
        let theirs = definition_rendering(oak, name);
        let ours = definition_rendering(froe_store, name);
        assert!(
            theirs.lines().count() > 1,
            "{subtree} renders as one line on Oak's store, so the comparison would be vacuous"
        );
        if theirs != ours {
            let difference = first_definition_difference(&theirs, &ours);
            panic!(
                "{subtree}: froe's rebuild wrote a different definition node from Oak's own \
                 rebuild of the same store.\n{difference}"
            );
        }
    }
}

/// The one node the rebuild deliberately leaves to Oak, asserted on both
/// sides rather than excluded silently.
///
/// froe removes `:suggest-data` and builds no suggester — `docs/index.md`
/// §5.6 — because the dictionary is Lucene's own suggester artifact rather
/// than an index froe writes, and Oak's suggester schedule rebuilds it
/// from the `:suggest` field on the lane's next cycle. Oak's own rebuild
/// of a definition carrying `useInSuggest` writes one, so the difference
/// is real and is the only one the renderings below drop.
fn assert_suggest_data_is_oaks_alone(oak: &Path, froe_store: &Path, name: &str) {
    let node = format!("/oak:index/{name}/:suggest-data");
    let carries = |store: &Path| {
        digest_store(store)
            .lines()
            .any(|line| line.starts_with(&node))
    };
    if !carries(oak) {
        assert!(
            !carries(froe_store),
            "{node}: froe's rebuild wrote a suggester dictionary Oak's own rebuild did not"
        );
        return;
    }
    assert!(
        !carries(froe_store),
        "{node}: froe's rebuild wrote a suggester dictionary, which it declares it does not          build — so the exclusion below would hide a difference rather than declare one"
    );
    eprintln!("    declared: Oak's own {node} is the one node froe leaves to Oak's suggester");
}

/// One definition's subtree, with `:data`, `:suggest-data` and the
/// fresh-by-design values removed.
fn definition_rendering(store: &Path, name: &str) -> String {
    let subtree = format!("/oak:index/{name}");
    let data = format!("{subtree}/:data");
    let suggester = format!("{subtree}/:suggest-data");
    let clone = format!("{subtree}/:index-definition");
    digest_store(store)
        .lines()
        .filter(|line| {
            line.starts_with(&subtree)
                && line[subtree.len()..]
                    .chars()
                    .next()
                    .is_none_or(|character| character == '/' || character == '\t')
        })
        .filter(|line| !line.starts_with(&data) && !line.starts_with(&suggester))
        .map(|line| {
            let in_the_clone = line.starts_with(&clone);
            line.split('\t')
                .filter(|field| {
                    let fresh = FRESH_BY_DESIGN.iter().any(|name| field.starts_with(name));
                    let counted = in_the_clone && field.starts_with("reindexCount=");
                    !(fresh || counted)
                })
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first line the two renderings disagree on, with both sides.
fn first_definition_difference(oak: &str, ours: &str) -> String {
    for (at, (one, other)) in oak.lines().zip(ours.lines()).enumerate() {
        if one != other {
            return format!("  line {}:\n    oak:  {one}\n    froe: {other}", at + 1);
        }
    }
    format!(
        "  the renderings share a prefix; oak has {} lines and froe {}",
        oak.lines().count(),
        ours.lines().count()
    )
}
