//! Which definitions are rebuilt, against which state, and what is refused.
//!
//! Every refusal here is a distinct variant carrying the definition's path,
//! because "the reindex refused" is not an answer an operator can act on and
//! "the reindex rebuilt it wrong" is not a failure they would notice.
//!
//! # The state root is not always the head
//!
//! Oak's editors see the state of the commit they run in: the head for a
//! synchronous definition, the lane's checkpoint for an asynchronous one.
//! Rebuilding an asynchronous definition from the head would be wrong for a
//! counter — the lane's next cycle diffs from its recorded checkpoint, and
//! every node added since would be counted twice — so a dangling checkpoint
//! is a refusal rather than a fallback to the head.
//!
//! # What `--from-head` authorizes, and what it does not
//!
//! It is consulted **only** for a definition whose lane checkpoint is
//! dangling or absent, and ignored for an intact lane. Under it:
//!
//! * a **mirror or unique** definition is rebuilt from the head, because
//!   Oak's replay re-inserts entries that already exist and leaves every
//!   `match` and `entry` as it was — only the randomized `:count_*`
//!   estimates drift;
//! * a **counter** takes the *reset* action instead. A rebuild would be
//!   wrong: the replay doubles a counter whether or not froe ran, so the
//!   right move is to leave Oak a definition with no hidden child, which its
//!   own rule rebuilds from scratch.

use crate::content::SegmentProvider;
use crate::content::node::NodeState;
use crate::index::definition::{IndexDefinition, IndexType};
use crate::index::inventory::IndexInventory;
use crate::index::lanes::AsyncLanes;
use crate::segment::record::RecordIdentifier;

/// The lane a definition is parked at while an operator-triggered reindex
/// runs. A definition already there is treated as synchronous.
const ASYNC_REINDEX_LANE: &str = "async-reindex";

/// Which state a selected definition is rebuilt from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IndexingState {
    /// The head: a synchronous definition, or one parked at
    /// `async = async-reindex`, or a from-head rebuild.
    Head,
    /// The lane's checkpoint, for an asynchronous definition.
    LaneCheckpoint {
        /// The lane name.
        lane: String,
        /// The checkpoint name.
        checkpoint: String,
    },
    /// Not rebuilt: the hidden children are removed and Oak's own replay
    /// rebuilds from scratch. A counter on a lane whose checkpoint is
    /// dangling or absent, under `--from-head`.
    /// A counter and a Lucene definition both reach it, and for the same
    /// kind of reason: Oak's own replay after a lost checkpoint would
    /// double what froe rebuilt — the counter through its editor, the
    /// Lucene index through its writer's appending reindex branch — while
    /// a definition with no hidden child is the case Oak rebuilds from
    /// scratch.
    ResetForReplay {
        /// The lane whose checkpoint could not be resolved.
        lane: String,
    },
}

/// One definition selected for a run.
pub struct SelectedIndex {
    /// The model.
    pub definition: IndexDefinition,
    /// The definition node's record, read under the lock.
    pub definition_record: RecordIdentifier,
    /// The state root's record, absent for a reset.
    pub state_root: Option<RecordIdentifier>,
    /// Which state, and why.
    pub state: IndexingState,
}

