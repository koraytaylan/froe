//! The aggregate walk: the nodes and the relative properties a document
//! takes from **outside** the node it is made for.
//!
//! `docs/analysis/lucene-oak-documents.md` §4, from `oak-search`'s
//! `Aggregate` and `FulltextDocumentMaker.indexAggregates`.
//!
//! Oak combines two kinds of include into one list — the relative property
//! definitions first, then the `aggregates/<type>/include*` children — and
//! walks it per child. A matched node contributes its properties, and a
//! matched node whose own rule declares an aggregate is entered in turn,
//! as deep as `reaggregateLimit` allows.

use super::{
    DocumentMaker, DocumentState, FULLTEXT_FIELD, IndexResult, IndexingRule, Match, Matcher,
    NodeState, PropertyInclude, PropertyState, PropertyType, RELATIVE_NODE_PREFIX, values_of,
};

impl DocumentMaker<'_> {
    /// §4: the aggregates, whose values arrive in each aggregated node's
    /// property order — and, in front of them, the relative property
    /// definitions Oak turns into property includes of the same walk.
    pub(super) fn index_aggregates(
        &self,
        node: &NodeState<'_>,
        path: &str,
        rule: &IndexingRule,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        let property_includes = rule.property_includes();
        // Before the node is read for anything: a definition with neither
        // include kind is the common one, and it owes this walk nothing.
        if !rule.aggregate.has_node_aggregates() && property_includes.is_empty() {
            return Ok(());
        }
        let walk = AggregateWalk {
            rule,
            // Oak hands `indexProperty` the **document's own** node state
            // for a property include, however deep the property lives, so
            // the binary gate is the root node's `jcr:mimeType`.
            root_has_mime_type: node.property("jcr:mimeType")?.is_some(),
            property_includes,
        };
        let level = AggregateLevel {
            nodes: rule.aggregate.matcher(),
            properties: PropertyIncludeMatcher::new(&walk.property_includes),
        };
        self.walk_aggregate(&walk, &level, node, path, state)
    }

    /// One level of the aggregate walk.
    ///
    /// The two include kinds are one list in Oak, the property includes in
    /// front of the node ones, and `collectAggregates` evaluates that list
    /// **per child** — so a child that ends both kinds contributes its
    /// property-include fields first.
    fn walk_aggregate(
        &self,
        walk: &AggregateWalk<'_>,
        level: &AggregateLevel<'_>,
        node: &NodeState<'_>,
        path: &str,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        for (name, child) in node.child_node_entries()? {
            let (next_properties, ended_properties) = level.properties.step(&name);
            let (next_nodes, outcome) = level.nodes.step(&name, &child)?;
            for include in ended_properties {
                self.index_property_include(&child, walk, include, state)?;
            }
            let child_path = format!("{}/{name}", path.trim_end_matches('/'));
            match outcome {
                Match::Stop if next_properties.is_exhausted() => continue,
                Match::Stop | Match::Continue => {}
                Match::Aggregate(includes) => {
                    for include in includes {
                        let relative = include.relative_node.then(|| {
                            format!("{RELATIVE_NODE_PREFIX}{}", include.elements.join("/"))
                        });
                        let names: Vec<&str> = std::iter::once(FULLTEXT_FIELD)
                            .chain(relative.as_deref())
                            .collect();
                        self.aggregate_node(&child, walk.writing_into(&names), state)?;
                    }
                }
            }
            let next = AggregateLevel {
                nodes: next_nodes,
                properties: next_properties,
            };
            self.walk_aggregate(walk, &next, &child, &child_path, state)?;
        }
        Ok(())
    }

    /// One property include's fields, under the **relative path** as the
    /// field name:
    ///
    /// ```java
    /// public void onResult(PropertyIncludeResult result) {
    ///     if (result.pd.ordered) addTypedOrderedFields(fields, result.propertyState, result.propertyPath, result.pd);
    ///     indexProperty(path, fields, state, result.propertyState, result.propertyPath, result.pd);
    /// }
    /// ```
    ///
    /// `propertyPath` is the definition's own parent path joined with the
    /// property's **own** name, which differ for a regular-expression
    /// definition.
    fn index_property_include(
        &self,
        node: &NodeState<'_>,
        walk: &AggregateWalk<'_>,
        include: PropertyInclude<'_>,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        let definition = include.definition;
        let parent = definition.ancestors.join("/");
        for mut property in included_properties(node, include)? {
            property.name = format!("{parent}/{}", property.name);
            if definition.ordered {
                self.index_ordered(&property, definition, state)?;
            }
            self.index_property(
                &property,
                definition,
                walk.rule,
                walk.root_has_mime_type,
                state,
            )?;
        }
        Ok(())
    }

    /// One aggregated node's properties, each a `:fulltext` value — and,
    /// for a `relativeNode` include, a `fullnode:<include path>` one
    /// **beside** it rather than instead of it.
    ///
    /// Oak's own rebuild is what says "beside": over a definition whose
    /// aggregate names one child twice, once plainly and once relatively,
    /// the page's `:fulltext` carries that child's values **twice** and
    /// `fullnode:jcr:content` carries them once — so the relative include
    /// contributed to both fields.
    ///
    /// The property definition consulted here is the one the rule covering
    /// **the aggregated node** holds, not the document's own rule:
    /// `indexAggregatedNode` resolves `getApplicableIndexingRule(result
    /// .nodeState)` and asks that rule whether the property is excluded
    /// from aggregation.
    fn aggregate_node(
        &self,
        node: &NodeState<'_>,
        into: AggregatedInto<'_>,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        let names = into.names;
        let covering = self.rules.applicable_rule(node)?;
        let has_mime_type = node.property("jcr:mimeType")?.is_some();
        for property in node.properties()? {
            if property.name.starts_with(':') {
                continue;
            }
            let definition = covering.and_then(|rule| rule.config_of(&property.name));
            if definition.is_some_and(|definition| definition.exclude_from_aggregation) {
                continue;
            }
            if property.property_type == PropertyType::Binary {
                self.index_binary(&property, has_mime_type, names, state);
                continue;
            }
            for value in values_of(&property) {
                let Some(text) = value.as_text() else {
                    continue;
                };
                for name in names {
                    let position = state.fields.len();
                    self.index_fulltext(name, &text, None, state);
                    if let Some(definition) = definition
                        && let Some(field) = state.fields.get_mut(position)
                    {
                        // `if (pd != null) field.setBoost(pd.boost)`.
                        field.boost = definition.boost;
                    }
                }
            }
        }
        self.reaggregate(node, covering, into, state)
    }

    /// The **re-aggregation**: an aggregated node whose own rule declares
    /// an aggregate contributes that aggregate's nodes too, into the same
    /// fields, after its own properties.
    ///
    /// `reaggregateLimit` — five by default — is how many levels deep that
    /// goes, and the one compared is the **root** aggregate's, however
    /// deep the walk is:
    ///
    /// ```java
    /// Aggregate nextAgg = currentInclude.getAggregate(matchedNodeState);
    /// if (nextAgg != null && aggregateStack.size() < rootState.rootAggregate.reAggregationLimit)
    /// ```
    ///
    /// Oak's rebuild of the interop fixture pins the rest: a `meta` child
    /// of type `sling:Folder` whose rule declares `include0 = inner` puts
    /// that grandchild's values in the page's `:fulltext` **and** in the
    /// `fullnode:meta` of the relative include that reached `meta`.
    fn reaggregate(
        &self,
        node: &NodeState<'_>,
        covering: Option<&IndexingRule>,
        into: AggregatedInto<'_>,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        let Some(rule) = covering else {
            return Ok(());
        };
        if !rule.aggregate.has_node_aggregates() || into.depth >= into.limit {
            return Ok(());
        }
        self.walk_reaggregate(node, &rule.aggregate.matcher(), into.deeper(), state)
    }

    /// One level of a re-aggregation's own walk, which carries the field
    /// names of the include that reached the node it started from.
    fn walk_reaggregate(
        &self,
        node: &NodeState<'_>,
        matcher: &Matcher<'_>,
        into: AggregatedInto<'_>,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        for (name, child) in node.child_node_entries()? {
            let (next, outcome) = matcher.step(&name, &child)?;
            match outcome {
                Match::Stop => continue,
                Match::Continue => {}
                Match::Aggregate(includes) => {
                    // Once per include that ended here, as at the top
                    // level — a node two includes name is aggregated
                    // twice — and into the **outer** include's fields,
                    // because the field name a re-aggregated value takes
                    // is the one the walk entered with.
                    for _ended in includes {
                        self.aggregate_node(&child, into, state)?;
                    }
                }
            }
            self.walk_reaggregate(&child, &next, into, state)?;
        }
        Ok(())
    }
}

