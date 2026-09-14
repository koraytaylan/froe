//! The `lucene_import` phase: froe's import against Oak, both directions.
//!
//! The round trip asks whether an index froe dumped and put back is the
//! index that was there. The out-of-band half asks the harder question:
//! whether an index **Oak's own editors built** installs, and whether Oak
//! then boots and answers a fulltext query through it.
//!
//! The shared reading helpers and the `lucene_dump` phase are in
//! `phase_lucene_transport.rs` beside this; one file per phase, because
//! together they outgrew the size gate.

use super::*;

/// The definition this phase moves. Named explicitly because the delta
/// assertion names one subtree, and plan 0010 adds a second definition.
const IMPORTED_DEFINITION: &str = "/oak:index/lucene";

/// The subtree an import is allowed to change, and the only one.
const IMPORTED_SUBTREE: &[&str] = &["/oak:index/lucene"];

/// Properties that are new by design on every import and cannot be
/// compared against the original.
///
/// `uniqueKey` is sixteen fresh bytes per file, `jcr:lastModified` is the
/// moment the file was written, and `:status/uid` is the fresh identifier
/// Oak's own importer leaves. `dirListing` is compared as a set instead of
/// being excluded, because its *contents* are the claim and only its order
/// is Oak's hash-set iteration.
const FRESH_BY_DESIGN: &[&str] = &["uniqueKey", "jcr:lastModified", "uid"];

/// The three `:status` properties an import legitimately does not write.
///
/// A recorded departure from oak-run's importer rather than a difference
/// this phase can assert away: they are per-cycle state, and Oak's own
/// importer leaves none to copy them from.
const PER_CYCLE_STATUS_PROPERTIES: &[&str] =
    &["indexedNodes", "lastUpdated", "reindexCompletionTimestamp"];

/// Phase: froe's import against Oak, in both directions the import exists
/// for.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn lucene_import() {
    let work = work_root().join("lucene-import");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create the phase's work directory");

    round_trip_through_froes_own_dump(&work);
    assert_the_refusals(&work);
    import_an_index_oak_built_out_of_band(&work);
    assert_oak_queries_through_the_imported_index(&work);
    eprintln!("  lucene_import phase passed");
}

/// The round trip: dump the fixture's index, lose it, put it back.
///
/// "Lose it" is the state a definition is actually in when this command is
/// reached for — the hidden children gone — rather than a contrived one.
fn round_trip_through_froes_own_dump(work: &Path) {
    eprintln!("  round trip through froe's own dump");
    let source = oak_store();

    let store = work.join("round-trip");
    copy_store(&source, &store);

    let dump = work.join("round-trip-dump");
    froe(&[
        "index",
        "dump",
        store.to_str().expect("utf-8"),
        "--output",
        dump.to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
    ]);

    let original = definition_digest(&store);
    let original_count = reindex_count(&store);
    let checkpoints_before = checkpoint_names(&store);
    let baseline = digest_store(&store);

    // The state a lost index leaves: every hidden child gone, the
    // definition still there. `reindexCount` is put back as it was, so the
    // import's own increment lands one above the original.
    remove_hidden_children(
        &store,
        &[BookkeepingReset {
            name: "lucene",
            reindex_count: original_count,
        }],
    );

    froe(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        dump.join("index-dumps").to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
        "--yes",
    ]);

    assert_the_index_is_back(&store, &original, &dump, original_count + 1);
    assert_eq!(
        checkpoint_names(&store),
        checkpoints_before,
        "an import releases no checkpoint"
    );
    assert_digest_delta(
        &baseline,
        &digest_store(&store),
        ExpectedDigestDelta::Subtrees(IMPORTED_SUBTREE),
        "lucene_import round trip",
    );
    froe(&["check", store.to_str().expect("utf-8")]);
    eprintln!("    the index is back, byte for byte");
}

