//! Test support: an independent writer for synthetic repositories.
//!
//! These builders serialize segments, archives, indexes, journals, and
//! manifests from scratch, sharing no code with the reader under test
//! (except the CRC32 function, which is itself verified against published
//! test vectors; the map hash below is likewise its own implementation,
//! verified against externally computed vectors). Round-tripping through
//! an independent encoder guards against bugs that a self-consistent
//! reader/writer pair would hide.

#![allow(
    unreachable_pub,
    reason = "this module is compiled into test binaries, where pub only means module-visible"
)]

use std::path::{Path, PathBuf};

use froe::checksum::crc32;

/// The map-entry hash, implemented independently of the production
/// `froe::hashing` module: `(String.hashCode(name) ^ M) * M + A` with the
/// `MapRecord` constants and wrapping 32-bit arithmetic over UTF-16 code
/// units. Verified against externally computed vectors in
/// `independent_map_hash_matches_external_vectors`.
pub fn independent_map_entry_hash(name: &str) -> u32 {
    let mut string_hash = 0u32;
    for code_unit in name.encode_utf16() {
        string_hash = string_hash
            .wrapping_mul(31)
            .wrapping_add(u32::from(code_unit));
    }
    (string_hash ^ 0xDEEC_E66D)
        .wrapping_mul(0xDEEC_E66D)
        .wrapping_add(0xB)
}

#[test]
fn independent_map_hash_matches_external_vectors() {
    assert_eq!(independent_map_entry_hash(""), 0xB460_0A74);
    assert_eq!(independent_map_entry_hash("a"), 0x3C9C_BB27);
    assert_eq!(independent_map_entry_hash("root"), 0xC289_24EE);
    assert_eq!(independent_map_entry_hash("content"), 0x9646_6A8F);
    assert_eq!(independent_map_entry_hash("Aa"), 0x6059_D734);
    assert_eq!(independent_map_entry_hash("BB"), 0x6059_D734);
}

/// A UUID as its two 64-bit halves.
pub type SegmentUuid = (u64, u64);

/// Formats a UUID the way tar entry names and journal lines spell it.
pub fn format_uuid(uuid: SegmentUuid) -> String {
    let (most, least) = uuid;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        most >> 32,
        (most >> 16) & 0xFFFF,
        most & 0xFFFF,
        least >> 48,
        least & 0xFFFF_FFFF_FFFF,
    )
}

/// A data segment UUID with the `a` kind nibble.
pub fn data_segment_uuid(seed: u64) -> SegmentUuid {
    (seed, 0xA000_0000_0000_0000 | seed)
}

/// Serializes a 6-byte record identifier: a segment reference (0 = same
/// segment, n = entry n-1 of the reference table) and a record number.
pub fn record_identifier_bytes(segment_reference: u16, record_number: u32) -> Vec<u8> {
    let mut bytes = segment_reference.to_be_bytes().to_vec();
    bytes.extend_from_slice(&record_number.to_be_bytes());
    bytes
}

