//! The `Lucene45` doc-values consumer, against bytes computed by hand.
//!
//! `docs/analysis/lucene-4-7-codec.md` §8.1. Every `.dvm` below is a whole
//! metadata file, derived from the Java quoted there; the `.dvd` payloads
//! are built from the block-packed and monotonic forms of §2, with an
//! independent bit packer in this file rather than the writer's own.

use froe::SortedPasses;
use froe::index::lucene::codec::doc_values::{
    DictionaryRecord, DocValuesConsumer, MISSING_ORD, NumericRecord, OrdinalRecord,
    SetOrdinalRecord,
};

/// `CodecUtil.writeHeader` at this format's version, 1.
fn header(name: &str) -> Vec<u8> {
    let mut bytes = vec![0x3f, 0xd7, 0x6c, 0x17, name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&1i32.to_be_bytes());
    bytes
}

fn data_header() -> Vec<u8> {
    header("Lucene45DocValuesData")
}

fn metadata_header() -> Vec<u8> {
    header("Lucene45ValuesMetadata")
}

/// Where `.dvd` stands after its header.
const DATA_START: u64 = 30;

/// `BLOCK_SIZE` as a `VInt`: 16,384 is three bytes.
const BLOCK_SIZE_VINT: [u8; 3] = [0x80, 0x80, 0x01];

/// The `-1` end-of-fields marker, which is the five-byte `VInt` form.
const END_OF_FIELDS: [u8; 5] = [0xff, 0xff, 0xff, 0xff, 0x0f];

fn push_vint(bytes: &mut Vec<u8>, mut value: u32) {
    while value & !0x7f != 0 {
        bytes.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
}

fn push_vlong(bytes: &mut Vec<u8>, value: i64) {
    let mut remaining = value as u64;
    let mut written = 0;
    while remaining & !0x7f != 0 && written < 8 {
        bytes.push((remaining & 0x7f) as u8 | 0x80);
        remaining >>= 7;
        written += 1;
    }
    bytes.push(remaining as u8);
}

/// Bit packing, most significant bit first, zero-padded — the same rule as
/// §2.1, written out again so the expectation does not come from the code
/// under test.
fn pack(values: &[u64], bits: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; (values.len() * bits as usize).div_ceil(8)];
    let mut at = 0usize;
    for value in values {
        for offset in (0..bits).rev() {
            if (value >> offset) & 1 == 1 {
                bytes[at / 8] |= 0x80 >> (at % 8);
            }
            at += 1;
        }
    }
    bytes
}

/// One block-packed block, per §2.2.
fn block_packed(values: &[i64]) -> Vec<u8> {
    let minimum = *values.iter().min().expect("a value");
    let maximum = *values.iter().max().expect("a value");
    let delta = maximum.wrapping_sub(minimum);
    let bits = match delta {
        negative if negative < 0 => 64,
        0 => 0,
        positive => 64 - (positive as u64).leading_zeros(),
    };
    let minimum = if bits == 64 {
        0
    } else if minimum > 0 {
        0.max(maximum.wrapping_sub(if bits >= 64 {
            i64::MAX
        } else {
            (1i64 << bits) - 1
        }))
    } else {
        minimum
    };
    let mut bytes = vec![((bits << 1) | u32::from(minimum == 0)) as u8];
    if minimum != 0 {
        let zigzag = (minimum >> 63) ^ (minimum << 1);
        push_vlong(&mut bytes, zigzag.wrapping_sub(1));
    }
    if bits > 0 {
        let shifted: Vec<u64> = values
            .iter()
            .map(|value| value.wrapping_sub(minimum) as u64)
            .collect();
        bytes.extend(pack(&shifted, bits));
    }
    bytes
}

