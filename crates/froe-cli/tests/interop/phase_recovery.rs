//! Phases 10 to 12: repairing, backing up, and recovering a store, then
//! the end-to-end run that chains every phase in order.

use super::*;

/// Phase 7: froe repairs an archive Oak left untrailered, and Oak reads it.
///
/// The only phase whose damage is produced by Oak itself rather than
/// simulated: the JVM is killed with SIGKILL while it holds an archive open,
/// which is what an OOM kill or a yanked host does and what leaves the
/// newest archive complete but without its `.gph`, `.brf` and index
/// trailers. A read-only froe command serves such a store through a
/// recovery scan; an authorized `froe compact --yes` rebuilds the index.
///
/// This phase exists because a froe-to-froe round trip is not evidence for a
/// format-writing feature — `CONTRIBUTING.md` says so in as many words. The
/// assertion that matters is the last one: a real Oak opens the rebuilt
/// archive and serves the same tree, without logging any of its own repair
/// messages, so it consumed froe's index rather than reconstructing one.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn repair() {
    let store = oak_store();
    let repair_store = work_root().join("repair-store");

    // Give Oak the store, let it open an archive, and kill it mid-life.
    let volume = PodmanVolume::new("froe-interop-repair");
    let bootstrap = PodmanContainer::run_detached("froe-repair-bootstrap", 8086, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-repair-kill", 8086, &volume.name);
    wait_for_sling(8086, "froe-repair-kill");

    // Written outside /content/interop so the baseline fingerprint still
    // describes the same tree after the repair; this content is incidental,
    // its only job is to make Oak hold an archive open with segments in it.
    eprintln!("  writing content so Oak's open archive holds segments");
    sling_post(8086, "/content/repairzone", "sling:Folder", "Repair Zone");
    for i in 1..=5u32 {
        sling_post(
            8086,
            &format!("/content/repairzone/page{i}"),
            "sling:OrderedFolder",
            &format!("Page {i}"),
        );
    }
    // Oak's flush thread runs on a timer; without this the kill can land
    // before anything reached the open archive at all.
    eprintln!("  waiting for Oak's flush thread");
    std::thread::sleep(Duration::from_secs(15));

    eprintln!("  killing the JVM with SIGKILL");
    sling.kill_uncleanly();
    drop(sling);

    let _ = std::fs::remove_dir_all(&repair_store);
    store_from_volume(&volume.name, &repair_store);

    // Read-only from here until cleanup runs: every froe *write* command
    // repairs an index-less archive on open, so touching one would heal the
    // fixture and the phase would assert nothing.
    eprintln!("  confirming Oak left an archive without an index");
    let archives = froe(&["archives", repair_store.to_str().unwrap()]);
    let indexless: Vec<&str> = archives
        .lines()
        .filter(|line| line.contains("recovered (no valid index"))
        .collect();
    assert_eq!(
        indexless.len(),
        1,
        "the killed JVM must leave exactly one untrailered archive:\n{archives}"
    );
    let damaged = indexless[0]
        .split_whitespace()
        .next()
        .expect("archive file name")
        .to_owned();
    eprintln!("  Oak left {damaged} untrailered");

    // A run whose repair is skipped must still refuse, and name the remedy.
    eprintln!("  froe compact --skip-repairing-archive-indexes must refuse");
    let refusal = froe_failure(&[
        "compact",
        repair_store.to_str().unwrap(),
        "--dry-run",
        "--skip-repairing-archive-indexes",
    ]);
    assert!(
        refusal.contains("authorize the repair"),
        "the refusal points at the authorization that repairs it: {refusal}"
    );

    // Read-only, so it does not heal the fixture the way a write command
    // would: froe's reader reconstructs a missing index in memory, which
    // is exactly what makes a digest of the damaged store meaningful as a
    // before-image.
    let digest_before = digest_store(&repair_store);

    eprintln!("  froe compact --yes (the repair is part of the default run)");
    let output = froe(&["compact", repair_store.to_str().unwrap(), "--yes"]);
    assert!(
        output.contains("archive indexes rebuilt"),
        "the run reports the rebuild: {output}"
    );
    assert!(
        repair_store.join(format!("{damaged}.bak")).exists(),
        "the original bytes are retained beside the rebuilt archive"
    );

    eprintln!("  every archive is indexed again");
    let after = froe(&["archives", repair_store.to_str().unwrap()]);
    assert!(
        !after.contains("recovered (no valid index"),
        "no archive is served through the recovery scan any more:\n{after}"
    );
    assert_check_passes_at_head(&repair_store, "repair");

    // Rebuilding an index must recover the content the crash left behind,
    // not a subset of it. A rebuilt index that silently omits entries
    // still parses, still boots, and still loses nodes.
    eprintln!("  content digest after the index rebuild");
    assert_digest_delta(
        &digest_before,
        &digest_store(&repair_store),
        ExpectedDigestDelta::CheckpointsOnly,
        "repair",
    );

    // The claim this phase exists for: Oak opens what froe rebuilt.
    eprintln!("  booting Sling against the froe-repaired store");
    let verify_volume = PodmanVolume::new("froe-interop-repair-verify");
    let bootstrap =
        PodmanContainer::run_detached("froe-repair-bootstrap2", 8086, &verify_volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&repair_store, &verify_volume.name);
    let verify = PodmanContainer::run_detached("froe-repair-verify", 8086, &verify_volume.name);
    wait_for_sling(8086, "froe-repair-verify");

    assert_oak_consumed_store_as_written("froe-repair-verify", "repair");
    assert_content_matches_baseline(8086, "repair");

    drop(verify);
    eprintln!("  repair phase passed");
}

