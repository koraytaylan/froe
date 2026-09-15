//! The indexing rules a definition declares, and the property definition a
//! name resolves to.
//!
//! `docs/analysis/lucene-oak-documents.md` §1 and §2, from
//! `oak-search`'s `IndexDefinition.IndexingRule` and `PropertyDefinition`
//! and from `oak-lucene`'s `LuceneIndexDefinition.createCodec`.
//!
//! This is the definition side of the document maker: which rule covers a
//! node, which property definition covers a name, and which definitions
//! froe reproduces at all.
//!
//! # What froe refuses
//!
//! A reindex that quietly writes fewer fields than Oak would is a query
//! that stops matching, so every construct this plan does not port is a
//! refusal that names itself — [`IndexError::UnsupportedDefinition`].

use std::collections::BTreeMap;

use crate::content::node::NodeState;
use crate::content::property::PropertyValue;
use crate::index::lucene::documents::aggregate::{self, Aggregate};
use crate::index::lucene::documents::name_pattern::{ALL_PROPERTIES, NamePattern};
use crate::index::value_pattern::ValuePattern;
use crate::index::{
    IndexError, IndexResult, IndexWarning, converting_boolean, converting_long, converting_strings,
    strict_name, strict_names, strict_string, values_of,
};

/// `PropertyDefinition.DEFAULT_BOOST`.
pub const DEFAULT_BOOST: f32 = 1.0;

/// `FulltextIndexPlanner.DEFAULT_PROPERTY_WEIGHT`, which is a consumer-JVM
/// system property with this default. froe writes no weight into the
/// index, so the value only ever reaches a query planner froe does not
/// run; it is carried because a definition may state it.
pub const DEFAULT_PROPERTY_WEIGHT: i64 = 5;

/// The name `OakCodec` registers under in `META-INF/services`, which is
/// what `Codec.forName` resolves an explicit `codec` property through.
pub const OAK_CODEC_NAME: &str = "oakCodec";

/// `FulltextIndexConstants.PROPDEF_PROP_NODE_NAME`.
pub const NODE_NAME_PROPERTY: &str = ":nodeName";

/// The codec `LuceneIndexDefinition.createCodec` resolves to, which is the
/// first question froe asks of a definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecVerdict {
    /// Fulltext-enabled with no explicit `codec`: the composition plan
    /// 0009 writes, and the only one froe reproduces.
    OakCodec,
    /// An explicit `codec` property, resolved by registered name.
    Named(String),
    /// Neither: Lucene's own default.
    Lucene46,
}

/// One `indexRules/<type>` child.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one field per property of Oak's own definition, which are genuinely independent flags"
)]
#[derive(Clone, Debug, PartialEq)]
pub struct IndexingRule {
    /// The child's name, which is the node type the rule covers.
    pub node_type_name: String,
    /// `inherited`, whose default is **true**: the rule then covers every
    /// registered subtype of its node type.
    pub inherited: bool,
    /// The node types the rule registers under, which is its own name and
    /// — when inherited — the subtypes the type registry records.
    registered_types: Vec<String>,
    /// `boost`, the rule's own.
    pub boost: f32,
    /// `includePropertyTypes`, empty for the rule's own default of all.
    pub include_property_types: Vec<String>,
    /// `nodeTypeIndex`, which makes the rule a pure node-type index over
    /// `jcr:primaryType` and `jcr:mixinTypes`.
    pub node_type_index: bool,
    /// `indexNodeName`, or a property definition named `:nodeName`.
    pub node_name_indexed: bool,
    /// The exactly-named property definitions, keyed by their name folded
    /// to lower case: Oak's own map is a `TreeMap` under
    /// `String.CASE_INSENSITIVE_ORDER`, and its values are what
    /// `propDefinitions` holds — in key order, not in stored order.
    pub properties: BTreeMap<String, PropertyDefinition>,
    /// The `isRegexp` ones, in stored order, which a name reaches only
    /// after every exact name has missed.
    pub patterns: Vec<(NamePattern, PropertyDefinition)>,
    /// The rule's node aggregates.
    pub aggregate: Aggregate,
    /// `aggregate.hasNodeAggregates() || hasAnyFullTextEnabledProperty()`,
    /// which is what selects `oakCodec`.
    pub fulltext_enabled: bool,
    /// `aggregate.hasNodeAggregates() || anyNodeScopeIndexedProperty()`.
    pub node_fulltext_indexed: bool,
    /// `areAlMatchingNodeByTypeIndexed`: whether **every** node the rule
    /// covers is indexed, which is what lets an otherwise empty document
    /// be written at all.
    pub indexes_all_nodes_of_matching_type: bool,
}

