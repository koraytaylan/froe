//! The transducer `.tip` stores, one per field.
//!
//! `docs/analysis/lucene-4-7-codec.md` §7.6, from `util/fst/FST.java`,
//! `util/fst/Builder.java`, `util/fst/NodeHash.java` and
//! `util/fst/ByteSequenceOutputs.java`.
//!
//! A map from sorted byte-string keys to byte-string outputs, serialized in
//! the form Lucene reads back. The terms writer builds one with exactly
//! these settings (`BlockTreeTermsWriter`, its `indexBuilder`):
//!
//! ```text
//! new Builder<BytesRef>(FST.INPUT_TYPE.BYTE1,
//!                       0, 0, true, false, Integer.MAX_VALUE,
//!                       outputs, null, false,
//!                       PackedInts.COMPACT, true, 15);
//! ```
//!
//! — single-byte labels, **no pruning** (both minimum suffix counts zero),
//! **suffix sharing on**, **non-singleton node sharing off**, and
//! **unpacked**. The zero pruning counts are what make this implementable in
//! a fraction of Lucene's code: `freezeTail`'s prune and defer branches are
//! all dead, so every frozen node is compiled immediately and no node is
//! ever dropped.
//!
//! # Two deliberate differences from Lucene's own output
//!
//! **Linear arcs only.** Lucene passes `allowArrayArcs = true` and so emits
//! the fixed-array form — an `ARCS_AS_FIXED_ARRAY` flags byte, a `VInt` arc
//! count and a `VInt` bytes-per-arc — for a node with at least five arcs at
//! depth three or less, or ten deeper. The reader dispatches on that flag
//! **per node**, so a transducer of linear arcs is one Lucene reads
//! correctly and seeks through more slowly. §10.3 records it.
//!
//! **Unpacked** is not a difference: Lucene's own terms writer passes
//! `doPackFST = false`.
//!
//! # The byte store reads backwards
//!
//! Each node's bytes are written forward and then **reversed in place**, and
//! the node's address is the index of its last byte. A reader seeks to that
//! address and reads *down*. Nothing about this is visible in the file's
//! outer structure, and a writer that omits the reversal produces a store
//! whose every node is unreadable while the file's header, counts and length
//! all check out.

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::index::lucene::codec::data_output::CodecOutput;

/// `FST.FILE_FORMAT_NAME`.
const FILE_FORMAT_NAME: &str = "FST";

/// `FST.VERSION_CURRENT` (`VERSION_VINT_TARGET`).
const VERSION_CURRENT: i32 = 4;

/// `FST.BIT_FINAL_ARC`.
const BIT_FINAL_ARC: u8 = 1 << 0;
/// `FST.BIT_LAST_ARC`.
const BIT_LAST_ARC: u8 = 1 << 1;
/// `FST.BIT_TARGET_NEXT`.
const BIT_TARGET_NEXT: u8 = 1 << 2;
/// `FST.BIT_STOP_NODE`.
const BIT_STOP_NODE: u8 = 1 << 3;
/// `FST.BIT_ARC_HAS_OUTPUT`.
const BIT_ARC_HAS_OUTPUT: u8 = 1 << 4;
/// `FST.BIT_ARC_HAS_FINAL_OUTPUT`.
const BIT_ARC_HAS_FINAL_OUTPUT: u8 = 1 << 5;

/// `FST.FINAL_END_NODE`.
const FINAL_END_NODE: i64 = -1;
/// `FST.NON_FINAL_END_NODE`.
const NON_FINAL_END_NODE: i64 = 0;

/// An arc under construction.
#[derive(Clone)]
struct BuilderArc {
    label: u8,
    /// The compiled target, once the node below has been frozen.
    target: i64,
    output: Vec<u8>,
    next_final_output: Vec<u8>,
    is_final: bool,
}

/// A node on the frontier, one per depth of the last key added.
#[derive(Clone, Default)]
struct UnCompiledNode {
    arcs: Vec<BuilderArc>,
    is_final: bool,
    /// The output that belongs to *ending* here.
    output: Vec<u8>,
}

