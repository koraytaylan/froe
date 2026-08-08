//! Reading one segment archive (`data00000a.tar`).
//!
//! The fast path memory-maps the file and locates segments through the
//! index at the end of the archive. When no valid index exists — a corrupt
//! file, or the archive a live repository is currently writing (its index
//! is only written on close) — the reader falls back to a linear *recovery
//! scan* over the tar entry headers, validating each candidate segment
//! against the CRC32 checksum embedded in its entry name.
//!
//! Unlike the Java implementation, which persists recovered entries to a
//! `.ro.bak` file even when opened read-only, this reader keeps recovery
//! results in memory and never writes to the repository directory.
//!
//! The memory mapping relies on the segment store's file protocol —
//! shared with Java, whose `FileChannel.map` carries the identical
//! assumption: existing archive bytes are immutable, a live writer only
//! ever *appends* (beyond the mapped length) or replaces whole files via
//! rename (which keeps this mapping on the old inode). A process that
//! truncates or rewrites an archive in place steps outside that protocol
//! and can fault the mapping — the same failure mode a running Oak
//! instance would suffer.

use std::collections::HashMap;
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::checksum::crc32;
use crate::error::{Error, Result};
use crate::segment::identifier::SegmentIdentifier;
use crate::tar_archive::binary_references::{BinaryReferences, parse_binary_references};
use crate::tar_archive::entry_header::TarEntryHeader;
use crate::tar_archive::graph::{SegmentGraph, parse_segment_graph};
use crate::tar_archive::index::{
    SegmentIndex, SegmentIndexEntry, index_entry_disk_size, parse_segment_index,
};

/// How the segments of an archive were located.
enum ArchiveContent {
    /// The archive has a valid index; segments are looked up through it.
    Indexed(SegmentIndex),
    /// The archive had no valid index; segments were recovered by scanning
    /// tar entry headers. Ranges point into the memory-mapped file.
    Recovered {
        /// Recovered segments in scan order.
        entries: Vec<(SegmentIdentifier, Range<usize>)>,
        /// Segment identifier to position in `entries`.
        lookup: HashMap<SegmentIdentifier, usize>,
    },
}

/// A read-only view of one segment archive.
pub struct TarArchiveReader {
    path: PathBuf,
    file_name: String,
    bytes: memmap2::Mmap,
    content: ArchiveContent,
}

impl TarArchiveReader {
    /// Opens an archive file, trying the index first and falling back to
    /// the recovery scan.
    pub fn open(path: &Path) -> Result<Self> {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file = File::open(path)?;
        let length = file.metadata()?.len();
        if length == 0 {
            return Err(Error::InvalidFormat {
                details: format!("archive {file_name} is empty"),
            });
        }
        // Archives beyond the 2 GiB index addressing limit cannot have a
        // valid index (positions are 32-bit), but the Java reader still
        // recovers their segments with the full scan — so no size check
        // here; index parsing rejects the file and recovery takes over.
        // SAFETY: the mapping is read-only and the segment store's file
        // protocol makes the mapped bytes immutable — Oak only ever
        // appends new files, appends to the archive it is currently
        // filling (beyond our mapped length), or replaces whole files by
        // rename, which leaves this mapping on the old inode. This is
        // the same assumption Java's FileChannel.map makes for the same
        // files. What the protocol cannot rule out — an unrelated
        // process truncating or rewriting the file in place — would
        // fault this mapping exactly as it would fault a running Oak
        // instance; froe accepts that shared residual risk rather than
        // give up zero-copy segment access.
        let bytes = unsafe { memmap2::Mmap::map(&file)? };

        let content = if let Ok(index) = parse_segment_index(&bytes) {
            ArchiveContent::Indexed(index)
        } else {
            let (entries, lookup) = recover_segment_entries(&bytes, &file_name);
            ArchiveContent::Recovered { entries, lookup }
        };
        Ok(Self {
            path: path.to_owned(),
            file_name,
            bytes,
            content,
        })
    }

