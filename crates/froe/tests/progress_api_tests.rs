//! Downstream-facing regressions for progress observation.
//!
//! Two properties are load-bearing and are asserted here from *outside*
//! the crate, the way a downstream caller sees them:
//!
//! * **Observation is inert.** An observed operation returns exactly what
//!   the unobserved one returns — the same plan, the same outcome, the
//!   same repository on disk. An observer is told what happened; it never
//!   decides anything. Neutralizing this — letting a hook change which
//!   items an operation visits — makes
//!   [`an_observed_plan_equals_an_unobserved_one`] and
//!   [`an_observed_cleanup_equals_an_unobserved_one`] fail.
//! * **The reported sequence is well formed.** Every advance falls inside
//!   a begin/end pair, counts never decrease, and a count never overshoots
//!   its declared total, so a renderer can trust them without defending
//!   itself. [`ObservationLog`] checks all three on every call, and every
//!   test below runs one through a real operation.

use froe::progress::{ProgressObserver, Step, WorkUnit};
use froe::store::Repository;
use froe::writer::record_writer::ChildNodesToWrite;
use froe::writer::store_writer::WritableRepository;
use froe::{
    CompactionOptions, PreparedCompaction, compact, compact_with_progress, plan_compaction,
};

/// A temporary directory removed when the test ends.
struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let unique = format!(
            "froe-progress-api-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One reported call, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reported {
    Began {
        description: String,
        unit: WorkUnit,
        total: Option<u64>,
    },
    Advanced(u64),
    TotalResolved(u64),
    Ended,
}

/// Records every call and asserts the sequence's invariants as it goes, so
/// a malformed report fails at the call that broke it rather than at some
/// later assertion.
#[derive(Default)]
struct ObservationLog {
    calls: Vec<Reported>,
    active: Option<(String, Option<u64>)>,
    last_count: u64,
}

impl ObservationLog {
    fn descriptions(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Reported::Began { description, .. } => Some(description.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The highest count reported within the step named `description`.
    fn highest_count_of(&self, description: &str) -> Option<u64> {
        let mut within = false;
        let mut highest = None;
        for call in &self.calls {
            match call {
                Reported::Began {
                    description: began, ..
                } => within = began == description,
                Reported::Ended => within = false,
                Reported::Advanced(count) if within => {
                    highest = Some(highest.map_or(*count, |previous: u64| previous.max(*count)));
                }
                _ => {}
            }
        }
        highest
    }

    fn began_and_ended_in_pairs(&self) -> bool {
        let mut open = false;
        for call in &self.calls {
            match call {
                Reported::Began { .. } => open = true,
                Reported::Ended => open = false,
                _ => {}
            }
        }
        !open
    }
}

impl ProgressObserver for ObservationLog {
    fn step_began(&mut self, step: &Step<'_>) {
        assert!(
            !step.description().is_empty(),
            "every step names the work it is doing"
        );
        self.active = Some((step.description().to_owned(), step.total()));
        self.last_count = 0;
        self.calls.push(Reported::Began {
            description: step.description().to_owned(),
            unit: step.unit(),
            total: step.total(),
        });
    }

    fn step_advanced(&mut self, completed: u64) {
        let (description, total) = self
            .active
            .as_ref()
            .expect("an advance outside a step has nothing to advance");
        assert!(
            completed >= self.last_count,
            "{description}: counts must not run backwards ({completed} after {})",
            self.last_count
        );
        if let Some(total) = total {
            assert!(
                completed <= *total,
                "{description}: counted {completed} of a declared {total}"
            );
        }
        self.last_count = completed;
        self.calls.push(Reported::Advanced(completed));
    }

    fn step_total_resolved(&mut self, total: u64) {
        let (description, _) = self
            .active
            .as_ref()
            .expect("a total outside a step belongs to nothing");
        assert!(
            total >= self.last_count,
            "{description}: a resolved total of {total} is below the {} already counted",
            self.last_count
        );
        if let Some(active) = self.active.as_mut() {
            active.1 = Some(total);
        }
        self.calls.push(Reported::TotalResolved(total));
    }

    fn step_ended(&mut self) {
        self.active = None;
        self.calls.push(Reported::Ended);
    }
}

/// Writes a second super-root over the first one's `root` child, sharing
/// every record below it. Returns the new super-root's identifier.
fn write_sharing_super_root(
    directory: &std::path::Path,
    head: froe::RecordIdentifier,
) -> froe::RecordIdentifier {
    let existing_root = {
        let repository = Repository::open(directory).expect("open to read the head");
        froe::NodeState::new(&repository, head)
            .child_node("root")
            .expect("read the head")
            .expect("the head has a content root")
            .record_identifier()
    };
    let store = WritableRepository::open(directory).expect("open the store directory");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);
    let checkpoints = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("write a fresh checkpoints container");
    let super_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("root".to_owned(), existing_root),
                ("checkpoints".to_owned(), checkpoints),
            ]),
            &[],
        )
        .expect("write the sharing super-root");
    writer.finish().expect("finish the writer");
    let previous = store.head();
    assert!(store.set_head(previous, super_root), "advance the head");
    store.close().expect("close the store");
    super_root
}