/// Why a definition was not selected.
///
/// Every variant carries the path, because a refusal an operator cannot
/// attribute is a refusal they will work around.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SelectionRefusal {
    /// A `lucene` definition selected with no binary-text policy. froe
    /// extracts no binary text, so what a binary property contributes is
    /// an operator's decision; the gate refuses rather than skipping,
    /// because a rebuild that quietly left every binary out would be a
    /// fulltext index that answers fewer queries than Oak's.
    LuceneWithoutBinaryTextPolicy {
        /// The definition.
        path: String,
    },
    /// A `lucene` definition declaring something this plan does not
    /// reproduce: a codec verdict that is not `oakCodec`, a
    /// definition-level `valueRegex`, a property definition using
    /// `function`, `dynamicBoost`, `useInSimilarity` or `similarityTags`,
    /// a `compatVersion` of 1, a `maxFieldLength` of zero, a
    /// consumer-registered analyzer, or one of the other constructs
    /// `documents::rules` names.
    LuceneDefinitionUnsupported {
        /// The definition.
        path: String,
        /// What it declares, as the rules reader named it.
        reason: String,
    },
    /// A `lucene` definition with no `async` property. Oak documents
    /// `async` as required for a Lucene index and provides no oracle for
    /// the synchronous case, so froe refuses rather than guessing which
    /// state to build from.
    LuceneSynchronous {
        /// The definition.
        path: String,
    },
    /// An `elasticsearch` definition: its data lives outside the repository.
    ExternalIndex {
        /// The definition.
        path: String,
        /// Its stored `type`.
        index_type: String,
    },
    /// A `disabled` or `ordered` definition, which has no editor at all.
    NoEditor {
        /// The definition.
        path: String,
        /// Its stored `type`.
        index_type: String,
    },
    /// A definition froe could not model.
    Unmodellable {
        /// The definition.
        path: String,
        /// What went wrong reading it.
        reason: String,
    },
    /// A `valuePattern` froe cannot evaluate.
    ValuePatternNotSupported {
        /// The definition.
        path: String,
    },
    /// A definition whose `PathFilter` Oak cannot construct. Oak's own cycle
    /// skips such a definition when the filter throws, so Oak leaves its
    /// `reindex` flag set and froe must not quietly succeed where Oak fails.
    PathFilterUnconstructable {
        /// The definition.
        path: String,
        /// Why the filter could not be constructed.
        reason: String,
    },
    /// A counter that is also maintained synchronously. Its lane's replay
    /// adds to what is there, so a counter rebuilt from the head and then
    /// replayed would be doubled.
    HybridCounter {
        /// The definition.
        path: String,
        /// The lane it also names.
        lane: String,
    },
    /// A definition carrying a composite-store mount's index data. A
    /// rebuild replaces the hidden children it produces, so rebuilding one
    /// of these would remove another mount's index — data froe did not
    /// write and cannot rebuild.
    MountFragmentPresent {
        /// The definition.
        path: String,
        /// The mount-decorated hidden child that was found.
        child_name: String,
    },
    /// A nested definition — one whose parent is not `/oak:index`. Oak
    /// scopes it to the node holding its `oak:index` and runs a child cycle
    /// whose paths are relative to that node. Refused, never approximated.
    NestedDefinition {
        /// The definition.
        path: String,
    },
    /// A node that is not an `oak:QueryIndexDefinition`.
    NotADefinition {
        /// The node.
        path: String,
    },
    /// The lane's checkpoint no longer exists, and `--from-head` was not
    /// given.
    DanglingLaneCheckpoint {
        /// The definition.
        path: String,
        /// The lane it indexes on.
        lane: String,
        /// The checkpoint the lane names.
        checkpoint: String,
    },
    /// The lane has no entry on `/:async` at all. Oak's first cycle treats
    /// this like a lost checkpoint and diffs from the missing state, so it
    /// is the same hazard under a different shape — and its own variant, so
    /// an operator can tell them apart.
    LaneAbsent {
        /// The definition.
        path: String,
        /// The lane with no state.
        lane: String,
    },
    /// A Lucene definition is parked on `async-reindex`, the lane an
    /// out-of-band reindex moves a definition onto.
    LuceneParkedOnTheReindexLane {
        /// The definition.
        path: String,
    },
    /// A lane is mid-run: `/:async/async-reindex` carries a checkpoint.
    ReindexLaneInProgress {
        /// The definition.
        path: String,
    },
}

impl SelectionRefusal {
    /// The definition this refusal is about.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::LuceneWithoutBinaryTextPolicy { path }
            | Self::LuceneDefinitionUnsupported { path, .. }
            | Self::LuceneSynchronous { path }
            | Self::ExternalIndex { path, .. }
            | Self::NoEditor { path, .. }
            | Self::Unmodellable { path, .. }
            | Self::ValuePatternNotSupported { path }
            | Self::MountFragmentPresent { path, .. }
            | Self::HybridCounter { path, .. }
            | Self::PathFilterUnconstructable { path, .. }
            | Self::NestedDefinition { path }
            | Self::NotADefinition { path }
            | Self::DanglingLaneCheckpoint { path, .. }
            | Self::LaneAbsent { path, .. }
            | Self::LuceneParkedOnTheReindexLane { path }
            | Self::ReindexLaneInProgress { path } => path,
        }
    }
}

