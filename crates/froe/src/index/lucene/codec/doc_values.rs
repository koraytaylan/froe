//! Doc values: `.dvm` and `.dvd`.
//!
//! `docs/analysis/lucene-4-7-codec.md` §8.1, from
//! `codecs/lucene45/Lucene45DocValuesConsumer.java` and its format.
//!
//! Three types, which are the three Oak's document maker uses:
//!
//! * **`NUMERIC`** — the `:dv<name>` longs, dates and doubles. A double doc
//!   value is the **raw bits of the double**, not the sortable-long
//!   encoding of the indexed term, and the two differ for every negative
//!   double, for `-0.0` and for `NaN`. (`:depth` is an indexed integer
//!   term, not a doc value.)
//! * **`SORTED`** — ordered strings and booleans: a dictionary and one
//!   ordinal per document.
//! * **`SORTED_SET`** — facets: a dictionary and any number of ordinals per
//!   document.
//!
//! # Three passes over one input
//!
//! The numeric branch measures its values before it writes them, and writes
//! a missing bitset between the two, so every source arrives as a
//! [`SortedPasses`] and is walked through `pass()` as many times as the
//! branch needs. Nothing is materialized: a stream longer than one
//! 16,384-value block is written a block at a time.

use std::io::Write;

use crate::error::{Error, Result};
use crate::external_sort::{SortedPasses, SpillRecord};
use crate::index::lucene::codec::data_output::CodecOutput;
use crate::index::lucene::codec::packed::{
    PACKED_VERSION_CURRENT, bits_required, write_block_packed, write_monotonic_block_packed,
    write_packed,
};

/// `Lucene45DocValuesFormat.DATA_CODEC`.
const DATA_CODEC: &str = "Lucene45DocValuesData";

/// `Lucene45DocValuesFormat.META_CODEC`.
const META_CODEC: &str = "Lucene45ValuesMetadata";

/// `VERSION_SORTED_SET_SINGLE_VALUE_OPTIMIZED`.
const VERSION_CURRENT: i32 = 1;

const NUMERIC: u8 = 0;
const BINARY: u8 = 1;
const SORTED: u8 = 2;
const SORTED_SET: u8 = 3;

/// `Lucene45DocValuesConsumer.BLOCK_SIZE`.
pub const BLOCK_SIZE: usize = 16384;

/// `Lucene45DocValuesConsumer.ADDRESS_INTERVAL`.
pub const ADDRESS_INTERVAL: usize = 16;

const DELTA_COMPRESSED: i32 = 0;
const GCD_COMPRESSED: i32 = 1;
const TABLE_COMPRESSED: i32 = 2;

const BINARY_FIXED_UNCOMPRESSED: i32 = 0;
// `BINARY_VARIABLE_UNCOMPRESSED`, 1, is what a `BINARY` doc-values field of
// varying lengths takes. Oak writes no `BINARY` field, and the dictionary
// path reaches the fixed form or the prefix-compressed one and never this,
// so froe has no constant for it.
const BINARY_PREFIX_COMPRESSED: i32 = 2;

const SORTED_SET_WITH_ADDRESSES: i32 = 0;
const SORTED_SET_SINGLE_VALUED_SORTED: i32 = 1;

/// `Lucene45DocValuesConsumer.MISSING_ORD`.
pub const MISSING_ORD: i64 = -1;

/// How many distinct values the table branch tolerates.
const TABLE_LIMIT: usize = 256;

/// One document's numeric doc value, in document order.
///
/// The stream is **dense**: one record per document of the segment, with
/// `None` where the document has no value. The reader addresses both the
/// values and the missing bitset by document number, so a sparse stream
/// would shift every later document's value.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct NumericRecord {
    /// The document, which is also the sort key.
    pub document: i32,
    /// The value, or `None` where the document has none.
    pub value: Option<i64>,
}

/// One entry of a `SORTED` or `SORTED_SET` dictionary, in value order.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct DictionaryRecord {
    /// The ordinal, which is also the sort key — the dictionary is in
    /// ascending value order, so the two agree.
    pub ordinal: i64,
    /// The value.
    pub value: Vec<u8>,
}

