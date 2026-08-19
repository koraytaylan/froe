//! The store shapes the maintenance phases need: orphaned nodes, a stale
//! archive, a truncated journal, and the assertions that prove each was
//! actually built.

use super::*;

/// Make a stale archive: copy the active data*.tar to the next generation
/// letter. This is the on-disk condition Oak leaves behind after its own
/// compaction publishes a newer generation.
/// Confirm every condition the reclamation fixture is meant to contain is
/// really present before the run.
///
/// Without this, a fixture step that silently failed to build its condition
/// would leave the post-cleanup assertions vacuously satisfied — the condition
/// would be absent afterwards because it was never there.
/// Writes nodes at generation zero that no head ever reaches, and returns the
/// archive they landed in.
///
/// The orphans a segment sweep is supposed to reclaim. This used to be made by
/// restoring the pre-compaction gen-0 archive at a spare archive number, which
/// stopped working once compaction began sharing bulk segments the way Oak
/// does: the compacted head then still references gen-0's binary blocks, so
/// re-introducing that archive is a genuine duplicate-segment condition and
/// cleanup rightly refuses it. Fresh unreferenced segments are unreachable by
/// construction rather than by an assumption about what compaction leaves.
pub(crate) fn write_orphan_nodes(store_path: &Path) -> PathBuf {
    let archives_before: std::collections::BTreeSet<String> = std::fs::read_dir(store_path)
        .expect("list archives before orphans")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    let written = {
        let store = WritableRepository::open(store_path).expect("open for orphan nodes");
        let mut writer =
            store.record_writer(froe::writer::segment_builder::GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            });
        let mut written = 0usize;
        for index in 0..2000 {
            let title = writer
                .write_string(&format!("orphan node {index}"))
                .expect("orphan title");
            writer
                .write_node(
                    Some("nt:unstructured"),
                    &[],
                    &ChildNodesToWrite::Zero,
                    &[PropertyToWrite {
                        name: "jcr:title".to_owned(),
                        property_type: PropertyType::String,
                        values: PropertyValuesToWrite::Single(title),
                    }],
                )
                .expect("orphan node");
            written += 1;
        }
        writer.finish().expect("finish orphan segments");
        // Deliberately no compare_and_set_head: nothing reaches these records.
        store.close().expect("close orphan writer");
        written
    };

    let orphan_archive = std::fs::read_dir(store_path)
        .expect("list archives after orphans")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("data") && !archives_before.contains(name))
        .map(|name| store_path.join(name))
        .expect("the orphan writer created a new archive");
    eprintln!(
        "  wrote {written} orphan nodes reachable from nothing, in {}",
        orphan_archive.display()
    );
    orphan_archive
}

/// Compacts once to advance the store off generation zero, asserting the
/// partial-archive rewrite that only this first pass can produce.
///
/// This is the one place the rewrite is reachable: the Oak store carries a
/// binary large enough to live in bulk segments, so the first compaction
/// keeps those where they lie while the data segments beside them die — an
/// archive that must be rewritten to its next generation letter rather
/// than unlinked. By the second pass the surviving archives hold nothing
/// but referenced bulk, which is wholly live, so asserting a rewrite there
/// would be asserting something the format cannot produce.
pub(crate) fn assert_first_compaction_rewrites_a_partial_archive(clean_store: &Path) {
    eprintln!("  step 1: froe compact (gen 0 -> 1)");
    let archives_before_first = archive_names(clean_store);
    let first_compaction = froe(&[
        "compact",
        clean_store.to_str().unwrap(),
        "--keep-expired-checkpoints",
        "--yes",
    ]);
    let archives_after_first = archive_names(clean_store);
    assert!(
        parse_count(&first_compaction, " rewritten") >= 1,
        "the first compaction rewrote at least one partially dead archive: {first_compaction}"
    );
    let rewritten_pairs: Vec<String> = archives_before_first
        .iter()
        .filter(|name| !archives_after_first.contains(*name))
        .filter_map(|name| {
            let letter = name.as_bytes()[name.len() - 5];
            let successor = format!(
                "data{}{}.tar",
                &name[4..name.len() - 5],
                (letter + 1) as char
            );
            archives_after_first
                .contains(&successor)
                .then(|| format!("{name} -> {successor}"))
        })
        .collect();
    assert!(
        !rewritten_pairs.is_empty(),
        "a source archive is gone and its successor letter holds the survivors; \
         before {archives_before_first:?} after {archives_after_first:?}"
    );
    eprintln!("  archives rewritten in place: {rewritten_pairs:?}");
}