impl IndexingRule {
    /// The property definition a name resolves to, or `None` when the rule
    /// indexes nothing under it.
    ///
    /// `IndexingRule.getConfig`: the exact map first, case-insensitively,
    /// then the patterns in order. **A hidden name goes through the
    /// patterns too** — Oak's own comment calls that bug compatibility,
    /// and it is why `:nodeName` reaches a catch-all pattern.
    #[must_use]
    pub fn config_of(&self, property_name: &str) -> Option<&PropertyDefinition> {
        if let Some(definition) = self.properties.get(&fold_case(property_name)) {
            return Some(definition);
        }
        self.patterns
            .iter()
            .find(|(pattern, _)| pattern.matches(property_name))
            .map(|(_, definition)| definition)
    }

    /// Whether the rule registers under a node type name, which is Oak's
    /// `ntReg.isNodeType(name, rule.getNodeTypeName())`.
    #[must_use]
    pub fn registers_under(&self, node_type_name: &str) -> bool {
        self.registered_types
            .iter()
            .any(|registered| registered == node_type_name)
    }

    /// Every property definition the rule holds, exact and patterned.
    pub fn definitions(&self) -> impl Iterator<Item = &PropertyDefinition> {
        self.properties
            .values()
            .chain(self.patterns.iter().map(|(_, definition)| definition))
    }

    /// The **relative** definitions, which Oak turns into
    /// `Aggregate.PropertyInclude`s and combines in front of the rule's
    /// node aggregates:
    ///
    /// ```java
    /// for (PropertyDefinition pd : propConfigs.values()) {
    ///     if (pd.relative) propIncludes.add(new Aggregate.PropertyInclude(pd));
    /// }
    /// …
    /// includes.addAll(propAggregate.getIncludes());
    /// if (nodeAggregate != null) includes.addAll(nodeAggregate.getIncludes());
    /// ```
    ///
    /// A relative name reaches nothing through [`Self::config_of`] — that
    /// is asked about a node's own property name, whose parent is empty —
    /// so this walk is the **only** way such a definition contributes a
    /// field, and `jcr:content/…` is the shape AEM's own definitions are
    /// written in.
    #[must_use]
    pub fn property_includes(&self) -> Vec<PropertyInclude<'_>> {
        let exact = self.properties.values().map(|definition| PropertyInclude {
            definition,
            pattern: None,
        });
        let patterned = self
            .patterns
            .iter()
            .map(|(pattern, definition)| PropertyInclude {
                definition,
                pattern: Some(pattern),
            });
        exact
            .chain(patterned)
            .filter(|include| include.definition.relative)
            .collect()
    }
}

/// One relative property definition as the aggregate walk needs it: the
/// definition, and — for a regular-expression one — the compiled name
/// expression its ancestor's property names are matched against.
#[derive(Clone, Copy, Debug)]
pub struct PropertyInclude<'rule> {
    /// The definition itself.
    pub definition: &'rule PropertyDefinition,
    /// The pattern, for an `isRegexp` definition.
    pub pattern: Option<&'rule NamePattern>,
}