/// The imported definition renders as the original did, allowing for what
/// is fresh by design, and every file reads back byte for byte.
fn assert_the_index_is_back(store: &Path, original: &str, dump: &Path, expected_count: i64) {
    froe(&["index", "check", store.to_str().expect("utf-8")]);

    assert_eq!(
        normalized_definition(&definition_digest(store)),
        normalized_definition(original),
        "the imported definition must render as the original did"
    );
    assert_eq!(
        reindex_count(store),
        expected_count,
        "reindexCount must be the file's value plus one"
    );
    assert_the_files_read_back(store, dump);
    assert_the_status_is_what_the_importer_leaves(store);
}

/// Every file reads back out of the store byte-identical to the file the
/// dump wrote.
///
/// The digest cannot make this claim: a `jcr:data` digest covers the
/// *stored blob*, which is the file followed by the sixteen fresh
/// `uniqueKey` bytes, so two honest imports of one file hash differently.
/// Reading through `OakDirectory` compares the file, which is the claim.
fn assert_the_files_read_back(store: &Path, dump: &Path) {
    use std::io::Read as _;

    let repository = froe::Repository::open(store).expect("open the store");
    let node = repository
        .node_at_path(IMPORTED_DEFINITION)
        .expect("resolve the definition")
        .expect("the definition exists");
    let definition = froe::index::IndexDefinition::read(&node, IMPORTED_DEFINITION)
        .expect("model the definition");
    let directory =
        froe::index::lucene::OakDirectory::open(&repository, &node, &definition, ":data")
            .expect("open :data")
            .expect(":data exists");

    let source = froe_dump_directory(dump, IMPORTED_DEFINITION).join("data");
    let mut names: Vec<String> = directory.file_names().to_vec();
    names.sort();
    let mut on_disk: Vec<String> = std::fs::read_dir(&source)
        .expect("read the dumped directory")
        .map(|entry| {
            entry
                .expect("read an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    on_disk.sort();
    assert_eq!(
        names, on_disk,
        "the imported directory holds a different file set than the dump"
    );

    for name in &names {
        let file = directory.file(name).expect("open a file");
        let mut stored = Vec::new();
        file.reader().read_to_end(&mut stored).expect("read a file");
        let original = std::fs::read(source.join(name)).expect("read the dumped file");
        assert_eq!(
            stored.len(),
            original.len(),
            "{name} reads back at a different length than the dump wrote"
        );
        if stored != original {
            let offset = stored
                .iter()
                .zip(original.iter())
                .position(|(ours, theirs)| ours != theirs)
                .expect("the lengths are equal and the contents differ");
            panic!(
                "{name} differs at byte {offset}: {:#04x} against {:#04x}",
                stored[offset], original[offset]
            );
        }
    }
    eprintln!("    {} files read back byte for byte", names.len());
}

/// `:status` carries the fresh `uid` and none of the per-cycle state.
fn assert_the_status_is_what_the_importer_leaves(store: &Path) {
    let status = digest_store(store)
        .lines()
        .find(|line| line.starts_with(&format!("{IMPORTED_DEFINITION}/:status\t")))
        .map(str::to_owned)
        .expect("the imported definition has a :status node");
    assert!(
        status.contains("uid=String:"),
        ":status must carry a fresh uid: {status}"
    );
    for absent in PER_CYCLE_STATUS_PROPERTIES {
        assert!(
            !status.contains(absent),
            ":status must not carry {absent}, which Oak's own importer leaves no state to              copy it from: {status}"
        );
    }
}

/// The digest lines of the imported definition's subtree.
fn definition_digest(store: &Path) -> String {
    digest_store(store)
        .lines()
        .filter(|line| {
            line.strip_prefix(IMPORTED_DEFINITION).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with('/') || rest.starts_with('\t')
            })
        })
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The digest with everything an honest import legitimately changes
/// reduced to a placeholder, and `dirListing` reduced to a sorted set.
///
/// `:status` is dropped entirely and asserted on its own, because the
/// three per-cycle properties are a recorded departure rather than a
/// difference that can be normalized field by field.
fn normalized_definition(digest: &str) -> Vec<String> {
    digest
        .lines()
        .filter(|line| !line.starts_with(&format!("{IMPORTED_DEFINITION}/:status\t")))
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let mut kept: Vec<String> = Vec::new();
            for (position, field) in fields.iter().enumerate() {
                if position == 0 {
                    kept.push((*field).to_owned());
                    continue;
                }
                let Some((name, value)) = field.split_once('=') else {
                    kept.push((*field).to_owned());
                    continue;
                };
                if FRESH_BY_DESIGN.contains(&name) {
                    // The property must still be *present*: dropping the
                    // name as well would let an import that wrote none of
                    // them pass.
                    kept.push(format!("{name}=<fresh>"));
                    continue;
                }
                if name == "dirListing" {
                    // Oak stores the listing in a concurrent hash set's
                    // iteration order and reads it back as a set, so only
                    // its contents are a claim. froe writes it in name
                    // order, a deviation recorded at the writer.
                    // The digest renders a property as `<type>:<values>`,
                    // so the type prefix has to come off before the values
                    // are sorted: left on, it pins whichever value happens
                    // to be first and the sort changes nothing.
                    let (property_type, listing) = value
                        .split_once(':')
                        .expect("a digest property value carries its type");
                    let mut names: Vec<&str> = listing.split('\u{1f}').collect();
                    names.sort_unstable();
                    kept.push(format!(
                        "dirListing={property_type}:{}",
                        names.join("\u{1f}")
                    ));
                    continue;
                }
                if name == "reindexCount" {
                    // Asserted exactly, on its own: the import's whole
                    // point is that this value advances.
                    kept.push("reindexCount=<advanced>".to_owned());
                    continue;
                }
                if name == "jcr:data" {
                    // The stored blob is the file followed by the sixteen
                    // fresh `uniqueKey` bytes, so its digest differs on
                    // every honest import. The bytes are compared through
                    // the reader instead.
                    let length = value.split_once('@').map_or(value, |(length, _)| length);
                    kept.push(format!("jcr:data=<blob {length}>"));
                    continue;
                }
                kept.push((*field).to_owned());
            }
            kept.join("\t")
        })
        .collect()
}

