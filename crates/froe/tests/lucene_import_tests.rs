//! `froe index import`, end to end over synthetic stores.
//!
//! The central test is a **round trip**: dump a store's index, reset its
//! bookkeeping, import it back, and require the `:data` subtree to match
//! what was there — apart from the four things that are new by design
//! (`uniqueKey`, `jcr:lastModified`, the status `uid`, and `dirListing`'s
//! order, compared as a set).
//!
//! That is the strongest claim these tests can make without Oak. The
//! comparison against Oak's own dumper and importer is task 0808's and
//! 0809's.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::Path;

use froe::index::lucene::dump::{DumpOptions, dump_lucene_indexes};
use froe::store::Repository;
use froe::writer::index::lucene_import::{
    LuceneImportOptions, PreparedLuceneImport, lucene_import, plan_lucene_import,
};
use support::filesystem_snapshot::directory_snapshot;
use support::lucene_import_fixtures::{
    Shape, TestDirectory, build_store, data_directory, digest_lines, dump, sample_files,
    stored_files,
};
use support::property_index_layout::{Node, Property};

#[test]
fn a_round_trip_reproduces_the_index_data() {
    // Dump, then import back over the same store. What comes out must be
    // what went in.
    let directory = TestDirectory::new("round-trip");
    let store = build_store(&directory, Shape::default());
    let before = stored_files(&store);
    let input = dump(&directory, &store);

    let outcome = lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");
    assert!(outcome.moved_the_head());
    assert_eq!(outcome.indexes.len(), 1);
    assert_eq!(stored_files(&store), before, "byte for byte");
}

#[test]
fn the_content_tree_is_untouched_and_the_store_still_checks() {
    let directory = TestDirectory::new("content-untouched");
    let store = build_store(&directory, Shape::default());
    let content_before = digest_lines(&store, "/content");
    let input = dump(&directory, &store);

    lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    assert_eq!(
        digest_lines(&store, "/content"),
        content_before,
        "an import must not change content"
    );
    let report = froe::tooling::check_consistency(
        &store,
        &["/".to_owned()],
        froe::tooling::BinaryCheck::EveryBlock,
        1,
    )
    .expect("check");
    assert!(report.has_good_revision(), "{report:?}");
}

#[test]
fn the_reopened_index_passes_the_structural_check() {
    // The container claim, after publication: the imported directory is a
    // coherent set of Lucene files.
    let directory = TestDirectory::new("structural");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    let repository = Repository::open(&store).expect("open");
    let node = repository
        .node_at_path("/oak:index/lucene")
        .expect("resolve")
        .expect("exists");
    let definition = froe::index::IndexDefinition::read(&node, "/oak:index/lucene").expect("model");
    let oak_directory =
        froe::index::lucene::OakDirectory::open(&repository, &node, &definition, ":data")
            .expect("open")
            .expect("exists");
    let report =
        froe::index::lucene::check::check_structure(&oak_directory).expect("check the structure");
    assert!(report.is_coherent(), "{report:?}");
    assert_eq!(report.codec_name.as_deref(), Some("oakCodec"));
    assert_eq!(report.live_document_count, 5);
}

#[test]
fn the_bookkeeping_is_what_oak_runs_importer_leaves() {
    let directory = TestDirectory::new("bookkeeping");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    let outcome = lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    let repository = Repository::open(&store).expect("open");
    let node = repository
        .node_at_path("/oak:index/lucene")
        .expect("resolve")
        .expect("exists");
    let definition = froe::index::IndexDefinition::read(&node, "/oak:index/lucene").expect("model");

    assert!(!definition.reindex.flagged, "reindex is cleared");
    // The file carried no `reindexCount`, so the stored one is 0 + 1.
    assert_eq!(definition.reindex.count, 1);
    assert_eq!(outcome.indexes[0].reindex_count, 1);

    // `:status` carries a `uid` and nothing else.
    let status = digest_lines(&store, "/oak:index/lucene/:status");
    let line = status.first().expect("a :status node");
    assert!(line.contains("uid=String:"), "{line}");
    for absent in ["lastUpdated", "indexedNodes", "reindexCompletionTimestamp"] {
        assert!(
            !line.contains(absent),
            "Oak's importer leaves no post-import state to copy {absent} from: {line}"
        );
    }
}

