//! `froe compact` followed by `froe cleanup` must leave no reclaimable
//! garbage behind.
//!
//! The store shape these tests use is the one from the field report that
//! motivated them: an archive holding a large binary — whose bulk segments
//! compaction references where they lie rather than copying — beside data
//! segments that die when the head moves on. Removing those dead segments
//! frees far less than a quarter of the file, so Apache Oak's `TarReader`
//! savings heuristic declines the rewrite; both froe's compaction reclaim
//! pass and its standalone cleanup used to apply that heuristic, which made
//! the garbage unreclaimable by any command, on every run, forever.
//!
//! The assertions deliberately do not ask the cleanup planner whether it is
//! satisfied with its own work. [`segments_unreachable_from_the_journal`] is
//! a reachability oracle written from the storage format alone: it reads
//! `journal.log`, walks segment header reference tables, and reports what
//! nothing in the journal reaches. It never consults a generation triple, an
//! archive index, a graph trailer, or any part of the planner it judges.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use froe::writer::{
    ChildNodesToWrite, CompactionKind, CompactionOptions, PropertyToWrite, PropertyValuesToWrite,
    WritableRepository, compact, plan_compaction,
};
use froe::{
    GarbageCollectionGeneration, PropertyType, RecordIdentifier, Repository, SegmentIdentifier,
    SegmentProvider,
};

/// A scratch repository directory, removed when the test drops it.
struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-reclamation-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test repository directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Segments on disk that nothing in `journal.log` reaches.
///
/// Written here rather than asked of the planner, so the verdict is
/// independent of the code it judges: this resolves every journal head, walks
/// `referenced_segments` breadth-first out of the parsed segment headers the
/// way an auditor holding only the format description would, and reports the
/// active segments the walk never arrived at.
fn segments_unreachable_from_the_journal(directory: &Path) -> Vec<SegmentIdentifier> {
    let repository = Repository::open(directory).expect("open the store for auditing");
    let roots: Vec<SegmentIdentifier> = repository
        .journal_entries()
        .iter()
        .filter_map(|entry| entry.record_identifier().map(|record| record.segment))
        .collect();
    assert!(
        !roots.is_empty(),
        "a store with no journal head cannot be audited for reachability"
    );

    let mut reached: HashSet<SegmentIdentifier> = HashSet::new();
    let mut pending: VecDeque<SegmentIdentifier> = roots.into_iter().collect();
    while let Some(identifier) = pending.pop_front() {
        if !reached.insert(identifier) {
            continue;
        }
        let segment = repository
            .segment(identifier)
            .expect("a journal-reachable segment must be readable");
        pending.extend(segment.structure.referenced_segments.iter().copied());
    }

    let mut unreachable: Vec<SegmentIdentifier> = repository
        .archives()
        .iter()
        .flat_map(froe::tar_archive::TarArchiveReader::segment_identifiers)
        .filter(|identifier| !reached.contains(identifier))
        .collect();
    unreachable.sort_by_key(|identifier| {
        (
            identifier.most_significant_bits,
            identifier.least_significant_bits,
        )
    });
    unreachable.dedup();
    unreachable
}

/// The field-report shape: one archive whose live bulk segments dwarf the
/// dead data segments beside them, so the savings heuristic can never be
/// satisfied for it. Returns the published head.
fn write_binary_heavy_churned_store(directory: &Path) -> RecordIdentifier {
    let store = WritableRepository::open(directory).expect("bootstrap the store");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    // Sixteen mebibytes of binary content. Compaction re-links the bulk
    // segments holding it instead of copying them, which is what keeps this
    // archive alive — and its dead data segments with it.
    let binary = writer
        .write_binary_content(&vec![7u8; 16 * 1024 * 1024])
        .expect("write the binary content");
    let asset = writer
        .write_node(
            Some("nt:file"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(binary),
            }],
        )
        .expect("write the asset node");

    // Churn in the same session, so the dead data segments share the archive
    // with the live bulk segments. Only the last revision is published.
    let mut last_head = None;
    for revision in 0..400 {
        let text = writer
            .write_string(&format!("revision-{revision}").repeat(500))
            .expect("write the revision text");
        let churn = writer
            .write_node(
                Some("nt:unstructured"),
                &[],
                &ChildNodesToWrite::Zero,
                &[PropertyToWrite {
                    name: "data".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::Single(text),
                }],
            )
            .expect("write the churn node");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Many(vec![
                    ("asset".to_owned(), asset),
                    ("churn".to_owned(), churn),
                ]),
                &[],
            )
            .expect("write the content root");
        last_head = Some(
            writer
                .write_node(
                    None,
                    &[],
                    &ChildNodesToWrite::One {
                        name: "root".to_owned(),
                        node: root,
                    },
                    &[],
                )
                .expect("write the super root"),
        );
    }
    writer.finish().expect("finish the writer");
    let head = last_head.expect("at least one revision was written");
    assert!(store.compare_and_set_head(store.head(), head));
    store.flush().expect("flush the store");
    store.close().expect("close the store");
    head
}

