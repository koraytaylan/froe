//! A canonical, deterministic rendering of everything a repository holds.
//!
//! The point of a digest is comparison. Maintenance is supposed to move
//! bytes without changing content, and the only way to hold it to that is
//! to render the content the same way before and after and diff the two.
//! `froe check` cannot answer this: it proves every record *parses*, which
//! a store whose properties decoded at the wrong arity also does.
//!
//! Three properties make the rendering comparable rather than merely
//! detailed:
//!
//! * **It is sorted.** Children and properties are emitted in UTF-8 byte
//!   order of their names, never in the order the map or template happens
//!   to store them. Two encodings of the same content — a map that split
//!   into a branch where the other stayed a leaf, a template whose slots
//!   hash differently — are legal alternatives, and a digest that ordered
//!   by storage would report a difference where there is none.
//! * **It excludes identity.** Record identifiers, segment identifiers and
//!   stable identifiers are all absent, because compaction legitimately
//!   changes every one of them. What survives compaction is exactly what
//!   this renders.
//! * **It includes arity and type.** `tags=String[]:a` and `tags=String:a`
//!   are different lines. Arity in particular is invisible to a check that
//!   only resolves records, and getting it wrong silently changes what an
//!   application reads back.
//!
//! Scope is the *super-root*, not the content tree: `root`'s subtree, the
//! super-root's own properties, and every checkpoint — including each
//! checkpoint node's own properties, because its expiry timestamp drives
//! froe's own retirement decisions and a corrupted one is self-fulfilling.
//!
//! # Lookup probes
//!
//! Oak reaches a child or property two ways: by enumerating them, and by
//! looking one up by name — `MapRecord.getEntry` descending on the
//! unsigned scrambled hash, `Template.getPropertyTemplate` binary-searching
//! the signed hash. Those paths read different bytes. A map leaf or
//! template slot array written in the wrong order leaves every entry
//! *present under enumeration* — so a digest, an export, and a consistency
//! check all pass — while `getChildNode("page3")` returns nothing and the
//! application 404s on a node the digest just proved exists. Sorting by
//! name, which the digest does for comparability, actively erases the
//! evidence.
//!
//! So every enumerated child and property is also looked up by name, and a
//! disagreement is reported. This is the only check here that reads the
//! bytes an ordinary traversal never touches.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

use crate::cache::{BoundedCache, CacheWeight};
use crate::checksum::Crc32;
use crate::content::node::{NodeState, PropertyValues};
use crate::content::property::PropertyValue;
use crate::content::value::{BinaryValue, read_binary_stream};
use crate::error::{Error, Result};
use crate::index::IndexError;
use crate::index::lanes::AsyncLanes;
use crate::segment::record::RecordIdentifier;
use crate::store::Repository;

/// The buffer a binary is folded through. Binaries are stored in 4 KiB
/// blocks, so a multiple of that reads whole blocks and keeps the checksum
/// on its striding path.
const BINARY_BUFFER_BYTES: usize = 64 * 1024;

/// Byte ceiling for checksums of inline binaries already folded on this
/// walk. A miss re-reads that one binary through `BINARY_BUFFER_BYTES`;
/// it never re-walks a subtree, so eviction is time rather than a different
/// digest. Insertion-order eviction matches a forward traversal: repeats
/// inside the current working set hit, a checkpoint after the head may not.
const BINARY_CHECKSUM_CACHE_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// How many lookup disagreements are retained before the digest stops
/// recording individuals. The count keeps rising; only the detail is
/// capped, so a systemically mis-ordered store reports a number rather
/// than exhausting memory naming every node in it.
const MAXIMUM_REPORTED_LOOKUP_FAILURES: usize = 64;

/// The path prefix under which a checkpoint's subtree is rendered. Not a
/// legal JCR name, so it can never collide with content.
const CHECKPOINT_PATH_PREFIX: &str = "#checkpoint";

/// The synthetic path of the super-root's own properties.
const SUPER_ROOT_PATH: &str = "#super-root";

