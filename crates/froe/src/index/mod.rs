//! Oak's query indexes: definitions, asynchronous lanes, status nodes and
//! the structures each index type stores in the repository.
//!
//! An Oak index is three things, and this module models all three:
//!
//! * a **definition** — a child of a node named `oak:index` whose
//!   `jcr:primaryType` is `oak:QueryIndexDefinition` and which carries a
//!   `type` ([`definition`]);
//! * an **asynchronous lane** — an entry under `/:async` recording the
//!   checkpoint a lane has indexed up to ([`lanes`]);
//! * **storage** — the hidden children an index writes: `:index` for the
//!   property family ([`property`]), `:references` and `:weakreferences`
//!   for the reference index, `:cnt` counters for the counter index
//!   ([`counter`]), and `:data` for Lucene ([`lucene`]).
//!
//! Everything here is read-only. The specifications it implements are
//! `docs/analysis/index-definitions.md`,
//! `docs/analysis/index-property-storage.md` and
//! `docs/analysis/index-lucene-storage.md`; where a rule below looks
//! arbitrary, those documents cite the Java method it came from.
//!
//! # Reading type strictly is the whole game
//!
//! Oak's typed getters on a node are **strict**: `getName` returns a value
//! only for a single `NAME`, `getNames` only for a `NAMES` array,
//! `getBoolean` only for a `BOOLEAN`, and so on. A read through
//! `PropertyState.getValue(Type)` **converts**. Which kind of read each of
//! Oak's own consumers performs is observable, because the consumers
//! disagree: a `propertyNames` stored as a `STRING` is *indexed* by the
//! editor, which converts, and never *selected* by the query planner, which
//! does not. This module reproduces each read at the strictness its Oak
//! consumer uses and reports the disagreement as an [`IndexWarning`] rather
//! than silently picking one side.
//!
//! # Every reader takes a provider and a root
//!
//! Readers take `&dyn SegmentProvider` plus a root record identifier, the
//! way [`crate::content::NodeState`] does, rather than a
//! [`crate::store::Repository`]. The mutating plans run the same readers
//! over an open write session before publication, where no `Repository`
//! exists yet.

use std::fmt;

use crate::content::node::{PropertyState, PropertyValues};
use crate::content::property::{PropertyType, PropertyValue};

pub mod counter;
pub mod definition;
pub mod definitions_json;
pub mod definitions_json_reader;
pub mod inventory;
pub mod lanes;
pub mod lucene;
pub mod path_filter;
pub mod property;
pub mod status;
pub mod value_pattern;

pub use definition::{IndexDefinition, IndexType, IndexingMode, ReindexState, index_paths};
pub use definitions_json::{ChildFilter, RenderOptions, render as render_definitions_json};
pub use inventory::{IndexInfo, IndexInventory};
pub use lanes::{AsyncLane, AsyncLanes};
pub use path_filter::{PathFilter, PathVerdict};
pub use status::{StatusNode, StoredDefinition, definition_drift};
pub use value_pattern::ValuePattern;

/// The name of the node definitions live under, at the root and, in stores
/// that allow it, anywhere in the content tree.
pub const INDEX_DEFINITIONS_NAME: &str = "oak:index";

/// The `jcr:primaryType` a node must carry, as a single `NAME`, for Oak's
/// indexer to treat it as a definition.
pub const INDEX_DEFINITIONS_NODE_TYPE: &str = "oak:QueryIndexDefinition";

/// The hidden child the property family stores its entries under.
pub const INDEX_CONTENT_NODE_NAME: &str = ":index";

/// The node the asynchronous indexer keeps its per-lane state on.
pub const ASYNC_NODE_NAME: &str = ":async";

/// The result of an index read.
pub type IndexResult<Value> = std::result::Result<Value, IndexError>;