/// Names of the `data*.tar` files currently in the directory.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the segment store matches \".tar\" case-sensitively, exactly as Oak does"
)]
fn archive_file_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .expect("list the repository directory")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("data") && name.ends_with(".tar"))
        .collect();
    names.sort();
    names
}

#[test]
fn compact_then_cleanup_leaves_nothing_unreachable_on_a_binary_heavy_store() {
    let directory = TestDirectory::new("binary-heavy");
    write_binary_heavy_churned_store(&directory.path);

    let before = archive_file_names(&directory.path);
    assert_eq!(
        before,
        vec!["data00000a.tar".to_owned()],
        "the fixture puts the bulk segments and the churn in one archive"
    );
    assert!(
        !segments_unreachable_from_the_journal(&directory.path).is_empty(),
        "the fixture must actually contain garbage before it is compacted"
    );

    compact(
        &directory.path,
        CompactionOptions::default().with_compaction(CompactionKind::Full),
    )
    .expect("compact the store");

    assert!(
        archive_file_names(&directory.path).contains(&"data00000b.tar".to_owned()),
        "the archive the savings heuristic declined must be rewritten: {:?}",
        archive_file_names(&directory.path)
    );

    let plan =
        plan_compaction(&directory.path, &CompactionOptions::default()).expect("plan a cleanup");
    assert_eq!(
        plan.retained_reclaimable_segments(),
        0,
        "compaction must not leave identified garbage for cleanup to decline: {:?}",
        plan.warnings()
    );
    compact(
        &directory.path,
        CompactionOptions::default().with_compaction(CompactionKind::Full),
    )
    .expect("apply the maintenance run");

    let unreachable = segments_unreachable_from_the_journal(&directory.path);
    assert!(
        unreachable.is_empty(),
        "compact then cleanup must leave no segment the journal cannot reach, found {}: {:?}",
        unreachable.len(),
        unreachable
    );

    // The content survived all of it.
    let repository = Repository::open(&directory.path).expect("reopen the cleaned store");
    let asset = repository
        .node_at_path("/asset")
        .expect("resolve the asset")
        .expect("the asset is still present");
    let data = asset
        .property("jcr:data")
        .expect("read the binary property")
        .expect("the binary property is still present");
    assert!(
        matches!(data.values, froe::PropertyValues::Single(_)),
        "the shared binary survives the reclamation"
    );
}

#[test]
fn repeated_compaction_and_cleanup_never_accumulates_unreachable_segments() {
    let directory = TestDirectory::new("repeated-rounds");
    write_binary_heavy_churned_store(&directory.path);

    // A checkpoint makes the super-root reach a second snapshot root, so the
    // mark phase's protected set participates rather than being trivially
    // empty.
    {
        let store = WritableRepository::open(&directory.path).expect("open for a checkpoint");
        froe::writer::create_checkpoint(&store, 60 * 60 * 1000, &[])
            .expect("create the checkpoint");
        store.close().expect("close after the checkpoint");
    }

    // Three rounds, because the residue this fixes was cumulative: each round
    // used to add a generation of garbage no later run could remove.
    for round in 0..3 {
        compact(
            &directory.path,
            CompactionOptions::default().with_compaction(CompactionKind::Full),
        )
        .expect("compact the store");

        compact(
            &directory.path,
            CompactionOptions::default().with_compaction(CompactionKind::Full),
        )
        .expect("apply the maintenance run");

        let unreachable = segments_unreachable_from_the_journal(&directory.path);
        assert!(
            unreachable.is_empty(),
            "round {round} left {} segments the journal cannot reach: {unreachable:?}",
            unreachable.len()
        );
    }
}

