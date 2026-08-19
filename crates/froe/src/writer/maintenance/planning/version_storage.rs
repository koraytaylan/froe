//! Orphaned version histories: `nt:versionHistory` nodes under
//! `/jcr:system/jcr:versionStorage` whose `jcr:versionableUuid` no longer
//! matches any live `jcr:uuid` outside version storage.
//!
//! Structurally these are reachable content — no generation sweep may touch
//! them — but semantically they are garbage the moment their versionable is
//! gone, and on a long-lived store they pin the version payloads and inline
//! binaries of everything ever deleted. The planner therefore detects them
//! on every run and reports what they hold; removing them is a separate,
//! explicitly confirmed decision (`--purge-orphaned-version-histories`),
//! because a versionable recreated with its old identifier — a content
//! package reinstall — re-attaches the surviving history, and purging
//! forfeits that.
//!
//! Detection is two ordered passes so the memory bound never depends on
//! tree-visit order: a pre-scan of the version-storage subtree collects
//! every history's facts, and the main verification walk then matches live
//! identifiers against that set, removing each match — so resident memory
//! is bounded by the history count, never by the store's referenceable-node
//! count.

use crate::content::node::{NodeState, PropertyState, PropertyValues};
use crate::content::property::PropertyValue;
use crate::content::provider::SegmentProvider;
use crate::content::value::BinaryValue;
use crate::segment::identifier::SegmentIdentifier;
use crate::segment::record::RecordIdentifier;
use std::collections::{HashMap, HashSet};

/// Everything the pre-scan established about one version history.
pub(crate) struct HistoryFacts {
    /// The `nt:versionHistory` node record — the subtree root a purge omits.
    pub(crate) record: RecordIdentifier,
    /// The history's path relative to the version-storage root.
    pub(crate) path: String,
    /// Nodes in the history's subtree, the history node included.
    pub(crate) nodes: u64,
    /// Inline binary bytes stored in the subtree's properties.
    pub(crate) inline_binary_bytes: u64,
    /// External binary references stored in the subtree's properties.
    pub(crate) external_references: u64,
    /// Bulk segments the subtree's long inline binaries occupy.
    pub(crate) bulk_segments: HashSet<SegmentIdentifier>,
    /// The newest `jcr:created` among the subtree's `nt:version` nodes, as
    /// seconds since the Unix epoch, when one parsed.
    pub(crate) newest_version_created: Option<i64>,
    /// Whether any frozen node froze an `nt:configuration` — such histories
    /// belong to configuration versioning and are excluded from purging.
    pub(crate) freezes_a_configuration: bool,
    /// `jcr:uuid` values inside the subtree — the history's own, its
    /// versions' — for the advisory inbound-reference pass. Collected only
    /// when a purge is selected, because only that pass reads them.
    pub(crate) internal_identifiers: Vec<u128>,
}

/// What the version-storage pre-scan collects and the main walk resolves.
#[derive(Default)]
pub(crate) struct VersionStorageCensus {
    /// `jcr:versionableUuid` to facts. The main walk removes each entry a
    /// live `jcr:uuid` matches, so what remains afterwards is the orphans.
    histories: HashMap<u128, HistoryFacts>,
    /// Bulk segments of histories whose versionables turned out alive,
    /// merged here as their facts are released.
    pub(crate) live_history_bulk: HashSet<SegmentIdentifier>,
    /// Intermediate directory nodes (`xx/yy/zz`) by relative path.
    pub(crate) intermediates: Vec<(String, RecordIdentifier)>,
    /// Histories under each intermediate path, so pruning can prove an
    /// intermediate keeps no history.
    pub(crate) histories_under_intermediate: HashMap<String, u64>,
    /// `jcr:versionableUuid` values that did not parse as identifiers.
    pub(crate) malformed_identifiers: u64,
    /// Histories in total, before any matching.
    pub(crate) total_histories: u64,
    /// Every live `jcr:uuid` the content walk reported. Kept as a set
    /// rather than resolved immediately, because the content walk runs
    /// *before* the pre-scan registers any history: a removal at report
    /// time would find nothing to remove.
    live_identifiers: HashSet<u128>,
}

impl VersionStorageCensus {
    /// Records one live identifier. The matched history — whether already
    /// registered or only seen by the later pre-scan — is released by
    /// [`Self::resolve_live_matches`].
    pub(crate) fn mark_live(&mut self, identifier: u128) {
        self.live_identifiers.insert(identifier);
    }