/// One document's ordinal in a `SORTED` field, in document order.
///
/// Dense, like [`NumericRecord`], and [`MISSING_ORD`] where the document
/// has no value — the ordinal stream goes through the numeric path with
/// storage optimization off, which has no missing bitset to put it in.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct OrdinalRecord {
    /// The document, which is also the sort key.
    pub document: i32,
    /// The ordinal, or [`MISSING_ORD`].
    pub ordinal: i64,
}

/// One (document, ordinal) pair of a `SORTED_SET` field.
///
/// Sparse: a document contributes one record per ordinal it carries and
/// none at all when it carries none. Sorted by document and then by
/// ordinal, which is the order the flat `ords` stream needs.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SetOrdinalRecord {
    /// The document.
    pub document: i32,
    /// One ordinal it carries.
    pub ordinal: i64,
}

macro_rules! numeric_spill_record {
    ($record:ident, $first:ident, $second:ident) => {
        impl SpillRecord for $record {
            fn encode(&self, buffer: &mut Vec<u8>) {
                buffer.extend_from_slice(&self.$first.to_be_bytes());
                buffer.extend_from_slice(&self.$second.to_be_bytes());
            }

            fn decode(bytes: &[u8]) -> Result<Self> {
                if bytes.len() != 12 {
                    return Err(Error::InvalidFormat {
                        details: format!("a doc-value record is twelve bytes, not {}", bytes.len()),
                    });
                }
                Ok(Self {
                    $first: i32::from_be_bytes(bytes[..4].try_into().expect("four bytes")),
                    $second: i64::from_be_bytes(bytes[4..].try_into().expect("eight bytes")),
                })
            }

            fn resident_size(&self) -> usize {
                12
            }
        }
    };
}

numeric_spill_record!(OrdinalRecord, document, ordinal);
numeric_spill_record!(SetOrdinalRecord, document, ordinal);

impl SpillRecord for NumericRecord {
    fn encode(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(&self.document.to_be_bytes());
        buffer.push(u8::from(self.value.is_some()));
        buffer.extend_from_slice(&self.value.unwrap_or(0).to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 13 {
            return Err(Error::InvalidFormat {
                details: format!("a numeric record is thirteen bytes, not {}", bytes.len()),
            });
        }
        let value = i64::from_be_bytes(bytes[5..].try_into().expect("eight bytes"));
        Ok(Self {
            document: i32::from_be_bytes(bytes[..4].try_into().expect("four bytes")),
            value: (bytes[4] == 1).then_some(value),
        })
    }

    fn resident_size(&self) -> usize {
        13
    }
}

impl SpillRecord for DictionaryRecord {
    fn encode(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(&self.ordinal.to_be_bytes());
        buffer.extend_from_slice(&self.value);
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 8 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a dictionary record is at least eight bytes, not {}",
                    bytes.len()
                ),
            });
        }
        Ok(Self {
            ordinal: i64::from_be_bytes(bytes[..8].try_into().expect("eight bytes")),
            value: bytes[8..].to_vec(),
        })
    }

    fn resident_size(&self) -> usize {
        8 + self.value.len()
    }
}

/// The two files, once every field is written.
#[derive(Debug)]
pub struct DocValuesFiles<Sink> {
    /// `.dvd`.
    pub data: Sink,
    /// `.dvm`.
    pub metadata: Sink,
}

/// A stream of values the numeric branch can walk more than once.
///
/// The branch measures before it writes and puts a missing bitset between
/// the two, so one walk is never enough.
trait NumericStream {
    fn walk(&mut self, visit: &mut dyn FnMut(Option<i64>) -> Result<()>) -> Result<()>;
}

/// A sorted sequence read through a projection.
struct MappedStream<'source, Record: SpillRecord, Map> {
    source: &'source mut SortedPasses<Record>,
    map: Map,
}

impl<Record: SpillRecord, Map: FnMut(&Record) -> Option<i64>> NumericStream
    for MappedStream<'_, Record, Map>
{
    fn walk(&mut self, visit: &mut dyn FnMut(Option<i64>) -> Result<()>) -> Result<()> {
        for record in self.source.pass()? {
            let record = record?;
            visit((self.map)(&record))?;
        }
        Ok(())
    }
}

/// A sorted set's ordinals, one per document, for the single-valued form.
struct SingleOrdinalStream<'source> {
    source: &'source mut SortedPasses<SetOrdinalRecord>,
    document_count: i64,
}

