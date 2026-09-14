//! The block-tree terms dictionary: `.tim` and `.tip`.
//!
//! `docs/analysis/lucene-4-7-codec.md` §7, from
//! `codecs/BlockTreeTermsWriter.java`.
//!
//! Terms of one field arrive in unsigned byte order with the postings
//! metadata §6 produced for them. The writer groups them into blocks of 25
//! to 48 entries by shared prefix, writes each block's suffixes,
//! statistics and metadata into `.tim`, and builds the field's `.tip`
//! transducer from block prefixes to file pointers.
//!
//! # The partition is a discarded transducer
//!
//! Lucene decides block boundaries by feeding every term into a second
//! transducer builder whose outputs are `NoOutputs` and whose `freezeTail`
//! is replaced, then throwing the transducer away and keeping what the
//! frontier counted (§7.7). froe keeps the frontier and not the builder:
//! the counting is the whole of what that machinery contributes, and
//! [`FstBuilder`] is used only for the `.tip` the reader actually loads.
//!
//! # What this does not do
//!
//! **No term longer than the index's limit, and no per-field trailer.**
//! Closing the writer appends one field directory to each file and an
//! eight-byte offset to it, which is the only way a reader finds either.

use std::io::Write;

use crate::error::{Error, Result};
use crate::index::lucene::codec::data_output::CodecOutput;
use crate::index::lucene::codec::fst::FstBuilder;
use crate::index::lucene::codec::postings::{
    IndexOptions, LongCount, PostingsWriter, TermMetadata, write_terms_header,
};

/// `BlockTreeTermsWriter.DEFAULT_MIN_BLOCK_SIZE`.
pub const MIN_ITEMS_IN_BLOCK: usize = 25;

/// `BlockTreeTermsWriter.DEFAULT_MAX_BLOCK_SIZE`.
pub const MAX_ITEMS_IN_BLOCK: usize = 48;

/// `BlockTreeTermsWriter.TERMS_CODEC_NAME`.
const TERMS_CODEC_NAME: &str = "BLOCK_TREE_TERMS_DICT";

/// `BlockTreeTermsWriter.TERMS_INDEX_CODEC_NAME`.
const TERMS_INDEX_CODEC_NAME: &str = "BLOCK_TREE_TERMS_INDEX";

/// `TERMS_VERSION_CURRENT` and `TERMS_INDEX_VERSION_CURRENT`, both
/// `VERSION_META_ARRAY`.
const VERSION_CURRENT: i32 = 2;

/// `BlockTreeTermsWriter.OUTPUT_FLAG_IS_FLOOR`.
const OUTPUT_FLAG_IS_FLOOR: u64 = 0x1;

/// `BlockTreeTermsWriter.OUTPUT_FLAG_HAS_TERMS`.
const OUTPUT_FLAG_HAS_TERMS: u64 = 0x2;

/// The field a term run belongs to.
#[derive(Clone, Copy, Debug)]
pub struct TermsFieldInfo {
    /// `FieldInfo.number`, which the directory records.
    pub number: i32,
    /// The index options, which decide `longsSize`, whether a term's total
    /// frequency is stored, and whether the directory carries
    /// `sumTotalTermFreq`.
    pub options: IndexOptions,
}

/// What a term contributes to the block's statistics section.
#[derive(Clone, Copy, Debug)]
pub struct TermStatistics {
    /// `TermStats.docFreq`: how many documents hold the term.
    pub document_frequency: i32,
    /// `TermStats.totalTermFreq`: how many times it occurs across them,
    /// and `-1` for a field without frequencies.
    pub total_term_frequency: i64,
}

/// What a field contributes to the directory when it finishes.
///
/// The three are exactly `TermsConsumer.finish`'s arguments. The first two
/// are running sums over the field's terms; **the document count is not** —
/// it is the cardinality of the set of documents the field appeared in,
/// which summing document frequencies would overcount for every document
/// holding several terms of one field. Lucene's index checker refuses a
/// segment where it does not match what it recomputes.
#[derive(Clone, Copy, Debug)]
pub struct FieldStatistics {
    /// `sumTotalTermFreq`, and `-1` for a field without frequencies, where
    /// it is omitted from the directory entirely.
    pub sum_total_term_frequency: i64,
    /// `sumDocFreq`: the sum of the terms' document frequencies.
    pub sum_document_frequency: i64,
    /// `docCount`: how many documents hold any term of the field.
    pub document_count: i32,
}