    /// Releases every history a recorded live identifier matches. Bulk
    /// segments the released histories hold move to the live side, so the
    /// released-bulk computation later cannot count them as freed. Called
    /// once both walks are complete; `orphans` is meaningless before it.
    pub(crate) fn resolve_live_matches(&mut self) {
        for identifier in std::mem::take(&mut self.live_identifiers) {
            if let Some(facts) = self.histories.remove(&identifier) {
                self.live_history_bulk.extend(facts.bulk_segments);
            }
        }
    }

    /// The histories no live identifier matched, keyed by versionable
    /// identifier. Meaningful only after [`Self::resolve_live_matches`].
    pub(crate) fn orphans(&self) -> &HashMap<u128, HistoryFacts> {
        &self.histories
    }
}

/// Rides the version-storage subtree walk, attributing every node to the
/// history above it. Depth-first order makes the attribution exact: a
/// history's descendants follow it contiguously, and histories never nest.
pub(crate) struct VersionStoragePreScan<'census> {
    pub(crate) census: &'census mut VersionStorageCensus,
    /// The store-wide external-binary census. Version storage is part of
    /// the head, and this walk is the only one that certifies most of its
    /// records — an external reference seen here and nowhere else would
    /// otherwise be missing from the footprint entirely.
    pub(crate) external_binaries: &'census mut super::ExternalBinaryCensus,
    /// Whether `internal_identifiers` are collected (a purge is selected).
    pub(crate) collect_internal_identifiers: bool,
    /// The history currently being attributed, as `(path, identifier)`.
    current: Option<(String, u128)>,
}

impl<'census> VersionStoragePreScan<'census> {
    pub(crate) fn new(
        census: &'census mut VersionStorageCensus,
        external_binaries: &'census mut super::ExternalBinaryCensus,
        collect_internal_identifiers: bool,
    ) -> Self {
        Self {
            census,
            external_binaries,
            collect_internal_identifiers,
            current: None,
        }
    }

    /// Opens a new history context at `path`.
    fn begin_history(&mut self, path: &str, node: &NodeState<'_>, properties: &[PropertyState]) {
        self.census.total_histories += 1;
        // Counted for every history, parseable or not: an unclassified
        // history still occupies its intermediates, and an intermediate
        // that holds one must never be pruned as empty.
        for ancestor in intermediate_prefixes(path) {
            *self
                .census
                .histories_under_intermediate
                .entry(ancestor.to_owned())
                .or_default() += 1;
        }
        let Some(identifier) =
            single_string_property(properties, "jcr:versionableUuid").and_then(parse_identifier)
        else {
            self.census.malformed_identifiers += 1;
            self.current = None;
            return;
        };
        self.census.histories.insert(
            identifier,
            HistoryFacts {
                record: node.record_identifier(),
                path: path.to_owned(),
                nodes: 0,
                inline_binary_bytes: 0,
                external_references: 0,
                bulk_segments: HashSet::new(),
                newest_version_created: None,
                freezes_a_configuration: false,
                internal_identifiers: Vec::new(),
            },
        );
        self.current = Some((path.to_owned(), identifier));
    }

    /// Attributes one node to the current history.
    fn attribute(
        &mut self,
        provider: &dyn SegmentProvider,
        identifier: u128,
        properties: &[PropertyState],
    ) {
        let collect_internal = self.collect_internal_identifiers;
        let Some(facts) = self.census.histories.get_mut(&identifier) else {
            return;
        };
        facts.nodes += 1;
        let primary_type = single_name_property(properties, "jcr:primaryType");
        for property in properties {
            for value in property_values(property) {
                match value {
                    PropertyValue::Binary(BinaryValue::Inline {
                        length,
                        record_identifier,
                    }) => {
                        facts.inline_binary_bytes =
                            facts.inline_binary_bytes.saturating_add(*length);
                        collect_bulk_blocks(
                            provider,
                            *record_identifier,
                            *length,
                            &mut facts.bulk_segments,
                        );
                    }
                    PropertyValue::Binary(BinaryValue::External { .. }) => {
                        facts.external_references += 1;
                    }
                    _ => {}
                }
            }
            if property.name == "jcr:created"
                && primary_type == Some("nt:version")
                && let Some(text) = first_text(property)
                && let Some(epoch_seconds) = parse_iso8601_epoch_seconds(&text)
            {
                facts.newest_version_created = Some(
                    facts
                        .newest_version_created
                        .map_or(epoch_seconds, |newest| newest.max(epoch_seconds)),
                );
            }
            if property.name == "jcr:frozenPrimaryType"
                && first_text(property).as_deref() == Some("nt:configuration")
            {
                facts.freezes_a_configuration = true;
            }
            if collect_internal
                && property.name == "jcr:uuid"
                && let Some(parsed) = first_text(property).as_deref().and_then(parse_identifier)
            {
                facts.internal_identifiers.push(parsed);
            }
        }
    }
}