/// Head safety at one retained generation follows from the reference guard,
/// not from the predicate.
///
/// At two retained generations a head one generation ahead of the data it
/// reaches was spared by arithmetic. At one it is not, so the only thing
/// standing between a head and its own segments is
/// `validate_reclaim_reference_invariant` — which must refuse, and must refuse
/// before anything on disk moves.
/// The merged run: one command, one lock, one head move — the copy into a
/// fresh generation *and* every reclamation task, with nothing left for a
/// second command to do.
#[test]
fn one_merged_run_compacts_and_reclaims_in_a_single_pass() {
    let directory = TestDirectory::new("merged-run");
    write_binary_heavy_churned_store(&directory.path);
    assert!(
        !segments_unreachable_from_the_journal(&directory.path).is_empty(),
        "the fixture carries garbage before the run"
    );

    let options = CompactionOptions::default().with_compaction(CompactionKind::Full);
    let outcome = compact(&directory.path, options).expect("the merged run applies");

    let compacted = outcome
        .compacted
        .expect("a merged run reports the generation it copied into");
    assert!(compacted.nodes > 0, "the copy rewrote the live tree");
    assert!(
        compacted.generation.is_compacted,
        "and wrote it into a compacted generation"
    );
    assert_ne!(
        outcome.head_after, outcome.head_before,
        "the head moved exactly once, to the copy"
    );

    // The archive the savings heuristic used to decline is rewritten by the
    // same run that copied the head.
    assert!(
        archive_file_names(&directory.path).contains(&"data00000b.tar".to_owned()),
        "the sub-gate archive was rewritten: {:?}",
        archive_file_names(&directory.path)
    );

    let unreachable = segments_unreachable_from_the_journal(&directory.path);
    assert!(
        unreachable.is_empty(),
        "one run leaves nothing the journal cannot reach, found {}: {unreachable:?}",
        unreachable.len()
    );

    // `gc.log` records the completed cycle, which is what lets a later Oak
    // tail compaction find its predecessor.
    let garbage_collection_log =
        std::fs::read_to_string(directory.path.join("gc.log")).expect("read gc.log");
    assert_eq!(
        garbage_collection_log.lines().count(),
        1,
        "exactly one cycle was recorded: {garbage_collection_log}"
    );

    // And the content survived all of it.
    let repository = Repository::open(&directory.path).expect("reopen the merged store");
    assert!(
        repository
            .node_at_path("/asset")
            .expect("resolve the asset")
            .is_some(),
        "the content survives the merged run"
    );
}

