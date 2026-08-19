//! What the planning walk collects beside its verification.
//!
//! The head verification already visits every distinct node once and decodes
//! every property for its checks, so facts about the content — today, the
//! external binaries it references — are collected there for free rather
//! than by a second walk. The collector rides the walk through
//! [`VerifiedContentObserver`] and can never influence it.

use crate::content::node::{PropertyState, PropertyValues};
use crate::content::property::PropertyValue;
use crate::content::provider::SegmentProvider;
use crate::content::value::BinaryValue;
use crate::segment::identifier::SegmentIdentifier;
use crate::tooling::VerifiedContentObserver;
use crate::writer::maintenance::plan::ExternalBinaryFootprint;
use crate::writer::maintenance::planning::version_storage::{
    VersionStorageCensus, collect_bulk_blocks, parse_identifier,
};
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

/// Everything the planning verification walk collects about the content.
#[derive(Default)]
pub(crate) struct PlanningContentCensus {
    pub(crate) external_binaries: ExternalBinaryCensus,
    /// What the version-storage pre-scan established; the main walk
    /// resolves it by matching live identifiers.
    pub(crate) version_storage: VersionStorageCensus,
    /// Whether a `jcr:uuid` seen now marks a versionable alive. True only
    /// during the head content walk: the version-storage subtree is
    /// pre-certified so the main walk never revisits it, and the checkpoint
    /// walk that follows must not mark anything — orphan-ness is judged
    /// against the head alone, deliberately, because a checkpoint carries
    /// its own version-storage snapshot and loses nothing.
    pub(crate) match_live_identifiers: bool,
    /// Bulk segments referenced outside orphan histories — head content,
    /// checkpoint content, and (merged as they resolve) live histories. A
    /// purge can only release bulk this set does not hold.
    pub(crate) live_bulk: HashSet<SegmentIdentifier>,
}

impl VerifiedContentObserver for PlanningContentCensus {
    fn node_verified(
        &mut self,
        provider: &dyn SegmentProvider,
        _path: &str,
        _node: &crate::content::node::NodeState<'_>,
        properties: &[PropertyState],
    ) {
        for property in properties {
            for value in property_values(property) {
                self.external_binaries.observe_value(value);
                if let PropertyValue::Binary(crate::content::value::BinaryValue::Inline {
                    length,
                    record_identifier,
                }) = value
                {
                    collect_bulk_blocks(provider, *record_identifier, *length, &mut self.live_bulk);
                }
            }
            if self.match_live_identifiers
                && property.name == "jcr:uuid"
                && let PropertyValues::Single(PropertyValue::String(text)) = &property.values
                && let Some(identifier) = parse_identifier(text)
            {
                self.version_storage.mark_live(identifier);
            }
        }
    }
}

/// Every value of a property, single or multiple.
fn property_values(property: &PropertyState) -> impl Iterator<Item = &PropertyValue> {
    match &property.values {
        PropertyValues::Single(value) => std::slice::from_ref(value).iter(),
        PropertyValues::Multiple(values) => values.iter(),
    }
}

/// The external binaries the walked content references, deduplicated by
/// blob identifier.
///
/// Deduplication is what makes the figure mean anything: version storage in
/// particular references the same binary from every version of its
/// versionable, and a sum over references would multiply each blob by its
/// version count. Distinctness is tracked through a 128-bit key per
/// identifier — the identifier's own leading content hash where it carries
/// one, a fixed-key hash of the full identifier otherwise — so memory stays
/// sixteen bytes per distinct blob however long the identifiers run.
#[derive(Default)]
pub(crate) struct ExternalBinaryCensus {
    distinct: HashSet<[u8; 16]>,
    measured_bytes: u64,
    unmeasured_references: u64,
}

impl ExternalBinaryCensus {
    /// Counts one property value when it is an external binary reference.
    pub(crate) fn observe_value(&mut self, value: &PropertyValue) {
        let PropertyValue::Binary(BinaryValue::External { blob_identifier }) = value else {
            return;
        };
        if !self.distinct.insert(distinctness_key(blob_identifier)) {
            return;
        }
        match parse_length_suffix(blob_identifier) {
            Some(length) => self.measured_bytes = self.measured_bytes.saturating_add(length),
            None => self.unmeasured_references += 1,
        }
    }

    /// The census as the figures a plan carries.
    pub(crate) fn footprint(&self) -> ExternalBinaryFootprint {
        ExternalBinaryFootprint {
            distinct_references: crate::progress::count(self.distinct.len()),
            measured_bytes: self.measured_bytes,
            unmeasured_references: self.unmeasured_references,
        }
    }
}

