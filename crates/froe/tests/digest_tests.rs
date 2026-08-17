//! The content digest, against a real repository on disk.
//!
//! The unit tests beside the implementation cover the grammar — escaping,
//! parsing, comparison. These cover the walk: that it reaches the whole
//! super-root, renders types and arity the way a comparison needs, orders
//! itself by name rather than by storage, streams binary content, and
//! reports the invariants it can judge on its own.

use std::path::{Path, PathBuf};

use froe::content::property::PropertyType;
use froe::store::Repository;
use froe::tooling::digest::{compare_digests, digest_repository};
use froe::writer::record_writer::{ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite};
use froe::writer::store_writer::WritableRepository;

/// A checkpoint name in Oak's shape, so the `/:async` closure check treats
/// it as a reference rather than as an ordinary string.
const CHECKPOINT_NAME: &str = "8b3d5f2a-1c4e-4a7b-9f01-2d3e4f5a6b7c";

/// A checkpoint that no longer exists, for the dangling-reference case.
const RETIRED_CHECKPOINT_NAME: &str = "0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";

/// Long enough to be stored as a block list rather than materialized in a
/// single record, so the digest's streaming path is the one under test.
const BINARY_CONTENT_BYTES: usize = 20_000;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-digest-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test repository directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn binary_content() -> Vec<u8> {
    (0..BINARY_CONTENT_BYTES)
        .map(|index| (index % 251) as u8)
        .collect()
}

/// Writes a repository exercising every rendering decision the digest
/// makes: several property types, both arities, an inline binary large
/// enough to be blocked, children whose name order differs from the order
/// a map stores them in, and a checkpoint.
///
/// `async_reference`, when given, becomes a string property on `/:async`,
/// which is how Oak records an index lane's resume point.
fn build_repository(directory: &Path, async_reference: Option<&str>) {
    let store = WritableRepository::open(directory).expect("open the store directory");
    let generation = store.writing_generation().expect("the writing generation");
    let mut writer = store.record_writer(generation);

    // Deliberately not in name order: the digest has to sort, and passing
    // them already sorted would not prove that it does.
    let content = write_content_node(&mut writer);

    let mut root_children = vec![("content".to_owned(), content)];
    if let Some(reference) = async_reference {
        let reference_value = writer.write_string(reference).expect("async value");
        // Alongside the resume point, always a `-temp` entry naming a
        // checkpoint that is already gone. That is what Oak's own
        // `async-temp` looks like on a pristine store — it lists
        // checkpoints the indexer intends to release, and releasing them
        // is the point — so a store with this in it must still read clean.
        let released_value = writer
            .write_string(RETIRED_CHECKPOINT_NAME)
            .expect("async-temp value");
        let async_state = writer
            .write_node(
                None,
                &[],
                &ChildNodesToWrite::Zero,
                &[
                    PropertyToWrite {
                        name: "async".to_owned(),
                        property_type: PropertyType::String,
                        values: PropertyValuesToWrite::Single(reference_value),
                    },
                    PropertyToWrite {
                        name: "async-temp".to_owned(),
                        property_type: PropertyType::String,
                        values: PropertyValuesToWrite::Multiple(vec![released_value]),
                    },
                ],
            )
            .expect("write the async state");
        root_children.push((":async".to_owned(), async_state));
    }

    let root = writer
        .write_node(
            Some("rep:root"),
            &[],
            &ChildNodesToWrite::Many(root_children),
            &[],
        )
        .expect("write the content root");

    // A checkpoint sharing the content root, which is what a real one does.
    let created = writer.write_string("1750000000000").expect("created");
    let checkpoint = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: "root".to_owned(),
                node: root,
            },
            &[PropertyToWrite {
                name: "created".to_owned(),
                property_type: PropertyType::Long,
                values: PropertyValuesToWrite::Single(created),
            }],
        )
        .expect("write the checkpoint");
    let checkpoints = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::One {
                name: CHECKPOINT_NAME.to_owned(),
                node: checkpoint,
            },
            &[],
        )
        .expect("write the checkpoints container");

    let super_root = writer
        .write_node(
            None,
            &[],
            &ChildNodesToWrite::Many(vec![
                ("root".to_owned(), root),
                ("checkpoints".to_owned(), checkpoints),
            ]),
            &[],
        )
        .expect("write the super-root");

    writer.finish().expect("finish the writer");
    let previous = store.head();
    assert!(store.set_head(previous, super_root), "advance the head");
    store.close().expect("close the store");
}