/// One `properties/<name>` child of a rule.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one field per property of Oak's own definition, which are genuinely independent flags"
)]
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyDefinition {
    /// The child node's own name, which is what the operator sees.
    pub node_name: String,
    /// `name`, defaulting to the child's name — the property this
    /// definition covers, or the pattern text when `isRegexp`.
    pub name: String,
    /// `isRegexp`.
    pub is_regexp: bool,
    /// Whether the name holds a `/`, which makes it a relative property
    /// and an aggregate include besides.
    pub relative: bool,
    /// The path elements above a relative name.
    pub ancestors: Vec<String>,
    /// `index`, whose default is true — and whose falsity forces every
    /// other flag below to false, whatever the definition says.
    pub index: bool,
    /// `propertyIndex`, which `sync` forces on.
    pub property_index: bool,
    /// `analyzed`, which an explicit `boost` forces on.
    pub analyzed: bool,
    /// `nodeScopeIndex`.
    pub node_scope_index: bool,
    /// `ordered`.
    pub ordered: bool,
    /// `useInExcerpt`, which is what stores the field.
    pub use_in_excerpt: bool,
    /// `useInSuggest`.
    pub use_in_suggest: bool,
    /// `useInSpellcheck`.
    pub use_in_spellcheck: bool,
    /// `facets`.
    pub facet: bool,
    /// `nullCheckEnabled`.
    pub null_check_enabled: bool,
    /// `notNullCheckEnabled`.
    pub not_null_check_enabled: bool,
    /// `excludeFromAggregation`.
    pub exclude_from_aggregation: bool,
    /// `boost`.
    pub boost: f32,
    /// `weight`.
    pub weight: i64,
    /// `oak.experimental.includePropertyTypes` on the definition itself,
    /// empty for its default of **all** of them.
    ///
    /// Oak gates the typed fields and the analyzed field with this list
    /// and the per-property fulltext loop with the **rule's**, which is a
    /// different list with a different default — §3.3.
    pub included_property_types: Vec<String>,
    /// `type`, the declared property type, which the ordered doc value
    /// takes whatever the property's own type is.
    pub declared_type: Option<String>,
    /// `unique`, which forces `sync` on.
    pub unique: bool,
    /// `sync`.
    pub sync: bool,
    /// The composed value restriction.
    pub value_pattern: ValuePattern,
}

impl PropertyDefinition {
    /// `PropertyDefinition.fulltextEnabled`, which is **not** the same as
    /// "takes part in fulltext": `useInSuggest` and `useInSpellcheck` are
    /// not in it.
    #[must_use]
    pub const fn fulltext_enabled(&self) -> bool {
        self.index && (self.analyzed || self.node_scope_index)
    }
}

/// Every rule of one definition, with the definition-level settings the
/// document maker also consults.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one field per property of Oak's own definition, which are genuinely independent flags"
)]
#[derive(Clone, Debug, PartialEq)]
pub struct IndexingRules {
    /// The rules, in the order `indexRules` yields its children.
    pub rules: Vec<IndexingRule>,
    /// Every `aggregates/<type>` child, as its own map keyed by the type
    /// it is declared under.
    ///
    /// A rule carries the one its own node type names, which is what
    /// selects `oakCodec` and what the walk starts from. This list is the
    /// **re-aggregation's** lookup, and it is a different question: Oak's
    /// `IndexDefinition.getAggregate(String)` is a plain map lookup, so an
    /// aggregate declared for a type no `indexRules` child covers is
    /// entered all the same, and a node covered by a rule through *type
    /// inheritance* does not inherit that rule's aggregate.
    pub aggregates: Vec<Aggregate>,
    /// `codec`'s verdict.
    pub codec: CodecVerdict,
    /// `analyzers/@indexOriginalTerm`.
    pub index_original_term: bool,
    /// `suggestion/@suggestAnalyzed`, falling back to the definition root.
    pub suggest_analyzed: bool,
    /// `evaluatePathRestrictions`, which routes `:ancestors`.
    pub evaluate_path_restrictions: bool,
    /// `maxFieldLength`, as stored; `None` for the default.
    pub maximum_field_length: Option<i64>,
    /// Whether a `tika` child was present. Nothing is read from it, and an
    /// operator wants to know that froe saw it.
    pub has_tika_configuration: bool,
}

/// Folds a name the way Oak's `String.CASE_INSENSITIVE_ORDER` does for the
/// names a definition holds.
fn fold_case(name: &str) -> String {
    name.to_lowercase()
}

// ---------------------------------------------------------- the reading