/// The definition's `reindexCount`.
fn reindex_count(store: &Path) -> i64 {
    let repository = froe::Repository::open(store).expect("open the store");
    let node = repository
        .node_at_path(IMPORTED_DEFINITION)
        .expect("resolve the definition")
        .expect("the definition exists");
    froe::index::IndexDefinition::read(&node, IMPORTED_DEFINITION)
        .expect("model the definition")
        .reindex
        .count
}

/// The checkpoint names a store holds.
fn checkpoint_names(store: &Path) -> Vec<String> {
    let repository = froe::Repository::open(store).expect("open the store");
    let mut names: Vec<String> = repository
        .checkpoints()
        .expect("read the checkpoints")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names
}

/// What the import refuses, against a real store, with the store unchanged
/// in every case.
fn assert_the_refusals(work: &Path) {
    eprintln!("  refusals");

    // A checkpoint that is not the attachment state.
    let (store, input) = store_and_dump(work, "refuse-checkpoint");
    let before = store_file_snapshot(&store);
    std::fs::write(
        input.join("indexer-info.properties"),
        "checkpoint=not-a-checkpoint-this-store-holds\n",
    )
    .expect("rewrite the properties file");
    let refusal = froe_failure(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        input.to_str().expect("utf-8"),
        "--yes",
    ]);
    assert!(
        refusal.contains("does not resolve in this store"),
        "a checkpoint the store does not hold must be refused: {refusal}"
    );
    assert_eq!(
        store_file_snapshot(&store),
        before,
        "a refused import must not write a byte"
    );
    eprintln!("    a checkpoint the store does not hold is refused");

    assert_a_checkpoint_that_is_not_the_attachment_state_is_refused(work);
    assert_a_drifting_definitions_file_is_refused(work);
    assert_a_synchronous_definition_is_refused(work);
}

