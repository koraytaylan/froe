//! `froe index dump`: Lucene index data out to oak-run's filesystem layout.
//!
//! `docs/analysis/index-lucene-storage.md` §6.1 specifies the layout. The
//! operation is **read-only against the store**: it takes no lock, opens the
//! repository read-only, and writes no byte inside it. The safety case
//! states that as a testable invariant — a file snapshot of the store taken
//! before and after a dump must be identical, `repo.lock` included, because
//! the dump never creates it.
//!
//! Each file is streamed through `OakDirectory` to a froe-named temporary
//! and renamed into place, so a file that appears under its real name is a
//! file that was written whole. The three metadata files are written last,
//! for the same reason: a directory carrying `index-details.txt` is one
//! whose index files are already there.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::content::node::NodeState;
use crate::error::{Error, Result};
use crate::index::lanes::AsyncLanes;
use crate::index::lucene::layout::{
    INDEX_DETAILS_FILE_NAME, INDEXER_INFO_FILE_NAME, IndexDetails, IndexerInfo,
    filesystem_directory_name, index_folder_base_name,
};
use crate::index::lucene::{OakDirectory, is_index_directory_name, is_suggest_directory_name};
use crate::index::{IndexDefinition, IndexType};
use crate::progress::{ProgressObserver, Step, WorkUnit, count, observe};
use crate::store::Repository;

/// The directory oak-run's importer scans for `index-details.txt`.
///
/// `froe index import --input <output>/index-dumps` and oak-run's
/// `--index-import-dir` both find the indexes at this level.
pub const INDEX_DUMPS_DIRECTORY_NAME: &str = "index-dumps";

/// The definitions file an out-of-band build carries.
pub const INDEX_DEFINITIONS_FILE_NAME: &str = "index-definitions.json";

/// The prefix every temporary this module writes carries.
///
/// A dump that died leaves these; the operator deletes the output directory,
/// and the never-overwrite guard blocks a rerun over it until they do.
const TEMPORARY_PREFIX: &str = ".froe-dump-";

/// The step opened while index files are written.
const DUMP_STEP: &str = "dumping index files";

/// What to dump, and where.
///
/// Private fields with `new` and `with_*` setters, as `CompactionOptions`
/// has: that is what makes an integration test build them the way a
/// downstream crate must, and what lets a later plan add an option without
/// breaking a struct literal.
#[derive(Clone, Debug)]
pub struct DumpOptions {
    indexes: Vec<String>,
    output: PathBuf,
}

impl DumpOptions {
    /// Dumps `indexes` — every Lucene definition when empty — under
    /// `output`.
    #[must_use]
    pub fn new(indexes: Vec<String>, output: PathBuf) -> Self {
        Self { indexes, output }
    }

    /// The definition paths requested, empty for all of them.
    #[must_use]
    pub fn indexes(&self) -> &[String] {
        &self.indexes
    }

    /// The directory `index-dumps` is written under.
    #[must_use]
    pub fn output(&self) -> &Path {
        &self.output
    }
}

/// What one definition's dump produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DumpedIndex {
    /// The definition's path.
    pub path: String,
    /// The directory its files were written under, relative to `index-dumps`.
    pub directory: String,
    /// Each file written, with its byte count.
    pub files: Vec<(String, u64)>,
    /// Directories reported but not copied, with the reason.
    ///
    /// A mount-decorated `:data` belongs to a composite store's other mount.
    /// froe reports it rather than copying it into a directory that claims
    /// to be this mount's.
    pub skipped_directories: Vec<String>,
}

/// Why no `indexer-info.properties` was written.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum NoCheckpointReason {
    /// The selection spans more than one lane, and the file names one
    /// checkpoint for the whole directory.
    SeveralLanes {
        /// The lanes, sorted.
        lanes: Vec<String>,
    },
    /// A selected definition is synchronous, so no lane checkpoint exists.
    SynchronousDefinition {
        /// The definition.
        path: String,
    },
    /// The lane's checkpoint no longer resolves in the store.
    DanglingCheckpoint {
        /// The lane.
        lane: String,
        /// The checkpoint it names.
        checkpoint: String,
    },
    /// The lane has no entry on `/:async` at all.
    LaneAbsent {
        /// The lane.
        lane: String,
    },
}