impl std::fmt::Display for SelectionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LuceneWithoutBinaryTextPolicy { path } => write!(
                formatter,
                "{path} is a lucene definition and no binary-text policy was given; froe \
                 extracts no binary text, so the run needs one before it can rebuild it"
            ),
            Self::LuceneDefinitionUnsupported { path, reason } => {
                write!(formatter, "{path} cannot be rebuilt natively: {reason}")
            }
            Self::LuceneSynchronous { path } => write!(
                formatter,
                "{path} is a lucene definition with no async property, which Oak documents as \
                 required and provides no oracle for"
            ),
            Self::ExternalIndex { path, index_type } => write!(
                formatter,
                "{path} is of type {index_type}, whose data lives outside the repository"
            ),
            Self::NoEditor { path, index_type } => write!(
                formatter,
                "{path} is of type {index_type}, which Oak has no editor for"
            ),
            Self::Unmodellable { path, reason } => {
                write!(formatter, "{path} could not be read: {reason}")
            }
            Self::ValuePatternNotSupported { path } => write!(
                formatter,
                "{path} carries a valuePattern froe cannot evaluate, so its entries \
                 cannot be derived"
            ),
            Self::PathFilterUnconstructable { path, reason } => write!(
                formatter,
                "{path} has a path filter Oak cannot construct ({reason}), so Oak's own \
                 cycle skips it and leaves its reindex flag set"
            ),
            Self::HybridCounter { path, lane } => write!(
                formatter,
                "{path} is a counter maintained both synchronously and on lane {lane}; \
                 rebuilding it from either state and letting the other replay would \
                 double every count"
            ),
            Self::MountFragmentPresent { path, child_name } => write!(
                formatter,
                "{path} carries {child_name}, a composite-store mount's index data that \
                 a rebuild would remove and cannot reproduce"
            ),
            Self::NestedDefinition { path } => write!(
                formatter,
                "{path} is nested under a content node rather than /oak:index; Oak runs \
                 a child cycle whose paths are relative to that node"
            ),
            Self::NotADefinition { path } => {
                write!(formatter, "{path} is not an oak:QueryIndexDefinition")
            }
            Self::DanglingLaneCheckpoint {
                path,
                lane,
                checkpoint,
            } => write!(
                formatter,
                "{path} indexes on lane {lane}, whose checkpoint {checkpoint} no longer \
                 exists; rerun with --from-head to authorize a rebuild from the head"
            ),
            Self::LaneAbsent { path, lane } => write!(
                formatter,
                "{path} indexes on lane {lane}, which has no state on /:async; rerun \
                 with --from-head to authorize a rebuild from the head"
            ),
            Self::LuceneParkedOnTheReindexLane { path } => write!(
                formatter,
                "{path} is a lucene definition parked on the async-reindex lane, which no \
                 ordinary indexing cycle maintains; restore the lane it belongs on before \
                 rebuilding it"
            ),
            Self::ReindexLaneInProgress { path } => write!(
                formatter,
                "{path} is parked on the async-reindex lane and that lane has a \
                 checkpoint, so a reindex is already in progress"
            ),
        }
    }
}

/// What a selection produced.
pub struct Selection {
    /// The definitions to rebuild or reset.
    pub selected: Vec<SelectedIndex>,
    /// Why each other definition was left alone.
    pub refused: Vec<SelectionRefusal>,
}

/// How a run chooses definitions.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SelectionOptions {
    /// Definition paths the operator named. Empty selects every definition
    /// flagged `reindex = true`.
    pub requested_paths: Vec<String>,
    /// Whether a dangling or absent lane checkpoint is authorized.
    pub from_head: bool,
    /// Whether the caller supplied a binary-text policy, without which a
    /// Lucene definition is refused.
    pub has_binary_text_policy: bool,
}