/// A run killed between finishing its copy and committing the head leaves
/// archives stamped ahead of the head. Nothing in the ordinary rules removes
/// them — compaction's own mark disables the dangling-future rule, and the
/// generation predicate spares a segment newer than its reference — so without
/// an explicit retirement each killed run would leave one more orphan
/// generation behind forever.
/// The plan states which archives the run will remove and which it will
/// rewrite. That claim is only worth making if it is exact, and it is exact
/// only while `predict_shared_bulk_segments` agrees with `copy_binary_value`
/// about which binary content the copy shares in place.
///
/// The fixture carries both shapes, because they diverge in opposite
/// directions: a long `Binary` property, whose bulk segments the copy
/// references where they lie and which therefore survive, and a long `String`
/// property, whose blocks are re-encoded so its old bulk segments die.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the fixture, the predicted dispositions and the performed ones belong in one place: the assertion is that they match"
)]
fn a_dry_run_plan_predicts_exactly_the_archives_the_run_sweeps() {
    let directory = TestDirectory::new("predicted-sweep");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);

        let binary = writer
            .write_binary_content(&vec![3u8; 2 * 1024 * 1024])
            .expect("write the shared binary");
        // Long enough to land in bulk segments, and a String, so the copy
        // re-encodes it instead of sharing it.
        let text = writer
            .write_string(&"re-encoded-value ".repeat(200_000))
            .expect("write the re-encoded text");
        let asset = writer
            .write_node(
                Some("nt:file"),
                &[],
                &ChildNodesToWrite::Zero,
                &[
                    PropertyToWrite {
                        name: "jcr:data".to_owned(),
                        property_type: PropertyType::Binary,
                        values: PropertyValuesToWrite::Single(binary),
                    },
                    PropertyToWrite {
                        name: "text".to_owned(),
                        property_type: PropertyType::String,
                        values: PropertyValuesToWrite::Single(text),
                    },
                ],
            )
            .expect("write the asset");
        let root = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "asset".to_owned(),
                    node: asset,
                },
                &[],
            )
            .expect("write the content root");
        let head = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: root,
                },
                &[],
            )
            .expect("write the super root");
        writer.finish().expect("finish");
        assert!(store.compare_and_set_head(store.head(), head));
        store.flush().expect("flush");
        store.close().expect("close");
    }

    let options = CompactionOptions::default().with_compaction(CompactionKind::Full);
    let plan = plan_compaction(&directory.path, &options).expect("plan the run");

    // What the plan says it will do to the archives.
    let mut predicted: Vec<String> = plan
        .actions()
        .iter()
        .filter_map(|action| match action {
            froe::CompactionAction::RemoveReclaimableArchive { file_name, .. } => {
                Some(format!("remove {file_name}"))
            }
            froe::CompactionAction::RewriteArchive {
                file_name,
                replacement_name,
                ..
            } => Some(format!("rewrite {file_name} as {replacement_name}")),
            _ => None,
        })
        .collect();
    predicted.sort();
    assert!(
        !predicted.is_empty(),
        "the plan predicts an archive-level sweep: {:?}",
        plan.actions()
    );

    let before = archive_file_names(&directory.path);
    compact(&directory.path, options).expect("apply the run");
    let after = archive_file_names(&directory.path);

    // What the run actually did to them.
    let mut performed: Vec<String> = Vec::new();
    for name in &before {
        if after.contains(name) {
            continue;
        }
        let successor = {
            let bytes = name.as_bytes();
            let letter = bytes[name.len() - 5];
            format!(
                "data{}{}.tar",
                &name[4..name.len() - 5],
                (letter + 1) as char
            )
        };
        if after.contains(&successor) {
            performed.push(format!("rewrite {name} as {successor}"));
        } else {
            performed.push(format!("remove {name}"));
        }
    }
    performed.sort();

    assert_eq!(
        predicted, performed,
        "the predicted sweep is exactly the sweep the run performed"
    );
    assert!(
        segments_unreachable_from_the_journal(&directory.path).is_empty(),
        "and the run still leaves nothing unreachable"
    );
}

#[test]
fn an_interrupted_compaction_is_retired_by_the_next_run() {
    let directory = TestDirectory::new("interrupted-residue");
    write_binary_heavy_churned_store(&directory.path);
    let archives_before = archive_file_names(&directory.path);

    // Exactly the state a kill after `writer.finish()` and before `compare_and_set_head`
    // produces: a complete compacted generation on disk that no head names.
    {
        let store = WritableRepository::open(&directory.path).expect("open for the abandoned copy");
        let head = store.head();
        let abandoned = GarbageCollectionGeneration {
            generation: 9,
            full_generation: 9,
            is_compacted: true,
        };
        let mut writer = store.record_writer(abandoned);
        froe::writer::deep_copy_tree(&store, &mut writer, head)
            .expect("copy the head into the abandoned generation");
        writer.finish().expect("finish the abandoned copy");
        // Deliberately no compare_and_set_head: this is the interruption.
        store.close().expect("close after the abandoned copy");
    }
    let archives_with_residue = archive_file_names(&directory.path);
    assert!(
        archives_with_residue.len() > archives_before.len(),
        "the abandoned copy left archives behind: {archives_with_residue:?}"
    );

    let plan = plan_compaction(
        &directory.path,
        &CompactionOptions::default().with_compaction(CompactionKind::Full),
    )
    .expect("plan a run against a store carrying residue");
    assert!(
        plan.actions().iter().any(|action| matches!(
            action,
            froe::CompactionAction::RetireInterruptedCompactionResidue { segments } if *segments > 0
        )),
        "the plan names the residue it will retire: {:?}",
        plan.actions()
    );

    compact(
        &directory.path,
        CompactionOptions::default().with_compaction(CompactionKind::Full),
    )
    .expect("the next run heals the store");

    let unreachable = segments_unreachable_from_the_journal(&directory.path);
    assert!(
        unreachable.is_empty(),
        "the residue is gone and nothing else was stranded: {unreachable:?}"
    );

    // And a second run does not grow the store: retirement converges.
    let after_first = archive_file_names(&directory.path);
    compact(
        &directory.path,
        CompactionOptions::default().with_compaction(CompactionKind::Full),
    )
    .expect("a second run");
    assert!(
        archive_file_names(&directory.path).len() <= after_first.len(),
        "repeated runs converge rather than accumulate"
    );
}