/// The two files, once every field is written.
#[derive(Debug)]
pub struct TermsFiles<Sink> {
    /// `.tim`.
    pub terms: Sink,
    /// `.tip`.
    pub index: Sink,
}

/// `BlockTreeTermsWriter.encodeOutput`.
const fn encode_output(file_pointer: u64, has_terms: bool, is_floor: bool) -> u64 {
    (file_pointer << 2)
        | if has_terms { OUTPUT_FLAG_HAS_TERMS } else { 0 }
        | if is_floor { OUTPUT_FLAG_IS_FLOOR } else { 0 }
}

/// One entry on the pending stack: a term waiting for a block, or a block
/// already written and waiting to become an entry of its parent.
enum PendingEntry {
    Term(PendingTerm),
    Block(PendingBlock),
}

struct PendingTerm {
    term: Vec<u8>,
    statistics: TermStatistics,
    metadata: TermMetadata,
}

struct PendingBlock {
    prefix: Vec<u8>,
    file_pointer: u64,
    has_terms: bool,
    is_floor: bool,
    /// The byte after the shared prefix this floor block starts at, and
    /// `-1` for the head of a floor run or a block that is not floored.
    floor_lead_byte: i32,
    /// Before [`compile_index`], the keys of the sub-blocks beneath this
    /// one; after it, those plus this block's own entry — the whole
    /// subtree, which is what the parent copies in.
    index: Vec<(Vec<u8>, Vec<u8>)>,
}

/// What one call of `writeBlock` is asked for.
///
/// Lucene passes these as seven positional arguments, three of them
/// booleans; naming them is the difference between reading a call site and
/// counting commas.
struct BlockRequest {
    /// The bytes the block is keyed by in `.tip`, which for a floor block
    /// after the first is the shared prefix plus its lead byte.
    prefix: Vec<u8>,
    /// How many bytes to strip from each entry to get its suffix — the
    /// shared prefix, never the floor block's longer key.
    prefix_length: usize,
    /// How far back from the top of the pending stack the block's slice
    /// begins.
    start_backwards: usize,
    /// How many entries it holds.
    length: usize,
    is_floor: bool,
    /// The byte after the shared prefix, and `-1` for the head of a floor
    /// run or a block that is not floored.
    floor_lead_byte: i32,
    is_last_in_floor: bool,
}

/// One node of the block-detection frontier (§7.7).
#[derive(Default)]
struct FrontierNode {
    /// Whether a term ends exactly at this prefix.
    is_final: bool,
    /// What the children frozen beneath this node reported upward: 1 for a
    /// child that wrote its blocks, its own whole count for a straggler.
    child_counts: Vec<u64>,
}

/// A field's entry in both directories.
struct FieldMetaData {
    number: i32,
    options: IndexOptions,
    term_count: u64,
    root_code: Vec<u8>,
    index_start: u64,
    statistics: FieldStatistics,
    long_count: usize,
}

/// The block-tree terms writer.
///
/// One per segment. Fields are written one at a time in the order the
/// directory will list them: [`Self::start_field`], then
/// [`Self::add_term`] per term in increasing byte order, then
/// [`Self::finish_field`]; [`Self::finish`] closes both files.
pub struct TermsWriter<Sink: Write> {
    terms: CodecOutput<Sink>,
    index: CodecOutput<Sink>,
    fields: Vec<FieldMetaData>,

    field: Option<TermsFieldInfo>,
    long_count: usize,
    term_count: u64,

    pending: Vec<PendingEntry>,
    frontier: Vec<FrontierNode>,
    last_term: Vec<u8>,
}

impl<Sink: Write> TermsWriter<Sink> {
    /// Opens both files and writes their headers — and, nested inside
    /// `.tim`, the postings format's own header and block size (§7.1).
    pub fn new(terms: Sink, index: Sink) -> Result<Self> {
        let mut terms = CodecOutput::new(terms);
        terms.write_header(TERMS_CODEC_NAME, VERSION_CURRENT)?;
        let mut index = CodecOutput::new(index);
        index.write_header(TERMS_INDEX_CODEC_NAME, VERSION_CURRENT)?;
        write_terms_header(&mut terms)?;
        Ok(Self {
            terms,
            index,
            fields: Vec::new(),
            field: None,
            long_count: 0,
            term_count: 0,
            pending: Vec::new(),
            frontier: vec![FrontierNode::default()],
            last_term: Vec::new(),
        })
    }