/// Chooses what to rebuild.
///
/// `provider` and `head_root` rather than the inventory alone, because three
/// outputs are reads rather than projections: the lane checkpoint's
/// `/checkpoints/<name>/root` record, the disabler predicate over the head's
/// raw `declaringNodeTypes`, and the definition record itself.
pub fn select(
    provider: &dyn SegmentProvider,
    super_root: &NodeState<'_>,
    inventory: &IndexInventory,
    options: &SelectionOptions,
) -> crate::Result<Selection> {
    let head_root = super_root
        .child_node("root")?
        .ok_or_else(|| crate::Error::InvalidFormat {
            details: "the super-root has no \"root\" child node".to_owned(),
        })?;
    let lanes = AsyncLanes::read(&head_root).map_err(index_error_to_store_error)?;

    let mut selection = Selection {
        selected: Vec::new(),
        refused: Vec::new(),
    };
    for info in &inventory.indexes {
        // An explicitly named definition is always answered — with a
        // selection or with a refusal naming it. An unnamed run considers
        // only what Oak's own cycle would: the flagged definitions.
        let named = options
            .requested_paths
            .iter()
            .any(|path| path == &info.path);
        if !options.requested_paths.is_empty() && !named {
            continue;
        }

        let Some(definition) = info.definition.clone() else {
            selection
                .refused
                .push(refuse_unmodellable(&head_root, info)?);
            continue;
        };
        if !named && !definition.reindex.flagged {
            continue;
        }
        if let Some(refusal) = refuse_by_shape(&definition) {
            selection.refused.push(refusal);
            continue;
        }
        if definition.index_type.as_ref() == Some(&IndexType::Lucene)
            && let Some(refusal) = refuse_lucene(&head_root, &definition, options)?
        {
            selection.refused.push(refusal);
            continue;
        }
        match resolve_state(
            provider,
            super_root,
            &head_root,
            &lanes,
            &definition,
            options,
        )? {
            Ok((state, state_root)) => {
                let Some(node) = descend(&head_root, &definition.path)? else {
                    selection.refused.push(SelectionRefusal::Unmodellable {
                        path: definition.path.clone(),
                        reason: "the definition node vanished between the listing and the \
                                 selection"
                            .to_owned(),
                    });
                    continue;
                };
                selection.selected.push(SelectedIndex {
                    definition_record: node.record_identifier(),
                    definition,
                    state_root,
                    state,
                });
            }
            Err(refusal) => selection.refused.push(refusal),
        }
    }
    answer_every_named_path(&head_root, inventory, options, &mut selection)?;
    Ok(selection)
}

/// Answers a requested path the inventory never listed.
///
/// The inventory enumerates nodes whose `jcr:primaryType` is
/// `oak:QueryIndexDefinition`, so a path naming anything else — a content
/// node, a typo, a definition someone removed — falls out of the loop above
/// without producing either a selection or a refusal. An operator who names
/// a definition is always answered, so the omission is filled here rather
/// than reported as nothing to do.
fn answer_every_named_path(
    head_root: &NodeState<'_>,
    inventory: &IndexInventory,
    options: &SelectionOptions,
    selection: &mut Selection,
) -> crate::Result<()> {
    for requested in &options.requested_paths {
        if inventory.indexes.iter().any(|info| &info.path == requested) {
            continue;
        }
        let Some(node) = descend(head_root, requested)? else {
            selection.refused.push(SelectionRefusal::Unmodellable {
                path: requested.clone(),
                reason: "there is no node at this path".to_owned(),
            });
            continue;
        };
        // The node is there but the enumeration passed over it. Why decides
        // the answer, and the shape rules already know: a nested definition
        // is nested, a `lucene` one is Lucene, and a node that is not a
        // definition at all is that.
        let refusal = match IndexDefinition::read(&node, requested) {
            Ok(definition) => {
                refuse_by_shape(&definition).unwrap_or(SelectionRefusal::Unmodellable {
                    path: requested.clone(),
                    reason: "the node type index did not enumerate this definition".to_owned(),
                })
            }
            Err(_) => SelectionRefusal::NotADefinition {
                path: requested.clone(),
            },
        };
        selection.refused.push(refusal);
    }
    Ok(())
}

