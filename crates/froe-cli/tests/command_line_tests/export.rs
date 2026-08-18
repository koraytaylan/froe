//! Exporting from the command line: the `extract` alias v0.1.0 shipped,
//! the `SQLite` and Parquet formats, and when a refresh needs `--full`.

use super::*;

#[test]
pub(crate) fn the_extract_alias_produces_the_export_output() {
    let directory = TestDirectory::new("extract-alias");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let binary = env!("CARGO_BIN_EXE_froe");

    let extract = std::process::Command::new(binary)
        .args(["extract", store.to_str().expect("path"), "--depth", "1"])
        .output()
        .expect("run extract");
    assert!(
        extract.status.success(),
        "extract must keep succeeding: {}",
        String::from_utf8_lossy(&extract.stderr)
    );

    let export = std::process::Command::new(binary)
        .args(["export", store.to_str().expect("path"), "--depth", "1"])
        .output()
        .expect("run export");
    assert!(export.status.success());

    assert!(
        !extract.stdout.is_empty(),
        "the export must emit JSON lines"
    );
    assert_eq!(
        extract.stdout, export.stdout,
        "both spellings must produce identical JSON lines"
    );
}

#[test]
pub(crate) fn the_extract_alias_writes_output_files() {
    let directory = TestDirectory::new("extract-alias-output");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("content.jsonl");

    let extract = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "extract",
            store.to_str().expect("path"),
            "--path",
            "/content",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run extract");
    assert!(
        extract.status.success(),
        "extract --output must keep succeeding: {}",
        String::from_utf8_lossy(&extract.stderr)
    );
    let written = std::fs::read_to_string(&output).expect("read output");
    assert_eq!(
        written,
        "{\"path\":\"/content\",\"properties\":{\"jcr:primaryType\":\"nt:unstructured\"}}\n"
    );
}

#[test]
pub(crate) fn the_sqlite_format_writes_a_database_file() {
    let directory = TestDirectory::new("sqlite-format");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("content.db");

    let export = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "sqlite",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        export.status.success(),
        "export --format sqlite must succeed: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let written = std::fs::read(&output).expect("read output");
    assert!(
        written.starts_with(b"SQLite format 3\0"),
        "the output must be a SQLite database file"
    );

    // A second export to the same path must refuse: the export never
    // overwrites.
    let rerun = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "sqlite",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("re-run export");
    assert!(!rerun.status.success());
    assert!(
        String::from_utf8_lossy(&rerun.stderr).contains("never overwrites"),
        "the rerun must refuse to overwrite: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
}

#[test]
pub(crate) fn the_sqlite_format_leaves_an_existing_file_untouched() {
    let directory = TestDirectory::new("sqlite-never-overwrites");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("victim.db");
    std::fs::write(&output, b"someone else's database").expect("seed");

    let export = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "sqlite",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(!export.status.success());
    assert_eq!(
        std::fs::read(&output).expect("read"),
        b"someone else's database",
        "the existing file must be untouched, not opened and modified"
    );
}

#[test]
pub(crate) fn the_parquet_export_refreshes_in_place() {
    let directory = TestDirectory::new("parquet-refresh");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");

    let first = parquet_export(&store, &output, &[]);
    assert!(
        first.contains("exported 2 nodes"),
        "the first export: {first}"
    );

    // An unchanged store: the export reports itself current and is not
    // rewritten.
    let before = std::fs::read(output.join("nodes.parquet")).expect("read");
    let second = parquet_export(&store, &output, &[]);
    assert!(
        second.contains("already current"),
        "the second export: {second}"
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        before,
        "a current export is not rewritten"
    );

    // A moved head: only the change is decoded.
    revise(&store);
    let third = parquet_export(&store, &output, &[]);
    assert!(
        third.contains("refreshed the export") && third.contains("2 changed ranges"),
        "the third export refreshes: {third}"
    );
    let revision = froe::store::Repository::open(&store)
        .expect("open")
        .head_record_identifier()
        .to_string();
    for name in ["nodes.parquet", "properties.parquet"] {
        let provenance = froe_export::read_export_provenance(&output.join(name))
            .expect("read")
            .expect("stamped");
        assert_eq!(
            provenance.revision(),
            revision,
            "{name} carries the new head's stamp"
        );
    }
    // The temporary files of the refresh never linger.
    let mut leftovers: Vec<String> = std::fs::read_dir(&output)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    leftovers.sort();
    assert_eq!(
        leftovers,
        vec![
            ".froe-export.lock".to_owned(),
            "nodes.parquet".to_owned(),
            "properties.parquet".to_owned(),
        ],
        "only the two tables and the lock file remain"
    );
}

#[test]
pub(crate) fn the_full_flag_rebuilds_the_export() {
    let directory = TestDirectory::new("parquet-full");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");

    parquet_export(&store, &output, &[]);
    let rebuilt = parquet_export(&store, &output, &["--full"]);
    assert!(
        rebuilt.contains("exported 2 nodes"),
        "--full runs a full export even when the existing one is current: {rebuilt}"
    );
    assert!(!rebuilt.contains("already current"), "{rebuilt}");
}

#[test]
pub(crate) fn a_compacted_store_requires_full_to_rebuild() {
    let directory = TestDirectory::new("parquet-compacted");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");
    parquet_export(&store, &output, &[]);

    let compact = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["compact", store.to_str().expect("path"), "--yes"])
        .output()
        .expect("run compact");
    assert!(
        compact.status.success(),
        "compact must succeed: {}",
        String::from_utf8_lossy(&compact.stderr)
    );

    // Compaction rewrites the journal to one line, so the stamped
    // revision is unprovable: indistinguishable from another
    // repository's export. Rebuilding takes the explicit flag.
    let before = std::fs::read(output.join("nodes.parquet")).expect("read");
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "parquet",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        !refused.status.success(),
        "an unprovable base is refused: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("does not resolve") && stderr.contains("--full"),
        "the refusal names the reason and the flag: {stderr}"
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        before,
        "the export survives the refusal"
    );

    let rebuilt = parquet_export(&store, &output, &["--full"]);
    assert!(
        rebuilt.contains("exported 2 nodes"),
        "the explicit rebuild completes: {rebuilt}"
    );
    // And after it, refreshes work against the new head again.
    let current = parquet_export(&store, &output, &[]);
    assert!(current.contains("already current"), "refreshed: {current}");
}