/// One monotonic block, per §2.3: the first value, the average as a float,
/// then the zigzag deltas from the line — or a width of zero when every
/// delta is zero.
#[expect(
    clippy::cast_precision_loss,
    reason = "docs/analysis/lucene-4-7-codec.md §2.3: the average is a float by design, and \
              this is the independent copy of that rule"
)]
fn monotonic(values: &[i64]) -> Vec<u8> {
    let minimum = values[0];
    let count = values.len();
    let average: f32 = if count == 1 {
        0.0
    } else {
        (values[count - 1] - minimum) as f32 / (count - 1) as f32
    };
    let deltas: Vec<i64> = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let expected = (average * index as f32) as i64;
            let delta = value - minimum - expected;
            (delta >> 63) ^ (delta << 1)
        })
        .collect();
    let widest = *deltas.iter().max().expect("a delta");
    let mut bytes = Vec::new();
    push_vlong(&mut bytes, minimum);
    bytes.extend_from_slice(&average.to_bits().to_be_bytes());
    if widest == 0 {
        bytes.push(0x00);
    } else {
        let bits = 64 - (widest as u64).leading_zeros();
        push_vint(&mut bytes, bits);
        let encoded: Vec<u64> = deltas.iter().map(|delta| *delta as u64).collect();
        bytes.extend(pack(&encoded, bits));
    }
    bytes
}

/// A `.dvm` numeric entry's fixed head, up to and including the block size.
struct NumericHead {
    field: u32,
    format: u32,
    missing_offset: i64,
    data_offset: i64,
    count: i64,
}

impl NumericHead {
    fn bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_vint(&mut bytes, self.field);
        bytes.push(0x00); // NUMERIC
        push_vint(&mut bytes, self.format);
        bytes.extend_from_slice(&self.missing_offset.to_be_bytes());
        bytes.push(0x01); // PackedInts.VERSION_CURRENT
        bytes.extend_from_slice(&self.data_offset.to_be_bytes());
        push_vlong(&mut bytes, self.count);
        bytes.extend_from_slice(&BLOCK_SIZE_VINT);
        bytes
    }
}

fn numeric_records(values: &[Option<i64>]) -> SortedPasses<NumericRecord> {
    SortedPasses::from_sorted_records(
        values
            .iter()
            .enumerate()
            .map(|(document, value)| NumericRecord {
                document: document as i32,
                value: *value,
            })
            .collect(),
    )
}

fn dictionary(values: &[&[u8]]) -> SortedPasses<DictionaryRecord> {
    SortedPasses::from_sorted_records(
        values
            .iter()
            .enumerate()
            .map(|(ordinal, value)| DictionaryRecord {
                ordinal: ordinal as i64,
                value: value.to_vec(),
            })
            .collect(),
    )
}

fn ordinals(values: &[i64]) -> SortedPasses<OrdinalRecord> {
    SortedPasses::from_sorted_records(
        values
            .iter()
            .enumerate()
            .map(|(document, ordinal)| OrdinalRecord {
                document: document as i32,
                ordinal: *ordinal,
            })
            .collect(),
    )
}

fn set_ordinals(pairs: &[(i32, i64)]) -> SortedPasses<SetOrdinalRecord> {
    SortedPasses::from_sorted_records(
        pairs
            .iter()
            .map(|(document, ordinal)| SetOrdinalRecord {
                document: *document,
                ordinal: *ordinal,
            })
            .collect(),
    )
}

fn consumer(document_count: i64) -> DocValuesConsumer<Vec<u8>> {
    DocValuesConsumer::new(Vec::new(), Vec::new(), document_count).expect("open")
}

fn assert_files(what: &str, produced: &(Vec<u8>, Vec<u8>), data: &[u8], metadata: &[u8]) {
    assert_eq!(produced.0, data, "{what}: .dvd");
    assert_eq!(produced.1, metadata, "{what}: .dvm");
}

#[test]
fn a_consumer_with_no_field_writes_only_the_end_marker() {
    let files = consumer(0).finish().expect("finish");
    let mut metadata = metadata_header();
    metadata.extend_from_slice(&END_OF_FIELDS);
    assert_files(
        "an empty consumer",
        &(files.data, files.metadata),
        &data_header(),
        &metadata,
    );
}