#[test]
fn a_head_reaching_a_reclaimable_generation_is_refused_without_mutation() {
    let directory = TestDirectory::new("reclaimable-head-refusal");
    {
        let store = WritableRepository::open(&directory.path).expect("bootstrap the store");

        // Content one generation behind the head that reaches it.
        let mut older = store.record_writer(GarbageCollectionGeneration {
            generation: 1,
            full_generation: 1,
            is_compacted: false,
        });
        let content_root = older
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("write the older content root");
        older.finish().expect("finish the older generation");

        let mut newer = store.record_writer(GarbageCollectionGeneration {
            generation: 2,
            full_generation: 2,
            is_compacted: false,
        });
        let head = newer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::One {
                    name: "root".to_owned(),
                    node: content_root,
                },
                &[],
            )
            .expect("write the super root");
        newer.finish().expect("finish the newer generation");
        assert!(store.compare_and_set_head(store.head(), head));
        store.flush().expect("flush");
        store.close().expect("close");
    }

    let before = archive_file_names(&directory.path);
    let unreachable_before = segments_unreachable_from_the_journal(&directory.path);

    let error = plan_compaction(&directory.path, &CompactionOptions::default())
        .expect_err("a head reaching a reclaimable generation must be refused");
    assert!(
        error
            .to_string()
            .contains("current head reaches data segment"),
        "the refusal names what it protected: {error}"
    );

    assert_eq!(
        archive_file_names(&directory.path),
        before,
        "a refusal moves nothing on disk"
    );
    assert_eq!(
        segments_unreachable_from_the_journal(&directory.path),
        unreachable_before,
        "and reclaims nothing"
    );
    Repository::open(&directory.path).expect("the refused store is still healthy");
}

#[test]
fn a_generation_z_archive_reports_the_residue_it_cannot_reclaim() {
    let directory = TestDirectory::new("generation-z");
    write_binary_heavy_churned_store(&directory.path);

    // Rename the only archive to the last name the `a`-`z` namespace has.
    // Nothing can rewrite it, which is a format limit rather than a policy,
    // so the run must say so rather than silently leave the garbage.
    std::fs::rename(
        directory.path.join("data00000a.tar"),
        directory.path.join("data00000z.tar"),
    )
    .expect("rename the archive to the last generation");

    for _ in 0..2 {
        compact(
            &directory.path,
            CompactionOptions::default().with_compaction(CompactionKind::Full),
        )
        .expect("compact the store");
    }

    let plan =
        plan_compaction(&directory.path, &CompactionOptions::default()).expect("plan a cleanup");
    assert!(
        plan.warnings()
            .iter()
            .any(|warning| warning.contains("data00000z.tar")
                && warning.contains("generation z cannot be rewritten")),
        "the unreclaimable residue must be named, not hidden: {:?}",
        plan.warnings()
    );
    assert!(
        plan.retained_reclaimable_segments() != 0,
        "the residue a format limit forces must still be counted"
    );
}