    /// The path this archive was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The archive's file name, for example `data00000a.tar`.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The size of the archive file in bytes.
    #[must_use]
    pub fn file_size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// The archive's index, or `None` when the archive was opened through
    /// the recovery scan.
    #[must_use]
    pub fn index(&self) -> Option<&SegmentIndex> {
        match &self.content {
            ArchiveContent::Indexed(index) => Some(index),
            ArchiveContent::Recovered { .. } => None,
        }
    }

    /// Whether the archive was opened through the recovery scan instead of
    /// a valid index.
    #[must_use]
    pub fn is_recovered(&self) -> bool {
        matches!(self.content, ArchiveContent::Recovered { .. })
    }

    /// The number of segments this archive provides.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        match &self.content {
            ArchiveContent::Indexed(index) => index.entries().len(),
            ArchiveContent::Recovered { entries, .. } => entries.len(),
        }
    }

    /// Whether the archive contains the given segment.
    #[must_use]
    pub fn contains_segment(&self, segment_identifier: SegmentIdentifier) -> bool {
        match &self.content {
            ArchiveContent::Indexed(index) => index.find_entry(segment_identifier).is_some(),
            ArchiveContent::Recovered { lookup, .. } => lookup.contains_key(&segment_identifier),
        }
    }

    /// The raw bytes of the given segment, or `None` when this archive does
    /// not contain it.
    #[must_use]
    pub fn segment_data(&self, segment_identifier: SegmentIdentifier) -> Option<&[u8]> {
        match &self.content {
            ArchiveContent::Indexed(index) => {
                let entry = index.find_entry(segment_identifier)?;
                let start = entry.position as usize;
                let end = start.checked_add(entry.size as usize)?;
                self.bytes.get(start..end)
            }
            ArchiveContent::Recovered { entries, lookup } => {
                let position = *lookup.get(&segment_identifier)?;
                self.bytes.get(entries[position].1.clone())
            }
        }
    }

    /// All segment identifiers this archive provides, in index order for
    /// indexed archives and in scan order for recovered ones.
    pub fn segment_identifiers(&self) -> impl Iterator<Item = SegmentIdentifier> + '_ {
        let indexed: Option<&SegmentIndex> = self.index();
        let recovered = match &self.content {
            ArchiveContent::Indexed(_) => &[][..],
            ArchiveContent::Recovered { entries, .. } => entries.as_slice(),
        };
        indexed
            .into_iter()
            .flat_map(|index| index.entries().iter().map(|entry| entry.segment_identifier))
            .chain(recovered.iter().map(|(identifier, _)| *identifier))
    }

    /// The index entry for a segment, when the archive has an index.
    #[must_use]
    pub fn index_entry(&self, segment_identifier: SegmentIdentifier) -> Option<&SegmentIndexEntry> {
        self.index()?.find_entry(segment_identifier)
    }

    /// Parses the archive's segment graph, when present and valid.
    #[must_use]
    pub fn segment_graph(&self) -> Option<SegmentGraph> {
        parse_segment_graph(&self.bytes, self.index()?)
    }

    /// Parses the archive's binary references catalog, when present and
    /// valid. Locating the catalog requires a valid graph, because the
    /// catalog sits immediately before the graph entry.
    #[must_use]
    pub fn binary_references(&self) -> Option<BinaryReferences> {
        let index = self.index()?;
        let graph = self.segment_graph()?;
        let graph_disk_size = 512 + graph.disk_structure_size().div_ceil(512) * 512;
        let anchor = self
            .bytes
            .len()
            .checked_sub(1024 + index_entry_disk_size(index) + graph_disk_size)?;
        parse_binary_references(&self.bytes, anchor)
    }
}