/// What a digest run observed, beside the digest itself.
///
/// `#[non_exhaustive]`: this grew a field in 0.11 and will grow more, and a
/// downstream match or literal should not break each time. Construct one
/// with [`DigestSummary::default`] and read the fields you need.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DigestSummary {
    /// Nodes rendered, across the content tree and every checkpoint.
    pub nodes: u64,
    /// Properties rendered.
    pub properties: u64,
    /// Properties omitted because their names matched an excluded prefix.
    /// They are counted rather than silently dropped, so the summary line
    /// says how much a run excused.
    pub excluded_properties: u64,
    /// Binary values whose content was read and checksummed.
    pub binaries: u64,
    /// Bytes of binary content read.
    pub binary_bytes: u64,
    /// Checkpoints rendered.
    pub checkpoints: u64,
    /// Children or properties that enumeration found and lookup did not
    /// agree about. Capped at a fixed number of entries so a systemically
    /// mis-ordered store reports a count rather than exhausting memory
    /// naming every node in it; `lookup_failures` carries the true total.
    pub reported_lookup_failures: Vec<String>,
    /// The total number of lookup disagreements.
    pub lookup_failures: u64,
    /// Checkpoint names referenced by `/:async` that no longer exist.
    /// Oak's asynchronous index lanes resume from these; a missing one
    /// silently costs a full reindex rather than failing.
    pub dangling_async_checkpoints: Vec<String>,
}

impl DigestSummary {
    /// Whether the run found nothing wrong. The digest itself still has to
    /// be compared against a baseline — this only covers the invariants
    /// the run can judge on its own.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.lookup_failures == 0 && self.dangling_async_checkpoints.is_empty()
    }

    fn record_lookup_failure(&mut self, detail: String) {
        self.lookup_failures += 1;
        if self.reported_lookup_failures.len() < MAXIMUM_REPORTED_LOOKUP_FAILURES {
            self.reported_lookup_failures.push(detail);
        }
    }
}

/// Renders `repository` to `output` and returns what the run observed.
///
/// Output is one line per node, `\n`-terminated:
///
/// ```text
/// <path>\t<name>=<Type>[]:<value>\x1F<value>\t<name>=<Type>:<value>
/// ```
///
/// Properties are separated by tabs and multiple values by `\x1F`, with
/// those bytes — and backslash, newline and carriage return — escaped
/// inside names and values, so the grammar survives any content.
///
/// Binary values render as `<length>@<crc32>` over their content, so a
/// binary that changed is a changed line rather than an unchanged record
/// identifier. External blobs render as `external:<identifier>`, which is
/// all the store itself knows about them.
pub fn digest_repository<Output: Write + ?Sized>(
    repository: &Repository,
    output: &mut Output,
) -> Result<DigestSummary> {
    digest_repository_excluding(repository, &[], &[], output)
}

/// Digests exactly like [`digest_repository`], omitting the content
/// subtrees under the given path prefixes and the properties whose names
/// start with one of the given property prefixes.
///
/// The subtree exclusion is the mechanism that lets a digest taken before a
/// confirmed version-history purge be compared against one taken after it,
/// with the purge and nothing else excused. It applies to the head's
/// content tree only, never to checkpoint snapshots: a purge preserves
/// those, so their sections must stay byte-identical.
///
/// The property exclusion is the mechanism plan 0007's reindex oracle
/// needs. Oak's approximate counters write `:count_<random uuid>`
/// properties whose names, presence and values are all drawn from a random
/// generator, so two indexes built from identical content differ on them by
/// design. Unlike the subtree exclusion it applies to **every node in the
/// rendering** — content, super-root and every checkpoint — because a
/// counter property appears wherever the index node it annotates does.
///
/// A digest with exclusions opens by naming them: `#excluded`, a tab and
/// each subtree prefix, then `#excluded-properties`, a tab and each
/// property prefix. Two digests are therefore never compared blind — the
/// comparison helpers treat those headers as lines like any other, which
/// makes a full digest and an excluding one deliberately incomparable.
pub fn digest_repository_excluding<Output: Write + ?Sized>(
    repository: &Repository,
    excluded_content_prefixes: &[String],
    excluded_property_prefixes: &[String],
    output: &mut Output,
) -> Result<DigestSummary> {
    let mut summary = DigestSummary::default();
    let mut renderer = Renderer {
        repository,
        buffer: vec![0u8; BINARY_BUFFER_BYTES],
        binary_checksums: BoundedCache::new(BINARY_CHECKSUM_CACHE_BUDGET_BYTES),
        excluded_property_prefixes,
    };
    for (label, prefixes) in [
        ("#excluded", excluded_content_prefixes),
        ("#excluded-properties", excluded_property_prefixes),
    ] {
        if prefixes.is_empty() {
            continue;
        }
        let mut sorted: Vec<&String> = prefixes.iter().collect();
        sorted.sort();
        let mut header = String::from(label);
        for prefix in sorted {
            header.push('\t');
            header.push_str(&escape(prefix));
        }
        header.push('\n');
        output
            .write_all(header.as_bytes())
            .map_err(Error::InputOutput)?;
    }

    // The content tree first, so the commonest diff — a content change —
    // appears at the top of the output rather than after the checkpoints.
    renderer.walk_excluding(
        &repository.content_root()?,
        "",
        excluded_content_prefixes,
        output,
        &mut summary,
    )?;

    // The super-root's own properties. Its children are `root` and
    // `checkpoints`, both rendered separately, so only the line is emitted.
    renderer.emit_node(&repository.head(), SUPER_ROOT_PATH, output, &mut summary)?;

    let checkpoints = repository.checkpoints()?;
    let mut checkpoint_names = HashSet::with_capacity(checkpoints.len());
    // Sorted, because the checkpoints map is stored by hash and a digest
    // that followed it would reorder whenever a checkpoint was added.
    let mut sorted = checkpoints;
    sorted.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    for (name, node) in &sorted {
        checkpoint_names.insert(name.clone());
        summary.checkpoints += 1;
        let path = format!("{CHECKPOINT_PATH_PREFIX}/{}", escape(name));
        renderer.walk_excluding(node, &path, &[], output, &mut summary)?;
    }

    // The rule lives with the lane model it belongs to; the digest is one
    // of its two callers. It resolves `/checkpoints` itself rather than
    // taking the set this walk gathered — the same answer from the same
    // store, and one fewer thing for the two to disagree about.
    summary.dangling_async_checkpoints =
        AsyncLanes::dangling_checkpoints(&repository.content_root()?, &repository.head()).map_err(
            |error| match error {
                // Reading `/:async` and `/checkpoints` is all it does, so
                // this is the only variant it can reach. The other arm
                // carries the message rather than discarding it, because a
                // variant that becomes reachable later must not go silent.
                IndexError::Record(source) => source,
                other => Error::InvalidFormat {
                    details: other.to_string(),
                },
            },
        )?;

    output.flush().map_err(Error::InputOutput)?;
    Ok(summary)
}