/// Phase 8: froe backup and restore.
///
/// Depends on `read` and `checkpoint` (writer). Independent of compact/
/// cleanup but later in the chain because it is lower-risk.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn backup() {
    let store = oak_store();
    let backup_dir = work_root().join("backup-store");
    let restore_store = work_root().join("restore-store");

    eprintln!("  froe backup");
    let backup_output = froe(&[
        "backup",
        store.to_str().unwrap(),
        backup_dir.to_str().unwrap(),
        "--yes",
    ]);
    assert!(
        backup_output.contains("backup complete"),
        "backup succeeded: {backup_output}"
    );

    eprintln!("  froe check of the backup");
    assert_check_passes_at_head(&backup_dir, "backup");

    // The strongest statement a backup can make, and the one the digest
    // makes available: it renders *identically* to its source. Identity —
    // record, segment and stable identifiers — is excluded from the
    // rendering, and everything else must agree exactly.
    //
    // This is the assertion that would have caught the backup writing a
    // target which referenced bulk segments living only in the source. That
    // backup opened, served its whole content tree, matched the Sling-side
    // fingerprint and passed a consistency check that did not read
    // binaries, while holding none of the binary content: 9.8 MB copied
    // from a 67 MB store.
    eprintln!("  content digest of the backup against its source");
    assert_digest_delta(
        &digest_store(&store),
        &digest_store(&backup_dir),
        ExpectedDigestDelta::None,
        "backup",
    );

    // Restore into a target whose *content* differs from the backup's, not a
    // byte copy of the store the backup came from. Restoring into a copy of its
    // own source cannot fail: a restore that wrote nothing would satisfy every
    // assertion, because the target already holds the expected tree.
    //
    // The commit-phase store carries froe-written nodes the backup does not, so
    // a real restore must make those nodes disappear and leave exactly the
    // baseline tree. The post-boot baseline comparison below reports unexpected
    // entries as well as missing ones, which is what detects a no-op.
    eprintln!("  preparing restore target from the commit store (content differs from the backup)");
    copy_store(&work_root().join("commit-store"), &restore_store);
    let target_head_before = froe_head(&restore_store);

    eprintln!("  froe restore");
    let restore_output = froe(&[
        "restore",
        backup_dir.to_str().unwrap(),
        restore_store.to_str().unwrap(),
        "--yes",
    ]);
    assert!(
        restore_output.contains("restore complete"),
        "restore succeeded: {restore_output}"
    );

    // Restore deep-copies the backup's head into the target, so the target gets
    // an equivalent tree at a *new* record identifier rather than the backup's
    // own. What must hold is that the head moved at all — a restore that wrote
    // nothing would leave it untouched.
    let target_head_after = froe_head(&restore_store);
    assert_ne!(
        target_head_after, target_head_before,
        "restore advanced the target's head; it was unchanged, so nothing was \
         written"
    );

    eprintln!("  froe check after restore");
    assert_check_passes_at_head(&restore_store, "restore");

    // The restored head must render exactly as the backup does. The target
    // began as the commit store, whose content differs, so this also fails
    // on a restore that wrote nothing — and unlike the head comparison
    // above, it fails on a restore that wrote *something else*.
    eprintln!("  content digest after restore against the backup");
    assert_digest_delta(
        &digest_store(&backup_dir),
        &digest_store(&restore_store),
        ExpectedDigestDelta::None,
        "restore",
    );

    eprintln!("  froe tree /content/interop after restore");
    let tree = froe(&[
        "tree",
        restore_store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "3",
    ]);
    assert!(
        tree.contains("sling:Folder"),
        "content tree preserved after restore"
    );

    eprintln!("  booting Sling against the froe-restored store");
    let volume = PodmanVolume::new("froe-interop-restore");
    let bootstrap = PodmanContainer::run_detached("froe-restore-bootstrap", 8085, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&restore_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-restore-verify", 8085, &volume.name);
    wait_for_sling(8085, "froe-restore-verify");

    eprintln!("  content snapshot from Sling after restore");
    assert_oak_consumed_store_as_written("froe-restore-verify", "restore");
    assert_content_matches_baseline(8085, "restore");

    drop(sling);
    eprintln!("  backup phase passed");
}

