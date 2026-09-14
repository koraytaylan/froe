//! Sorted `(key, path)` entries to the `:index` subtree Oak's own strategies
//! would leave behind.
//!
//! Two shapes, chosen by one strict `BOOLEAN` read of `unique` and by nothing
//! else — `docs/analysis/index-property-storage.md` §6:
//!
//! * **The mirror** (`ContentMirrorStoreStrategy`): under `:index`, a node
//!   per key, and below it one node per element of each indexed path, with
//!   `match = true` on the node the indexed path addresses.
//! * **The unique entry** (`UniqueEntryStoreStrategy`): under `:index`, one
//!   node per key carrying `entry` as a `String[]` of absolute paths. A key
//!   seen twice is a refusal, because Oak would refuse the commit.
//!
//! # Why the mirror builder streams
//!
//! Sorting by `(key, path elements)` gives exactly the order a depth-first
//! construction needs: when the sequence leaves a subtree, that subtree is
//! complete and its node can be written. So the builder holds, per ancestor
//! on the current path, only the completed `(name, record)` pairs of its
//! children — the widest fan-out on one root-to-leaf path, which is the
//! single key-proportional term the safety case admits, because the trie root
//! `:index` has one child per distinct key.
//!
//! # What is deliberately not written
//!
//! **No `:count_*` properties.** Every Oak insert adjusts the approximate
//! counter, so an index Oak rebuilt carries some — but a *fresh* Oak index
//! has none until the random generator happens to add one, and their absence
//! is a state Oak reads without complaint: the approximate counter answers
//! `-1` for a count that is not there. Writing a plausible-looking one would
//! make froe's output differ from every Oak rebuild in a way no test could
//! pin. This is invariant 1 of `index-property-storage.md` §13.

use crate::PropertyType;
use crate::error::{Error, Result};
use crate::index::property::unique::ENTRY_PROPERTY_NAME;
use crate::segment::record::RecordIdentifier;
use crate::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};

/// The property the mirror sets on every node an indexed path addresses.
const MATCH_PROPERTY_NAME: &str = "match";

/// What a build wrote, for the safety case's cost statement and for the
/// resident-state assertion its memory claim rests on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct BuilderAccounting {
    /// Node records written, `:index` included.
    pub nodes_written: u64,
    /// The most completed children held at once, across every ancestor on
    /// the current path. This is the number the memory claim is about, and
    /// it is asserted against each fixture's widest fan-out rather than
    /// against the process's resident set size.
    pub peak_resident_children: usize,
}

/// One level of the trie under construction: a node whose children are being
/// accumulated.
struct OpenLevel {
    /// The element name this level sits under.
    name: String,
    /// Completed children, in the order the sorted sequence produced them —
    /// which is byte order, since the entries arrive sorted.
    children: Vec<(String, RecordIdentifier)>,
    /// Whether an indexed path addresses this node itself.
    matched: bool,
}

/// Builds a `ContentMirrorStoreStrategy` `:index` subtree from sorted
/// entries.
pub struct MirrorBuilder<'writer, Sink: SegmentSink> {
    writer: &'writer mut RecordWriter<Sink>,
    /// The `:index` node's accumulating children, one per key.
    ///
    /// Kept out of the stack below, so no code path can pop it — which is
    /// what lets `finish` be total rather than documented as panicking.
    root_children: Vec<(String, RecordIdentifier)>,
    /// The open path below `:index`: the key node, then one level per path
    /// element.
    levels: Vec<OpenLevel>,
    accounting: BuilderAccounting,
}

impl<'writer, Sink: SegmentSink> MirrorBuilder<'writer, Sink> {
    /// An empty builder writing through `writer`.
    pub fn new(writer: &'writer mut RecordWriter<Sink>) -> Self {
        Self {
            writer,
            root_children: Vec::new(),
            levels: Vec::new(),
            accounting: BuilderAccounting::default(),
        }
    }

    /// Adds the entry for `path` under `key`.
    ///
    /// Entries must arrive in `(key, path elements)` order; the builder
    /// relies on it and does not check, because the sort is what establishes
    /// it and a check here would be a second implementation of the order.
    pub fn push(&mut self, key: &str, path: &str) -> Result<()> {
        let elements: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();

        // Level 0 is always the key node; a different key closes everything.
        if self.levels.first().map(|level| level.name.as_str()) != Some(key) {
            self.close_to_depth(0)?;
            self.levels.push(OpenLevel {
                name: key.to_owned(),
                children: Vec::new(),
                matched: false,
            });
        }

        let shared = self.shared_prefix(&elements);
        self.close_to_depth(1 + shared)?;
        for element in &elements[shared..] {
            self.levels.push(OpenLevel {
                name: (*element).to_owned(),
                children: Vec::new(),
                matched: false,
            });
        }

        // `match` is set unconditionally after descending every element, so
        // an interior node a shorter indexed path also addresses carries it
        // too — the case Oak's `nodetype` index hits for nested
        // `rep:AuthorizableFolder` nodes. The root path descends no element
        // and therefore addresses the key node itself.
        if let Some(addressed) = self.levels.last_mut() {
            addressed.matched = true;
        }
        Ok(())
    }

    /// Finishes the subtree and returns the `:index` node's record.
    ///
    /// An empty sequence still yields an `:index` node: Oak's uniqueness
    /// check creates it unconditionally, so a property definition always has
    /// one. The *reference* collector decides separately whether to build at
    /// all, because Oak creates `:references` and `:weakreferences` only on
    /// the first insert.
    pub fn finish(mut self) -> Result<(RecordIdentifier, BuilderAccounting)> {
        self.close_to_depth(0)?;
        let children = std::mem::take(&mut self.root_children);
        let record = self
            .writer
            .write_node(None, &[], &child_nodes(&children), &[])?;
        self.accounting.nodes_written += 1;
        Ok((record, self.accounting))
    }

