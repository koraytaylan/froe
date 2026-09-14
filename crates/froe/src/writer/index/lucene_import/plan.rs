//! What an import would do, worked out without the lock.
//!
//! Every refusal below lands here or in `prepare`'s replan, which is to say
//! **before the first `:data` record is appended**. A refused import leaves
//! a store byte-identical to the one it found.

use std::path::{Path, PathBuf};

use crate::content::node::NodeState;
use crate::error::{Error, Result};
use crate::index::lanes::AsyncLanes;
use crate::index::lucene::layout::{
    INDEX_DETAILS_FILE_NAME, INDEXER_INFO_FILE_NAME, IndexDetails, IndexerInfo,
};
use crate::index::{IndexDefinition, IndexType};
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;

/// The definitions file an import reads beside the index directories.
pub const INDEX_DEFINITIONS_FILE_NAME: &str = "index-definitions.json";

/// What to import, and from where.
///
/// Private fields with `new` and `with_*` setters, as `CompactionOptions`
/// has, so a downstream crate builds one the same way.
#[derive(Clone, Debug)]
pub struct LuceneImportOptions {
    input: PathBuf,
    indexes: Vec<String>,
}

impl LuceneImportOptions {
    /// Imports every definition the directory holds.
    #[must_use]
    pub fn new(input: PathBuf) -> Self {
        Self {
            input,
            indexes: Vec::new(),
        }
    }

    /// Restricts the import to these definition paths.
    #[must_use]
    pub fn with_indexes(mut self, indexes: impl IntoIterator<Item = String>) -> Self {
        self.indexes = indexes.into_iter().collect();
        self
    }

    /// The directory to import from — oak-run's `index-dumps`.
    #[must_use]
    pub fn input(&self) -> &Path {
        &self.input
    }

    /// The definition paths requested, empty for all of them.
    #[must_use]
    pub fn indexes(&self) -> &[String] {
        &self.indexes
    }
}

/// One definition the import will write.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlannedImport {
    /// The definition's path in the store.
    pub path: String,
    /// The local directory its files come from.
    pub directory: PathBuf,
    /// Each hidden child to write, and the local directory holding it.
    pub mappings: Vec<(String, PathBuf)>,
    /// Mappings named in `index-details.txt` that will be skipped, with the
    /// reason. `:suggest-data` is the expected one.
    pub skipped_mappings: Vec<(String, String)>,
    /// How many files will be copied.
    pub file_count: usize,
    /// Their total size.
    pub byte_count: u64,
    /// The `reindexCount` the import will store: the file's plus one.
    pub reindex_count: u64,
}

/// What an import would do.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct LuceneImportPlan {
    /// The canonicalized store directory.
    pub directory: PathBuf,
    /// The directory imported from.
    pub input: PathBuf,
    /// The checkpoint `indexer-info.properties` names.
    pub checkpoint: String,
    /// One entry per definition to import, in path order.
    pub imports: Vec<PlannedImport>,
    /// Definitions the file carries that have no index directory, which the
    /// import ignores as oak-run's importer ignores them.
    pub definitions_without_directories: Vec<String>,
}

impl LuceneImportPlan {
    /// Whether the plan would write anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.imports.is_empty()
    }

    /// Every file the run will copy.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.imports.iter().map(|import| import.file_count).sum()
    }
}

/// The `:suggest-data` mapping's skip reason.
///
/// A recorded departure from oak-run's Lucene importer, which copies it.
/// Safe because Oak's own Lucene writer rebuilds the suggestions whenever
/// their `lastUpdated` is missing.
pub const SUGGEST_SKIP_REASON: &str = "froe never imports suggester data; Oak's own writer rebuilds it when its \
     lastUpdated is missing";

/// Reads the input directory and works out what an import would do.
pub fn plan_lucene_import(
    directory: &Path,
    options: &LuceneImportOptions,
) -> Result<LuceneImportPlan> {
    let directory = crate::writer::maintenance::canonical_repository_directory(directory)?;
    let repository = Repository::open(&directory)?;
    let plan = build_plan(&directory, &repository, options)?;
    drop(repository);
    Ok(plan)
}

