//! Oak's approximate counter, as a rebuild writes it.
//!
//! `oak-core`, `plugins/index/counter/ApproximateCounter.java`, specified in
//! `docs/analysis/index-property-storage.md` §11. Every insert calls
//! `adjustCountSync(node, 1)`, and the mirror strategy calls it on exactly
//! two nodes — the `:index` node and the key node — while the unique
//! strategy calls it on the `:index` node alone.
//!
//! # Why a rebuild writes them at all
//!
//! froe's first reindex deliberately wrote none, on the reasoning that
//! their absence is "a state Oak reads without complaint". Oak does read it
//! without complaint — and then **stops choosing the index**. Its cost
//! model prices a property index through `getCountSync`, which answers
//! `-1` when no counter is present, so a rebuilt index Oak can no longer
//! price loses to a full traversal. The interop query probe caught it: over
//! Oak's own rebuild of the fixture Oak planned
//! `property uuid … estimatedCost: 3102.0`, and over froe's rebuild of the
//! same entries it planned `traverse allNodes (warning: slow)`.
//!
//! An index that is correct and no longer used is the worst outcome a
//! maintenance command can have, because nothing reports it. So froe runs
//! Oak's own algorithm with froe's own entropy: the *bytes* cannot match —
//! the name, the presence and the value are each drawn from a random
//! generator, and two Oak reindexes of one tree disagree on them — but the
//! behaviour does, which is what the cost model reads.

use crate::error::Result;

/// `ApproximateCounter.COUNT_PROPERTY_PREFIX`.
pub const COUNT_PROPERTY_PREFIX: &str = ":count_";

/// `ApproximateCounter.COUNT_RESOLUTION`.
const COUNT_RESOLUTION: i64 = 100;

/// `ApproximateCounter.COUNT_MAX`.
const COUNT_MAX: i64 = 10_000_000;

/// The positive counters accumulated on one node during a rebuild.
///
/// A rebuild only ever adds, so every value is positive and `getMaxCount`
/// reduces to the largest of them.
#[derive(Default)]
pub struct ApproximateCounter {
    values: Vec<i64>,
}

impl ApproximateCounter {
    /// `adjustCountSync(node, 1)`: one insert.
    ///
    /// Both gates are `RANDOM.nextInt(n) != 0` / `> 0`, so each passes with
    /// probability `1/n`.
    pub fn record_one_insert(&mut self) {
        if next_below(COUNT_RESOLUTION) != 0 {
            return;
        }
        let max = self.maximum();
        if max >= COUNT_MAX {
            return;
        }
        // `Math.max(COUNT_RESOLUTION, max * 2) / COUNT_RESOLUTION`, in
        // integer arithmetic as Java does it.
        let x = COUNT_RESOLUTION.max(max.saturating_mul(2)) / COUNT_RESOLUTION;
        if next_below(x) > 0 {
            return;
        }
        self.values.push(x * COUNT_RESOLUTION);
    }

    /// `getMaxCount(node, true)`: the largest positive counter, or zero.
    fn maximum(&self) -> i64 {
        self.values.iter().copied().max().unwrap_or(0)
    }

    /// The counters as properties, each under a fresh random name.
    pub fn properties<Sink: crate::writer::record_writer::SegmentSink>(
        &self,
        writer: &mut crate::writer::record_writer::RecordWriter<Sink>,
    ) -> Result<Vec<crate::writer::record_writer::PropertyToWrite>> {
        use crate::writer::record_writer::{PropertyToWrite, PropertyValuesToWrite};

        let mut properties = Vec::with_capacity(self.values.len());
        for value in &self.values {
            let written = writer.write_string(&value.to_string())?;
            properties.push(PropertyToWrite {
                name: format!("{COUNT_PROPERTY_PREFIX}{}", random_identifier()),
                property_type: crate::PropertyType::Long,
                values: PropertyValuesToWrite::Single(written),
            });
        }
        Ok(properties)
    }
}

/// `RANDOM.nextInt(bound)` for a positive bound.
///
/// Java's own `nextInt` rejects the biased tail of the range; froe draws a
/// 32-bit value and reduces it modulo the bound. The bias is at most one
/// part in 2^32 over a bound never larger than 100,000, and nothing here
/// is a security decision — the counter is an estimate whose whole point
/// is that it is approximate.
fn next_below(bound: i64) -> i64 {
    if bound <= 1 {
        return 0;
    }
    let bound = u64::try_from(bound).unwrap_or(1);
    i64::try_from(u64::from(crate::writer::identifier_generator::random_u32()) % bound).unwrap_or(0)
}

/// `UUID.randomUUID()`, rendered as Oak renders it in a property name.
///
/// The same entropy stream every other identifier draws from, with a
/// proper random variant nibble in place of the segment kind marker.
fn random_identifier() -> String {
    let identifier = crate::writer::identifier_generator::new_data_segment_identifier();
    let most = identifier.most_significant_bits;
    let least = (identifier.least_significant_bits & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        most >> 32,
        (most >> 16) & 0xFFFF,
        most & 0xFFFF,
        least >> 48,
        least & 0xFFFF_FFFF_FFFF,
    )
}
