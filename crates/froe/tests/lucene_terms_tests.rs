//! The block-tree terms dictionary, against bytes computed by hand from
//! the specification and read back by an independent decoder.
//!
//! `docs/analysis/lucene-4-7-codec.md` §7. One field's whole `.tim` is
//! pinned byte for byte below; the larger corpora — the block-size
//! boundaries at 24, 48, 49 and 200 entries, and nested prefixes — are
//! walked by a decoder written to **the reader's** rules: blocks are
//! entered through the root code, sub-blocks through their backward
//! pointers, and floor followers through the fact that they sit
//! immediately after the block that says it is not the last in its floor.
//! What comes back out must be the terms that went in, with their
//! statistics and their postings metadata.
//!
//! The decoder also reconstructs the `.tip` entries it should have
//! produced, and the file's transducer must equal what task 0903's
//! `FstBuilder` makes of them.

use froe::index::lucene::codec::fst::FstBuilder;
use froe::index::lucene::codec::postings::{
    IndexOptions, PostingsWriter, SegmentShape, TermMetadata,
};
use froe::index::lucene::codec::terms::{
    FieldStatistics, TermStatistics, TermsFieldInfo, TermsWriter,
};

/// A field to write, and the terms it holds.
struct Field {
    info: TermsFieldInfo,
    terms: Vec<Vec<u8>>,
}

/// What one term was given to the writer, for the decoder to be checked
/// against.
#[derive(Debug, PartialEq, Eq)]
struct Given {
    term: Vec<u8>,
    document_frequency: i32,
    total_term_frequency: i64,
    metadata: TermMetadata,
}

struct Written {
    terms: Vec<u8>,
    index: Vec<u8>,
    given: Vec<Vec<Given>>,
    statistics: Vec<FieldStatistics>,
}

/// How many documents the term at `index` occurs in: 1, 2, 3, 1, … so both
/// the pulsed and the unpulsed postings shapes appear in every corpus.
fn document_count_for(index: usize) -> i32 {
    index as i32 % 3 + 1
}

fn write_fields(fields: &[Field]) -> Written {
    let any_positions = fields
        .iter()
        .any(|field| field.info.options.has_positions());
    let any_offsets = fields.iter().any(|field| field.info.options.has_offsets());
    let mut postings = PostingsWriter::new(
        SegmentShape {
            document_count: 8,
            any_field_has_positions: any_positions,
            any_field_has_offsets: any_offsets,
        },
        Vec::new(),
        any_positions.then(Vec::new),
        any_offsets.then(Vec::new),
    )
    .expect("open the postings files");
    let mut writer = TermsWriter::new(Vec::new(), Vec::new()).expect("open the terms files");

    let mut given = Vec::new();
    let mut statistics = Vec::new();
    for field in fields {
        writer
            .start_field(field.info, &mut postings)
            .expect("start the field");
        let mut field_given = Vec::new();
        let mut sum_total_term_frequency = 0i64;
        let mut sum_document_frequency = 0i64;
        let mut highest_document = -1i32;
        for (index, term) in field.terms.iter().enumerate() {
            let documents = document_count_for(index);
            postings.start_term();
            for document in 0..documents {
                postings.start_document(document, 1).expect("the document");
                if field.info.options.has_positions() {
                    postings.add_position(0, 0, 3).expect("the position");
                }
                postings.finish_document();
            }
            let metadata = postings.finish_term().expect("finish the term");
            let term_statistics = TermStatistics {
                document_frequency: metadata.document_frequency,
                total_term_frequency: metadata.total_term_frequency,
            };
            writer
                .add_term(term, term_statistics, metadata)
                .expect("add the term");
            sum_document_frequency += i64::from(metadata.document_frequency);
            if field.info.options.has_frequencies() {
                sum_total_term_frequency += metadata.total_term_frequency;
            }
            highest_document = highest_document.max(documents - 1);
            field_given.push(Given {
                term: term.clone(),
                document_frequency: metadata.document_frequency,
                total_term_frequency: metadata.total_term_frequency,
                metadata,
            });
        }
        let field_statistics = FieldStatistics {
            sum_total_term_frequency: if field.info.options.has_frequencies() {
                sum_total_term_frequency
            } else {
                -1
            },
            sum_document_frequency,
            document_count: highest_document + 1,
        };
        writer
            .finish_field(field_statistics)
            .expect("finish the field");
        given.push(field_given);
        statistics.push(field_statistics);
    }

    let files = writer.finish().expect("finish");
    Written {
        terms: files.terms,
        index: files.index,
        given,
        statistics,
    }
}