impl std::fmt::Display for NoCheckpointReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SeveralLanes { lanes } => write!(
                formatter,
                "the selection spans the lanes {}, and indexer-info.properties names one \
                 checkpoint for the whole directory; dump per lane with --index",
                lanes.join(", ")
            ),
            Self::SynchronousDefinition { path } => write!(
                formatter,
                "{path} is synchronous, so there is no lane checkpoint to record; the \
                 files are a backup and cannot be imported"
            ),
            Self::DanglingCheckpoint { lane, checkpoint } => write!(
                formatter,
                "lane {lane} names checkpoint {checkpoint}, which no longer resolves in \
                 the store"
            ),
            Self::LaneAbsent { lane } => {
                write!(formatter, "lane {lane} has no state on /:async")
            }
        }
    }
}

/// What a dump did.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct DumpOutcome {
    /// The `index-dumps` directory written.
    pub directory: PathBuf,
    /// One entry per dumped definition.
    pub indexes: Vec<DumpedIndex>,
    /// The checkpoint recorded, when every definition agreed on one.
    pub checkpoint: Option<String>,
    /// Why none was recorded, when none was.
    pub no_checkpoint_reason: Option<NoCheckpointReason>,
    /// Definitions named with `--index` that are not Lucene, by path.
    pub skipped_definitions: Vec<(String, String)>,
}

impl DumpOutcome {
    /// Every file written, across every definition.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.indexes.iter().map(|index| index.files.len()).sum()
    }

    /// Whether the directory oak-run's importer needs is complete.
    #[must_use]
    pub fn is_importable(&self) -> bool {
        self.checkpoint.is_some() && !self.indexes.is_empty()
    }
}

/// Dumps the selected Lucene indexes.
pub fn dump_lucene_indexes(repository: &Repository, options: &DumpOptions) -> Result<DumpOutcome> {
    dump_lucene_indexes_with_progress(repository, options, &mut crate::progress::DiscardedProgress)
}

/// Dumps exactly as [`dump_lucene_indexes`] does, reporting each file.
pub fn dump_lucene_indexes_with_progress(
    repository: &Repository,
    options: &DumpOptions,
    observer: &mut dyn ProgressObserver,
) -> Result<DumpOutcome> {
    let repository_directory = repository.directory();
    crate::tooling::output_directory::refuse_output_inside_repository(
        repository_directory,
        options.output(),
    )?;
    let dumps = options.output().join(INDEX_DUMPS_DIRECTORY_NAME);
    refuse_existing_dump_output(&dumps)?;

    let content_root = repository.content_root()?;
    let selected = select_definitions(repository, &content_root, options.indexes())?;
    std::fs::create_dir_all(&dumps)?;

    let step = Step::new(DUMP_STEP, WorkUnit::Files);
    let indexes = observe(observer, &step, |observer| {
        let mut written = Vec::new();
        let mut files_so_far = 0usize;
        for (path, node, definition) in &selected {
            let dumped = dump_one(
                repository,
                node,
                definition,
                path,
                &dumps,
                &mut |count_so_far| {
                    observer.step_advanced(count(files_so_far + count_so_far));
                },
            )?;
            files_so_far += dumped.files.len();
            written.push(dumped);
        }
        observer.step_advanced(count(files_so_far));
        Ok::<_, Error>(written)
    })?;

    // The metadata files last: a directory carrying `index-details.txt` is
    // one whose index files are already there.
    let (checkpoint, no_checkpoint_reason) = resolve_checkpoint(&content_root, &selected)?;
    write_definitions_file(repository, &dumps, &selected)?;
    if let Some(checkpoint) = &checkpoint {
        write_file_durably(
            &dumps.join(INDEXER_INFO_FILE_NAME),
            IndexerInfo {
                checkpoint: checkpoint.clone(),
            }
            .render()
            .as_bytes(),
        )?;
    }
    sync_directory(&dumps);

    Ok(DumpOutcome {
        directory: dumps,
        indexes,
        checkpoint,
        no_checkpoint_reason,
        skipped_definitions: Vec::new(),
    })
}