    /// Begins a field, handing its options to the postings writer and
    /// keeping the file-pointer count the directory records as
    /// `longsSize`.
    pub fn start_field<PostingsSink: Write>(
        &mut self,
        field: TermsFieldInfo,
        postings: &mut PostingsWriter<PostingsSink>,
    ) -> Result<LongCount> {
        if self.field.is_some() {
            return Err(Error::InvalidFormat {
                details: "a field is already open; finish it before starting another".to_owned(),
            });
        }
        let long_count = postings.set_field(field.options)?;
        self.field = Some(field);
        self.long_count = long_count.value();
        self.term_count = 0;
        self.pending.clear();
        self.frontier = vec![FrontierNode::default()];
        self.last_term.clear();
        Ok(long_count)
    }

    /// Adds one term, in increasing unsigned byte order.
    ///
    /// The term goes through the frontier **before** it joins the pending
    /// stack, because freezing may write blocks out of the entries already
    /// there and this term is not one of them.
    pub fn add_term(
        &mut self,
        term: &[u8],
        statistics: TermStatistics,
        metadata: TermMetadata,
    ) -> Result<()> {
        if self.field.is_none() {
            return Err(Error::InvalidFormat {
                details: "a term outside any field".to_owned(),
            });
        }
        if statistics.document_frequency < 1 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a term occurs in at least one document, so a frequency of {} \
                     cannot be written",
                    statistics.document_frequency
                ),
            });
        }
        if self.term_count > 0 && term <= self.last_term.as_slice() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "terms are written in increasing byte order and {term:?} does not \
                     follow {:?}",
                    self.last_term
                ),
            });
        }
        self.push_term(term)?;
        self.pending.push(PendingEntry::Term(PendingTerm {
            term: term.to_vec(),
            statistics,
            metadata,
        }));
        self.term_count += 1;
        Ok(())
    }

    /// Finishes the field: freezes the frontier to the root, saves the
    /// field's transducer, and records its directory entry.
    ///
    /// **A field with no term contributes nothing** — no directory entry,
    /// no transducer, and no change to the field count — which is what
    /// Lucene does and what a field whose every value analyzed to no token
    /// produces.
    pub fn finish_field(&mut self, statistics: FieldStatistics) -> Result<()> {
        let Some(field) = self.field else {
            return Err(Error::InvalidFormat {
                details: "no field is open".to_owned(),
            });
        };
        if self.term_count == 0 {
            self.field = None;
            return Ok(());
        }
        // The field stays open through the freeze: writing the root block
        // needs its index options.
        self.freeze(0)?;
        self.field = None;

        let root = match self.pending.pop() {
            Some(PendingEntry::Block(block)) if block.prefix.is_empty() => block,
            _ => {
                return Err(Error::InvalidFormat {
                    details: "finishing a field leaves exactly one root block at the empty \
                              prefix; the frontier produced something else"
                        .to_owned(),
                });
            }
        };
        if !self.pending.is_empty() {
            return Err(Error::InvalidFormat {
                details: "the root block did not absorb every pending entry".to_owned(),
            });
        }

        let index_start = self.index.position();
        let mut builder = FstBuilder::new();
        for (key, output) in &root.index {
            builder.add(key, output)?;
        }
        self.index.write_bytes(&builder.finish()?)?;

        // The root block's prefix is empty, so its output is the
        // transducer's empty output — and the directory's `rootCode`.
        let root_code = root
            .index
            .first()
            .filter(|(key, _)| key.is_empty())
            .map(|(_, output)| output.clone())
            .ok_or_else(|| Error::InvalidFormat {
                details: "the root block's own entry is missing from its index".to_owned(),
            })?;

        self.fields.push(FieldMetaData {
            number: field.number,
            options: field.options,
            term_count: self.term_count,
            root_code,
            index_start,
            statistics,
            long_count: self.long_count,
        });
        Ok(())
    }

    /// Writes both directories and their trailers, and hands back the
    /// files.
    pub fn finish(mut self) -> Result<TermsFiles<Sink>> {
        if self.field.is_some() {
            return Err(Error::InvalidFormat {
                details: "a field is still open".to_owned(),
            });
        }
        let directory_start = self.terms.position();
        let index_directory_start = self.index.position();

        self.terms.write_vint(self.fields.len() as i32)?;
        for field in &self.fields {
            self.terms.write_vint(field.number)?;
            self.terms.write_vlong(field.term_count as i64)?;
            self.terms.write_vint(field.root_code.len() as i32)?;
            self.terms.write_bytes(&field.root_code)?;
            if field.options.has_frequencies() {
                self.terms
                    .write_vlong(field.statistics.sum_total_term_frequency)?;
            }
            self.terms
                .write_vlong(field.statistics.sum_document_frequency)?;
            self.terms.write_vint(field.statistics.document_count)?;
            self.terms.write_vint(field.long_count as i32)?;
            self.index.write_vlong(field.index_start as i64)?;
        }
        // Eight bytes, absolute: the only pointer to either directory.
        self.terms.write_long(directory_start as i64)?;
        self.index.write_long(index_directory_start as i64)?;
        Ok(TermsFiles {
            terms: self.terms.into_inner(),
            index: self.index.into_inner(),
        })
    }
}

