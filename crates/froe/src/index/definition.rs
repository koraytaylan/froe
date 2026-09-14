//! The `oak:QueryIndexDefinition` node, and the enumeration of definition
//! paths Oak's own index path service performs.
//!
//! `docs/analysis/index-definitions.md` §1 records what makes a node a
//! definition, §1.3 the properties, §2 the lane, and §9 the two branches and
//! two preconditions of the enumeration. Every read below is at the
//! strictness of the Oak consumer whose verdict it reproduces; where two of
//! Oak's own consumers disagree, both readings are recorded — the editor's as
//! the value and the planner's as an [`IndexWarning`].

use std::collections::BTreeSet;

use crate::content::node::{NodeState, PropertyState};
use crate::content::property::PropertyValue;
use crate::index::path_filter::PathFilter;
use crate::index::value_pattern::ValuePattern;
use crate::index::{
    INDEX_CONTENT_NODE_NAME, INDEX_DEFINITIONS_NAME, INDEX_DEFINITIONS_NODE_TYPE, IndexError,
    IndexResult, IndexWarning, converting_boolean, converting_long, converting_strings,
    stored_type_name, strict_boolean, strict_name, strict_names, strict_string,
};

/// The default `blobSize` a Lucene definition gets, `1024 * 1024 - 1024`.
pub const DEFAULT_LUCENE_BLOB_SIZE: i64 = 1_047_552;

/// The smallest `blobSize` Oak accepts; a definition's value is clamped up.
pub const MINIMUM_LUCENE_BLOB_SIZE: i64 = 1024;

/// The default `resolution` a counter definition gets.
pub const DEFAULT_COUNTER_RESOLUTION: i64 = 1000;

/// The index types froe distinguishes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IndexType {
    /// The property family: `property`, including unique and node-type
    /// indexes, which are ordinary property indexes.
    Property,
    /// The reference index, which stores under `:references` and
    /// `:weakreferences`.
    Reference,
    /// The approximate descendant-node counter.
    Counter,
    /// A Lucene index.
    Lucene,
    /// An Elasticsearch index, whose data lives outside the repository.
    Elasticsearch,
    /// A disabled index, with whatever `:originalType` records it used to be.
    Disabled {
        /// The type oak-run's version purge recorded before disabling it, if
        /// any. A `disabled` index with no `:originalType` was disabled by
        /// hand, which is the distinction that tool itself draws.
        original: Option<String>,
    },
    /// The deprecated `ordered` type, which has no editor provider.
    Ordered,
    /// A type froe does not model. It is listed and never rebuilt.
    Unknown(String),
}

impl IndexType {
    /// The type's name as it is stored in the `type` property.
    #[must_use]
    pub fn stored_name(&self) -> &str {
        match self {
            IndexType::Property => "property",
            IndexType::Reference => "reference",
            IndexType::Counter => "counter",
            IndexType::Lucene => "lucene",
            IndexType::Elasticsearch => "elasticsearch",
            IndexType::Disabled { .. } => "disabled",
            IndexType::Ordered => "ordered",
            IndexType::Unknown(name) => name,
        }
    }

    fn from_stored(name: &str, original: Option<String>) -> Self {
        match name {
            "property" => IndexType::Property,
            "reference" => IndexType::Reference,
            "counter" => IndexType::Counter,
            "lucene" => IndexType::Lucene,
            "elasticsearch" => IndexType::Elasticsearch,
            "disabled" => IndexType::Disabled { original },
            "ordered" => IndexType::Ordered,
            other => IndexType::Unknown(other.to_owned()),
        }
    }
}

/// Which cycles maintain a definition.
///
/// `synchronous` is the absence of an `async` property; `sync` and `nrt` are
/// the two synonyms a hybrid definition lists beside its lane name so that a
/// synchronous cycle selects it too.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct IndexingMode {
    /// The definition has no `async` property at all.
    pub synchronous: bool,
    /// The `async` property lists `sync`.
    pub synchronous_synonym: bool,
    /// The `async` property lists `nrt`.
    pub near_real_time: bool,
}