struct Renderer<'repository> {
    repository: &'repository Repository,
    buffer: Vec<u8>,
    binary_checksums: BoundedCache<RecordIdentifier, CachedBinaryChecksum>,
    /// Applied at every node rather than at a subtree root, because the
    /// properties this excuses — Oak's `:count_*` approximate counters —
    /// are scattered through the index storage rather than gathered under
    /// one path.
    excluded_property_prefixes: &'repository [String],
}

/// What folding one inline binary produced, so a later property that names
/// the same record does not stream it again.
#[derive(Clone, Copy)]
struct CachedBinaryChecksum {
    read_bytes: u64,
    checksum: u32,
}

impl CacheWeight for CachedBinaryChecksum {
    fn cache_weight(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// One step of the explicit walk. `Leave` restores the ancestor set, which
/// is what makes the cycle check exact rather than a depth guess: a
/// corrupt child pointer back into an ancestor terminates instead of
/// looping forever, while a legitimately repeated subtree elsewhere in the
/// tree is still rendered.
enum Step<'provider> {
    Visit {
        node: NodeState<'provider>,
        path: String,
    },
    Leave {
        record: RecordIdentifier,
    },
}

/// Whether `path` lies under any of the excluded prefixes (or is one).
fn excluded(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        path.strip_prefix(prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

impl Renderer<'_> {
    fn walk_excluding<Output: Write + ?Sized>(
        &mut self,
        root: &NodeState<'_>,
        root_path: &str,
        excluded_prefixes: &[String],
        output: &mut Output,
        summary: &mut DigestSummary,
    ) -> Result<()> {
        let mut stack = vec![Step::Visit {
            node: *root,
            path: root_path.to_owned(),
        }];
        let mut ancestors: HashSet<RecordIdentifier> = HashSet::new();

        while let Some(step) = stack.pop() {
            match step {
                Step::Leave { record } => {
                    ancestors.remove(&record);
                }
                Step::Visit { node, path } => {
                    let record = node.record_identifier();
                    if !ancestors.insert(record) {
                        return Err(Error::InvalidFormat {
                            details: format!(
                                "the node at {} is its own ancestor, so the tree cannot be \
                                 rendered; this is corruption, not deep content",
                                display_path(&path)
                            ),
                        });
                    }
                    stack.push(Step::Leave { record });

                    self.emit_node(&node, &path, output, summary)?;

                    // Enumerated once and reused for both the lookup probe
                    // and the scheduling, because enumeration is the
                    // expensive half of a wide node and doing it twice
                    // would double the cost of the whole walk.
                    let mut entries = node.child_node_entries()?;
                    entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                    for (name, child) in &entries {
                        let found = node.child_node(name)?;
                        if found.map(|found| found.record_identifier())
                            != Some(child.record_identifier())
                        {
                            summary.record_lookup_failure(format!(
                                "{}: child {name:?} is present when enumerated but not \
                                 reachable by lookup, so an application resolving that path \
                                 finds nothing",
                                display_path(&path)
                            ));
                        }
                    }
                    for (name, child) in entries.into_iter().rev() {
                        let child_path = format!("{path}/{}", escape(&name));
                        if excluded(&child_path, excluded_prefixes) {
                            continue;
                        }
                        stack.push(Step::Visit {
                            node: child,
                            path: child_path,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn emit_node<Output: Write + ?Sized>(
        &mut self,
        node: &NodeState<'_>,
        path: &str,
        output: &mut Output,
        summary: &mut DigestSummary,
    ) -> Result<()> {
        summary.nodes += 1;

        let mut properties = node.properties()?;
        if !self.excluded_property_prefixes.is_empty() {
            // Counted rather than dropped silently: the summary line says
            // how much the run excused, so an exclusion that matched more
            // than it was meant to is visible without a second digest.
            let before = properties.len();
            properties.retain(|property| {
                !self
                    .excluded_property_prefixes
                    .iter()
                    .any(|prefix| property.name.starts_with(prefix.as_str()))
            });
            summary.excluded_properties += (before - properties.len()) as u64;
        }
        properties.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));

        let mut line = String::with_capacity(64);
        line.push_str(&display_path(path));
        for property in &properties {
            summary.properties += 1;
            line.push('\t');
            line.push_str(&escape(&property.name));
            line.push('=');
            let (values, arity) = match &property.values {
                PropertyValues::Single(value) => (std::slice::from_ref(value), ""),
                PropertyValues::Multiple(values) => (values.as_slice(), "[]"),
            };
            line.push_str(property.property_type.jcr_name());
            line.push_str(arity);
            line.push(':');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    line.push('\u{1F}');
                }
                line.push_str(&self.render_value(value, summary)?);
            }
        }
        line.push('\n');
        output
            .write_all(line.as_bytes())
            .map_err(Error::InputOutput)?;

        // Lookup probes, after the line is emitted so the digest is
        // complete even when a probe reports a disagreement.
        for property in &properties {
            let found = node.property(&property.name)?;
            let agrees = found.as_ref().is_some_and(|found| {
                found.property_type == property.property_type && found.values == property.values
            });
            if !agrees {
                summary.record_lookup_failure(format!(
                    "{}: property {:?} is present when enumerated but {} when looked up by name",
                    display_path(path),
                    property.name,
                    if found.is_some() {
                        "decodes differently"
                    } else {
                        "absent"
                    }
                ));
            }
        }

        Ok(())
    }

    fn render_value(
        &mut self,
        value: &PropertyValue,
        summary: &mut DigestSummary,
    ) -> Result<String> {
        match value {
            PropertyValue::Binary(BinaryValue::Inline {
                length,
                record_identifier,
            }) => {
                let folded = self.fold_inline_binary(*record_identifier)?;
                summary.binaries += 1;
                summary.binary_bytes += folded.read_bytes;
                // The declared length is rendered beside the content
                // checksum rather than trusted: a length that disagrees
                // with what the blocks actually yield is itself damage,
                // and it shows up here as a changed line.
                Ok(format!(
                    "{length}/{}@{:08x}",
                    folded.read_bytes, folded.checksum
                ))
            }
            PropertyValue::Binary(BinaryValue::External { blob_identifier }) => {
                Ok(format!("external:{}", escape(blob_identifier)))
            }
            other => Ok(escape(&other.as_text().unwrap_or_default())),
        }
    }

    fn fold_inline_binary(
        &mut self,
        record_identifier: RecordIdentifier,
    ) -> Result<CachedBinaryChecksum> {
        if let Some(cached) = self.binary_checksums.get(&record_identifier) {
            return Ok(cached);
        }
        let mut stream = read_binary_stream(self.repository, record_identifier)?;
        let mut running = Crc32::new();
        let mut read_bytes: u64 = 0;
        loop {
            let count = stream.read(&mut self.buffer).map_err(Error::InputOutput)?;
            if count == 0 {
                break;
            }
            running.update(&self.buffer[..count]);
            read_bytes += count as u64;
        }
        let folded = CachedBinaryChecksum {
            read_bytes,
            checksum: running.finish(),
        };
        self.binary_checksums.insert(record_identifier, folded);
        Ok(folded)
    }
}

/// The rendered form of a path. The content root is `/`; everything else
/// already carries its leading separator or its synthetic prefix.
fn display_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    }
}

/// Escapes the bytes the line grammar reserves, so any name or value can
/// appear in it. The escape character itself is escaped first, which is
/// what makes the encoding unambiguous.
fn escape(text: &str) -> String {
    if !text
        .chars()
        .any(|character| matches!(character, '\\' | '\t' | '\n' | '\r' | '\u{1F}'))
    {
        return text.to_owned();
    }
    let mut escaped = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\u{1F}' => escaped.push_str("\\u001f"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Reads a digest back into `path -> line` form, for callers that want to
/// report *where* two digests differ rather than that they do.
///
/// Comparing whole files answers "did anything change"; a repository whose
/// digest changed is only actionable once the operation that changed it
/// can be pointed at a node.
#[must_use]
pub fn parse_digest(digest: &str) -> HashMap<&str, &str> {
    digest
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once('\t') {
            Some((path, properties)) => (path, properties),
            None => (line, ""),
        })
        .collect()
}

/// How two digests differ, as paths rather than as line numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DigestDifference {
    /// Paths present in the baseline and absent afterwards.
    pub removed: Vec<String>,
    /// Paths absent from the baseline and present afterwards.
    pub added: Vec<String>,
    /// Paths present in both whose properties differ.
    pub changed: Vec<String>,
}

impl DigestDifference {
    /// Whether the two digests are identical.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty() && self.changed.is_empty()
    }
}

/// Compares two digests, reporting the paths that differ.
#[must_use]
pub fn compare_digests(baseline: &str, current: &str) -> DigestDifference {
    let baseline_nodes = parse_digest(baseline);
    let current_nodes = parse_digest(current);
    let mut difference = DigestDifference::default();
    for (path, properties) in &baseline_nodes {
        match current_nodes.get(path) {
            None => difference.removed.push((*path).to_owned()),
            Some(current_properties) if current_properties != properties => {
                difference.changed.push((*path).to_owned());
            }
            Some(_) => {}
        }
    }
    for path in current_nodes.keys() {
        if !baseline_nodes.contains_key(path) {
            difference.added.push((*path).to_owned());
        }
    }
    difference.removed.sort();
    difference.added.sort();
    difference.changed.sort();
    difference
}

#[cfg(test)]
mod tests {
    use super::{compare_digests, escape, parse_digest};