/// Writes `/content` and its three leaves, exercising both property
/// arities, four property types, a mixin, and an inline binary.
fn write_content_node<Sink: froe::writer::record_writer::SegmentSink>(
    writer: &mut froe::writer::record_writer::RecordWriter<Sink>,
) -> froe::segment::record::RecordIdentifier {
    let title = writer.write_string("Alpha").expect("title value");
    let first_tag = writer.write_string("one").expect("first tag");
    let second_tag = writer.write_string("two").expect("second tag");
    let count = writer.write_string("42").expect("count value");
    let enabled = writer.write_string("true").expect("boolean value");
    let alpha = writer
        .write_node(
            Some("nt:unstructured"),
            &["mix:versionable".to_owned()],
            &ChildNodesToWrite::Zero,
            &[
                PropertyToWrite {
                    name: "jcr:title".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::Single(title),
                },
                PropertyToWrite {
                    name: "tags".to_owned(),
                    property_type: PropertyType::String,
                    values: PropertyValuesToWrite::Multiple(vec![first_tag, second_tag]),
                },
                PropertyToWrite {
                    name: "count".to_owned(),
                    property_type: PropertyType::Long,
                    values: PropertyValuesToWrite::Single(count),
                },
                PropertyToWrite {
                    name: "enabled".to_owned(),
                    property_type: PropertyType::Boolean,
                    values: PropertyValuesToWrite::Single(enabled),
                },
            ],
        )
        .expect("write the alpha node");

    let data = writer
        .write_binary_content(&binary_content())
        .expect("write the binary");
    let zebra = writer
        .write_node(
            Some("nt:resource"),
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(data),
            }],
        )
        .expect("write the zebra node");

    let middle = writer
        .write_node(Some("nt:folder"), &[], &ChildNodesToWrite::Zero, &[])
        .expect("write the middle node");

    writer
        .write_node(
            Some("nt:unstructured"),
            &[],
            &ChildNodesToWrite::Many(vec![
                ("zebra".to_owned(), zebra),
                ("alpha".to_owned(), alpha),
                ("middle".to_owned(), middle),
            ]),
            &[],
        )
        .expect("write the content node")
}

fn digest_of(directory: &Path) -> (String, froe::tooling::digest::DigestSummary) {
    let repository = Repository::open(directory).expect("open the repository");
    let mut rendered = Vec::new();
    let summary = digest_repository(&repository, &mut rendered).expect("digest the repository");
    (
        String::from_utf8(rendered).expect("the digest is valid UTF-8"),
        summary,
    )
}