/// The reindex bookkeeping on a definition.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ReindexState {
    /// `reindex`, read converting to a boolean the way `shouldReindex` reads
    /// it, so a `STRING` `"true"` flags a definition.
    pub flagged: bool,
    /// `reindexCount`, read converting to a long, zero when absent.
    pub count: i64,
    /// `reindex-async`, read **strictly** as a `BOOLEAN`, which is how Oak
    /// reads it and therefore the type froe must write.
    pub asynchronous: bool,
    /// `corrupt`, the date the index was marked corrupt, in its stored form.
    /// A definition carrying it is skipped by every cycle that is not
    /// reindexing it.
    pub corrupt_since: Option<String>,
}

/// The property-family fields of a definition.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PropertyIndexFields {
    /// `propertyNames`, read converting as the editor reads it.
    pub property_names: Vec<String>,
    /// `declaringNodeTypes`, read **strictly** as `NAMES`; empty for any
    /// other stored type, which yields a predicate matching nothing.
    pub declaring_node_types: Vec<String>,
    /// `unique`, read **strictly** as a `BOOLEAN`. This one field selects the
    /// storage strategy in every Oak reader and in every froe reader.
    pub unique: bool,
    /// The composed value restriction.
    pub value_pattern: ValuePattern,
}

/// The Lucene fields of a definition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LuceneIndexFields {
    /// `compatVersion`, the definition's declared compatibility version.
    pub compat_version: Option<i64>,
    /// `:version`, the hidden format version the editor writes on every
    /// reindex and import.
    pub format_version: Option<i64>,
    /// `blobSize`, clamped up to [`MINIMUM_LUCENE_BLOB_SIZE`], defaulting to
    /// [`DEFAULT_LUCENE_BLOB_SIZE`].
    pub blob_size: i64,
    /// `saveDirectoryListing`, default true, which gates both the read and
    /// the write of `dirListing`.
    pub save_directory_listing: bool,
    /// `evaluatePathRestrictions`.
    pub evaluate_path_restrictions: bool,
    /// `includePropertyTypes`.
    pub include_property_types: Vec<String>,
    /// Whether any indexing rule enables fulltext indexing.
    pub fulltext_enabled: bool,
}

impl Default for LuceneIndexFields {
    fn default() -> Self {
        Self {
            compat_version: None,
            format_version: None,
            blob_size: DEFAULT_LUCENE_BLOB_SIZE,
            save_directory_listing: true,
            evaluate_path_restrictions: false,
            include_property_types: Vec::new(),
            fulltext_enabled: false,
        }
    }
}

/// One `oak:QueryIndexDefinition` node, read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexDefinition {
    /// The absolute path of the definition node.
    pub path: String,
    /// The definition node's name, the last path element.
    pub name: String,
    /// The `type`, read strictly as a single `STRING` the way Oak's indexer
    /// reads it. A definition Oak's indexer skips carries an
    /// [`IndexWarning::IgnoredByOak`] and no type at all.
    pub index_type: Option<IndexType>,
    /// The asynchronous lane, when there is one.
    pub lane: Option<String>,
    /// Which cycles maintain this definition.
    pub indexing_mode: IndexingMode,
    /// The reindex bookkeeping.
    pub reindex: ReindexState,
    /// `supersedes`, read converting to strings.
    pub supersedes: Vec<String>,
    /// `tags`.
    pub tags: Vec<String>,
    /// `selectionPolicy`.
    pub selection_policy: Option<String>,
    /// `queryPaths`.
    pub query_paths: Vec<String>,
    /// `useIfExists`.
    pub use_if_exists: Option<String>,
    /// `deprecated`, read strictly as a `BOOLEAN`.
    pub deprecated: bool,
    /// `entryCount`, the operator's override of the estimated entry count.
    pub entry_count: Option<i64>,
    /// `keyCount`.
    pub key_count: Option<i64>,
    /// The property family's fields.
    pub property: PropertyIndexFields,
    /// Lucene's fields.
    pub lucene: LuceneIndexFields,
    /// The path restriction.
    pub path_filter: PathFilter,
    /// `resolution`, the counter's sampling resolution.
    pub resolution: Option<i64>,
    /// `seed`, the counter's or the fulltext editor's random seed, as stored.
    /// Every counter run after the one that created it narrows this to 32
    /// bits and sign-extends, so a rebuild must too.
    pub seed: Option<i64>,
    /// The visible child names, in stored order.
    pub visible_children: Vec<String>,
    /// The hidden child names, in stored order.
    pub hidden_children: Vec<String>,
    /// Facts that are not errors: Oak tolerates them, and an operator wants
    /// to know.
    pub warnings: Vec<IndexWarning>,
}

