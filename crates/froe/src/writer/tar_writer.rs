//! Writing one tar archive.
//!
//! Mirrors Oak's `SegmentTarWriter`/`TarWriter` protocol exactly:
//!
//! * the file is created lazily on the first segment write — an untouched
//!   writer leaves zero bytes on disk;
//! * every entry is a v7-style 512-byte tar header (no `ustar` magic)
//!   followed by the payload; segment entries pad *after* the payload
//!   with the unpadded size in the header, while the binary references
//!   and graph trailers pad *before* the payload with the padded size in
//!   the header, and the index payload is internally front-padded;
//! * graph edges and binary references accumulate in memory and reach the
//!   disk only in `close()`, in the mandatory trailer order: `.brf`, then
//!   `.gph`, then `.idx`, then two zero blocks;
//! * index entries are sorted by *signed* UUID halves;
//! * segments in the open file are readable back through positional
//!   reads.
//!
//! One deliberate improvement over Oak: `close()` forces the file to disk
//! before closing (Oak leaves trailer durability to the operating
//! system); this is strictly more durable and changes no bytes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::checksum::crc32;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::writer::segment_builder::GarbageCollectionGeneration;

/// The tar block size.
const BLOCK_SIZE: usize = 512;

/// Serialized size of a version 2 index entry.
const INDEX_ENTRY_SIZE: usize = 33;

/// Size of the trailer footers.
const FOOTER_SIZE: usize = 16;

/// Binary references grouped by generation triple, then by segment.
type BinaryReferencesByGeneration =
    BTreeMap<(i32, i32, bool), BTreeMap<(u64, u64), BTreeSet<String>>>;

/// One segment recorded for the index, in insertion order.
struct PendingIndexEntry {
    identifier: SegmentIdentifier,
    /// File offset of the first payload byte.
    data_offset: u32,
    size: u32,
    generation: GarbageCollectionGeneration,
}

/// Writes one segment archive file.
pub struct TarArchiveWriter {
    path: PathBuf,
    file_name: String,
    file: Option<File>,
    length: u64,
    index_entries: Vec<PendingIndexEntry>,
    index_lookup: HashMap<SegmentIdentifier, usize>,
    /// Graph edges in deterministic order (readers accept any order).
    graph_edges: BTreeMap<(u64, u64), BTreeSet<(u64, u64)>>,
    /// Binary references: generation triple to segment to references.
    binary_references: BinaryReferencesByGeneration,
    closed: bool,
}

impl TarArchiveWriter {
    /// Creates a writer for `file_name` inside `directory`. No file is
    /// created until the first segment is written.
    #[must_use]
    pub fn new(directory: &Path, file_name: &str) -> Self {
        Self {
            path: directory.join(file_name),
            file_name: file_name.to_owned(),
            file: None,
            length: 0,
            index_entries: Vec::new(),
            index_lookup: HashMap::new(),
            graph_edges: BTreeMap::new(),
            binary_references: BTreeMap::new(),
            closed: false,
        }
    }

    /// The archive's file name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Whether the file has been created (a segment has been written).
    #[must_use]
    pub fn is_created(&self) -> bool {
        self.file.is_some()
    }

    /// The current file length in bytes.
    #[must_use]
    pub fn length(&self) -> u64 {
        self.length
    }

