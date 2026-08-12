//! Oak-compatible human-readable dumps of raw segment bytes.
//!
//! Oak's `SegmentDump` is the last-resort diagnostic for format and
//! interoperability failures: it prints the segment header, reference and
//! record tables, then the exact stored bytes in Apache Commons IO's
//! 16-byte hex-dump layout. This module reproduces that presentation without
//! opening the repository for write or taking its lock.
//!
//! Corrupt input remains diagnosable: parsing happens only after the raw byte
//! length is bounded, and a parse failure is rendered as terminal-safe text
//! between the header and the complete hex dump. Segments over the format's
//! 256 KiB limit are refused before parsing or allocating their rendered form;
//! control and bidirectional characters in optional segment info are escaped,
//! with backslashes doubled so the escape notation remains unambiguous;
//! and an unknown record type is printed as `UNKNOWN(n)` where Oak's enum
//! indexing throws. Apart from that terminal-safety escaping, valid Oak
//! segments keep the same headers, GC-generation punctuation,
//! record-number/offset conventions, platform line endings, and Commons IO
//! byte rows byte-for-byte.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::segment::identifier::{SegmentIdentifier, SegmentKind};
use crate::segment::parsed_segment::{MAXIMUM_SEGMENT_SIZE, ParsedSegment};
use crate::segment::record::RecordType;
use crate::segment::view::SegmentView;
use crate::store::Repository;

/// The separator used by Oak's `SegmentDump.dumpSegment` between logical
/// sections; see `docs/analysis/read-tooling.md` section 5.2.
const SECTION_SEPARATOR: &str =
    "--------------------------------------------------------------------------";

// SegmentDump uses Java's platform line separator (`%n`) throughout; see
// `docs/analysis/read-tooling.md` section 5.2.
#[cfg(windows)]
const LINE_SEPARATOR: &str = "\r\n";
#[cfg(not(windows))]
const LINE_SEPARATOR: &str = "\n";

/// Renders one segment in Oak's `SegmentDump` layout.
///
/// Record numbers and virtual offsets are lower-case, eight-digit
/// hexadecimal values, matching Oak. Raw bytes use the Apache Commons IO
/// layout: an upper-case eight-digit offset, sixteen upper-case byte values,
/// and printable ASCII (bytes `0x20..=0x7e`) on every line.
///
/// This repository-backed entry point reaches the exact archive bytes before
/// asking the segment parser to interpret them, so bad magic, versions, or
/// tables do not hide the raw diagnostic.
pub fn dump_segment(repository: &Repository, identifier: SegmentIdentifier) -> Result<String> {
    dump_segment_bytes(identifier, repository.segment_bytes(identifier)?)
}

/// Renders exact stored bytes in Oak's `SegmentDump` layout.
///
/// For a structurally invalid segment at or below the format size limit, the
/// returned text always contains the header, a terminal-safe `Parse error:`
/// line, and the complete raw hex dump. Optional segment info is rendered as
/// terminal-safe, unambiguous text; an unreadable info record is simply
/// omitted. Over-size input is the one deliberate refusal: it is rejected
/// before parsing and before allocating the much larger text rendering.
pub fn dump_segment_bytes(identifier: SegmentIdentifier, bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAXIMUM_SEGMENT_SIZE {
        return Err(Error::InvalidFormat {
            details: format!(
                "segment {identifier} has {} bytes, exceeding the {MAXIMUM_SEGMENT_SIZE}-byte format limit",
                bytes.len()
            ),
        });
    }
    let mut output = String::new();

    write!(
        output,
        "Segment {identifier} ({} bytes){LINE_SEPARATOR}",
        bytes.len()
    )
    .expect("writing to a String cannot fail");

    match ParsedSegment::parse(identifier, bytes) {
        Ok(structure) => append_parsed_structure(&mut output, structure, bytes),
        Err(error) => {
            write!(
                output,
                "Parse error: {}{LINE_SEPARATOR}",
                visible_text(&error.to_string())
            )
            .expect("writing to a String cannot fail");
        }
    }

    output.push_str(SECTION_SEPARATOR);
    output.push_str(LINE_SEPARATOR);
    append_hex_dump(&mut output, bytes);
    output.push_str(SECTION_SEPARATOR);
    output.push_str(LINE_SEPARATOR);
    Ok(output)
}