impl IndexDefinition {
    /// Reads the definition node at `path`.
    ///
    /// The only failures are the ones Oak itself throws on: an `async`
    /// property naming zero or several lanes, a path filter that cannot be
    /// constructed, and a single non-`STRING` value prefix. Everything else
    /// Oak tolerates is tolerated here and reported as a warning, so one
    /// malformed definition never kills a listing.
    pub fn read(node: &NodeState<'_>, path: &str) -> IndexResult<Self> {
        let mut warnings = Vec::new();
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();

        let primary_type = node.property("jcr:primaryType")?;
        if strict_name(primary_type.as_ref()) != Some(INDEX_DEFINITIONS_NODE_TYPE) {
            warnings.push(IndexWarning::IgnoredByOak {
                condition: format!(
                    "jcr:primaryType does not read as the NAME {INDEX_DEFINITIONS_NODE_TYPE}"
                ),
            });
        }

        let type_property = node.property("type")?;
        let index_type = if let Some(stored) = strict_string(type_property.as_ref()) {
            {
                let original = node
                    .property(":originalType")?
                    .as_ref()
                    .and_then(|property| {
                        crate::index::values_of(property)
                            .first()
                            .and_then(PropertyValue::as_text)
                    });
                Some(IndexType::from_stored(stored, original))
            }
        } else {
            {
                warnings.push(IndexWarning::IgnoredByOak {
                    condition: type_property.as_ref().map_or_else(
                        || "type is absent".to_owned(),
                        |property| {
                            format!(
                                "type is stored as {} rather than a single String",
                                stored_type_name(property)
                            )
                        },
                    ),
                });
                None
            }
        };

        let (lane, indexing_mode) = read_lane(node, path)?;
        let property = read_property_fields(node, path, &mut warnings)?;
        let path_filter = PathFilter::from_definition(node, path)?;
        let (visible_children, hidden_children) = read_child_names(node, &mut warnings)?;

        Ok(Self {
            path: path.to_owned(),
            name,
            index_type,
            lane,
            indexing_mode,
            reindex: read_reindex_state(node)?,
            supersedes: converting_strings(node.property("supersedes")?.as_ref()),
            tags: converting_strings(node.property("tags")?.as_ref()),
            selection_policy: strict_string(node.property("selectionPolicy")?.as_ref())
                .map(str::to_owned),
            query_paths: converting_strings(node.property("queryPaths")?.as_ref()),
            use_if_exists: strict_string(node.property("useIfExists")?.as_ref()).map(str::to_owned),
            deprecated: strict_boolean(node.property("deprecated")?.as_ref()),
            entry_count: converting_long(node.property("entryCount")?.as_ref()),
            key_count: converting_long(node.property("keyCount")?.as_ref()),
            property,
            lucene: read_lucene_fields(node)?,
            path_filter,
            resolution: converting_long(node.property("resolution")?.as_ref()),
            seed: converting_long(node.property("seed")?.as_ref()),
            visible_children,
            hidden_children,
            warnings,
        })
    }

    /// Whether Oak's indexer maintains this definition at all: it needs both
    /// a strictly-read `type` and the right `jcr:primaryType`.
    #[must_use]
    pub fn is_maintained_by_oak(&self) -> bool {
        !self
            .warnings
            .iter()
            .any(|warning| matches!(warning, IndexWarning::IgnoredByOak { .. }))
    }