/// `ConfigUtil.getOptionalValue` for a boolean: absent is the default,
/// present is read **converting**, so a `STRING` `"true"` counts.
fn optional_boolean(node: &NodeState<'_>, name: &str, default: bool) -> IndexResult<bool> {
    let property = node.property(name)?;
    Ok(match property {
        None => default,
        Some(property) => converting_boolean(Some(&property)),
    })
}

/// The same for a long.
fn optional_long(node: &NodeState<'_>, name: &str, default: i64) -> IndexResult<i64> {
    Ok(converting_long(node.property(name)?.as_ref()).unwrap_or(default))
}

/// `ConfigUtil.getOptionalValue` for a float, which Oak reads through
/// `Type.DOUBLE`.
fn optional_float(node: &NodeState<'_>, name: &str, default: f32) -> IndexResult<f32> {
    let Some(property) = node.property(name)? else {
        return Ok(default);
    };
    let Some(text) = values_of(&property)
        .first()
        .and_then(PropertyValue::as_text)
    else {
        return Ok(default);
    };
    Ok(text.parse::<f32>().unwrap_or(default))
}

impl PropertyDefinition {
    /// Reads one `properties/<name>` child, in the order Oak's own
    /// constructor reads it — the order matters, because `index` gates
    /// almost everything after it and `boost` forces `analyzed` on.
    ///
    /// # Errors
    ///
    /// Propagates the property reads, and refuses a construct this plan
    /// does not port.
    pub fn read(
        definition_path: &str,
        node_name: &str,
        node: &NodeState<'_>,
        warnings: &mut Vec<IndexWarning>,
    ) -> IndexResult<Self> {
        for unsupported in [
            "function",
            "dynamicBoost",
            "useInSimilarity",
            "similarityTags",
        ] {
            if node.property(unsupported)?.is_some() {
                return Err(IndexError::UnsupportedDefinition {
                    definition_path: definition_path.to_owned(),
                    feature: format!("the property definition {node_name} sets {unsupported}"),
                });
            }
        }
        let is_regexp = optional_boolean(node, "isRegexp", false)?;
        let name = strict_string(node.property("name")?.as_ref())
            .map_or_else(|| node_name.to_owned(), str::to_owned);
        let relative = is_relative_property(&name);
        let boost = optional_float(node, "boost", DEFAULT_BOOST)?;
        let weight = optional_long(node, "weight", DEFAULT_PROPERTY_WEIGHT)?;
        let index = optional_boolean(node, "index", true)?;
        // Everything below is `getOptionalValueIfIndexed`: an unindexed
        // definition reads as false however it is written.
        let if_indexed = |name: &str, default: bool| -> IndexResult<bool> {
            if index {
                optional_boolean(node, name, default)
            } else {
                Ok(false)
            }
        };
        // An explicit boost means the field MUST be analyzed, whatever
        // `analyzed` says.
        let analyzed = if node.property("boost")?.is_some() {
            true
        } else {
            if_indexed("analyzed", false)?
        };
        let unique = if_indexed("unique", false)?;
        let sync = unique || if_indexed("sync", false)?;
        let null_check_enabled = if_indexed("nullCheckEnabled", false)?;
        if null_check_enabled && is_regexp {
            // `PropertyDefinition.validate` throws, so Oak cannot load the
            // definition at all.
            return Err(IndexError::UnsupportedDefinition {
                definition_path: definition_path.to_owned(),
                feature: format!(
                    "the property definition {node_name} sets nullCheckEnabled on a regular \
                     expression, which Oak's own validation refuses"
                ),
            });
        }
        Ok(Self {
            node_name: node_name.to_owned(),
            ancestors: ancestors_of(&name),
            name,
            is_regexp,
            relative,
            index,
            property_index: sync || if_indexed("propertyIndex", false)?,
            analyzed,
            node_scope_index: if_indexed("nodeScopeIndex", false)?,
            ordered: if_indexed("ordered", false)?,
            use_in_excerpt: if_indexed("useInExcerpt", false)?,
            use_in_suggest: if_indexed("useInSuggest", false)?,
            use_in_spellcheck: if_indexed("useInSpellcheck", false)?,
            facet: if_indexed("facets", false)?,
            null_check_enabled,
            not_null_check_enabled: if_indexed("notNullCheckEnabled", false)?,
            exclude_from_aggregation: if_indexed("excludeFromAggregation", false)?,
            boost,
            weight,
            declared_type: strict_string(node.property("type")?.as_ref()).map(str::to_owned),
            // `PropertyDefinition.includedPropertyTypes`, which is the
            // definition's **own** `oak.experimental.includePropertyTypes`
            // with a default of every type — not the rule's list, and not
            // the index definition's.
            included_property_types: converting_strings(
                node.property("oak.experimental.includePropertyTypes")?
                    .as_ref(),
            ),
            unique,
            sync,
            value_pattern: ValuePattern::from_definition(node, definition_path, warnings)?,
        })
    }
}