impl NumericStream for SingleOrdinalStream<'_> {
    fn walk(&mut self, visit: &mut dyn FnMut(Option<i64>) -> Result<()>) -> Result<()> {
        let mut document = 0i64;
        for record in self.source.pass()? {
            let record = record?;
            let at = i64::from(record.document);
            while document < at {
                visit(Some(MISSING_ORD))?;
                document += 1;
            }
            visit(Some(record.ordinal))?;
            document += 1;
        }
        while document < self.document_count {
            visit(Some(MISSING_ORD))?;
            document += 1;
        }
        Ok(())
    }
}

/// `MathUtil.gcd`, the binary algorithm, on magnitudes.
fn greatest_common_divisor(first: i64, second: i64) -> i64 {
    let mut left = first.unsigned_abs();
    let mut right = second.unsigned_abs();
    if left == 0 {
        return right as i64;
    }
    if right == 0 {
        return left as i64;
    }
    let common = (left | right).trailing_zeros();
    left >>= left.trailing_zeros();
    loop {
        right >>= right.trailing_zeros();
        if left == right {
            break;
        }
        if left > right {
            std::mem::swap(&mut left, &mut right);
        }
        right -= left;
    }
    (left << common) as i64
}

/// `StringHelper.bytesDifference`: how many leading bytes two values share.
fn common_prefix_length(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(first, second)| first == second)
        .count()
}