fn read_vlong(bytes: &[u8], at: usize) -> (u64, usize) {
    let mut value = 0u64;
    let mut shift = 0;
    let mut cursor = at;
    loop {
        let byte = bytes[cursor];
        cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return (value, cursor);
        }
        shift += 7;
    }
}

fn read_vint(bytes: &[u8], at: usize) -> (i32, usize) {
    let (value, next) = read_vlong(bytes, at);
    (value as i32, next)
}

fn read_long(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// The directory's start, which lives only in the file's last eight bytes.
fn directory_start(file: &[u8]) -> u64 {
    read_long(file, file.len() - 8)
}

/// One field's entry in `.tim`'s directory.
#[derive(Debug)]
struct DirectoryEntry {
    number: i32,
    term_count: u64,
    root_code: Vec<u8>,
    sum_total_term_frequency: Option<i64>,
    sum_document_frequency: i64,
    document_count: i32,
    long_count: i32,
}

fn read_directory(terms: &[u8], options: &[IndexOptions]) -> Vec<DirectoryEntry> {
    let mut at = directory_start(terms) as usize;
    let (count, next) = read_vint(terms, at);
    at = next;
    let mut entries = Vec::new();
    for field_options in options.iter().take(count as usize) {
        let (number, next) = read_vint(terms, at);
        let (term_count, next) = read_vlong(terms, next);
        let (root_length, next) = read_vint(terms, next);
        let root_code = terms[next..next + root_length as usize].to_vec();
        let mut cursor = next + root_length as usize;
        let sum_total_term_frequency = if field_options.has_frequencies() {
            let (value, next) = read_vlong(terms, cursor);
            cursor = next;
            Some(value as i64)
        } else {
            None
        };
        let (sum_document_frequency, next) = read_vlong(terms, cursor);
        let (document_count, next) = read_vint(terms, next);
        let (long_count, next) = read_vint(terms, next);
        at = next;
        entries.push(DirectoryEntry {
            number,
            term_count,
            root_code,
            sum_total_term_frequency,
            sum_document_frequency: sum_document_frequency as i64,
            document_count,
            long_count,
        });
    }
    entries
}

/// What one block said about itself.
struct BlockInfo {
    end: usize,
    has_terms: bool,
    is_last_in_floor: bool,
    /// The first byte of the first entry's suffix, and `-1` when that
    /// suffix is empty — the label a floor follower is keyed by.
    lead_byte: i32,
}

/// Reads `.tim` the way the reader does: in through the root code, down
/// through the sub-block pointers, and along a floor run by the fact that
/// the next block begins where this one ends.
struct Walk<'a> {
    terms: &'a [u8],
    options: IndexOptions,
    long_count: usize,
    decoded: Vec<Given>,
    index_pairs: Vec<(Vec<u8>, Vec<u8>)>,
    blocks: usize,
    floor_runs: usize,
    leaf_blocks: usize,
}

impl<'a> Walk<'a> {
    fn new(terms: &'a [u8], options: IndexOptions, long_count: usize) -> Self {
        Self {
            terms,
            options,
            long_count,
            decoded: Vec::new(),
            index_pairs: Vec::new(),
            blocks: 0,
            floor_runs: 0,
            leaf_blocks: 0,
        }
    }

    /// Walks one block and every floor follower behind it, and records the
    /// `.tip` entry the head should have.
    fn walk(&mut self, start: usize, prefix: Vec<u8>) {
        let mut chain: Vec<(u64, bool, i32)> = Vec::new();
        let mut at = start;
        loop {
            let info = self.read_block(at, &prefix);
            chain.push((at as u64, info.has_terms, info.lead_byte));
            if info.is_last_in_floor {
                break;
            }
            at = info.end;
        }
        let is_floor = chain.len() > 1;
        if is_floor {
            self.floor_runs += 1;
        }
        let (head, head_has_terms, _) = chain[0];

        let mut output = Vec::new();
        write_vlong(
            &mut output,
            (head << 2) | u64::from(head_has_terms) << 1 | u64::from(is_floor),
        );
        if is_floor {
            write_vlong(&mut output, (chain.len() - 1) as u64);
            for (pointer, has_terms, lead_byte) in &chain[1..] {
                output.push(*lead_byte as u8);
                write_vlong(&mut output, ((pointer - head) << 1) | u64::from(*has_terms));
            }
        }
        self.index_pairs.push((prefix, output));
    }