    /// The number of segments written.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.index_entries.len()
    }

    /// Whether the archive holds the given segment.
    #[must_use]
    pub fn contains_segment(&self, identifier: SegmentIdentifier) -> bool {
        self.index_lookup.contains_key(&identifier)
    }

    /// Writes one segment entry, registering its graph edges and binary
    /// references for the trailers. Returns the file length after the
    /// write — the caller decides on rotation.
    pub fn write_segment(
        &mut self,
        identifier: SegmentIdentifier,
        content: &[u8],
        generation: GarbageCollectionGeneration,
        referenced_segments: &[SegmentIdentifier],
        binary_reference_identifiers: &[String],
    ) -> Result<u64> {
        if self.closed {
            return Err(Error::InvalidFormat {
                details: format!("archive {} is already closed", self.file_name),
            });
        }
        if self.file.is_none() {
            self.file = Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .read(true)
                    .write(true)
                    .open(&self.path)?,
            );
        }

        let entry_name = format!("{identifier}.{:08x}", crc32(content));
        let header = entry_header(&entry_name, content.len() as u64);
        let file = self.file.as_mut().ok_or_else(|| Error::InvalidFormat {
            details: "the archive file disappeared after creation".to_owned(),
        })?;
        file.write_all(&header)?;
        let data_offset = self.length + BLOCK_SIZE as u64;
        file.write_all(content)?;
        let padding = padding_size(content.len());
        file.write_all(&vec![0u8; padding])?;
        self.length = data_offset + content.len() as u64 + padding as u64;
        if self.length > i32::MAX as u64 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "archive {} exceeds the 2 GiB limit; rotation is overdue",
                    self.file_name
                ),
            });
        }

        let position = self.index_entries.len();
        self.index_entries.push(PendingIndexEntry {
            identifier,
            data_offset: data_offset as u32,
            size: content.len() as u32,
            generation,
        });
        self.index_lookup.insert(identifier, position);

        let source = (
            identifier.most_significant_bits,
            identifier.least_significant_bits,
        );
        for referenced in referenced_segments {
            self.graph_edges.entry(source).or_default().insert((
                referenced.most_significant_bits,
                referenced.least_significant_bits,
            ));
        }
        if !binary_reference_identifiers.is_empty() {
            let generation_key = (
                generation.generation,
                generation.full_generation,
                generation.is_compacted,
            );
            let segment_references = self
                .binary_references
                .entry(generation_key)
                .or_default()
                .entry(source)
                .or_default();
            for reference in binary_reference_identifiers {
                segment_references.insert(reference.clone());
            }
        }
        Ok(self.length)
    }

    /// Reads a segment back from the open file.
    pub fn read_segment(&self, identifier: SegmentIdentifier) -> Result<Option<Vec<u8>>> {
        let Some(&position) = self.index_lookup.get(&identifier) else {
            return Ok(None);
        };
        let entry = &self.index_entries[position];
        let file = self.file.as_ref().ok_or_else(|| Error::InvalidFormat {
            details: "index entries exist but no file was created".to_owned(),
        })?;
        let mut content = vec![0u8; entry.size as usize];
        read_at_position(file, u64::from(entry.data_offset), &mut content)?;
        Ok(Some(content))
    }

    /// Forces written bytes to disk.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(file) = &self.file {
            file.sync_data()?;
        }
        Ok(())
    }

    /// Finalizes the archive: binary references, graph, index, the two
    /// terminating zero blocks, and an fsync. Returns whether a file was
    /// written at all.
    pub fn close(mut self) -> Result<bool> {
        self.closed = true;
        let Some(mut file) = self.file.take() else {
            return Ok(false);
        };

        let references_payload = self.serialize_binary_references();
        write_trailer_entry(
            &mut file,
            &format!("{}.brf", self.file_name),
            &references_payload,
        )?;
        let graph_payload = self.serialize_graph();
        write_trailer_entry(
            &mut file,
            &format!("{}.gph", self.file_name),
            &graph_payload,
        )?;

        let index_payload = self.serialize_index();
        let header = entry_header(
            &format!("{}.idx", self.file_name),
            index_payload.len() as u64,
        );
        file.write_all(&header)?;
        file.write_all(&index_payload)?;

        file.write_all(&[0u8; 2 * BLOCK_SIZE])?;
        file.sync_all()?;
        Ok(true)
    }

    /// Serializes the binary references payload (version 2, unpadded).
    fn serialize_binary_references(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        for ((generation, full_generation, compacted), segments) in &self.binary_references {
            payload.extend_from_slice(&generation.to_be_bytes());
            payload.extend_from_slice(&full_generation.to_be_bytes());
            payload.push(u8::from(*compacted));
            payload.extend_from_slice(&(segments.len() as u32).to_be_bytes());
            for ((most, least), references) in segments {
                payload.extend_from_slice(&most.to_be_bytes());
                payload.extend_from_slice(&least.to_be_bytes());
                payload.extend_from_slice(&(references.len() as u32).to_be_bytes());
                for reference in references {
                    let encoded = reference.as_bytes();
                    payload.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
                    payload.extend_from_slice(encoded);
                }
            }
        }
        let checksum = crc32(&payload);
        payload.extend_from_slice(&checksum.to_be_bytes());
        payload.extend_from_slice(&(self.binary_references.len() as u32).to_be_bytes());
        payload.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        payload.extend_from_slice(&0x0A31_420Au32.to_be_bytes());
        payload
    }

    /// Serializes the graph payload (unpadded).
    fn serialize_graph(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        for ((most, least), targets) in &self.graph_edges {
            payload.extend_from_slice(&most.to_be_bytes());
            payload.extend_from_slice(&least.to_be_bytes());
            payload.extend_from_slice(&(targets.len() as u32).to_be_bytes());
            for (target_most, target_least) in targets {
                payload.extend_from_slice(&target_most.to_be_bytes());
                payload.extend_from_slice(&target_least.to_be_bytes());
            }
        }
        let checksum = crc32(&payload);
        payload.extend_from_slice(&checksum.to_be_bytes());
        payload.extend_from_slice(&(self.graph_edges.len() as u32).to_be_bytes());
        payload.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        payload.extend_from_slice(&0x0A30_470Au32.to_be_bytes());
        payload
    }

    /// Serializes the complete index payload: front padding, entries
    /// sorted by signed UUID halves, and the footer.
    fn serialize_index(&self) -> Vec<u8> {
        let data_size = self.index_entries.len() * INDEX_ENTRY_SIZE + FOOTER_SIZE;
        let total_size = data_size.div_ceil(BLOCK_SIZE) * BLOCK_SIZE;

        let mut sorted: Vec<&PendingIndexEntry> = self.index_entries.iter().collect();
        sorted.sort_by_key(|entry| {
            (
                entry.identifier.most_significant_bits as i64,
                entry.identifier.least_significant_bits as i64,
            )
        });

        let mut entries_bytes = Vec::with_capacity(self.index_entries.len() * INDEX_ENTRY_SIZE);
        for entry in sorted {
            entries_bytes.extend_from_slice(&entry.identifier.most_significant_bits.to_be_bytes());
            entries_bytes.extend_from_slice(&entry.identifier.least_significant_bits.to_be_bytes());
            entries_bytes.extend_from_slice(&entry.data_offset.to_be_bytes());
            entries_bytes.extend_from_slice(&entry.size.to_be_bytes());
            entries_bytes.extend_from_slice(&entry.generation.generation.to_be_bytes());
            entries_bytes.extend_from_slice(&entry.generation.full_generation.to_be_bytes());
            entries_bytes.push(u8::from(entry.generation.is_compacted));
        }

        let mut payload = vec![0u8; total_size - data_size];
        payload.extend_from_slice(&entries_bytes);
        payload.extend_from_slice(&crc32(&entries_bytes).to_be_bytes());
        payload.extend_from_slice(&(self.index_entries.len() as u32).to_be_bytes());
        payload.extend_from_slice(&(total_size as u32).to_be_bytes());
        payload.extend_from_slice(&0x0A31_4B0Au32.to_be_bytes());
        payload
    }
}