/// Writes a stream as block-packed blocks of [`BLOCK_SIZE`].
///
/// `AbstractBlockPackedWriter.finish` flushes whatever is left and writes
/// no trailer, so a stream shorter than one block is one block and an empty
/// stream is nothing at all.
fn write_block_packed_stream<Sink: Write>(
    data: &mut CodecOutput<Sink>,
    stream: &mut dyn NumericStream,
    transform: &dyn Fn(i64) -> i64,
) -> Result<()> {
    let mut block: Vec<i64> = Vec::with_capacity(BLOCK_SIZE);
    let mut failure = None;
    stream.walk(&mut |value| {
        block.push(transform(value.unwrap_or(0)));
        if block.len() == BLOCK_SIZE {
            if let Err(error) = write_block_packed(data, &block) {
                failure = Some(error);
            }
            block.clear();
        }
        Ok(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    if !block.is_empty() {
        write_block_packed(data, &block)?;
    }
    Ok(())
}

/// Writes a monotonic stream as blocks of [`BLOCK_SIZE`].
fn write_monotonic_blocks<Sink: Write>(data: &mut CodecOutput<Sink>, values: &[i64]) -> Result<()> {
    for block in values.chunks(BLOCK_SIZE) {
        write_monotonic_block_packed(data, block)?;
    }
    Ok(())
}

/// Writes a stream as header-less packed values at `bits`.
///
/// In groups of eight, which is exact: eight values at any width occupy
/// `bits` whole bytes, so the groups concatenate into the one contiguous
/// bit stream `PackedInts.getWriterNoHeader` would have written, and the
/// final short group pads its last byte the same way.
fn write_packed_stream<Sink: Write>(
    data: &mut CodecOutput<Sink>,
    stream: &mut dyn NumericStream,
    encode: &dyn Fn(i64) -> Result<u64>,
    bits: u32,
) -> Result<()> {
    let mut group: Vec<u64> = Vec::with_capacity(8);
    let mut failure = None;
    stream.walk(&mut |value| {
        group.push(encode(value.unwrap_or(0))?);
        if group.len() == 8 {
            if let Err(error) = write_packed(data, &group, bits) {
                failure = Some(error);
            }
            group.clear();
        }
        Ok(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    if !group.is_empty() {
        write_packed(data, &group, bits)?;
    }
    Ok(())
}

/// The `Lucene45` doc-values consumer.
///
/// One per segment. Fields are added in the order the metadata will list
/// them, and [`Self::finish`] closes `.dvm` with the `-1` end-of-fields
/// marker without which a reader consumes whatever follows as another
/// field number.
pub struct DocValuesConsumer<Sink: Write> {
    data: CodecOutput<Sink>,
    metadata: CodecOutput<Sink>,
    document_count: i64,
}

impl<Sink: Write> DocValuesConsumer<Sink> {
    /// Opens both files and writes their headers.
    ///
    /// `document_count` is the segment's, which the sorted-set addresses
    /// entry writes as its own `maxDoc`, and which every dense stream's
    /// length must match.
    pub fn new(data: Sink, metadata: Sink, document_count: i64) -> Result<Self> {
        let mut data = CodecOutput::new(data);
        let mut metadata = CodecOutput::new(metadata);
        data.write_header(DATA_CODEC, VERSION_CURRENT)?;
        metadata.write_header(META_CODEC, VERSION_CURRENT)?;
        Ok(Self {
            data,
            metadata,
            document_count,
        })
    }

    /// A `NUMERIC` field, one value per document.
    pub fn add_numeric(
        &mut self,
        field_number: i32,
        values: &mut SortedPasses<NumericRecord>,
    ) -> Result<()> {
        let mut expected = 0i32;
        for record in values.pass()? {
            let record = record?;
            if record.document != expected {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "a numeric doc-value stream is dense and in document order: document \
                         {expected} was expected and {} came",
                        record.document
                    ),
                });
            }
            expected += 1;
        }
        if i64::from(expected) != self.document_count {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a numeric doc-value stream holds one value per document: {expected} for a \
                     segment of {} documents",
                    self.document_count
                ),
            });
        }
        let mut stream = MappedStream {
            source: values,
            map: |record: &NumericRecord| record.value,
        };
        self.write_numeric_entry(field_number, true, &mut stream)
    }

    /// A `SORTED` field: a dictionary in value order, and one ordinal per
    /// document with [`MISSING_ORD`] where a document has no value.
    pub fn add_sorted(
        &mut self,
        field_number: i32,
        values: &mut SortedPasses<DictionaryRecord>,
        ordinals: &mut SortedPasses<OrdinalRecord>,
    ) -> Result<()> {
        let mut stream = MappedStream {
            source: ordinals,
            map: |record: &OrdinalRecord| Some(record.ordinal),
        };
        self.write_sorted(field_number, values, &mut stream)
    }

    /// A `SORTED_SET` field: a dictionary, and the ordinals each document
    /// carries — none, one, or several.
    pub fn add_sorted_set(
        &mut self,
        field_number: i32,
        values: &mut SortedPasses<DictionaryRecord>,
        document_ordinals: &mut SortedPasses<SetOrdinalRecord>,
    ) -> Result<()> {
        self.metadata.write_vint(field_number)?;
        self.metadata.write_byte(SORTED_SET)?;

        // "No document carries **more than one**" — a set where some
        // documents carry none and the rest exactly one is still single
        // valued, and takes the optimized form with MISSING_ORD for the
        // empty ones.
        let mut single_valued = true;
        Self::walk_counts(document_ordinals, self.document_count, &mut |count| {
            if count > 1 {
                single_valued = false;
            }
            Ok(())
        })?;

        if single_valued {
            self.metadata.write_vint(SORTED_SET_SINGLE_VALUED_SORTED)?;
            let mut stream = SingleOrdinalStream {
                source: document_ordinals,
                document_count: self.document_count,
            };
            return self.write_sorted(field_number, values, &mut stream);
        }

        self.metadata.write_vint(SORTED_SET_WITH_ADDRESSES)?;
        self.add_terms_dictionary(field_number, values)?;

        // The flat stream of ordinals, whose count is the number of
        // ordinals and not the number of documents.
        let mut stream = MappedStream {
            source: document_ordinals,
            map: |record: &SetOrdinalRecord| Some(record.ordinal),
        };
        self.write_numeric_entry(field_number, false, &mut stream)?;

        // The doc-to-ordinal index. Its metadata is hard-coded and its
        // declared format is **inert**: it says `DELTA_COMPRESSED` and its
        // count is `maxDoc` rather than the stream's own length, but the
        // payload below is a *monotonic* cumulative sum and the reader
        // decodes it monotonically without consulting the declaration.
        self.metadata.write_vint(field_number)?;
        self.metadata.write_byte(NUMERIC)?;
        self.metadata.write_vint(DELTA_COMPRESSED)?;
        self.metadata.write_long(-1)?;
        self.metadata.write_vint(PACKED_VERSION_CURRENT)?;
        self.metadata.write_long(self.data.position() as i64)?;
        self.metadata.write_vlong(self.document_count)?;
        self.metadata.write_vint(BLOCK_SIZE as i32)?;

        let mut addresses: Vec<i64> = Vec::new();
        let mut running = 0i64;
        let mut failure = None;
        Self::walk_counts(document_ordinals, self.document_count, &mut |count| {
            running += count;
            addresses.push(running);
            if addresses.len() == BLOCK_SIZE {
                if let Err(error) = write_monotonic_block_packed(&mut self.data, &addresses) {
                    failure = Some(error);
                }
                addresses.clear();
            }
            Ok(())
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        if !addresses.is_empty() {
            write_monotonic_block_packed(&mut self.data, &addresses)?;
        }
        Ok(())
    }

    /// Writes `.dvm`'s end-of-fields marker and hands back both files.
    pub fn finish(mut self) -> Result<DocValuesFiles<Sink>> {
        // A `-1` vint is the five-byte form of §1.1, and a reader that does
        // not find it consumes whatever follows as another field number.
        self.metadata.write_vint(-1)?;
        Ok(DocValuesFiles {
            data: self.data.into_inner(),
            metadata: self.metadata.into_inner(),
        })
    }

    /// One count per document of the segment, zero for a document that
    /// carries no ordinal.
    fn walk_counts(
        source: &mut SortedPasses<SetOrdinalRecord>,
        document_count: i64,
        visit: &mut dyn FnMut(i64) -> Result<()>,
    ) -> Result<()> {
        let mut document = 0i64;
        let mut count = 0i64;
        for record in source.pass()? {
            let record = record?;
            let at = i64::from(record.document);
            if at < document || at >= document_count {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "a sorted-set stream is in document order and inside the segment: \
                         document {at} follows {document} of {document_count}"
                    ),
                });
            }
            while document < at {
                visit(count)?;
                count = 0;
                document += 1;
            }
            count += 1;
        }
        while document < document_count {
            visit(count)?;
            count = 0;
            document += 1;
        }
        Ok(())
    }

    /// `addSortedField`: the type byte, the dictionary, then the ordinals
    /// through the numeric path **with storage optimization off** — which
    /// is why they are always `DELTA_COMPRESSED` and carry no missing
    /// bitset. All three entries repeat the same field number.
    fn write_sorted(
        &mut self,
        field_number: i32,
        values: &mut SortedPasses<DictionaryRecord>,
        ordinals: &mut dyn NumericStream,
    ) -> Result<()> {
        self.metadata.write_vint(field_number)?;
        self.metadata.write_byte(SORTED)?;
        self.add_terms_dictionary(field_number, values)?;
        self.write_numeric_entry(field_number, false, ordinals)
    }
}