/// The name of a segment entry inside an archive:
/// `<uuid>` optionally followed by `.<crc32 as exactly 8 lowercase hex
/// digits>` and optionally a further `.`-prefixed suffix.
fn parse_segment_entry_name(name: &str) -> Option<(SegmentIdentifier, Option<u32>)> {
    // Entry names decode lossily from the tar header, so they may contain
    // multi-byte characters at any position; guard every split against
    // char boundaries instead of panicking.
    if name.len() < 36 || !name.is_char_boundary(36) {
        return None;
    }
    let (uuid_text, rest) = name.split_at(36);
    let identifier: SegmentIdentifier = uuid_text.parse().ok()?;
    if rest.is_empty() {
        return Some((identifier, None));
    }
    let suffix = rest.strip_prefix('.')?;
    // A checksum group must be exactly eight lowercase hexadecimal digits,
    // either ending the name or followed by another dotted suffix.
    if suffix.len() >= 8 && suffix.is_char_boundary(8) {
        let (checksum_text, remainder) = suffix.split_at(8);
        let is_checksum = checksum_text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && (remainder.is_empty() || remainder.starts_with('.'));
        if is_checksum {
            let checksum = u32::from_str_radix(checksum_text, 16).ok()?;
            return Some((identifier, Some(checksum)));
        }
    }
    // No checksum group: the whole rest must be a dotted suffix, which it
    // is (it starts with '.').
    Some((identifier, None))
}

/// Scans an archive without a valid index and collects every segment entry
/// whose name (and, when present, name-embedded checksum) is valid.
///
/// This mirrors the Java `SegmentTarManager.recoverEntries` byte for byte,
/// including its quirks: a later duplicate of a segment replaces the
/// earlier copy only when its name carries a checksum, and entries that are
/// neither segments nor the archive's own `.idx` entry are skipped whole
/// while the `.idx` entry is *not* skipped (the scanner walks into its
/// payload, which the name filters then discard).
/// Recovered segments in scan order, plus a lookup from identifier to the
/// segment's position in that order.
type RecoveredSegments = (
    Vec<(SegmentIdentifier, Range<usize>)>,
    HashMap<SegmentIdentifier, usize>,
);

fn recover_segment_entries(bytes: &[u8], archive_file_name: &str) -> RecoveredSegments {
    let mut entries: Vec<(SegmentIdentifier, Range<usize>)> = Vec::new();
    let mut lookup: HashMap<SegmentIdentifier, usize> = HashMap::new();
    let index_entry_name = format!("{archive_file_name}.idx");
    let length = bytes.len();
    let mut position = 0usize;

    while position + 512 <= length {
        let header_bytes = &bytes[position..position + 512];
        let position_after_header = position + 512;
        let Some(header) = TarEntryHeader::parse(header_bytes) else {
            // An all-zero block. The Java scanner only stops when exactly
            // two more blocks follow to the end of the file; otherwise the
            // block falls through as an entry with an empty name, which the
            // filters below skip.
            if position_after_header + 1024 == length {
                break;
            }
            position = position_after_header;
            continue;
        };

        // The size may be negative: the Java scanner accumulates it in a
        // wrapping 32-bit integer and its arithmetic tolerates the result.
        let size = header.size;
        if position_after_header as i64 + size > length as i64 {
            // Truncated final entry: ignore it and stop the scan.
            break;
        }
        // The distance the Java scanner seeks past an entry's data: the
        // size rounded up to the block boundary with truncating division
        // (zero or negative for wrapped sizes). Forward progress of at
        // least one block is enforced where Java could seek backwards and
        // loop forever.
        let padded_size = ((size + 511) / 512) * 512;
        let skip_past_data = |from: usize| -> usize {
            let target = from as i64 + padded_size;
            if target > from as i64 {
                target as usize
            } else {
                from
            }
        };

        if let Some((identifier, checksum)) = parse_segment_entry_name(&header.name) {
            if size >= 0 && (checksum.is_some() || !lookup.contains_key(&identifier)) {
                let size = size as usize;
                let data = position_after_header..position_after_header + size;
                let next_position = skip_past_data(position_after_header);
                if let Some(expected) = checksum
                    && crc32(&bytes[data.clone()]) != expected
                {
                    // Corrupt segment: drop it and continue scanning.
                    position = next_position;
                    continue;
                }
                if let Some(&existing) = lookup.get(&identifier) {
                    entries[existing].1 = data;
                } else {
                    lookup.insert(identifier, entries.len());
                    entries.push((identifier, data));
                }
                position = next_position;
            } else {
                // Two cases advance by only the header block: a
                // checksum-less duplicate of an already recovered segment
                // (the Java scanner reads nothing and does not skip the
                // data, so the next iteration parses the segment's first
                // data block as a header — kept for bug compatibility),
                // and a wrapped-negative size, which Java cannot read
                // either.
                position = position_after_header;
            }
        } else if header.name != index_entry_name {
            // Unknown entry (including `.gph` and `.brf`): skip it whole.
            position = skip_past_data(position_after_header);
        } else {
            // The `.idx` entry is not skipped; the scanner walks into its
            // payload. Kept for bug compatibility with the Java scanner.
            position = position_after_header;
        }
    }
    (entries, lookup)
}

