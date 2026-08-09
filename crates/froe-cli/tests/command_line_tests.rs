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