/// Every reported effect of a cleanup run, plus the journal's end state.
///
/// Extracted from the phase so each condition is named in one place. The
/// obvious check — that the output contains "orphan segments removed" — is
/// worthless, because that phrase comes from an unconditional format
/// template and is printed even when the count is zero. Parsing the number
/// is what makes a zero-count run fail.
pub(crate) fn assert_cleanup_counts_and_journal(cleanup_output: &str, clean_store: &Path) {
    let removed_segments = parse_count(cleanup_output, " orphan segments removed");
    let removed_stale = parse_count(cleanup_output, " stale removed");
    let removed_checkpoints = parse_count(cleanup_output, " checkpoints and");
    let removed_journal_lines = parse_count(cleanup_output, " journal lines removed");
    assert!(
        removed_segments > 0,
        "the run reclaimed orphan segments, not zero: {cleanup_output}"
    );
    assert!(
        removed_stale >= 1,
        "the run removed the stale archive: {cleanup_output}"
    );
    assert_eq!(
        removed_checkpoints, 1,
        "the run dropped the one expired checkpoint: {cleanup_output}"
    );
    // Every earlier revision goes, not only the two corrupt lines: a run
    // retires history to the head it just compacted. Asserting the end state
    // is both stronger and stable against the fixture's line count.
    assert!(
        removed_journal_lines >= 2,
        "the run retired at least the two corrupt journal lines: {cleanup_output}"
    );
    let journal_after = std::fs::read_to_string(clean_store.join("journal.log"))
        .expect("read the journal after the run");
    assert_eq!(
        journal_after.lines().count(),
        1,
        "the run leaves exactly one journal line: {journal_after}"
    );
    assert!(
        !journal_after.contains("this_line_has_no_space")
            && !journal_after.contains("not-a-uuid:bad"),
        "and neither corrupt line survives: {journal_after}"
    );
}

/// The `data*.tar` names currently in a store, sorted.
pub(crate) fn archive_names(store: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(store)
        .expect("list the store directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("data") && name.ends_with(".tar"))
        .collect();
    names.sort();
    names
}

pub(crate) fn assert_cleanup_fixture_built(
    store: &Path,
    stale: &StaleArchive,
    checkpoint_name: &str,
    orphan_archive: &Path,
) {
    let checkpoints = froe(&["checkpoints", store.to_str().unwrap()]);
    assert!(
        checkpoints.contains(checkpoint_name),
        "the checkpoint {checkpoint_name} that must expire is present before \
         cleanup: {checkpoints}"
    );
    let journal = std::fs::read_to_string(store.join("journal.log")).expect("read journal before");
    assert!(
        journal.contains("this_line_has_no_space") && journal.contains("not-a-uuid:bad"),
        "both corrupt journal lines are present before cleanup: {journal}"
    );
    assert!(
        stale.superseded.exists() && stale.winner.exists(),
        "both letters of the stale-archive pair are present before cleanup: {} and {}",
        stale.superseded.display(),
        stale.winner.display()
    );
    assert!(
        orphan_archive.exists(),
        "the orphan-bearing archive {} is present before cleanup",
        orphan_archive.display()
    );
}

/// Confirm each condition is gone from disk and from froe's own listings,
/// independently of the counts cleanup reported.
pub(crate) fn assert_cleanup_conditions_removed(
    store: &Path,
    stale: &StaleArchive,
    checkpoint_name: &str,
    orphan_archive: &Path,
) {
    assert!(
        !stale.superseded.exists(),
        "the superseded archive letter {} is gone from disk after cleanup",
        stale.superseded.display()
    );
    // The safety-relevant direction: the run removed the superseded letter and
    // left the winner, rather than deleting the archive Oak will actually open.
    assert!(
        stale.winner.exists(),
        "the winning archive letter {} survived cleanup",
        stale.winner.display()
    );
    // The orphan-bearing archive is the reclaimable-segment condition; if it
    // is still here, the segments task reclaimed nothing regardless of the
    // count it printed.
    assert!(
        !orphan_archive.exists(),
        "the orphan-bearing archive {} was reclaimed",
        orphan_archive.display()
    );
    let journal = std::fs::read_to_string(store.join("journal.log")).expect("read journal after");
    assert!(
        !journal.contains("this_line_has_no_space") && !journal.contains("not-a-uuid:bad"),
        "both corrupt journal lines are gone from the journal: {journal}"
    );
    assert!(
        !journal.trim().is_empty(),
        "cleanup left a usable journal, not an empty one"
    );
    let checkpoints = froe(&["checkpoints", store.to_str().unwrap()]);
    assert!(
        !checkpoints.contains(checkpoint_name),
        "the expired checkpoint {checkpoint_name} is gone after cleanup: {checkpoints}"
    );
}

/// The stale-archive condition: two files for one archive number.
///
/// Oak selects the highest generation letter (`tar-layer.md` §"generation
/// letter selection"), so copying the active archive to the next letter makes
/// the *copy* the winner and leaves the original superseded. Cleanup must
/// remove the superseded file and must not touch the winner, so the phase
/// needs both paths to assert either direction.
pub(crate) struct StaleArchive {
    pub(crate) superseded: PathBuf,
    pub(crate) winner: PathBuf,
}

/// Create the stale-archive condition and return both files, so the phase can
/// assert on exact paths rather than a hardcoded guess.
pub(crate) fn make_stale_archive(store: &Path) -> StaleArchive {
    let active = std::fs::read_dir(store)
        .expect("read_dir")
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            name.starts_with("data") && name.ends_with(".tar")
        })
        .min()
        .expect("at least one data*.tar");
    let base = active.file_name().unwrap().to_string_lossy().into_owned();
    let letter = base.as_bytes()[base.len() - 5];
    assert!(letter.is_ascii_lowercase(), "invalid generation letter");
    assert!(
        letter != b'z',
        "the active archive is already at generation z, so the stale-archive \
         condition cannot be built; skipping it would leave the phase asserting \
         nothing about stale archives"
    );
    let next_letter = (letter + 1) as char;
    let stale_name = format!("data{}{}.tar", &base[4..base.len() - 5], next_letter);
    let stale_path = store.join(&stale_name);
    eprintln!("  creating stale archive: {base} -> {stale_name}");
    std::fs::copy(&active, &stale_path).expect("copy stale archive");
    StaleArchive {
        superseded: active,
        winner: stale_path,
    }
}

