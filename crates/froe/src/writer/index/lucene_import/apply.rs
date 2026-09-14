//! The ordered mutation sequence of an import.
//!
//! The same shape plan 0007's apply has, and for the same reasons:
//!
//! 1. **Index records appended** into fresh archives at a number above every
//!    physical name — additive, so nothing existing is touched.
//! 2. **The definition rewritten** through task 0706's `DefinitionEdits`, so
//!    one identity discipline governs every property and child it does not
//!    name.
//! 3. **Every imported file read back** through `OakDirectory` from the open
//!    session and compared byte for byte, before anything is published.
//!    That is the one semantic claim an import can make about content it did
//!    not compute, and it is made where failing it costs nothing.
//! 4. **One `compare_and_set_head`, one `flush`.**
//! 5. **A fresh reopen**, verifying the head and each new subtree.
//!
//! The session is closed on every path. Plan 0007 learned that the hard way:
//! a returned error that left the archive without its trailers made the next
//! `froe compact` refuse the whole store as damaged.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::index::IndexDefinition;
use crate::progress::{ProgressObserver, Step, WorkUnit, count, observe};
use crate::segment::record::RecordIdentifier;
use crate::writer::index::definition_update::{DefinitionEdits, DisablerVerdict, ReindexCount};
use crate::writer::index::lucene_directory::{DirectoryListing, OakDirectoryWriter};
use crate::writer::index::lucene_import::plan::PlannedImport;
use crate::writer::index::lucene_import::prepared::PreparedLuceneImport;
use crate::writer::record_writer::{
    PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};
use crate::writer::store_writer::WritableRepository;

/// The step opened while index files are copied in.
const COPY_STEP: &str = "copying index files";

/// The step opened while they are read back.
const VERIFY_STEP: &str = "verifying the imported index";

/// What one definition's import produced.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct ImportedIndex {
    /// The definition's path.
    pub path: String,
    /// Each file written, with its byte count.
    pub files: Vec<(String, u64)>,
    /// The fresh `uid` written to `:status`.
    pub unique_identifier: String,
    /// The `reindexCount` stored.
    pub reindex_count: u64,
    /// The hidden children dropped, as Oak's own definition updater drops
    /// them when it installs the file's node wholesale.
    pub dropped_hidden_children: Vec<String>,
    /// Mappings the import skipped, with the reason.
    pub skipped_mappings: Vec<(String, String)>,
}

/// What an import did.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct LuceneImportOutcome {
    /// One entry per definition, in plan order.
    pub indexes: Vec<ImportedIndex>,
    /// The checkpoint the state rule matched against.
    pub checkpoint: String,
    /// The head before the run.
    pub head_before: RecordIdentifier,
    /// The head after.
    pub head_after: RecordIdentifier,
}

impl LuceneImportOutcome {
    /// Whether the run moved the head.
    #[must_use]
    pub fn moved_the_head(&self) -> bool {
        self.head_before != self.head_after
    }

    /// Every file written, across every definition.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.indexes.iter().map(|index| index.files.len()).sum()
    }
}

/// Applies a prepared import.
pub(crate) fn apply_prepared(
    prepared: &PreparedLuceneImport,
    observer: &mut dyn ProgressObserver,
) -> Result<LuceneImportOutcome> {
    prepared.recheck_before_mutation()?;

    let store = WritableRepository::open_prepared(
        &prepared.directory,
        prepared.repository_lock.clone(),
        prepared.certified_archive_number,
    )?;
    let head_before = store.head();

    let written = write_every_index(&store, prepared, observer);

    // On the way out of a failure, put the session's head back where it was
    // found, then close: closing seals the archive, and a run that failed
    // after `compare_and_set_head` must not publish itself by closing.
    if written.is_err() {
        let current = store.head();
        if current != head_before {
            store.compare_and_set_head(current, head_before);
        }
    }
    let closed = store.close();
    let written = written?;
    closed?;

    verify_after_reopen(&prepared.directory, written.head_after, &written.indexes)?;

    Ok(LuceneImportOutcome {
        indexes: written.indexes,
        checkpoint: prepared.plan.checkpoint.clone(),
        head_before,
        head_after: written.head_after,
    })
}

/// What the mutating half produced.
struct WrittenImport {
    indexes: Vec<ImportedIndex>,
    head_after: RecordIdentifier,
}