/// Writes a `.brf`/`.gph` trailer entry: header with the padded size,
/// then front padding, then the payload.
fn write_trailer_entry(file: &mut File, entry_name: &str, payload: &[u8]) -> Result<()> {
    let padding = padding_size(payload.len());
    let header = entry_header(entry_name, (payload.len() + padding) as u64);
    file.write_all(&header)?;
    file.write_all(&vec![0u8; padding])?;
    file.write_all(payload)?;
    Ok(())
}

/// The zero padding needed after `size` payload bytes.
fn padding_size(size: usize) -> usize {
    match size % BLOCK_SIZE {
        0 => 0,
        remainder => BLOCK_SIZE - remainder,
    }
}

/// Builds the 512-byte v7-style tar entry header Oak writes: name, mode
/// `0000400`, zero uid/gid, octal size and modification time, checksum
/// as six octal digits plus NUL plus space, and typeflag `'0'`.
fn entry_header(name: &str, size: u64) -> [u8; BLOCK_SIZE] {
    let mut header = [0u8; BLOCK_SIZE];
    let name_bytes = name.as_bytes();
    let name_length = name_bytes.len().min(100);
    header[..name_length].copy_from_slice(&name_bytes[..name_length]);
    header[100..107].copy_from_slice(b"0000400");
    header[108..115].copy_from_slice(b"0000000");
    header[116..123].copy_from_slice(b"0000000");
    let size_field = format!("{size:011o}");
    header[124..124 + size_field.len()].copy_from_slice(size_field.as_bytes());
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let time_field = format!("{seconds:011o}");
    header[136..136 + time_field.len()].copy_from_slice(time_field.as_bytes());
    header[148..156].copy_from_slice(b"        ");
    header[156] = b'0';
    let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
    let checksum_field = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum_field.as_bytes());
    header
}