impl crate::tooling::VerifiedContentObserver for VersionStoragePreScan<'_> {
    /// One node of the version-storage subtree, in walk order.
    fn node_verified(
        &mut self,
        provider: &dyn SegmentProvider,
        path: &str,
        node: &NodeState<'_>,
        properties: &[PropertyState],
    ) {
        // Every certified record's values reach the store-wide external
        // census — intermediates and unattributed nodes included, because
        // the footprint describes the head, not the orphan set.
        for property in properties {
            for value in property_values(property) {
                self.external_binaries.observe_value(value);
            }
        }
        if let Some((current_path, identifier)) = &self.current {
            if within(path, current_path) {
                let identifier = *identifier;
                self.attribute(provider, identifier, properties);
                return;
            }
            self.current = None;
        }
        if single_name_property(properties, "jcr:primaryType") == Some("nt:versionHistory") {
            let begun = path.to_owned();
            self.begin_history(&begun, node, properties);
            if let Some((_, identifier)) = &self.current {
                let identifier = *identifier;
                self.attribute(provider, identifier, properties);
            }
            return;
        }
        // Not a history and not inside one: an intermediate directory node.
        // The version-storage root itself (the empty path) is never pruned,
        // so it is not recorded as one.
        if !path.is_empty() {
            self.census
                .intermediates
                .push((path.to_owned(), node.record_identifier()));
        }
    }
}

/// Whether `path` lies within the subtree rooted at `root` (or is it).
fn within(path: &str, root: &str) -> bool {
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The proper ancestors of a history path, excluding the root itself:
/// `/xx/yy/zz/uuid` yields `/xx`, `/xx/yy`, `/xx/yy/zz`.
fn intermediate_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .skip(1)
        .map(|(position, _)| &path[..position])
}

/// The single textual value of a named string-like property.
fn single_string_property<'properties>(
    properties: &'properties [PropertyState],
    name: &str,
) -> Option<&'properties str> {
    properties
        .iter()
        .find(|property| property.name == name)
        .and_then(|property| match &property.values {
            PropertyValues::Single(PropertyValue::String(text) | PropertyValue::Name(text)) => {
                Some(text.as_str())
            }
            _ => None,
        })
}

/// The single name value of a named property.
fn single_name_property<'properties>(
    properties: &'properties [PropertyState],
    name: &str,
) -> Option<&'properties str> {
    single_string_property(properties, name)
}

/// Every value of a property, single or multiple.
fn property_values(property: &PropertyState) -> impl Iterator<Item = &PropertyValue> {
    match &property.values {
        PropertyValues::Single(value) => std::slice::from_ref(value).iter(),
        PropertyValues::Multiple(values) => values.iter(),
    }
}

/// The first value's text, whatever the property's type renders as.
fn first_text(property: &PropertyState) -> Option<String> {
    property_values(property)
        .next()
        .and_then(PropertyValue::as_text)
}

/// The bulk segments holding one long inline binary's blocks. Shorter
/// binaries live in data segments the copy rewrites anyway, exactly the
/// rule `RecordWriter::copy_binary_value` applies.
pub(crate) fn collect_bulk_blocks(
    provider: &dyn SegmentProvider,
    value: RecordIdentifier,
    length: u64,
    bulk_segments: &mut HashSet<SegmentIdentifier>,
) {
    if length < crate::writer::record_writer::MEDIUM_VALUE_LIMIT as u64 {
        return;
    }
    let block_count = length.div_ceil(crate::content::value::BLOCK_SIZE);
    let Ok(view) = provider.segment(value.segment) else {
        return;
    };
    let Ok(list_identifier) = view.read_record_identifier(value.record_number, 8, 0) else {
        return;
    };
    let Ok(blocks) =
        crate::content::list::uncounted_list_entries(provider, list_identifier, block_count)
    else {
        return;
    };
    for block in blocks {
        if block.segment.is_bulk_segment() {
            bulk_segments.insert(block.segment);
        }
    }
}