/// Writes a small repository: a content root with `children` leaves.
fn build_repository(directory: &std::path::Path, children: usize) {
    let store = WritableRepository::open(directory).expect("open the store directory");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);
    let mut entries = Vec::with_capacity(children);
    for index in 0..children {
        let leaf = writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("write a leaf node");
        entries.push((format!("page-{index}"), leaf));
    }
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Many(entries),
            &[],
        )
        .expect("write the content node");
    let root = writer
        .write_node(
            Some("rep:root"),
            &[],
            &ChildNodesToWrite::One {
                name: "content".to_owned(),
                node: content,
            },
            &[],
        )
        .expect("write the content root");
    let checkpoints = writer
        .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
        .expect("write the checkpoints container");
    let super_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("root".to_owned(), root),
                ("checkpoints".to_owned(), checkpoints),
            ]),
            &[],
        )
        .expect("write the super-root");
    writer.finish().expect("finish the writer");
    let previous = store.head();
    assert!(store.set_head(previous, super_root), "advance the head");
    store.close().expect("close the store");
}

#[test]
fn a_downstream_observer_sees_the_archive_scan_of_every_open() {
    let directory = TestDirectory::new("open");
    build_repository(directory.path(), 4);

    let mut log = ObservationLog::default();
    let repository =
        Repository::open_with_progress(directory.path(), &mut log).expect("open the repository");
    assert_eq!(repository.archives().len(), 1);
    assert_eq!(
        log.descriptions(),
        ["opening archives"],
        "opening reports exactly one step"
    );
    assert!(log.began_and_ended_in_pairs());
    assert_eq!(
        log.highest_count_of("opening archives"),
        Some(1),
        "the step counts up to the archive it opened"
    );
    let Reported::Began { unit, total, .. } = &log.calls[0] else {
        panic!("the first call begins the step");
    };
    assert_eq!(*unit, WorkUnit::Archives);
    assert_eq!(*total, Some(1), "the archive count is known in advance");
}

#[test]
fn an_observed_plan_equals_an_unobserved_one() {
    let directory = TestDirectory::new("plan");
    build_repository(directory.path(), 8);

    let unobserved =
        plan_compaction(directory.path(), &CompactionOptions::default()).expect("plan");
    let mut log = ObservationLog::default();
    let observed = froe::plan_compaction_with_progress(
        directory.path(),
        &CompactionOptions::default(),
        &mut log,
    )
    .expect("plan while observed");

    assert_eq!(
        observed, unobserved,
        "observation must not change the plan an operator confirms"
    );
    assert!(log.began_and_ended_in_pairs());
    for required in [
        "opening archives",
        "verifying the current head",
        "analyzing journal revisions",
    ] {
        assert!(
            log.descriptions().contains(&required),
            "planning must report {required:?}: {:?}",
            log.descriptions()
        );
    }
    assert!(
        log.highest_count_of("verifying the current head")
            .is_some_and(|nodes| nodes >= 8),
        "the head verification counts the nodes it resolved: {:?}",
        log.highest_count_of("verifying the current head")
    );
}

