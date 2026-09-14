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
    /// A `lucene` definition. Plan 0010 fills this in.
    LuceneNotYetSupported {
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
            Self::LuceneNotYetSupported { path }
            | Self::ExternalIndex { path, .. }
            | Self::NoEditor { path, .. }
            | Self::Unmodellable { path, .. }
            | Self::ValuePatternNotSupported { path }
            | Self::PathFilterUnconstructable { path, .. }
            | Self::NestedDefinition { path }
            | Self::NotADefinition { path }
            | Self::DanglingLaneCheckpoint { path, .. }
            | Self::LaneAbsent { path, .. }
            | Self::ReindexLaneInProgress { path } => path,
        }
    }
}

impl std::fmt::Display for SelectionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LuceneNotYetSupported { path } => write!(
                formatter,
                "{path} is a lucene definition, which this froe version does not rebuild"
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
            let refusal = SelectionRefusal::Unmodellable {
                path: info.path.clone(),
                reason: info
                    .model_error
                    .clone()
                    .unwrap_or_else(|| "the definition could not be read".to_owned()),
            };
            selection.refused.push(refusal);
            continue;
        };
        if !named && !definition.reindex.flagged {
            continue;
        }
        if let Some(refusal) = refuse_by_shape(&definition) {
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
    Ok(selection)
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

    match definition.index_type.as_ref() {
        None => Some(SelectionRefusal::NotADefinition { path }),
        Some(IndexType::Lucene) => Some(SelectionRefusal::LuceneNotYetSupported { path }),
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
        Some(IndexType::Property | IndexType::Reference | IndexType::Counter) => None,
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
    // A definition parked at `async = async-reindex` is treated as
    // synchronous: the lane runs only when an operator triggers it, and its
    // completion removes `async` again — which is the state this run
    // produces. A lane *mid-run* carries a checkpoint, and that is refused.
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

    // A rebuild would double a counter whether or not froe ran, so the
    // counter is reset instead and Oak's own replay rebuilds it.
    if definition.index_type.as_ref() == Some(&IndexType::Counter) {
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