    fn read_block(&mut self, at: usize, prefix: &[u8]) -> BlockInfo {
        self.blocks += 1;
        let (header, next) = read_vint(self.terms, at);
        let entry_count = (header >> 1) as usize;
        let is_last_in_floor = header & 1 == 1;

        let (suffix_header, next) = read_vint(self.terms, next);
        let suffix_length = (suffix_header >> 1) as usize;
        let is_leaf = suffix_header & 1 == 1;
        if is_leaf {
            self.leaf_blocks += 1;
        }
        let mut suffix_at = next;
        let suffix_end = next + suffix_length;

        let (statistics_length, next) = read_vint(self.terms, suffix_end);
        let mut statistics_at = next;
        let statistics_end = next + statistics_length as usize;

        let (metadata_length, next) = read_vint(self.terms, statistics_end);
        let mut metadata_at = next;
        let end = next + metadata_length as usize;

        // The pointers are absolute for the block's first term and deltas
        // after it, which is what `absolute` means in §7.9.
        let mut document_start = 0u64;
        let mut position_start = 0u64;
        let mut payload_start = 0u64;
        let mut has_terms = false;
        let mut lead_byte = -1i32;

        for entry in 0..entry_count {
            let (code, next) = read_vint(self.terms, suffix_at);
            let (length, is_sub_block) = if is_leaf {
                (code as usize, false)
            } else {
                ((code >> 1) as usize, code & 1 == 1)
            };
            let suffix = self.terms[next..next + length].to_vec();
            suffix_at = next + length;
            if entry == 0 {
                lead_byte = suffix.first().map_or(-1, |byte| i32::from(*byte));
            }
            let mut full = prefix.to_vec();
            full.extend_from_slice(&suffix);

            if is_sub_block {
                let (delta, next) = read_vlong(self.terms, suffix_at);
                suffix_at = next;
                self.walk((at as u64 - delta) as usize, full);
                continue;
            }

            has_terms = true;
            let (document_frequency, next) = read_vint(self.terms, statistics_at);
            statistics_at = next;
            let total_term_frequency = if self.options.has_frequencies() {
                let (excess, next) = read_vlong(self.terms, statistics_at);
                statistics_at = next;
                excess as i64 + i64::from(document_frequency)
            } else {
                -1
            };

            let (metadata, next) = self.read_metadata(
                metadata_at,
                document_frequency,
                total_term_frequency,
                (document_start, position_start, payload_start),
            );
            metadata_at = next;
            document_start = metadata.document_start;
            position_start = metadata.position_start;
            payload_start = metadata.payload_start;

            self.decoded.push(Given {
                term: full,
                document_frequency,
                total_term_frequency,
                metadata,
            });
        }
        assert_eq!(suffix_at, suffix_end, "the suffix section came out uneven");
        assert_eq!(
            statistics_at, statistics_end,
            "the statistics section came out uneven"
        );
        assert_eq!(metadata_at, end, "the metadata section came out uneven");

        BlockInfo {
            end,
            has_terms,
            is_last_in_floor,
            lead_byte,
        }
    }
}