/// What a matched node's values are written into: the fields, and how
/// deep a re-aggregation from it may still go.
#[derive(Clone, Copy)]
struct AggregatedInto<'walk> {
    /// `:fulltext`, and the `fullnode:<path>` of the include that reached
    /// the node this descends from.
    names: &'walk [&'walk str],
    /// The **root** aggregate's `reAggregationLimit`, which is the one Oak
    /// compares its stack against however deep the stack is.
    limit: usize,
    /// How many aggregates the walk has entered, which is that stack's
    /// size.
    depth: usize,
}

impl AggregatedInto<'_> {
    /// The same fields, one aggregate deeper.
    const fn deeper(self) -> Self {
        Self {
            depth: self.depth + 1,
            ..self
        }
    }
}

/// What every level of the aggregate walk carries down.
struct AggregateWalk<'rule> {
    /// The rule the document is being made under.
    rule: &'rule IndexingRule,
    /// Whether the **document's own** node declares `jcr:mimeType`, which
    /// is the gate a property include's binary is gated by however deep
    /// the property lives: Oak passes the root state to `indexProperty`.
    root_has_mime_type: bool,
    /// The relative definitions, in Oak's own combined order.
    property_includes: Vec<PropertyInclude<'rule>>,
}

impl AggregateWalk<'_> {
    /// The fields an include of this walk writes into, at the top of the
    /// aggregate stack.
    fn writing_into<'names>(&self, names: &'names [&'names str]) -> AggregatedInto<'names> {
        AggregatedInto {
            names,
            limit: usize::try_from(self.rule.aggregate.reaggregation_limit).unwrap_or(0),
            depth: 0,
        }
    }
}

