//! Command-line compatibility tests: the `extract` spelling shipped in
//! v0.1.0 and must keep working as a hidden alias of `export`.

use froe::writer::record_writer::ChildNodesToWrite;
use froe::writer::store_writer::WritableRepository;

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Writes a store whose content tree is `/content`.
fn populate(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let content = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("content");
    let root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "content".to_owned(),
                node: content,
            },
            &[],
        )
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
    assert!(store.set_head(previous, head));
    store.close().expect("close");
}

#[test]
fn the_extract_alias_produces_the_export_output() {
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
fn the_extract_alias_writes_output_files() {
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
fn the_sqlite_format_writes_a_database_file() {
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
fn the_sqlite_format_leaves_an_existing_file_untouched() {
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
fn the_export_reports_progress_and_a_summary_on_stderr() {
    let directory = TestDirectory::new("export-progress");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let export = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args(["export", store.to_str().expect("path")])
        .output()
        .expect("run export");
    assert!(
        export.status.success(),
        "export must succeed: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let stderr = String::from_utf8_lossy(&export.stderr);
    assert!(
        stderr.contains("exported 2 nodes"),
        "the summary must report the node count: {stderr}"
    );
    assert!(
        stderr.contains("nodes/s"),
        "the summary must report the rate: {stderr}"
    );
}

#[test]
fn the_quiet_flag_silences_progress_but_keeps_the_summary() {
    let directory = TestDirectory::new("export-quiet");
    let store = directory.path.join("segmentstore");
    std::fs::create_dir_all(&store).expect("create store directory");
    populate(&store);

    let export = std::process::Command::new(env!("CARGO_BIN_EXE_froe"))
        .args([
            "export",
            store.to_str().expect("path"),
            "--quiet",
            "--output",
            directory.path.join("content.jsonl").to_str().expect("path"),
        ])
        .output()
        .expect("run export");
    assert!(
        export.status.success(),
        "export --quiet must succeed: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let stderr = String::from_utf8_lossy(&export.stderr);
    assert!(
        !stderr.contains("nodes ("),
        "quiet must silence the progress reports: {stderr}"
    );
    assert!(
        stderr.contains("exported 2 nodes"),
        "the summary must still be printed: {stderr}"
    );
}