fn append_parsed_structure(output: &mut String, structure: ParsedSegment, bytes: &[u8]) {
    let structure = Arc::new(structure);
    if structure.kind != SegmentKind::Data {
        return;
    }
    let view = SegmentView {
        structure: Arc::clone(&structure),
        bytes: bytes.into(),
    };
    if let Some(first_record) = structure.record_table().first()
        && let Some(info) = bounded_segment_info(&view, first_record.record_number)
    {
        write!(
            output,
            "Info: {info}, Generation: GCGeneration{{generation={}, fullGeneration={}, isCompacted={}}}{LINE_SEPARATOR}",
            structure.generation,
            structure.full_generation,
            structure.is_compacted,
            info = visible_text(&info),
        )
        .expect("writing to a String cannot fail");
    }

    output.push_str(SECTION_SEPARATOR);
    output.push_str(LINE_SEPARATOR);
    for (reference_index, reference) in structure.referenced_segments.iter().enumerate() {
        write!(
            output,
            "reference {:02x}: {reference}{LINE_SEPARATOR}",
            reference_index + 1
        )
        .expect("writing to a String cannot fail");
    }
    for record in structure.record_table() {
        let record_type = oak_record_type_name(record.record_type(), record.type_byte);
        // Oak performs this calculation as signed Java `int` arithmetic and
        // prints even a negative address as its eight hexadecimal bits. Do
        // the same instead of letting an invalid virtual address suppress the
        // raw bytes that make the diagnostic useful.
        let address = bytes.len() as i32 - (MAXIMUM_SEGMENT_SIZE as i32 - record.offset as i32);
        write!(
            output,
            "{record_type:>10} record {:08x}: {:08x} @ {address:08x}{LINE_SEPARATOR}",
            record.record_number, record.offset
        )
        .expect("writing to a String cannot fail");
    }
}

/// Segment writers always encode the optional info as a small or medium
/// value. Reading only those inline forms keeps a diagnostic of hostile
/// input from following an alleged multi-gigabyte long-string graph merely
/// to render optional metadata.
fn bounded_segment_info(view: &SegmentView<'_>, record_number: u32) -> Option<String> {
    let head = view.read_u8(record_number, 0).ok()?;
    let (offset, length) = if head & 0x80 == 0 {
        (1, usize::from(head))
    } else if head & 0x40 == 0 {
        let stored = view.read_u16(record_number, 0).ok()?;
        (2, usize::from(stored & 0x3fff) + 128)
    } else {
        return None;
    };
    let bytes = view.read_bytes(record_number, offset, length).ok()?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn visible_text(text: &str) -> String {
    let mut visible = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\\' {
            // Keep the escape notation injective: a literal `\u{1b}` must
            // remain distinguishable from an actual escape character.
            visible.push_str("\\\\");
        } else if character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            write!(visible, "\\u{{{:x}}}", character as u32)
                .expect("writing to a String cannot fail");
        } else {
            visible.push(character);
        }
    }
    visible
}

fn oak_record_type_name(record_type: Option<RecordType>, type_byte: u8) -> String {
    match record_type {
        Some(RecordType::MapLeaf) => "LEAF".to_owned(),
        Some(RecordType::MapBranch) => "BRANCH".to_owned(),
        Some(RecordType::ListBucket) => "BUCKET".to_owned(),
        Some(RecordType::List) => "LIST".to_owned(),
        Some(RecordType::Value) => "VALUE".to_owned(),
        Some(RecordType::Block) => "BLOCK".to_owned(),
        Some(RecordType::Template) => "TEMPLATE".to_owned(),
        Some(RecordType::Node) => "NODE".to_owned(),
        Some(RecordType::ExternalBlobIdentifier) => "BLOB_ID".to_owned(),
        // Oak's diagnostic indexes `RecordType.values()` and crashes on
        // an unknown ordinal. Keeping the byte visible is a deliberately
        // safer diagnostic deviation for corrupt input.
        None => format!("UNKNOWN({type_byte})"),
    }
}