/// Refuses an `index-dumps` directory that already holds anything.
///
/// The never-overwrite rule. An interrupted dump leaves a partial file set,
/// and a rerun over it would produce a directory that is part one dump and
/// part another — importable-looking and wrong. The operator deletes it.
pub fn refuse_existing_dump_output(dumps: &Path) -> Result<()> {
    let Ok(mut entries) = std::fs::read_dir(dumps) else {
        return Ok(());
    };
    if entries.next().is_some() {
        return Err(Error::InvalidFormat {
            details: format!(
                "{} already holds a dump; froe never writes over one, because a \
                 directory that is part one dump and part another looks importable and \
                 is not. Delete it and rerun.",
                dumps.display()
            ),
        });
    }
    Ok(())
}

/// The Lucene definitions to dump, in path order.
fn select_definitions<'store>(
    repository: &'store Repository,
    content_root: &NodeState<'store>,
    requested: &[String],
) -> Result<Vec<(String, NodeState<'store>, IndexDefinition)>> {
    let _ = repository;
    let Some(oak_index) = content_root.child_node(crate::index::INDEX_DEFINITIONS_NAME)? else {
        return Ok(Vec::new());
    };
    let mut selected = Vec::new();
    for (name, node) in oak_index.child_node_entries()? {
        if name.starts_with(':') {
            continue;
        }
        let path = format!("/{}/{name}", crate::index::INDEX_DEFINITIONS_NAME);
        if !requested.is_empty() && !requested.iter().any(|wanted| wanted == &path) {
            continue;
        }
        let Ok(definition) = IndexDefinition::read(&node, &path) else {
            continue;
        };
        if definition.index_type != Some(IndexType::Lucene) {
            if requested.is_empty() {
                continue;
            }
            return Err(Error::InvalidFormat {
                details: format!("{path} is not a lucene definition, so there is nothing to dump"),
            });
        }
        selected.push((path, node, definition));
    }
    selected.sort_by(|left, right| left.0.cmp(&right.0));

    // Every explicitly named path is answered. A path naming nothing is a
    // typo or a definition somebody removed, and reporting "nothing to
    // dump" for it would send an operator away believing they had a backup.
    for wanted in requested {
        if !selected.iter().any(|(path, _, _)| path == wanted) {
            return Err(Error::InvalidFormat {
                details: format!("{wanted} names no lucene definition in this store"),
            });
        }
    }
    Ok(selected)
}

/// Dumps one definition's directories.
fn dump_one(
    repository: &Repository,
    node: &NodeState<'_>,
    definition: &IndexDefinition,
    path: &str,
    dumps: &Path,
    report: &mut dyn FnMut(usize),
) -> Result<DumpedIndex> {
    let base = index_folder_base_name(path);
    let index_directory = dumps.join(&base);
    std::fs::create_dir_all(&index_directory)?;

    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut mappings: BTreeMap<String, String> = BTreeMap::new();

    for (child_name, _) in node.child_node_entries()? {
        if !child_name.starts_with(':') {
            continue;
        }
        let is_index = is_index_directory_name(&child_name);
        let is_suggest = is_suggest_directory_name(&child_name);
        if !is_index && !is_suggest {
            continue;
        }
        // A mount-decorated directory belongs to a composite store's other
        // mount. Reported, never copied into a directory claiming to be
        // this mount's.
        if child_name != crate::index::lucene::INDEX_DATA_CHILD_NAME
            && child_name != crate::index::lucene::SUGGEST_DATA_CHILD_NAME
        {
            skipped.push(child_name);
            continue;
        }
        let Some(directory) =
            OakDirectory::open(repository, node, definition, &child_name).map_err(index_error)?
        else {
            continue;
        };
        let filesystem_name = filesystem_directory_name(&child_name);
        let target = index_directory.join(&filesystem_name);
        std::fs::create_dir_all(&target)?;
        mappings.insert(filesystem_name, child_name.clone());

        for file_name in directory.file_names() {
            let file = directory.file(file_name).map_err(index_error)?;
            let written = write_index_file(&target, file_name, &file)?;
            files.push((file_name.clone(), written));
            report(files.len());
        }
        sync_directory(&target);
    }

    write_file_durably(
        &index_directory.join(INDEX_DETAILS_FILE_NAME),
        IndexDetails {
            meta_format_version: 1,
            index_path: path.to_owned(),
            creation_time: epoch_milliseconds(),
            directory_mappings: mappings,
        }
        .render()
        .as_bytes(),
    )?;
    sync_directory(&index_directory);

    Ok(DumpedIndex {
        path: path.to_owned(),
        directory: base,
        files,
        skipped_directories: skipped,
    })
}