impl Walk<'_> {
    /// One term's metadata entry: `longsSize` pointer deltas, then the
    /// bytes whose very shape the reader derives from the statistics it
    /// has already read (§6.4).
    fn read_metadata(
        &self,
        at: usize,
        document_frequency: i32,
        total_term_frequency: i64,
        running: (u64, u64, u64),
    ) -> (TermMetadata, usize) {
        let (mut document_start, mut position_start, mut payload_start) = running;
        let (delta, mut cursor) = read_vlong(self.terms, at);
        document_start += delta;
        if self.long_count > 1 {
            let (delta, next) = read_vlong(self.terms, cursor);
            position_start += delta;
            cursor = next;
        }
        if self.long_count > 2 {
            let (delta, next) = read_vlong(self.terms, cursor);
            payload_start += delta;
            cursor = next;
        }
        let singleton_document = if document_frequency == 1 {
            let (value, next) = read_vint(self.terms, cursor);
            cursor = next;
            Some(value)
        } else {
            None
        };
        let last_position_block_offset =
            if self.options.has_positions() && total_term_frequency > 128 {
                let (value, next) = read_vlong(self.terms, cursor);
                cursor = next;
                Some(value)
            } else {
                None
            };
        let skip_offset = if document_frequency > 128 {
            let (value, next) = read_vlong(self.terms, cursor);
            cursor = next;
            Some(value)
        } else {
            None
        };
        (
            TermMetadata {
                document_start,
                position_start,
                payload_start,
                singleton_document,
                last_position_block_offset,
                skip_offset,
                document_frequency,
                total_term_frequency,
            },
            cursor,
        )
    }
}

fn write_vlong(bytes: &mut Vec<u8>, mut value: u64) {
    while value & !0x7f != 0 {
        bytes.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
}

/// Walks one field and checks everything the walk can see.
fn check_field<'a>(written: &'a Written, options: &[IndexOptions], field: usize) -> Walk<'a> {
    let directory = read_directory(&written.terms, options);
    let entry = &directory[field];
    let options = options[field];
    let (root_output, _) = read_vlong(&entry.root_code, 0);
    let root_pointer = (root_output >> 2) as usize;

    let mut walk = Walk::new(&written.terms, options, entry.long_count as usize);
    walk.walk(root_pointer, Vec::new());

    // A field without positions carries no `.pos` or `.pay` pointer in its
    // metadata — `longsSize` says so, and the decoder cannot see what was
    // never written. The writer still latches them when the segment has
    // those files, so the comparison drops what the format drops.
    let given: Vec<Given> = written.given[field]
        .iter()
        .map(|term| Given {
            term: term.term.clone(),
            document_frequency: term.document_frequency,
            total_term_frequency: term.total_term_frequency,
            metadata: TermMetadata {
                position_start: if entry.long_count > 1 {
                    term.metadata.position_start
                } else {
                    0
                },
                payload_start: if entry.long_count > 2 {
                    term.metadata.payload_start
                } else {
                    0
                },
                ..term.metadata
            },
        })
        .collect();
    assert_eq!(
        walk.decoded, given,
        "the terms that came back are not the terms that went in"
    );
    assert_eq!(entry.term_count as usize, written.given[field].len());

    // The root entry the directory carries is the transducer's empty
    // output, and the transducer is what task 0903's builder makes of every
    // block's key.
    walk.index_pairs.sort();
    assert_eq!(
        walk.index_pairs[0],
        (Vec::new(), entry.root_code.clone()),
        "the root code is the empty key's output"
    );
    let mut builder = FstBuilder::new();
    for (key, output) in &walk.index_pairs {
        builder.add(key, output).expect("add");
    }
    let expected = builder.finish().expect("finish");
    let index_start = read_index_start(&written.index, field);
    let index_end = read_index_start(&written.index, field + 1);
    assert_eq!(
        &written.index[index_start..index_end],
        expected.as_slice(),
        "the .tip transducer is not the one these block keys produce"
    );
    walk
}

/// The `indexStartFP` of field `index`, with one past the last field
/// answering where the transducers end — the directory's own start.
fn read_index_start(index: &[u8], field: usize) -> usize {
    let directory = directory_start(index) as usize;
    let mut at = directory;
    let mut starts = Vec::new();
    while at < index.len() - 8 {
        let (value, next) = read_vlong(index, at);
        starts.push(value as usize);
        at = next;
    }
    starts.push(directory);
    starts[field]
}

const DOCUMENTS: IndexOptions = IndexOptions::Documents;
const OFFSETS: IndexOptions = IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets;

fn field(number: i32, options: IndexOptions, terms: Vec<Vec<u8>>) -> Field {
    Field {
        info: TermsFieldInfo { number, options },
        terms,
    }
}