#[test]
fn an_observed_cleanup_equals_an_unobserved_one() {
    let unobserved_directory = TestDirectory::new("apply-plain");
    let observed_directory = TestDirectory::new("apply-observed");
    build_repository(unobserved_directory.path(), 6);
    build_repository(observed_directory.path(), 6);

    let unobserved =
        compact(unobserved_directory.path(), CompactionOptions::default()).expect("clean up");
    let mut log = ObservationLog::default();
    let observed = compact_with_progress(
        observed_directory.path(),
        CompactionOptions::default(),
        &mut log,
    )
    .expect("clean up while observed");

    assert_eq!(
        observed.removed_checkpoints, unobserved.removed_checkpoints,
        "observation must not change what a cleanup removes"
    );
    assert_eq!(
        observed.removed_journal_lines,
        unobserved.removed_journal_lines
    );
    assert_eq!(observed.removed_segments(), unobserved.removed_segments());
    assert_eq!(
        observed.rewritten_archives, unobserved.rewritten_archives,
        "observation must not change which archives are rewritten"
    );
    assert_eq!(
        observed.removed_reclaimable_archives,
        unobserved.removed_reclaimable_archives
    );
    assert_eq!(
        observed.removed_stale_archives,
        unobserved.removed_stale_archives
    );
    assert_eq!(observed.is_complete(), unobserved.is_complete());
    assert_eq!(
        observed.deletion_failures().len(),
        unobserved.deletion_failures().len()
    );
    assert!(log.began_and_ended_in_pairs());

    // Both repositories must still open and resolve the same shape.
    let unobserved_repository =
        Repository::open(unobserved_directory.path()).expect("reopen the unobserved store");
    let observed_repository =
        Repository::open(observed_directory.path()).expect("reopen the observed store");
    assert_eq!(
        unobserved_repository.archives().len(),
        observed_repository.archives().len(),
        "observation must not change the archives left on disk"
    );
    assert_eq!(
        unobserved_repository.segment_count(),
        observed_repository.segment_count(),
        "observation must not change the segments left on disk"
    );
}

#[test]
fn a_prepared_cleanup_reports_both_of_its_phases() {
    let directory = TestDirectory::new("prepared");
    build_repository(directory.path(), 4);

    let mut log = ObservationLog::default();
    let prepared = PreparedCompaction::prepare_with_progress(
        directory.path(),
        CompactionOptions::default(),
        &mut log,
    )
    .expect("prepare under the lock");
    let planned = log.descriptions().len();
    assert!(planned > 0, "the locked replan reports its steps");
    prepared
        .apply_with_progress(&mut log)
        .expect("apply the authoritative plan");
    assert!(
        log.descriptions().len() > planned,
        "applying reports steps of its own: {:?}",
        log.descriptions()
    );
    assert!(log.began_and_ended_in_pairs());
    // The final verification reopens the store and re-walks the head,
    // and each of those reports a step of its own.
    assert!(
        log.descriptions()
            .iter()
            .filter(|description| **description == "verifying the current head")
            .count()
            >= 2,
        "the head is verified again after applying: {:?}",
        log.descriptions()
    );
}