impl<Sink: Write> TermsWriter<Sink> {
    /// The frontier walk of `Builder.add` (§7.7), kept for its counts.
    fn push_term(&mut self, term: &[u8]) -> Result<()> {
        if term.is_empty() {
            // The empty term takes `Builder.add`'s early path: it marks the
            // root final and counts there, and does not become the previous
            // term. It sorts first, so there is nothing to freeze.
            self.frontier[0].is_final = true;
            return Ok(());
        }
        if self.frontier.len() < term.len() + 1 {
            self.frontier
                .resize_with(term.len() + 1, FrontierNode::default);
        }

        let stop = self.last_term.len().min(term.len());
        let mut common = 0;
        while common < stop && self.last_term[common] == term[common] {
            common += 1;
        }

        self.freeze(common + 1)?;
        self.frontier[term.len()].is_final = true;
        self.last_term = term.to_vec();
        Ok(())
    }

    /// `FindBlocks.freeze`: deepest first, a node that reaches the minimum
    /// writes its blocks and reports one entry upward, one that does not
    /// carries its whole count up as stragglers.
    fn freeze(&mut self, prefix_length_plus_one: usize) -> Result<()> {
        let mut depth = self.last_term.len() + 1;
        while depth > prefix_length_plus_one {
            depth -= 1;
            let node = &mut self.frontier[depth];
            let mut total = u64::from(node.is_final);
            total += node.child_counts.iter().sum::<u64>();
            node.is_final = false;
            node.child_counts.clear();

            let report = if total >= MIN_ITEMS_IN_BLOCK as u64 || depth == 0 {
                self.write_blocks(depth, total as usize)?;
                1
            } else {
                total
            };
            if depth > 0 {
                // The parent reads this on its own turn of the same loop,
                // through the arc that still points here.
                self.frontier[depth - 1].child_counts.push(report);
            }
        }
        Ok(())
    }