/// Single-byte terms, `a` onward.
fn letters(count: usize) -> Vec<Vec<u8>> {
    (0..count).map(|index| vec![b'a' + index as u8]).collect()
}

/// Two-byte terms under one shared first byte, so a prefix of length one
/// carries them all and no deeper prefix carries more than one.
fn under_one_prefix(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|index| vec![b'x', b'!' + index as u8])
        .collect()
}

#[test]
fn a_three_term_field_is_one_leaf_block() {
    let written = write_fields(&[field(
        0,
        DOCUMENTS,
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
    )]);

    // `.tim` opens with its own header and then the postings format's,
    // nested inside it (§7.1): 30 bytes and 38.
    let mut expected = Vec::new();
    expected.extend_from_slice(&[0x3f, 0xd7, 0x6c, 0x17]);
    expected.push(21);
    expected.extend_from_slice(b"BLOCK_TREE_TERMS_DICT");
    expected.extend_from_slice(&2i32.to_be_bytes());
    expected.extend_from_slice(&[0x3f, 0xd7, 0x6c, 0x17]);
    expected.push(27);
    expected.extend_from_slice(b"Lucene41PostingsWriterTerms");
    expected.extend_from_slice(&1i32.to_be_bytes());
    expected.extend_from_slice(&[0x80, 0x01]); // the block size, 128
    assert_eq!(expected.len(), 68, "the block starts at 68");

    // The one block: three entries, last in its (non-existent) floor.
    expected.push(0x07); // (3 << 1) | 1
    // A leaf block's suffixes carry no sub-block bit: a length and the
    // bytes, six in all.
    expected.push(0x0d); // (6 << 1) | 1
    expected.extend_from_slice(&[0x01, b'a', 0x01, b'b', 0x01, b'c']);
    // Statistics: the document frequencies 1, 2, 3, and no total term
    // frequency at all, because the field is DOCS_ONLY.
    expected.push(0x03);
    expected.extend_from_slice(&[0x01, 0x02, 0x03]);
    // Metadata, one `longs` entry each because the field has no positions.
    // The first term's pointer is absolute — 67, where `.doc` stands after
    // its header and format table — and the rest are deltas. Only the
    // first term is pulsed, so only it carries a singleton document.
    expected.push(0x04);
    expected.extend_from_slice(&[0x43, 0x00, 0x00, 0x02]);

    // The directory: one field, three terms, the root code, the document
    // frequency sum, the document count and `longsSize`.
    expected.push(0x01); // one field
    expected.push(0x00); // field number
    expected.push(0x03); // numTerms
    expected.push(0x02); // rootCode length
    expected.extend_from_slice(&[0x92, 0x02]); // (68 << 2) | HAS_TERMS
    expected.push(0x06); // sumDocFreq
    expected.push(0x03); // docCount
    expected.push(0x01); // longsSize
    expected.extend_from_slice(&85u64.to_be_bytes());

    assert_eq!(
        written.terms, expected,
        "the .tim of a three-term DOCS_ONLY field"
    );
}

#[test]
fn the_last_eight_bytes_point_at_each_directory() {
    let written = write_fields(&[field(0, DOCUMENTS, letters(3))]);
    for (name, file) in [("terms", &written.terms), ("index", &written.index)] {
        let start = directory_start(file) as usize;
        assert!(
            start < file.len() - 8,
            "{name}: the directory starts inside the file"
        );
        assert!(start > 0, "{name}: and after its header");
    }
    // The terms directory's first byte is the field count.
    let start = directory_start(&written.terms) as usize;
    assert_eq!(written.terms[start], 1);
}

#[test]
fn a_field_with_one_term_round_trips() {
    let written = write_fields(&[field(3, OFFSETS, vec![b"only".to_vec()])]);
    let walk = check_field(&written, &[OFFSETS], 0);
    assert_eq!(walk.blocks, 1);
    assert_eq!(walk.leaf_blocks, 1);
    assert_eq!(walk.floor_runs, 0);
}

