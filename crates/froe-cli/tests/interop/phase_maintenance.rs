//! Phases 5 to 9: compaction, checkpoint removal, cleanup, and journal
//! retention, each verified against Oak and against the digest.

use super::*;

/// Phase 5: froe compacts a copy of the store and Sling boots against it.
///
/// Depends on `read` (to verify the result) and `checkpoint` (to trust
/// the writer). If this fails, cleanup's multi-generational fixture cannot
/// be built (it uses two compactions).
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn compact() {
    let store = oak_store();
    let compact_store = work_root().join("compact-store");
    eprintln!("  copying store to {}", compact_store.display());
    copy_store(&store, &compact_store);

    // Truncate journal so churned orphans are truly unreachable.
    eprintln!("  truncating journal to head");
    truncate_journal_to_head(&compact_store);

    eprintln!("  content digest before compaction");
    let digest_before = digest_store(&compact_store);

    eprintln!("  froe compact --yes");
    let compact_output = froe(&["compact", compact_store.to_str().unwrap(), "--yes"]);
    assert!(
        compact_output.contains("compacted"),
        "compaction reported success: {compact_output}"
    );

    eprintln!("  froe summary after compaction");
    let after = froe(&["summary", compact_store.to_str().unwrap()]);
    assert!(
        after.contains("journal entries   1"),
        "journal collapsed to 1 entry: {after}"
    );

    eprintln!("  froe check after compaction");
    assert_check_passes_at_head(&compact_store, "compact");

    // The claim compaction actually makes: every node, property, type,
    // arity, value and binary survived the rewrite unchanged. Checkpoints
    // are exempt because compaction retires expired ones by design — the
    // content tree is not.
    eprintln!("  content digest after compaction");
    assert_digest_delta(
        &digest_before,
        &digest_store(&compact_store),
        ExpectedDigestDelta::CheckpointsOnly,
        "compact",
    );

    eprintln!("  froe tree /content/interop after compaction (content preserved)");
    let tree = froe(&[
        "tree",
        compact_store.to_str().unwrap(),
        "/content/interop",
        "--depth",
        "3",
    ]);
    assert!(tree.contains("sling:Folder"), "content tree preserved");

    eprintln!("  booting Sling against the froe-compacted store");
    let volume = PodmanVolume::new("froe-interop-compact");
    let bootstrap = PodmanContainer::run_detached("froe-compact-bootstrap", 8083, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&compact_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-compact-verify", 8083, &volume.name);
    wait_for_sling(8083, "froe-compact-verify");

    eprintln!("  content snapshot from Sling after compaction");
    assert_oak_consumed_store_as_written("froe-compact-verify", "compact");
    assert_content_matches_baseline(8083, "compact");
    let snapshot = content_snapshot(8083);
    assert!(snapshot.contains("Page 1"), "page 1 preserved: {snapshot}");

    assert_binary_round_trips_byte_for_byte(8083, "compact");

    drop(sling);
    eprintln!("  compact phase passed");
}

/// Tail compaction against a real Oak store.
///
/// A materially different reclamation path from full compaction: it advances
/// the generation but keeps the shared full generation, reclaiming by
/// generation alone. Covering only the full form would leave a documented flag
/// of a maintenance command unexercised against Oak.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn compact_tail() {
    let store = oak_store();
    let tail_store = work_root().join("compact-tail-store");
    eprintln!("  copying store to {}", tail_store.display());
    copy_store(&store, &tail_store);

    eprintln!("  truncating journal to head");
    truncate_journal_to_head(&tail_store);

    let digest_before = digest_store(&tail_store);

    eprintln!("  froe compact --tail --yes");
    froe(&["compact", tail_store.to_str().unwrap(), "--tail", "--yes"]);

    eprintln!("  froe check after tail compaction");
    assert_check_passes_at_head(&tail_store, "compact --tail");

    eprintln!("  content digest after tail compaction");
    assert_digest_delta(
        &digest_before,
        &digest_store(&tail_store),
        ExpectedDigestDelta::CheckpointsOnly,
        "compact --tail",
    );

    eprintln!("  booting Sling against the tail-compacted store");
    let volume = PodmanVolume::new("froe-interop-compact-tail");
    let bootstrap = PodmanContainer::run_detached("froe-tail-bootstrap", 8087, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&tail_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-tail-verify", 8087, &volume.name);
    wait_for_sling(8087, "froe-tail-verify");

    assert_oak_consumed_store_as_written("froe-tail-verify", "compact --tail");
    assert_content_matches_baseline(8087, "compact --tail");

    drop(sling);
    eprintln!("  compact --tail phase passed");
}

/// Checkpoint removal against a real Oak store.
///
/// Removal rewrites the super-root's `checkpoints` subtree and commits a new
/// head, which is the structure Oak's asynchronous indexer depends on through
/// `/:async`. The safety-relevant property is that `remove-unreferenced`
/// removes an unreferenced checkpoint and *keeps* the one the indexer
/// references: removing that would make Oak discard its index state and
/// reindex the repository from scratch.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn checkpoint_removal() {
    let store = oak_store();
    let removal_store = work_root().join("checkpoint-removal-store");
    eprintln!("  copying store to {}", removal_store.display());
    copy_store(&store, &removal_store);

    let referenced = async_referenced_checkpoint(&removal_store);
    eprintln!("  Oak's async indexer references checkpoint {referenced}");
    let listing = froe(&["checkpoints", removal_store.to_str().unwrap()]);
    assert!(
        listing.contains(&referenced),
        "the async-referenced checkpoint is present to begin with: {listing}"
    );

    // Two froe-created checkpoints, neither referenced by the indexer.
    let mut created = Vec::new();
    for _ in 0..2 {
        let output = froe(&[
            "checkpoint",
            "create",
            removal_store.to_str().unwrap(),
            "--lifetime-milliseconds",
            "3600000",
            "--yes",
        ]);
        created.push(
            output
                .lines()
                .find_map(|line| line.trim().strip_prefix("created checkpoint "))
                .unwrap_or_else(|| panic!("create reports a name: {output}"))
                .trim()
                .to_owned(),
        );
    }
    eprintln!("  created unreferenced checkpoints {created:?}");

    eprintln!("  froe checkpoint remove (by name)");
    froe(&[
        "checkpoint",
        "remove",
        removal_store.to_str().unwrap(),
        &created[0],
        "--yes",
    ]);
    let after_remove = froe(&["checkpoints", removal_store.to_str().unwrap()]);
    assert!(
        !after_remove.contains(&created[0]),
        "the named checkpoint was removed: {after_remove}"
    );
    assert!(
        after_remove.contains(&created[1]) && after_remove.contains(&referenced),
        "removing one checkpoint by name left the others: {after_remove}"
    );

    eprintln!("  froe checkpoint remove-unreferenced");
    froe(&[
        "checkpoint",
        "remove-unreferenced",
        removal_store.to_str().unwrap(),
        "--yes",
    ]);
    let after_unreferenced = froe(&["checkpoints", removal_store.to_str().unwrap()]);
    assert!(
        !after_unreferenced.contains(&created[1]),
        "the remaining unreferenced checkpoint was removed: {after_unreferenced}"
    );
    assert!(
        after_unreferenced.contains(&referenced),
        "the checkpoint Oak's async indexer references SURVIVED remove-unreferenced; \
         removing it would force a full reindex: {after_unreferenced}"
    );

    eprintln!("  froe check after checkpoint removal");
    assert_check_passes_at_head(&removal_store, "checkpoint removal");

    eprintln!("  booting Sling against the store with a rewritten checkpoints subtree");
    let volume = PodmanVolume::new("froe-interop-checkpoint-removal");
    let bootstrap = PodmanContainer::run_detached("froe-cprm-bootstrap", 8088, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&removal_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-cprm-verify", 8088, &volume.name);
    wait_for_sling(8088, "froe-cprm-verify");

    assert_oak_consumed_store_as_written("froe-cprm-verify", "checkpoint removal");
    assert_content_matches_baseline(8088, "checkpoint removal");
    drop(sling);

    // remove-all is the broadest form; assert it empties the subtree and that
    // the store still verifies afterwards.
    eprintln!("  froe checkpoint remove-all");
    froe(&[
        "checkpoint",
        "remove-all",
        removal_store.to_str().unwrap(),
        "--yes",
    ]);
    let after_all = froe(&["checkpoints", removal_store.to_str().unwrap()]);
    assert!(
        after_all.contains("no checkpoints"),
        "remove-all emptied the checkpoints subtree: {after_all}"
    );
    assert_check_passes_at_head(&removal_store, "checkpoint removal (remove-all)");

    eprintln!("  checkpoint removal phase passed");
}

/// Phase 6: froe cleanup against a multi-generational store.
///
/// Builds a gen 0→1→2 store by compacting twice, then restoring the
/// original gen-0 archive. Those gen-0 segments are 2 full generations
/// behind the head, not protected by journal history (truncated), and
/// not referenced by surviving gen-2 segments — true orphans the
/// `segments` task reclaims. Also tests expired checkpoints, stale
/// archives, and corrupt journal lines.
///
/// Depends on `compact` (to build the fixture). If this fails, the
/// write path's plan-and-apply machinery is broken.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn cleanup() {
    let store = oak_store();
    let clean_store = work_root().join("cleanup-store");
    eprintln!("  copying store to {}", clean_store.display());
    copy_store(&store, &clean_store);

    // Build a multi-generational store: compact twice to advance the head
    // to full_generation=2.
    //
    // `--keep-expired-checkpoints` on both, because these two runs build the
    // fixture rather than being the thing under test. Every run drops expired
    // checkpoints by default, and the checkpoint this phase must watch the
    // *third* run expire was created — with a one-second lifetime — back in
    // the checkpoint phase. Without the flag the setup would consume the
    // condition before the assertion could observe it.
    // Step 1 is where the partial-archive rewrite happens, and the only place
    // it can: the Oak store carries a binary large enough to live in bulk
    // segments, so this first compaction keeps those where they lie while the
    // data segments beside them die — an archive that must be rewritten to its
    // next generation letter rather than unlinked. By step 2 the surviving
    // archives hold nothing but referenced bulk, which is wholly live and has
    // nothing left to reclaim.
    assert_first_compaction_rewrites_a_partial_archive(&clean_store);

    eprintln!("  step 2: froe compact again (gen 1 -> 2)");
    froe(&[
        "compact",
        clean_store.to_str().unwrap(),
        "--keep-expired-checkpoints",
        "--yes",
    ]);

    // Orphan nodes, written directly at generation zero and never linked to
    // any head. Generations behind the compacted head, so the FULL predicate
    // reclaims them.
    //
    // This used to restore the pre-compaction gen-0 archive at a spare
    // archive number, which stopped working once compaction began sharing
    // bulk segments the way Oak does: the compacted head then still
    // references gen-0's binary blocks, so re-introducing that archive is a
    // genuine duplicate-segment condition and cleanup rightly refuses it.
    // Writing fresh unreferenced segments produces the orphans the phase
    // actually wants, and cannot collide with anything the head holds.
    eprintln!("  step 3: writing orphan nodes at generation zero");
    let orphan_archive = write_orphan_nodes(&clean_store);

    // The second reclamation shape needs no fixture: the store carries a
    // binary large enough to live in bulk segments, and compaction references
    // those where they lie. The archives holding them therefore survive with
    // their data segments dead — a partial archive the sweep must rewrite to
    // its next generation letter rather than unlink. Oak reads the result at
    // the end of this phase.

    // Wait for the checkpoint from phase 3 to expire.
    eprintln!("  waiting 2s for the checkpoint to expire");
    std::thread::sleep(Duration::from_secs(2));

    // Add the remaining simulation conditions.
    eprintln!("  making stale archive");
    let stale = make_stale_archive(&clean_store);

    eprintln!("  truncating journal to head");
    truncate_journal_to_head(&clean_store);

    eprintln!("  appending corrupt journal lines");
    append_corrupt_journal_lines(&clean_store);

    // Dry-run: verify the plan sees the orphan segments.
    eprintln!("  froe compact --dry-run");
    let dry_run = froe(&["compact", clean_store.to_str().unwrap(), "--dry-run"]);
    assert!(
        dry_run.contains("orphan segments"),
        "dry-run found orphan segments: {dry_run}"
    );

    // Apply: reclaim the orphan segments, stale archive, expired checkpoint,
    // and corrupt journal lines.
    //
    // Every assertion below names a specific effect. The obvious check —
    // that the output contains "orphan segments removed" — is worthless,
    // because that phrase comes from an unconditional format template and is
    // printed even when the count is zero.
    let expiring_checkpoint = std::fs::read_to_string(work_root().join("expiring-checkpoint.txt"))
        .expect("the checkpoint phase records the name of the checkpoint that will expire");
    assert_cleanup_fixture_built(
        &clean_store,
        &stale,
        expiring_checkpoint.trim(),
        &orphan_archive,
    );

    let digest_before = digest_store(&clean_store);

    eprintln!("  froe compact --yes");
    let cleanup_output = froe(&["compact", clean_store.to_str().unwrap(), "--yes"]);

    assert_cleanup_counts_and_journal(&cleanup_output, &clean_store);
    assert!(
        cleanup_output.contains("compaction complete"),
        "the run completed without deferred or failed deletions: {cleanup_output}"
    );
    // The rewrite itself was asserted at step 1, which is where a partially
    // dead archive exists. What matters here is that the archive it produced —
    // a froe-written successor carrying a survivor subset and reconstructed
    // `.gph`, `.brf` and `.idx` trailers — is still in the store Oak boots
    // against at the end of this phase.
    assert!(
        archive_names(&clean_store)
            .iter()
            .any(|name| !name.ends_with("a.tar")),
        "a rewritten successor archive is part of the store Oak will open: {:?}",
        archive_names(&clean_store)
    );

    // Then confirm the same effects on disk, independently of what was
    // reported.
    assert_cleanup_conditions_removed(
        &clean_store,
        &stale,
        expiring_checkpoint.trim(),
        &orphan_archive,
    );

    eprintln!("  froe summary after cleanup");
    froe(&["summary", clean_store.to_str().unwrap()]);

    eprintln!("  froe check after cleanup");
    assert_check_passes_at_head(&clean_store, "cleanup");

    // The run removed an orphan archive, a stale archive, an expired
    // checkpoint and two corrupt journal lines in one pass. None of that
    // is allowed to have cost a single content node.
    eprintln!("  content digest after cleanup");
    assert_digest_delta(
        &digest_before,
        &digest_store(&clean_store),
        ExpectedDigestDelta::CheckpointsOnly,
        "cleanup",
    );

    eprintln!("  booting Sling against the froe-cleaned store");
    let volume = PodmanVolume::new("froe-interop-cleanup");
    let bootstrap = PodmanContainer::run_detached("froe-cleanup-bootstrap", 8082, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&clean_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-cleanup-verify", 8082, &volume.name);
    wait_for_sling(8082, "froe-cleanup-verify");

    eprintln!("  content snapshot from Sling after cleanup");
    assert_oak_consumed_store_as_written("froe-cleanup-verify", "cleanup");
    assert_content_matches_baseline(8082, "cleanup");

    drop(sling);
    eprintln!("  cleanup phase passed");
}

/// Phase 6b: froe retires journal history, and Oak boots the result.
///
/// Retiring journal history is the one thing froe does that makes
/// repository bytes unreachable *by policy* rather than by Oak's own
/// generation predicate: it removes journal lines whose revisions still
/// resolve, and the segments behind them are then swept in the same run. A
/// froe-to-froe round trip cannot be evidence for that — froe agreeing with
/// its own reachability rules proves nothing about whether Oak can still open
/// what is left.
///
/// So the assertion that matters is the last one: a real Oak boots a store
/// whose history froe deliberately destroyed, and serves the exact baseline
/// tree from the one revision froe kept.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn journal_retention() {
    let store = oak_store();
    let retention_store = work_root().join("retention-store");
    eprintln!("  copying store to {}", retention_store.display());
    copy_store(&store, &retention_store);

    let journal_path = retention_store.join("journal.log");
    let revisions_before = std::fs::read_to_string(&journal_path)
        .expect("read journal")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    eprintln!("  Oak left {revisions_before} journal revisions");
    assert!(
        revisions_before > 1,
        "the fixture needs history to retire; Oak wrote {revisions_before} revisions"
    );

    // Retiring history is no longer an opt-in bound: a plain run retires
    // every revision but the head it just compacted. That is what makes this
    // phase load-bearing — it is the only evidence that a real Oak opens a
    // store whose history froe deliberately destroyed.
    eprintln!("  froe compact --dry-run");
    let dry_run = froe(&["compact", retention_store.to_str().unwrap(), "--dry-run"]);
    assert!(
        dry_run.contains("retire all") && dry_run.contains("journal lines"),
        "the plan names the history it retires: {dry_run}"
    );
    assert!(
        dry_run.contains("not recoverable from the store"),
        "and says the retirement is irreversible: {dry_run}"
    );

    eprintln!("  froe compact --yes");
    let output = froe(&["compact", retention_store.to_str().unwrap(), "--yes"]);
    let removed_lines = parse_count(&output, " journal lines removed");
    assert!(
        removed_lines > 0,
        "the run retired journal revisions, not zero: {output}"
    );
    assert!(
        output.contains("compaction complete"),
        "cleanup completed without deferred or failed deletions: {output}"
    );

    // On disk, independently of what was reported: exactly one revision left.
    let revisions_after = std::fs::read_to_string(&journal_path)
        .expect("read journal after")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(
        revisions_after, 1,
        "a bound of one leaves one revision; {revisions_after} remain"
    );
    assert!(
        retention_store.join("journal.log.bak.000").is_file(),
        "the pre-rewrite journal is retained under a numbered backup"
    );

    eprintln!("  froe check after retiring history");
    assert_check_passes_at_head(&retention_store, "journal-retention");

    // Boot Sling against the store whose history froe destroyed.
    eprintln!("  booting Sling against the history-retired store");
    let volume = PodmanVolume::new("froe-interop-retention");
    let bootstrap = PodmanContainer::run_detached("froe-retention-bootstrap", 8089, &volume.name);
    std::thread::sleep(Duration::from_secs(20));
    drop(bootstrap);
    store_into_volume(&retention_store, &volume.name);
    let sling = PodmanContainer::run_detached("froe-retention-verify", 8089, &volume.name);
    wait_for_sling(8089, "froe-retention-verify");

    eprintln!("  content snapshot from Sling after retiring history");
    assert_oak_consumed_store_as_written("froe-retention-verify", "journal-retention");
    assert_content_matches_baseline(8089, "journal-retention");

    drop(sling);
    eprintln!("  journal-retention phase passed");
}