/// `PropertyDefinition.isRelativeProperty`: a name holding a `/` that is
/// neither absolute nor the catch-all pattern.
fn is_relative_property(name: &str) -> bool {
    !name.starts_with('/') && name != ALL_PROPERTIES && name.contains('/')
}

/// `PropertyDefinition.computeAncestors`: the path elements above the
/// name, and none at all for the catch-all pattern.
fn ancestors_of(name: &str) -> Vec<String> {
    if name == ALL_PROPERTIES {
        return Vec::new();
    }
    let Some(at) = name.rfind('/') else {
        return Vec::new();
    };
    name[..at]
        .split('/')
        .filter(|element| !element.is_empty())
        .map(str::to_owned)
        .collect()
}

impl IndexingRule {
    /// Reads one `indexRules/<type>` child.
    ///
    /// # Errors
    ///
    /// Propagates the reads, and refuses a rule this plan does not port.
    pub fn read(
        definition_path: &str,
        node_type_name: &str,
        node: &NodeState<'_>,
        content_root: &NodeState<'_>,
        aggregates: &[Aggregate],
        warnings: &mut Vec<IndexWarning>,
    ) -> IndexResult<Self> {
        let inherited = optional_boolean(node, "inherited", true)?;
        let node_type_index = optional_boolean(node, "nodeTypeIndex", false)?;
        let mut properties: BTreeMap<String, PropertyDefinition> = BTreeMap::new();
        let mut patterns = Vec::new();
        if node_type_index {
            // A pure node-type rule indexes `jcr:primaryType` and
            // `jcr:mixinTypes` and ignores every property definition. Its
            // `sync` is the rule's own, and a synchronous rule keeps a
            // `:property-index` this plan does not build.
            if optional_boolean(node, "sync", false)? {
                return Err(IndexError::UnsupportedDefinition {
                    definition_path: definition_path.to_owned(),
                    feature: format!(
                        "the rule {node_type_name} is a synchronous nodeTypeIndex, which Oak \
                         keeps a :property-index for"
                    ),
                });
            }
        } else if let Some(property_node) = node.child_node("properties")? {
            for (name, child) in property_node.child_node_entries()? {
                if properties.contains_key(&fold_case(&name)) {
                    // `collectPropConfigs` guards on the **child's** name
                    // against a map keyed by each definition's `name`, and
                    // creates nothing when it hits — so a second child
                    // whose own name a previous definition already claimed
                    // is skipped whole, refusals included.
                    continue;
                }
                let definition =
                    PropertyDefinition::read(definition_path, &name, &child, warnings)?;
                if definition.sync || definition.unique {
                    return Err(IndexError::UnsupportedDefinition {
                        definition_path: definition_path.to_owned(),
                        feature: format!(
                            "the property definition {name} of rule {node_type_name} is {}, which \
                             Oak keeps a :property-index for",
                            if definition.unique { "unique" } else { "sync" }
                        ),
                    });
                }
                if definition.is_regexp {
                    let pattern = NamePattern::parse(definition_path, &definition.name)?;
                    patterns.push((pattern, definition));
                } else {
                    // A plain `put`: two children whose `name` properties
                    // agree case-insensitively leave the **later** one, and
                    // the earlier is gone from `propDefinitions` entirely.
                    properties.insert(fold_case(&definition.name), definition);
                }
            }
        }
        let aggregate = aggregate::for_node_type(aggregates, node_type_name);
        let fulltext_enabled = aggregate.has_node_aggregates()
            || properties
                .values()
                .chain(patterns.iter().map(|(_, definition)| definition))
                .any(PropertyDefinition::fulltext_enabled);
        let node_fulltext_indexed = aggregate.has_node_aggregates()
            || properties
                .values()
                .chain(patterns.iter().map(|(_, definition)| definition))
                .any(|definition| definition.node_scope_index);
        // `jcr:primaryType` is on every node, so a rule that indexes it
        // covers every node it applies to; so does a non-relative
        // `nullCheckEnabled` property, which OAK-1085 is about.
        let indexes_all_nodes_of_matching_type = node_type_index
            || node_fulltext_indexed
            || properties
                .values()
                .any(|definition| definition.null_check_enabled && !definition.relative)
            || properties.contains_key(&fold_case("jcr:primaryType"));
        let node_name_indexed = optional_boolean(node, "indexNodeName", false)?
            || properties
                .values()
                .any(|definition| definition.name == NODE_NAME_PROPERTY);
        let rule = Self {
            node_type_name: node_type_name.to_owned(),
            inherited,
            registered_types: registered_types(content_root, node_type_name, inherited)?,
            boost: optional_float(node, "boost", DEFAULT_BOOST)?,
            include_property_types: converting_strings(
                node.property("includePropertyTypes")?.as_ref(),
            ),
            node_type_index,
            node_name_indexed,
            properties,
            patterns,
            aggregate,
            fulltext_enabled,
            node_fulltext_indexed,
            indexes_all_nodes_of_matching_type,
        };
        rule.validate(definition_path)?;
        Ok(rule)
    }