#[test]
fn a_field_with_no_term_contributes_nothing() {
    let written = write_fields(&[
        field(0, DOCUMENTS, letters(3)),
        field(1, OFFSETS, Vec::new()),
    ]);
    let directory = read_directory(&written.terms, &[DOCUMENTS]);
    assert_eq!(
        directory.len(),
        1,
        "a field with no term writes no directory entry"
    );
    assert_eq!(directory[0].number, 0);
    // And no transducer: the index holds exactly one field's.
    let start = directory_start(&written.index) as usize;
    let (_, next) = read_vlong(&written.index, start);
    assert_eq!(next, written.index.len() - 8, "one indexStartFP, not two");
    check_field(&written, &[DOCUMENTS], 0);
}

#[test]
fn a_root_block_is_never_floored_however_many_entries_it_holds() {
    // Forty-nine single-byte terms: every one of them is an entry of the
    // root block, which §7.8 short-circuits out of the floor case on its
    // empty prefix alone.
    let written = write_fields(&[field(0, DOCUMENTS, letters(49))]);
    let walk = check_field(&written, &[DOCUMENTS], 0);
    assert_eq!(walk.blocks, 1, "one block");
    assert_eq!(walk.floor_runs, 0, "and not a floored one");
}

#[test]
fn a_prefix_at_the_maximum_is_one_block_and_one_past_it_floors() {
    for (count, expected_blocks, expected_floor_runs) in [(48, 2, 0), (49, 3, 1)] {
        let written = write_fields(&[field(0, DOCUMENTS, under_one_prefix(count))]);
        let walk = check_field(&written, &[DOCUMENTS], 0);
        assert_eq!(
            walk.blocks, expected_blocks,
            "{count} terms under one prefix: the block count"
        );
        assert_eq!(
            walk.floor_runs, expected_floor_runs,
            "{count} terms under one prefix: the floor runs"
        );
        // The root block holds the one sub-block entry and no term, so it
        // is not a leaf.
        assert_eq!(walk.leaf_blocks, expected_blocks - 1);
    }
}

#[test]
fn terms_below_the_minimum_stay_stragglers() {
    // Twenty-four terms under one prefix: the prefix node never reaches the
    // minimum, so it writes nothing and its count rides up to the root.
    let written = write_fields(&[field(0, DOCUMENTS, under_one_prefix(24))]);
    let walk = check_field(&written, &[DOCUMENTS], 0);
    assert_eq!(walk.blocks, 1, "one root block holding all twenty-four");
    assert_eq!(walk.leaf_blocks, 1);
}

#[test]
fn nested_prefixes_become_nested_blocks() {
    let mut terms = Vec::new();
    for first in *b"abc" {
        for second in 0..30u8 {
            terms.push(vec![b'p', first, b'!' + second]);
        }
    }
    let written = write_fields(&[field(0, OFFSETS, terms)]);
    let walk = check_field(&written, &[OFFSETS], 0);
    // Each of the three three-byte prefixes reaches the minimum on its own
    // and becomes a block; the root then holds those three sub-blocks.
    assert_eq!(walk.blocks, 4);
    assert_eq!(walk.leaf_blocks, 3);
    assert_eq!(walk.floor_runs, 0);
}

#[test]
fn a_docs_only_field_beside_one_with_positions_and_offsets() {
    let written = write_fields(&[
        field(0, DOCUMENTS, letters(5)),
        field(7, OFFSETS, letters(5)),
    ]);
    let options = [DOCUMENTS, OFFSETS];
    let directory = read_directory(&written.terms, &options);
    assert_eq!(directory.len(), 2);

    assert_eq!(directory[0].long_count, 1, "no positions: one file pointer");
    assert_eq!(
        directory[0].sum_total_term_frequency, None,
        "a DOCS_ONLY field omits sumTotalTermFreq rather than writing zero"
    );
    assert_eq!(directory[0].sum_document_frequency, 1 + 2 + 3 + 1 + 2);
    assert_eq!(
        directory[0].document_count, written.statistics[0].document_count,
        "the document count is the field's own, not a sum over its terms"
    );

    assert_eq!(
        directory[1].long_count, 3,
        "positions and offsets: three file pointers"
    );
    assert_eq!(directory[1].number, 7);
    assert_eq!(
        directory[1].sum_total_term_frequency,
        Some(written.statistics[1].sum_total_term_frequency)
    );

    check_field(&written, &options, 0);
    check_field(&written, &options, 1);
}