/// Builds a transducer from sorted keys.
///
/// Keys must arrive in ascending byte order; a key that does not is a typed
/// refusal rather than a silently wrong automaton. Lucene asserts the same
/// thing, and its assertion is disabled in a production JVM.
pub struct FstBuilder {
    frontier: Vec<UnCompiledNode>,
    last_input: Vec<u8>,
    /// The node store. Each node's bytes are reversed in place as it is
    /// frozen.
    bytes: Vec<u8>,
    /// Structural key to address, for the single-arc tails Lucene shares.
    dedup: HashMap<Vec<u8>, i64>,
    last_frozen_node: i64,
    node_count: u64,
    arc_count: u64,
    arc_with_output_count: u64,
    empty_output: Option<Vec<u8>>,
    finished: bool,
}

impl Default for FstBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl FstBuilder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            frontier: vec![UnCompiledNode::default()],
            last_input: Vec::new(),
            // `FST`'s writing constructor pads the store with one zero
            // byte: "ensure no node gets address 0 which is reserved to
            // mean the stop state w/ no arcs". Every node address is
            // therefore one higher than the bytes alone would give, and a
            // transducer carrying only the empty key has a one-byte store
            // rather than an empty one — which is what Lucene's reader
            // needs to construct a `BytesStore` at all.
            bytes: vec![0],
            dedup: HashMap::new(),
            last_frozen_node: 0,
            node_count: 0,
            arc_count: 0,
            arc_with_output_count: 0,
            empty_output: None,
            finished: false,
        }
    }

    /// Adds one key and its output.
    ///
    /// The empty key is special-cased exactly as Lucene special-cases it:
    /// finality lives on an *incoming* arc, and the root has none, so the
    /// empty key's output is stored beside the automaton rather than in it.
    pub fn add(&mut self, key: &[u8], output: &[u8]) -> Result<()> {
        if self.finished {
            return Err(Error::InvalidFormat {
                details: "this transducer is already finished".to_owned(),
            });
        }
        if !self.last_input.is_empty() && key < self.last_input.as_slice() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "transducer keys arrive in ascending order; {key:?} follows {:?}",
                    self.last_input
                ),
            });
        }

        if key.is_empty() {
            self.frontier[0].is_final = true;
            self.empty_output = Some(output.to_vec());
            return Ok(());
        }

        // The shared prefix with the previous key.
        let mut shared = 0;
        let stop = self.last_input.len().min(key.len());
        while shared < stop && self.last_input[shared] == key[shared] {
            shared += 1;
        }
        let prefix_len_plus_1 = shared + 1;

        while self.frontier.len() < key.len() + 1 {
            self.frontier.push(UnCompiledNode::default());
        }

        self.freeze_tail(prefix_len_plus_1)?;

        for index in prefix_len_plus_1..=key.len() {
            self.frontier[index - 1].arcs.push(BuilderArc {
                label: key[index - 1],
                target: 0,
                output: Vec::new(),
                next_final_output: Vec::new(),
                is_final: false,
            });
        }

        let last_index = key.len();
        if self.last_input.len() != key.len() || prefix_len_plus_1 != key.len() + 1 {
            self.frontier[last_index].is_final = true;
            self.frontier[last_index].output.clear();
        }

        // Push the conflicting part of the output forward, only as far as
        // needed: each arc keeps the longest prefix every key through it
        // shares, and the rest moves down.
        let mut remaining = output.to_vec();
        for index in 1..prefix_len_plus_1 {
            let label = key[index - 1];
            let last_output = self.frontier[index - 1]
                .arcs
                .last()
                .filter(|arc| arc.label == label)
                .map(|arc| arc.output.clone())
                .unwrap_or_default();
            if last_output.is_empty() {
                continue;
            }
            let common = common_prefix(&remaining, &last_output);
            let word_suffix = last_output[common.len()..].to_vec();
            if let Some(arc) = self.frontier[index - 1].arcs.last_mut() {
                arc.output.clone_from(&common);
            }
            // Everything the arc gave up is prepended to every output below
            // it, so the concatenation along any path is unchanged.
            prepend_output(&mut self.frontier[index], &word_suffix);
            remaining = remaining[common.len()..].to_vec();
        }

        if self.last_input.len() == key.len() && prefix_len_plus_1 == key.len() + 1 {
            // The same key twice. Lucene's byte-sequence outputs cannot
            // merge two different values, and its own `merge` throws; froe
            // refuses rather than keeping one of them.
            return Err(Error::InvalidFormat {
                details: format!("{key:?} is added twice, and a byte-string output cannot merge"),
            });
        }
        if let Some(arc) = self.frontier[prefix_len_plus_1 - 1].arcs.last_mut() {
            arc.output = remaining;
        }

        self.last_input = key.to_vec();
        Ok(())
    }

    /// Freezes every frontier node below `prefix_len_plus_1`.
    ///
    /// With both minimum suffix counts at zero, Lucene's prune and defer
    /// branches are unreachable: every node here is compiled, and none is
    /// dropped. What remains is the walk from the deepest node up.
    fn freeze_tail(&mut self, prefix_len_plus_1: usize) -> Result<()> {
        let down_to = prefix_len_plus_1.max(1);
        let mut index = self.last_input.len();
        while index >= down_to {
            let node = std::mem::take(&mut self.frontier[index]);
            // Lucene fakes a node with no arcs as final, because its own
            // enumerators mishandle a non-final dead end even though the
            // format can express one.
            let is_final = node.is_final || node.arcs.is_empty();
            let next_final_output = node.output.clone();
            let compiled = self.compile_node(&node)?;

            let label = self.last_input[index - 1];
            let parent = &mut self.frontier[index - 1];
            if let Some(arc) = parent.arcs.last_mut() {
                debug_assert_eq!(arc.label, label, "replaceLast targets the last arc");
                arc.target = compiled;
                arc.next_final_output = next_final_output;
                arc.is_final = is_final;
            }
            if index == 0 {
                break;
            }
            index -= 1;
        }
        Ok(())
    }

    /// Compiles one node, sharing it when Lucene would.
    ///
    /// `doShareNonSingletonNodes` is false for the terms writer, so only a
    /// node with a single arc is ever shared — and a node with none goes
    /// straight to the store, where it becomes an end-node sentinel rather
    /// than bytes.
    fn compile_node(&mut self, node: &UnCompiledNode) -> Result<i64> {
        if node.arcs.is_empty() {
            return Ok(if node.is_final {
                FINAL_END_NODE
            } else {
                NON_FINAL_END_NODE
            });
        }
        if node.arcs.len() > 1 {
            return self.add_node(node);
        }
        let key = dedup_key(node);
        if let Some(address) = self.dedup.get(&key) {
            // A hit returns the existing address and does **not** touch
            // `last_frozen_node`, so the next node's `BIT_TARGET_NEXT`
            // decision is unaffected. Lucene's `NodeHash.add` behaves the
            // same way, and a writer that updates it here produces
            // different flags from the second shared tail onward.
            return Ok(*address);
        }
        let address = self.add_node(node)?;
        self.dedup.insert(key, address);
        Ok(address)
    }

    /// Writes one node into the store and returns its address.
    fn add_node(&mut self, node: &UnCompiledNode) -> Result<i64> {
        let start = self.bytes.len();
        self.arc_count += node.arcs.len() as u64;
        let last = node.arcs.len() - 1;

        for (index, arc) in node.arcs.iter().enumerate() {
            let mut flags = 0u8;
            if index == last {
                flags |= BIT_LAST_ARC;
            }
            if self.last_frozen_node == arc.target {
                flags |= BIT_TARGET_NEXT;
            }
            if arc.is_final {
                flags |= BIT_FINAL_ARC;
                if !arc.next_final_output.is_empty() {
                    flags |= BIT_ARC_HAS_FINAL_OUTPUT;
                }
            }
            let target_has_arcs = arc.target > 0;
            if !target_has_arcs {
                flags |= BIT_STOP_NODE;
            }
            if !arc.output.is_empty() {
                flags |= BIT_ARC_HAS_OUTPUT;
            }

            let mut sink = CodecOutput::new(&mut self.bytes);
            sink.write_byte(flags)?;
            // BYTE1: the label is one byte.
            sink.write_byte(arc.label)?;
            if !arc.output.is_empty() {
                write_output(&mut sink, &arc.output)?;
                self.arc_with_output_count += 1;
            }
            if !arc.next_final_output.is_empty() {
                write_output(&mut sink, &arc.next_final_output)?;
            }
            if target_has_arcs && flags & BIT_TARGET_NEXT == 0 {
                sink.write_vlong(arc.target)?;
            }
        }

        let address = self.bytes.len() as i64 - 1;
        // Each node is reversed in place, so a reader seeking to the address
        // above reads the arcs in the order they were written by walking
        // *down*.
        self.bytes[start..].reverse();

        self.node_count += 1;
        self.last_frozen_node = address;
        Ok(address)
    }

    /// Finishes and serializes.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        if self.finished {
            return Err(Error::InvalidFormat {
                details: "this transducer is already finished".to_owned(),
            });
        }
        self.freeze_tail(0)?;
        self.finished = true;

        let root = std::mem::take(&mut self.frontier[0]);
        if root.arcs.is_empty() && self.empty_output.is_none() {
            return Err(Error::InvalidFormat {
                details: "a transducer with no key at all has no serialized form".to_owned(),
            });
        }
        let mut start_node = self.compile_node(&root)?;
        // `FST.finish`: an automaton accepting only the empty string has a
        // final end node for a root, which is not an address. Lucene forces
        // it to 0 over the empty store.
        if start_node == FINAL_END_NODE && self.empty_output.is_some() {
            start_node = 0;
        }

        let mut buffer = Vec::new();
        let mut output = CodecOutput::new(&mut buffer);
        output.write_header(FILE_FORMAT_NAME, VERSION_CURRENT)?;
        // Unpacked.
        output.write_byte(0)?;
        match &self.empty_output {
            Some(value) => {
                output.write_byte(1)?;
                // The empty output is serialized, then **reversed**, then
                // written with its own length — so the reader, which reads
                // the store backwards, finds it the right way round.
                let mut inner = Vec::new();
                write_output(&mut CodecOutput::new(&mut inner), value)?;
                inner.reverse();
                output.write_vint(i32::try_from(inner.len()).unwrap_or(i32::MAX))?;
                output.write_bytes(&inner)?;
            }
            None => output.write_byte(0)?,
        }
        // BYTE1.
        output.write_byte(0)?;
        output.write_vlong(start_node)?;
        output.write_vlong(self.node_count as i64)?;
        output.write_vlong(self.arc_count as i64)?;
        output.write_vlong(self.arc_with_output_count as i64)?;
        output.write_vlong(self.bytes.len() as i64)?;
        output.write_bytes(&self.bytes)?;
        Ok(buffer)
    }
}