#[test]
fn ten_values_zero_to_nine_land_in_delta_compressed() {
    // Ten distinct values need four bits for their ordinal and the delta
    // of nine needs four for itself, so the table's second condition —
    // *narrower* than the delta — is not met and the branch is not taken.
    let mut values = numeric_records(&(0..10).map(Some).collect::<Vec<_>>());
    let mut consumer = consumer(10);
    consumer.add_numeric(0, &mut values).expect("the field");
    let files = consumer.finish().expect("finish");

    let mut metadata = metadata_header();
    metadata.extend(
        NumericHead {
            field: 0,
            format: 0,
            missing_offset: -1,
            data_offset: DATA_START as i64,
            count: 10,
        }
        .bytes(),
    );
    metadata.extend_from_slice(&END_OF_FIELDS);

    let mut data = data_header();
    data.extend(block_packed(&(0..10).collect::<Vec<i64>>()));

    assert_files(
        "ten values",
        &(files.data, files.metadata),
        &data,
        &metadata,
    );
}

#[test]
fn a_negative_double_makes_the_delta_overflow_and_takes_the_table() {
    // A doc value is the **raw bits** of the double. One negative and one
    // positive double put the minimum below zero and the maximum above it
    // by more than `i64::MAX`, so `max - min` wraps negative — which is the
    // table branch's first condition.
    let positive = 1.0f64.to_bits() as i64;
    let negative = (-1.0f64).to_bits() as i64;
    assert!(negative < 0 && positive > 0);
    assert!(positive.wrapping_sub(negative) < 0, "the delta overflows");

    let mut values = numeric_records(&[Some(positive), Some(negative)]);
    let mut consumer = consumer(2);
    consumer.add_numeric(0, &mut values).expect("the field");
    let files = consumer.finish().expect("finish");

    let mut metadata = metadata_header();
    metadata.extend(
        NumericHead {
            field: 0,
            format: 2,
            missing_offset: -1,
            data_offset: DATA_START as i64,
            count: 2,
        }
        .bytes(),
    );
    // The table, ascending — froe's order, not the hash order Lucene
    // happens to produce (§10.3).
    push_vint(&mut metadata, 2);
    metadata.extend_from_slice(&negative.to_be_bytes());
    metadata.extend_from_slice(&positive.to_be_bytes());
    metadata.extend_from_slice(&END_OF_FIELDS);

    // One bit per ordinal: the first document holds the positive value,
    // which is ordinal 1.
    let mut data = data_header();
    data.extend(pack(&[1, 0], 1));

    assert_files(
        "a negative double",
        &(files.data, files.metadata),
        &data,
        &metadata,
    );
}

#[test]
fn a_double_of_magnitude_two_bails_the_divisor_out() {
    // `2.0`'s raw bits are `i64::MAX / 2 + 1`, so the very first value puts
    // the divisor beyond recovery and `GCD_COMPRESSED` is unreachable for a
    // double field of that magnitude — whatever common factor the bits
    // share.
    let two = 2.0f64.to_bits() as i64;
    let four = 4.0f64.to_bits() as i64;
    assert!(two > i64::MAX / 2);

    let mut values = numeric_records(&[Some(two), Some(four)]);
    let mut consumer = consumer(2);
    consumer.add_numeric(0, &mut values).expect("the field");
    let files = consumer.finish().expect("finish");

    let mut metadata = metadata_header();
    metadata.extend(
        NumericHead {
            field: 0,
            // The table, not the divisor: two distinct values need one bit
            // and the delta needs fifty-three.
            format: 2,
            missing_offset: -1,
            data_offset: DATA_START as i64,
            count: 2,
        }
        .bytes(),
    );
    push_vint(&mut metadata, 2);
    metadata.extend_from_slice(&two.to_be_bytes());
    metadata.extend_from_slice(&four.to_be_bytes());
    metadata.extend_from_slice(&END_OF_FIELDS);

    let mut data = data_header();
    data.extend(pack(&[0, 1], 1));

    assert_files(
        "a double of magnitude two",
        &(files.data, files.metadata),
        &data,
        &metadata,
    );
}