/// The plan, against an already open repository.
pub(crate) fn build_plan(
    directory: &Path,
    repository: &Repository,
    options: &LuceneImportOptions,
) -> Result<LuceneImportPlan> {
    let input = options.input();
    let info = read_indexer_info(input)?;
    let local = read_local_directories(input)?;
    let definitions_file = read_definitions_file(input)?;

    let content_root = repository.content_root()?;
    let lanes = AsyncLanes::read(&content_root).map_err(index_error)?;
    let checkpoint_root = resolve_checkpoint_root(repository, &info.checkpoint)?;

    let mut imports = Vec::new();
    let mut refusals: Vec<String> = Vec::new();
    for (index_path, local_directory) in &local {
        if !options.indexes().is_empty()
            && !options.indexes().iter().any(|wanted| wanted == index_path)
        {
            continue;
        }
        match plan_one(
            repository,
            &content_root,
            &lanes,
            checkpoint_root,
            &info.checkpoint,
            index_path,
            local_directory,
            &definitions_file,
        ) {
            Ok(import) => imports.push(import),
            Err(refusal) => refusals.push(refusal.to_string()),
        }
    }

    if !refusals.is_empty() {
        // Every failing definition is named, not just the first: an
        // operator fixing a directory needs the whole list, and a second
        // run to discover the second problem is a second AEM outage.
        return Err(Error::InvalidFormat {
            details: format!(
                "{} of the {} definitions in {} cannot be imported:\n  {}",
                refusals.len(),
                local.len(),
                input.display(),
                refusals.join("\n  ")
            ),
        });
    }

    for wanted in options.indexes() {
        if !imports.iter().any(|import| &import.path == wanted) {
            return Err(Error::InvalidFormat {
                details: format!("{wanted} has no index directory in {}", input.display()),
            });
        }
    }

    let definitions_without_directories = definitions_file
        .definitions
        .keys()
        .filter(|path| !local.iter().any(|(index_path, _)| &index_path == path))
        .cloned()
        .collect();

    imports.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(LuceneImportPlan {
        directory: directory.to_owned(),
        input: input.to_owned(),
        checkpoint: info.checkpoint,
        imports,
        definitions_without_directories,
    })
}

/// One definition's plan, or the reason it cannot be imported.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a distinct fact the rule needs; bundling them would hide \
              which check reads which"
)]
fn plan_one(
    repository: &Repository,
    content_root: &NodeState<'_>,
    lanes: &AsyncLanes,
    checkpoint_root: RecordIdentifier,
    checkpoint: &str,
    index_path: &str,
    local_directory: &Path,
    definitions_file: &crate::index::definitions_json_reader::ParsedDefinitions,
) -> Result<PlannedImport> {
    let _ = content_root;
    let Some(node) = repository.node_at_path(index_path)? else {
        return Err(Error::InvalidFormat {
            details: format!(
                "{index_path} names no node in the store; adding a definition is oak-run's \
                 job, not an import's"
            ),
        });
    };
    let definition = IndexDefinition::read(&node, index_path).map_err(index_error)?;
    if definition.index_type != Some(IndexType::Lucene) {
        return Err(Error::InvalidFormat {
            details: format!("{index_path} is not a lucene definition"),
        });
    }

    // Asynchronous, and not hybrid. oak-run's own importer never completes
    // the synchronous case — its catch-up step skips the `sync` lane and
    // leaves the definition on `async = temp-sync` — so there is no Oak
    // behaviour to match and no oracle to prove against.
    let Some(lane) = definition.lane.clone() else {
        return Err(Error::InvalidFormat {
            details: format!(
                "{index_path} is synchronous, and oak-run's own importer never completes \
                 that case; there is no Oak behaviour for froe to match"
            ),
        });
    };
    if definition.indexing_mode.synchronous_synonym {
        return Err(Error::InvalidFormat {
            details: format!(
                "{index_path} is hybrid — it lists sync beside lane {lane} — and Oak keeps \
                 its synchronous property index in the hidden :property-index child, which \
                 froe does not build"
            ),
        });
    }

    // The state rule, per definition.
    let lane_checkpoint = lanes
        .lane(&lane)
        .and_then(|state| state.checkpoint.clone())
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("{index_path} indexes on lane {lane}, which has no state on /:async"),
        })?;
    let lane_root = resolve_checkpoint_root(repository, &lane_checkpoint)?;
    if lane_root != checkpoint_root {
        return Err(Error::InvalidFormat {
            details: format!(
                "{index_path} was built at checkpoint {checkpoint} (root {checkpoint_root}), \
                 but lane {lane} resumes from {lane_checkpoint} (root {lane_root}); rebuild \
                 at the lane's own checkpoint"
            ),
        });
    }

    // Definition drift, against the file's copy. The file's definition is
    // materialized into memory so both sides are node states, and the
    // comparison runs here — before the first record is appended to the
    // store — so a drifting file leaves the store byte-identical.
    let Some(parsed) = definitions_file.definitions.get(index_path) else {
        return Err(Error::InvalidFormat {
            details: format!(
                "{index_path} has an index directory but no entry in \
                 {INDEX_DEFINITIONS_FILE_NAME}; oak-run's importer requires the file to \
                 describe every directory"
            ),
        });
    };
    let materialized = crate::writer::index::lucene_import::materialize::materialize(parsed)?;
    let verdict = super::drift::compare(&materialized.node(), &node).map_err(index_error)?;
    if !verdict.is_clean() {
        return Err(index_error(super::drift::refusal(index_path, &verdict)));
    }

    let Mappings {
        written: mappings,
        skipped,
        file_count,
        byte_count,
    } = read_mappings(index_path, local_directory)?;

    // The file's `reindexCount` plus one, which is what the importer's data
    // step produces after Oak's own updater installed the file's value.
    let file_count_property = definitions_file
        .definitions
        .get(index_path)
        .and_then(|parsed| parsed.properties.get("reindexCount"))
        .and_then(|property| property.values.first())
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0);

    Ok(PlannedImport {
        path: index_path.to_owned(),
        directory: local_directory.to_owned(),
        mappings,
        skipped_mappings: skipped,
        file_count,
        byte_count,
        reindex_count: file_count_property + 1,
    })
}