/// A small string value record (length < 128).
pub fn string_record(text: &str) -> Vec<u8> {
    assert!(text.len() < 128, "test strings use the small encoding");
    let mut bytes = vec![text.len() as u8];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Record type bytes as stored in the record reference table.
pub const TYPE_MAP_LEAF: u8 = 0;
pub const TYPE_MAP_BRANCH: u8 = 1;
pub const TYPE_LIST_BUCKET: u8 = 2;
pub const TYPE_VALUE: u8 = 4;
pub const TYPE_TEMPLATE: u8 = 6;
pub const TYPE_NODE: u8 = 7;
pub const TYPE_EXTERNAL_BLOB_IDENTIFIER: u8 = 8;

/// Builds one version 13 data segment.
/// One map entry to serialize: the name, the key's serialized record
/// identifier, and the value's serialized record identifier.
pub type MapEntryFixture = (String, Vec<u8>, Vec<u8>);

pub struct SegmentBuilder {
    referenced_segments: Vec<SegmentUuid>,
    records: Vec<(u32, u8, Vec<u8>)>,
}

impl SegmentBuilder {
    pub fn new(_uuid: SegmentUuid) -> Self {
        Self {
            referenced_segments: Vec::new(),
            records: Vec::new(),
        }
    }

    /// Declares a referenced segment, returning the reference value to use
    /// in record identifiers (1-based).
    pub fn add_referenced_segment(&mut self, uuid: SegmentUuid) -> u16 {
        self.referenced_segments.push(uuid);
        self.referenced_segments.len() as u16
    }

    pub fn add_record(&mut self, record_number: u32, type_byte: u8, content: Vec<u8>) {
        self.records.push((record_number, type_byte, content));
    }

    /// Serializes the segment: header, reference tables, and record data
    /// laid out forward from the header (any placement is legal as long
    /// as the table's virtual offsets are correct).
    pub fn build(&self) -> Vec<u8> {
        let mut records = self.records.clone();
        records.sort_by_key(|(record_number, _, _)| *record_number);

        let tables_end = 32 + self.referenced_segments.len() * 16 + records.len() * 9;
        let mut positions = Vec::with_capacity(records.len());
        let mut cursor = tables_end.div_ceil(4) * 4;
        for (_, _, content) in &records {
            positions.push(cursor);
            cursor += content.len().div_ceil(4) * 4;
        }
        let segment_size = cursor.div_ceil(16) * 16;
        assert!(
            segment_size <= 262_144,
            "test segment exceeds the maximum size"
        );

        let mut bytes = vec![0u8; segment_size];
        bytes[0..3].copy_from_slice(b"0aK");
        bytes[3] = 13;
        // Full generation 1, compacted.
        bytes[4..8].copy_from_slice(&(1u32 | 0x8000_0000).to_be_bytes());
        // Generation 1.
        bytes[10..14].copy_from_slice(&1u32.to_be_bytes());
        bytes[14..18].copy_from_slice(&(self.referenced_segments.len() as u32).to_be_bytes());
        bytes[18..22].copy_from_slice(&(records.len() as u32).to_be_bytes());

        for (reference_index, (most, least)) in self.referenced_segments.iter().enumerate() {
            let base = 32 + reference_index * 16;
            bytes[base..base + 8].copy_from_slice(&most.to_be_bytes());
            bytes[base + 8..base + 16].copy_from_slice(&least.to_be_bytes());
        }

        let table_base = 32 + self.referenced_segments.len() * 16;
        for (record_index, (record_number, type_byte, content)) in records.iter().enumerate() {
            let base = table_base + record_index * 9;
            let position = positions[record_index];
            let virtual_offset = (262_144 - (segment_size - position)) as u32;
            bytes[base..base + 4].copy_from_slice(&record_number.to_be_bytes());
            bytes[base + 4] = *type_byte;
            bytes[base + 5..base + 9].copy_from_slice(&virtual_offset.to_be_bytes());
            bytes[position..position + content.len()].copy_from_slice(content);
        }
        bytes
    }
}

/// Builds a map record set (a branch with leaf buckets, or a single leaf)
/// for the given entries, allocating records through `allocate`. Entries
/// are `(name, key record identifier bytes, value record identifier
/// bytes)`. Returns the record number of the map root.
pub fn build_child_map(
    builder: &mut SegmentBuilder,
    allocate: &mut impl FnMut() -> u32,
    entries: &[MapEntryFixture],
) -> u32 {
    if entries.len() <= 32 {
        let leaf = leaf_map_record(0, entries);
        let record_number = allocate();
        builder.add_record(record_number, TYPE_MAP_LEAF, leaf);
        return record_number;
    }
    // Branch at level 0: group entries into buckets by the top five bits
    // of their hash.
    let mut buckets: Vec<Vec<MapEntryFixture>> = vec![Vec::new(); 32];
    for (name, key, value) in entries {
        let bucket_index = ((independent_map_entry_hash(name) >> 27) & 0x1F) as usize;
        buckets[bucket_index].push((name.clone(), key.clone(), value.clone()));
    }
    let mut bitmap = 0u32;
    let mut bucket_records = Vec::new();
    for (bucket_index, bucket) in buckets.iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        bitmap |= 1 << bucket_index;
        let record_number = allocate();
        builder.add_record(record_number, TYPE_MAP_LEAF, leaf_map_record(1, bucket));
        bucket_records.push(record_number);
    }
    // Branch head: level 0 in the top three bits, total entry count below.
    let mut branch_bytes = (entries.len() as u32).to_be_bytes().to_vec();
    branch_bytes.extend_from_slice(&bitmap.to_be_bytes());
    for record_number in bucket_records {
        branch_bytes.extend_from_slice(&record_identifier_bytes(0, record_number));
    }
    let record_number = allocate();
    builder.add_record(record_number, TYPE_MAP_BRANCH, branch_bytes);
    record_number
}