    /// `writeBlocks` (§7.8): one block, or a floor run.
    fn write_blocks(&mut self, prefix_length: usize, count: usize) -> Result<()> {
        if prefix_length == 0 || count <= MAX_ITEMS_IN_BLOCK {
            let prefix = self.last_term[..prefix_length].to_vec();
            let mut block = self.write_block(BlockRequest {
                prefix,
                prefix_length,
                start_backwards: count,
                length: count,
                is_floor: false,
                floor_lead_byte: -1,
                is_last_in_floor: true,
            })?;
            compile_index(&mut block, Vec::new())?;
            self.pending.push(PendingEntry::Block(block));
            return Ok(());
        }

        let groups = self.group_by_lead_byte(prefix_length, count);
        let mut pending_count = 0usize;
        let mut start_label = groups[0].0;
        let mut remaining = count;
        let mut first_block: Option<PendingBlock> = None;
        let mut floor_blocks: Vec<PendingBlock> = Vec::new();

        for (index, (_, group_size)) in groups.iter().enumerate() {
            pending_count += group_size;
            if pending_count < MIN_ITEMS_IN_BLOCK {
                continue;
            }
            let prefix = self.floor_prefix(prefix_length, start_label);
            let block = self.write_block(BlockRequest {
                prefix,
                prefix_length,
                start_backwards: remaining,
                length: pending_count,
                is_floor: true,
                floor_lead_byte: start_label,
                is_last_in_floor: remaining == pending_count,
            })?;
            if first_block.is_none() {
                first_block = Some(block);
            } else {
                floor_blocks.push(block);
            }
            remaining -= pending_count;
            pending_count = 0;
            start_label = groups.get(index + 1).map_or(-1, |group| group.0);
            if remaining == 0 {
                break;
            }
            if remaining <= MAX_ITEMS_IN_BLOCK {
                // The tail fits one block, which may leave it below the
                // minimum — a shape the reader accepts.
                let prefix = self.floor_prefix(prefix_length, start_label);
                floor_blocks.push(self.write_block(BlockRequest {
                    prefix,
                    prefix_length,
                    start_backwards: remaining,
                    length: remaining,
                    is_floor: true,
                    floor_lead_byte: start_label,
                    is_last_in_floor: true,
                })?);
                break;
            }
        }

        let mut first = first_block.ok_or_else(|| Error::InvalidFormat {
            details: "a floored prefix writes at least one block".to_owned(),
        })?;
        compile_index(&mut first, floor_blocks)?;
        self.pending.push(PendingEntry::Block(first));
        Ok(())
    }

    /// The prefix a floor block is keyed and stripped by: the shared
    /// prefix, plus the lead byte unless the group is the `-1` one.
    fn floor_prefix(&self, prefix_length: usize, start_label: i32) -> Vec<u8> {
        let mut prefix = self.last_term[..prefix_length].to_vec();
        if start_label != -1 {
            prefix.push(start_label as u8);
        }
        prefix
    }

    /// The grouping of §7.8, quirk and all: `groups[0]`'s label is always
    /// `-1`, because the guard that suppresses the first flush leaves the
    /// running label at its initial value.
    ///
    /// Reproduced rather than corrected. It only ever splits one entry into
    /// a group of its own, and a group of one never reaches the minimum, so
    /// the segmenter below always merges it forward — but the block
    /// boundaries are what the `.tip` keys by, so this is not a place to
    /// improve on Lucene.
    fn group_by_lead_byte(&self, prefix_length: usize, count: usize) -> Vec<(i32, usize)> {
        let mut groups: Vec<(i32, usize)> = Vec::new();
        let mut last_label = -1i32;
        let mut size = 0usize;
        for entry in &self.pending[self.pending.len() - count..] {
            let label = match entry {
                PendingEntry::Term(term) => {
                    if term.term.len() == prefix_length {
                        -1
                    } else {
                        i32::from(term.term[prefix_length])
                    }
                }
                PendingEntry::Block(block) => i32::from(block.prefix[prefix_length]),
            };
            if label != last_label && size != 0 {
                groups.push((last_label, size));
                last_label = label;
                size = 0;
            }
            size += 1;
        }
        groups.push((last_label, size));
        groups
    }