#[test]
fn a_common_divisor_survives_only_past_the_table_limit() {
    // The table wins for any arithmetic progression of at most 256 terms,
    // because the ordinal is always narrower than the delta. Three hundred
    // even numbers put it out of reach and the divisor branch is taken.
    let values: Vec<i64> = (0..300).map(|index| index * 2).collect();
    let mut records = numeric_records(&values.iter().map(|value| Some(*value)).collect::<Vec<_>>());
    let mut consumer = consumer(300);
    consumer.add_numeric(0, &mut records).expect("the field");
    let files = consumer.finish().expect("finish");

    let mut metadata = metadata_header();
    metadata.extend(
        NumericHead {
            field: 0,
            format: 1,
            missing_offset: -1,
            data_offset: DATA_START as i64,
            count: 300,
        }
        .bytes(),
    );
    metadata.extend_from_slice(&0i64.to_be_bytes()); // the minimum
    metadata.extend_from_slice(&2i64.to_be_bytes()); // the divisor
    metadata.extend_from_slice(&END_OF_FIELDS);

    let mut data = data_header();
    data.extend(block_packed(&(0..300).collect::<Vec<i64>>()));

    assert_files(
        "three hundred even numbers",
        &(files.data, files.metadata),
        &data,
        &metadata,
    );
}

#[test]
fn a_document_without_the_field_gets_a_missing_bitset() {
    let mut values = numeric_records(&[Some(5), None, Some(7)]);
    let mut consumer = consumer(3);
    consumer.add_numeric(0, &mut values).expect("the field");
    let files = consumer.finish().expect("finish");

    let mut metadata = metadata_header();
    metadata.extend(
        NumericHead {
            field: 0,
            format: 2,
            // The bitset goes in first, so the missing offset is where the
            // data stood before it and the data offset is one byte later.
            missing_offset: DATA_START as i64,
            data_offset: DATA_START as i64 + 1,
            count: 3,
        }
        .bytes(),
    );
    // A missing value counts as zero, so the table holds 0, 5 and 7.
    push_vint(&mut metadata, 3);
    for value in [0i64, 5, 7] {
        metadata.extend_from_slice(&value.to_be_bytes());
    }
    metadata.extend_from_slice(&END_OF_FIELDS);

    let mut data = data_header();
    // One bit per document, least significant first, **set** where the
    // document has the field: present, missing, present.
    data.push(0b0000_0101);
    data.extend(pack(&[1, 0, 2], 2));

    assert_files(
        "a sparse field",
        &(files.data, files.metadata),
        &data,
        &metadata,
    );
}

/// The `.dvm` entry a fixed-length dictionary writes — its **own** field
/// number and `BINARY` type byte, which is why a `SORTED` field's metadata
/// carries the number three times.
fn fixed_dictionary_entry(field: u32, length: u32, count: i64, start: i64) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_vint(&mut bytes, field);
    bytes.push(0x01); // BINARY
    push_vint(&mut bytes, 0); // BINARY_FIXED_UNCOMPRESSED
    bytes.extend_from_slice(&(-1i64).to_be_bytes());
    push_vint(&mut bytes, length);
    push_vint(&mut bytes, length);
    push_vlong(&mut bytes, count);
    bytes.extend_from_slice(&start.to_be_bytes());
    bytes
}