/// Parses a JCR identifier — `8-4-4-4-12` hexadecimal — to its 128 bits,
/// case-insensitively, so the set and any external oracle comparing string
/// equality can never diverge on case.
pub(crate) fn parse_identifier(text: &str) -> Option<u128> {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    let mut value: u128 = 0;
    for (position, byte) in bytes.iter().enumerate() {
        match position {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return None;
                }
            }
            _ => {
                let digit = (*byte as char).to_digit(16)?;
                value = (value << 4) | u128::from(digit);
            }
        }
    }
    Some(value)
}

/// Seconds since the Unix epoch of an ISO-8601 timestamp in the form Oak
/// serializes dates: `2012-03-01T12:30:45.678+01:00`, with `Z` accepted
/// for a zero offset and the fraction optional. Integer arithmetic
/// throughout; `None` for anything that does not parse, because a date the
/// store renders strangely must never fail a plan.
pub(crate) fn parse_iso8601_epoch_seconds(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = bytes.get(range)?;
        let mut value = 0i64;
        for byte in slice {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        Some(value)
    };
    let separator = |position: usize, expected: u8| bytes.get(position) == Some(&expected);
    if !(separator(4, b'-')
        && separator(7, b'-')
        && separator(10, b'T')
        && separator(13, b':')
        && separator(16, b':'))
    {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut cursor = 19;
    if separator(cursor, b'.') {
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
    }
    let offset_seconds = match bytes.get(cursor) {
        Some(b'Z') if cursor + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            if cursor + 6 != bytes.len() || !separator(cursor + 3, b':') {
                return None;
            }
            let hours = digits(cursor + 1..cursor + 3)?;
            let minutes = digits(cursor + 4..cursor + 6)?;
            let magnitude = (hours * 60 + minutes) * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };
    // Days since the epoch by Howard Hinnant's civil-days algorithm.
    let year_adjusted = year - i64::from(month <= 2);
    let era = year_adjusted.div_euclid(400);
    let year_of_era = year_adjusted - era * 400;
    let month_shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds)
}

#[cfg(test)]
mod tests {
    use super::{intermediate_prefixes, parse_identifier, parse_iso8601_epoch_seconds, within};

    /// Hand-computed epochs, including the offsets Oak actually writes.
    #[test]
    fn epochs_match_a_hand_computed_table() {
        let table = [
            ("1970-01-01T00:00:00.000Z", 0),
            ("1970-01-01T01:00:00.000+01:00", 0),
            ("1969-12-31T23:00:00.000-01:00", 0),
            ("2012-03-01T12:30:45.678+01:00", 1_330_601_445),
            ("2026-08-19T07:02:27Z", 1_787_122_947),
        ];
        for (text, expected) in table {
            assert_eq!(
                parse_iso8601_epoch_seconds(text),
                Some(expected),
                "for {text}"
            );
        }
    }

    #[test]
    fn malformed_dates_parse_to_nothing() {
        for text in [
            "",
            "2012-03-01",
            "2012-03-01T12:30:45",
            "2012-13-01T12:30:45Z",
            "2012-03-01T12:30:45+0100",
            "not a date at all",
        ] {
            assert_eq!(parse_iso8601_epoch_seconds(text), None, "for {text}");
        }
    }

    #[test]
    fn identifiers_parse_case_insensitively_and_reject_malformation() {
        let lower = parse_identifier("00b6d84c-9256-5b98-a45f-1bb0a9fef2ef");
        let upper = parse_identifier("00B6D84C-9256-5B98-A45F-1BB0A9FEF2EF");
        assert!(lower.is_some());
        assert_eq!(lower, upper);
        for text in [
            "",
            "00b6d84c92565b98a45f1bb0a9fef2ef",
            "00b6d84c-9256-5b98-a45f-1bb0a9fef2eg",
            "00b6d84c-9256-5b98-a45f-1bb0a9fef2ef0",
        ] {
            assert_eq!(parse_identifier(text), None, "for {text}");
        }
    }

