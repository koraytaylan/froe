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
use crate::tar_archive::graph::{
    BoundedSegmentGraph, SegmentGraph, parse_segment_graph, parse_segment_graph_with_limits,
};
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
        /// Why the index was rejected, from [`parse_segment_index`].
        ///
        /// Kept because "no index" alone cannot separate the two cases an
        /// operator has to tell apart: a trailer that was never written —
        /// a writer killed before it closed the archive, where every byte
        /// is still present — from one that is present and no longer
        /// validates, which is real damage. The rejection reason is the
        /// only place that distinction survives, and it used to be
        /// discarded at the point of the fallback.
        reason: String,
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
        let file = File::open(path)?;
        Self::open_file(path, &file)
    }

    /// Opens an archive from an already-held file descriptor while retaining
    /// `path` as its logical name. Maintenance publication uses this to bind a
    /// validation certificate to the exact inode it will later publish rather
    /// than reopening a replaceable pathname between validation steps.
    pub(crate) fn open_file(path: &Path, file: &File) -> Result<Self> {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
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
        let bytes = unsafe { memmap2::Mmap::map(file)? };

        let content = match parse_segment_index(&bytes) {
            Ok(index) => ArchiveContent::Indexed(index),
            Err(error) => {
                let (entries, lookup) = recover_segment_entries(&bytes, &file_name);
                ArchiveContent::Recovered {
                    // `parse_segment_index` only ever fails with
                    // `InvalidFormat`, whose `Display` prefixes "invalid
                    // segment-tar data:". The detail alone is kept so a
                    // caller can embed it in its own sentence without
                    // nesting that prefix inside another error.
                    reason: match error {
                        Error::InvalidFormat { details } => details,
                        other => other.to_string(),
                    },
                    entries,
                    lookup,
                }
            }
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

    /// Why the index was rejected, or `None` for an indexed archive.
    ///
    /// The distinction this carries is operational, not cosmetic. An
    /// unrecognized magic number on an archive with no terminating zero
    /// blocks is a trailer that was never written, which is what a killed
    /// writer leaves and what a rebuild restores losslessly. A checksum
    /// mismatch, or entries that do not sort, is a trailer that was written
    /// and no longer validates — the same symptom, the opposite prognosis.
    #[must_use]
    pub fn recovery_reason(&self) -> Option<&str> {
        match &self.content {
            ArchiveContent::Indexed(_) => None,
            ArchiveContent::Recovered { reason, .. } => Some(reason.as_str()),
        }
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
            ArchiveContent::Recovered {
                entries, lookup, ..
            } => {
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

    /// Validates the tar entry named by one index row, including the UUID,
    /// declared size, and payload CRC encoded in its entry name.
    ///
    /// The normal indexed read path intentionally trusts the index and jumps
    /// directly to payload bytes. Destructive maintenance needs the stronger
    /// certificate: a valid trailer index must not authorize deletion of an
    /// alternate archive when its referenced payload entry is stale or
    /// corrupt.
    #[allow(
        clippy::too_many_lines,
        reason = "the index/header/name/size/payload certificate is safest as one linear validation sequence"
    )]
    pub(crate) fn validate_indexed_segment_entry(
        &self,
        segment_identifier: SegmentIdentifier,
    ) -> crate::error::Result<()> {
        let entry = self.index_entry(segment_identifier).ok_or_else(|| {
            crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} has no index entry for segment {segment_identifier}",
                    self.file_name
                ),
            }
        })?;
        let payload_start = entry.position as usize;
        let header_start = payload_start.checked_sub(512).ok_or_else(|| {
            crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} indexes segment {segment_identifier} before a complete tar header",
                    self.file_name
                ),
            }
        })?;
        let header_end = header_start.checked_add(512).ok_or_else(|| {
            crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} overflows the tar-header range for segment {segment_identifier}",
                    self.file_name
                ),
            }
        })?;
        let header_block = self.bytes.get(header_start..header_end).ok_or_else(|| {
            crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} truncates the tar header for segment {segment_identifier}",
                    self.file_name
                ),
            }
        })?;
        let header = TarEntryHeader::parse(header_block).ok_or_else(|| {
            crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} has no tar entry header for indexed segment {segment_identifier}",
                    self.file_name
                ),
            }
        })?;
        let (header_identifier, expected_crc) = parse_segment_entry_name(&header.name)
            .and_then(|(identifier, checksum)| checksum.map(|checksum| (identifier, checksum)))
            .ok_or_else(|| crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} has malformed or checksum-free tar entry name {:?} for indexed segment {segment_identifier}",
                    self.file_name, header.name
                ),
            })?;
        if header_identifier != segment_identifier {
            return Err(crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} index names segment {segment_identifier}, but its tar entry names {header_identifier}",
                    self.file_name
                ),
            });
        }
        let canonical_name = format!("{segment_identifier}.{expected_crc:08x}");
        if header.name != canonical_name {
            return Err(crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} tar entry name {:?} is not the canonical indexed-segment name {canonical_name:?}",
                    self.file_name, header.name
                ),
            });
        }
        if header.size != i64::from(entry.size) {
            return Err(crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} index size {} disagrees with tar entry size {} for segment {segment_identifier}",
                    self.file_name, entry.size, header.size
                ),
            });
        }
        if !tar_header_checksum_is_valid(header_block) {
            return Err(crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} has an invalid tar-header checksum for segment {segment_identifier}",
                    self.file_name
                ),
            });
        }
        let payload_end = payload_start
            .checked_add(entry.size as usize)
            .ok_or_else(|| crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} overflows the payload range for segment {segment_identifier}",
                    self.file_name
                ),
            })?;
        let payload = self.bytes.get(payload_start..payload_end).ok_or_else(|| {
            crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} truncates the payload for segment {segment_identifier}",
                    self.file_name
                ),
            }
        })?;
        let actual_crc = crc32(payload);
        if actual_crc != expected_crc {
            return Err(crate::error::Error::InvalidFormat {
                details: format!(
                    "archive {} payload CRC {actual_crc:08x} disagrees with tar entry CRC {expected_crc:08x} for segment {segment_identifier}",
                    self.file_name
                ),
            });
        }
        Ok(())
    }

    /// The CRC32 recorded in a segment's tar entry name.
    ///
    /// Read from the name rather than recomputed: a caller that has already
    /// run [`Self::validate_indexed_segment_entry`] knows the payload hashes
    /// to this value, so comparing it against an independently recorded
    /// checksum identifies the payload without hashing it a second time.
    #[must_use]
    pub(crate) fn segment_entry_checksum(
        &self,
        segment_identifier: SegmentIdentifier,
    ) -> Option<u32> {
        let entry = self.index_entry(segment_identifier)?;
        let payload_start = entry.position as usize;
        let header_start = payload_start.checked_sub(512)?;
        let header_block = self.bytes.get(header_start..payload_start)?;
        let header = TarEntryHeader::parse(header_block)?;
        let (header_identifier, checksum) = parse_segment_entry_name(&header.name)?;
        (header_identifier == segment_identifier)
            .then_some(checksum)
            .flatten()
    }

    /// Parses the archive's segment graph, when present and valid.
    #[must_use]
    pub fn segment_graph(&self) -> Option<SegmentGraph> {
        parse_segment_graph(&self.bytes, self.index()?)
    }

    /// Parses the optional graph while bounding checksum and allocation work.
    pub(crate) fn segment_graph_with_limits(
        &self,
        maximum_work_units: u64,
        maximum_rows: usize,
        maximum_edges: usize,
    ) -> BoundedSegmentGraph {
        let Some(index) = self.index() else {
            return BoundedSegmentGraph::Unavailable { work_units: 0 };
        };
        parse_segment_graph_with_limits(
            &self.bytes,
            index,
            maximum_work_units,
            maximum_rows,
            maximum_edges,
        )
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

fn tar_header_checksum_is_valid(block: &[u8]) -> bool {
    let Some(block) = block.get(..512) else {
        return false;
    };
    let Ok(field) = std::str::from_utf8(&block[148..156]) else {
        return false;
    };
    let digits = field.trim_matches(['\0', ' ']);
    if digits.is_empty() || !digits.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return false;
    }
    let Ok(stored) = u32::from_str_radix(digits, 8) else {
        return false;
    };
    let calculated: u32 = block
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if (148..156).contains(&index) {
                u32::from(b' ')
            } else {
                u32::from(byte)
            }
        })
        .sum();
    stored == calculated
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
    use std::io::{Seek, SeekFrom, Write};

    use super::{TarArchiveReader, parse_segment_entry_name, recover_segment_entries};
    use crate::checksum::crc32;
    use crate::segment::identifier::SegmentIdentifier;
    use crate::writer::segment_builder::GarbageCollectionGeneration;
    use crate::writer::tar_writer::TarArchiveWriter;

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

    #[test]
    fn indexed_entry_validation_checks_the_name_size_and_payload_crc() {
        let directory = std::env::temp_dir().join(format!(
            "froe-indexed-entry-validation-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("data00000a.tar");
        let identifier = SegmentIdentifier::new(7, 0xA000_0000_0000_0007);
        let payload = vec![0x77; 64];
        let mut writer = TarArchiveWriter::new(&directory, "data00000a.tar");
        writer
            .write_segment(
                identifier,
                &payload,
                GarbageCollectionGeneration {
                    generation: 1,
                    full_generation: 1,
                    is_compacted: false,
                },
                &[],
                &[],
            )
            .expect("write segment");
        writer.close().expect("close archive");
        let pristine = std::fs::read(&path).expect("read pristine archive");

        let reader = TarArchiveReader::open(&path).expect("open pristine archive");
        reader
            .validate_indexed_segment_entry(identifier)
            .expect("writer produced a fully certified indexed entry");
        let position = reader
            .index_entry(identifier)
            .expect("index entry")
            .position;
        drop(reader);

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open archive for corruption");
        file.seek(SeekFrom::Start(u64::from(position)))
            .expect("seek payload");
        file.write_all(&[0x76]).expect("corrupt payload");
        file.sync_all().expect("sync corruption");
        drop(file);
        let reader = TarArchiveReader::open(&path).expect("index remains structurally valid");
        assert!(
            reader
                .validate_indexed_segment_entry(identifier)
                .expect_err("payload CRC mismatch must fail")
                .to_string()
                .contains("payload CRC")
        );
        drop(reader);

        let header_start = usize::try_from(position).expect("position fits") - 512;
        let mut wrong_name = pristine.clone();
        wrong_name[header_start] = b'f';
        std::fs::write(&path, wrong_name).expect("write wrong entry name");
        let reader = TarArchiveReader::open(&path).expect("index still parses");
        assert!(
            reader
                .validate_indexed_segment_entry(identifier)
                .expect_err("header/index identifier mismatch must fail")
                .to_string()
                .contains("index names")
        );
        drop(reader);

        std::fs::write(&path, &pristine).expect("restore archive");
        let header_size_field = header_start + 124;
        let mut wrong_size = pristine.clone();
        wrong_size[header_size_field..header_size_field + 12].copy_from_slice(b"00000000000\0");
        std::fs::write(&path, wrong_size).expect("write wrong header size");
        let reader = TarArchiveReader::open(&path).expect("index still parses");
        assert!(
            reader
                .validate_indexed_segment_entry(identifier)
                .expect_err("header/index size mismatch must fail")
                .to_string()
                .contains("index size")
        );
        drop(reader);

        let mut wrong_header_checksum = pristine;
        wrong_header_checksum[header_start + 100] ^= 0x01;
        std::fs::write(&path, wrong_header_checksum).expect("write bad header checksum");
        let reader = TarArchiveReader::open(&path).expect("index still parses");
        assert!(
            reader
                .validate_indexed_segment_entry(identifier)
                .expect_err("tar header checksum mismatch must fail")
                .to_string()
                .contains("tar-header checksum")
        );
        drop(reader);

        std::fs::remove_dir_all(&directory).expect("remove test directory");
    }
}
