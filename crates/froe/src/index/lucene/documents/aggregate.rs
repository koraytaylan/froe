//! Node aggregates: the `aggregates/<type>/include*` children, and the
//! matcher that walks them.
//!
//! `docs/analysis/lucene-oak-documents.md` §4, from `oak-search`'s
//! `Aggregate.java`.
//!
//! An include declares a `path` of `/`-separated steps, each `*` or a
//! literal child name. **There is no double-star step**: a non-`*` element
//! is compared literally, `maxDepth` is the number of steps, and
//! `primaryType` is enforced on the last step alone, which is what
//! Jackrabbit 2 did before it.

use crate::content::node::NodeState;
use crate::index::{
    IndexResult, children_in_tree_order, converting_boolean, converting_long, strict_name,
    strict_string,
};

/// `Aggregate.MATCH_ALL`.
const MATCH_ALL: &str = "*";

/// `Aggregate.RECURSIVE_AGGREGATION_LIMIT_DEFAULT`.
pub const DEFAULT_REAGGREGATION_LIMIT: i64 = 5;

/// One `include*` child of an aggregate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Include {
    /// The child node's own name.
    pub node_name: String,
    /// `path`, split into steps.
    pub elements: Vec<String>,
    /// `primaryType`, enforced on the last step only.
    pub primary_type: Option<String>,
    /// `relativeNode`, which moves a matched node's text from `:fulltext`
    /// to `fullnode:<the include's path>`.
    pub relative_node: bool,
}

impl Include {
    /// `Aggregate.Include.maxDepth`.
    #[must_use]
    pub fn maximum_depth(&self) -> usize {
        self.elements.len()
    }

    /// Whether the step at `depth` matches a child of that name, with the
    /// primary-type test on the last step.
    ///
    /// # Errors
    ///
    /// Propagates a read of the child's `jcr:primaryType`.
    pub fn matches(&self, name: &str, node: &NodeState<'_>, depth: usize) -> IndexResult<bool> {
        let Some(element) = self.elements.get(depth) else {
            return Ok(false);
        };
        if element != MATCH_ALL && element != name {
            return Ok(false);
        }
        if depth + 1 == self.maximum_depth()
            && let Some(primary_type) = &self.primary_type
            && strict_name(node.property("jcr:primaryType")?.as_ref())
                != Some(primary_type.as_str())
        {
            return Ok(false);
        }
        Ok(true)
    }
}

/// One node type's aggregate, which is the set of its includes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Aggregate {
    /// The node type the aggregate is declared under.
    pub node_type_name: String,
    /// The includes, in stored order.
    pub includes: Vec<Include>,
    /// `reaggregateLimit`, which bounds how deep a re-aggregation walks.
    pub reaggregation_limit: i64,
}

impl Aggregate {
    /// `Aggregate.hasNodeAggregates`, which is half of what selects
    /// `oakCodec`.
    #[must_use]
    pub fn has_node_aggregates(&self) -> bool {
        !self.includes.is_empty()
    }

    /// Reads one `aggregates/<type>` child.
    ///
    /// # Errors
    ///
    /// Propagates a read of the aggregate's children.
    pub fn read(node_type_name: &str, node: &NodeState<'_>) -> IndexResult<Self> {
        let mut includes = Vec::new();
        for (name, child) in children_in_tree_order(node)? {
            if !name.starts_with("include") {
                continue;
            }
            let path = strict_string(child.property("path")?.as_ref())
                .map(str::to_owned)
                .unwrap_or_default();
            includes.push(Include {
                node_name: name,
                elements: path
                    .split('/')
                    .filter(|element| !element.is_empty())
                    .map(str::to_owned)
                    .collect(),
                primary_type: strict_string(child.property("primaryType")?.as_ref())
                    .map(str::to_owned),
                relative_node: converting_boolean(child.property("relativeNode")?.as_ref()),
            });
        }
        Ok(Self {
            node_type_name: node_type_name.to_owned(),
            includes,
            reaggregation_limit: converting_long(node.property("reaggregateLimit")?.as_ref())
                .unwrap_or(DEFAULT_REAGGREGATION_LIMIT),
        })
    }

    /// Opens a match over the includes, which is `Aggregate.createMatcher`.
    #[must_use]
    pub fn matcher(&self) -> Matcher<'_> {
        Matcher {
            aggregate: self,
            live: (0..self.includes.len()).map(|at| (at, 0usize)).collect(),
        }
    }
}

/// The state machine `Aggregate` walks a subtree with: which includes are
/// still live, and how deep each of them has matched.
#[derive(Clone, Debug)]
pub struct Matcher<'aggregate> {
    aggregate: &'aggregate Aggregate,
    /// One entry per live include: its index, and the depth it has
    /// matched to.
    live: Vec<(usize, usize)>,
}

/// What a step of the matcher found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Match<'aggregate> {
    /// No include can still match; the walk stops here.
    Stop,
    /// Some include may still match deeper, but none ends here.
    Continue,
    /// At least one include ends on this node, which therefore
    /// contributes its properties. The includes that ended are named,
    /// because a `relativeNode` one changes the field their text lands in.
    Aggregate(Vec<&'aggregate Include>),
}

impl<'aggregate> Matcher<'aggregate> {
    /// Steps into the child `name`.
    ///
    /// # Errors
    ///
    /// Propagates the primary-type read the last step performs.
    pub fn step(&self, name: &str, node: &NodeState<'_>) -> IndexResult<(Self, Match<'aggregate>)> {
        let mut live = Vec::new();
        let mut ended = Vec::new();
        for (at, depth) in &self.live {
            let include = &self.aggregate.includes[*at];
            if !include.matches(name, node, *depth)? {
                continue;
            }
            if depth + 1 == include.maximum_depth() {
                ended.push(include);
            } else {
                live.push((*at, depth + 1));
            }
        }
        let outcome = if !ended.is_empty() {
            Match::Aggregate(ended)
        } else if live.is_empty() {
            Match::Stop
        } else {
            Match::Continue
        };
        Ok((
            Self {
                aggregate: self.aggregate,
                live,
            },
            outcome,
        ))
    }
}

/// Reads every `aggregates/<type>` child of a definition.
///
/// # Errors
///
/// Propagates the child reads.
pub fn read_aggregates(definition: &NodeState<'_>) -> IndexResult<Vec<Aggregate>> {
    let Some(aggregates) = definition.child_node("aggregates")? else {
        return Ok(Vec::new());
    };
    let mut produced = Vec::new();
    for (name, child) in children_in_tree_order(&aggregates)? {
        produced.push(Aggregate::read(&name, &child)?);
    }
    Ok(produced)
}

/// The aggregate declared for a node type, or an empty one.
#[must_use]
pub fn for_node_type(aggregates: &[Aggregate], node_type_name: &str) -> Aggregate {
    aggregates
        .iter()
        .find(|aggregate| aggregate.node_type_name == node_type_name)
        .cloned()
        .unwrap_or_else(|| Aggregate {
            node_type_name: node_type_name.to_owned(),
            includes: Vec::new(),
            reaggregation_limit: DEFAULT_REAGGREGATION_LIMIT,
        })
}