/// What the directory mappings resolve to.
struct Mappings {
    written: Vec<(String, PathBuf)>,
    skipped: Vec<(String, String)>,
    file_count: usize,
    byte_count: u64,
}

/// Walks one definition's `index-details.txt` mappings.
fn read_mappings(index_path: &str, local_directory: &Path) -> Result<Mappings> {
    let details = read_index_details(local_directory)?;
    let mut resolved = Mappings {
        written: Vec::new(),
        skipped: Vec::new(),
        file_count: 0,
        byte_count: 0,
    };
    for (filesystem_name, jcr_name) in &details.directory_mappings {
        let source = local_directory.join(filesystem_name);
        if crate::index::lucene::is_suggest_directory_name(jcr_name) {
            resolved
                .skipped
                .push((jcr_name.clone(), SUGGEST_SKIP_REASON.to_owned()));
            continue;
        }
        if !crate::index::lucene::is_index_directory_name(jcr_name) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{index_path}'s {INDEX_DETAILS_FILE_NAME} maps {filesystem_name} to \
                     {jcr_name}, which is neither an index nor a suggester directory"
                ),
            });
        }
        for entry in std::fs::read_dir(&source)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                resolved.file_count += 1;
                resolved.byte_count += entry.metadata()?.len();
            }
        }
        resolved.written.push((jcr_name.clone(), source));
    }
    Ok(resolved)
}

/// `indexer-info.properties`, refused when absent.
fn read_indexer_info(input: &Path) -> Result<IndexerInfo> {
    let path = input.join(INDEXER_INFO_FILE_NAME);
    let content = std::fs::read_to_string(&path).map_err(|error| Error::InvalidFormat {
        details: format!(
            "{} could not be read ({error}); an import needs the checkpoint the index was \
             built at, and a dump writes none when it could not name one",
            path.display()
        ),
    })?;
    IndexerInfo::parse(&content)
}

/// One local index directory's `index-details.txt`.
fn read_index_details(local: &Path) -> Result<IndexDetails> {
    let path = local.join(INDEX_DETAILS_FILE_NAME);
    let content = std::fs::read_to_string(&path)?;
    IndexDetails::parse(&content)
}

/// Every local directory, by the index path its details name.
fn read_local_directories(input: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(input)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if !path.join(INDEX_DETAILS_FILE_NAME).is_file() {
            continue;
        }
        let details = read_index_details(&path)?;
        if details.index_path.is_empty() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{}/{INDEX_DETAILS_FILE_NAME} names no indexPath",
                    path.display()
                ),
            });
        }
        found.push((details.index_path, path));
    }
    found.sort();
    Ok(found)
}

/// The definitions file, parsed.
fn read_definitions_file(
    input: &Path,
) -> Result<crate::index::definitions_json_reader::ParsedDefinitions> {
    let path = input.join(INDEX_DEFINITIONS_FILE_NAME);
    let content = std::fs::read_to_string(&path).map_err(|error| Error::InvalidFormat {
        details: format!(
            "{} could not be read ({error}); oak-run's importer requires it and froe reads \
             it back to check the definition has not drifted",
            path.display()
        ),
    })?;
    crate::index::definitions_json_reader::parse(&content).map_err(index_error)
}

/// A checkpoint's `root` record, refused when the checkpoint is gone.
///
/// Checkpoints hang off the **super-root**, not the content root, which is
/// what `Repository::checkpoints` reads. A lookup under the content root
/// would find nothing in every real store and call every checkpoint
/// dangling.
fn resolve_checkpoint_root(repository: &Repository, checkpoint: &str) -> Result<RecordIdentifier> {
    let root = repository
        .checkpoints()?
        .into_iter()
        .find(|(name, _)| name == checkpoint)
        .map(|(_, node)| node.child_node("root"))
        .transpose()?
        .flatten()
        .ok_or_else(|| Error::InvalidFormat {
            details: format!("checkpoint {checkpoint} does not resolve in this store"),
        })?;
    Ok(root.record_identifier())
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