/// A failure that belongs to one definition, or to the store as a whole.
///
/// Anything attributable to a single definition that Oak itself tolerates is
/// an [`IndexWarning`] rather than a variant here; a variant is for the cases
/// where Oak throws, so that froe refuses where Oak refuses instead of
/// inventing a reading.
#[derive(Debug)]
#[non_exhaustive]
pub enum IndexError {
    /// Reading a record failed. This belongs to no single definition.
    Record(crate::Error),
    /// An `async` property named no lane: every value was `sync` or `nrt`,
    /// or there were none. Oak's `IndexUtils.getAsyncLaneName` throws here.
    NoLaneName {
        /// The definition whose `async` property named no lane.
        definition_path: String,
    },
    /// An `async` property named more than one lane, which
    /// `IndexUtils.getAsyncLaneName` also throws on.
    SeveralLaneNames {
        /// The definition whose `async` property named several lanes.
        definition_path: String,
        /// The candidate names, sorted, with `sync` and `nrt` removed.
        lane_names: Vec<String>,
    },
    /// An `includedPaths` or `excludedPaths` value was not an absolute path.
    /// Oak's `PathFilter` constructor throws before it does anything else.
    RelativeFilterPath {
        /// The definition carrying the offending value.
        definition_path: String,
        /// Whether the value came from the include set or the exclude set.
        path_set: FilterPathSet,
        /// The offending value.
        value: String,
    },
    /// Unifying the include set against the exclude set left it empty, which
    /// Oak's `PathFilter` constructor refuses.
    EmptyIncludeSet {
        /// The definition whose filter cannot be constructed.
        definition_path: String,
    },
    /// A single-valued `valueIncludedPrefixes` or `valueExcludedPrefixes` was
    /// not a `STRING`. Oak reads it as `null` and then throws a
    /// `NullPointerException` inside `ValuePattern.matches`, so the
    /// definition is one Oak cannot index at all.
    NonStringPrefixValue {
        /// The definition carrying the offending property.
        definition_path: String,
        /// The property name, `valueIncludedPrefixes` or
        /// `valueExcludedPrefixes`.
        property_name: String,
    },
    /// A value pattern froe cannot evaluate was reached by the composed
    /// match rule. froe carries no regular-expression engine and will not
    /// approximate Java's.
    UnsupportedValuePattern {
        /// The definition carrying the pattern.
        definition_path: String,
        /// The stored pattern text, for the operator to read.
        pattern: String,
    },
    /// `/oak:index/nodetype` is absent, or its `type` does not read strictly
    /// as the `STRING` `property`. Oak's index path service refuses the whole
    /// enumeration here, before it chooses a branch.
    NodeTypeIndexUnusable {
        /// What was found instead, for the message.
        found: String,
    },
    /// The node-type index declares `oak:QueryIndexDefinition`, so the
    /// enumeration takes the query branch — but no `property` index with an
    /// `:index` child covers `jcr:primaryType` or `jcr:mixinTypes`, which is
    /// the state Oak's own `NodeTypeIndex.query` throws on.
    NodeTypeIndexHasNoData {
        /// The property whose lookup found nothing.
        property_name: String,
    },
}

impl From<crate::Error> for IndexError {
    fn from(error: crate::Error) -> Self {
        IndexError::Record(error)
    }
}

impl fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Record(source) => write!(formatter, "{source}"),
            IndexError::NoLaneName { definition_path } => write!(
                formatter,
                "the index definition at {definition_path} has an \"async\" property that names \
                 no lane: every value is \"sync\" or \"nrt\""
            ),
            IndexError::SeveralLaneNames {
                definition_path,
                lane_names,
            } => write!(
                formatter,
                "the index definition at {definition_path} names several asynchronous lanes \
                 ({}); Oak accepts exactly one",
                lane_names.join(", ")
            ),
            IndexError::RelativeFilterPath {
                definition_path,
                path_set,
                value,
            } => write!(
                formatter,
                "the index definition at {definition_path} has a relative path {value:?} in its \
                 {path_set} list; Oak requires absolute paths"
            ),
            IndexError::EmptyIncludeSet { definition_path } => write!(
                formatter,
                "the index definition at {definition_path} excludes every one of its included \
                 paths, so Oak cannot construct its path filter"
            ),
            IndexError::NonStringPrefixValue {
                definition_path,
                property_name,
            } => write!(
                formatter,
                "the index definition at {definition_path} stores a single-valued \
                 {property_name} that is not a STRING; Oak reads it as null and fails on the \
                 first value it tests"
            ),
            IndexError::UnsupportedValuePattern {
                definition_path,
                pattern,
            } => write!(
                formatter,
                "the index definition at {definition_path} restricts values with the regular \
                 expression {pattern:?}, which froe does not evaluate"
            ),
            IndexError::NodeTypeIndexUnusable { found } => write!(
                formatter,
                "the node-type index at /oak:index/nodetype is unusable ({found}), so the index \
                 paths cannot be enumerated"
            ),
            IndexError::NodeTypeIndexHasNoData { property_name } => write!(
                formatter,
                "no property index with an :index child covers {property_name}, so the node-type \
                 index cannot be queried for index definitions"
            ),
        }
    }
}

impl std::error::Error for IndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IndexError::Record(source) => Some(source),
            _ => None,
        }
    }
}

