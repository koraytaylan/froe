//! The index inventory: one record per definition, aggregating what Oak's own
//! index printer prints and what froe can add read-only.
//!
//! # One malformed definition must never kill a listing
//!
//! That is the rule the error handling here follows, and it is not the
//! obvious one. Anything attributable to a **single** definition — a typed
//! error from its model construction, a stale stored definition, a dangling
//! lane checkpoint, a `valuePattern` froe cannot evaluate, a type froe does
//! not know — is carried on that definition's own [`IndexInfo`] as a typed
//! field or a warning naming the path and the kind. [`IndexInventory::collect`]
//! returns an error only for a failure that belongs to no definition: an
//! input or record failure reading `/oak:index`, `/:async` or `/checkpoints`.
//!
//! An operator whose store has one broken definition needs to see the other
//! twenty-two. Oak's own index-information service takes the opposite line and
//! throws, which is why the interop comparison runs over the intersection of
//! the two listings rather than expecting them to agree on the set.
//!
//! # Where Oak throws, froe warns
//!
//! Enumerating non-root definitions needs the node-type index, and Oak's index
//! path service refuses the whole enumeration when it cannot use it. The
//! inventory mirrors the three cases that service distinguishes but **warns**
//! where it throws, so `froe index list` runs on a store where
//! `froe index definitions` correctly refuses: a listing that shows what is
//! there is useful, while a definitions file with the wrong key set is worse
//! than none.
//!
//! A checkpoint that cannot be *read* is never reported as dangling. Dangling
//! means resolved and absent; reporting an unreadable checkpoint as gone would
//! send an operator to delete something that is still there.

use crate::content::SegmentProvider;
use crate::content::node::NodeState;
use crate::index::counter::{CountBound, CounterIndex, NodeCountEstimate, estimated_node_count};
use crate::index::definition::{IndexDefinition, IndexType};
use crate::index::lanes::AsyncLanes;
use crate::index::lucene::{
    OakDirectory, SUGGEST_DATA_CHILD_NAME, is_index_directory_name, is_suggest_directory_name,
};
use crate::index::property::mirror::MirrorIndex;
use crate::index::property::unique::UniqueIndex;
use crate::index::status::{
    DefinitionDifference, STORED_DEFINITION_NODE_NAME, StatusNode, StoredDefinition,
    definition_drift,
};
use crate::index::{
    INDEX_CONTENT_NODE_NAME, INDEX_DEFINITIONS_NAME, INDEX_DEFINITIONS_NODE_TYPE, IndexError,
    IndexResult, IndexWarning, strict_name,
};
use crate::progress::{DiscardedProgress, ProgressObserver, Step, WorkUnit, count, observe};

/// The step this walk reports.
const INVENTORY_STEP: &str = "inventorying indexes";

/// The two hidden-child facts Oak's index printer reports for a Lucene
/// definition, kept together because they are one question — what shape the
/// definition's hidden children say it is — rather than two.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HiddenChildFacts {
    /// A composite-store mount child, which froe reports but does not model.
    pub has_mount: bool,
    /// `:property-index`, the synchronous half of a hybrid index. It is the
    /// only child Oak ever flags `retainNodeInReindex`.
    pub has_property_index: bool,
}