impl<Sink: Write> DocValuesConsumer<Sink> {
    /// `addNumericField` (§8.1.1): the statistics pass, the format
    /// decision, the missing bitset and the payload.
    fn write_numeric_entry(
        &mut self,
        field_number: i32,
        optimize_storage: bool,
        stream: &mut dyn NumericStream,
    ) -> Result<()> {
        let statistics = Self::measure(optimize_storage, stream)?;
        let format = statistics.format();

        self.metadata.write_vint(field_number)?;
        self.metadata.write_byte(NUMERIC)?;
        self.metadata.write_vint(format)?;
        if statistics.missing {
            // The bitset goes into `.dvd` between two `.dvm` positions, so
            // the data pointer below is read after it has moved.
            self.metadata.write_long(self.data.position() as i64)?;
            Self::write_missing_bitset(&mut self.data, stream)?;
        } else {
            self.metadata.write_long(-1)?;
        }
        self.metadata.write_vint(PACKED_VERSION_CURRENT)?;
        self.metadata.write_long(self.data.position() as i64)?;
        self.metadata.write_vlong(statistics.count as i64)?;
        self.metadata.write_vint(BLOCK_SIZE as i32)?;

        match format {
            GCD_COMPRESSED => {
                self.metadata.write_long(statistics.minimum)?;
                self.metadata.write_long(statistics.divisor)?;
                let minimum = statistics.minimum;
                let divisor = statistics.divisor;
                write_block_packed_stream(&mut self.data, stream, &move |value| {
                    (value - minimum) / divisor
                })
            }
            TABLE_COMPRESSED => {
                let table: Vec<i64> = statistics.unique.clone().unwrap_or_default();
                self.metadata.write_vint(table.len() as i32)?;
                for value in &table {
                    self.metadata.write_long(*value)?;
                }
                let bits = bits_required((table.len() as u64).wrapping_sub(1));
                let lookup = table;
                write_packed_stream(
                    &mut self.data,
                    stream,
                    &move |value| {
                        lookup
                            .binary_search(&value)
                            .map(|at| at as u64)
                            .map_err(|_| Error::InvalidFormat {
                                details: format!(
                                    "{value} is not in the table the statistics pass built; \
                                     the two passes saw different values"
                                ),
                            })
                    },
                    bits,
                )
            }
            _ => write_block_packed_stream(&mut self.data, stream, &|value| value),
        }
    }