#[test]
fn a_suggest_data_mapping_is_skipped_with_its_reason() {
    let directory = TestDirectory::new("suggest");
    let store = directory.store();
    let definition = Node::new()
        .with(
            "jcr:primaryType",
            Property::Name("oak:QueryIndexDefinition".to_owned()),
        )
        .with("type", Property::Text("lucene".to_owned()))
        .with("async", Property::Text("async".to_owned()))
        .with_child(":data", data_directory(&sample_files()))
        .with_child(
            ":suggest-data",
            data_directory(&[("suggester".to_owned(), b"suggestions".to_vec())]),
        );
    let root = Node::new()
        .with_child("content", Node::new())
        .with_child(
            ":async",
            Node::new().with("async", Property::Text("checkpoint-1".to_owned())),
        )
        .with_child("oak:index", Node::new().with_child("lucene", definition));
    support::property_index_layout::write_repository_with_checkpoints(
        &store,
        &root,
        &[("checkpoint-1", root.clone())],
    );

    let input = dump(&directory, &store);
    let outcome = lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    assert_eq!(outcome.indexes[0].skipped_mappings.len(), 1);
    assert!(
        outcome.indexes[0].skipped_mappings[0]
            .1
            .contains("rebuilds it when its lastUpdated is missing"),
        "{:?}",
        outcome.indexes[0].skipped_mappings
    );
    // Never imported, and the old one is dropped with the other hidden
    // children, as Oak's own updater drops them.
    assert!(
        digest_lines(&store, "/oak:index/lucene/:suggest-data").is_empty(),
        "suggester data is never imported"
    );
}

#[test]
fn a_checkpoint_that_does_not_resolve_is_refused() {
    let directory = TestDirectory::new("dangling");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);

    // A checkpoint name the store does not hold at all, which is what an
    // operator gets from a directory built against a different store.
    let info = input.join("indexer-info.properties");
    std::fs::write(&info, "checkpoint=some-other-checkpoint\n").expect("rewrite");

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect_err("a checkpoint that does not resolve must be refused");
    assert!(
        error.to_string().contains("does not resolve in this store"),
        "{error}"
    );
}

/// The state rule itself: a checkpoint that **resolves**, to a state other
/// than the one the lane will resume from.
///
/// This is the precondition that stands in for oak-run's live
/// bring-up-to-date. The dangling case above never reaches it — the
/// resolution fails first — so without this test the rule has no
/// regression at all.
#[test]
fn a_checkpoint_that_resolves_to_another_state_is_refused_naming_both_roots() {
    let directory = TestDirectory::new("rival");
    let store = build_store(
        &directory,
        Shape {
            rival_checkpoint: true,
            ..Shape::default()
        },
    );
    let input = dump(&directory, &store);

    // The directory claims it was built at `checkpoint-2`; the lane will
    // resume from `checkpoint-1`, and the two pin different roots.
    let info = input.join("indexer-info.properties");
    std::fs::write(&info, "checkpoint=checkpoint-2\n").expect("rewrite");

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect_err("an index built at another state must be refused")
        .to_string();
    assert!(
        error.contains("was built at checkpoint checkpoint-2")
            && error.contains("lane async resumes from checkpoint-1"),
        "the refusal names both checkpoints and both roots: {error}"
    );
    assert!(
        error.contains("rebuild at the lane's own checkpoint"),
        "the refusal names the remedy: {error}"
    );
}

#[test]
fn a_synchronous_definition_is_refused_by_name() {
    let directory = TestDirectory::new("synchronous");
    let store = build_store(
        &directory,
        Shape {
            lane: None,
            checkpoint: false,
            ..Shape::default()
        },
    );
    // A synchronous definition dumps as a backup with no properties file,
    // so the import has nothing to read — which is itself the refusal an
    // operator meets first.
    dump_lucene_indexes(
        &Repository::open(&store).expect("open"),
        &DumpOptions::new(Vec::new(), directory.dump()),
    )
    .expect("dump");

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(directory.input()))
        .expect_err("a directory without indexer-info.properties must be refused");
    assert!(
        error.to_string().contains("indexer-info.properties"),
        "{error}"
    );
}

#[test]
fn a_hybrid_definition_is_refused_by_name() {
    let directory = TestDirectory::new("hybrid");
    let store = build_store(
        &directory,
        Shape {
            hybrid: true,
            ..Shape::default()
        },
    );
    let input = dump(&directory, &store);

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect_err("a hybrid definition must be refused");
    assert!(
        error.to_string().contains("hybrid") && error.to_string().contains(":property-index"),
        "the refusal says why: {error}"
    );
}

#[test]
fn a_named_path_with_no_index_directory_is_refused() {
    let directory = TestDirectory::new("named-missing");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);

    let error = plan_lucene_import(
        &store,
        &LuceneImportOptions::new(input).with_indexes(["/oak:index/absent".to_owned()]),
    )
    .expect_err("a named path with no directory must be refused");
    assert!(error.to_string().contains("/oak:index/absent"), "{error}");
}