#[test]
fn every_observable_reader_reports_through_the_same_trait() {
    let directory = TestDirectory::new("readers");
    build_repository(directory.path(), 5);

    let mut log = ObservationLog::default();
    let report = froe::tooling::check_consistency_with_progress(
        directory.path(),
        &[],
        false,
        usize::MAX,
        &mut log,
    )
    .expect("check consistency");
    assert!(report.has_good_revision());
    // One step per revision, counting the nodes that revision resolves:
    // a single step over the whole loop could only count revisions, and a
    // healthy store pins every path at the first one, so it would sit at
    // zero for the entire run.
    let revision_step = log
        .descriptions()
        .into_iter()
        .find(|description| description.starts_with("checking revision "))
        .unwrap_or_else(|| panic!("check reports its revision walk: {:?}", log.descriptions()))
        .to_owned();
    assert_eq!(revision_step, "checking revision 1 of 1");
    assert!(
        log.highest_count_of(&revision_step)
            .is_some_and(|nodes| nodes >= 5),
        "the revision step counts the nodes it resolved, not a frozen revision count: {:?}",
        log.highest_count_of(&revision_step)
    );

    let mut log = ObservationLog::default();
    let outcome = froe::tooling::search_nodes_with_progress(
        directory.path(),
        &froe::tooling::SearchQuery {
            has_properties: vec!["jcr:primaryType".to_owned()],
            ..froe::tooling::SearchQuery::default()
        },
        0,
        &mut log,
    )
    .expect("search nodes");
    assert!(!outcome.matches.is_empty());
    assert!(
        log.descriptions().contains(&"searching segments"),
        "search reports its segment sweep: {:?}",
        log.descriptions()
    );

    let mut log = ObservationLog::default();
    let history = froe::tooling::node_history_with_progress(directory.path(), "/", &mut log)
        .expect("trace history");
    assert!(!history.is_empty());
    assert!(
        log.descriptions().contains(&"tracing revisions"),
        "history reports its revision walk: {:?}",
        log.descriptions()
    );

    let mut log = ObservationLog::default();
    let differences = froe::tooling::diff_revisions_with_progress(
        directory.path(),
        "head",
        "head",
        "/",
        &mut log,
    )
    .expect("compare revisions");
    assert!(
        differences.is_empty(),
        "a revision differs from itself in nothing"
    );
    assert!(
        log.descriptions().contains(&"comparing revisions"),
        "difference reports its comparison walk: {:?}",
        log.descriptions()
    );
}

/// Java counts every attempted journal entry and tests `--revisions` only
/// at the end of an iteration that did not skip, so an unresolvable line
/// can carry the count one past the limit. No reported label may advertise
/// that overshoot: a run bounded to two revisions must never say
/// "revision 3 of 2".
#[test]
fn a_bounded_check_never_labels_a_revision_past_its_bound() {
    let directory = TestDirectory::new("bounded-check");
    build_repository(directory.path(), 3);
    // A line the reader cannot resolve, between two it can.
    let journal_path = directory.path().join("journal.log");
    let journal = std::fs::read_to_string(&journal_path).expect("read the journal");
    let mut lines: Vec<String> = journal.lines().map(str::to_owned).collect();
    let newest = lines.last().cloned().expect("a journal line");
    lines.push("not-a-record root 1".to_owned());
    lines.push(newest);
    std::fs::write(&journal_path, format!("{}\n", lines.join("\n"))).expect("rewrite journal");

    let mut log = ObservationLog::default();
    froe::tooling::check_consistency_with_progress(
        directory.path(),
        &["/no/such/path".to_owned()],
        false,
        2,
        &mut log,
    )
    .expect("check");

    for description in log.descriptions() {
        let Some(position) = description.strip_prefix("checking revision ") else {
            continue;
        };
        let (position, bound) = position.split_once(" of ").expect("a labelled position");
        let position: usize = position.parse().expect("a numeric position");
        let bound: usize = bound.parse().expect("a numeric bound");
        assert!(
            position <= bound,
            "a bounded run advertised {description:?}, past its own bound"
        );
        assert_eq!(bound, 2, "the declared bound is the limit, not the journal");
    }
}