/// Phase 8: froe recover-journal.
///
/// Deletes journal.log, then rebuilds it from the segments. Depends on
/// `read`. Last because it is the most destructive (deletes the journal).
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn recover() {
    let store = oak_store();
    let recover_store = work_root().join("recover-store");
    eprintln!("  copying store to {}", recover_store.display());
    copy_store(&store, &recover_store);

    // Recovery's defining property is which revision it restores, so pin the
    // head before destroying the journal. Recovery writes every surviving
    // candidate and only verifies the newest, so "the journal resolves" is
    // satisfied by resolving to an older revision — which would silently lose
    // every commit after it.
    let head_before = froe_head(&recover_store);
    // Taken before the journal is destroyed, because after that there is
    // no head to render from. Recovery restoring the same *revision* and
    // recovery restoring the same *content* are different claims, and only
    // the second one is what an operator actually needs.
    let digest_before = digest_store(&recover_store);

    eprintln!("  deleting journal.log");
    std::fs::remove_file(recover_store.join("journal.log")).expect("remove journal");

    eprintln!("  froe recover-journal --yes");
    let recover_output = froe(&["recover-journal", recover_store.to_str().unwrap(), "--yes"]);
    assert!(
        !recover_output.is_empty(),
        "recover-journal produced output"
    );

    eprintln!("  froe summary after recovery");
    let head_after = froe_head(&recover_store);
    assert_eq!(
        head_after, head_before,
        "recovery restored the same head it started from, not an older revision"
    );

    eprintln!("  froe check after recovery");
    assert_check_passes_at_head(&recover_store, "recover");

    eprintln!("  content digest after recovery");
    assert_digest_delta(
        &digest_before,
        &digest_store(&recover_store),
        ExpectedDigestDelta::None,
        "recover",
    );

    eprintln!("  froe tree /content/interop after recovery");
    let tree = froe(&[
        "tree",
        recover_store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "3",
    ]);
    assert!(
        tree.contains("sling:Folder"),
        "content tree preserved after recovery"
    );

    eprintln!("  booting Sling against the froe-recovered store");
    let volume = PodmanVolume::new("froe-interop-recover");
    let bootstrap = PodmanContainer::run_detached("froe-recover-bootstrap", 8086, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&recover_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-recover-verify", 8086, &volume.name);
    wait_for_sling(8086, "froe-recover-verify");

    eprintln!("  content snapshot from Sling after recovery");
    assert_oak_consumed_store_as_written("froe-recover-verify", "recover");
    assert_content_matches_baseline(8086, "recover");

    drop(sling);
    eprintln!("  recover phase passed");
}

/// Run all interop phases in order.
///
/// This is a convenience wrapper that runs the phases in dependency order
/// within a single test. Individual phases can be run separately for
/// debugging.
#[test]
#[ignore = "requires podman and the apache/sling:14 image"]
pub(crate) fn interop_full() {
    generate();
    read();
    // Before `commit`, and load-bearing: froe's direct commits run none of
    // Oak's index editors, so after `commit` the fixture's synchronous
    // `jcr:title` index legitimately lacks an entry and the comparison
    // against Oak would report a difference that says nothing about either
    // side's reading. `index_inventory` asserts its own position too.
    judge_smoke();
    index_inventory();
    // Before `commit` for the same reason `index_inventory` is: froe's
    // direct commits run none of Oak's index editors, so afterwards the
    // fixture is legitimately short an entry and the oracle would report a
    // difference that says nothing about either rebuild.
    property_reindex();
    // Read-only, so its position is free; it runs here because the
    // fixture's Lucene index reflects the state Sling left, and every
    // later phase rewrites the store around it.
    lucene_dump();
    lucene_import();
    // froe's own Lucene writer against Lucene's, over a corpus of its own:
    // it reads the fixture not at all and writes only into its work
    // directory, so its position is free. It runs here because plan 0010's
    // rebuild installs what this phase proves.
    lucene_writer_conformance();
    commit();
    checkpoint();
    compact();
    compact_tail();
    checkpoint_removal();
    cleanup();
    journal_retention();
    compact_convergence();
    version_history_purge();
    repair();
    backup();
    recover();
    write_run_record();
    eprintln!("  all interop phases passed");
}

/// The Lucene document count `index_inventory` recorded, or `unknown` when
/// that phase did not run in this process.
///
/// The count is Oak's own, over a directory Oak dumped: the run record names
/// it because it is what says froe's readers were pointed at real index data
/// rather than at something that merely parsed.
fn lucene_document_count() -> String {
    std::fs::read_to_string(work_root().join("index-lucene-numdocs.txt"))
        .map_or_else(|_| "unknown".to_owned(), |count| count.trim().to_owned())
}

/// Write the run record: what was verified, against which Oak build, when.
///
/// A passing run whose only trace is a console line cannot be audited later.
/// This is the artifact that turns "we have an interop suite" into "the round
/// trip was performed against oak-segment-tar X on this date", which is what
/// the interoperability requirement in `CONTRIBUTING.md` actually asks to be
/// recorded.
pub(crate) fn write_run_record() {
    let oak_version = std::fs::read_to_string(work_root().join("oak-build.txt"))
        .expect("the generate phase records the Oak build");
    let manifest = std::fs::read_to_string(oak_store().join("manifest")).expect("read manifest");
    let store_version = manifest
        .lines()
        .find_map(|line| line.trim().strip_prefix("store.version="))
        .unwrap_or("unknown")
        .to_owned();
    let canonical_index = std::fs::read_to_string(work_root().join("canonical-index-property.txt"))
        .map_or_else(
            |_| "verdict not recorded in this process".to_owned(),
            |verdict| verdict.trim().to_owned(),
        );
    let seconds_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let record = format!(
        "{header}{phases}{closing}",
        header = run_record_header(seconds_since_epoch, &oak_version, &store_version),
        phases = run_record_phases(&canonical_index, &lucene_document_count()),
        closing = RUN_RECORD_CLOSING,
    );
    let path = work_root().join("interop-run-record.txt");
    std::fs::write(&path, &record).expect("write the interop run record");
    eprintln!("  run record written to {}", path.display());
    eprint!("{record}");
}

/// What the run was: when, against which image and build, with which binary.
fn run_record_header(seconds_since_epoch: u64, oak_version: &str, store_version: &str) -> String {
    format!(
        "froe Oak interoperability run record\n\
         \n\
         unix timestamp:      {seconds_since_epoch}\n\
         image:               {image}\n\
         oak-segment-tar:     {oak_version}\n\
         store.version:       {store_version}\n\
         froe binary:         {binary}\n\
         \n\
         Phases passed, in dependency order:\n",
        image = sling_image(),
        binary = froe_bin().display(),
    )
}

/// One entry per phase, saying what that phase proved.
///
/// Split at the writer's own conformance phase, which is where the record
/// turns from what froe does to Oak's store to what it does beside it.
fn run_record_phases(canonical_index: &str, lucene_documents: &str) -> String {
    format!(
        "{}{}",
        run_record_reading_phases(canonical_index, lucene_documents),
        run_record_writing_phases(),
    )
}

/// The phases up to and including the writer's conformance.
fn run_record_reading_phases(canonical_index: &str, lucene_documents: &str) -> String {
    format!(
        "\x20 generate    Oak wrote the fixture store\n\
         \x20 read        froe read Oak's store (summary, tree, check, search, export)\n\
         \x20 judge_smoke the Oak-side judge compiled inside the pinned image and\n\
         \x20             each of its verdicts was shown reachable: Oak's own dumper\n\
         \x20             produced a Lucene directory froe did not write, Lucene's own\n\
         \x20             CheckIndex called it clean, and one flipped byte made it refuse\n\
         \x20 index_      froe's index definitions were byte-identical to Oak's own\n\
         \x20 inventory   IndexDefinitionPrinter, froe index list agreed with Oak's\n\
         \x20             IndexPrinter field by field over every definition both list,\n\
         \x20             froe index check passed the pristine store and named a forged\n\
         \x20             stale entry with exit 3, every Lucene directory Oak dumped was\n\
         \x20             a valid Lucene index ({lucene_documents} documents), and Oak's own index\n\
         \x20             update was asked whether its editor covers the subtrees whose\n\
         \x20             nodes the fixture's node-type index does not name — it does\n\
         \x20 property_   Oak rebuilt every property-family definition in the fixture\n\
         \x20 reindex     and froe's own offline rebuild of the *same extracted bytes*\n\
         \x20             rendered identically for every one of them, under\n\
         \x20             --exclude-property-prefix :count_ for the randomized\n\
         \x20             approximate counters froe omits by recorded deviation. The\n\
         \x20             definitions were discovered from the store, not listed. Before\n\
         \x20             froe's rebuild each definition's bookkeeping was put back to\n\
         \x20             what Oak started from — reindex flagged, reindexCount one\n\
         \x20             below Oak's value — so froe's single increment landed on\n\
         \x20             exactly Oak's. The counter was checked canonical first\n\
         \x20             ({canonical_index}), because a lane cycle between Oak's\n\
         \x20             rebuild and the stop can leave a :cnt-less mirror node no\n\
         \x20             rebuild produces. Nothing outside /oak:index changed, and\n\
         \x20             froe check passed at the new head\n\
         \x20 lucene_     froe's `index dump` of every lucene definition was\n\
         \x20 dump        byte-identical to Oak's own LuceneIndexDumper reading the\n\
         \x20             same :data — file set and contents both — with\n\
         \x20             index-details.txt agreeing once each side's Java properties\n\
         \x20             escaping was undone. Lucene's own CheckIndex called froe's\n\
         \x20             output a valid index, Oak's own IndexConsistencyChecker was\n\
         \x20             clean at its FULL level with the CheckIndex pass *reached*\n\
         \x20             rather than skipped, and Oak's document count over froe's\n\
         \x20             output equalled the count froe computes from segments_N and\n\
         \x20             each .si. One flipped byte in a copy made the comparison\n\
         \x20             name the file and the offset. The store was byte-identical\n\
         \x20             afterwards\n\
         \x20 lucene_     Both directions of `froe index import`. Round trip: froe\n\
         \x20 import      dumped the index, the definition's hidden children were\n\
         \x20             removed on a copy — the state a lost index leaves — and\n\
         \x20             froe imported the dump back; every file read back out of\n\
         \x20             the store byte-identical, and the definition rendered as the\n\
         \x20             original did apart from what is fresh by design (uniqueKey,\n\
         \x20             jcr:lastModified, the status uid, dirListing's order as a\n\
         \x20             set, and jcr:data, whose stored blob carries the fresh\n\
         \x20             uniqueKey). Out of band: the judge reproduced oak-run's\n\
         \x20             IndexerSupport sequence from the classes the image ships —\n\
         \x20             an in-memory copy of the lane checkpoint's state, the lane\n\
         \x20             switch and reindex flag, Oak's own cycle under the\n\
         \x20             visible-editor filter, the lanes switched back, Oak's own\n\
         \x20             dumper and JsonSerializer for the artefact — and froe\n\
         \x20             imported that, landing reindexCount two above the original\n\
         \x20             as oak-run's own import leaves it, clearing the corrupt flag\n\
         \x20             the copy carried, and releasing no checkpoint. A booted Oak\n\
         \x20             answered a fulltext query through each imported index with\n\
         \x20             the rows the pristine store answers and EXPLAIN naming\n\
         \x20             lucene:lucene, logging no reindex and no index failure.\n\
         \x20             Four refusals each left the store byte-identical\n\
         \x20 lucene_     A committed corpus of 8,311 documents was written twice:\n\
         \x20 writer_     by froe's own Lucene writer and by Lucene's own IndexWriter\n\
         \x20 conformance under the same oakCodec composition. Lucene's CheckIndex\n\
         \x20             called froe's directory clean, and the two indexes\n\
         \x20             enumerated identically: every field with its options, every\n\
         \x20             term with statistics recomputed from live postings, every\n\
         \x20             posting with frequency, positions and offsets, every stored\n\
         \x20             value, every doc value beside its has-a-value bitset, every\n\
         \x20             norm, the document count and the commit file's counter.\n\
         \x20             Equality of enumerations is equality of **contents**, not\n\
         \x20             of bytes: froe's recorded choices — PACKED for every bit\n\
         \x20             width, linear transducer arcs, an ascending value table —\n\
         \x20             are invisible to a reader that honours what the file says.\n\
         \x20             It says nothing about merging or deletions either: both\n\
         \x20             indexes are one segment written in one commit, which is\n\
         \x20             what froe writes and all it writes. Every transducer in the\n\
         \x20             committed corpus enumerated back to its exact input map\n"
    )
}

/// The phases that write to the store.
fn run_record_writing_phases() -> String {
    String::from(
        "\x20 commit      Oak served content froe committed\n\
         \x20 checkpoint  froe created a checkpoint, listed by name\n\
         \x20 compact     Oak served the exact baseline tree after full compaction\n\
         \x20 compact     Oak served the exact baseline tree after tail compaction\n\
         \x20 --tail\n\
         \x20 checkpoint  remove by name, remove-unreferenced and remove-all all\n\
         \x20 removal     applied; the checkpoint Oak's async indexer references\n\
         \x20             survived remove-unreferenced, and Oak served the exact\n\
         \x20             baseline tree afterwards\n\
         \x20 reclaim     Oak served the exact baseline tree after orphan, stale-archive,\n\
         \x20             expired-checkpoint and corrupt-journal-line removal, and after\n\
         \x20             a partially dead archive was rewritten to its next generation\n\
         \x20             letter with a survivor subset and reconstructed .gph, .brf\n\
         \x20             and .idx trailers\n\
         \x20 journal     a plain froe compact retired every revision but the head it\n\
         \x20 retention   wrote and swept the segments behind them; Oak booted the\n\
         \x20             result and served the exact baseline tree from the single\n\
         \x20             revision froe kept\n\
         \x20 repair      Oak's own JVM was killed with SIGKILL while it held an archive\n\
         \x20             open, leaving it without its trailers; an authorized froe\n\
         \x20             compact rebuilt the index, and Oak then served\n\
         \x20             the exact baseline tree from the rebuilt archive\n\
         \x20 backup      Oak served the exact baseline tree after backup and restore\n\
         \x20 recover     Oak served the exact baseline tree after journal recovery\n",
    )
}

/// What the run does *not* cover, and what the judge is.
const RUN_RECORD_CLOSING: &str = "\n\
         The judge is Oak itself, compiled and run inside the same pinned image\n\
         from the bundles it ships; it is not a second implementation and not a\n\
         second image.\n\
         \n\
         Every boot additionally asserted that Oak logged none of its repair\n\
         messages, so Oak consumed the store as froe wrote it rather than\n\
         reconstructing it.\n\
         \n\
         The froe-side edits made to a copy *before* an operation under test,\n\
         each through the public writer API and each visible in the digest\n\
         delta the phase declares: index bookkeeping put back to what Oak\n\
         started from (property_reindex); a definition's hidden children\n\
         removed, the state a lost index leaves (lucene_import); `corrupt`\n\
         forged as a DATE, the type Oak's own async lane writes\n\
         (lucene_import); `async` removed, which is what makes a definition\n\
         synchronous (lucene_import). Nothing in the fixture store itself is\n\
         edited — every phase works on its own copy.\n\
         \n\
         The judge's Oak-side oracles on the pinned build: LuceneJudge's\n\
         `dump` (Oak's own LuceneIndexDumper), `checkindex` (Lucene's own\n\
         CheckIndex), `numdocs` and `sample-index`; Consistency's level 1 and\n\
         2 (Oak's own IndexConsistencyChecker, the second running CheckIndex\n\
         over a local copy); OutOfBandBuild's `build` (Oak's own Lucene\n\
         editors, dumper and JsonSerializer, driving oak-run's own\n\
         out-of-band sequence); and IndexJudge's definition and index\n\
         printers.\n\
         \n\
         Not covered: native macOS or Windows execution, store.version=1,\n\
         external blob stores, and Adobe AEM itself (this loop is Apache Sling\n\
         with Oak).\n";