#[test]
fn the_store_is_untouched_until_the_first_record() {
    let directory = TestDirectory::new("additive");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    let before: Vec<(std::ffi::OsString, Vec<u8>)> = std::fs::read_dir(&store)
        .expect("read")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name(),
                std::fs::read(entry.path()).expect("read"),
            )
        })
        .collect();

    lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    for (name, bytes) in before {
        if name == "repo.lock" {
            continue;
        }
        let after = std::fs::read(store.join(&name)).expect("read after");
        assert!(
            after.starts_with(&bytes),
            "{} was rewritten rather than appended to",
            name.to_string_lossy()
        );
    }
}

#[test]
fn the_checkpoints_are_unchanged_by_an_import() {
    // A recorded departure from oak-run's importer, whose fourth step
    // releases the checkpoint `indexer-info.properties` names: the only
    // checkpoint froe accepts is the lane's, which the lane owns.
    let directory = TestDirectory::new("checkpoints");
    let store = build_store(&directory, Shape::default());
    let before = digest_lines(&store, "#checkpoint");
    let input = dump(&directory, &store);

    lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    assert_eq!(
        digest_lines(&store, "#checkpoint"),
        before,
        "an import releases no checkpoint"
    );
}

#[test]
fn an_observed_import_equals_an_unobserved_one() {
    let plain = TestDirectory::new("observed-plain");
    let plain_store = build_store(&plain, Shape::default());
    let plain_input = dump(&plain, &plain_store);
    let unobserved =
        lucene_import(&plain_store, &LuceneImportOptions::new(plain_input)).expect("import");

    let observed_directory = TestDirectory::new("observed");
    let observed_store = build_store(&observed_directory, Shape::default());
    let observed_input = dump(&observed_directory, &observed_store);
    let mut log = support::observation_log::ObservationLog::default();
    let observed = PreparedLuceneImport::prepare_with_progress(
        &observed_store,
        &LuceneImportOptions::new(observed_input),
        &mut log,
    )
    .expect("prepare")
    .apply_with_progress(&mut log)
    .expect("apply");

    assert_eq!(observed.indexes.len(), unobserved.indexes.len());
    assert_eq!(
        observed.indexes[0].files, unobserved.indexes[0].files,
        "observation changed what was written"
    );
    assert!(log.began_and_ended_in_pairs());
}

#[test]
fn an_observed_import_plan_equals_an_unobserved_one() {
    let directory = TestDirectory::new("observed-plan");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);

    let unobserved =
        plan_lucene_import(&store, &LuceneImportOptions::new(input.clone())).expect("plan");
    let observed = plan_lucene_import(&store, &LuceneImportOptions::new(input)).expect("plan");
    assert_eq!(observed, unobserved);
}

#[test]
fn a_dry_plan_takes_no_lock_and_writes_nothing() {
    let directory = TestDirectory::new("plan-read-only");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    let before = directory_snapshot(&store);

    let plan = plan_lucene_import(&store, &LuceneImportOptions::new(input)).expect("plan");
    assert_eq!(plan.imports.len(), 1);
    assert_eq!(plan.file_count(), sample_files().len());
    assert_eq!(
        directory_snapshot(&store),
        before,
        "planning wrote to the store"
    );
}

/// Rewrites the dumped `index-definitions.json`, inserting `line` as the
/// first member of the `lucene` definition's object.
fn insert_into_the_definitions_file(input: &Path, line: &str) {
    let path = input.join("index-definitions.json");
    let content = std::fs::read_to_string(&path).expect("read the definitions file");
    let opening = "\"/oak:index/lucene\": {";
    let at = content
        .find(opening)
        .expect("the file carries the definition")
        + opening.len();
    let mut edited = content;
    edited.insert_str(at, &format!("\n    {line},"));
    std::fs::write(&path, edited).expect("rewrite the definitions file");
}

/// The drift refusal, through the plan — the production caller.
///
/// The comparison exists so that an import never installs data built
/// against a definition the store no longer holds. A property in the file
/// that the store lacks is exactly that, and it is refused before the
/// first record is appended.
#[test]
fn a_definitions_file_that_drifts_from_the_store_is_refused() {
    let directory = TestDirectory::new("drift-refused");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    insert_into_the_definitions_file(&input, "\"evaluatePathRestrictions\": true");

    let error = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect_err("a file that describes a different definition must be refused");
    let text = error.to_string();
    assert!(
        text.contains("does not describe /oak:index/lucene as the store holds it")
            && text.contains("/evaluatePathRestrictions"),
        "the refusal names the definition and the difference: {text}"
    );
    assert!(
        text.contains("froe imports index *data*, never a definition change"),
        "the refusal names the remedy: {text}"
    );
}