/// The state rule itself, against a checkpoint that **resolves**.
///
/// The dangling case above never reaches the rule: resolution fails first.
/// A checkpoint taken at the head does resolve, and its root is not the
/// lane's — every lane cycle commits after taking its checkpoint, so the
/// head's root never equals a lane checkpoint's, which is the whole reason
/// the rule accepts only the lane's own.
fn assert_a_checkpoint_that_is_not_the_attachment_state_is_refused(work: &Path) {
    let (store, input) = store_and_dump(work, "refuse-state-rule");

    froe(&[
        "checkpoint",
        "create",
        store.to_str().expect("utf-8"),
        "--yes",
    ]);
    let lane = lane_checkpoint(&store);
    let rival = checkpoint_names(&store)
        .into_iter()
        .find(|name| name != &lane)
        .expect("the store now holds a checkpoint that is not the lane's");

    let before = store_file_snapshot(&store);
    std::fs::write(
        input.join("indexer-info.properties"),
        format!("checkpoint={rival}\n"),
    )
    .expect("rewrite the properties file");

    let refusal = froe_failure(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        input.to_str().expect("utf-8"),
        "--yes",
    ]);
    assert!(
        refusal.contains(&format!("was built at checkpoint {rival}"))
            && refusal.contains(&format!("lane {LANE} resumes from {lane}")),
        "the refusal must name both checkpoints and both roots: {refusal}"
    );
    assert!(
        refusal.contains("rebuild at the lane's own checkpoint"),
        "the refusal must name the remedy: {refusal}"
    );
    assert_eq!(
        store_file_snapshot(&store),
        before,
        "a refused import must not write a byte"
    );
    eprintln!("    an index built at another state is refused naming both roots");
}

/// A definitions file carrying a visible property the store's definition
/// does not.
fn assert_a_drifting_definitions_file_is_refused(work: &Path) {
    let (store, input) = store_and_dump(work, "refuse-drift");
    let before = store_file_snapshot(&store);

    let path = input.join("index-definitions.json");
    let content = std::fs::read_to_string(&path).expect("read the definitions file");
    let opening = format!("\"{IMPORTED_DEFINITION}\": {{");
    let at = content
        .find(&opening)
        .expect("the file carries the definition")
        + opening.len();
    let mut edited = content;
    edited.insert_str(at, "\n    \"evaluatePathRestrictions\": true,");
    std::fs::write(&path, edited).expect("rewrite the definitions file");

    let refusal = froe_failure(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        input.to_str().expect("utf-8"),
        "--yes",
    ]);
    assert!(
        refusal.contains("/evaluatePathRestrictions")
            && refusal.contains("never a definition change"),
        "a drifting definitions file must be refused naming the difference: {refusal}"
    );
    assert_eq!(
        store_file_snapshot(&store),
        before,
        "a refused import must not write a byte"
    );
    eprintln!("    a drifting definitions file is refused naming the property");
}

/// A definition made synchronous on the copy: oak-run's own importer never
/// completes that case, so froe refuses it rather than inventing one.
fn assert_a_synchronous_definition_is_refused(work: &Path) {
    let (store, input) = store_and_dump(work, "refuse-synchronous");

    edit_definitions(
        &store,
        &[BookkeepingReset {
            name: "lucene",
            reindex_count: reindex_count(&store),
        }],
        DefinitionEdit {
            remove_async: true,
            ..DefinitionEdit::default()
        },
    );
    let before = store_file_snapshot(&store);

    let refusal = froe_failure(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        input.to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
        "--yes",
    ]);
    assert!(
        refusal.contains(IMPORTED_DEFINITION) && refusal.contains("synchronous"),
        "a synchronous definition must be refused by name: {refusal}"
    );
    assert_eq!(
        store_file_snapshot(&store),
        before,
        "a refused import must not write a byte"
    );
    eprintln!("    a synchronous definition is refused by name");
}

/// A fresh copy of the fixture with its Lucene index dumped beside it.
fn store_and_dump(work: &Path, name: &str) -> (PathBuf, PathBuf) {
    let store = work.join(name);
    copy_store(&oak_store(), &store);
    let dump = work.join(format!("{name}-dump"));
    froe(&[
        "index",
        "dump",
        store.to_str().expect("utf-8"),
        "--output",
        dump.to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
    ]);
    (store, dump.join("index-dumps"))
}

/// The fulltext query the imported index has to answer.
///
/// Restricted to the five interop pages, whose `jcr:title` values the
/// fixture's default definition indexes without Tika. Plan 0010's variant
/// definition does not cover them, so this stays a question about *this*
/// index.
const FULLTEXT_QUERY: &str = "SELECT [jcr:path] FROM [nt:base] WHERE \
     ISDESCENDANTNODE([/content/interop/pages]) AND CONTAINS(*, 'Page')";