    #[test]
    fn subtree_membership_respects_name_boundaries() {
        assert!(within("/00/1f/2e/history", "/00/1f/2e/history"));
        assert!(within("/00/1f/2e/history/1.0", "/00/1f/2e/history"));
        assert!(!within("/00/1f/2e/history-two", "/00/1f/2e/history"));
        assert!(!within("/00/1f/2e", "/00/1f/2e/history"));
    }

    #[test]
    fn intermediate_prefixes_name_every_proper_ancestor() {
        let prefixes: Vec<&str> = intermediate_prefixes("/00/1f/2e/history").collect();
        assert_eq!(prefixes, ["/00", "/00/1f", "/00/1f/2e"]);
    }
}

/// The purge a plan carries: which subtree roots the copy omits, and the
/// counts the actions and the summary state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PurgeSelection {
    /// The records the copy declines to enter, outside checkpoint
    /// snapshots: every selected history, plus every intermediate directory
    /// node all of whose histories are selected.
    pub(crate) omitted_records: Vec<RecordIdentifier>,
    /// Selected histories.
    pub(crate) histories: u64,
    /// Nodes those histories hold, the history nodes included.
    pub(crate) nodes: u64,
    /// The selected histories' versionable identifiers, for the report's
    /// released-bulk computation.
    pub(crate) selected_identifiers: HashSet<u128>,
    /// Orphans kept because their newest version is younger than the age
    /// bound, or carries no parsable creation date at all.
    pub(crate) kept_by_age: u64,
    /// Orphans kept because they freeze an `nt:configuration`.
    pub(crate) kept_configurations: u64,
    /// Orphans kept because a reference outside version storage names a
    /// record inside them.
    pub(crate) kept_by_references: u64,
    /// The intermediate ancestors of omitted records that are not
    /// themselves omitted — half of the context-dependent set; the caller
    /// completes it with the storage chain above version storage.
    pub(crate) context_dependent_intermediates: Vec<RecordIdentifier>,
}

/// Selects which orphans a purge removes, applying the exclusions in the
/// order an operator reads them in the plan: configuration histories, the
/// age bound, then the advisory reference demotions.
pub(crate) fn select_purge(
    census: &VersionStorageCensus,
    minimum_age: Option<std::time::Duration>,
    now_epoch_seconds: i64,
    demoted_by_references: &HashSet<u128>,
) -> PurgeSelection {
    let mut selection = PurgeSelection {
        omitted_records: Vec::new(),
        histories: 0,
        nodes: 0,
        selected_identifiers: HashSet::new(),
        kept_by_age: 0,
        kept_configurations: 0,
        kept_by_references: 0,
        context_dependent_intermediates: Vec::new(),
    };
    let mut selected_paths: Vec<&str> = Vec::new();
    for (identifier, facts) in census.orphans() {
        if facts.freezes_a_configuration {
            selection.kept_configurations += 1;
            continue;
        }
        if let Some(minimum_age) = minimum_age {
            // No parsable creation date keeps the history: an age bound the
            // planner cannot evaluate must fail closed, not open.
            let old_enough = facts.newest_version_created.is_some_and(|created| {
                now_epoch_seconds.saturating_sub(created)
                    >= i64::try_from(minimum_age.as_secs()).unwrap_or(i64::MAX)
            });
            if !old_enough {
                selection.kept_by_age += 1;
                continue;
            }
        }
        if demoted_by_references.contains(identifier) {
            selection.kept_by_references += 1;
            continue;
        }
        selection.histories += 1;
        selection.nodes += facts.nodes;
        selection.selected_identifiers.insert(*identifier);
        selection.omitted_records.push(facts.record);
        selected_paths.push(&facts.path);
    }
    // An intermediate directory node is omitted exactly when everything
    // beneath it goes: at least one history, every history beneath it
    // selected, and every *recorded* node beneath it — deeper
    // intermediates, stray leaves the pre-scan recorded as intermediates,
    // unclassified histories through the totals — itself omissible. The
    // check runs bottom-up so a single kept or unclassifiable node
    // anywhere in a chain keeps the whole chain.
    let mut selected_under: HashMap<&str, u64> = HashMap::new();
    for path in &selected_paths {
        for ancestor in intermediate_prefixes(path) {
            *selected_under.entry(ancestor).or_default() += 1;
        }
    }
    let mut blocked_children: HashMap<&str, bool> = HashMap::new();
    let mut by_depth: Vec<&(String, RecordIdentifier)> = census.intermediates.iter().collect();
    by_depth.sort_by_key(|(path, _)| std::cmp::Reverse(path.matches('/').count()));
    for (path, record) in by_depth {
        let total = census
            .histories_under_intermediate
            .get(path.as_str())
            .copied()
            .unwrap_or(0);
        let selected = selected_under.get(path.as_str()).copied().unwrap_or(0);
        let child_blocks = blocked_children
            .get(path.as_str())
            .copied()
            .unwrap_or(false);
        let omissible = total != 0 && selected == total && !child_blocks;
        if omissible {
            selection.omitted_records.push(*record);
        } else {
            if selected != 0 {
                // An ancestor that keeps some content but loses a history:
                // its head-scope copy differs from its checkpoint-scope
                // copy.
                selection.context_dependent_intermediates.push(*record);
            }
            if let Some(separator) = path.rfind('/')
                && separator != 0
            {
                *blocked_children.entry(&path[..separator]).or_default() = true;
            }
        }
    }
    selection
}