/// Truncate the journal to just the head line, exactly as Oak's `compact`
/// tool does after compaction.
///
/// The line kept is the one naming the head froe actually *binds* — the
/// newest revision whose segment exists — not blindly the last line: a
/// Sling shutdown outrun by the container's stop grace can append a
/// revision whose segments never reached disk, and truncating to that line
/// would discard the tolerant fallback and leave an unopenable store.
pub(crate) fn truncate_journal_to_head(store: &Path) {
    let bound_head = froe::Repository::open(store)
        .expect("open the store to resolve its bound head")
        .head_record_identifier();
    let bound_revision = format!("{}:{}", bound_head.segment, bound_head.record_number);
    let journal = store.join("journal.log");
    let content = std::fs::read_to_string(&journal).expect("read journal");
    let head_line = content
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .find(|line| line.split_whitespace().next() == Some(bound_revision.as_str()))
        .unwrap_or_else(|| {
            panic!("no journal line names the bound head {bound_revision}:\n{content}")
        });
    eprintln!("  truncating journal to head: {head_line}");
    std::fs::write(&journal, format!("{head_line}\n")).expect("write journal");
}

/// Append two corrupt journal lines to test the journal cleanup task's
/// parser-skipped and invalid-record-identifier removal paths.
pub(crate) fn append_corrupt_journal_lines(store: &Path) {
    let journal = store.join("journal.log");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .expect("open journal for append");
    // ParserSkippedNoSpace: a line with no ASCII space.
    file.write_all(b"this_line_has_no_space\n")
        .expect("write corrupt line 1");
    // InvalidRecordIdentifier: first field is not a record id.
    file.write_all(b"not-a-uuid:bad root 1234567890\n")
        .expect("write corrupt line 2");
}

/// The checkpoint the Oak async indexer references, read from `/:async`.
pub(crate) fn async_referenced_checkpoint(store: &Path) -> String {
    let node = froe(&["node", store.to_str().unwrap(), "/:async"]);
    let line = node
        .lines()
        .find(|line| line.contains("async <String>"))
        .unwrap_or_else(|| panic!("/:async carries an async checkpoint reference: {node}"));
    let (_, quoted) = line
        .split_once("= \"")
        .unwrap_or_else(|| panic!("the async property is a quoted string: {line}"));
    quoted.trim_end().trim_end_matches('"').to_owned()
}
