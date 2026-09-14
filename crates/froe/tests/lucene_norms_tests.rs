//! Norms, replayed against Lucene's own default similarity and against
//! bytes computed by hand.
//!
//! `docs/analysis/lucene-4-7-codec.md` §8.2. The quantization table in
//! `tests/fixtures/lucene-norm-vectors.tsv` was produced inside the pinned
//! image by `DefaultSimilarity.computeNorm` itself — the command is in the
//! file's header — so what is checked here is froe's arithmetic against
//! Lucene's, not against a second reading of the same source.

use froe::SortedPasses;
use froe::index::lucene::codec::norms::{NormRecord, NormsConsumer, norm_byte};

/// `CodecUtil.writeHeader` at this format's version, 1.
fn header(name: &str) -> Vec<u8> {
    let mut bytes = vec![0x3f, 0xd7, 0x6c, 0x17, name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&1i32.to_be_bytes());
    bytes
}

fn records(norms: &[u8]) -> SortedPasses<NormRecord> {
    SortedPasses::from_sorted_records(
        norms
            .iter()
            .enumerate()
            .map(|(document, norm)| NormRecord {
                document: document as i32,
                norm: *norm,
            })
            .collect(),
    )
}

#[test]
fn every_vector_lucene_produced_comes_back() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/lucene-norm-vectors.tsv");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut checked = 0usize;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 4, "a vector row has four columns: {line}");
        let boost = f32::from_bits(fields[0].parse::<u32>().expect("the boost bits"));
        let length: u32 = fields[1].parse().expect("the length");
        let overlap: u32 = fields[2].parse().expect("the overlap");
        let expected: u8 = fields[3].parse().expect("the norm");
        // The default similarity discounts overlapping positions, so the
        // term count is the length less them.
        let produced = norm_byte(boost, length - overlap);
        assert_eq!(
            produced, expected,
            "boost {boost}, length {length}, overlap {overlap}"
        );
        checked += 1;
    }
    assert!(checked > 300, "the table is the one that was generated");
}

#[test]
fn a_field_with_no_term_on_a_document_quantizes_to_the_top_byte() {
    // `1.0 / sqrt(0)` is infinite, and the small-float encoding returns
    // `-1` for it rather than saturating at the top of the ordinary range.
    assert_eq!(norm_byte(1.0, 0), 0xff);
    assert_eq!(norm_byte(2.5, 0), 0xff);
    // A boost of zero is the other end: the value is zero or negative, so
    // the byte is `0` and not `1`.
    assert_eq!(norm_byte(0.0, 4), 0x00);
}

#[test]
fn the_two_headers_name_lucene41_and_the_metadata_ends_with_the_marker() {
    let files = NormsConsumer::new(Vec::new(), Vec::new(), 0)
        .expect("open")
        .finish()
        .expect("finish");
    assert_eq!(files.data, header("Lucene41NormsData"));
    let mut metadata = header("Lucene41NormsMetadata");
    metadata.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
    assert_eq!(files.metadata, metadata);
}

#[test]
fn a_field_every_document_carries_is_one_byte_each() {
    let norms = [
        norm_byte(1.0, 1),
        norm_byte(1.0, 4),
        norm_byte(2.0, 9),
        norm_byte(1.0, 0),
    ];
    let mut records = records(&norms);
    let mut consumer = NormsConsumer::new(Vec::new(), Vec::new(), 4).expect("open");
    consumer.add_field(3, &mut records).expect("the field");
    let files = consumer.finish().expect("finish");

    let mut data = header("Lucene41NormsData");
    data.extend_from_slice(&norms);
    assert_eq!(files.data, data, ".nvd");
    assert_eq!(
        files.data.len(),
        26 + 4,
        "one byte per document, and no more"
    );

    let mut metadata = header("Lucene41NormsMetadata");
    metadata.push(0x03); // the field number
    metadata.push(0x00); // NUMBER
    metadata.extend_from_slice(&26i64.to_be_bytes()); // where .nvd stood
    // UNCOMPRESSED is 2 in this format — not 0, which is what the
    // doc-values format calls it. Nothing follows it: no packed-integer
    // version and no block size.
    metadata.push(0x02);
    metadata.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
    assert_eq!(files.metadata, metadata, ".nvm");
}

#[test]
fn a_document_that_lacks_the_field_takes_the_byte_zero() {
    // The stream spans every document of the segment; a document without
    // the field is padded with `0`, which is what Lucene's own accumulation
    // does — and a stream of only the documents that carried the field
    // would leave the reader taking the following bytes as norms.
    let norms = [norm_byte(1.0, 2), 0, 0, norm_byte(1.0, 3), 0];
    let mut records = records(&norms);
    let mut consumer = NormsConsumer::new(Vec::new(), Vec::new(), 5).expect("open");
    consumer.add_field(0, &mut records).expect("the field");
    let files = consumer.finish().expect("finish");

    let mut data = header("Lucene41NormsData");
    data.extend_from_slice(&norms);
    assert_eq!(files.data, data);
    assert_eq!(files.data.len(), 26 + 5);
}

#[test]
fn two_fields_take_two_entries_and_two_data_pointers() {
    let first = [norm_byte(1.0, 1), norm_byte(1.0, 2)];
    let second = [norm_byte(3.0, 4), 0];
    let mut first_records = records(&first);
    let mut second_records = records(&second);
    let mut consumer = NormsConsumer::new(Vec::new(), Vec::new(), 2).expect("open");
    consumer
        .add_field(0, &mut first_records)
        .expect("the first");
    consumer
        .add_field(7, &mut second_records)
        .expect("the second");
    let files = consumer.finish().expect("finish");

    let mut metadata = header("Lucene41NormsMetadata");
    metadata.push(0x00);
    metadata.push(0x00);
    metadata.extend_from_slice(&26i64.to_be_bytes());
    metadata.push(0x02);
    metadata.push(0x07);
    metadata.push(0x00);
    metadata.extend_from_slice(&28i64.to_be_bytes());
    metadata.push(0x02);
    metadata.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
    assert_eq!(files.metadata, metadata);
}

#[test]
fn a_stream_of_the_wrong_length_is_refused() {
    let mut records = records(&[1, 2]);
    let mut consumer = NormsConsumer::new(Vec::new(), Vec::new(), 3).expect("open");
    let refusal = consumer
        .add_field(0, &mut records)
        .expect_err("two norms for three documents");
    assert!(
        refusal.to_string().contains("one byte per document"),
        "{refusal}"
    );
}

#[test]
fn a_stream_that_skips_a_document_is_refused() {
    let mut records = SortedPasses::from_sorted_records(vec![
        NormRecord {
            document: 0,
            norm: 1,
        },
        NormRecord {
            document: 2,
            norm: 3,
        },
    ]);
    let mut consumer = NormsConsumer::new(Vec::new(), Vec::new(), 3).expect("open");
    let refusal = consumer
        .add_field(0, &mut records)
        .expect_err("document 1 is missing");
    assert!(refusal.to_string().contains("dense"), "{refusal}");
}