/// The refusal for a definition the inventory could not model.
///
/// The inventory carries the failure as text, which is right for a listing
/// and not enough here: an operator who is told a path filter cannot be
/// constructed learns that Oak's own cycle skips this definition too and
/// leaves its `reindex` flag set, where "could not be read" only says froe
/// gave up. So the read is repeated on this one node, on the refusal path
/// only, to recover the variant.
fn refuse_unmodellable(
    head_root: &NodeState<'_>,
    info: &crate::index::inventory::IndexInfo,
) -> crate::Result<SelectionRefusal> {
    let path = info.path.clone();
    let unmodellable = |reason: String| SelectionRefusal::Unmodellable {
        path: path.clone(),
        reason,
    };
    let Some(node) = descend(head_root, &info.path)? else {
        return Ok(unmodellable(
            info.model_error
                .clone()
                .unwrap_or_else(|| "the definition node is absent".to_owned()),
        ));
    };
    match IndexDefinition::read(&node, &info.path) {
        Ok(_) => Ok(unmodellable(
            "the definition could not be read when the inventory was collected".to_owned(),
        )),
        Err(
            error @ (crate::index::IndexError::RelativeFilterPath { .. }
            | crate::index::IndexError::EmptyIncludeSet { .. }),
        ) => Ok(SelectionRefusal::PathFilterUnconstructable {
            path,
            reason: error.to_string(),
        }),
        Err(other) => Ok(unmodellable(other.to_string())),
    }
}

/// The refusals a Lucene definition's own rules decide, which
/// [`refuse_by_shape`] cannot: they need the definition node.
///
/// The rules are read against the **head's** node types here, because what
/// this answers is whether the definition can be rebuilt at all — a
/// question about its own shape. The rebuild reads them again against the
/// state root, as Oak's own cycle does.
fn refuse_lucene(
    head_root: &NodeState<'_>,
    definition: &IndexDefinition,
    options: &SelectionOptions,
) -> crate::Result<Option<SelectionRefusal>> {
    let path = definition.path.clone();
    if !options.has_binary_text_policy {
        return Ok(Some(SelectionRefusal::LuceneWithoutBinaryTextPolicy {
            path,
        }));
    }
    if definition.indexing_mode.synchronous {
        return Ok(Some(SelectionRefusal::LuceneSynchronous { path }));
    }
    // A hybrid definition lists `sync` beside its lane name, and Oak keeps
    // a synchronous `:property-index` for it that this plan does not build.
    if definition.indexing_mode.synchronous_synonym {
        return Ok(Some(SelectionRefusal::LuceneDefinitionUnsupported {
            path,
            reason: "it is hybrid — `async` lists `sync` beside its lane — and Oak keeps a \
                     synchronous :property-index for it that froe does not build"
                .to_owned(),
        }));
    }
    let Some(node) = descend(head_root, &definition.path)? else {
        return Ok(Some(SelectionRefusal::Unmodellable {
            path,
            reason: "the definition node vanished between the listing and the selection".to_owned(),
        }));
    };
    let mut warnings = Vec::new();
    match crate::index::lucene::documents::rules::IndexingRules::read(
        &node,
        &definition.path,
        head_root,
        &mut warnings,
    ) {
        Ok(_) => Ok(None),
        Err(crate::index::IndexError::Record(error)) => Err(error),
        Err(other) => Ok(Some(SelectionRefusal::LuceneDefinitionUnsupported {
            path,
            reason: other.to_string(),
        })),
    }
}

