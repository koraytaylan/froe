//! Oak's `PathFilter`: the `includedPaths`/`excludedPaths` pair an index
//! definition may carry, and the three verdicts it gives a path.
//!
//! `docs/analysis/index-definitions.md` §4 specifies the construction, which
//! has three steps in a fixed order and two refusals. Both refusals are
//! reproduced as typed errors, because Oak's own constructor throws: a
//! definition that trips one is a definition Oak cannot build a filter for,
//! and guessing what it meant would index content Oak does not.
//!
//! The *unified* include set — not the stored one — is what the filter admits
//! and what a caller deriving a work budget must iterate, which is why
//! [`PathFilter::include_paths`] returns it.

use crate::content::node::NodeState;
use crate::index::{FilterPathSet, IndexError, IndexResult, strict_string, strict_strings};

/// What a path filter says about one path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathVerdict {
    /// Index this node.
    Include,
    /// Skip this node and its whole subtree.
    Exclude,
    /// Do not index this node, but walk into it: an include lies below.
    Traverse,
}

/// An index definition's path restriction.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PathFilter {
    included_paths: Vec<String>,
    excluded_paths: Vec<String>,
}

impl Default for PathFilter {
    /// The filter Oak returns when a definition carries neither property:
    /// everything is included.
    fn default() -> Self {
        Self {
            included_paths: vec!["/".to_owned()],
            excluded_paths: Vec::new(),
        }
    }
}

impl PathFilter {
    /// Reads the filter from a definition node.
    ///
    /// A definition with neither property gets [`PathFilter::default`]
    /// without either refusal being evaluated, exactly as `PathFilter.from`
    /// short-circuits.
    pub fn from_definition(definition: &NodeState<'_>, definition_path: &str) -> IndexResult<Self> {
        let include_property = definition.property("includedPaths")?;
        let exclude_property = definition.property("excludedPaths")?;
        if include_property.is_none() && exclude_property.is_none() {
            return Ok(Self::default());
        }
        let includes =
            string_or_strings(include_property.as_ref()).unwrap_or_else(|| vec!["/".to_owned()]);
        let excludes = string_or_strings(exclude_property.as_ref()).unwrap_or_default();
        Self::new(&includes, &excludes, definition_path)
    }

    /// Constructs the filter from an already-read include and exclude set,
    /// applying the absoluteness refusals, then the unification, then the
    /// empty-include refusal — in that order, which is Oak's.
    pub fn new(
        includes: &[String],
        excludes: &[String],
        definition_path: &str,
    ) -> IndexResult<Self> {
        refuse_relative(includes, FilterPathSet::Included, definition_path)?;
        refuse_relative(excludes, FilterPathSet::Excluded, definition_path)?;
        let (included_paths, excluded_paths) = unify(includes, excludes);
        if included_paths.is_empty() {
            return Err(IndexError::EmptyIncludeSet {
                definition_path: definition_path.to_owned(),
            });
        }
        Ok(Self {
            included_paths,
            excluded_paths,
        })
    }

    /// The verdict for `path`, in Oak's order: exclude wins, then include by
    /// ancestry, then traverse for a strict ancestor of an include.
    #[must_use]
    pub fn filter(&self, path: &str) -> PathVerdict {
        if self
            .excluded_paths
            .iter()
            .any(|excluded| excluded == path || is_ancestor(excluded, path))
        {
            return PathVerdict::Exclude;
        }
        if self
            .included_paths
            .iter()
            .any(|included| included == path || is_ancestor(included, path))
        {
            return PathVerdict::Include;
        }
        if self
            .included_paths
            .iter()
            .any(|included| is_ancestor(path, included))
        {
            return PathVerdict::Traverse;
        }
        PathVerdict::Exclude
    }

    /// The unified include set — the paths this filter actually admits,
    /// sorted, which is not necessarily what the definition stored.
    #[must_use]
    pub fn include_paths(&self) -> &[String] {
        &self.included_paths
    }

    /// The retained exclude set, sorted. Oak drops every exclude that lies
    /// under no include, so this too can be shorter than what was stored.
    #[must_use]
    pub fn exclude_paths(&self) -> &[String] {
        &self.excluded_paths
    }

    /// Whether this is the filter a definition with neither property gets.
    #[must_use]
    pub fn includes_everything(&self) -> bool {
        self.excluded_paths.is_empty()
            && self.included_paths.len() == 1
            && self.included_paths[0] == "/"
    }
}

/// Oak's `PathFilter.getStrings`: a `STRING` reads as a one-element list, a
/// `STRINGS` as itself, and every other type falls back to the caller's
/// default.
fn string_or_strings(
    property: Option<&crate::content::node::PropertyState>,
) -> Option<Vec<String>> {
    strict_strings(property).or_else(|| strict_string(property).map(|text| vec![text.to_owned()]))
}

fn refuse_relative(
    paths: &[String],
    path_set: FilterPathSet,
    definition_path: &str,
) -> IndexResult<()> {
    for path in paths {
        if !path.starts_with('/') {
            return Err(IndexError::RelativeFilterPath {
                definition_path: definition_path.to_owned(),
                path_set,
                value: path.clone(),
            });
        }
    }
    Ok(())
}