/// Streams one index file to a temporary and renames it into place.
fn write_index_file(
    target: &Path,
    file_name: &str,
    file: &crate::index::lucene::OakIndexFile<'_>,
) -> Result<u64> {
    let temporary = target.join(format!("{TEMPORARY_PREFIX}{file_name}"));
    let final_path = target.join(file_name);
    let written = {
        let mut handle = std::fs::File::create(&temporary).inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })?;
        let mut reader = file.reader();
        let copied = match std::io::copy(&mut reader, &mut handle) {
            Ok(copied) => copied,
            Err(error) => {
                // A returned error takes its temporary with it. The safety
                // case's dump row says exactly this: the completed file set
                // stays, the froe-named temporaries do not.
                drop(handle);
                let _ = std::fs::remove_file(&temporary);
                return Err(error.into());
            }
        };
        if let Err(error) = handle.sync_all() {
            drop(handle);
            let _ = std::fs::remove_file(&temporary);
            return Err(error.into());
        }
        copied
    };
    std::fs::rename(&temporary, &final_path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(written)
}

/// The definitions file, in the variant an out-of-band build dumps with.
fn write_definitions_file(
    repository: &Repository,
    dumps: &Path,
    selected: &[(String, NodeState<'_>, IndexDefinition)],
) -> Result<()> {
    let definitions: Vec<(String, NodeState<'_>)> = selected
        .iter()
        .map(|(path, node, _)| (path.clone(), *node))
        .collect();
    let rendered = crate::index::definitions_json::render(
        repository,
        &definitions,
        crate::index::definitions_json::RenderOptions {
            child_filter: crate::index::definitions_json::ChildFilter::OutOfBandBuild,
            ..crate::index::definitions_json::RenderOptions::default()
        },
    )
    .map_err(|error| Error::InvalidFormat {
        details: error.to_string(),
    })?;
    write_file_durably(
        &dumps.join(INDEX_DEFINITIONS_FILE_NAME),
        rendered.as_bytes(),
    )
}

/// The one checkpoint the whole directory records, or why there is none.
fn resolve_checkpoint(
    content_root: &NodeState<'_>,
    selected: &[(String, NodeState<'_>, IndexDefinition)],
) -> Result<(Option<String>, Option<NoCheckpointReason>)> {
    if selected.is_empty() {
        return Ok((None, None));
    }
    let mut lanes: Vec<String> = Vec::new();
    for (path, _, definition) in selected {
        let Some(lane) = definition.lane.clone() else {
            return Ok((
                None,
                Some(NoCheckpointReason::SynchronousDefinition { path: path.clone() }),
            ));
        };
        if !lanes.contains(&lane) {
            lanes.push(lane);
        }
    }
    lanes.sort();
    if lanes.len() > 1 {
        return Ok((None, Some(NoCheckpointReason::SeveralLanes { lanes })));
    }
    let lane = lanes.into_iter().next().expect("one lane");

    let async_lanes = AsyncLanes::read(content_root).map_err(|error| Error::InvalidFormat {
        details: error.to_string(),
    })?;
    let Some(checkpoint) = async_lanes
        .lane(&lane)
        .and_then(|state| state.checkpoint.clone())
    else {
        return Ok((None, Some(NoCheckpointReason::LaneAbsent { lane })));
    };
    // The checkpoint has to resolve: one that does not names a state no
    // build can be made at, and oak-run warns exactly that.
    let resolves = content_root
        .child_node("checkpoints")?
        .and_then(|checkpoints| checkpoints.child_node(&checkpoint).transpose())
        .transpose()?
        .is_some();
    if !resolves {
        return Ok((
            None,
            Some(NoCheckpointReason::DanglingCheckpoint { lane, checkpoint }),
        ));
    }
    Ok((Some(checkpoint), None))
}

/// Writes a small file and fsyncs it.
fn write_file_durably(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut handle = std::fs::File::create(path)?;
    handle.write_all(bytes)?;
    handle.sync_all()?;
    Ok(())
}

/// Fsyncs a directory so its entries are durable, best-effort.
fn sync_directory(path: &Path) {
    if let Ok(handle) = std::fs::File::open(path) {
        let _ = handle.sync_all();
    }
}

/// An index-layer failure, as a store error.
///
/// The two layers keep separate error types on purpose; this is the one
/// place the dump crosses between them, so the conversion is named rather
/// than blanket-implemented.
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