/// The refusals that follow from the definition alone.
fn refuse_by_shape(definition: &IndexDefinition) -> Option<SelectionRefusal> {
    let path = definition.path.clone();

    // Oak scopes a nested definition to the node holding its `oak:index` and
    // runs a child cycle whose editor root is that node, so its paths are
    // relative to it. Refused rather than approximated.
    if !path.starts_with("/oak:index/") || path.matches('/').count() != 2 {
        return Some(SelectionRefusal::NestedDefinition { path });
    }

    // Oak's own editor filters values through the pattern before indexing
    // them. froe evaluates the prefix halves and not the regular expression,
    // so a rebuild would write entries Oak would have left out — a wrong
    // index rather than a missing one.
    if definition
        .property
        .value_pattern
        .regular_expression()
        .is_some()
    {
        return Some(SelectionRefusal::ValuePatternNotSupported { path });
    }

    // A rebuild replaces the definition's hidden children, and a mount
    // fragment's data belongs to a composite store's other mount.
    if let Some(child_name) = definition.mount_children().first() {
        return Some(SelectionRefusal::MountFragmentPresent {
            path,
            child_name: (*child_name).to_owned(),
        });
    }

    match definition.index_type.as_ref() {
        None => Some(SelectionRefusal::NotADefinition { path }),
        Some(IndexType::Elasticsearch) => Some(SelectionRefusal::ExternalIndex {
            path,
            index_type: "elasticsearch".to_owned(),
        }),
        Some(IndexType::Disabled { .. }) => Some(SelectionRefusal::NoEditor {
            path,
            index_type: "disabled".to_owned(),
        }),
        Some(IndexType::Ordered) => Some(SelectionRefusal::NoEditor {
            path,
            index_type: "ordered".to_owned(),
        }),
        Some(IndexType::Unknown(name)) => Some(SelectionRefusal::NoEditor {
            path,
            index_type: name.clone(),
        }),
        // A Lucene definition's own refusals need its rules, which this
        // function cannot read: `refuse_lucene` does them after this
        // returns nothing.
        Some(
            IndexType::Property | IndexType::Reference | IndexType::Counter | IndexType::Lucene,
        ) => None,
    }
}

