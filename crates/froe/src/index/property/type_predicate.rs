//! Oak's `TypePredicate`: the `declaringNodeTypes` restriction, resolved
//! against the node types stored under `/jcr:system/jcr:nodeTypes`.
//!
//! `docs/analysis/index-property-storage.md` §6.1 quotes `addNodeType` and
//! `test`. The asymmetry a rebuild must reproduce is that a **mixin**
//! contributes its own name and its `rep:mixinSubtypes` to the mixin set,
//! while a **non-mixin** contributes its own name to the primary set and
//! nothing to the mixin set — so a declared primary type never matches a node
//! by that node's mixins. `rep:primarySubtypes` of the declared type joins
//! the primary set either way.
//!
//! Oak builds the two sets lazily, on the first `test`. froe builds them in
//! the constructor, which is already fallible; the difference is not
//! observable, because a predicate that is never tested is never built in
//! either implementation and the sets are bounded by the declared names.

use std::collections::BTreeSet;

use crate::content::node::NodeState;
use crate::index::{IndexResult, strict_boolean, strict_name, strict_names};

/// The node-type restriction on a property-index definition.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TypePredicate {
    primary_types: BTreeSet<String>,
    mixin_types: BTreeSet<String>,
}

impl TypePredicate {
    /// Resolves `declared_names` against `/jcr:system/jcr:nodeTypes`.
    ///
    /// `declared_names` is what a **strict** `NAMES` read of
    /// `declaringNodeTypes` yielded, which is the same read Oak performs; a
    /// definition storing the property as anything else yields an empty list
    /// here and therefore a predicate matching nothing, exactly as Oak's
    /// does.
    pub fn new(content_root: &NodeState<'_>, declared_names: &[String]) -> IndexResult<Self> {
        let node_types = match content_root.child_node("jcr:system")? {
            None => None,
            Some(system) => system.child_node("jcr:nodeTypes")?,
        };
        let mut primary_types = BTreeSet::new();
        let mut mixin_types = BTreeSet::new();
        for name in declared_names {
            let node_type = match &node_types {
                None => None,
                Some(types) => types.child_node(name)?,
            };
            // `rep:primarySubtypes` joins the primary set for every declared
            // name, mixin or not — the loop in `addNodeType` runs before the
            // `jcr:isMixin` branch.
            if let Some(node_type) = &node_type {
                primary_types.extend(
                    strict_names(node_type.property("rep:primarySubtypes")?.as_ref())
                        .unwrap_or_default(),
                );
            }
            let is_mixin = match &node_type {
                None => false,
                Some(node_type) => strict_boolean(node_type.property("jcr:isMixin")?.as_ref()),
            };
            if is_mixin {
                mixin_types.insert(name.clone());
                if let Some(node_type) = &node_type {
                    mixin_types.extend(
                        strict_names(node_type.property("rep:mixinSubtypes")?.as_ref())
                            .unwrap_or_default(),
                    );
                }
            } else {
                // "No need to check whether the type actually exists, as if
                // it doesn't there should in any case be no matching
                // content" — `TypePredicate.addNodeType`.
                primary_types.insert(name.clone());
            }
        }
        Ok(Self {
            primary_types,
            mixin_types,
        })
    }

    /// Whether the predicate matches nothing, which is the state a
    /// definition with no usable `declaringNodeTypes` produces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.primary_types.is_empty() && self.mixin_types.is_empty()
    }

    /// Whether `node` is an instance of one of the declared types.
    ///
    /// Both reads are **strict**: a `jcr:primaryType` that is not a single
    /// `NAME`, or a `jcr:mixinTypes` that is not `NAMES`, matches nothing.
    pub fn test(&self, node: &NodeState<'_>) -> IndexResult<bool> {
        if let Some(primary) = strict_name(node.property("jcr:primaryType")?.as_ref())
            && self.primary_types.contains(primary)
        {
            return Ok(true);
        }
        let mixins = strict_names(node.property("jcr:mixinTypes")?.as_ref()).unwrap_or_default();
        Ok(mixins.iter().any(|mixin| self.mixin_types.contains(mixin)))
    }
}