    /// `IndexingRule.validateRuleDefinition`, which throws rather than
    /// warning: Oak cannot load such a definition at all.
    fn validate(&self, definition_path: &str) -> IndexResult<()> {
        if self.node_type_name == "nt:base"
            && self
                .definitions()
                .any(|definition| definition.null_check_enabled)
        {
            return Err(IndexError::UnsupportedDefinition {
                definition_path: definition_path.to_owned(),
                feature: "an nt:base rule carries a nullCheckEnabled property definition, which \
                          Oak's own rule validation refuses"
                    .to_owned(),
            });
        }
        Ok(())
    }
}

/// The node types a rule registers under: its own name, and — when
/// inherited — every subtype the registry records.
///
/// Oak walks every registered node type and keeps the ones
/// `isNodeType(name, ruleType)` accepts. The subtype lists Oak's own
/// `TypePredicate` reads are the same closure from the other side, and
/// reading them is one lookup rather than a walk of the whole registry:
/// `rep:primarySubtypes` holds the primary types below a type and
/// `rep:mixinSubtypes` the mixins, so the primary-and-mixin asymmetry
/// `TypePredicate` documents falls out of the content rather than being
/// imposed here.
fn registered_types(
    content_root: &NodeState<'_>,
    node_type_name: &str,
    inherited: bool,
) -> IndexResult<Vec<String>> {
    let mut names = vec![node_type_name.to_owned()];
    if !inherited {
        return Ok(names);
    }
    let node_type = match content_root.child_node("jcr:system")? {
        None => None,
        Some(system) => match system.child_node("jcr:nodeTypes")? {
            None => None,
            Some(types) => types.child_node(node_type_name)?,
        },
    };
    if let Some(node_type) = node_type {
        for property in ["rep:primarySubtypes", "rep:mixinSubtypes"] {
            names.extend(strict_names(node_type.property(property)?.as_ref()).unwrap_or_default());
        }
    }
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

impl IndexingRules {
    /// Reads every rule of one definition, with the definition-level
    /// settings the document maker consults.
    ///
    /// # Errors
    ///
    /// Refuses, by name, a definition this plan does not reproduce: one
    /// whose codec verdict is not `oakCodec`, an old-format definition
    /// with no `indexRules`, a `compatVersion` of 1, a `maxFieldLength` of
    /// zero, a consumer-registered analyzer, and every construct
    /// [`PropertyDefinition::read`] and [`IndexingRule::read`] refuse.
    pub fn read(
        definition: &NodeState<'_>,
        definition_path: &str,
        content_root: &NodeState<'_>,
        warnings: &mut Vec<IndexWarning>,
    ) -> IndexResult<Self> {
        let refuse = |feature: String| IndexError::UnsupportedDefinition {
            definition_path: definition_path.to_owned(),
            feature,
        };
        let compat_version = optional_long(definition, "compatVersion", 2)?;
        if compat_version < 2 {
            return Err(refuse(format!(
                "compatVersion {compat_version} names its analyzed fields without the `full:` \
                 prefix, which is a second format rather than a variation on this one"
            )));
        }
        let Some(rule_node) = definition.child_node("indexRules")? else {
            return Err(refuse(
                "it carries no indexRules, so Oak synthesizes version-1 rules from the flat \
                 includePropertyNames"
                    .to_owned(),
            ));
        };
        let aggregates = aggregate::read_aggregates(definition)?;
        let mut rules = Vec::new();
        for (node_type_name, node) in rule_node.child_node_entries()? {
            rules.push(IndexingRule::read(
                definition_path,
                &node_type_name,
                &node,
                content_root,
                &aggregates,
                warnings,
            )?);
        }
        let fulltext_enabled = rules.iter().any(|rule| rule.fulltext_enabled);
        let codec = match strict_string(definition.property("codec")?.as_ref()) {
            // `Codec.forName` resolves the registered name, and the name
            // `OakCodec` registers under is the composition froe writes —
            // so a definition that names it outright is the one froe
            // writes for, whether or not it is fulltext-enabled. AEM's own
            // definitions carry it.
            Some(OAK_CODEC_NAME) => CodecVerdict::OakCodec,
            Some(name) => CodecVerdict::Named(name.to_owned()),
            None if fulltext_enabled => CodecVerdict::OakCodec,
            None => CodecVerdict::Lucene46,
        };
        if codec != CodecVerdict::OakCodec {
            return Err(refuse(format!(
                "its codec resolves to {}, and froe writes the oakCodec composition alone",
                match &codec {
                    CodecVerdict::Named(name) => name.clone(),
                    _ => "Lucene46".to_owned(),
                }
            )));
        }
        if let Some(pattern) = strict_string(definition.property("valueRegex")?.as_ref()) {
            // A definition-level regular expression, applied with
            // `Matcher.find` inside the per-property fulltext loop. froe
            // evaluates no value regular expression — plans 0006 and 0007
            // refuse `valuePattern` for the same reason — and a value
            // wrongly kept or dropped here is a term that should not be
            // in the index or one that should.
            return Err(refuse(format!(
                "it restricts values with the regular expression {pattern:?}, which froe does \
                 not evaluate"
            )));
        }
        let maximum_field_length = converting_long(definition.property("maxFieldLength")?.as_ref());
        if maximum_field_length == Some(0) {
            return Err(refuse(
                "maxFieldLength is zero, which leaves every analyzed field empty in 4.7.2 rather \
                 than being an error Oak reports"
                    .to_owned(),
            ));
        }
        if let Some(analyzers) = definition.child_node("analyzers")?
            && let Some((name, _)) = analyzers.child_node_entries()?.into_iter().next()
        {
            return Err(refuse(format!(
                "its analyzers node declares the child {name}, and froe reproduces no \
                 consumer-registered analyzer"
            )));
        }
        let index_original_term = match definition.child_node("analyzers")? {
            None => false,
            Some(analyzers) => optional_boolean(&analyzers, "indexOriginalTerm", false)?,
        };
        // `suggestion/@suggestAnalyzed`, falling back to the definition
        // root, which is where older definitions carry it.
        let suggest_analyzed = match definition.child_node("suggestion")? {
            Some(suggestion) if suggestion.property("suggestAnalyzed")?.is_some() => {
                optional_boolean(&suggestion, "suggestAnalyzed", false)?
            }
            _ => optional_boolean(definition, "suggestAnalyzed", false)?,
        };
        let rules = Self {
            rules,
            aggregates,
            codec,
            index_original_term,
            suggest_analyzed,
            evaluate_path_restrictions: optional_boolean(
                definition,
                "evaluatePathRestrictions",
                false,
            )?,
            maximum_field_length,
            has_tika_configuration: definition.child_node("tika")?.is_some(),
        };
        rules.refuse_conflicting_doc_value_types(definition_path)?;
        Ok(rules)
    }

    /// The rule that applies to a node, which is Oak's
    /// `getApplicableIndexingRule`: the **primary type first**, then the
    /// mixins in order, and within each the first rule registered under
    /// that name.
    ///
    /// # Errors
    ///
    /// Propagates the node-type reads.
    /// The aggregate a **matched** node's own type declares, which is
    /// `Aggregate.NodeInclude.getAggregate`:
    ///
    /// ```java
    /// Aggregate agg = aggMapper.getAggregate(ConfigUtil.getPrimaryTypeName(state));
    /// if (agg == null) {
    ///     for (String mixin : ConfigUtil.getMixinNames(state)) {
    ///         agg = aggMapper.getAggregate(mixin);
    ///         if (agg != null) break;
    ///     }
    /// }
    /// ```
    ///
    /// with `aggMapper` the definition's own `aggregates` map. The lookup
    /// is by **name, exactly** — no rule, and no type inheritance.
    ///
    /// # Errors
    ///
    /// Propagates the node-type reads.
    pub fn aggregate_of(&self, node: &NodeState<'_>) -> IndexResult<Option<&Aggregate>> {
        let primary = strict_name(node.property("jcr:primaryType")?.as_ref()).map(str::to_owned);
        let mixins = strict_names(node.property("jcr:mixinTypes")?.as_ref()).unwrap_or_default();
        for name in primary.into_iter().chain(mixins) {
            if let Some(aggregate) = self
                .aggregates
                .iter()
                .find(|aggregate| aggregate.node_type_name == name)
            {
                return Ok(Some(aggregate));
            }
        }
        Ok(None)
    }

    /// The rule that applies to a node, which is Oak's
    /// `getApplicableIndexingRule`: the **primary type first**, then the
    /// mixins in order, and within each the first rule registered under
    /// that name — a rule's own registration covering its subtypes when
    /// it is `inherited`.
    ///
    /// # Errors
    ///
    /// Propagates the node-type reads.
    pub fn applicable_rule(&self, node: &NodeState<'_>) -> IndexResult<Option<&IndexingRule>> {
        let primary = strict_name(node.property("jcr:primaryType")?.as_ref()).map(str::to_owned);
        let mixins = strict_names(node.property("jcr:mixinTypes")?.as_ref()).unwrap_or_default();
        for name in primary.into_iter().chain(mixins) {
            if let Some(rule) = self.rules.iter().find(|rule| rule.registers_under(&name)) {
                return Ok(Some(rule));
            }
        }
        Ok(None)
    }

    /// Two rules typing one property differently produce one field name
    /// with two doc-value types. Oak's writer refuses the change and its
    /// editor drops the later documents, which document depending on the
    /// traversal order; froe refuses the definition at load instead.
    ///
    /// **A recorded departure**: Oak indexes such a definition and loses
    /// documents, where froe indexes none of it.
    fn refuse_conflicting_doc_value_types(&self, definition_path: &str) -> IndexResult<()> {
        let mut seen: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
        for rule in &self.rules {
            for definition in rule.definitions() {
                if !definition.ordered {
                    continue;
                }
                let declared = definition.declared_type.as_deref().unwrap_or("UNDEFINED");
                if let Some((first_rule, first_type)) = seen.get(definition.name.as_str())
                    && *first_type != declared
                {
                    return Err(IndexError::UnsupportedDefinition {
                        definition_path: definition_path.to_owned(),
                        feature: format!(
                            "the ordered property {} is typed {first_type} by rule {first_rule} \
                             and {declared} by rule {}, which gives one doc-value field two types",
                            definition.name, rule.node_type_name
                        ),
                    });
                }
                seen.insert(
                    definition.name.as_str(),
                    (rule.node_type_name.as_str(), declared),
                );
            }
        }
        Ok(())
    }
}
