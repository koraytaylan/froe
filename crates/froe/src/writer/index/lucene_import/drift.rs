//! Whether the definitions file describes the definition in the store.
//!
//! The file is **never byte-compared** with the store, because oak-run
//! prints the lane-switched, already-reindexed *copy* of the definition:
//! `reindexCount` one above the store's, `refresh = true` from the lane
//! revert, a `seed` if the run created one, and the copy's `:status`. A
//! byte comparison would refuse every honest build.
//!
//! So the comparison is the one Oak itself uses for drift — the one its
//! Lucene index-information provider performs over visible clones, which
//! keep hidden properties and drop hidden child nodes — extended by the
//! properties an out-of-band build legitimately rewrites.
//!
//! **The tolerances are directional, and that is the whole difficulty.**
//! Plan 0006's comparison takes a set of property *names* to ignore, and a
//! symmetric ignore would excuse real drift: a `seed` that differs on both
//! sides is drift, not a rewrite. So the ignore set handles the name and
//! this module asserts the five directions itself.

use crate::content::node::NodeState;
use crate::index::status::{DefinitionDifference, DifferenceKind, definition_drift};
use crate::index::{IndexError, IndexResult};

/// Property names an out-of-band build may rewrite.
///
/// Ignored by name in the underlying comparison, then checked directionally
/// below. `reindex` and `reindexCount` are already in plan 0006's own
/// ignore set.
pub const REWRITTEN_PROPERTY_NAMES: [&str; 4] = ["refresh", "seed", "corrupt", "indexImportState"];

/// The child an out-of-band build's document maker persists.
pub const FACETS_CHILD_NAME: &str = "facets";

/// What the comparison concluded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DriftVerdict {
    /// Differences that are not tolerated, sorted.
    pub differences: Vec<DefinitionDifference>,
}

impl DriftVerdict {
    /// Whether the file describes the definition in the store.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.differences.is_empty()
    }

    /// The first difference, for a refusal that names one thing.
    #[must_use]
    pub fn first(&self) -> Option<&DefinitionDifference> {
        self.differences.first()
    }
}

/// Compares the file's definition against the store's.
///
/// `file` is the definition as the definitions file carries it, written into
/// a scratch tree so both sides are node states; `stored` is the store's own
/// definition node.
///
/// The five directional tolerances:
///
/// * `refresh` — only on the file's side. The lane revert sets it; a store
///   that carries one the file lacks is drift.
/// * `seed` — only when the store lacks it. A run that created one is
///   ordinary; two different seeds are drift, because the counter they
///   drive would disagree.
/// * `corrupt` and `indexImportState` — only when the store has them and
///   the file does not. The copy's reindex cleared them, and a
///   corrupt-flagged index is the usual reason for an out-of-band build.
///   The reverse — a file flagging an index the store considers healthy —
///   is drift.
/// * a `facets` child — only when the file has it and the store does not.
pub fn compare(file: &NodeState<'_>, stored: &NodeState<'_>) -> IndexResult<DriftVerdict> {
    let ignored: Vec<String> = REWRITTEN_PROPERTY_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();

    // `facets` is directional and must be keyed on the **store**, which is
    // what governs it. When the store has no `facets` child, the file's is
    // dropped before the comparison; otherwise it is dropped from neither,
    // so two equal children compare equal, two differing children are
    // drift, and a store-only child is the removed visible child it is.
    //
    // Dropping it unconditionally from the file's side would manufacture a
    // removed visible child in the ordinary case, where both sides carry
    // one: Oak creates that child the moment any facet configuration is
    // built, and the dump keeps visible children.
    let store_has_facets = stored.child_node(FACETS_CHILD_NAME)?.is_some();

    let mut differences = definition_drift(file, stored, &ignored)?;
    differences.retain(|difference| {
        !(!store_has_facets
            && difference.kind == DifferenceKind::Added
            && is_facets_path(&difference.path))
    });

    // Now the directions the name-based ignore could not express.
    differences.extend(directional_property_differences(file, stored)?);
    differences.sort();
    differences.dedup();
    Ok(DriftVerdict { differences })
}

/// Whether a difference path is the `facets` child or anything under it.
fn is_facets_path(path: &str) -> bool {
    path == format!("/{FACETS_CHILD_NAME}") || path.starts_with(&format!("/{FACETS_CHILD_NAME}/"))
}

/// The four rewritten properties, checked in the one direction each allows.
fn directional_property_differences(
    file: &NodeState<'_>,
    stored: &NodeState<'_>,
) -> IndexResult<Vec<DefinitionDifference>> {
    let mut differences = Vec::new();
    for name in REWRITTEN_PROPERTY_NAMES {
        let in_file = rendered_property(file, name)?;
        let in_store = rendered_property(stored, name)?;
        let tolerated = match name {
            // The lane revert sets `refresh` on the file's side.
            "refresh" => in_store.is_none(),
            // A run that created a seed is ordinary; two different seeds
            // are drift, because the counters they drive would disagree.
            "seed" => in_store.is_none() || in_file == in_store,
            // The copy's reindex cleared these. A file flagging an index
            // the store considers healthy is the other direction, and is
            // drift.
            "corrupt" | "indexImportState" => in_file.is_none(),
            _ => true,
        };
        if tolerated || in_file == in_store {
            continue;
        }
        differences.push(DefinitionDifference {
            path: format!("/{name}"),
            kind: match (in_file.is_some(), in_store.is_some()) {
                (true, false) => DifferenceKind::Added,
                (false, true) => DifferenceKind::Removed,
                _ => DifferenceKind::Changed,
            },
        });
    }
    Ok(differences)
}

/// One property rendered as text, for comparison.
fn rendered_property(node: &NodeState<'_>, name: &str) -> IndexResult<Option<String>> {
    let Some(property) = node.property(name)? else {
        return Ok(None);
    };
    let rendered = match &property.values {
        crate::content::node::PropertyValues::Single(value) => value.as_text().unwrap_or_default(),
        crate::content::node::PropertyValues::Multiple(values) => values
            .iter()
            .map(|value| value.as_text().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\u{1}"),
    };
    Ok(Some(format!("{:?}:{rendered}", property.property_type)))
}

/// A drift refusal, naming the first difference.
#[must_use]
pub fn refusal(index_path: &str, verdict: &DriftVerdict) -> IndexError {
    let detail = verdict.first().map_or_else(
        || "the definitions file does not describe this definition".to_owned(),
        |difference| {
            let what = match difference.kind {
                DifferenceKind::Added => "the file has",
                DifferenceKind::Removed => "the store has",
                DifferenceKind::Changed => "the two disagree on",
            };
            format!("{what} {}", difference.path)
        },
    );
    IndexError::Record(crate::Error::InvalidFormat {
        details: format!(
            "the definitions file does not describe {index_path} as the store holds it: \
             {detail}. froe imports index *data*, never a definition change — make \
             definition changes through oak-run or AEM first."
        ),
    })
}