/// Orphan candidates that something outside version storage still points
/// at: an advisory pass over REFERENCE- and WEAKREFERENCE-typed property
/// values, run only when a purge is selected. Oak does not enforce
/// referential integrity, so this is best-effort protection for custom
/// applications that store version references — it demotes, and never
/// authorizes.
///
/// The walk covers the head's content tree and skips exactly the
/// version-storage subtree: references between histories are copies frozen
/// alongside the content that held them, and a checkpoint's snapshot keeps
/// resolving its own version storage regardless of the head's purge. The
/// skip is anchored to the resolved version-storage *record*, not to node
/// names, so an application node that happens to be called `jcr:system`
/// still has its references checked.
pub(crate) fn demoted_by_inbound_references(
    repository: &crate::store::Repository,
    content_root: RecordIdentifier,
    version_storage_record: Option<RecordIdentifier>,
    candidate_identifiers: &HashMap<u128, u128>,
    observer: &mut dyn crate::progress::ProgressObserver,
) -> crate::error::Result<HashSet<u128>> {
    crate::progress::observe(
        observer,
        &crate::progress::Step::new(
            "checking references into purged histories",
            crate::progress::WorkUnit::Nodes,
        ),
        |observer| {
            let mut demoted = HashSet::new();
            // Packed, not a `HashSet<RecordIdentifier>`: this set grows with
            // the content tree, and the bounded-memory case prices verifier
            // walks at eight bytes per record, not twenty-four.
            let mut visited = crate::packed_records::PackedRecordSet::new();
            let mut pending = vec![content_root];
            let mut traced = crate::progress::StrideCounter::new(512);
            while let Some(record) = pending.pop() {
                if visited.contains(record) {
                    continue;
                }
                visited.insert(record);
                traced.advance(observer);
                let node = repository.node(record);
                for property in node.properties()? {
                    if !matches!(
                        property.property_type,
                        crate::content::property::PropertyType::Reference
                            | crate::content::property::PropertyType::WeakReference
                    ) {
                        continue;
                    }
                    for value in property_values(&property) {
                        if let Some(target) = value.as_text().as_deref().and_then(parse_identifier)
                            && let Some(history) = candidate_identifiers.get(&target)
                        {
                            demoted.insert(*history);
                        }
                    }
                }
                for (_, child) in node.child_node_entries()? {
                    if Some(child.record_identifier()) == version_storage_record {
                        continue;
                    }
                    pending.push(child.record_identifier());
                }
            }
            traced.finish(observer);
            if !demoted.is_empty() {
                observer.step_concluded(&format!(
                    "{} histories kept: something still references them",
                    crate::units::format_count(crate::progress::count(demoted.len())),
                ));
            }
            Ok(demoted)
        },
    )
}

/// The map the reference pass matches against: every internal identifier of
/// every purge candidate, to the candidate's own versionable identifier.
pub(crate) fn candidate_internal_identifiers(
    census: &VersionStorageCensus,
    candidates: &HashSet<u128>,
) -> HashMap<u128, u128> {
    let mut map = HashMap::new();
    for (identifier, facts) in census.orphans() {
        if !candidates.contains(identifier) {
            continue;
        }
        for internal in &facts.internal_identifiers {
            map.insert(*internal, *identifier);
        }
    }
    map
}