/// A step whose work spans several calls keeps one running total.
/// `NodeTreeVerifier` is used that way by the cleanup planner's
/// prospective-plan validation, once per retained journal root, and its
/// subtree cache makes every root after the first cheaper — so a counter
/// that restarted per call would report a smaller number than it just
/// did, breaking the cumulative contract `ProgressObserver` documents.
#[test]
fn a_verifier_reporting_several_roots_keeps_one_running_total() {
    let directory = TestDirectory::new("several-roots");
    build_repository(directory.path(), 6);
    let repository = Repository::open(directory.path()).expect("open the repository");
    let head = repository.head_record_identifier();

    // A second super-root reusing the first's content root: verifying it
    // walks only the two nodes above that root, every one below being
    // served from the verifier's cache. That is the shape a repository's
    // retained journal roots have, and the shape that exposes a counter
    // restarting — a few nodes reported after many.
    let second_head = write_sharing_super_root(directory.path(), head);
    let repository = Repository::open(directory.path()).expect("reopen the repository");

    let mut log = ObservationLog::default();
    log.step_began(&Step::new(
        "validating the prospective plan",
        WorkUnit::Nodes,
    ));
    let mut verifier = froe::tooling::NodeTreeVerifier::new(&repository);
    verifier
        .verify_with_progress(head, &mut log)
        .expect("verify the first root");
    verifier
        .verify_with_progress(second_head, &mut log)
        .expect("verify the sharing root");
    log.step_ended();

    // ObservationLog panics on any decrease, so reaching here already pins
    // that the total carried over. The count is also exact: a node reached
    // from both roots is certified — and so reported — once, which is what
    // an independent walk of the two roots counts below.
    let distinct = distinct_nodes_below(&repository, &[head, second_head]);
    assert_eq!(
        log.highest_count_of("validating the prospective plan"),
        Some(distinct as u64),
        "the step reports each distinct node exactly once across both roots"
    );
}

/// The distinct node records the given roots reach, counted with a plain
/// `HashSet` walk that shares no code with the verifier's certificate set.
fn distinct_nodes_below(repository: &Repository, roots: &[froe::RecordIdentifier]) -> usize {
    let mut seen: std::collections::HashSet<froe::RecordIdentifier> =
        std::collections::HashSet::new();
    let mut pending: Vec<froe::RecordIdentifier> = roots.to_vec();
    while let Some(record) = pending.pop() {
        if !seen.insert(record) {
            continue;
        }
        let node = repository.node(record);
        for (_, child) in node
            .child_node_entries()
            .expect("enumerate the child nodes")
        {
            pending.push(child.record_identifier());
        }
    }
    seen.len()
}

#[test]
fn an_observed_backup_equals_an_unobserved_one() {
    let source = TestDirectory::new("backup-source");
    let unobserved_target = TestDirectory::new("backup-plain");
    let observed_target = TestDirectory::new("backup-observed");
    build_repository(source.path(), 5);

    froe::backup(source.path(), unobserved_target.path()).expect("back up");
    let mut log = ObservationLog::default();
    froe::backup_with_progress(source.path(), observed_target.path(), &mut log)
        .expect("back up while observed");

    let unobserved = Repository::open(unobserved_target.path()).expect("reopen the plain backup");
    let observed = Repository::open(observed_target.path()).expect("reopen the observed backup");
    assert_eq!(
        unobserved.segment_count(),
        observed.segment_count(),
        "observation must not change what a backup copies"
    );
    assert!(log.began_and_ended_in_pairs());
    assert!(
        log.descriptions().contains(&"copying nodes"),
        "a backup reports its node copy: {:?}",
        log.descriptions()
    );
    assert!(
        log.highest_count_of("copying nodes")
            .is_some_and(|nodes| nodes >= 5),
        "the copy counts the nodes it wrote"
    );
}
