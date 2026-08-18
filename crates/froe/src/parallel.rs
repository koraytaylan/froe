//! How wide froe fans a scan out.
//!
//! Two read paths walk a store's segments in parallel: the search over a
//! revision's records, and the recovery scan for super-roots. Both spawn
//! scoped workers over a shared cursor, and both need the same answer to
//! the same question — which is why it is answered once, here.

/// Workers to spawn for `work_items` units of work.
///
/// Never more than the machine offers, never more than there is work for,
/// and never zero: a store with one archive still gets one worker, and a
/// machine that will not report its parallelism gets one rather than none.
pub(crate) fn worker_count(work_items: usize) -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(work_items.max(1))
}