/// Which of a path filter's two lists a value came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilterPathSet {
    /// `includedPaths`.
    Included,
    /// `excludedPaths`.
    Excluded,
}

impl fmt::Display for FilterPathSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterPathSet::Included => formatter.write_str("included"),
            FilterPathSet::Excluded => formatter.write_str("excluded"),
        }
    }
}

/// A fact about one definition that is not an error: Oak tolerates it, and a
/// listing must still succeed, but an operator wants to know.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum IndexWarning {
    /// The definition's `type` or `jcr:primaryType` does not read strictly,
    /// so Oak's indexer skips the node entirely and the index is not
    /// maintained.
    IgnoredByOak {
        /// Which read failed, in Oak's own words.
        condition: String,
    },
    /// A property the editor reads converting and the query planner reads
    /// strictly is stored with a type only the editor accepts. The index is
    /// populated and never selected.
    IndexedButNeverSelected {
        /// The property name.
        property_name: String,
        /// The stored type, as its JCR name plus `[]` when multi-valued.
        stored_type: String,
    },
    /// `valueExcludedPrefixes` is stored with a type the query side reads as
    /// an empty exclusion. The planner then answers queries from an index the
    /// editor never populated with those values: wrong results, not merely an
    /// unused index.
    ExcludedPrefixesIgnoredByQueries {
        /// The stored type, as its JCR name plus `[]` when multi-valued.
        stored_type: String,
    },
    /// `declaringNodeTypes` is not stored as `NAMES`, so Oak's type predicate
    /// matches nothing and the index indexes nothing.
    DeclaringNodeTypesMatchNothing {
        /// The stored type, as its JCR name plus `[]` when multi-valued.
        stored_type: String,
    },
    /// `unique` is stored as something other than a `BOOLEAN`, so every Oak
    /// reader treats the index as a mirror index whatever the value says.
    UniqueIsNotBoolean {
        /// The stored type, as its JCR name plus `[]` when multi-valued.
        stored_type: String,
    },
    /// The definition restricts values with a regular expression. froe
    /// reports the definition but refuses to rebuild or check it.
    ValuePatternNotEvaluated {
        /// The stored pattern text.
        pattern: String,
    },
    /// The definition's parent is not `/oak:index`, so it is a non-root
    /// index. It is listed; the mutating commands refuse it.
    NonRootDefinition,
    /// The store carries mount-decorated hidden children, which froe detects
    /// and reports but does not model.
    CompositeMountPresent {
        /// The decorated child name, such as `:oak-libs-index-data`.
        child_name: String,
    },
    /// The non-root definitions could not be enumerated, with the condition
    /// that held. Oak's own verdict in this state is the same.
    NonRootDefinitionsNotEnumerated {
        /// Which of the path service's conditions held.
        condition: String,
    },
}

impl fmt::Display for IndexWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexWarning::IgnoredByOak { condition } => write!(
                formatter,
                "ignored by Oak's indexer ({condition}), so this index is not maintained"
            ),
            IndexWarning::IndexedButNeverSelected {
                property_name,
                stored_type,
            } => write!(
                formatter,
                "{property_name} is stored as {stored_type}; the editor converts it and indexes \
                 it, but the query planner reads it strictly and never selects this index"
            ),
            IndexWarning::ExcludedPrefixesIgnoredByQueries { stored_type } => write!(
                formatter,
                "valueExcludedPrefixes is stored as {stored_type}; queries ignore the exclusion \
                 and answer from an index the editor never populated with those values"
            ),
            IndexWarning::DeclaringNodeTypesMatchNothing { stored_type } => write!(
                formatter,
                "declaringNodeTypes is stored as {stored_type} rather than Name[], so Oak's type \
                 predicate matches nothing and this index indexes nothing"
            ),
            IndexWarning::UniqueIsNotBoolean { stored_type } => write!(
                formatter,
                "unique is stored as {stored_type} rather than Boolean, so Oak treats this as a \
                 mirror index whatever the value says"
            ),
            IndexWarning::ValuePatternNotEvaluated { pattern } => write!(
                formatter,
                "valuePattern {pattern:?} is a regular expression froe does not evaluate"
            ),
            IndexWarning::NonRootDefinition => {
                formatter.write_str("the definition is not a child of /oak:index")
            }
            IndexWarning::CompositeMountPresent { child_name } => write!(
                formatter,
                "the hidden child {child_name} belongs to a composite-store mount, which froe \
                 reports but does not model"
            ),
            IndexWarning::NonRootDefinitionsNotEnumerated { condition } => write!(
                formatter,
                "non-root index definitions were not enumerated ({condition})"
            ),
        }
    }
}