#[cfg(test)]
mod tests {
    use super::{TarArchiveReader, parse_segment_entry_name, recover_segment_entries};
    use crate::checksum::crc32;
    use crate::segment::identifier::SegmentIdentifier;

    fn tar_entry(name: &str, data: &[u8]) -> Vec<u8> {
        let mut block = vec![0u8; 512];
        block[..name.len()].copy_from_slice(name.as_bytes());
        let size_field = format!("{:011o}\0", data.len());
        block[124..136].copy_from_slice(size_field.as_bytes());
        block.extend_from_slice(data);
        block.extend(std::iter::repeat_n(
            0u8,
            data.len().div_ceil(512) * 512 - data.len(),
        ));
        block
    }

    fn segment_entry_name(identifier: SegmentIdentifier, data: &[u8]) -> String {
        format!("{identifier}.{:08x}", crc32(data))
    }

    #[test]
    fn parses_segment_entry_names() {
        let identifier = SegmentIdentifier::new(0xF813_78FB_92B1_4B52, 0xA5C8_E0A6_7152_ED2C);
        let bare = identifier.to_string();
        assert_eq!(parse_segment_entry_name(&bare), Some((identifier, None)));

        let with_checksum = format!("{bare}.0012abcd");
        assert_eq!(
            parse_segment_entry_name(&with_checksum),
            Some((identifier, Some(0x0012_ABCD)))
        );

        let with_suffix = format!("{bare}.0012abcd.future");
        assert_eq!(
            parse_segment_entry_name(&with_suffix),
            Some((identifier, Some(0x0012_ABCD)))
        );

        let nine_hexadecimal_digits = format!("{bare}.0012abcde");
        assert_eq!(
            parse_segment_entry_name(&nine_hexadecimal_digits),
            Some((identifier, None))
        );

        let uppercase_checksum = format!("{bare}.0012ABCD");
        assert_eq!(
            parse_segment_entry_name(&uppercase_checksum),
            Some((identifier, None))
        );

        assert_eq!(parse_segment_entry_name("data00000a.tar.idx"), None);
        assert_eq!(parse_segment_entry_name(""), None);
    }

    #[test]
    fn recovery_scan_collects_valid_segments() {
        let first = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let second = SegmentIdentifier::new(2, 0xA000_0000_0000_0002);
        let first_data = vec![0x11u8; 100];
        let second_data = vec![0x22u8; 700];

        let mut archive = Vec::new();
        archive.extend(tar_entry(
            &segment_entry_name(first, &first_data),
            &first_data,
        ));
        archive.extend(tar_entry(
            &segment_entry_name(second, &second_data),
            &second_data,
        ));
        archive.extend_from_slice(&[0u8; 1024]);

        let (entries, lookup) = recover_segment_entries(&archive, "data00000a.tar");
        assert_eq!(entries.len(), 2);
        assert!(lookup.contains_key(&first));
        assert_eq!(archive[entries[lookup[&second]].1.clone()], second_data[..]);
    }

    #[test]
    fn recovery_scan_drops_segments_with_wrong_checksum() {
        let identifier = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let data = vec![0x33u8; 64];
        let wrong_name = format!("{identifier}.deadbeef");
        let mut archive = tar_entry(&wrong_name, &data);
        archive.extend_from_slice(&[0u8; 1024]);

        let (entries, _) = recover_segment_entries(&archive, "data00000a.tar");
        assert!(entries.is_empty());
    }

