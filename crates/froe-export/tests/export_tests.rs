//! End-to-end export tests: write a store with the froe writer, export
//! it, and check the emitted JSON lines byte for byte.

use froe::content::PropertyType;
use froe::store::Repository;
use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use froe::writer::store_writer::WritableRepository;
use froe_export::{JsonLinesSink, create_export_directory, create_export_output, export_subtree};

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("froe-export-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Writes a store whose content tree is `/content/jcr:content`, with a
/// `title` property on `/content`. Single-child chains keep the document
/// order deterministic.
fn populate(directory: &std::path::Path) {
    let store = WritableRepository::open(directory).expect("open");
    let generation = store.writing_generation().expect("generation");
    let mut writer = store.record_writer(generation);
    let page_content = writer
        .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("jcr:content");
    let title_value = writer.write_string("Hello").expect("title value");
    let content = writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::One {
                name: "jcr:content".to_owned(),
                node: page_content,
            },
            &[PropertyToWrite {
                name: "title".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Single(title_value),
            }],
        )
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
    assert!(store.compare_and_set_head(previous, head));
    store.close().expect("close");
}

fn export_to_string(
    repository: &Repository,
    path: &str,
    depth: Option<usize>,
) -> (Option<u64>, String) {
    let mut output = Vec::new();
    let mut sink = JsonLinesSink::new(&mut output);
    let written = export_subtree(repository, path, depth, &mut sink).expect("export");
    (written, String::from_utf8(output).expect("valid UTF-8"))
}

#[test]
fn exports_json_lines_in_document_order() {
    let directory = TestDirectory::new("document-order");
    populate(&directory.path);
    let repository = Repository::open(&directory.path).expect("open");

    let (written, output) = export_to_string(&repository, "/", None);
    assert_eq!(written, Some(3));
    assert_eq!(
        output,
        "{\"path\":\"/\",\"properties\":{}}\n\
         {\"path\":\"/content\",\"properties\":{\"jcr:primaryType\":\"nt:unstructured\",\
         \"title\":\"Hello\"}}\n\
         {\"path\":\"/content/jcr:content\",\"properties\":{\"jcr:primaryType\":\
         \"nt:unstructured\"}}\n"
    );
}

#[test]
fn the_depth_limit_bounds_the_export() {
    let directory = TestDirectory::new("depth-limit");
    populate(&directory.path);
    let repository = Repository::open(&directory.path).expect("open");

    let (written, output) = export_to_string(&repository, "/", Some(1));
    assert_eq!(written, Some(2));
    assert!(output.ends_with("{\"path\":\"/content\",\"properties\":{\"jcr:primaryType\":\"nt:unstructured\",\"title\":\"Hello\"}}\n"));
}

#[test]
fn a_missing_path_exports_nothing() {
    let directory = TestDirectory::new("missing-path");
    populate(&directory.path);
    let repository = Repository::open(&directory.path).expect("open");

    let (written, output) = export_to_string(&repository, "/absent", None);
    assert_eq!(written, None);
    assert!(output.is_empty(), "the sink must stay untouched");
}

#[test]
fn the_output_guard_refuses_existing_files() {
    let directory = TestDirectory::new("existing-output");
    populate(&directory.path);
    let occupied = directory.path.parent().expect("parent").join(format!(
        "froe-export-existing-output-file-{}",
        std::process::id()
    ));
    std::fs::write(&occupied, b"precious").expect("write");
    let error = create_export_output(&directory.path, &occupied).expect_err("must refuse");
    assert!(
        error.to_string().contains("already exists"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(&occupied).expect("read"),
        b"precious",
        "the existing file must be untouched"
    );
    let _ = std::fs::remove_file(&occupied);
}

#[test]
fn the_output_guard_refuses_paths_inside_the_repository() {
    let directory = TestDirectory::new("inside-repository");
    populate(&directory.path);
    let inside = directory.path.join("output.jsonl");
    let error = create_export_output(&directory.path, &inside).expect_err("must refuse");
    assert!(
        error
            .to_string()
            .contains("inside the repository directory"),
        "unexpected error: {error}"
    );
    assert!(!inside.exists(), "no file must be created");
}

#[test]
fn the_directory_guard_refuses_directories_inside_the_repository() {
    let directory = TestDirectory::new("directory-inside-repository");
    populate(&directory.path);
    let inside = directory.path.join("export").join("deeper");
    let error = create_export_directory(&directory.path, &inside).expect_err("must refuse");
    assert!(
        error
            .to_string()
            .contains("inside the repository directory"),
        "unexpected error: {error}"
    );
    assert!(
        !directory.path.join("export").exists(),
        "no directory must be created"
    );
}

#[test]
fn the_directory_guard_creates_directories_outside_the_repository() {
    let directory = TestDirectory::new("directory-outside-repository");
    populate(&directory.path);
    let outside = directory.path.parent().expect("parent").join(format!(
        "froe-export-outside-directory-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outside);
    let nested = outside.join("nested");
    create_export_directory(&directory.path, &nested).expect("create");
    assert!(nested.is_dir());
    // Creating it again is fine; only files are guarded against reuse.
    create_export_directory(&directory.path, &nested).expect("create again");
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn the_output_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new("owner-only");
    populate(&directory.path);
    let output = directory.path.parent().expect("parent").join(format!(
        "froe-export-owner-only-output-{}",
        std::process::id()
    ));
    let file = create_export_output(&directory.path, &output).expect("create");
    let mode = file.metadata().expect("metadata").permissions().mode();
    assert_eq!(
        mode & 0o777 & !0o600,
        0,
        "mode {mode:o} is broader than 0600"
    );
    drop(file);
    let _ = std::fs::remove_file(&output);
}