/// Everything froe knows about one index definition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexInfo {
    /// The definition's absolute path.
    pub path: String,
    /// The model, when it could be built. `None` means the definition could
    /// not be read at all, and `model_error` says why.
    pub definition: Option<IndexDefinition>,
    /// Why the model could not be built, if it could not. A definition with
    /// this set is listed and treated as uncheckable.
    pub model_error: Option<String>,
    /// The lane's last-indexed-to date, in its stored form.
    pub indexed_up_to: Option<String>,
    /// `:status/lastUpdated`.
    pub last_updated: Option<String>,
    /// `:status/reindexCompletionTimestamp`.
    pub reindex_completion_timestamp: Option<String>,
    /// `:index-definition/creationTimestamp`, which is **absent** right after
    /// a reindex or an import until a later refresh writes it.
    pub creation_timestamp: Option<String>,
    /// The index's size in bytes, for the types where that means anything.
    pub size_in_bytes: Option<u64>,
    /// The suggester's size in bytes.
    pub suggest_size_in_bytes: Option<u64>,
    /// The estimated entry count, as the type's own information provider
    /// computes it. `None` where no estimate is available.
    pub estimated_entry_count: Option<u64>,
    /// The estimated node count below the content root, for a counter
    /// definition.
    pub estimated_node_count: Option<NodeCountEstimate>,
    /// The Lucene document count. Always `None` until froe can read
    /// `segments_N`; the field exists so the shape does not change when it
    /// can.
    pub document_count: Option<u64>,
    /// The Lucene files and their lengths.
    pub lucene_files: Vec<(String, u64)>,
    /// The hidden children Oak's own index printer reports the presence of.
    pub hidden_children: HiddenChildFacts,
    /// Whether the definition has drifted from its stored clone.
    pub definition_changed: bool,
    /// Where it drifted — froe's own sorted list of changed paths, not Oak's
    /// JSOP text.
    pub definition_diff: Vec<DefinitionDifference>,
    /// The lane checkpoint this definition's data corresponds to, when it
    /// resolves.
    pub lane_checkpoint: Option<String>,
    /// Whether the lane checkpoint is dangling — resolved and absent.
    pub lane_checkpoint_dangling: bool,
    /// How many randomized `:count_*` approximate counters the storage holds.
    pub approximate_counters: usize,
    /// Facts that are not errors.
    pub warnings: Vec<IndexWarning>,
}

impl IndexInfo {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            definition: None,
            model_error: None,
            indexed_up_to: None,
            last_updated: None,
            reindex_completion_timestamp: None,
            creation_timestamp: None,
            size_in_bytes: None,
            suggest_size_in_bytes: None,
            estimated_entry_count: None,
            estimated_node_count: None,
            document_count: None,
            lucene_files: Vec::new(),
            hidden_children: HiddenChildFacts::default(),
            definition_changed: false,
            definition_diff: Vec::new(),
            lane_checkpoint: None,
            lane_checkpoint_dangling: false,
            approximate_counters: 0,
            warnings: Vec::new(),
        }
    }

    /// The stored `type`, or `None` for a definition the model could not be
    /// built for.
    #[must_use]
    pub fn index_type(&self) -> Option<&IndexType> {
        self.definition.as_ref()?.index_type.as_ref()
    }

    /// Whether anything about this definition needs an operator's attention:
    /// it could not be modelled, it drifted, or it carries a warning.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.model_error.is_some()
            || self.definition_changed
            || self.lane_checkpoint_dangling
            || !self.warnings.is_empty()
    }
}

/// Every index definition a store holds, with what could be read of each.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct IndexInventory {
    /// One record per definition, in the order the definitions were found.
    pub indexes: Vec<IndexInfo>,
    /// The lane state, read once.
    pub lanes: AsyncLanes,
    /// Lane checkpoints `/:async` names that `/checkpoints` no longer holds.
    pub dangling_checkpoints: Vec<String>,
    /// Facts about the store rather than about one definition — chiefly that
    /// the non-root definitions could not be enumerated.
    pub warnings: Vec<IndexWarning>,
}

impl IndexInventory {
    /// Collects the inventory from a super-root.
    pub fn collect(
        provider: &dyn SegmentProvider,
        super_root: &NodeState<'_>,
    ) -> IndexResult<Self> {
        Self::collect_with_progress(provider, super_root, &mut DiscardedProgress)
    }

    /// Collects exactly like [`Self::collect`], reporting the definition
    /// nodes it reads to `observer`.
    pub fn collect_with_progress(
        provider: &dyn SegmentProvider,
        super_root: &NodeState<'_>,
        observer: &mut dyn ProgressObserver,
    ) -> IndexResult<Self> {
        let content_root = super_root.child_node("root")?.ok_or_else(|| {
            IndexError::Record(crate::Error::InvalidFormat {
                details: "the super-root has no \"root\" child node".to_owned(),
            })
        })?;

        let mut warnings = Vec::new();
        let definition_paths = enumerate_definition_paths(&content_root, &mut warnings)?;
        let lanes = AsyncLanes::read(&content_root)?;
        let dangling_checkpoints = AsyncLanes::dangling_checkpoints(&content_root, super_root)?;

        let step =
            Step::new(INVENTORY_STEP, WorkUnit::Nodes).with_total(count(definition_paths.len()));
        let indexes = observe(observer, &step, |observer| {
            let mut indexes = Vec::with_capacity(definition_paths.len());
            for (position, path) in definition_paths.iter().enumerate() {
                observer.step_advanced(count(position));
                indexes.push(collect_one(
                    provider,
                    &content_root,
                    path,
                    &lanes,
                    &dangling_checkpoints,
                )?);
            }
            observer.step_advanced(count(indexes.len()));
            Ok::<_, IndexError>(indexes)
        })?;

        Ok(Self {
            indexes,
            lanes,
            dangling_checkpoints,
            warnings,
        })
    }