    #[test]
    fn recovery_scan_replaces_duplicates_with_checksum() {
        let identifier = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let old_data = vec![0x44u8; 32];
        let new_data = vec![0x55u8; 32];
        let mut archive = Vec::new();
        archive.extend(tar_entry(
            &segment_entry_name(identifier, &old_data),
            &old_data,
        ));
        archive.extend(tar_entry(
            &segment_entry_name(identifier, &new_data),
            &new_data,
        ));
        archive.extend_from_slice(&[0u8; 1024]);

        let (entries, lookup) = recover_segment_entries(&archive, "data00000a.tar");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            archive[entries[lookup[&identifier]].1.clone()],
            new_data[..]
        );
    }

    #[test]
    fn recovery_scan_skips_metadata_and_stops_at_truncation() {
        let identifier = SegmentIdentifier::new(1, 0xA000_0000_0000_0001);
        let data = vec![0x66u8; 16];
        let mut archive = Vec::new();
        archive.extend(tar_entry("data00000a.tar.brf", &[0u8; 32]));
        archive.extend(tar_entry(&segment_entry_name(identifier, &data), &data));
        // Truncated final entry: header claims more data than the file has.
        let mut truncated = vec![0u8; 512];
        truncated[..4].copy_from_slice(b"tail");
        truncated[124..136].copy_from_slice(b"00000010000\0");
        archive.extend(truncated);

        let (entries, _) = recover_segment_entries(&archive, "data00000a.tar");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, identifier);
    }

    #[test]
    fn recovery_scan_continues_past_wrapped_size_fields() {
        // An entry whose octal size wraps negative in 32-bit arithmetic
        // must not end the scan: the Java scanner wraps the same way and
        // still recovers the segments that follow.
        let identifier = SegmentIdentifier::new(2, 0xA000_0000_0000_0002);
        let data = vec![0x42u8; 64];
        let mut wrapped = vec![0u8; 512];
        wrapped[..7].copy_from_slice(b"strange");
        wrapped[124..136].copy_from_slice(b"77777777777\0");
        let mut archive = wrapped;
        archive.extend(tar_entry(&segment_entry_name(identifier, &data), &data));
        archive.extend_from_slice(&[0u8; 1024]);

        let (entries, _) = recover_segment_entries(&archive, "data00000a.tar");
        assert_eq!(
            entries.len(),
            1,
            "the segment after the wrapped entry is recovered"
        );
        assert_eq!(entries[0].0, identifier);
    }

    #[test]
    fn recovery_scan_survives_non_utf8_entry_names() {
        // A header name of 35 ASCII bytes followed by an invalid byte
        // decodes lossily with a replacement character across byte 36;
        // the scan must skip it, not panic.
        let mut hostile = vec![0u8; 512];
        hostile[..35].copy_from_slice(&[b'a'; 35]);
        hostile[35] = 0xFF;
        hostile[124..136].copy_from_slice(b"00000000000\0");
        let mut archive = hostile;
        archive.extend_from_slice(&[0u8; 1024]);

        let (entries, _) = recover_segment_entries(&archive, "data00000a.tar");
        assert!(entries.is_empty());
    }

    #[test]
    fn opens_archive_files_through_recovery_when_index_is_missing() {
        let identifier = SegmentIdentifier::new(7, 0xA000_0000_0000_0007);
        let data = vec![0x77u8; 256];
        let mut archive = tar_entry(&segment_entry_name(identifier, &data), &data);
        archive.extend_from_slice(&[0u8; 1024]);

        let directory = std::env::temp_dir().join(format!(
            "froe-archive-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("data00000a.tar");
        std::fs::write(&path, &archive).expect("write test archive");

        let reader = TarArchiveReader::open(&path).expect("open archive");
        assert!(reader.is_recovered());
        assert_eq!(reader.segment_count(), 1);
        assert!(reader.contains_segment(identifier));
        assert_eq!(
            reader.segment_data(identifier).expect("segment data"),
            &data[..]
        );

        std::fs::remove_dir_all(&directory).expect("remove test directory");
    }
}