    /// `writeBlock` (§7.9): the two flagged counts and the three
    /// length-prefixed sections.
    fn write_block(&mut self, request: BlockRequest) -> Result<PendingBlock> {
        let BlockRequest {
            prefix,
            prefix_length,
            start_backwards,
            length,
            is_floor,
            floor_lead_byte,
            is_last_in_floor,
        } = request;
        let options = self.field.ok_or_else(|| Error::InvalidFormat {
            details: "a block outside any field".to_owned(),
        })?;
        let start = self.pending.len() - start_backwards;
        let start_file_pointer = self.terms.position();
        let slice: Vec<PendingEntry> = self.pending.drain(start..start + length).collect();
        let is_leaf = slice
            .iter()
            .all(|entry| matches!(entry, PendingEntry::Term(_)));

        self.terms
            .write_vint(((length as i32) << 1) | i32::from(is_last_in_floor))?;

        let mut suffixes = Vec::new();
        let mut statistics = Vec::new();
        let mut metadata = Vec::new();
        let mut previous: Option<TermMetadata> = None;
        let mut term_count = 0usize;
        let mut index = Vec::new();

        for entry in slice {
            match entry {
                PendingEntry::Term(term) => {
                    let suffix = &term.term[prefix_length..];
                    let mut output = CodecOutput::new(&mut suffixes);
                    // A leaf block spends no bit distinguishing entries.
                    output.write_vint(if is_leaf {
                        suffix.len() as i32
                    } else {
                        (suffix.len() as i32) << 1
                    })?;
                    output.write_bytes(suffix)?;

                    let mut output = CodecOutput::new(&mut statistics);
                    output.write_vint(term.statistics.document_frequency)?;
                    if options.options.has_frequencies() {
                        // Stored as the excess over the document frequency.
                        output.write_vlong(
                            term.statistics.total_term_frequency
                                - i64::from(term.statistics.document_frequency),
                        )?;
                    }

                    encode_term(
                        &mut metadata,
                        options.options,
                        &term.metadata,
                        previous.as_ref(),
                    )?;
                    previous = Some(term.metadata);
                    term_count += 1;
                }
                PendingEntry::Block(block) => {
                    let suffix = &block.prefix[prefix_length..];
                    let mut output = CodecOutput::new(&mut suffixes);
                    output.write_vint(((suffix.len() as i32) << 1) | 1)?;
                    output.write_bytes(suffix)?;
                    // Backwards: the sub-block was written before its
                    // parent, so the reader subtracts.
                    output.write_vlong((start_file_pointer - block.file_pointer) as i64)?;
                    index.extend(block.index);
                }
            }
        }

        self.terms
            .write_vint(((suffixes.len() as i32) << 1) | i32::from(is_leaf))?;
        self.terms.write_bytes(&suffixes)?;
        self.terms.write_vint(statistics.len() as i32)?;
        self.terms.write_bytes(&statistics)?;
        self.terms.write_vint(metadata.len() as i32)?;
        self.terms.write_bytes(&metadata)?;

        Ok(PendingBlock {
            prefix,
            file_pointer: start_file_pointer,
            has_terms: term_count != 0,
            is_floor,
            floor_lead_byte,
            index,
        })
    }
}

/// `PendingBlock.compileIndex` (§7.10): the block's own key and output in
/// front of every key beneath it.
fn compile_index(block: &mut PendingBlock, floor_blocks: Vec<PendingBlock>) -> Result<()> {
    let mut encoded = Vec::new();
    {
        let mut output = CodecOutput::new(&mut encoded);
        output.write_vlong(
            encode_output(block.file_pointer, block.has_terms, block.is_floor) as i64,
        )?;
        if block.is_floor {
            output.write_vint(floor_blocks.len() as i32)?;
            for sub in &floor_blocks {
                output.write_byte(sub.floor_lead_byte as u8)?;
                output.write_vlong(
                    (((sub.file_pointer - block.file_pointer) << 1) | u64::from(sub.has_terms))
                        as i64,
                )?;
            }
        }
    }
    let mut index = vec![(block.prefix.clone(), encoded)];
    index.append(&mut block.index);
    for sub in floor_blocks {
        index.extend(sub.index);
    }
    block.index = index;
    Ok(())
}

/// `Lucene41PostingsWriter.encodeTerm` (§6.4), written into the block's
/// metadata section: the pointer deltas as `VLong`s, then the term's own
/// byte stream.
///
/// `previous` is `None` for the first term of a block, which is Lucene's
/// `absolute`: the deltas are then the absolute pointers, so a reader can
/// start at any block.
fn encode_term(
    metadata: &mut Vec<u8>,
    options: IndexOptions,
    term: &TermMetadata,
    previous: Option<&TermMetadata>,
) -> Result<()> {
    let mut output = CodecOutput::new(metadata);
    output.write_vlong(
        (term.document_start - previous.map_or(0, |state| state.document_start)) as i64,
    )?;
    if options.has_positions() {
        output.write_vlong(
            (term.position_start - previous.map_or(0, |state| state.position_start)) as i64,
        )?;
        if options.has_offsets() {
            output.write_vlong(
                (term.payload_start - previous.map_or(0, |state| state.payload_start)) as i64,
            )?;
        }
    }
    if let Some(document) = term.singleton_document {
        output.write_vint(document)?;
    }
    if options.has_positions()
        && let Some(offset) = term.last_position_block_offset
    {
        output.write_vlong(offset as i64)?;
    }
    if let Some(offset) = term.skip_offset {
        output.write_vlong(offset as i64)?;
    }
    Ok(())
}