    /// Collects exactly the definitions at `paths`, in the order given.
    ///
    /// The path service is **never consulted**, and neither is its nodetype
    /// precondition — which is how oak-run behaves when it is given
    /// `--index-paths`, and what lets `froe index definitions --index …`
    /// succeed on a store whose `/oak:index/nodetype` index is disabled
    /// while an unnarrowed run refuses exactly as Oak's printer does.
    ///
    /// A path naming no node yields a record carrying that as its model
    /// error, exactly as an enumerated definition that vanished would:
    /// deciding what a caller's own list means is the caller's, and
    /// `froe index` refuses an unknown path by name before it gets here.
    pub fn collect_selected(
        provider: &dyn SegmentProvider,
        super_root: &NodeState<'_>,
        paths: &[String],
        observer: &mut dyn ProgressObserver,
    ) -> IndexResult<Self> {
        let content_root = super_root.child_node("root")?.ok_or_else(|| {
            IndexError::Record(crate::Error::InvalidFormat {
                details: "the super-root has no \"root\" child node".to_owned(),
            })
        })?;
        let lanes = AsyncLanes::read(&content_root)?;
        let dangling_checkpoints = AsyncLanes::dangling_checkpoints(&content_root, super_root)?;

        let step = Step::new(INVENTORY_STEP, WorkUnit::Nodes).with_total(count(paths.len()));
        let indexes = observe(observer, &step, |observer| {
            let mut indexes = Vec::with_capacity(paths.len());
            for (position, path) in paths.iter().enumerate() {
                observer.step_advanced(count(position));
                indexes.push(collect_one(
                    provider,
                    &content_root,
                    path,
                    &lanes,
                    &dangling_checkpoints,
                )?);
            }
            observer.step_advanced(count(indexes.len()));
            Ok::<_, IndexError>(indexes)
        })?;

        Ok(Self {
            indexes,
            lanes,
            dangling_checkpoints,
            warnings: Vec::new(),
        })
    }

    /// One record by definition path.
    #[must_use]
    pub fn index_at(&self, path: &str) -> Option<&IndexInfo> {
        self.indexes.iter().find(|info| info.path == path)
    }
}

/// The three cases Oak's index path service distinguishes, with a **warning**
/// where it throws.
///
/// 1. The nodetype index is absent, or its `type` does not read strictly as
///    the `STRING` `property`.
/// 2. It is present but does not declare `oak:QueryIndexDefinition` — which
///    is the **default** Oak store, since the shipped nodetype index is
///    created with no `declaringNodeTypes` at all, and is the real Sling
///    fixture's own state. Oak's verdict there is that non-root indexes will
///    not be listed.
/// 3. It is present and declares it, and the non-root definitions come from
///    that index's mirror entries.
///
/// The first two yield the root-level definitions plus a warning naming which
/// condition held.
fn enumerate_definition_paths(
    content_root: &NodeState<'_>,
    warnings: &mut Vec<IndexWarning>,
) -> IndexResult<Vec<String>> {
    match crate::index::definition::index_paths(content_root) {
        Ok(paths) => Ok(paths),
        Err(IndexError::NodeTypeIndexUnusable { found }) => {
            warnings.push(IndexWarning::NonRootDefinitionsNotEnumerated { condition: found });
            root_level_paths(content_root)
        }
        Err(IndexError::NodeTypeIndexHasNoData { property_name }) => {
            warnings.push(IndexWarning::NonRootDefinitionsNotEnumerated {
                condition: format!("no property index with an :index child covers {property_name}"),
            });
            root_level_paths(content_root)
        }
        Err(other) => Err(other),
    }
}

/// `/oak:index`'s stored child order, filtered the way the path service's own
/// fallback branch filters it.
fn root_level_paths(content_root: &NodeState<'_>) -> IndexResult<Vec<String>> {
    let Some(oak_index) = content_root.child_node(INDEX_DEFINITIONS_NAME)? else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    for (name, child) in oak_index.child_node_entries()? {
        if strict_name(child.property("jcr:primaryType")?.as_ref())
            == Some(INDEX_DEFINITIONS_NODE_TYPE)
        {
            paths.push(format!("/{INDEX_DEFINITIONS_NAME}/{name}"));
        }
    }
    Ok(paths)
}