    /// Whether the definition carries the named hidden child.
    #[must_use]
    pub fn has_hidden_child(&self, name: &str) -> bool {
        self.hidden_children.iter().any(|child| child == name)
    }

    /// Whether the definition carries the named visible child.
    #[must_use]
    pub fn has_visible_child(&self, name: &str) -> bool {
        self.visible_children.iter().any(|child| child == name)
    }

    /// The hidden children that belong to a composite-store mount, which froe
    /// reports rather than models.
    #[must_use]
    pub fn mount_children(&self) -> Vec<&str> {
        self.hidden_children
            .iter()
            .filter(|name| is_mount_decorated(name))
            .map(String::as_str)
            .collect()
    }
}

/// `IndexUtils.getAsyncLaneName`, with the two refusals it performs.
fn read_lane(node: &NodeState<'_>, path: &str) -> IndexResult<(Option<String>, IndexingMode)> {
    let Some(property) = node.property("async")? else {
        return Ok((
            None,
            IndexingMode {
                synchronous: true,
                ..IndexingMode::default()
            },
        ));
    };
    let values = converting_strings(Some(&property));
    let mode = IndexingMode {
        synchronous: false,
        synchronous_synonym: values.iter().any(|value| value == "sync"),
        near_real_time: values.iter().any(|value| value == "nrt"),
    };
    let candidates: BTreeSet<String> = values
        .into_iter()
        .filter(|value| value != "sync" && value != "nrt")
        .collect();
    match candidates.len() {
        0 => Err(IndexError::NoLaneName {
            definition_path: path.to_owned(),
        }),
        1 => Ok((candidates.into_iter().next(), mode)),
        _ => Err(IndexError::SeveralLaneNames {
            definition_path: path.to_owned(),
            lane_names: candidates.into_iter().collect(),
        }),
    }
}

fn read_reindex_state(node: &NodeState<'_>) -> IndexResult<ReindexState> {
    Ok(ReindexState {
        flagged: converting_boolean(node.property("reindex")?.as_ref()),
        count: converting_long(node.property("reindexCount")?.as_ref()).unwrap_or(0),
        asynchronous: strict_boolean(node.property("reindex-async")?.as_ref()),
        corrupt_since: node.property("corrupt")?.as_ref().and_then(|property| {
            crate::index::values_of(property)
                .first()
                .and_then(PropertyValue::as_text)
        }),
    })
}

fn read_property_fields(
    node: &NodeState<'_>,
    path: &str,
    warnings: &mut Vec<IndexWarning>,
) -> IndexResult<PropertyIndexFields> {
    let names_property = node.property("propertyNames")?;
    let property_names = converting_strings(names_property.as_ref());
    if let Some(property) = &names_property
        && strict_names(Some(property)).is_none()
    {
        warnings.push(IndexWarning::IndexedButNeverSelected {
            property_name: "propertyNames".to_owned(),
            stored_type: stored_type_name(property),
        });
    }

    let declaring_property = node.property("declaringNodeTypes")?;
    let declaring_node_types = strict_names(declaring_property.as_ref()).unwrap_or_default();
    if let Some(property) = &declaring_property
        && strict_names(Some(property)).is_none()
    {
        warnings.push(IndexWarning::DeclaringNodeTypesMatchNothing {
            stored_type: stored_type_name(property),
        });
    }

    let unique_property = node.property("unique")?;
    let unique = strict_boolean(unique_property.as_ref());
    if let Some(property) = &unique_property
        && !matches!(
            property.property_type,
            crate::content::PropertyType::Boolean
        )
    {
        warnings.push(IndexWarning::UniqueIsNotBoolean {
            stored_type: stored_type_name(property),
        });
    }

    Ok(PropertyIndexFields {
        property_names,
        declaring_node_types,
        unique,
        value_pattern: ValuePattern::from_definition(node, path, warnings)?,
    })
}

