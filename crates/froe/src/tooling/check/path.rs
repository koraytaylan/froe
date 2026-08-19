//! Checking one requested path at one revision: resolving it, reading the
//! node shallowly, and saying where it sits relative to the root.

use super::{
    BinaryCheck, Error, NodeState, PackedRecordSet, PathRoot, PathToCheck, RecordIdentifier,
    Result, SegmentProvider, SubtreeChecks, VerifiedNodeCount, materialize_binary, verify_subtree,
};

/// Checks one path at one revision: resolves it under its root,
/// re-probes the sub-paths found corrupt at newer revisions (each must
/// exist and pass a shallow check, or the path stays inconsistent and
/// the full traversal is skipped — Java's `findFirstCorruptedPathInSet`),
/// then verifies the whole subtree. Returns a short reason on failure.
pub(crate) fn check_one_path(
    provider: &dyn SegmentProvider,
    super_root: &NodeState<'_>,
    path_to_check: &mut PathToCheck,
    binary_check: BinaryCheck,
    verified: &mut PackedRecordSet,
    progress: &mut VerifiedNodeCount<'_>,
) -> std::result::Result<(), String> {
    let node = match resolve_path(super_root, &path_to_check.root, &path_to_check.path) {
        Ok(Some(node)) => node,
        Ok(None) => return Err("path does not exist".to_owned()),
        Err(error) => return Err(error.to_string()),
    };
    for corrupt_path in &path_to_check.corrupt_paths {
        match resolve_relative(&node, corrupt_path) {
            Ok(Some(corrupt_node)) => {
                if let Err(reason) =
                    check_node_shallow(provider, corrupt_node.record_identifier(), binary_check)
                {
                    return Err(format!(
                        "previously corrupt path {}: {reason}",
                        display_relative(corrupt_path)
                    ));
                }
            }
            Ok(None) => {
                return Err(format!(
                    "previously corrupt path {} does not exist",
                    display_relative(corrupt_path)
                ));
            }
            Err(error) => {
                return Err(format!(
                    "previously corrupt path {}: {error}",
                    display_relative(corrupt_path)
                ));
            }
        }
    }
    match verify_subtree(
        provider,
        node.record_identifier(),
        SubtreeChecks {
            binaries: binary_check,
            stable_identifiers: false,
        },
        verified,
        progress,
    ) {
        Ok(()) => Ok(()),
        Err(corrupt) => {
            if !path_to_check.corrupt_paths.contains(&corrupt.path) {
                path_to_check.corrupt_paths.push(corrupt.path.clone());
            }
            Err(format!(
                "{} at {}",
                corrupt.reason,
                display_relative(&corrupt.path)
            ))
        }
    }
}

/// Resolves a relative path (empty = the node itself) under a node.
pub(crate) fn resolve_relative<'provider>(
    node: &NodeState<'provider>,
    relative_path: &str,
) -> Result<Option<NodeState<'provider>>> {
    let mut current = *node;
    for name in relative_path
        .split('/')
        .filter(|segment| !segment.is_empty())
    {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// Renders a relative corrupt path for messages: the checked node itself
/// is `/`.
pub(crate) fn display_relative(relative_path: &str) -> &str {
    if relative_path.is_empty() {
        "/"
    } else {
        relative_path
    }
}

/// Checks one node without recursing: every property is decoded, and —
/// when asked — every inline binary is read, exactly Java's `checkNode`.
/// Returns the decoded properties, which the walk already paid for, so a
/// caller collecting content facts never decodes a node twice.
pub(crate) fn check_node_shallow(
    provider: &dyn SegmentProvider,
    record: RecordIdentifier,
    binary_check: BinaryCheck,
) -> std::result::Result<Vec<crate::content::node::PropertyState>, String> {
    let node = NodeState::new(provider, record);
    let properties = node.properties().map_err(|error| error.to_string())?;
    if binary_check == BinaryCheck::EveryBlock {
        for property in &properties {
            match &property.values {
                crate::content::node::PropertyValues::Single(value) => {
                    materialize_binary(provider, value).map_err(|error| error.to_string())?;
                }
                crate::content::node::PropertyValues::Multiple(values) => {
                    for value in values {
                        materialize_binary(provider, value).map_err(|error| error.to_string())?;
                    }
                }
            }
        }
    }
    Ok(properties)
}

/// Resolves a content path under its root: the head's content root, or a
/// checkpoint's root snapshot. User paths are always content paths — a
/// content node literally named `checkpoints` is reachable, unlike Java's
/// path-hijacking alternatives.
pub(crate) fn resolve_path<'provider>(
    super_root: &NodeState<'provider>,
    root: &PathRoot,
    path: &str,
) -> Result<Option<NodeState<'provider>>> {
    let mut current = match root {
        PathRoot::Head => match super_root.child_node("root")? {
            Some(content_root) => content_root,
            None => {
                return Err(Error::InvalidFormat {
                    details: "the super-root has no \"root\" child node".to_owned(),
                });
            }
        },
        PathRoot::Checkpoint(name) => {
            let Some(checkpoints) = super_root.child_node("checkpoints")? else {
                return Ok(None);
            };
            let Some(checkpoint) = checkpoints.child_node(name)? else {
                return Ok(None);
            };
            match checkpoint.child_node("root")? {
                Some(snapshot_root) => snapshot_root,
                None => return Ok(None),
            }
        }
    };
    for name in path.split('/').filter(|segment| !segment.is_empty()) {
        match current.child_node(name)? {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}
