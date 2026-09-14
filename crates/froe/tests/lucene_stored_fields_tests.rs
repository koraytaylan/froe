//! The `Lucene40` stored-fields writer, against bytes computed by hand.
//!
//! `docs/analysis/lucene-4-7-codec.md` §5. The headers are the format's
//! own — `Lucene40StoredFieldsData` and `Lucene40StoredFieldsIndex`, both
//! at version **0**, because `oakCodec` leaves this format alone and its
//! numbering restarts.

use froe::index::lucene::codec::stored_fields::{StoredFieldsWriter, StoredValue};

/// `CodecUtil.writeHeader`: magic, the name as a string, the version as a
/// big-endian `Int`.
fn header(name: &str) -> Vec<u8> {
    let mut bytes = vec![0x3f, 0xd7, 0x6c, 0x17, name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&0i32.to_be_bytes());
    bytes
}

fn data_header() -> Vec<u8> {
    header("Lucene40StoredFieldsData")
}

fn index_header() -> Vec<u8> {
    header("Lucene40StoredFieldsIndex")
}

fn writer() -> StoredFieldsWriter<Vec<u8>> {
    StoredFieldsWriter::new(Vec::new(), Vec::new()).expect("open the stored-fields files")
}

#[test]
fn the_two_headers_are_the_lucene40_ones_at_version_zero() {
    let files = writer().finish(0).expect("finish");
    assert_eq!(files.data, data_header());
    assert_eq!(files.index, index_header());
    assert_eq!(files.data.len(), 33, "the .fdt header length");
    assert_eq!(files.index.len(), 34, "the .fdx header length");
}

#[test]
fn a_document_with_no_stored_fields_is_still_a_record() {
    let mut writer = writer();
    writer.start_document(0).expect("start");
    writer.finish_document().expect("finish");
    let files = writer.finish(1).expect("finish");

    // A count of zero and nothing after it — and an index entry all the
    // same, which is what keeps `.fdx` a fixed eight bytes per document.
    let mut data = data_header();
    data.push(0x00);
    assert_eq!(files.data, data);

    let mut index = index_header();
    index.extend_from_slice(&33u64.to_be_bytes());
    assert_eq!(files.index, index);
}

#[test]
fn a_string_a_binary_and_a_long() {
    let mut writer = writer();
    writer.start_document(3).expect("start");
    writer
        .write_field(1, StoredValue::Text("hi"))
        .expect("the string");
    writer
        .write_field(2, StoredValue::Binary(&[0xde, 0xad]))
        .expect("the binary");
    writer
        .write_field(5, StoredValue::Long(-2))
        .expect("the long");
    writer.finish_document().expect("finish the document");
    let files = writer.finish(1).expect("finish");

    let mut data = data_header();
    data.push(0x03); // three stored fields
    // A string sets no bit at all, and its value is a byte length then the
    // UTF-8.
    data.extend_from_slice(&[0x01, 0x00, 0x02, b'h', b'i']);
    // A binary sets FIELD_IS_BINARY, bit 1, and carries its own length.
    data.extend_from_slice(&[0x02, 0x02, 0x02, 0xde, 0xad]);
    // A long sets code 2 at shift 3, and is eight fixed big-endian bytes —
    // not a vlong, so a negative value costs the same as any other.
    data.extend_from_slice(&[0x05, 0x10]);
    data.extend_from_slice(&(-2i64).to_be_bytes());
    assert_eq!(files.data, data);

    let mut index = index_header();
    index.extend_from_slice(&33u64.to_be_bytes());
    assert_eq!(files.index, index);
}

#[test]
fn the_numeric_types_are_raw_bits_at_fixed_widths() {
    let mut writer = writer();
    writer.start_document(3).expect("start");
    writer
        .write_field(0, StoredValue::Integer(7))
        .expect("the int");
    writer
        .write_field(1, StoredValue::Float(1.0))
        .expect("the float");
    writer
        .write_field(2, StoredValue::Double(-0.5))
        .expect("the double");
    writer.finish_document().expect("finish the document");
    let files = writer.finish(1).expect("finish");

    let mut data = data_header();
    data.push(0x03);
    // Code 1 at shift 3, then four big-endian bytes.
    data.extend_from_slice(&[0x00, 0x08, 0x00, 0x00, 0x00, 0x07]);
    // Code 3, then `Float.floatToIntBits(1.0f)` — 0x3f800000.
    data.extend_from_slice(&[0x01, 0x18, 0x3f, 0x80, 0x00, 0x00]);
    // Code 4, then `Double.doubleToLongBits(-0.5)` — 0xbfe0…0.
    data.extend_from_slice(&[0x02, 0x20]);
    data.extend_from_slice(&[0xbf, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    assert_eq!(files.data, data);
}

#[test]
fn a_thousand_documents_are_eight_index_bytes_each() {
    let mut writer = writer();
    for document in 0..1000i32 {
        writer.start_document(1).expect("start");
        writer
            .write_field(0, StoredValue::Integer(document))
            .expect("the field");
        writer.finish_document().expect("finish the document");
    }
    let files = writer.finish(1000).expect("finish");

    // Every record is the same seven bytes: the field count, the field
    // number, the bits byte and four for the value.
    assert_eq!(files.data.len(), 33 + 7 * 1000);
    assert_eq!(files.index.len(), 34 + 8 * 1000);
    for document in 0..1000usize {
        let at = 34 + document * 8;
        let pointer = u64::from_be_bytes(
            files.index[at..at + 8]
                .try_into()
                .expect("an eight-byte pointer"),
        );
        assert_eq!(
            pointer,
            33 + 7 * document as u64,
            "the pointer for document {document}"
        );
    }
}

#[test]
fn a_document_that_writes_more_fields_than_it_declared_is_refused() {
    let mut writer = writer();
    writer.start_document(1).expect("start");
    writer
        .write_field(0, StoredValue::Integer(1))
        .expect("the field");
    let refusal = writer
        .write_field(1, StoredValue::Integer(2))
        .expect_err("one more than declared");
    assert!(
        refusal.to_string().contains("cannot be revised"),
        "{refusal}"
    );
}

#[test]
fn a_document_that_writes_fewer_fields_than_it_declared_is_refused() {
    let mut writer = writer();
    writer.start_document(2).expect("start");
    writer
        .write_field(0, StoredValue::Integer(1))
        .expect("the field");
    let refusal = writer.finish_document().expect_err("one short");
    assert!(refusal.to_string().contains("wrote 1"), "{refusal}");
}

#[test]
fn a_document_count_that_does_not_match_the_index_is_refused() {
    let mut writer = writer();
    writer.start_document(0).expect("start");
    writer.finish_document().expect("finish the document");
    let refusal = writer.finish(2).expect_err("two declared, one written");
    assert!(
        refusal.to_string().contains("fdx size mismatch"),
        "{refusal}"
    );
}

#[test]
fn a_field_outside_a_document_is_refused() {
    let mut writer = writer();
    let refusal = writer
        .write_field(0, StoredValue::Integer(1))
        .expect_err("no document is open");
    assert!(
        refusal.to_string().contains("outside any document"),
        "{refusal}"
    );
}