    /// The statistics pass, which runs in full only under storage
    /// optimization — without it the branch needs nothing but the count.
    fn measure(optimize_storage: bool, stream: &mut dyn NumericStream) -> Result<Statistics> {
        let mut statistics = Statistics::default();
        if !optimize_storage {
            stream.walk(&mut |_| {
                statistics.count += 1;
                Ok(())
            })?;
            return Ok(statistics);
        }
        let mut unique = Some(Vec::<i64>::new());
        let mut disable_table = false;
        stream.walk(&mut |value| {
            // A missing value counts as **zero** here and in every payload
            // branch, so it takes part in the minimum, the divisor and the
            // table like any other.
            let value = value.unwrap_or_else(|| {
                statistics.missing = true;
                0
            });
            if statistics.divisor != 1 {
                if !(i64::MIN / 2..=i64::MAX / 2).contains(&value) {
                    // One extreme value disables the divisor for the whole
                    // field, because `value - minimum` could overflow and
                    // make the arithmetic lie.
                    statistics.divisor = 1;
                } else if statistics.count != 0 {
                    // Against the **running** minimum, not the final one.
                    statistics.divisor =
                        greatest_common_divisor(statistics.divisor, value - statistics.minimum);
                }
            }
            statistics.minimum = statistics.minimum.min(value);
            statistics.maximum = statistics.maximum.max(value);
            if let Some(values) = unique.as_mut()
                && let Err(at) = values.binary_search(&value)
            {
                values.insert(at, value);
                disable_table = values.len() > TABLE_LIMIT;
            }
            if disable_table {
                unique = None;
            }
            statistics.count += 1;
            Ok(())
        })?;
        statistics.unique = unique;
        Ok(statistics)
    }

