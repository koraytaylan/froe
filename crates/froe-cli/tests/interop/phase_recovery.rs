//! Phases 10 to 12: repairing, backing up, and recovering a store, then
//! the end-to-end run that chains every phase in order.

use super::*;

/// Phase 7: froe repairs an archive Oak left untrailered, and Oak reads it.
///
/// The only phase whose damage is produced by Oak itself rather than
/// simulated: the JVM is killed with SIGKILL while it holds an archive open,
/// which is what an OOM kill or a yanked host does and what leaves the
/// newest archive complete but without its `.gph`, `.brf` and index
/// trailers. Every other froe command refuses such a store;
/// `--repair-archive-indexes` rebuilds it.
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

    // A run must still refuse, and name the flag that fixes it.
    eprintln!("  froe compact without the repair flag must refuse");
    let refusal = froe_failure(&["compact", repair_store.to_str().unwrap(), "--dry-run"]);
    assert!(
        refusal.contains("--repair-archive-indexes"),
        "the refusal points at the flag that repairs it: {refusal}"
    );

    // Read-only, so it does not heal the fixture the way a write command
    // would: froe's reader reconstructs a missing index in memory, which
    // is exactly what makes a digest of the damaged store meaningful as a
    // before-image.
    let digest_before = digest_store(&repair_store);

    eprintln!("  froe compact --repair-archive-indexes");
    let output = froe(&[
        "compact",
        repair_store.to_str().unwrap(),
        "--yes",
        "--repair-archive-indexes",
    ]);
    assert!(
        parse_count(&output, " (originals retained") > 0
            || output.contains("archive indexes rebuilt"),
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
    let seconds_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let record = format!(
        "froe Oak interoperability run record\n\
         \n\
         unix timestamp:      {seconds_since_epoch}\n\
         image:               {image}\n\
         oak-segment-tar:     {oak_version}\n\
         store.version:       {store_version}\n\
         froe binary:         {binary}\n\
         \n\
         Phases passed, in dependency order:\n\
         \x20 generate    Oak wrote the fixture store\n\
         \x20 read        froe read Oak's store (summary, tree, check, search, export)\n\
         \x20 commit      Oak served content froe committed\n\
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
         \x20             open, leaving it without its trailers; froe compact\n\
         \x20             --repair-archive-indexes rebuilt the index, and Oak then served\n\
         \x20             the exact baseline tree from the rebuilt archive\n\
         \x20 backup      Oak served the exact baseline tree after backup and restore\n\
         \x20 recover     Oak served the exact baseline tree after journal recovery\n\
         \n\
         Every boot additionally asserted that Oak logged none of its repair\n\
         messages, so Oak consumed the store as froe wrote it rather than\n\
         reconstructing it.\n\
         \n\
         Not covered: native macOS or Windows execution, store.version=1,\n\
         external blob stores, and Adobe AEM itself (this loop is Apache Sling\n\
         with Oak).\n",
        image = sling_image(),
        binary = froe_bin().display()
    );
    let path = work_root().join("interop-run-record.txt");
    std::fs::write(&path, &record).expect("write the interop run record");
    eprintln!("  run record written to {}", path.display());
    eprint!("{record}");
}
