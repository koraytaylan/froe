//! Finding every super-root a store still holds, by scanning its segments
//! in parallel — the search a journal recovery starts from.

use super::{
    ArchiveSet, AtomicUsize, Mutex, NodeState, Ordering, ProgressObserver, RecordIdentifier,
    SegmentIdentifier, SegmentProvider, Step, WorkUnit,
};
use crate::parallel::worker_count;

/// A candidate head discovered during recovery.
pub(crate) struct Candidate {
    pub(crate) record: RecordIdentifier,
    pub(crate) timestamp_milliseconds: i64,
}

/// Distinct data segments, not every archive occurrence. A segment's record
/// table is strictly ascending by record number, so one segment cannot yield
/// a duplicate record; only a segment served by two archives could, and the
/// location map settles that for free.
pub(crate) fn collect_super_root_candidates(
    provider: &ArchiveSet,
    observer: &mut dyn ProgressObserver,
) -> Vec<Candidate> {
    let identifiers: Vec<SegmentIdentifier> = provider.distinct_segment_identifiers().collect();
    observer.step_began(
        &Step::new("scanning segments for super-roots", WorkUnit::Segments)
            .with_total(crate::progress::count(identifiers.len())),
    );
    let workers = worker_count(identifiers.len());
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Vec<Candidate>>> =
        identifiers.iter().map(|_| Mutex::new(Vec::new())).collect();
    std::thread::scope(|scope| {
        for _ in 1..workers {
            scope.spawn(|| scan_super_root_worker(provider, &identifiers, &next, &slots));
        }
        while scan_next_super_root_segment(provider, &identifiers, &next, &slots) {
            let completed = next.load(Ordering::Relaxed).min(identifiers.len());
            observer.step_advanced(crate::progress::count(completed));
        }
    });
    observer.step_advanced(crate::progress::count(identifiers.len()));
    observer.step_ended();
    slots
        .into_iter()
        .flat_map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
        .collect()
}

pub(crate) fn scan_super_root_worker(
    provider: &ArchiveSet,
    identifiers: &[SegmentIdentifier],
    next: &AtomicUsize,
    slots: &[Mutex<Vec<Candidate>>],
) {
    while scan_next_super_root_segment(provider, identifiers, next, slots) {}
}

pub(crate) fn scan_next_super_root_segment(
    provider: &ArchiveSet,
    identifiers: &[SegmentIdentifier],
    next: &AtomicUsize,
    slots: &[Mutex<Vec<Candidate>>],
) -> bool {
    let position = next.fetch_add(1, Ordering::Relaxed);
    let Some(identifier) = identifiers.get(position) else {
        return false;
    };
    let found = super_roots_in_segment(provider, *identifier);
    *slots[position]
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = found;
    true
}

pub(crate) fn super_roots_in_segment(
    provider: &ArchiveSet,
    segment_identifier: SegmentIdentifier,
) -> Vec<Candidate> {
    if segment_identifier.is_bulk_segment() {
        return Vec::new();
    }
    let Ok(view) = provider.segment(segment_identifier) else {
        return Vec::new();
    };
    // A segment without a parseable info timestamp is skipped whole,
    // as in Java ("No timestamp found in segment ..."); Java aborts
    // the entire run on malformed info JSON, which is folded into the
    // same skip here — strictly safer, recovery proceeds on the rest.
    let Some(timestamp) = read_segment_info_timestamp(provider, segment_identifier) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in view.structure.record_table() {
        if entry.record_type() != Some(crate::segment::record::RecordType::Node) {
            continue;
        }
        let record = RecordIdentifier::new(segment_identifier, entry.record_number);
        if is_super_root(provider, record) {
            candidates.push(Candidate {
                record,
                timestamp_milliseconds: timestamp,
            });
        }
    }
    candidates
}

/// Whether a node looks like a super-root: Oak's recovery keeps a
/// candidate iff it has *both* a `root` and a `checkpoints` child —
/// requiring only `root` would let ordinary content nodes (every page
/// with a child named `root`) flood the candidate list and even become
/// the recovered head.
pub(crate) fn is_super_root(provider: &dyn SegmentProvider, record: RecordIdentifier) -> bool {
    let node = NodeState::new(provider, record);
    matches!(node.child_node("root"), Ok(Some(_)))
        && matches!(node.child_node("checkpoints"), Ok(Some(_)))
}

/// The segment UUID as Java's `UUID.compareTo` orders it: most then least
/// significant half, compared as *signed* 64-bit values.
pub(crate) fn signed_uuid_key(record: RecordIdentifier) -> (i64, i64) {
    (
        record.segment.most_significant_bits as i64,
        record.segment.least_significant_bits as i64,
    )
}

/// Reads the `"t"` timestamp from a segment's info record (record 0).
pub(crate) fn read_segment_info_timestamp(
    provider: &dyn SegmentProvider,
    segment: SegmentIdentifier,
) -> Option<i64> {
    let view = provider.segment(segment).ok()?;
    let first_record = view.structure.record_table().first()?.record_number;
    let info =
        crate::content::value::read_string(provider, RecordIdentifier::new(segment, first_record))
            .ok()?;
    parse_info_timestamp(&info)
}

/// Extracts the `"t":<number>` value from a segment-info JSON string.
pub(crate) fn parse_info_timestamp(info: &str) -> Option<i64> {
    let marker = "\"t\":";
    let start = info.find(marker)? + marker.len();
    let rest = &info[start..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_info_timestamp;

    #[test]
    fn parses_segment_info_timestamps() {
        assert_eq!(
            parse_info_timestamp("{\"wid\":\"froe\",\"sno\":3,\"t\":1700000000000}"),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_info_timestamp("{\"wid\":\"x\"}"), None);
    }
}