fn read_lucene_fields(node: &NodeState<'_>) -> IndexResult<LuceneIndexFields> {
    let blob_size = converting_long(node.property("blobSize")?.as_ref())
        .unwrap_or(DEFAULT_LUCENE_BLOB_SIZE)
        .max(MINIMUM_LUCENE_BLOB_SIZE);
    let save_directory_listing = node
        .property("saveDirectoryListing")?
        .as_ref()
        .is_none_or(|property| converting_boolean(Some(property)));
    Ok(LuceneIndexFields {
        compat_version: converting_long(node.property("compatVersion")?.as_ref()),
        format_version: converting_long(node.property(":version")?.as_ref()),
        blob_size,
        save_directory_listing,
        evaluate_path_restrictions: converting_boolean(
            node.property("evaluatePathRestrictions")?.as_ref(),
        ),
        include_property_types: converting_strings(node.property("includePropertyTypes")?.as_ref()),
        fulltext_enabled: fulltext_enabled(node)?,
    })
}

/// Whether any indexing rule's property definition enables fulltext
/// indexing, which is what selects `oakCodec` and therefore what plan 0010's
/// native reindex has to reproduce.
fn fulltext_enabled(node: &NodeState<'_>) -> IndexResult<bool> {
    let Some(rules) = node.child_node("indexRules")? else {
        return Ok(false);
    };
    for (_, rule) in rules.child_node_entries()? {
        let Some(properties) = rule.child_node("properties")? else {
            continue;
        };
        for (_, definition) in properties.child_node_entries()? {
            if converting_boolean(definition.property("analyzed")?.as_ref())
                || converting_boolean(definition.property("nodeScopeIndex")?.as_ref())
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn read_child_names(
    node: &NodeState<'_>,
    warnings: &mut Vec<IndexWarning>,
) -> IndexResult<(Vec<String>, Vec<String>)> {
    let mut visible = Vec::new();
    let mut hidden = Vec::new();
    for (name, _) in node.child_node_entries()? {
        if name.starts_with(':') {
            if is_mount_decorated(&name) {
                warnings.push(IndexWarning::CompositeMountPresent {
                    child_name: name.clone(),
                });
            }
            hidden.push(name);
        } else {
            visible.push(name);
        }
    }
    Ok((visible, hidden))
}

/// A hidden child belonging to a composite-store mount rather than to the
/// default one: `:oak:mount-*`, or a name ending in one of the decorated
/// suffixes `Multiplexers` and `MultiplexersLucene` build.
fn is_mount_decorated(name: &str) -> bool {
    name.starts_with(":oak:mount-")
        || (name.starts_with(':')
            && (name.ends_with("-index")
                || name.ends_with("-index-data")
                || name.ends_with("-suggest-data"))
            && name != ":index")
}

/// The definition paths, in the order Oak's index path service yields them.
///
/// `docs/analysis/index-definitions.md` §9. The precondition is evaluated
/// before either branch is chosen, exactly as `IndexPathServiceImpl` does, so
/// a disabled nodetype index refuses the whole enumeration whatever it
/// declares.
///
/// This is the order `froe index definitions` must reproduce, and it is never
/// a sort: branch one is `/oak:index`'s stored child order, branch two is the
/// node-type index's own depth-first mirror walk de-duplicated by path.
pub fn index_paths(content_root: &NodeState<'_>) -> IndexResult<Vec<String>> {
    let Some(oak_index) = content_root.child_node(INDEX_DEFINITIONS_NAME)? else {
        return Err(IndexError::NodeTypeIndexUnusable {
            found: "/oak:index is absent".to_owned(),
        });
    };
    let Some(node_type_index) = oak_index.child_node("nodetype")? else {
        return Err(IndexError::NodeTypeIndexUnusable {
            found: "/oak:index/nodetype is absent".to_owned(),
        });
    };
    let type_property = node_type_index.property("type")?;
    if strict_string(type_property.as_ref()) != Some("property") {
        return Err(IndexError::NodeTypeIndexUnusable {
            found: type_property.as_ref().map_or_else(
                || "its type property is absent".to_owned(),
                |property| {
                    format!(
                        "its type is stored as {} rather than the String \"property\"",
                        stored_type_name(property)
                    )
                },
            ),
        });
    }

    let declares_definitions =
        strict_names(node_type_index.property("declaringNodeTypes")?.as_ref())
            .is_some_and(|names| names.iter().any(|name| name == INDEX_DEFINITIONS_NODE_TYPE));
    if declares_definitions {
        return node_type_mirror_paths(content_root, &oak_index);
    }
    root_definition_paths(&oak_index)
}

/// Branch one: `/oak:index`'s stored child order, filtered to children whose
/// `jcr:primaryType` reads strictly as the `NAME` `oak:QueryIndexDefinition`.
fn root_definition_paths(oak_index: &NodeState<'_>) -> IndexResult<Vec<String>> {
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

/// Branch two: the mirror walk of whichever property index Oak's lookup
/// selects for `jcr:primaryType` and then for `jcr:mixinTypes`, depth-first
/// and de-duplicated by path.
///
/// The *values* each query looks up are the filter's own type sets, not the
/// type name: `SelectorImpl` fills `primaryTypes` from the node type's
/// `rep:primarySubtypes` plus the name itself for a non-mixin, and
/// `mixinTypes` from `rep:mixinSubtypes`. A stock store has no subtype of
/// `oak:QueryIndexDefinition`, so each query looks up exactly one key; where
/// there were several, Oak's own order over them is a `HashSet` iteration
/// order and so is not a thing to reproduce, which is why froe walks them
/// sorted and says so here.
fn node_type_mirror_paths(
    content_root: &NodeState<'_>,
    oak_index: &NodeState<'_>,
) -> IndexResult<Vec<String>> {
    let selector = NodeTypeSelector::read(content_root, INDEX_DEFINITIONS_NODE_TYPE)?;
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    for (property_name, values) in [
        ("jcr:primaryType", &selector.primary_types),
        ("jcr:mixinTypes", &selector.mixin_types),
    ] {
        let index = select_property_index(oak_index, property_name, &selector.supertypes)?
            .ok_or_else(|| IndexError::NodeTypeIndexHasNoData {
                property_name: property_name.to_owned(),
            })?;
        let Some(entries) = index.child_node(INDEX_CONTENT_NODE_NAME)? else {
            return Err(IndexError::NodeTypeIndexHasNoData {
                property_name: property_name.to_owned(),
            });
        };
        for value in values {
            let Some(key) = entries.child_node(&mirror_key(value))? else {
                continue;
            };
            collect_mirror_matches(&key, "", &mut paths, &mut seen)?;
        }
    }
    Ok(paths)
}

/// The three type sets `SelectorImpl` derives for a node-type restriction,
/// read from `/jcr:system/jcr:nodeTypes` exactly as
/// `NodeStateNodeTypeInfoProvider` reads them: every one a strict `NAMES`
/// read, with the type's own name added to the supertype set and to whichever
/// of the two subtype sets its `jcr:isMixin` selects.
struct NodeTypeSelector {
    supertypes: BTreeSet<String>,
    primary_types: BTreeSet<String>,
    mixin_types: BTreeSet<String>,
}

impl NodeTypeSelector {
    fn read(content_root: &NodeState<'_>, node_type_name: &str) -> IndexResult<Self> {
        let node_type = match content_root.child_node("jcr:system")? {
            None => None,
            Some(system) => match system.child_node("jcr:nodeTypes")? {
                None => None,
                Some(types) => types.child_node(node_type_name)?,
            },
        };
        let mut supertypes = BTreeSet::new();
        let mut primary_types = BTreeSet::new();
        let mut mixin_types = BTreeSet::new();
        let mut is_mixin = false;
        if let Some(node_type) = &node_type {
            supertypes.extend(
                strict_names(node_type.property("rep:supertypes")?.as_ref()).unwrap_or_default(),
            );
            primary_types.extend(
                strict_names(node_type.property("rep:primarySubtypes")?.as_ref())
                    .unwrap_or_default(),
            );
            mixin_types.extend(
                strict_names(node_type.property("rep:mixinSubtypes")?.as_ref()).unwrap_or_default(),
            );
            is_mixin = strict_boolean(node_type.property("jcr:isMixin")?.as_ref());
        }
        supertypes.insert(node_type_name.to_owned());
        if is_mixin {
            mixin_types.insert(node_type_name.to_owned());
        } else {
            primary_types.insert(node_type_name.to_owned());
        }
        Ok(Self {
            supertypes,
            primary_types,
            mixin_types,
        })
    }
}

/// The key a mirror index stores `value` under: the URL encoding
/// `PropertyIndexUtil.encode` produces.
fn mirror_key(value: &str) -> String {
    crate::java::url_encode(value.encode_utf16())
}

/// A depth-first walk of a mirror key's subtree, collecting the paths of
/// every node carrying `match = true`, hidden children skipped below the key
/// level as Oak's visible editor skips them.
fn collect_mirror_matches(
    node: &NodeState<'_>,
    path: &str,
    paths: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
) -> IndexResult<()> {
    if strict_boolean(node.property("match")?.as_ref()) {
        let absolute = if path.is_empty() {
            "/".to_owned()
        } else {
            path.to_owned()
        };
        if seen.insert(absolute.clone()) {
            paths.push(absolute);
        }
    }
    for (name, child) in node.child_node_entries()? {
        if name.starts_with(':') {
            continue;
        }
        collect_mirror_matches(&child, &format!("{path}/{name}"), paths, seen)?;
    }
    Ok(())
}

/// `PropertyIndexLookup.getIndexNode`, with the node-type restriction the
/// printer's query carries.
///
/// In `/oak:index` stored order: the `type` is read converting and arrays are
/// skipped; `propertyNames` and `declaringNodeTypes` go through `getNames`,
/// which is strict `NAMES` with a converting `STRINGS` fallback; and the
/// index must have an `:index` child. Then the **first** definition whose
/// `declaringNodeTypes` names one of `supertypes` wins immediately; one
/// naming only other types is **never** selected; and the first without
/// `declaringNodeTypes` is the fallback — which in a Sling store is
/// `/oak:index/nodetype`, whatever precedes it in child order.
fn select_property_index<'provider>(
    oak_index: &NodeState<'provider>,
    property_name: &str,
    supertypes: &BTreeSet<String>,
) -> IndexResult<Option<NodeState<'provider>>> {
    let mut fallback = None;
    for (_, child) in oak_index.child_node_entries()? {
        if !is_candidate_property_index(&child, property_name)? {
            continue;
        }
        let declaring = child.property("declaringNodeTypes")?;
        if declaring.is_some() {
            if names_converting(declaring.as_ref())
                .iter()
                .any(|name| supertypes.contains(name))
            {
                return Ok(Some(child));
            }
        } else if fallback.is_none() {
            fallback = Some(child);
        }
    }
    Ok(fallback)
}

/// The three conditions a definition must meet before its node-type
/// restriction is even considered.
fn is_candidate_property_index(child: &NodeState<'_>, property_name: &str) -> IndexResult<bool> {
    let Some(type_property) = child.property("type")? else {
        return Ok(false);
    };
    if is_array(&type_property) {
        return Ok(false);
    }
    let stored = crate::index::values_of(&type_property)
        .first()
        .and_then(PropertyValue::as_text);
    if stored.as_deref() != Some("property") {
        return Ok(false);
    }
    if !names_converting(child.property("propertyNames")?.as_ref())
        .iter()
        .any(|name| name == property_name)
    {
        return Ok(false);
    }
    Ok(child.child_node(INDEX_CONTENT_NODE_NAME)?.is_some())
}

/// `PropertyIndexLookup.getNames`: strict `NAMES`, falling back to a
/// converting read with a warning, which is why the query side selects an
/// index the planner would not.
fn names_converting(property: Option<&PropertyState>) -> Vec<String> {
    strict_names(property).unwrap_or_else(|| converting_strings(property))
}

fn is_array(property: &PropertyState) -> bool {
    matches!(
        property.values,
        crate::content::node::PropertyValues::Multiple(_)
    )
}