/// `ByteSequenceOutputs.write`: a `VInt` length, then the bytes.
///
/// `writeFinalOutput` is not overridden, so a final output takes the same
/// form.
fn write_output<Sink: std::io::Write>(output: &mut CodecOutput<Sink>, value: &[u8]) -> Result<()> {
    output.write_vint(i32::try_from(value.len()).unwrap_or(i32::MAX))?;
    output.write_bytes(value)
}

/// `ByteSequenceOutputs.common`.
fn common_prefix(left: &[u8], right: &[u8]) -> Vec<u8> {
    let shared = left
        .iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count();
    left[..shared].to_vec()
}

/// `Builder.UnCompiledNode.prependOutput`: every arc out of the node, and
/// the node's own final output, gain the prefix the parent gave up.
fn prepend_output(node: &mut UnCompiledNode, prefix: &[u8]) {
    if prefix.is_empty() {
        return;
    }
    for arc in &mut node.arcs {
        let mut combined = prefix.to_vec();
        combined.extend_from_slice(&arc.output);
        arc.output = combined;
    }
    if node.is_final {
        let mut combined = prefix.to_vec();
        combined.extend_from_slice(&node.output);
        node.output = combined;
    }
}

/// A node's identity for sharing: its arcs, resolved to addresses.
///
/// `NodeHash.nodesEqual` compares label, target, output, final output and
/// finality — not the serialized flags, whose `BIT_TARGET_NEXT` depends on
/// *when* the node was written rather than on what it contains.
fn dedup_key(node: &UnCompiledNode) -> Vec<u8> {
    let mut key = Vec::new();
    for arc in &node.arcs {
        key.push(arc.label);
        key.extend_from_slice(&arc.target.to_be_bytes());
        key.push(u8::from(arc.is_final));
        key.extend_from_slice(&(arc.output.len() as u32).to_be_bytes());
        key.extend_from_slice(&arc.output);
        key.extend_from_slice(&(arc.next_final_output.len() as u32).to_be_bytes());
        key.extend_from_slice(&arc.next_final_output);
    }
    key
}