#[test]
pub(crate) fn an_export_of_another_repository_requires_full() {
    let directory = TestDirectory::new("parquet-cross-repo");
    let store_a = directory.path.join("store-a");
    let store_b = directory.path.join("store-b");
    std::fs::create_dir_all(&store_a).expect("create store a");
    std::fs::create_dir_all(&store_b).expect("create store b");
    populate(&store_a);
    populate(&store_b);
    let output = directory.path.join("export");

    // A complete, valid, same-scope export — of store B.
    parquet_export(&store_b, &output, &[]);
    let before = std::fs::read(output.join("nodes.parquet")).expect("read");

    // Exporting store A over it without --full refuses.
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store_a.to_str().expect("path"),
            "--format",
            "parquet",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        !refused.status.success(),
        "another repository's export must be refused: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        before,
        "the foreign repository's export survives"
    );

    let rebuilt = parquet_export(&store_a, &output, &["--full"]);
    assert!(
        rebuilt.contains("exported 2 nodes"),
        "the explicit rebuild completes: {rebuilt}"
    );
}

#[test]
pub(crate) fn a_foreign_parquet_file_requires_full_to_replace() {
    let directory = TestDirectory::new("parquet-foreign");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");
    std::fs::create_dir_all(&output).expect("create output directory");
    std::fs::write(output.join("nodes.parquet"), b"not a parquet file").expect("seed");

    // Without --full the export refuses to destroy data it does not own.
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "parquet",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        !refused.status.success(),
        "the export must refuse foreign files"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("refusing to replace") && stderr.contains("--full"),
        "the refusal names the escape hatch: {stderr}"
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        b"not a parquet file",
        "the foreign file survives"
    );

    // With --full the same directory is rebuilt explicitly.
    let rebuilt = parquet_export(&store, &output, &["--full"]);
    assert!(
        rebuilt.contains("exported 2 nodes"),
        "the explicit rebuild completes: {rebuilt}"
    );
    assert!(
        froe_export::read_export_provenance(&output.join("nodes.parquet"))
            .expect("read")
            .is_some(),
        "the replacement is a stamped froe export"
    );
}

#[test]
pub(crate) fn a_removed_export_root_fails_like_a_missing_path() {
    let directory = TestDirectory::new("parquet-root-removed");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);
    let output = directory.path.join("export");
    parquet_export(&store, &output, &["--path", "/content"]);
    let before = std::fs::read(output.join("nodes.parquet")).expect("read");

    // Commit a revision whose content tree has no /content.
    {
        let store = WritableRepository::open(&store).expect("open");
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        let root = writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &[])
            .expect("root");
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
            .expect("super root");
        writer.finish().expect("finish");
        let previous = store.head();
        assert!(store.compare_and_set_head(previous, head));
        store.close().expect("close");
    }

    // The same failure a first export of a missing path produces, and
    // the existing export stays untouched.
    let failed = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--format",
            "parquet",
            "--path",
            "/content",
            "--output",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        !failed.status.success(),
        "a vanished export root fails: {}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("no node at /content"),
        "the reason: {stderr}"
    );
    assert_eq!(
        std::fs::read(output.join("nodes.parquet")).expect("read"),
        before,
        "the existing export is preserved"
    );
}

#[test]
pub(crate) fn the_full_flag_applies_only_to_parquet() {
    let directory = TestDirectory::new("full-non-parquet");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--full",
            "--output",
            directory.path.join("content.jsonl").to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(!refused.status.success(), "--full must not apply here");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("--full applies only to the parquet format"),
        "the message: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}