/// Reads exactly `target.len()` bytes at `position` without moving the
/// file cursor.
fn read_at_position(file: &File, position: u64, target: &mut [u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(target, position)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut clone = file.try_clone()?;
        clone.seek(SeekFrom::Start(position))?;
        clone.read_exact(target)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::TarArchiveWriter;
    use crate::segment::identifier::SegmentIdentifier;
    use crate::tar_archive::archive::TarArchiveReader;
    use crate::writer::segment_builder::GarbageCollectionGeneration;

    fn test_generation() -> GarbageCollectionGeneration {
        GarbageCollectionGeneration {
            generation: 2,
            full_generation: 1,
            is_compacted: true,
        }
    }

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("froe-tar-writer-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn data_identifier(seed: u64) -> SegmentIdentifier {
        SegmentIdentifier::new(seed, 0xA000_0000_0000_0000 | seed)
    }

    #[test]
    fn written_archives_reopen_through_their_index() {
        let directory = TestDirectory::new("reopen");
        let mut writer = TarArchiveWriter::new(&directory.path, "data00000a.tar");

        let first = data_identifier(1);
        let second = data_identifier(2);
        let first_content = vec![0x11u8; 100];
        let second_content = vec![0x22u8; 700];
        writer
            .write_segment(first, &first_content, test_generation(), &[second], &[])
            .expect("write first");
        writer
            .write_segment(
                second,
                &second_content,
                test_generation(),
                &[],
                &["blob-reference-one".to_owned()],
            )
            .expect("write second");

        assert!(writer.contains_segment(first));
        assert_eq!(
            writer
                .read_segment(first)
                .expect("read back")
                .expect("present"),
            first_content,
            "segments read back from the open file"
        );
        assert!(writer.close().expect("close"), "a file was written");

        let reader = TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open");
        assert!(!reader.is_recovered(), "the written index must validate");
        assert_eq!(reader.segment_count(), 2);
        assert_eq!(
            reader.segment_data(first).expect("first"),
            &first_content[..]
        );
        assert_eq!(
            reader.segment_data(second).expect("second"),
            &second_content[..]
        );

        let index_entry = reader.index_entry(first).expect("entry");
        assert_eq!(index_entry.generation, 2);
        assert_eq!(index_entry.full_generation, 1);
        assert!(index_entry.is_compacted);

        let graph = reader.segment_graph().expect("graph parses");
        assert_eq!(graph.as_map()[&first], &[second]);

        let references = reader.binary_references().expect("references parse");
        assert_eq!(references.generations.len(), 1);
        assert_eq!(
            references.generations[0].segments[0].1,
            vec!["blob-reference-one"]
        );
    }

    #[test]
    fn untouched_writers_leave_no_file() {
        let directory = TestDirectory::new("untouched");
        let writer = TarArchiveWriter::new(&directory.path, "data00000a.tar");
        assert!(!writer.close().expect("close"), "no file was written");
        assert!(!directory.path.join("data00000a.tar").exists());
    }

    #[test]
    fn recovery_scan_agrees_with_the_index() {
        // Truncate the trailers off a written archive; the recovery scan
        // must still find every segment by name checksum.
        let directory = TestDirectory::new("recovery-parity");
        let mut writer = TarArchiveWriter::new(&directory.path, "data00000a.tar");
        let identifier = data_identifier(7);
        let content = vec![0x77u8; 300];
        writer
            .write_segment(identifier, &content, test_generation(), &[], &[])
            .expect("write");
        let data_length = writer.length();
        writer.close().expect("close");

        let path = directory.path.join("data00000a.tar");
        let full = std::fs::read(&path).expect("read archive");
        let mut truncated = full[..data_length as usize].to_vec();
        truncated.extend_from_slice(&[0u8; 1024]);
        std::fs::write(&path, &truncated).expect("write truncated");

        let reader = TarArchiveReader::open(&path).expect("open");
        assert!(reader.is_recovered());
        assert_eq!(
            reader.segment_data(identifier).expect("segment"),
            &content[..]
        );
    }

    #[test]
    fn empty_trailer_structures_parse() {
        let directory = TestDirectory::new("empty-trailers");
        let mut writer = TarArchiveWriter::new(&directory.path, "data00000a.tar");
        let identifier = data_identifier(3);
        writer
            .write_segment(identifier, &[0xABu8; 64], test_generation(), &[], &[])
            .expect("write");
        writer.close().expect("close");

        let reader = TarArchiveReader::open(&directory.path.join("data00000a.tar")).expect("open");
        let graph = reader.segment_graph().expect("empty graph parses");
        assert!(graph.adjacency.is_empty());
        let references = reader.binary_references().expect("empty references parse");
        assert!(references.generations.is_empty());
    }
}