#[test]
fn terms_out_of_order_are_refused() {
    let mut postings = PostingsWriter::new(
        SegmentShape {
            document_count: 4,
            any_field_has_positions: false,
            any_field_has_offsets: false,
        },
        Vec::new(),
        None,
        None,
    )
    .expect("open");
    let mut writer = TermsWriter::new(Vec::new(), Vec::new()).expect("open");
    writer
        .start_field(
            TermsFieldInfo {
                number: 0,
                options: DOCUMENTS,
            },
            &mut postings,
        )
        .expect("start the field");
    postings.start_term();
    postings.start_document(0, 1).expect("document");
    postings.finish_document();
    let metadata = postings.finish_term().expect("finish");
    let statistics = TermStatistics {
        document_frequency: 1,
        total_term_frequency: -1,
    };
    writer.add_term(b"b", statistics, metadata).expect("first");
    let refusal = writer
        .add_term(b"a", statistics, metadata)
        .expect_err("a descending term");
    assert!(
        refusal.to_string().contains("increasing byte order"),
        "{refusal}"
    );
}

#[test]
fn a_wide_prefix_floors_into_several_blocks() {
    // Two hundred terms under one prefix, each with its own second byte.
    // The segmenter cuts greedily at the minimum, seven times, and the
    // remainder of twenty-five then fits one last block.
    let written = write_fields(&[field(0, DOCUMENTS, under_one_prefix(200))]);
    let walk = check_field(&written, &[DOCUMENTS], 0);
    assert_eq!(walk.floor_runs, 1);
    assert_eq!(walk.blocks, 9, "eight floor blocks and the root");
    assert_eq!(walk.leaf_blocks, 8);
}

#[test]
fn a_term_equal_to_its_floored_prefix_leads_the_first_block() {
    // The one entry whose suffix is empty, which §7.8's grouping labels
    // `-1` for real rather than by the quirk — and which keeps the floor
    // head keyed at the plain prefix.
    let mut terms = vec![b"x".to_vec()];
    terms.extend(under_one_prefix(49));
    let written = write_fields(&[field(0, DOCUMENTS, terms)]);
    let walk = check_field(&written, &[DOCUMENTS], 0);
    assert_eq!(walk.floor_runs, 1);
    assert_eq!(walk.blocks, 3, "two floor blocks and the root");
    // The floor head's `.tip` key is the prefix itself, not the prefix plus
    // a lead byte.
    let keys: Vec<&[u8]> = walk
        .index_pairs
        .iter()
        .map(|(key, _)| key.as_slice())
        .collect();
    assert_eq!(keys, vec![&b""[..], &b"x"[..]]);
}

#[test]
fn floor_blocks_can_hold_sub_blocks() {
    // Fifty prefixes of twenty-five terms each: every one of them reaches
    // the minimum and becomes a sub-block, and the fifty sub-blocks then
    // exceed the maximum under their shared first byte — so the floor
    // blocks hold sub-blocks and no term of their own.
    let mut terms = Vec::new();
    for second in 0..50u8 {
        for third in 0..25u8 {
            terms.push(vec![b'x', b'!' + second, b'!' + third]);
        }
    }
    let written = write_fields(&[field(0, DOCUMENTS, terms)]);
    let walk = check_field(&written, &[DOCUMENTS], 0);
    assert_eq!(walk.floor_runs, 1);
    assert_eq!(
        walk.blocks, 53,
        "fifty sub-blocks, two floor blocks, the root"
    );
    assert_eq!(walk.leaf_blocks, 50, "only the sub-blocks hold terms");

    // A floor block with no term of its own clears `OUTPUT_FLAG_HAS_TERMS`
    // in the `.tip` payload; the decoder rebuilt that payload from what it
    // walked, and the transducer comparison in `check_field` already
    // required it to match.
    let (_, head_output) = walk
        .index_pairs
        .iter()
        .find(|(key, _)| key == b"x")
        .expect("the floor head is keyed at its prefix");
    let (encoded, _) = read_vlong(head_output, 0);
    assert_eq!(encoded & 0x2, 0, "the floor head holds no term");
    assert_eq!(encoded & 0x1, 0x1, "and says it is floored");
}