/// And the direction that is not drift: the lane revert sets `refresh` on
/// the file's side, so a file carrying one the store lacks is accepted.
#[test]
fn a_refresh_the_lane_revert_set_is_accepted() {
    let directory = TestDirectory::new("drift-refresh");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    insert_into_the_definitions_file(&input, "\"refresh\": true");

    let plan = plan_lucene_import(&store, &LuceneImportOptions::new(input))
        .expect("a file-side refresh is what an honest out-of-band build leaves");
    assert_eq!(plan.imports.len(), 1);
}

/// The disabler's flag, raised where Oak's own importer raises it.
///
/// `docs/analysis/index-definitions.md` §5.5: the flag goes up when a
/// `supersedes` entry names an index whose `type` does not read **strictly**
/// as the `STRING` `disabled`. froe raises it and never acts on it.
#[test]
fn a_supersedes_naming_an_active_index_raises_the_disabler_flag() {
    let directory = TestDirectory::new("disabler-active");
    let store = build_store(
        &directory,
        Shape {
            superseded: Some("property"),
            ..Shape::default()
        },
    );
    let input = dump(&directory, &store);
    lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    let line = digest_lines(&store, "/oak:index/lucene")
        .into_iter()
        .next()
        .expect("the definition");
    assert!(
        line.contains(":disableIndexesOnNextCycle=Boolean:true"),
        "an active superseded index raises the flag: {line}"
    );
}

/// And the other side of the same predicate: a superseded index already
/// disabled raises nothing.
#[test]
fn a_supersedes_naming_a_disabled_index_raises_nothing() {
    let directory = TestDirectory::new("disabler-disabled");
    let store = build_store(
        &directory,
        Shape {
            superseded: Some("disabled"),
            ..Shape::default()
        },
    );
    let input = dump(&directory, &store);
    lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    let line = digest_lines(&store, "/oak:index/lucene")
        .into_iter()
        .next()
        .expect("the definition");
    assert!(
        !line.contains(":disableIndexesOnNextCycle"),
        "a disabled superseded index raises nothing: {line}"
    );
}

/// `:version` is written on every import, and the fresh-index rule
/// collapses to 2 unless the definition carries a `compatMode`.
#[test]
fn the_fresh_index_format_version_is_written() {
    for (mode, expected) in [(None, 2), (Some(1), 1), (Some(2), 2)] {
        let directory = TestDirectory::new(&format!("version-{mode:?}"));
        let store = build_store(
            &directory,
            Shape {
                compatibility_mode: mode,
                ..Shape::default()
            },
        );
        let input = dump(&directory, &store);
        lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

        let line = digest_lines(&store, "/oak:index/lucene")
            .into_iter()
            .next()
            .expect("the definition");
        assert!(
            line.contains(&format!(":version=Long:{expected}")),
            "compatMode {mode:?} must give :version {expected}: {line}"
        );
    }
}

/// `:index-definition` is the visible clone of the **updated** state: every
/// property the import wrote, and not one hidden child.
#[test]
fn the_stored_definition_clones_the_updated_visible_state() {
    let directory = TestDirectory::new("stored-definition");
    let store = build_store(&directory, Shape::default());
    let input = dump(&directory, &store);
    lucene_import(&store, &LuceneImportOptions::new(input)).expect("import");

    let clone = digest_lines(&store, "/oak:index/lucene/:index-definition");
    let line = clone.first().expect("a :index-definition node");

    // Hidden *properties* are kept, which is what makes the clone useful
    // for drift: `:version` is one, and so is the disabler's flag when it
    // is raised.
    assert!(line.contains(":version=Long:2"), "{line}");
    // The properties the import rewrote are the updated ones, not the base
    // state's: a clone carrying `reindex=true` would be a reindex's clone.
    assert!(line.contains("reindex=Boolean:false"), "{line}");
    assert!(line.contains("reindexCount=Long:1"), "{line}");

    // Hidden *children* are dropped, at every depth.
    for hidden in [":data", ":status", ":index-definition"] {
        assert!(
            !clone
                .iter()
                .any(|line| line.contains(&format!("/oak:index/lucene/:index-definition/{hidden}"))),
            "the clone kept the hidden child {hidden}: {clone:?}"
        );
    }
}
