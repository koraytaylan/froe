//! One test per guard the import introduces, reaching the production entry.
//!
//! A refusal is a claim about what froe will *not* do, and a test that only
//! shows the accepting path proves nothing about it. Each test here drives
//! `plan_lucene_import` or `PreparedLuceneImport::apply` — never a checker
//! or a reader in isolation — and pins the typed refusal or the preserved
//! bytes.
//!
//! The guards whose regression is necessarily in-crate are not here,
//! because a `#[cfg(test)]` seam is absent from the library an integration
//! test links against. The safety case's guards table cites those by name:
//! task 0805's gate-wiring test in `writer/index/lucene_import/prepared.rs`
//! and task 0715's identity twins in `writer/maintenance/apply_identity.rs`.
//!
//! The outcome guards — the state rule, the synchronous and hybrid
//! refusals, the unknown `indexPath`, the drift refusal and its
//! tolerances, the checkpoint-set preservation — live in
//! `lucene_import_tests.rs` beside the round trip they qualify, and the
//! table cites them there.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use froe::writer::index::lucene_import::{
    LuceneImportOptions, PreparedLuceneImport, plan_lucene_import,
};
use support::lucene_import_fixtures::{Shape, TestDirectory, build_store, dump};

/// The directory an import reads has to be a coherent Lucene index, and the
/// refusal lands before a byte is copied.
///
/// A file the commit names that is not there is the sharpest form of it:
/// Oak would open the directory, fail to find the segment, and leave the
/// definition unusable — after froe had already published it.
#[test]
fn an_index_directory_missing_a_file_the_commit_names_is_refused_before_any_copy() {
    let directory = TestDirectory::new("guard-missing-file");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    let before = store_files(&store);

    std::fs::remove_file(input.join("lucene").join("data").join("_0.cfs"))
        .expect("remove a file the commit names");

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect_err("a directory missing a referenced file must be refused");
    let text = error.to_string();
    assert!(
        text.contains("not a coherent Lucene index") && text.contains("_0.cfs"),
        "the refusal names the directory and the file: {text}"
    );
    assert!(
        text.contains("before copying a byte"),
        "the refusal says when it landed: {text}"
    );
    assert_eq!(
        store_files(&store),
        before,
        "a refused plan must leave the store byte-identical"
    );
}

/// A file whose header does not read is refused as unreadable rather than
/// copied as bytes.
#[test]
fn an_index_directory_whose_commit_file_does_not_read_is_refused() {
    let directory = TestDirectory::new("guard-unreadable");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);

    std::fs::write(
        input.join("lucene").join("data").join("segments_1"),
        b"not a commit file",
    )
    .expect("corrupt the commit file");

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect_err("a commit file that does not read must be refused");
    let text = error.to_string();
    assert!(
        text.contains("not a coherent Lucene index") && text.contains("segments_1"),
        "the refusal names the file that would not read: {text}"
    );
}

/// A file no segment names is refused too. Oak ignores such a file; froe
/// refuses the directory, because a file nothing refers to is the
/// signature of a dump that was copied wrong.
#[test]
fn an_index_directory_holding_a_file_no_segment_names_is_refused() {
    let directory = TestDirectory::new("guard-unreferenced");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);

    std::fs::write(
        input.join("lucene").join("data").join("_9.cfs"),
        b"a file from some other index",
    )
    .expect("plant an unreferenced file");

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect_err("an unreferenced file must be refused");
    assert!(
        error.to_string().contains("_9.cfs"),
        "the refusal names the file: {error}"
    );
}

/// `index-details.txt` maps a local directory to a hidden child name. A
/// name that is neither an index nor a suggester directory is refused
/// rather than written under a name Oak does not read.
#[test]
fn a_directory_mapping_to_an_unknown_hidden_child_is_refused() {
    let directory = TestDirectory::new("guard-unknown-mapping");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);

    let details = input.join("lucene").join("index-details.txt");
    let content = std::fs::read_to_string(&details).expect("read index-details.txt");
    std::fs::write(&details, content.replace(":data", ":something-else"))
        .expect("rewrite the mapping");

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect_err("an unknown directory mapping must be refused");
    assert!(
        error.to_string().contains(":something-else"),
        "the refusal names the mapping it cannot place: {error}"
    );
}

/// The plan an operator confirmed described a store that no longer exists.
/// Applying it anyway would act on a state nobody approved.
#[test]
fn a_directory_that_changed_during_confirmation_is_refused_before_any_record() {
    let directory = TestDirectory::new("guard-fingerprint");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    let prepared =
        PreparedLuceneImport::prepare(&store, &LuceneImportOptions::new(input)).expect("prepare");

    // `repo.lock` is excluded from the fingerprint by design — the run
    // creates it — so this has to be a name the fingerprint covers.
    std::fs::write(store.join("data00099a.tar"), b"not an archive\n")
        .expect("plant a change during confirmation");

    let error = prepared
        .apply()
        .expect_err("a changed directory must be refused")
        .to_string();
    assert!(
        error.contains("changed between planning and applying"),
        "the refusal says what changed and when: {error}"
    );
}

/// The fingerprint skips `repo.lock`, so a lock file swapped during
/// confirmation is invisible to it. The path-identity recheck is the only
/// thing that sees it, and what it sees is that the lock this run holds is
/// no longer the lock at that path.
#[cfg(unix)]
#[test]
fn a_replaced_lock_file_is_refused_before_the_store_is_opened() {
    let directory = TestDirectory::new("guard-lock-identity");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    let prepared =
        PreparedLuceneImport::prepare(&store, &LuceneImportOptions::new(input)).expect("prepare");

    std::fs::remove_file(store.join("repo.lock")).expect("remove the lock we hold");
    std::fs::write(store.join("repo.lock"), b"").expect("put a different inode there");

    let error = prepared
        .apply()
        .expect_err("a replaced lock file must be refused")
        .to_string();
    assert!(
        error.contains("lock"),
        "the refusal names the lock: {error}"
    );
}

/// The run is additive: it must never write into an archive number that
/// already exists on disk, whether or not that archive is active.
#[test]
fn the_imported_records_go_to_an_archive_number_above_every_physical_name() {
    let directory = TestDirectory::new("guard-archive-number");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    let numbers_before = archive_numbers(&store);
    let highest_before = *numbers_before
        .iter()
        .max()
        .expect("the fixture has an archive");

    froe::writer::index::lucene_import::lucene_import(&store, &LuceneImportOptions::new(input))
        .expect("import");

    let added: Vec<u32> = archive_numbers(&store)
        .into_iter()
        .filter(|number| !numbers_before.contains(number))
        .collect();
    assert!(!added.is_empty(), "the run wrote no archive");
    for number in added {
        assert!(
            number > highest_before,
            "the run wrote archive {number}, at or below the {highest_before} already there"
        );
    }
}

/// Every file in the store with its bytes.
fn store_files(store: &std::path::Path) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(store)
        .expect("read the store directory")
        .map(|entry| {
            let entry = entry.expect("directory entry");
            (
                entry.file_name(),
                std::fs::read(entry.path()).expect("read the file"),
            )
        })
        .collect()
}

/// The numeric part of every `dataNNNNNx.tar` in the store.
fn archive_numbers(store: &std::path::Path) -> Vec<u32> {
    std::fs::read_dir(store)
        .expect("read the store")
        .filter_map(|entry| {
            let name = entry.expect("entry").file_name();
            if std::path::Path::new(&name).extension()? != "tar" {
                return None;
            }
            name.to_string_lossy()
                .strip_prefix("data")?
                .get(..5)?
                .parse()
                .ok()
        })
        .collect()
}