/// One level of the walk: where each include kind has matched to.
struct AggregateLevel<'rule> {
    nodes: Matcher<'rule>,
    properties: PropertyIncludeMatcher<'rule>,
}

/// The property includes' half of the walk, which is the same state
/// machine over each definition's **ancestor** elements.
struct PropertyIncludeMatcher<'rule> {
    includes: &'rule [PropertyInclude<'rule>],
    /// One entry per live include: its index, and the depth it has
    /// matched to.
    live: Vec<(usize, usize)>,
}

impl<'rule> PropertyIncludeMatcher<'rule> {
    fn new(includes: &'rule [PropertyInclude<'rule>]) -> Self {
        let live = includes
            .iter()
            .enumerate()
            .filter(|(_, include)| !include.definition.ancestors.is_empty())
            .map(|(at, _)| (at, 0usize))
            .collect();
        Self { includes, live }
    }

    /// Steps into the child `name`, returning the level below and the
    /// includes whose ancestor path ends on that child.
    fn step(&self, name: &str) -> (Self, Vec<PropertyInclude<'rule>>) {
        let mut live = Vec::new();
        let mut ended = Vec::new();
        for (at, depth) in &self.live {
            let ancestors = &self.includes[*at].definition.ancestors;
            let Some(element) = ancestors.get(*depth) else {
                continue;
            };
            if element != MATCH_ALL_STEP && element != name {
                continue;
            }
            if depth + 1 == ancestors.len() {
                ended.push(self.includes[*at]);
            } else {
                live.push((*at, depth + 1));
            }
        }
        (
            Self {
                includes: self.includes,
                live,
            },
            ended,
        )
    }

    /// Whether no include can still match below this level, which is the
    /// property half of `Match::Stop`.
    const fn is_exhausted(&self) -> bool {
        self.live.is_empty()
    }
}

/// `Aggregate.MATCH_ALL`, which an ancestor element carries as well.
const MATCH_ALL_STEP: &str = "*";

/// The properties one property include contributes at the node its
/// ancestor path ended on.
///
/// ```java
/// if (pattern != null) {
///     for (PropertyState ps : nodeState.getProperties()) {
///         if (pattern.matcher(ps.getName()).matches()) results.onResult(…);
///     }
/// } else {
///     PropertyState ps = nodeState.getProperty(propertyName);
///     if (ps != null) results.onResult(…);
/// }
/// ```
///
/// The names arrive as the node state yields them, and the name the
/// pattern is matched against is the property's own — which is why the
/// field name is rebuilt from the definition's parent path rather than
/// taken from the definition's name.
///
/// **A hidden name contributes nothing here**, where the per-property
/// pass of §3.3 lets one through to the patterns as bug compatibility.
/// Oak's own rebuild of the interop fixture is what says so: a
/// `jcr:content/.*` definition over a node carrying `:childOrder` wrote
/// `full:jcr:content/jcr:primaryType` and no `:childOrder` field of
/// either kind.
fn included_properties(
    node: &NodeState<'_>,
    include: PropertyInclude<'_>,
) -> IndexResult<Vec<PropertyState>> {
    let definition = include.definition;
    let name = definition
        .name
        .rsplit('/')
        .next()
        .unwrap_or(&definition.name);
    let Some(pattern) = include.pattern else {
        if name.starts_with(':') {
            return Ok(Vec::new());
        }
        return Ok(node.property(name)?.into_iter().collect());
    };
    let parent = definition.ancestors.join("/");
    Ok(node
        .properties()?
        .into_iter()
        .filter(|property| !property.name.starts_with(':'))
        .filter(|property| pattern.matches(&format!("{parent}/{}", property.name)))
        .collect())
}