/// Serializes a leaf map record at the given level: head, hashes sorted
/// as unsigned values with ties broken by name in UTF-16 code unit order
/// (Java's `String.compareTo`, which differs from Rust byte order for
/// supplementary characters), then interleaved key/value identifiers.
fn leaf_map_record(level: u32, entries: &[MapEntryFixture]) -> Vec<u8> {
    let mut sorted: Vec<&MapEntryFixture> = entries.iter().collect();
    sorted.sort_by(|first, second| {
        independent_map_entry_hash(&first.0)
            .cmp(&independent_map_entry_hash(&second.0))
            .then_with(|| first.0.encode_utf16().cmp(second.0.encode_utf16()))
    });
    let head = (level << 29) | entries.len() as u32;
    let mut bytes = head.to_be_bytes().to_vec();
    for (name, _, _) in &sorted {
        bytes.extend_from_slice(&independent_map_entry_hash(name).to_be_bytes());
    }
    for (_, key, value) in &sorted {
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(value);
    }
    bytes
}

/// One entry going into an archive: the segment UUID and its bytes.
pub struct ArchiveBuilder {
    segments: Vec<(SegmentUuid, Vec<u8>)>,
    include_index: bool,
}

impl ArchiveBuilder {
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            include_index: true,
        }
    }

    /// Omits the index (and the other trailer entries), simulating the
    /// archive a live repository is currently writing.
    pub fn without_index(mut self) -> Self {
        self.include_index = false;
        self
    }

    pub fn add_segment(&mut self, uuid: SegmentUuid, bytes: Vec<u8>) {
        self.segments.push((uuid, bytes));
    }

    /// Serializes the archive: segment entries, then (unless disabled)
    /// the binary references, graph, and index trailer entries, then the
    /// two terminating zero blocks.
    pub fn build(&self, archive_file_name: &str) -> Vec<u8> {
        let mut archive = Vec::new();
        let mut index_entries: Vec<(SegmentUuid, u32, u32)> = Vec::new();

        for (uuid, bytes) in &self.segments {
            let entry_name = format!("{}.{:08x}", format_uuid(*uuid), crc32(bytes));
            archive.extend(tar_entry_header(&entry_name, bytes.len() as u64));
            let position = archive.len() as u32;
            archive.extend_from_slice(bytes);
            archive.extend(std::iter::repeat_n(
                0u8,
                bytes.len().div_ceil(512) * 512 - bytes.len(),
            ));
            index_entries.push((*uuid, position, bytes.len() as u32));
        }

        if self.include_index {
            // Empty binary references catalog (version 2): footer only,
            // front-padded to one block; the data before the footer is
            // empty, so its checksum is the CRC32 of no bytes.
            let mut references_payload = vec![0u8; 512 - 16];
            references_payload.extend_from_slice(&crc32(&[]).to_be_bytes());
            references_payload.extend_from_slice(&0u32.to_be_bytes());
            references_payload.extend_from_slice(&16u32.to_be_bytes());
            references_payload.extend_from_slice(&0x0A31_420Au32.to_be_bytes());
            archive.extend(tar_entry_header(
                &format!("{archive_file_name}.brf"),
                references_payload.len() as u64,
            ));
            archive.extend_from_slice(&references_payload);

            // Empty graph: same shape with the graph magic.
            let mut graph_payload = vec![0u8; 512 - 16];
            graph_payload.extend_from_slice(&crc32(&[]).to_be_bytes());
            graph_payload.extend_from_slice(&0u32.to_be_bytes());
            graph_payload.extend_from_slice(&16u32.to_be_bytes());
            graph_payload.extend_from_slice(&0x0A30_470Au32.to_be_bytes());
            archive.extend(tar_entry_header(
                &format!("{archive_file_name}.gph"),
                graph_payload.len() as u64,
            ));
            archive.extend_from_slice(&graph_payload);

            // Index (version 2), entries sorted by signed UUID halves.
            let mut sorted = index_entries.clone();
            sorted.sort_by_key(|((most, least), _, _)| (*most as i64, *least as i64));
            let mut entries_bytes = Vec::new();
            for ((most, least), position, size) in &sorted {
                entries_bytes.extend_from_slice(&most.to_be_bytes());
                entries_bytes.extend_from_slice(&least.to_be_bytes());
                entries_bytes.extend_from_slice(&position.to_be_bytes());
                entries_bytes.extend_from_slice(&size.to_be_bytes());
                entries_bytes.extend_from_slice(&1u32.to_be_bytes()); // generation
                entries_bytes.extend_from_slice(&1u32.to_be_bytes()); // full generation
                entries_bytes.push(1); // compacted
            }
            let data_size = entries_bytes.len() + 16;
            let padded_size = data_size.div_ceil(512) * 512;
            let mut index_payload = vec![0u8; padded_size - data_size];
            index_payload.extend_from_slice(&entries_bytes);
            index_payload.extend_from_slice(&crc32(&entries_bytes).to_be_bytes());
            index_payload.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
            index_payload.extend_from_slice(&(padded_size as u32).to_be_bytes());
            index_payload.extend_from_slice(&0x0A31_4B0Au32.to_be_bytes());
            archive.extend(tar_entry_header(
                &format!("{archive_file_name}.idx"),
                index_payload.len() as u64,
            ));
            archive.extend_from_slice(&index_payload);
        }

        archive.extend_from_slice(&[0u8; 1024]);
        archive
    }
}