/// How many rows the pristine store answers with.
///
/// Asserted rather than merely recorded: a comparison against an empty
/// answer would pass on a store whose index answers nothing at all.
const EXPECTED_FULLTEXT_ROWS: usize = 5;

/// Boots Oak on `store` and asks it what it can see through the index.
///
/// Returns the sorted rows and the plan. The probe is installed after the
/// import rather than before, because the query is over content the
/// fixture already holds: what is under test is whether Oak resolves that
/// content *through the index froe installed*, and the plan is what says
/// so.
fn ask_oak_through_the_index(store: &Path, label: &str, phase: &str) -> (Vec<String>, Vec<String>) {
    let volume = PodmanVolume::new(&format!("froe-interop-{label}"));
    let container = format!("froe-{label}");

    // A throwaway boot first, so Sling's own bundle installation is not
    // part of the store under test — the same bootstrap every phase that
    // boots on a froe-written store uses.
    let bootstrap =
        PodmanContainer::run_detached(&format!("{container}-bootstrap"), 8093, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(store, &volume.name);

    let sling = PodmanContainer::run_detached(&container, 8093, &volume.name);
    wait_for_sling(8093, &container);

    assert_oak_consumed_store_as_written(&container, phase);
    assert_oak_did_not_reindex(&container, phase);
    assert_oak_reported_no_index_failure(&container, phase);

    sling::sling_install_query_probe(8093);
    assert_eq!(
        sling::sling_query(8093, "SELECT * FROM [rep:root]"),
        vec!["/".to_owned()],
        "{phase}: the query probe does not answer, so every comparison built on it is vacuous"
    );

    let mut rows = sling::sling_query(8093, FULLTEXT_QUERY);
    rows.sort();
    let plan = sling::sling_query(8093, &format!("EXPLAIN {FULLTEXT_QUERY}"));

    // Read after the queries: a failure raised while answering them is
    // exactly the failure this is looking for.
    assert_oak_reported_no_index_failure(&container, phase);
    drop(sling);
    (rows, plan)
}

/// Oak boots on the imported store, logs no failure, and answers the
/// fulltext query through the index froe installed.
fn assert_oak_queries_through_the_imported_index(work: &Path) {
    eprintln!("  Oak answers through the imported index");

    let pristine = work.join("oak-pristine");
    copy_store(&oak_store(), &pristine);
    let (expected_rows, expected_plan) =
        ask_oak_through_the_index(&pristine, "lucene-pristine", "lucene_import pristine");
    assert_eq!(
        expected_rows.len(),
        EXPECTED_FULLTEXT_ROWS,
        "the pristine store answers {} rows rather than {EXPECTED_FULLTEXT_ROWS}, so the \
         comparison below would compare nothing",
        expected_rows.len()
    );
    assert!(
        expected_plan
            .iter()
            .any(|line| line.contains("lucene:lucene")),
        "the pristine store does not answer this query through the lucene index, so the \
         plan comparison below would prove nothing: {expected_plan:?}"
    );
    eprintln!(
        "    pristine: {} rows through lucene:lucene",
        expected_rows.len()
    );

    let imported = work.join("round-trip");
    let (rows, plan) =
        ask_oak_through_the_index(&imported, "lucene-imported", "lucene_import imported");
    assert_eq!(
        rows, expected_rows,
        "the imported index answers different rows than the index it replaced"
    );
    assert!(
        plan.iter().any(|line| line.contains("lucene:lucene")),
        "Oak did not use the imported index to answer the query: {plan:?}"
    );
    eprintln!(
        "    imported: the same {} rows, through lucene:lucene",
        rows.len()
    );
}

/// The lane the fixture's Lucene definition indexes on.
const LANE: &str = "async";

/// The checkpoint `/:async/<lane>` names, which is the only state an
/// asynchronous definition's lane can resume from.
fn lane_checkpoint(store: &Path) -> String {
    let repository = froe::Repository::open(store).expect("open the store");
    let lanes = repository
        .node_at_path("/:async")
        .expect("resolve /:async")
        .expect("the fixture has a lane state");
    let property = lanes
        .property(LANE)
        .expect("read the lane property")
        .unwrap_or_else(|| panic!("/:async has no {LANE} property"));
    match &property.values {
        froe::PropertyValues::Single(value) => {
            value.as_text().expect("the lane names a checkpoint")
        }
        froe::PropertyValues::Multiple(_) => panic!("/:async/{LANE} is multi-valued"),
    }
}

/// The second direction: an index Oak built out of band, imported by froe.
///
/// The definition is flagged `corrupt` on the copy's head first, because
/// that is the standard reason to reach for an out-of-band build and it is
/// what makes the drift comparison's tolerance load-bearing: the judge
/// builds from the lane checkpoint's state, which predates the flag, so its
/// definitions file lacks `corrupt` regardless — the acceptance that a
/// symmetric ignore set could not express.
fn import_an_index_oak_built_out_of_band(work: &Path) {
    eprintln!("  an index Oak built out of band");
    let judge = Judge::compile();

    let store = work.join("out-of-band");
    copy_store(&oak_store(), &store);
    let original_count = reindex_count(&store);
    let checkpoint = lane_checkpoint(&store);
    eprintln!("    lane {LANE} resumes from {checkpoint}");

    edit_definitions(
        &store,
        &[BookkeepingReset {
            name: "lucene",
            reindex_count: original_count,
        }],
        DefinitionEdit {
            flag_corrupt: true,
            ..DefinitionEdit::default()
        },
    );
    assert!(
        definition_digest(&store).contains("corrupt=Date:"),
        "the copy's definition must carry the corrupt flag the import has to clear"
    );

    let built = work.join("out-of-band-build");
    std::fs::create_dir_all(&built).expect("create the build directory");
    judge.run(
        "OutOfBandBuild",
        &["/store", IMPORTED_DEFINITION, &checkpoint, "/out"],
        vec![
            Mount::read_only(&store, "/store"),
            Mount::writable(&built, "/out"),
        ],
    );

    let baseline = digest_store(&store);
    let checkpoints_before = checkpoint_names(&store);
    froe(&[
        "index",
        "import",
        store.to_str().expect("utf-8"),
        "--input",
        built.join("index-dumps").to_str().expect("utf-8"),
        "--index",
        IMPORTED_DEFINITION,
        "--yes",
    ]);

    // oak-run's own import ends two above the original: the copy's reindex
    // took it one above, and the import's own increment takes it one more.
    assert_eq!(
        reindex_count(&store),
        original_count + 2,
        "an oak-run-shaped build's file carries the copy's count, and the import adds one"
    );
    let after = definition_digest(&store);
    assert!(
        !after.contains("corrupt="),
        "the import must clear the corrupt flag: {after}"
    );
    assert!(
        checkpoint_names(&store).contains(&checkpoint),
        "the lane's own checkpoint must survive the import"
    );
    assert_eq!(
        checkpoint_names(&store),
        checkpoints_before,
        "an import releases no checkpoint"
    );
    assert_digest_delta(
        &baseline,
        &digest_store(&store),
        ExpectedDigestDelta::Subtrees(IMPORTED_SUBTREE),
        "lucene_import out-of-band",
    );
    froe(&["check", store.to_str().expect("utf-8")]);
    froe(&["index", "check", store.to_str().expect("utf-8")]);

    eprintln!("    judge: consistency level 2 over the imported index");
    let checker_work = work.join("out-of-band-consistency");
    std::fs::create_dir_all(&checker_work).expect("create the checker's work directory");
    let output = judge.run(
        "Consistency",
        &[
            "/store",
            IMPORTED_DEFINITION,
            FULL_CONSISTENCY_LEVEL,
            "/work",
        ],
        vec![
            Mount::read_only(&store, "/store"),
            Mount::writable(&checker_work, "/work"),
        ],
    );
    assert!(
        output.contains("clean=true") && output.contains("indexCheckStatus=clean"),
        "Oak's own checker must pass at its full level over the imported index:\n{output}"
    );
    eprintln!(
        "    imported, reindexCount {} -> {}",
        original_count,
        original_count + 2
    );
}