/// Appends Apache Commons IO's stable 16-byte hex-dump layout.
fn append_hex_dump(output: &mut String, bytes: &[u8]) {
    for (line_index, line) in bytes.chunks(16).enumerate() {
        write!(output, "{:08X} ", line_index * 16).expect("writing to a String cannot fail");
        for byte_index in 0..16 {
            if let Some(byte) = line.get(byte_index) {
                write!(output, "{byte:02X} ").expect("writing to a String cannot fail");
            } else {
                output.push_str("   ");
            }
        }
        for &byte in line {
            output.push(if (0x20..0x7f).contains(&byte) {
                char::from(byte)
            } else {
                '.'
            });
        }
        output.push_str(LINE_SEPARATOR);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LINE_SEPARATOR, MAXIMUM_SEGMENT_SIZE, SECTION_SEPARATOR, append_hex_dump,
        dump_segment_bytes,
    };
    use crate::segment::parsed_segment::tests::{
        bulk_segment_identifier, data_segment_identifier, synthetic_data_segment,
    };

    const GOLDEN_SECTION_SEPARATOR: &str =
        "--------------------------------------------------------------------------";
    #[cfg(windows)]
    const GOLDEN_LINE_SEPARATOR: &str = "\r\n";
    #[cfg(not(windows))]
    const GOLDEN_LINE_SEPARATOR: &str = "\n";

    #[test]
    fn hex_dump_formats_zero_sixteen_and_partial_line_boundaries() {
        let mut empty = String::new();
        append_hex_dump(&mut empty, &[]);
        assert!(empty.is_empty());

        for (length, expected_lines) in [(1, 1), (15, 1), (16, 1), (17, 2)] {
            let mut boundary = String::new();
            append_hex_dump(&mut boundary, &vec![b'x'; length]);
            assert_eq!(
                boundary.lines().count(),
                expected_lines,
                "{length}-byte boundary"
            );
            if length == 17 {
                assert!(
                    boundary
                        .lines()
                        .nth(1)
                        .expect("second line")
                        .starts_with("00000010 ")
                );
            }
        }

        let bytes: Vec<u8> = (0..17).collect();
        let mut output = String::new();
        append_hex_dump(&mut output, &bytes);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "00000000 00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F ................"
        );
        assert_eq!(
            lines[1],
            "00000010 10                                              ."
        );
    }

    #[test]
    fn dump_uses_oak_record_names_hex_numbers_and_addresses() {
        let identifier = data_segment_identifier(9);
        let info = b"{\"wid\":\"x\"}";
        let mut info_record = vec![info.len() as u8];
        info_record.extend_from_slice(info);
        let segment = synthetic_data_segment(
            &[data_segment_identifier(10)],
            &[(0x2a, 4, info_record), (0x2b, 7, vec![0; 12])],
        );
        let dump = dump_segment_bytes(identifier, &segment).expect("dump");
        assert!(dump.starts_with(&format!("Segment {identifier} (")));
        assert!(dump.contains(
            "Info: {\"wid\":\"x\"}, Generation: GCGeneration{generation=1, \
             fullGeneration=1, isCompacted=true}"
        ));
        assert!(dump.contains("reference 01: 00000000-0000-000a-a000-00000000000a"));
        assert!(dump.contains("     VALUE record 0000002a:"));
        assert!(dump.contains("      NODE record 0000002b:"));
        assert!(
            dump.contains(" @ 000000"),
            "address is buffer-relative: {dump}"
        );
        assert!(dump.contains("00000000 30 61 4B 0D "));
        assert!(dump.ends_with(&format!(
            "--------------------------------------------------------------------------{LINE_SEPARATOR}"
        )));
    }

    #[test]
    fn nonempty_data_segment_has_an_exact_oak_layout() {
        let identifier = data_segment_identifier(8);
        // Independently hand-built version-13 data segment with one segment
        // reference and three record-table entries. Record zero is Oak's
        // conventional info VALUE; TEMPLATE and NODE make the exact golden
        // cover type padding, distinct offsets, and multiple byte rows.
        let bytes: [u8; 112] = [
            // 0x00: fixed header, first half.
            0x30, 0x61, 0x4b, 0x0d, 0x80, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
            0x00, 0x00,
            // 0x10: fixed header, second half (one reference, three records).
            0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, // 0x20: referenced data-segment UUID.
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0xa0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x0a, // 0x30: VALUE entry and most of the TEMPLATE entry.
            0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x03, 0xff, 0xdc, 0x00, 0x00, 0x00, 0x2a, 0x06,
            0x00, 0x03, // 0x40: rest of TEMPLATE, NODE entry, padding, and info VALUE.
            0xff, 0xe0, 0x00, 0x00, 0x00, 0x2b, 0x07, 0x00, 0x03, 0xff, 0xe8, 0x00, 0x03, 0x4f,
            0x61, 0x6b, // 0x50: TEMPLATE bytes followed by the first NODE bytes.
            0x54, 0x45, 0x4d, 0x50, 0x4c, 0x41, 0x54, 0x45, 0x4e, 0x4f, 0x44, 0x45, 0x2d, 0x72,
            0x65, 0x63, // 0x60: rest of NODE plus segment padding.
            0x6f, 0x72, 0x64, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let dump = dump_segment_bytes(identifier, &bytes).expect("dump");
        let expected = format!(
            concat!(
                "Segment 00000000-0000-0008-a000-000000000008 (112 bytes){line_separator}",
                "Info: Oak, Generation: GCGeneration{{generation=1, fullGeneration=1, isCompacted=true}}{line_separator}",
                "{section_separator}{line_separator}",
                "reference 01: 00000000-0000-000a-a000-00000000000a{line_separator}",
                "     VALUE record 00000000: 0003ffdc @ 0000004c{line_separator}",
                "  TEMPLATE record 0000002a: 0003ffe0 @ 00000050{line_separator}",
                "      NODE record 0000002b: 0003ffe8 @ 00000058{line_separator}",
                "{section_separator}{line_separator}",
                "00000000 30 61 4B 0D 80 00 00 01 00 00 00 00 00 01 00 00 0aK.............{line_separator}",
                "00000010 00 01 00 00 00 03 00 00 00 00 00 00 00 00 00 00 ................{line_separator}",
                "00000020 00 00 00 00 00 00 00 0A A0 00 00 00 00 00 00 0A ................{line_separator}",
                "00000030 00 00 00 00 04 00 03 FF DC 00 00 00 2A 06 00 03 ............*...{line_separator}",
                "00000040 FF E0 00 00 00 2B 07 00 03 FF E8 00 03 4F 61 6B .....+.......Oak{line_separator}",
                "00000050 54 45 4D 50 4C 41 54 45 4E 4F 44 45 2D 72 65 63 TEMPLATENODE-rec{line_separator}",
                "00000060 6F 72 64 21 00 00 00 00 00 00 00 00 00 00 00 00 ord!............{line_separator}",
                "{section_separator}{line_separator}",
            ),
            line_separator = GOLDEN_LINE_SEPARATOR,
            section_separator = GOLDEN_SECTION_SEPARATOR,
        );
        assert_eq!(dump, expected);
    }

    #[test]
    fn nonempty_bulk_segment_has_an_exact_oak_layout() {
        let identifier = bulk_segment_identifier(3);
        let bytes = [
            0x00, 0x1f, 0x20, 0x21, 0x41, 0x5c, 0x7e, 0x7f, 0x80, 0x30, 0x61, 0x4b, 0x09, 0x0a,
            0x0d, 0xff, 0x5a,
        ];

        let dump = dump_segment_bytes(identifier, &bytes).expect("dump");
        let expected = format!(
            "Segment 00000000-0000-0003-b000-000000000003 (17 bytes){GOLDEN_LINE_SEPARATOR}\
             {GOLDEN_SECTION_SEPARATOR}{GOLDEN_LINE_SEPARATOR}\
             00000000 00 1F 20 21 41 5C 7E 7F 80 30 61 4B 09 0A 0D FF .. !A\\~..0aK....{GOLDEN_LINE_SEPARATOR}\
             00000010 5A                                              Z{GOLDEN_LINE_SEPARATOR}\
             {GOLDEN_SECTION_SEPARATOR}{GOLDEN_LINE_SEPARATOR}"
        );
        assert_eq!(dump, expected);
    }

    #[test]
    fn oversized_corrupt_segment_is_rejected_before_rendering() {
        let identifier = data_segment_identifier(4);
        let bytes = vec![0; MAXIMUM_SEGMENT_SIZE + 1];

        let error = dump_segment_bytes(identifier, &bytes).expect_err("oversized segment");
        assert!(error.to_string().contains("262144-byte format limit"));
        assert!(
            !error.to_string().contains("magic"),
            "the size gate runs before structural parsing"
        );
    }

    #[test]
    fn unknown_record_type_stays_visible_instead_of_panicking_like_oak() {
        let identifier = data_segment_identifier(5);
        let bytes = synthetic_data_segment(&[], &[(0, 255, vec![0])]);

        let dump = dump_segment_bytes(identifier, &bytes).expect("diagnostic remains available");
        assert!(dump.contains("UNKNOWN(255) record 00000000:"));
    }

    #[test]
    fn bad_magic_version_table_and_offset_still_render_all_raw_bytes() {
        let identifier = data_segment_identifier(6);
        let valid = synthetic_data_segment(&[], &[(0, 4, vec![0])]);
        let mut bad_magic = valid.clone();
        bad_magic[0] = b'x';
        let mut bad_version = valid.clone();
        bad_version[3] = 99;
        let mut bad_table = valid;
        bad_table[18..22].copy_from_slice(&u32::MAX.to_be_bytes());
        let mut bad_offset = synthetic_data_segment(&[], &[(0, 4, vec![0])]);
        bad_offset[37..41].copy_from_slice(&(MAXIMUM_SEGMENT_SIZE as u32).to_be_bytes());

        for (label, bytes) in [
            ("magic", bad_magic),
            ("version", bad_version),
            ("table", bad_table),
            ("offset", bad_offset),
        ] {
            let dump = dump_segment_bytes(identifier, &bytes).expect("raw diagnostic");
            assert!(
                dump.starts_with(&format!(
                    "Segment {identifier} ({} bytes){LINE_SEPARATOR}",
                    bytes.len()
                )),
                "{label} header"
            );
            assert!(
                dump.contains("Parse error: invalid segment-tar data:"),
                "{label}"
            );
            assert!(dump.contains("00000000 "), "{label} raw bytes");
            assert!(dump.ends_with(&format!(
                "{LINE_SEPARATOR}{SECTION_SEPARATOR}{LINE_SEPARATOR}"
            )));
        }
    }

    #[test]
    fn invalid_virtual_address_is_printed_like_oak_without_hiding_raw_bytes() {
        let identifier = data_segment_identifier(7);
        let mut bytes = synthetic_data_segment(&[], &[(0, 4, vec![0])]);
        // The first table entry starts at byte 32; its virtual offset is the
        // four bytes after record number and type.
        bytes[37..41].copy_from_slice(&0u32.to_be_bytes());
        let expected_address = bytes.len() as i32 - MAXIMUM_SEGMENT_SIZE as i32;

        let dump = dump_segment_bytes(identifier, &bytes).expect("raw diagnostic");
        assert!(dump.contains(&format!(
            "VALUE record 00000000: 00000000 @ {expected_address:08x}"
        )));
        assert!(dump.contains("00000000 30 61 4B 0D"));
    }

    #[test]
    fn production_dump_escapes_hostile_info_without_colliding_with_literal_escape_text() {
        let identifier = data_segment_identifier(10);
        let info = "esc=\u{1b};osc=\u{1b}]0;title\u{7};bidi=\u{202e}\u{2066};literal=\\u{1b}";
        let info_bytes = info.as_bytes();
        assert!(info_bytes.len() < 128);

        // Independently encode a one-record data segment. The record starts
        // immediately after the aligned header/table and its virtual offset
        // is derived directly from Oak's fixed 256 KiB address space.
        let record_position = 44usize;
        let record_length = 1 + info_bytes.len();
        let size = (record_position + record_length.div_ceil(4) * 4).div_ceil(16) * 16;
        let mut bytes = vec![0u8; size];
        bytes[0..3].copy_from_slice(b"0aK");
        bytes[3] = 13;
        bytes[4..8].copy_from_slice(&0x8000_0001u32.to_be_bytes());
        bytes[10..14].copy_from_slice(&1u32.to_be_bytes());
        bytes[18..22].copy_from_slice(&1u32.to_be_bytes());
        bytes[32..36].copy_from_slice(&0u32.to_be_bytes());
        bytes[36] = 4;
        let virtual_offset = (262_144 - (size - record_position)) as u32;
        bytes[37..41].copy_from_slice(&virtual_offset.to_be_bytes());
        bytes[record_position] = info_bytes.len() as u8;
        bytes[record_position + 1..record_position + record_length].copy_from_slice(info_bytes);

        let dump = dump_segment_bytes(identifier, &bytes).expect("hostile info dump");
        let expected_info = format!(
            r"Info: esc=\u{{1b}};osc=\u{{1b}}]0;title\u{{7}};bidi=\u{{202e}}\u{{2066}};literal=\\u{{1b}}, Generation: GCGeneration{{generation=1, fullGeneration=1, isCompacted=true}}{GOLDEN_LINE_SEPARATOR}"
        );
        assert!(dump.contains(&expected_info), "escaped info line: {dump:?}");
        assert!(!dump.contains('\u{1b}'), "ESC reached terminal output");
        assert!(
            !dump.contains('\u{7}'),
            "OSC terminator reached terminal output"
        );
        assert!(
            !dump.contains('\u{202e}'),
            "bidi override reached terminal output"
        );
        assert!(
            !dump.contains('\u{2066}'),
            "bidi isolate reached terminal output"
        );
    }
}