/// The stored type of `property`, as its JCR name plus `[]` when the property
/// is multi-valued. This is what an [`IndexWarning`] names, so an operator can
/// see the type Oak actually saw.
pub(crate) fn stored_type_name(property: &PropertyState) -> String {
    let base = property.property_type.jcr_name();
    match property.values {
        PropertyValues::Single(_) => base.to_owned(),
        PropertyValues::Multiple(_) => format!("{base}[]"),
    }
}

/// The values of `property` as the slice every read below iterates.
pub(crate) fn values_of(property: &PropertyState) -> &[PropertyValue] {
    match &property.values {
        PropertyValues::Single(value) => std::slice::from_ref(value),
        PropertyValues::Multiple(values) => values.as_slice(),
    }
}

/// Oak's `NodeState.getName`: a value only for a *single* `NAME`.
pub(crate) fn strict_name(property: Option<&PropertyState>) -> Option<&str> {
    let property = property?;
    match (&property.values, property.property_type) {
        (PropertyValues::Single(PropertyValue::Name(text)), PropertyType::Name) => Some(text),
        _ => None,
    }
}

/// Oak's `NodeState.getNames`: values only for a multi-valued `NAMES`.
pub(crate) fn strict_names(property: Option<&PropertyState>) -> Option<Vec<String>> {
    let property = property?;
    match (&property.values, property.property_type) {
        (PropertyValues::Multiple(values), PropertyType::Name) => Some(
            values
                .iter()
                .filter_map(PropertyValue::as_text)
                .collect::<Vec<_>>(),
        ),
        _ => None,
    }
}

/// Oak's `NodeState.getString`: a value only for a *single* `STRING`.
pub(crate) fn strict_string(property: Option<&PropertyState>) -> Option<&str> {
    let property = property?;
    match (&property.values, property.property_type) {
        (PropertyValues::Single(PropertyValue::String(text)), PropertyType::String) => Some(text),
        _ => None,
    }
}

/// Oak's `NodeState.getStrings`: values only for a multi-valued `STRINGS`.
pub(crate) fn strict_strings(property: Option<&PropertyState>) -> Option<Vec<String>> {
    let property = property?;
    match (&property.values, property.property_type) {
        (PropertyValues::Multiple(values), PropertyType::String) => Some(
            values
                .iter()
                .filter_map(PropertyValue::as_text)
                .collect::<Vec<_>>(),
        ),
        _ => None,
    }
}

/// Oak's `NodeState.getBoolean`: `true` only for a *single* `BOOLEAN` whose
/// value is true, `false` for everything else including absence.
pub(crate) fn strict_boolean(property: Option<&PropertyState>) -> bool {
    matches!(
        property.map(|property| (&property.values, property.property_type)),
        Some((
            PropertyValues::Single(PropertyValue::Boolean(true)),
            PropertyType::Boolean
        ))
    )
}

/// Oak's `PropertyState.getValue(Type.STRINGS)`: every value as the string it
/// is stored as, whatever the property's type.
///
/// In a segment store this is not a conversion at all —
/// `SegmentPropertyState.getValue` returns the stored string verbatim for
/// every request that is not `BINARY` — which is why
/// [`PropertyValue::as_text`] is the whole implementation. A binary
/// contributes nothing, as it does to Oak's own key derivation.
pub(crate) fn converting_strings(property: Option<&PropertyState>) -> Vec<String> {
    property.map_or_else(Vec::new, |property| {
        values_of(property)
            .iter()
            .filter_map(PropertyValue::as_text)
            .collect()
    })
}

/// Oak's `PropertyState.getValue(Type.BOOLEAN)` over the first value:
/// `Boolean.parseBoolean` of the stored string, so a `STRING` `"true"`
/// converts to `true` and everything else to `false`.
pub(crate) fn converting_boolean(property: Option<&PropertyState>) -> bool {
    property
        .and_then(|property| values_of(property).first())
        .and_then(PropertyValue::as_text)
        .is_some_and(|text| text.eq_ignore_ascii_case("true"))
}

/// Oak's `PropertyState.getValue(Type.LONG)` over the first value:
/// `Long.parseLong` of the stored string, so a `STRING` `"500"` converts.
/// `None` where Java would throw a `NumberFormatException`.
pub(crate) fn converting_long(property: Option<&PropertyState>) -> Option<i64> {
    let text = property
        .and_then(|property| values_of(property).first())
        .and_then(PropertyValue::as_text)?;
    crate::java::parse_java_signed_decimal(&text, i64::MIN.into(), i64::MAX.into())
        .and_then(|value| i64::try_from(value).ok())
}