/// Which state this definition is rebuilt from.
fn resolve_state(
    provider: &dyn SegmentProvider,
    super_root: &NodeState<'_>,
    head_root: &NodeState<'_>,
    lanes: &AsyncLanes,
    definition: &IndexDefinition,
    options: &SelectionOptions,
) -> crate::Result<std::result::Result<(IndexingState, Option<RecordIdentifier>), SelectionRefusal>>
{
    let _ = provider;
    // A definition parked at `async = async-reindex` is rebuilt from the
    // head: the lane runs only when an operator triggers it, so the
    // definition is as current as the head and its later replay leaves a
    // property family's `match` and `entry` unchanged. A lane *mid-run*
    // carries a checkpoint, and that is refused.
    //
    // A **Lucene** definition is refused outright instead. froe does not
    // remove `async`, so the definition stays on a lane
    // `IndexUpdate.isIncluded` admits to no ordinary cycle
    // (`index-definitions.md` §2.2): the index froe built would go stale
    // silently, and the lane's own next cycle diffs from a missing before
    // state, which is the branch `FulltextIndexEditor` re-enters reindex
    // mode on and `DefaultIndexWriter` *appends* through — doubling the
    // index, the same hazard the lost-checkpoint reset below exists for.
    // A reset is not the answer either, because nothing runs that lane
    // unless an operator asks it to.
    if definition.lane.as_deref() == Some(ASYNC_REINDEX_LANE) {
        if lanes
            .lane(ASYNC_REINDEX_LANE)
            .and_then(|lane| lane.checkpoint.as_deref())
            .is_some()
        {
            return Ok(Err(SelectionRefusal::ReindexLaneInProgress {
                path: definition.path.clone(),
            }));
        }
        if definition.index_type.as_ref() == Some(&IndexType::Lucene) {
            return Ok(Err(SelectionRefusal::LuceneParkedOnTheReindexLane {
                path: definition.path.clone(),
            }));
        }
        return Ok(Ok((
            IndexingState::Head,
            Some(head_root.record_identifier()),
        )));
    }

    let Some(lane_name) = definition.lane.as_deref() else {
        return Ok(Ok((
            IndexingState::Head,
            Some(head_root.record_identifier()),
        )));
    };

    // A *hybrid* definition lists `sync` beside its lane name, and Oak's
    // synchronous cycle then maintains it on every commit as well as the
    // lane doing so. Such an index is as current as the head, not as its
    // lane — so rebuilding it from the lane's checkpoint would drop every
    // entry committed since, which is a silently incomplete index. The
    // lane's own later replay re-inserts what it already holds, leaving
    // `match` and `entry` unchanged.
    //
    // A counter is the exception: its replay *adds*, so a counter rebuilt
    // from the head and then replayed by its lane would be doubled. froe
    // refuses rather than produce a number nobody can trust.
    if definition.indexing_mode.synchronous_synonym {
        if definition.index_type.as_ref() == Some(&IndexType::Counter) {
            return Ok(Err(SelectionRefusal::HybridCounter {
                path: definition.path.clone(),
                lane: lane_name.to_owned(),
            }));
        }
        return Ok(Ok((
            IndexingState::Head,
            Some(head_root.record_identifier()),
        )));
    }

    let lane = lanes.lane(lane_name);
    let checkpoint = lane.and_then(|lane| lane.checkpoint.as_deref());
    let resolved = match checkpoint {
        None => None,
        Some(name) => super_root
            .child_node("checkpoints")?
            .and_then(|checkpoints| checkpoints.child_node(name).transpose())
            .transpose()?
            .and_then(|node| node.child_node("root").transpose())
            .transpose()?,
    };

    if let Some(root) = resolved {
        return Ok(Ok((
            IndexingState::LaneCheckpoint {
                lane: lane_name.to_owned(),
                checkpoint: checkpoint.unwrap_or_default().to_owned(),
            },
            Some(root.record_identifier()),
        )));
    }

    // The lane cannot be resolved. `--from-head` is the only thing that
    // authorizes proceeding, and what it authorizes depends on the type.
    if !options.from_head {
        return Ok(Err(match checkpoint {
            Some(name) => SelectionRefusal::DanglingLaneCheckpoint {
                path: definition.path.clone(),
                lane: lane_name.to_owned(),
                checkpoint: name.to_owned(),
            },
            None => SelectionRefusal::LaneAbsent {
                path: definition.path.clone(),
                lane: lane_name.to_owned(),
            },
        }));
    }

    // A counter is **reset** rather than rebuilt from the head: Oak's own
    // replay adds to a counter rather than replacing it, so a rebuilt one
    // would be doubled, and a number nobody can trust is worse than a
    // number Oak rebuilds itself.
    //
    // This branch once refused instead, on a measurement that a lane whose
    // checkpoint is gone never completes a cycle. Plan 0010's task 1009
    // found that measurement to be a symptom of froe's own defect — the
    // definition had been rewritten with its template property names out
    // of Oak's sorted order, so Oak's conflict merge failed the lane's
    // commit — and with the writer corrected Oak rebuilds on the first
    // cycle. `docs/index.md` §5.9 records that the behaviour changed
    // twice.
    if definition.index_type.as_ref() == Some(&IndexType::Counter) {
        return Ok(Ok((
            IndexingState::ResetForReplay {
                lane: lane_name.to_owned(),
            },
            None,
        )));
    }

    // A Lucene definition is **reset** rather than rebuilt from the head,
    // which is the one place this plan needs the variant plan 0007 kept.
    //
    // After a lost checkpoint Oak's fulltext editor re-enters reindex mode
    // on a missing before state at the root, and its index writer's reindex
    // branch *appends* every document again to the retained `:data` —
    // doubling the index whether or not froe rebuilt it. A definition with
    // no hidden child is the case Oak rebuilds from scratch, so removing
    // them is what makes the next cycle produce a correct index.
    if definition.index_type.as_ref() == Some(&IndexType::Lucene) {
        return Ok(Ok((
            IndexingState::ResetForReplay {
                lane: lane_name.to_owned(),
            },
            None,
        )));
    }

    Ok(Ok((
        IndexingState::Head,
        Some(head_root.record_identifier()),
    )))
}

/// Resolves an absolute path below `root`.
fn descend<'provider>(
    root: &NodeState<'provider>,
    path: &str,
) -> crate::Result<Option<NodeState<'provider>>> {
    let mut node = *root;
    for element in path.split('/').filter(|element| !element.is_empty()) {
        match node.child_node(element)? {
            Some(child) => node = child,
            None => return Ok(None),
        }
    }
    Ok(Some(node))
}

fn index_error_to_store_error(error: crate::index::IndexError) -> crate::Error {
    match error {
        crate::index::IndexError::Record(source) => source,
        other => crate::Error::InvalidFormat {
            details: other.to_string(),
        },
    }
}
