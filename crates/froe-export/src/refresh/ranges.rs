//! Turning the paths a diff reports into dirty ranges: the spans of the
//! old export a refresh replaces, and the lookup the merge asks whether
//! a row falls inside one.

use super::{HashSet, NodeDifference};

/// One dirty path range: the old rows it replaces and the replacement
/// to re-export, if any.
pub(crate) struct DirtyRange {
    /// The range's root path.
    pub(crate) path: String,
    /// Whether the range covers the whole subtree below `path` (`true`,
    /// for added and removed nodes) or only the node's own rows
    /// (`false`, for property changes).
    pub(crate) subtree: bool,
    /// What replaces the range's old rows.
    pub(crate) replacement: Replacement,
}

/// What replaces a dirty range's old rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Replacement {
    /// Nothing: the rows are excised without replacement (a removed
    /// node).
    Excise,
    /// Re-export the range's root with this depth limit — `None` for
    /// the whole subtree, `Some(0)` for just the node's own rows.
    ReExport { depth: Option<usize> },
}

/// How many path segments a normalized absolute path carries: `/` is 0,
/// `/a/b` is 2.
pub(crate) fn path_depth(path: &str) -> usize {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

/// Maps a diff to dirty ranges, sorted by path. Ranges beyond the
/// export's depth limit carry no rows — the old file has none there
/// and a full re-export would write none — so they are dropped; an
/// added subtree inside the limit re-exports with its remaining depth.
#[cfg(test)]
pub(crate) fn dirty_ranges(
    differences: &[NodeDifference],
    root_path: &str,
    depth_limit: Option<usize>,
) -> Vec<DirtyRange> {
    let mut ranges: Vec<DirtyRange> = differences
        .iter()
        .filter_map(|difference| dirty_range_for(difference, root_path, depth_limit))
        .collect();
    normalize_dirty_ranges(&mut ranges);
    ranges
}

/// The dirty range one difference implies, or `None` when the change falls
/// outside the export's depth limit.
///
/// Split out from [`dirty_ranges`] so a refresh can fold each difference as
/// the diff produces it. Holding the whole change set only to reduce it to
/// ranges meant both collections were live at once, and the change set is
/// far the larger of the two — every entry carries full before and after
/// property state.
pub(crate) fn dirty_range_for(
    difference: &NodeDifference,
    root_path: &str,
    depth_limit: Option<usize>,
) -> Option<DirtyRange> {
    let root_depth = path_depth(root_path);
    {
        let (path, subtree, replacement) = match difference {
            NodeDifference::NodeAdded { path } => {
                (path, true, Replacement::ReExport { depth: None })
            }
            NodeDifference::NodeRemoved { path } => (path, true, Replacement::Excise),
            NodeDifference::PropertyChanged { path, .. } => {
                (path, false, Replacement::ReExport { depth: Some(0) })
            }
        };
        let range_depth = path_depth(path).saturating_sub(root_depth);
        // Rows beyond the export's depth limit exist in neither the old
        // file nor a full re-export, whatever the change — dropping the
        // range here keeps a deep removal from rewriting both tables
        // for zero effect.
        if depth_limit.is_some_and(|limit| range_depth > limit) {
            return None;
        }
        let replacement = match (replacement, depth_limit) {
            (Replacement::Excise, _) => Replacement::Excise,
            (Replacement::ReExport { depth: None }, Some(limit)) => Replacement::ReExport {
                depth: Some(limit - range_depth),
            },
            (reexport @ Replacement::ReExport { .. }, _) => reexport,
        };
        Some(DirtyRange {
            path: path.clone(),
            subtree,
            replacement,
        })
    }
}

/// Sorts and folds dirty ranges into the canonical set the refresh applies.
pub(crate) fn normalize_dirty_ranges(ranges: &mut Vec<DirtyRange>) {
    ranges.sort_by(|first, second| first.path.cmp(&second.path));
    // The diff never reports nested or duplicated ranges, but the merge
    // relies on it: fold any duplicate defensively — the subtree shape
    // and a present replacement win. `dedup_by` passes the later
    // element first and removes it when the closure returns true, so
    // the retained earlier element accumulates the folded facts.
    ranges.dedup_by(|later, retained| {
        if later.path != retained.path {
            return false;
        }
        retained.subtree |= later.subtree;
        if retained.replacement == Replacement::Excise {
            retained.replacement = later.replacement;
        }
        true
    });
}

/// Whether `path` lies inside `range`: the range root itself, or — for
/// subtree ranges — any descendant. The descendant test respects the
/// `/` boundary, so `/a/bc` is not under `/a/b`.
pub(crate) fn path_in_range(path: &str, range: &DirtyRange) -> bool {
    path == range.path || (range.subtree && path_under(path, &range.path))
}

/// Whether `path` is a proper descendant of `ancestor`.
pub(crate) fn path_under(path: &str, ancestor: &str) -> bool {
    if ancestor == "/" {
        return path.starts_with('/') && path != "/";
    }
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The dirty ranges indexed for containment queries: a path is dirty
/// when it is an exact range root or has a subtree range root among its
/// ancestors (itself included).
pub(crate) struct RangeIndex<'ranges> {
    pub(crate) exact: HashSet<&'ranges str>,
    pub(crate) subtree: HashSet<&'ranges str>,
}

impl<'ranges> RangeIndex<'ranges> {
    pub(crate) fn new(ranges: &'ranges [DirtyRange]) -> Self {
        let mut index = Self {
            exact: HashSet::new(),
            subtree: HashSet::new(),
        };
        for range in ranges {
            if range.subtree {
                index.subtree.insert(range.path.as_str());
            } else {
                index.exact.insert(range.path.as_str());
            }
        }
        index
    }

    /// Whether `path` falls inside any dirty range.
    pub(crate) fn contains(&self, path: &str) -> bool {
        if self.exact.contains(path) || self.subtree.contains(path) {
            return true;
        }
        let mut rest = path;
        while let Some((parent, _)) = rest.rsplit_once('/') {
            let parent = if parent.is_empty() { "/" } else { parent };
            if self.subtree.contains(parent) {
                return true;
            }
            if parent == "/" {
                return false;
            }
            rest = parent;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{DirtyRange, RangeIndex, Replacement, dirty_ranges, path_in_range, path_under};
    use froe::content::{PropertyState, PropertyType, PropertyValue, PropertyValues};
    use froe::tooling::diff::{NodeDifference, PropertyChange};

    // ---- pure-logic unit tests --------------------------------------

    fn property_change(path: &str) -> NodeDifference {
        NodeDifference::PropertyChanged {
            path: path.to_owned(),
            change: PropertyChange::Added(PropertyState {
                name: "p".to_owned(),
                property_type: PropertyType::String,
                values: PropertyValues::Single(PropertyValue::String("v".to_owned())),
            }),
        }
    }

    fn range_paths(ranges: &[DirtyRange]) -> Vec<(&str, bool, Replacement)> {
        ranges
            .iter()
            .map(|range| (range.path.as_str(), range.subtree, range.replacement))
            .collect()
    }

    #[test]
    fn dirty_ranges_map_the_difference_kinds() {
        let differences = vec![
            property_change("/root/b"),
            NodeDifference::NodeRemoved {
                path: "/root/a".to_owned(),
            },
            NodeDifference::NodeAdded {
                path: "/root/c".to_owned(),
            },
        ];
        let ranges = dirty_ranges(&differences, "/root", None);
        assert_eq!(
            range_paths(&ranges),
            vec![
                ("/root/a", true, Replacement::Excise),
                ("/root/b", false, Replacement::ReExport { depth: Some(0) }),
                ("/root/c", true, Replacement::ReExport { depth: None }),
            ],
            "sorted by path; removals excise, changes re-export the node, additions the subtree"
        );
    }

    #[test]
    fn dirty_ranges_apply_the_depth_limit() {
        let differences = vec![
            NodeDifference::NodeAdded {
                path: "/root/a".to_owned(),
            },
            NodeDifference::NodeAdded {
                path: "/root/a/b".to_owned(),
            },
            property_change("/root/a/b/c"),
        ];
        let ranges = dirty_ranges(&differences, "/root", Some(1));
        assert_eq!(
            range_paths(&ranges),
            vec![("/root/a", true, Replacement::ReExport { depth: Some(0) })],
            "an addition at the limit keeps its root only; deeper ranges carry no rows"
        );
        let wider = dirty_ranges(&differences, "/root", Some(2));
        assert_eq!(
            range_paths(&wider),
            vec![
                ("/root/a", true, Replacement::ReExport { depth: Some(1) }),
                ("/root/a/b", true, Replacement::ReExport { depth: Some(0) }),
            ],
            "additions inside the limit re-export with their remaining depth"
        );
    }

    #[test]
    fn dirty_ranges_drop_out_of_depth_removals() {
        let differences = vec![NodeDifference::NodeRemoved {
            path: "/root/a/b".to_owned(),
        }];
        assert!(
            dirty_ranges(&differences, "/root", Some(1)).is_empty(),
            "a removal beyond the limit touches no exported row"
        );
        assert_eq!(
            range_paths(&dirty_ranges(&differences, "/root", Some(2))),
            vec![("/root/a/b", true, Replacement::Excise)],
            "at the limit it excises"
        );
    }

    #[test]
    fn dirty_ranges_fold_duplicates_defensively() {
        let differences = vec![
            NodeDifference::NodeRemoved {
                path: "/root/a".to_owned(),
            },
            property_change("/root/a"),
        ];
        let ranges = dirty_ranges(&differences, "/root", None);
        assert_eq!(
            range_paths(&ranges),
            vec![("/root/a", true, Replacement::ReExport { depth: Some(0) })],
        );
    }

    #[test]
    fn range_containment_respects_the_slash_boundary() {
        let subtree = DirtyRange {
            path: "/a/b".to_owned(),
            subtree: true,
            replacement: Replacement::Excise,
        };
        let exact = DirtyRange {
            path: "/a/b".to_owned(),
            subtree: false,
            replacement: Replacement::Excise,
        };
        assert!(path_in_range("/a/b", &exact));
        assert!(path_in_range("/a/b/c", &subtree));
        assert!(!path_in_range("/a/b/c", &exact));
        assert!(!path_in_range("/a/bc", &subtree), "/a/bc is not under /a/b");
        assert!(!path_in_range("/a", &subtree));
        assert!(path_under("/anything", "/"));
        assert!(!path_under("/", "/"));
    }

    #[test]
    fn the_range_index_finds_containing_ranges() {
        let ranges = [
            DirtyRange {
                path: "/a".to_owned(),
                subtree: true,
                replacement: Replacement::Excise,
            },
            DirtyRange {
                path: "/b/c".to_owned(),
                subtree: false,
                replacement: Replacement::Excise,
            },
            DirtyRange {
                path: "/".to_owned(),
                subtree: true,
                replacement: Replacement::Excise,
            },
        ];
        let index = RangeIndex::new(&ranges[..2]);
        assert!(index.contains("/a/deep/path"));
        assert!(index.contains("/b/c"));
        assert!(!index.contains("/b/c/d"));
        assert!(!index.contains("/b"));
        let root_index = RangeIndex::new(&ranges[2..]);
        assert!(root_index.contains("/anything/at/all"));
        assert!(root_index.contains("/"));
    }
}