/// The line for `path`, without its path prefix.
fn line_for<'digest>(digest: &'digest str, path: &str) -> &'digest str {
    digest
        .lines()
        .find_map(|line| match line.split_once('\t') {
            Some((candidate, properties)) if candidate == path => Some(properties),
            _ if line == path => Some(""),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no line for {path} in:\n{digest}"))
}

#[test]
fn the_digest_renders_types_and_arity_so_a_comparison_can_see_them() {
    let directory = TestDirectory::new("types");
    build_repository(&directory.path, None);
    let (digest, summary) = digest_of(&directory.path);

    let alpha = line_for(&digest, "/content/alpha");
    // Arity is the whole point: a single-valued and a multi-valued
    // property of the same type and value must not render alike, because
    // a template mix-up changes exactly that and nothing else.
    assert!(
        alpha.contains("tags=String[]:one\u{1F}two"),
        "multi-valued properties carry their arity and every value: {alpha}"
    );
    assert!(
        alpha.contains("jcr:title=String:Alpha"),
        "single-valued properties carry no arity marker: {alpha}"
    );
    // Types are rendered, not inferred from the value's spelling.
    assert!(alpha.contains("count=Long:42"), "{alpha}");
    assert!(alpha.contains("enabled=Boolean:true"), "{alpha}");
    // The synthesized node-state properties are part of the content view.
    assert!(
        alpha.contains("jcr:primaryType=Name:nt:unstructured"),
        "{alpha}"
    );
    assert!(
        alpha.contains("jcr:mixinTypes=Name[]:mix:versionable"),
        "mixins render as the multi-valued name property Oak presents: {alpha}"
    );

    assert!(
        summary.is_clean(),
        "no lookup or checkpoint problems: {summary:?}"
    );
    assert_eq!(summary.lookup_failures, 0);
    assert_eq!(summary.checkpoints, 1);
}

#[test]
fn the_digest_orders_by_name_not_by_how_the_map_stores_children() {
    let directory = TestDirectory::new("ordering");
    build_repository(&directory.path, None);
    let (digest, _) = digest_of(&directory.path);

    // The children were written zebra, alpha, middle, and a map stores
    // them by scrambled hash. Neither order may leak into the digest, or
    // two legal encodings of identical content would compare unequal.
    let position = |path: &str| {
        digest
            .lines()
            .position(|line| line.split('\t').next() == Some(path))
            .unwrap_or_else(|| panic!("no line for {path}"))
    };
    assert!(
        position("/content/alpha") < position("/content/middle"),
        "children are emitted in name order"
    );
    assert!(position("/content/middle") < position("/content/zebra"));

    // Properties are sorted within the line for the same reason.
    let alpha = line_for(&digest, "/content/alpha");
    let names: Vec<&str> = alpha
        .split('\t')
        .filter_map(|property| property.split('=').next())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(
        names, sorted,
        "properties are emitted in name order: {alpha}"
    );
}

#[test]
fn the_digest_reaches_the_super_root_and_every_checkpoint() {
    let directory = TestDirectory::new("scope");
    build_repository(&directory.path, None);
    let (digest, summary) = digest_of(&directory.path);

    // A digest scoped to the content tree would miss all of this, and
    // checkpoint content is exactly what maintenance is supposed to
    // preserve.
    assert!(
        digest.lines().any(|line| line.starts_with("#super-root")),
        "the super-root's own properties are rendered"
    );
    let checkpoint_root = format!("#checkpoint/{CHECKPOINT_NAME}");
    assert!(
        digest
            .lines()
            .any(|line| line.starts_with(&checkpoint_root)),
        "the checkpoint node itself is rendered"
    );
    assert!(
        line_for(&digest, &checkpoint_root).contains("created=Long:1750000000000"),
        "the checkpoint's own properties are rendered, expiry included"
    );
    assert!(
        digest
            .lines()
            .any(|line| line.starts_with(&format!("{checkpoint_root}/root/content/alpha"))),
        "the checkpoint's content snapshot is rendered too"
    );
    assert_eq!(summary.checkpoints, 1);
}

#[test]
fn binary_content_is_read_and_checksummed_rather_than_named_by_record() {
    let directory = TestDirectory::new("binaries");
    build_repository(&directory.path, None);
    let (digest, summary) = digest_of(&directory.path);

    let content = binary_content();
    let expected = format!(
        "jcr:data=Binary:{}/{}@{:08x}",
        content.len(),
        content.len(),
        froe::checksum::crc32(&content)
    );
    let zebra = line_for(&digest, "/content/zebra");
    assert!(
        zebra.contains(&expected),
        "the binary renders as its length and content checksum, so a changed \
         binary is a changed line: expected {expected} in {zebra}"
    );

    // Counted once even though the checkpoint shares the same subtree? No:
    // the checkpoint is a second path to the same binary and is rendered
    // there too, so both readings are accounted for.
    assert_eq!(summary.binaries, 2, "content tree and checkpoint snapshot");
    assert_eq!(summary.binary_bytes, 2 * content.len() as u64);
}

#[test]
fn digesting_the_same_store_twice_is_byte_identical() {
    let directory = TestDirectory::new("determinism");
    build_repository(&directory.path, None);

    // Without this the tool is useless for its purpose: every comparison
    // would report a difference and the operator would learn to ignore it.
    let (first, first_summary) = digest_of(&directory.path);
    let (second, second_summary) = digest_of(&directory.path);
    assert_eq!(first, second, "the digest is deterministic");
    assert_eq!(first_summary, second_summary);
    assert!(
        compare_digests(&first, &second).is_empty(),
        "comparison agrees with byte equality"
    );
    assert!(
        first.lines().count() > 5,
        "the digest is not vacuously empty"
    );
}

#[test]
fn a_checkpoint_that_async_still_references_but_no_longer_exists_is_reported() {
    let present = TestDirectory::new("async-present");
    build_repository(&present.path, Some(CHECKPOINT_NAME));
    let (_, summary) = digest_of(&present.path);
    assert!(
        summary.dangling_async_checkpoints.is_empty(),
        "a reference to a checkpoint that exists is not a finding, and neither is a \
         `-temp` entry naming one already released — a check that fired on either would \
         fail on every pristine Oak store, which is worse than no check at all: {summary:?}"
    );
    assert!(summary.is_clean());

    // The damage this catches: maintenance retired a checkpoint an index
    // lane still resumes from. Oak boots, serves content, and silently
    // reindexes from scratch — there is no error anywhere.
    let retired = TestDirectory::new("async-retired");
    build_repository(&retired.path, Some(RETIRED_CHECKPOINT_NAME));
    let (_, summary) = digest_of(&retired.path);
    assert_eq!(
        summary.dangling_async_checkpoints,
        vec![RETIRED_CHECKPOINT_NAME.to_owned()],
        "a reference to a checkpoint that is gone is reported"
    );
    assert!(!summary.is_clean(), "and the run is not clean");
}