/// Builds a 512-byte tar entry header with a correct header checksum.
fn tar_entry_header(name: &str, size: u64) -> Vec<u8> {
    let mut header = vec![0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..107].copy_from_slice(b"0000400");
    header[108..115].copy_from_slice(b"0000000");
    header[116..123].copy_from_slice(b"0000000");
    let size_field = format!("{size:011o}");
    header[124..135].copy_from_slice(size_field.as_bytes());
    header[136..147].copy_from_slice(b"00000000000");
    header[156] = b'0';
    header[148..156].copy_from_slice(b"        ");
    let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
    let checksum_field = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum_field.as_bytes());
    header
}

/// A temporary directory removed on drop.
pub struct TestDirectory {
    pub path: PathBuf,
}

impl TestDirectory {
    pub fn new(test_name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-integration-{test_name}-{}",
            std::process::id()
        ));
        if path.exists() {
            std::fs::remove_dir_all(&path).expect("clean stale test directory");
        }
        std::fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Writes a complete repository directory: archives, journal, manifest.
pub fn write_repository(
    directory: &Path,
    archives: &[(String, Vec<u8>)],
    journal_lines: &[String],
) {
    for (file_name, bytes) in archives {
        std::fs::write(directory.join(file_name), bytes).expect("write archive");
    }
    std::fs::write(
        directory.join("journal.log"),
        journal_lines.join("\n") + "\n",
    )
    .expect("write journal");
    std::fs::write(
        directory.join("manifest"),
        "#froe test repository\nstore.version=2\n",
    )
    .expect("write manifest");
}