    /// How many of the open path's elements `elements` shares.
    fn shared_prefix(&self, elements: &[&str]) -> usize {
        // `levels[0]` is the key node, so the open elements start at 1.
        let mut shared = 0;
        while shared < elements.len() {
            let Some(level) = self.levels.get(1 + shared) else {
                break;
            };
            if level.name != elements[shared] {
                break;
            }
            shared += 1;
        }
        shared
    }

    /// Writes and pops every open level deeper than `depth`, attaching each
    /// to its parent — the `:index` root when the stack empties.
    fn close_to_depth(&mut self, depth: usize) -> Result<()> {
        while self.levels.len() > depth {
            let Some(level) = self.levels.pop() else {
                break;
            };
            let record = self.write_level(&level)?;
            let parent = match self.levels.last_mut() {
                Some(parent) => &mut parent.children,
                None => &mut self.root_children,
            };
            parent.push((level.name, record));
            // Sampled here, not in `push`: residency grows when a completed
            // child is attached to its parent, which is what the memory
            // claim is about. Sampling only on entry arrival misses the last
            // child of every node, including the widest one.
            self.record_residency();
        }
        Ok(())
    }

    fn write_level(&mut self, level: &OpenLevel) -> Result<RecordIdentifier> {
        let properties = if level.matched {
            let value = self.writer.write_string("true")?;
            vec![PropertyToWrite {
                name: MATCH_PROPERTY_NAME.to_owned(),
                property_type: PropertyType::Boolean,
                values: PropertyValuesToWrite::Single(value),
            }]
        } else {
            Vec::new()
        };
        let record = self
            .writer
            // No primary type: Oak's own strategies set none.
            .write_node(None, &[], &child_nodes(&level.children), &properties)?;
        self.accounting.nodes_written += 1;
        Ok(record)
    }

    fn record_residency(&mut self) {
        let resident: usize = self.root_children.len()
            + self
                .levels
                .iter()
                .map(|level| level.children.len())
                .sum::<usize>();
        self.accounting.peak_resident_children =
            self.accounting.peak_resident_children.max(resident);
    }
}

/// Builds a `UniqueEntryStoreStrategy` `:index` subtree from sorted entries.
///
/// One node per key, carrying `entry` as a `String[]` of absolute paths. A
/// single node whose two indexed properties share a value produces one entry,
/// not a duplicate; two *distinct* nodes sharing a key is the refusal.
pub struct UniqueBuilder<'writer, Sink: SegmentSink> {
    writer: &'writer mut RecordWriter<Sink>,
    children: Vec<(String, RecordIdentifier)>,
    open: Option<(String, Vec<String>)>,
    accounting: BuilderAccounting,
}

impl<'writer, Sink: SegmentSink> UniqueBuilder<'writer, Sink> {
    /// An empty builder writing through `writer`.
    pub fn new(writer: &'writer mut RecordWriter<Sink>) -> Self {
        Self {
            writer,
            children: Vec::new(),
            open: None,
            accounting: BuilderAccounting::default(),
        }
    }

    /// Adds the entry for `path` under `key`, refusing a second distinct
    /// path.
    pub fn push(&mut self, key: &str, path: &str) -> Result<()> {
        match &mut self.open {
            Some((open_key, paths)) if open_key == key => {
                if paths.iter().all(|existing| existing != path) {
                    // Refused before the path is recorded, so the error's
                    // `paths` names what was found rather than what the
                    // refusal itself added.
                    let mut found = paths.clone();
                    found.push(path.to_owned());
                    return Err(Error::DuplicateUniqueKey {
                        key: key.to_owned(),
                        paths: found,
                    });
                }
                // A repeated `(key, path)` — a multi-valued property whose
                // values encode to one key — is one entry, not a duplicate.
            }
            _ => {
                self.close_open()?;
                self.open = Some((key.to_owned(), vec![path.to_owned()]));
            }
        }
        self.accounting.peak_resident_children = self
            .accounting
            .peak_resident_children
            .max(self.children.len());
        Ok(())
    }

    /// Finishes the subtree and returns the `:index` node's record.
    pub fn finish(mut self) -> Result<(RecordIdentifier, BuilderAccounting)> {
        self.close_open()?;
        let record = self
            .writer
            .write_node(None, &[], &child_nodes(&self.children), &[])?;
        self.accounting.nodes_written += 1;
        Ok((record, self.accounting))
    }

    fn close_open(&mut self) -> Result<()> {
        let Some((key, paths)) = self.open.take() else {
            return Ok(());
        };
        let values: Vec<RecordIdentifier> = paths
            .iter()
            .map(|path| self.writer.write_string(path))
            .collect::<Result<Vec<_>>>()?;
        let record = self.writer.write_node(
            None,
            &[],
            &ChildNodesToWrite::Zero,
            &[PropertyToWrite {
                name: ENTRY_PROPERTY_NAME.to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Multiple(values),
            }],
        )?;
        self.accounting.nodes_written += 1;
        self.children.push((key, record));
        Ok(())
    }
}

/// The child-node shape for `children`.
///
/// `Zero` rather than `Many(vec![])`: an empty `Many` writes child arity 2
/// plus an empty map record, a shape Oak's own segment writer never produces,
/// and the difference is invisible in a content digest but visible in the
/// template.
fn child_nodes(children: &[(String, RecordIdentifier)]) -> ChildNodesToWrite {
    match children {
        [] => ChildNodesToWrite::Zero,
        [(name, node)] => ChildNodesToWrite::One {
            name: name.clone(),
            node: *node,
        },
        many => ChildNodesToWrite::Many(many.to_vec()),
    }
}