fn collect_one(
    provider: &dyn SegmentProvider,
    content_root: &NodeState<'_>,
    path: &str,
    lanes: &AsyncLanes,
    dangling_checkpoints: &[String],
) -> IndexResult<IndexInfo> {
    let mut info = IndexInfo::new(path);
    let Some(node) = resolve(content_root, path)? else {
        info.model_error = Some("the definition node is absent".to_owned());
        return Ok(info);
    };

    let definition = match IndexDefinition::read(&node, path) {
        Ok(definition) => definition,
        Err(IndexError::Record(source)) => return Err(IndexError::Record(source)),
        Err(other) => {
            // Every other variant is attributable to this definition alone,
            // so it is carried here rather than ending the listing.
            info.model_error = Some(other.to_string());
            return Ok(info);
        }
    };

    if !path.starts_with(&format!("/{INDEX_DEFINITIONS_NAME}/")) {
        info.warnings.push(IndexWarning::NonRootDefinition);
    }
    info.warnings.extend(definition.warnings.iter().cloned());
    info.hidden_children = HiddenChildFacts {
        has_mount: !definition.mount_children().is_empty(),
        has_property_index: definition.has_hidden_child(":property-index"),
    };

    read_lane_facts(&definition, lanes, dangling_checkpoints, &mut info);
    read_status_facts(&node, &mut info)?;
    read_drift(&node, &mut info)?;
    read_storage_facts(provider, content_root, &node, &definition, &mut info)?;

    info.definition = Some(definition);
    Ok(info)
}

fn read_lane_facts(
    definition: &IndexDefinition,
    lanes: &AsyncLanes,
    dangling_checkpoints: &[String],
    info: &mut IndexInfo,
) {
    let Some(lane_name) = definition.lane.as_deref() else {
        return;
    };
    let Some(lane) = lanes.lane(lane_name) else {
        return;
    };
    info.indexed_up_to.clone_from(&lane.last_indexed_to);
    info.lane_checkpoint.clone_from(&lane.checkpoint);
    info.lane_checkpoint_dangling = lane
        .checkpoint
        .as_ref()
        .is_some_and(|checkpoint| dangling_checkpoints.contains(checkpoint));
}

fn read_status_facts(node: &NodeState<'_>, info: &mut IndexInfo) -> IndexResult<()> {
    if let Some(status) = StatusNode::read(node)? {
        info.last_updated = status.last_updated;
        info.reindex_completion_timestamp = status.reindex_completion_timestamp;
    }
    if let Some(stored) = StoredDefinition::read(node)? {
        info.creation_timestamp = stored.creation_timestamp;
    }
    Ok(())
}

fn read_drift(node: &NodeState<'_>, info: &mut IndexInfo) -> IndexResult<()> {
    let Some(stored) = node.child_node(STORED_DEFINITION_NODE_NAME)? else {
        return Ok(());
    };
    let differences = definition_drift(node, &stored, &[])?;
    info.definition_changed = !differences.is_empty();
    info.definition_diff = differences;
    Ok(())
}

fn read_storage_facts(
    provider: &dyn SegmentProvider,
    content_root: &NodeState<'_>,
    node: &NodeState<'_>,
    definition: &IndexDefinition,
    info: &mut IndexInfo,
) -> IndexResult<()> {
    match definition.index_type.as_ref() {
        Some(IndexType::Property) => read_property_facts(node, definition, info),
        Some(IndexType::Reference) => read_reference_facts(node, definition, info),
        Some(IndexType::Counter) => {
            let index = CounterIndex::open(*node, definition);
            if index.has_data_node()? {
                info.estimated_node_count = Some(estimated_node_count(
                    content_root,
                    "/",
                    CountBound::Expected,
                )?);
            } else {
                info.estimated_node_count = Some(NodeCountEstimate::Unknown);
            }
            Ok(())
        }
        Some(IndexType::Lucene) => read_lucene_facts(provider, node, definition, info),
        _ => Ok(()),
    }
}