    /// `writeMissingBitset`: one bit per document, least significant first,
    /// **set** where the document has the field.
    fn write_missing_bitset(
        data: &mut CodecOutput<Sink>,
        stream: &mut dyn NumericStream,
    ) -> Result<()> {
        let mut bits = 0u8;
        let mut count = 0usize;
        let mut failure = None;
        stream.walk(&mut |value| {
            if count == 8 {
                if let Err(error) = data.write_byte(bits) {
                    failure = Some(error);
                }
                count = 0;
                bits = 0;
            }
            if value.is_some() {
                bits |= 1 << (count & 7);
            }
            count += 1;
            Ok(())
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        if count > 0 {
            data.write_byte(bits)?;
        }
        Ok(())
    }

    /// `addTermsDict`: a plain binary field when every value has one
    /// length, prefix-compressed otherwise.
    fn add_terms_dictionary(
        &mut self,
        field_number: i32,
        values: &mut SortedPasses<DictionaryRecord>,
    ) -> Result<()> {
        let mut minimum = i32::MAX;
        let mut maximum = i32::MIN;
        for record in values.pass()? {
            let length = record?.value.len() as i32;
            minimum = minimum.min(length);
            maximum = maximum.max(length);
        }
        if minimum == maximum {
            return self.add_fixed_length_dictionary(field_number, values, minimum);
        }
        self.add_prefix_compressed_dictionary(field_number, values, minimum, maximum)
    }

    /// `addBinaryField` for the fixed-length case, which is the only one
    /// `addTermsDict` reaches — it delegates here exactly when every value
    /// has the same length, and the addresses are then implicit.
    ///
    /// Lucene's method also has a variable-length branch with a monotonic
    /// address stream, for a `BINARY` doc-values field. **Oak writes none**,
    /// so froe refuses that shape rather than carrying a branch no test can
    /// reach. `addBinaryField` writes its own field number and type byte,
    /// which is why a `SORTED` field's metadata carries the number three
    /// times.
    fn add_fixed_length_dictionary(
        &mut self,
        field_number: i32,
        values: &mut SortedPasses<DictionaryRecord>,
        length: i32,
    ) -> Result<()> {
        self.metadata.write_vint(field_number)?;
        self.metadata.write_byte(BINARY)?;
        let start = self.data.position();
        let mut count = 0u64;
        for record in values.pass()? {
            let record = record?;
            self.data.write_bytes(&record.value)?;
            count += 1;
        }
        self.metadata.write_vint(BINARY_FIXED_UNCOMPRESSED)?;
        // A dictionary has no missing values.
        self.metadata.write_long(-1)?;
        self.metadata.write_vint(length)?;
        self.metadata.write_vint(length)?;
        self.metadata.write_vlong(count as i64)?;
        self.metadata.write_long(start as i64)?;
        Ok(())
    }

    /// The `BINARY_PREFIX_COMPRESSED` dictionary: every sixteenth term
    /// absolute, its offset in a monotonic stream appended after the terms.
    fn add_prefix_compressed_dictionary(
        &mut self,
        field_number: i32,
        values: &mut SortedPasses<DictionaryRecord>,
        minimum: i32,
        maximum: i32,
    ) -> Result<()> {
        self.metadata.write_vint(field_number)?;
        self.metadata.write_byte(BINARY)?;
        self.metadata.write_vint(BINARY_PREFIX_COMPRESSED)?;
        self.metadata.write_long(-1)?;

        let start = self.data.position();
        let mut addresses: Vec<i64> = Vec::new();
        let mut last: Vec<u8> = Vec::new();
        let mut count = 0u64;
        for record in values.pass()? {
            let record = record?;
            if count.is_multiple_of(ADDRESS_INTERVAL as u64) {
                addresses.push((self.data.position() - start) as i64);
                // Emptying the running term forces the first of each block
                // to be written whole, so a reader can start there.
                last.clear();
            }
            let shared = common_prefix_length(&last, &record.value);
            self.data.write_vint(shared as i32)?;
            self.data.write_vint((record.value.len() - shared) as i32)?;
            self.data.write_bytes(&record.value[shared..])?;
            last.clear();
            last.extend_from_slice(&record.value);
            count += 1;
        }
        let index_start = self.data.position();
        write_monotonic_blocks(&mut self.data, &addresses)?;

        self.metadata.write_vint(minimum)?;
        self.metadata.write_vint(maximum)?;
        self.metadata.write_vlong(count as i64)?;
        self.metadata.write_long(start as i64)?;
        self.metadata.write_vint(ADDRESS_INTERVAL as i32)?;
        self.metadata.write_long(index_start as i64)?;
        self.metadata.write_vint(PACKED_VERSION_CURRENT)?;
        self.metadata.write_vint(BLOCK_SIZE as i32)?;
        Ok(())
    }
}

/// What the statistics pass found.
struct Statistics {
    count: u64,
    minimum: i64,
    maximum: i64,
    /// The running gcd: `0` before the second value, `1` once it is
    /// disabled.
    divisor: i64,
    missing: bool,
    /// The distinct values in ascending order, or `None` once there are
    /// more than [`TABLE_LIMIT`] of them.
    unique: Option<Vec<i64>>,
}

impl Default for Statistics {
    fn default() -> Self {
        Self {
            count: 0,
            minimum: i64::MAX,
            maximum: i64::MIN,
            divisor: 0,
            missing: false,
            unique: None,
        }
    }
}

impl Statistics {
    /// The format decision of §8.1.1.
    fn format(&self) -> i32 {
        let delta = self.maximum.wrapping_sub(self.minimum);
        if let Some(values) = self.unique.as_ref() {
            // The table needs at most 256 distinct values **and** either a
            // delta that overflowed into the negative — which one negative
            // and one positive double's raw bits produce — or an ordinal
            // narrower than the delta. Ten values 0..9 have four bits
            // either way and land in `DELTA_COMPRESSED`.
            //
            // For an empty stream the subtraction wraps to 64 bits
            // required, which is what Java's `bitsRequired(-1)` gives, and
            // the comparison is false either way.
            let narrower =
                bits_required((values.len() as u64).wrapping_sub(1)) < bits_required(delta as u64);
            if (delta < 0 || narrower) && i32::try_from(self.count).is_ok() {
                return TABLE_COMPRESSED;
            }
        }
        if self.divisor != 0 && self.divisor != 1 {
            GCD_COMPRESSED
        } else {
            DELTA_COMPRESSED
        }
    }
}