#[test]
fn a_sorted_field_whose_values_share_a_length_uses_the_fixed_dictionary() {
    let mut values = dictionary(&[b"aa", b"bb"]);
    let mut order = ordinals(&[0, 1]);
    let mut consumer = consumer(2);
    consumer
        .add_sorted(0, &mut values, &mut order)
        .expect("the field");
    let files = consumer.finish().expect("finish");

    let mut metadata = metadata_header();
    push_vint(&mut metadata, 0);
    metadata.push(0x02); // SORTED
    metadata.extend(fixed_dictionary_entry(0, 2, 2, DATA_START as i64));
    metadata.extend(
        NumericHead {
            field: 0,
            // Always delta-compressed: the ordinal stream goes through the
            // numeric path with storage optimization off, so the statistics
            // pass that would choose otherwise never runs — and the missing
            // offset is a hard -1 for the same reason.
            format: 0,
            missing_offset: -1,
            data_offset: DATA_START as i64 + 4,
            count: 2,
        }
        .bytes(),
    );
    metadata.extend_from_slice(&END_OF_FIELDS);

    let mut data = data_header();
    data.extend_from_slice(b"aabb");
    data.extend(block_packed(&[0, 1]));

    assert_files(
        "a fixed-length dictionary",
        &(files.data, files.metadata),
        &data,
        &metadata,
    );
}

#[test]
fn a_dictionary_of_varying_lengths_is_prefix_compressed() {
    let mut values = dictionary(&[b"a", b"bb"]);
    let mut order = ordinals(&[0, 1]);
    let mut consumer = consumer(2);
    consumer
        .add_sorted(0, &mut values, &mut order)
        .expect("the field");
    let files = consumer.finish().expect("finish");

    // Every sixteenth term is absolute; with two terms only the first is,
    // and the second shares no prefix with it either way.
    let terms: Vec<u8> = vec![0x00, 0x01, b'a', 0x00, 0x02, b'b', b'b'];
    let index_start = DATA_START as i64 + terms.len() as i64;
    let addresses = monotonic(&[0]);

    let mut metadata = metadata_header();
    push_vint(&mut metadata, 0);
    metadata.push(0x02); // SORTED
    push_vint(&mut metadata, 0);
    metadata.push(0x01); // BINARY
    push_vint(&mut metadata, 2); // BINARY_PREFIX_COMPRESSED
    metadata.extend_from_slice(&(-1i64).to_be_bytes());
    push_vint(&mut metadata, 1); // the shortest value
    push_vint(&mut metadata, 2); // the longest
    push_vlong(&mut metadata, 2); // how many
    metadata.extend_from_slice(&DATA_START.to_be_bytes());
    push_vint(&mut metadata, 16); // ADDRESS_INTERVAL
    metadata.extend_from_slice(&index_start.to_be_bytes());
    metadata.push(0x01); // PackedInts.VERSION_CURRENT
    metadata.extend_from_slice(&BLOCK_SIZE_VINT);
    metadata.extend(
        NumericHead {
            field: 0,
            format: 0,
            missing_offset: -1,
            data_offset: index_start + addresses.len() as i64,
            count: 2,
        }
        .bytes(),
    );
    metadata.extend_from_slice(&END_OF_FIELDS);

    let mut data = data_header();
    data.extend_from_slice(&terms);
    data.extend_from_slice(&addresses);
    data.extend(block_packed(&[0, 1]));

    assert_files(
        "a prefix-compressed dictionary",
        &(files.data, files.metadata),
        &data,
        &metadata,
    );
}