/// Everything that needs the store open.
fn write_every_index(
    store: &WritableRepository,
    prepared: &PreparedLuceneImport,
    observer: &mut dyn ProgressObserver,
) -> Result<WrittenImport> {
    let head_before = store.head();
    let total = prepared.plan.file_count();

    let mut rewritten: BTreeMap<String, RecordIdentifier> = BTreeMap::new();
    let mut indexes = Vec::new();
    {
        let mut writer = store.record_writer(store.writing_generation()?);
        let step = Step::new(COPY_STEP, WorkUnit::Files).with_total(count(total));
        let mut copied = 0usize;
        observe(observer, &step, |observer| {
            for import in &prepared.plan.imports {
                let (record, imported) = import_one(store, &mut writer, import, &mut |written| {
                    observer.step_advanced(count(copied + written));
                })?;
                copied += imported.files.len();
                rewritten.insert(import.path.clone(), record);
                indexes.push(imported);
            }
            observer.step_advanced(count(copied));
            Ok::<_, Error>(())
        })?;
        // The definition records must be in the store before the spine that
        // references them is written.
        writer.finish()?;
    }

    let mut writer = store.record_writer(store.writing_generation()?);
    let head_after = rewrite_the_spine(store, &mut writer, head_before, &rewritten)?;

    let step = Step::new(VERIFY_STEP, WorkUnit::Files).with_total(count(total));
    observe(observer, &step, |observer| {
        verify_before_publication(store, prepared, &rewritten, observer)
    })?;

    writer.finish()?;
    if !store.compare_and_set_head(head_before, head_after) {
        return Err(Error::InvalidFormat {
            details: "the head moved while the import held the lock, which cannot happen \
                      and means the lock did not hold"
                .to_owned(),
        });
    }
    store.flush()?;

    Ok(WrittenImport {
        indexes,
        head_after,
    })
}

/// One definition: its directories written, then its node rewritten.
fn import_one<Sink: SegmentSink>(
    store: &WritableRepository,
    writer: &mut RecordWriter<Sink>,
    import: &PlannedImport,
    report: &mut dyn FnMut(usize),
) -> Result<(RecordIdentifier, ImportedIndex)> {
    let definition_node = store
        .head_node()
        .child_node("root")?
        .and_then(|root| descend(&root, &import.path).transpose())
        .transpose()?
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("{} vanished between the plan and the apply", import.path),
        })?;
    let definition = IndexDefinition::read(&definition_node, &import.path).map_err(index_error)?;

    let mut hidden_children = Vec::new();
    let mut files = Vec::new();
    for (jcr_name, source) in &import.mappings {
        let (record, written) = write_directory(writer, &definition, source, &mut |count| {
            report(files.len() + count);
        })?;
        files.extend(written);
        hidden_children.push((jcr_name.clone(), record));
    }

    // The bookkeeping oak-run performs, composed into one rewrite.
    let unique_identifier = epoch_milliseconds().to_string();
    let status = write_status_node(writer, &unique_identifier)?;
    hidden_children.push((":status".to_owned(), status));

    let dropped: Vec<String> = definition_node
        .child_node_entries()?
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| {
            name.starts_with(':') && !hidden_children.iter().any(|(kept, _)| kept == name)
        })
        .collect();

    let mut edits = DefinitionEdits::reindexed(DisablerVerdict::Leave, hidden_children);
    edits.reindex_count = ReindexCount::Set(import.reindex_count);
    // Oak's own cycle clears `corrupt`, and the importer's definition-refresh
    // step removes `indexImportState`.
    edits.property_removals.push("corrupt".to_owned());
    edits.property_removals.push("indexImportState".to_owned());

    let record = crate::writer::index::definition_update::rewrite_definition(
        store,
        writer,
        &definition_node,
        &edits,
    )?;

    Ok((
        record,
        ImportedIndex {
            path: import.path.clone(),
            files,
            unique_identifier,
            reindex_count: import.reindex_count,
            dropped_hidden_children: dropped,
            skipped_mappings: import.skipped_mappings.clone(),
        },
    ))
}

/// One local directory, streamed into a `:data`-shaped subtree.
fn write_directory<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    definition: &IndexDefinition,
    source: &Path,
    report: &mut dyn FnMut(usize),
) -> Result<(RecordIdentifier, Vec<(String, u64)>)> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(source)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .collect();
    names.sort();

    // The definition's own settings, already resolved by the model: the
    // blob size clamped to Oak's minimum, and whether Oak would write a
    // `dirListing` at all. Re-deriving either here would let the writer and
    // the reader disagree about the same definition.
    let blob_size = definition.lucene.blob_size;
    let listing = if definition.lucene.save_directory_listing {
        DirectoryListing::Saved
    } else {
        DirectoryListing::Omitted
    };
    let mut builder = OakDirectoryWriter::new(writer, blob_size, listing);

    let mut written = Vec::new();
    for path in names {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("{} has a name that is not UTF-8", path.display()),
            })?
            .to_owned();
        let length = std::fs::metadata(&path)?.len();
        // One file at a time, streamed: the largest file bounds memory, not
        // the index.
        let handle = std::fs::File::open(&path)?;
        builder.add_file(&name, handle)?;
        written.push((name, length));
        report(written.len());
    }
    Ok((builder.finish()?, written))
}

/// The `:status` node oak-run's importer leaves behind.
///
/// `uid` alone. The other three properties task 0601 records on a `:status`
/// node — `lastUpdated`, `indexedNodes` and `reindexCompletionTimestamp` —
/// are deliberately absent: Oak's importer leaves no post-import state to
/// copy them from. A recorded departure, visible to `froe index list` and to
/// Oak's own printer.
fn write_status_node<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    unique_identifier: &str,
) -> Result<RecordIdentifier> {
    let value = writer.write_string(unique_identifier)?;
    writer.write_node(
        None,
        &[],
        &crate::writer::record_writer::ChildNodesToWrite::Zero,
        &[PropertyToWrite {
            name: "uid".to_owned(),
            property_type: crate::PropertyType::String,
            values: PropertyValuesToWrite::Single(value),
        }],
    )
}