    #[test]
    fn escaping_is_unambiguous_for_every_reserved_byte() {
        // The grammar reserves tab (property separator), \x1F (value
        // separator) and newline (record separator). A name or value
        // containing one must not be able to forge a boundary, and the
        // escape character must itself be escaped or the encoding is
        // ambiguous.
        assert_eq!(escape("plain"), "plain");
        assert_eq!(escape("a\tb"), "a\\tb");
        assert_eq!(escape("a\nb"), "a\\nb");
        assert_eq!(escape("a\rb"), "a\\rb");
        assert_eq!(escape("a\u{1F}b"), "a\\u001fb");
        assert_eq!(escape("a\\b"), "a\\\\b");

        // The forging case: a literal backslash-t must not decode as a tab,
        // which is exactly what escaping the backslash first prevents.
        assert_ne!(escape("a\\tb"), escape("a\tb"));
    }

    #[test]
    fn parsing_keeps_a_node_with_no_properties() {
        // A node whose every property was dropped still has to appear, or
        // the difference would read as "unchanged" rather than "emptied".
        let parsed = parse_digest("/\n/content\tjcr:primaryType=Name:nt:folder\n");
        assert_eq!(parsed.get("/"), Some(&""));
        assert_eq!(
            parsed.get("/content"),
            Some(&"jcr:primaryType=Name:nt:folder")
        );
    }

    #[test]
    fn comparing_reports_the_paths_that_differ_not_merely_that_they_do() {
        let baseline = "/\n/a\tp=String:1\n/b\tp=String:2\n";
        let current = "/\n/a\tp=String:9\n/c\tp=String:3\n";
        let difference = compare_digests(baseline, current);
        assert_eq!(difference.changed, vec!["/a".to_owned()]);
        assert_eq!(difference.removed, vec!["/b".to_owned()]);
        assert_eq!(difference.added, vec!["/c".to_owned()]);
        assert!(!difference.is_empty());

        assert!(compare_digests(baseline, baseline).is_empty());
    }

    #[test]
    fn arity_alone_is_a_difference() {
        // The defect this tool exists to catch: a property that decodes at
        // the wrong arity is otherwise identical, so a digest that omitted
        // arity would call these two stores the same.
        let single = "/a\ttags=String:alpha\n";
        let multiple = "/a\ttags=String[]:alpha\n";
        assert_eq!(compare_digests(single, multiple).changed, vec!["/a"]);
    }
}