/// `PropertyIndexInfoProvider.computeCountEstimate`: the approximate counters
/// of every hidden child, summed; else zero when a hidden child has no
/// children at all; else no estimate.
///
/// The same computation serves a unique index, whose only counter sits on
/// `:index`.
fn read_property_facts(
    node: &NodeState<'_>,
    definition: &IndexDefinition,
    info: &mut IndexInfo,
) -> IndexResult<()> {
    let mut estimate: Option<u64> = None;
    for (name, child) in node.child_node_entries()? {
        if !name.starts_with(':') {
            continue;
        }
        let sum = approximate_count_sum(&child)?;
        if let Some(sum) = sum
            && sum > 0
        {
            *estimate.get_or_insert(0) += sum;
        } else if sum.is_none() && estimate.is_none() && child.child_node_entries()?.is_empty() {
            estimate = Some(0);
        }
    }
    // The two strategies hold their counters in different places — the
    // mirror on `:index` *and* every key node, the unique on `:index` alone
    // — so each reader counts its own, and neither guesses from the shape it
    // finds.
    info.approximate_counters = if definition.property.unique {
        match UniqueIndex::open(node, INDEX_CONTENT_NODE_NAME)? {
            Some(index) => index.approximate_counter_count()?,
            None => 0,
        }
    } else {
        match MirrorIndex::open(node, &definition.path, INDEX_CONTENT_NODE_NAME)? {
            Some(index) => index.approximate_counter_count()?,
            None => 0,
        }
    };
    info.estimated_entry_count = estimate;
    Ok(())
}

fn read_reference_facts(
    node: &NodeState<'_>,
    definition: &IndexDefinition,
    info: &mut IndexInfo,
) -> IndexResult<()> {
    let mut counters = 0usize;
    for child_name in [":references", ":weakreferences"] {
        if let Some(index) = MirrorIndex::open(node, &definition.path, child_name)? {
            counters += index.approximate_counter_count()?;
        }
    }
    info.approximate_counters = counters;
    Ok(())
}

/// `LuceneIndexInfoProvider.computeSize`, minus the document count: the sum of
/// the file lengths of every index directory, and of every suggest directory
/// separately.
fn read_lucene_facts(
    provider: &dyn SegmentProvider,
    node: &NodeState<'_>,
    definition: &IndexDefinition,
    info: &mut IndexInfo,
) -> IndexResult<()> {
    let mut size = 0u64;
    let mut suggest_size = 0u64;
    let mut files = Vec::new();
    let mut has_index_directory = false;
    for (name, _) in node.child_node_entries()? {
        if !name.starts_with(':') {
            continue;
        }
        let is_index = is_index_directory_name(&name);
        let is_suggest = is_suggest_directory_name(&name);
        if !is_index && !is_suggest {
            continue;
        }
        let Some(directory) = OakDirectory::open(provider, node, definition, &name)? else {
            continue;
        };
        for file_name in directory.file_names() {
            let file = directory.file(file_name)?;
            if is_index {
                size += file.length();
                files.push((file_name.clone(), file.length()));
            } else {
                suggest_size += file.length();
            }
        }
        has_index_directory |= is_index;
    }
    if has_index_directory {
        info.size_in_bytes = Some(size);
    }
    if node.child_node(SUGGEST_DATA_CHILD_NAME)?.is_some() {
        info.suggest_size_in_bytes = Some(suggest_size);
    }
    files.sort();
    info.lucene_files = files;
    Ok(())
}

/// `ApproximateCounter.getCountSync` over a node: `None` when it carries no
/// `:count_*` property at all.
fn approximate_count_sum(node: &NodeState<'_>) -> IndexResult<Option<u64>> {
    let mut found = false;
    let mut added = 0i64;
    let mut removed = 0i64;
    for property in node.properties()? {
        if !property
            .name
            .starts_with(crate::index::property::mirror::APPROXIMATE_COUNT_PREFIX)
        {
            continue;
        }
        found = true;
        let value = crate::index::converting_long(Some(&property)).unwrap_or(0);
        if value > 0 {
            added += value;
        } else {
            removed -= value;
        }
    }
    if !found {
        return Ok(None);
    }
    let estimate = (added / 2).max(added - removed);
    Ok(Some(u64::try_from(estimate).unwrap_or(0)))
}

fn resolve<'provider>(
    content_root: &NodeState<'provider>,
    path: &str,
) -> IndexResult<Option<NodeState<'provider>>> {
    let mut current = *content_root;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}