#[test]
fn a_sorted_set_no_document_repeats_takes_the_single_valued_form() {
    // Two documents, one ordinal each — and then the same set with an empty
    // document, which is still single valued because no document carries
    // *more than* one.
    for (pairs, expected_ordinals) in [
        (vec![(0, 0i64), (1, 1)], vec![0i64, 1]),
        (vec![(0, 0i64), (2, 1)], vec![0i64, MISSING_ORD, 1]),
    ] {
        let documents = expected_ordinals.len() as i64;
        let mut values = dictionary(&[b"aa", b"bb"]);
        let mut set = set_ordinals(&pairs);
        let mut consumer = consumer(documents);
        consumer
            .add_sorted_set(0, &mut values, &mut set)
            .expect("the field");
        let files = consumer.finish().expect("finish");

        let mut metadata = metadata_header();
        push_vint(&mut metadata, 0);
        metadata.push(0x03); // SORTED_SET
        push_vint(&mut metadata, 1); // SORTED_SET_SINGLE_VALUED_SORTED
        push_vint(&mut metadata, 0);
        metadata.push(0x02); // SORTED
        metadata.extend(fixed_dictionary_entry(0, 2, 2, DATA_START as i64));
        metadata.extend(
            NumericHead {
                field: 0,
                format: 0,
                missing_offset: -1,
                data_offset: DATA_START as i64 + 4,
                count: documents,
            }
            .bytes(),
        );
        metadata.extend_from_slice(&END_OF_FIELDS);

        let mut data = data_header();
        data.extend_from_slice(b"aabb");
        data.extend(block_packed(&expected_ordinals));

        assert_files(
            "a single-valued sorted set",
            &(files.data, files.metadata),
            &data,
            &metadata,
        );
    }
}

#[test]
fn a_document_with_two_ordinals_takes_the_addresses_form() {
    // Three documents: the first carries two ordinals, the second none, the
    // third one. The counts are 2, 0, 1 and the addresses their running sum.
    let mut values = dictionary(&[b"aa", b"bb"]);
    let mut set = set_ordinals(&[(0, 0), (0, 1), (2, 1)]);
    let mut consumer = consumer(3);
    consumer
        .add_sorted_set(0, &mut values, &mut set)
        .expect("the field");
    let files = consumer.finish().expect("finish");

    let flat = block_packed(&[0, 1, 1]);
    let addresses_offset = DATA_START as i64 + 4 + flat.len() as i64;

    let mut metadata = metadata_header();
    push_vint(&mut metadata, 0);
    metadata.push(0x03); // SORTED_SET
    push_vint(&mut metadata, 0); // SORTED_SET_WITH_ADDRESSES
    metadata.extend(fixed_dictionary_entry(0, 2, 2, DATA_START as i64));
    metadata.extend(
        NumericHead {
            field: 0,
            format: 0,
            missing_offset: -1,
            data_offset: DATA_START as i64 + 4,
            // The flat stream's count is the number of **ordinals**, not
            // the number of documents.
            count: 3,
        }
        .bytes(),
    );
    metadata.extend(
        NumericHead {
            field: 0,
            // Declared delta-compressed and never read as such: the payload
            // below is a monotonic cumulative sum, and its count is `maxDoc`.
            format: 0,
            missing_offset: -1,
            data_offset: addresses_offset,
            count: 3,
        }
        .bytes(),
    );
    metadata.extend_from_slice(&END_OF_FIELDS);

    let mut data = data_header();
    data.extend_from_slice(b"aabb");
    data.extend_from_slice(&flat);
    data.extend(monotonic(&[2, 2, 3]));

    assert_files(
        "a multi-valued sorted set",
        &(files.data, files.metadata),
        &data,
        &metadata,
    );
}

#[test]
fn a_numeric_stream_that_is_not_dense_is_refused() {
    let mut values = SortedPasses::from_sorted_records(vec![NumericRecord {
        document: 1,
        value: Some(5),
    }]);
    let mut consumer = consumer(2);
    let refusal = consumer
        .add_numeric(0, &mut values)
        .expect_err("document 0 is missing from the stream");
    assert!(refusal.to_string().contains("dense"), "{refusal}");
}

#[test]
fn a_numeric_stream_of_the_wrong_length_is_refused() {
    let mut values = numeric_records(&[Some(1), Some(2)]);
    let mut consumer = consumer(3);
    let refusal = consumer
        .add_numeric(0, &mut values)
        .expect_err("two values for three documents");
    assert!(
        refusal.to_string().contains("one value per document"),
        "{refusal}"
    );
}