/// Rewrites `/oak:index` and the spine above it.
fn rewrite_the_spine<Sink: SegmentSink>(
    store: &WritableRepository,
    writer: &mut RecordWriter<Sink>,
    head: RecordIdentifier,
    rewritten: &BTreeMap<String, RecordIdentifier>,
) -> Result<RecordIdentifier> {
    let super_root = store.head_node();
    let content_root = super_root
        .child_node("root")?
        .ok_or_else(|| Error::InvalidFormat {
            details: "the super-root has no \"root\" child node".to_owned(),
        })?;
    let oak_index = content_root
        .child_node(crate::index::INDEX_DEFINITIONS_NAME)?
        .ok_or_else(|| Error::InvalidFormat {
            details: "the store has no /oak:index".to_owned(),
        })?;

    let mut definition_edits = crate::writer::commit::ChildEdits::new();
    for (path, record) in rewritten {
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        definition_edits.insert(name, Some(*record));
    }
    let new_oak_index = crate::writer::commit::rewrite_node_with_child_edits(
        store,
        writer,
        Some(oak_index.record_identifier()),
        &definition_edits,
    )?;

    let mut root_edits = crate::writer::commit::ChildEdits::new();
    root_edits.insert(
        crate::index::INDEX_DEFINITIONS_NAME.to_owned(),
        Some(new_oak_index),
    );
    let new_root = crate::writer::commit::rewrite_node_with_child_edits(
        store,
        writer,
        Some(content_root.record_identifier()),
        &root_edits,
    )?;

    let mut super_edits = crate::writer::commit::ChildEdits::new();
    super_edits.insert("root".to_owned(), Some(new_root));
    crate::writer::commit::rewrite_node_with_child_edits(store, writer, Some(head), &super_edits)
}

/// Every imported file read back through the open session and compared.
///
/// The one semantic claim an import can make about content it did not
/// compute, made before publication so that failing it costs nothing.
fn verify_before_publication(
    store: &WritableRepository,
    prepared: &PreparedLuceneImport,
    rewritten: &BTreeMap<String, RecordIdentifier>,
    observer: &mut dyn ProgressObserver,
) -> Result<()> {
    let mut verified = 0usize;
    for import in &prepared.plan.imports {
        let record = rewritten
            .get(&import.path)
            .copied()
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("{} was planned but not written", import.path),
            })?;
        let node = crate::content::node::NodeState::new(store, record);
        let definition = IndexDefinition::read(&node, &import.path).map_err(index_error)?;

        for (jcr_name, source) in &import.mappings {
            let directory =
                crate::index::lucene::OakDirectory::open(store, &node, &definition, jcr_name)
                    .map_err(index_error)?
                    .ok_or_else(|| Error::InvalidFormat {
                        details: format!("{} has no {jcr_name} after the import", import.path),
                    })?;
            for file_name in directory.file_names() {
                let file = directory.file(file_name).map_err(index_error)?;
                let mut stored = Vec::new();
                file.reader().read_to_end(&mut stored)?;
                let original = std::fs::read(source.join(file_name))?;
                if stored != original {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "{}'s {jcr_name}/{file_name} reads back as {} bytes where the \
                             file on disk holds {}; the import is refused before anything \
                             is published",
                            import.path,
                            stored.len(),
                            original.len()
                        ),
                    });
                }
                verified += 1;
                observer.step_advanced(count(verified));
            }
        }
    }
    Ok(())
}

/// The head, the subtrees and the single journal line, after a reopen.
fn verify_after_reopen(
    directory: &Path,
    published: RecordIdentifier,
    indexes: &[ImportedIndex],
) -> Result<()> {
    let repository = crate::store::Repository::open(directory)?;
    if repository.head_record_identifier() != published {
        return Err(Error::InvalidFormat {
            details: format!(
                "the reopened store's head is {} rather than the {published} just published",
                repository.head_record_identifier()
            ),
        });
    }
    for index in indexes {
        let node = repository
            .node_at_path(&index.path)?
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("the published head does not reach {}", index.path),
            })?;
        crate::tooling::check::verify_node_tree(&repository, node.record_identifier())?;
    }
    Ok(())
}

/// Resolves an absolute path under `root`.
fn descend<'store>(
    root: &crate::content::node::NodeState<'store>,
    path: &str,
) -> Result<Option<crate::content::node::NodeState<'store>>> {
    let mut node = *root;
    for element in path.split('/').filter(|element| !element.is_empty()) {
        let Some(child) = node.child_node(element)? else {
            return Ok(None);
        };
        node = child;
    }
    Ok(Some(node))
}

/// An index-layer failure, as a store error.
fn index_error(error: crate::index::IndexError) -> Error {
    match error {
        crate::index::IndexError::Record(source) => source,
        other => Error::InvalidFormat {
            details: other.to_string(),
        },
    }
}

/// Now, in epoch milliseconds.
fn epoch_milliseconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(0))
}