/// `PathUtils.unifyInExcludes`: drop an include equal to or under an exclude,
/// drop an include under another include, and retain only the excludes that
/// lie *strictly* under some include.
///
/// Oak collects the removals and applies them after both loops, so an include
/// that is itself about to be dropped still contributes excludes to the
/// retain set. That is reproduced here rather than optimized away: the
/// difference is unobservable only because the empty-include refusal fires
/// straight after, and a later change to either could make it observable.
fn unify(includes: &[String], excludes: &[String]) -> (Vec<String>, Vec<String>) {
    let mut removed: Vec<&String> = Vec::new();
    let mut retained: Vec<&String> = Vec::new();
    for include in includes {
        for exclude in excludes {
            if exclude == include || is_ancestor(exclude, include) {
                removed.push(include);
            } else if is_ancestor(include, exclude) {
                retained.push(exclude);
            }
        }
        for other in includes {
            if is_ancestor(include, other) {
                removed.push(other);
            }
        }
    }
    let mut surviving_includes: Vec<String> = includes
        .iter()
        .filter(|include| !removed.contains(include))
        .cloned()
        .collect();
    let mut surviving_excludes: Vec<String> = excludes
        .iter()
        .filter(|exclude| retained.contains(exclude))
        .cloned()
        .collect();
    surviving_includes.sort();
    surviving_includes.dedup();
    surviving_excludes.sort();
    surviving_excludes.dedup();
    (surviving_includes, surviving_excludes)
}

/// `PathUtils.isAncestor`: strictly an ancestor, so a path is never its own.
fn is_ancestor(ancestor: &str, path: &str) -> bool {
    if ancestor == "/" {
        return path.len() > 1 && path.starts_with('/');
    }
    path.len() > ancestor.len()
        && path.starts_with(ancestor)
        && path.as_bytes()[ancestor.len()] == b'/'
}

#[cfg(test)]
mod tests {
    use super::{PathFilter, PathVerdict, is_ancestor, unify};

    fn paths(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn filter(includes: &[&str], excludes: &[&str]) -> PathFilter {
        PathFilter::new(&paths(includes), &paths(excludes), "/oak:index/test")
            .expect("a constructible filter")
    }

    #[test]
    fn the_root_is_an_ancestor_of_everything_but_itself() {
        assert!(is_ancestor("/", "/content"));
        assert!(!is_ancestor("/", "/"));
    }

    #[test]
    fn a_name_prefix_is_not_an_ancestor() {
        assert!(!is_ancestor("/content", "/contentious"));
        assert!(is_ancestor("/content", "/content/x"));
    }

    #[test]
    fn empty_sets_include_everything() {
        let filter = PathFilter::default();
        assert!(filter.includes_everything());
        assert_eq!(filter.filter("/content"), PathVerdict::Include);
    }

    #[test]
    fn an_exclude_wins_over_the_include_that_contains_it() {
        let filter = filter(&["/content"], &["/content/private"]);
        assert_eq!(filter.filter("/content/public"), PathVerdict::Include);
        assert_eq!(filter.filter("/content/private"), PathVerdict::Exclude);
        assert_eq!(filter.filter("/content/private/x"), PathVerdict::Exclude);
    }

    #[test]
    fn an_ancestor_of_an_include_is_traversed() {
        let filter = filter(&["/content/dam"], &[]);
        assert_eq!(filter.filter("/content"), PathVerdict::Traverse);
        assert_eq!(filter.filter("/"), PathVerdict::Traverse);
        assert_eq!(filter.filter("/etc"), PathVerdict::Exclude);
    }

    #[test]
    fn a_redundant_include_under_another_include_is_dropped() {
        let filter = filter(&["/content", "/content/dam"], &[]);
        assert_eq!(filter.include_paths(), ["/content"]);
    }

    #[test]
    fn an_exclude_under_no_include_is_dropped_entirely() {
        let filter = filter(&["/content"], &["/etc/private"]);
        assert!(filter.exclude_paths().is_empty());
        assert_eq!(filter.filter("/etc/private"), PathVerdict::Exclude);
    }

    #[test]
    fn an_exclude_equal_to_an_include_drops_the_include_rather_than_being_retained() {
        let (included, excluded) = unify(&paths(&["/content"]), &paths(&["/content"]));
        assert!(included.is_empty());
        assert!(excluded.is_empty());
    }

    #[test]
    fn excluding_every_include_is_refused() {
        let error = PathFilter::new(&paths(&["/content"]), &paths(&["/"]), "/oak:index/test")
            .expect_err("an empty include set is refused");
        assert!(
            matches!(error, crate::index::IndexError::EmptyIncludeSet { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_relative_include_is_refused_by_name() {
        let error = PathFilter::new(&paths(&["content"]), &[], "/oak:index/test")
            .expect_err("a relative path is refused");
        assert!(
            matches!(
                &error,
                crate::index::IndexError::RelativeFilterPath { value, .. } if value == "content"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_relative_exclude_is_refused_before_the_unification_runs() {
        let error = PathFilter::new(&paths(&["/content"]), &paths(&["etc"]), "/oak:index/test")
            .expect_err("a relative path is refused");
        assert!(
            matches!(
                &error,
                crate::index::IndexError::RelativeFilterPath {
                    path_set: crate::index::FilterPathSet::Excluded,
                    ..
                }
            ),
            "{error}"
        );
    }
}