/// A 128-bit distinctness key for one blob identifier.
///
/// A `FileDataStore` identifier opens with the binary's content hash in
/// hexadecimal, so its first thirty-two digits are a ready-made exact key.
/// Any other shape is keyed by hashing the identifier twice with the
/// standard library's fixed-key hasher — one pass seeded with the length so
/// the two halves disagree — which is distinctness by hash rather than by
/// value, and is documented as such where the figure is reported.
fn distinctness_key(blob_identifier: &str) -> [u8; 16] {
    let bytes = blob_identifier.as_bytes();
    if bytes.len() >= 32
        && let Some(hexadecimal) = blob_identifier.get(..32)
        && bytes[..32].iter().all(u8::is_ascii_hexdigit)
    {
        let mut key = [0u8; 16];
        for (position, slot) in key.iter_mut().enumerate() {
            let pair = &hexadecimal[position * 2..position * 2 + 2];
            *slot = u8::from_str_radix(pair, 16).expect("two hexadecimal digits");
        }
        return key;
    }
    let mut first_half = DefaultHasher::new();
    blob_identifier.hash(&mut first_half);
    let mut second_half = DefaultHasher::new();
    blob_identifier.len().hash(&mut second_half);
    blob_identifier.hash(&mut second_half);
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&first_half.finish().to_be_bytes());
    key[8..].copy_from_slice(&second_half.finish().to_be_bytes());
    key
}

/// The byte length an Oak blob identifier carries after its final `#`, when
/// it carries one: `FileDataStore` and its relatives append the binary's
/// length there, which is the only size information a segment store has
/// about an external binary.
fn parse_length_suffix(blob_identifier: &str) -> Option<u64> {
    let (_, suffix) = blob_identifier.rsplit_once('#')?;
    suffix.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{ExternalBinaryCensus, distinctness_key, parse_length_suffix};
    use crate::content::property::PropertyValue;
    use crate::content::value::BinaryValue;

    fn external(identifier: &str) -> PropertyValue {
        PropertyValue::Binary(BinaryValue::External {
            blob_identifier: identifier.to_owned(),
        })
    }

    /// The identifiers a real `FileDataStore` writes: hash, `#`, length.
    #[test]
    fn measured_references_deduplicate_and_sum_their_lengths() {
        let mut census = ExternalBinaryCensus::default();
        let first = "00b6d84c92565b98a45f1bb0a9fef2eff804239ba1b96e4ae4e29e0e4222829a#12345";
        let second = "ffb6d84c92565b98a45f1bb0a9fef2eff804239ba1b96e4ae4e29e0e4222829a#100";
        census.observe_value(&external(first));
        census.observe_value(&external(first));
        census.observe_value(&external(second));
        let footprint = census.footprint();
        assert_eq!(footprint.distinct_references, 2);
        assert_eq!(footprint.measured_bytes, 12_445);
        assert_eq!(footprint.unmeasured_references, 0);
    }

    /// An identifier without a parsable length still counts as a distinct
    /// reference; it just cannot contribute bytes.
    #[test]
    fn an_unmeasured_reference_is_counted_but_contributes_no_bytes() {
        let mut census = ExternalBinaryCensus::default();
        census.observe_value(&external("custom-blob-store-identifier"));
        census.observe_value(&external("custom-blob-store-identifier"));
        let footprint = census.footprint();
        assert_eq!(footprint.distinct_references, 1);
        assert_eq!(footprint.measured_bytes, 0);
        assert_eq!(footprint.unmeasured_references, 1);
    }

    /// A non-binary value is not a reference at all.
    #[test]
    fn non_binary_values_are_ignored() {
        let mut census = ExternalBinaryCensus::default();
        census.observe_value(&PropertyValue::String("not a binary".to_owned()));
        assert_eq!(census.footprint().distinct_references, 0);
    }

    /// The exact-key fast path and the hashed fallback must both be stable
    /// and must separate identifiers that differ.
    #[test]
    fn distinctness_keys_separate_differing_identifiers() {
        let hexadecimal_one = "00b6d84c92565b98a45f1bb0a9fef2ef#1";
        let hexadecimal_two = "10b6d84c92565b98a45f1bb0a9fef2ef#1";
        assert_ne!(
            distinctness_key(hexadecimal_one),
            distinctness_key(hexadecimal_two)
        );
        assert_eq!(
            distinctness_key(hexadecimal_one),
            distinctness_key(hexadecimal_one)
        );
        assert_ne!(
            distinctness_key("opaque-one"),
            distinctness_key("opaque-two")
        );
        assert_eq!(
            distinctness_key("opaque-one"),
            distinctness_key("opaque-one")
        );
    }

    /// Only the final `#` starts the length; earlier ones belong to the
    /// identifier.
    #[test]
    fn length_parses_from_the_final_hash_separator_only() {
        assert_eq!(parse_length_suffix("abc#123"), Some(123));
        assert_eq!(parse_length_suffix("a#b#42"), Some(42));
        assert_eq!(parse_length_suffix("abc"), None);
        assert_eq!(parse_length_suffix("abc#notanumber"), None);
    }
}
