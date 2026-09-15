//! The Lucene arm of the rebuild: documents, a segment, and `:data`.
//!
//! `docs/plans/0010-lucene-offline-reindex/ARCHITECTURE.md` is the safety
//! case. What this module owns is everything between the state root and the
//! `:data` record: the walk, the document maker, plan 0009's writer, and the
//! copy of the segment's own file set into the store.
//!
//! # Three things it does not own
//!
//! * **Publication.** The record it returns is written into fresh archives
//!   and is unreachable until `apply.rs` moves the head once, with every
//!   other rebuilt definition.
//! * **The definition rewrite.** `apply.rs` composes the edits.
//! * **The open protocol.** `prepared.rs` took the lock, replanned and
//!   certified the archive number before this module is reached.
//!
//! # What it writes outside the store
//!
//! A fresh segment directory per attempt, inside the run's subdirectory, so
//! a dead run's complete segment can never be mistaken for this attempt's.
//! Everything in it is spill and staging: no byte of it reaches the store
//! except through the copy, which takes the file set `finish` returns **by
//! name** and never the directory listing.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::content::node::NodeState;
use crate::error::Result;
use crate::external_sort::{RunLocation, SortBudget};
use crate::index::definition::IndexDefinition;
use crate::index::lucene::codec::segment_info::SegmentDirectory;
use crate::index::lucene::documents::binaries::BinaryTextPolicy;
use crate::index::lucene::documents::document_maker::DocumentMaker;
use crate::index::lucene::documents::facets::FacetDimension;
use crate::index::lucene::documents::rules::IndexingRules;
use crate::index::lucene::writer::{LuceneIndexWriter, WrittenIndex};
use crate::index::path_filter::PathVerdict;
use crate::progress::{ProgressObserver, Step, WorkUnit, count, observe};
use crate::segment::record::RecordIdentifier;
use crate::writer::index::lucene_directory::{DirectoryListing, OakDirectoryWriter};
use crate::writer::record_writer::{
    PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};

/// The step the document pass opens.
const DOCUMENTS_STEP: &str = "making index documents";

/// The step the writer's own work opens.
const SEGMENT_STEP: &str = "writing index segments";

/// The step the `:data` copy opens.
const COPY_STEP: &str = "copying index files";

/// Fired after the last document is added and before `finish`: spilled runs
/// and a partial stored-fields file in the run's subdirectory.
const AFTER_LAST_DOCUMENT: &str = "lucene-reindex.after-last-document";

/// Fired when `finish` returns and before the first `:data` record: a
/// complete segment in the run's subdirectory.
const AFTER_SEGMENT_FINISHED: &str = "lucene-reindex.after-segment-finished";

/// Fired between two files of the copy.
const MID_FILE_COPY: &str = "lucene-reindex.mid-file-copy";

/// One durability boundary of the mutation table, for the fault probes.
#[cfg(test)]
fn probe(cutpoint: &str) -> Result<()> {
    crate::writer::fault_injection::fail_if_armed(cutpoint)?;
    crate::writer::fault_injection::crash_if_armed(cutpoint);
    Ok(())
}

/// Outside a test build there are no cutpoints and this compiles to nothing.
#[cfg(not(test))]
#[inline]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the sibling this stands in for can fail, and the caller is the same either way"
)]
fn probe(_cutpoint: &str) -> Result<()> {
    Ok(())
}

/// What a counting pass found, which is what the plan reports.
///
/// The two byte totals are **proxies** for the work directory, and the plan
/// says so: nothing here measures bytes per token or bytes per posting, and
/// a stated figure would be worse than a named proxy.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LuceneEstimate {
    /// Documents the walk would make: one per included node with a rule.
    pub documents: u64,
    /// Nodes entered, whether or not they produced a document.
    pub nodes_visited: u64,
    /// The bytes of values the rules mark stored.
    pub stored_bytes: u64,
    /// The bytes of values the rules mark indexed.
    pub indexed_bytes: u64,
}

/// What a rebuild produced.
#[derive(Clone, Debug)]
pub struct LuceneRebuild {
    /// The `:data` record, ready for the definition's hidden children.
    pub data_record: RecordIdentifier,
    /// Documents written.
    pub documents: u64,
    /// Nodes entered.
    pub nodes_visited: u64,
    /// The segment's files, in the order they were copied.
    pub files: Vec<String>,
    /// Their total bytes.
    pub segment_bytes: u64,
    /// The facet dimensions the documents indexed, for the `facets` subtree
    /// the visible definition persists.
    pub facet_dimensions: Vec<FacetDimension>,
}

/// What one rebuild is over: the state, the definition and its rules, the
/// policy, and where the segment is assembled.
pub struct LuceneRebuildSubject<'subject> {
    /// The state root the walk covers.
    pub state_root: &'subject NodeState<'subject>,
    /// The definition being rebuilt.
    pub definition: &'subject IndexDefinition,
    /// Its rules, read against the state root.
    pub rules: &'subject IndexingRules,
    /// Where a binary property's text comes from.
    pub policy: &'subject BinaryTextPolicy,
    /// A fresh directory inside the run's subdirectory.
    pub segment_directory: &'subject Path,
    /// Where the writer spills.
    pub runs: &'subject RunLocation,
    /// What it may hold before it does.
    pub budget: &'subject SortBudget,
}

/// A directory of real files under the run's subdirectory.
struct WorkingDirectory {
    path: PathBuf,
}

impl SegmentDirectory for WorkingDirectory {
    fn write_file(
        &mut self,
        name: &str,
        write: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
    ) -> Result<()> {
        let mut file = std::fs::File::create(self.path.join(name))?;
        write(&mut file)?;
        file.sync_all()?;
        Ok(())
    }
}

/// Counts what a rebuild would produce, writing nothing.
///
/// # Errors
///
/// Propagates the walk, and the document-time refusals the rules raise —
/// the counting pass resolves a rule per node and reads no value it does
/// not measure.
pub fn estimate_lucene_index(
    state_root: &NodeState<'_>,
    definition: &IndexDefinition,
    rules: &IndexingRules,
    observer: &mut dyn ProgressObserver,
) -> Result<LuceneEstimate> {
    let mut estimate = LuceneEstimate::default();
    let step = Step::new(DOCUMENTS_STEP, WorkUnit::Nodes);
    observe(observer, &step, |observer| {
        super::property_collector::walk_visible(state_root, |node, path| {
            estimate.nodes_visited += 1;
            observer.step_advanced(count(estimate.nodes_visited as usize));
            if definition.path_filter.filter(path) != PathVerdict::Include {
                return Ok(());
            }
            let Some(rule) = rules
                .applicable_rule(node)
                .map_err(super::property_collector::index_error_to_store_error)?
            else {
                return Ok(());
            };
            estimate.documents += 1;
            for property in node.properties()? {
                if property.name.starts_with(':') {
                    continue;
                }
                let Some(definition) = rule.config_of(&property.name) else {
                    continue;
                };
                if !definition.index {
                    continue;
                }
                let bytes = value_bytes(&property);
                if definition.use_in_excerpt {
                    estimate.stored_bytes += bytes;
                }
                estimate.indexed_bytes += bytes;
            }
            Ok(())
        })
    })?;
    Ok(estimate)
}

/// The resident bytes of a property's values, which is what the plan's two
/// totals are built from.
fn value_bytes(property: &crate::content::node::PropertyState) -> u64 {
    let values = match &property.values {
        crate::content::node::PropertyValues::Single(value) => std::slice::from_ref(value),
        crate::content::node::PropertyValues::Multiple(values) => values,
    };
    values
        .iter()
        .map(|value| value.as_text().map_or(0, |text| text.len() as u64))
        .sum()
}

/// Rebuilds one Lucene definition: documents, a segment, and the `:data`
/// record.
///
/// # Errors
///
/// Propagates the walk and the writer, and returns the document-time
/// refusal of an unparseable `DATE` value — the one refusal this stage
/// raises, because Oak's own path fails the commit there.
pub fn rebuild_lucene_index<Sink: SegmentSink>(
    subject: &LuceneRebuildSubject<'_>,
    writer: &mut RecordWriter<Sink>,
    observer: &mut dyn ProgressObserver,
) -> Result<LuceneRebuild> {
    let &LuceneRebuildSubject {
        definition,
        segment_directory,
        ..
    } = subject;
    // A fresh directory per attempt: a dead run's complete segment is never
    // mistaken for this one's.
    if segment_directory.exists() {
        std::fs::remove_dir_all(segment_directory)?;
    }
    std::fs::create_dir_all(segment_directory)?;

    let written = make_and_write_documents(subject, observer)?;

    probe(AFTER_SEGMENT_FINISHED)?;
    plant_a_stray_file(segment_directory)?;

    let copied = copy_segment_into_store(
        definition,
        segment_directory,
        &written.index,
        writer,
        observer,
    )?;
    Ok(LuceneRebuild {
        data_record: copied.0,
        documents: u64::try_from(written.index.document_count).unwrap_or(0),
        nodes_visited: written.nodes_visited,
        files: written.index.files.clone(),
        segment_bytes: copied.1,
        facet_dimensions: written.facet_dimensions,
    })
}

// A test-only way to leave a file in the segment directory that no segment
// names. The copy takes the file set `finish` returned, so a stray can
// never reach `:data` — and a suite whose segment directories only ever
// hold the segment cannot tell that apart from a copy that walks the
// directory. This seam is what makes the difference observable.
#[cfg(test)]
std::thread_local! {
    static STRAY_FILE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Makes the next rebuild on this thread leave `name` in its segment
/// directory.
#[cfg(test)]
pub(crate) fn plant_stray_file(name: Option<String>) {
    STRAY_FILE.with(|cell| *cell.borrow_mut() = name);
}

#[cfg(test)]
fn plant_a_stray_file(segment_directory: &Path) -> Result<()> {
    let planted = STRAY_FILE.with(|cell| cell.borrow().clone());
    if let Some(name) = planted {
        std::fs::write(segment_directory.join(name), b"not a segment file\n")?;
    }
    Ok(())
}

/// Outside a test build there is no seam and this compiles to nothing.
#[cfg(not(test))]
#[inline]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the sibling this stands in for can fail, and the caller is the same either way"
)]
fn plant_a_stray_file(_segment_directory: &Path) -> Result<()> {
    Ok(())
}

/// What the document pass produced.
struct WrittenSegment {
    index: WrittenIndex,
    nodes_visited: u64,
    facet_dimensions: Vec<FacetDimension>,
}

/// The walk, the maker and plan 0009's writer.
fn make_and_write_documents(
    subject: &LuceneRebuildSubject<'_>,
    observer: &mut dyn ProgressObserver,
) -> Result<WrittenSegment> {
    let LuceneRebuildSubject {
        state_root,
        definition,
        rules,
        policy,
        segment_directory,
        runs,
        budget,
    } = subject;
    let mut index_writer = LuceneIndexWriter::new(
        WorkingDirectory {
            path: segment_directory.to_path_buf(),
        },
        (*runs).clone(),
        (*budget).clone(),
    );
    let maker = DocumentMaker::new(&definition.path, rules, (*policy).clone());
    let mut nodes_visited = 0u64;
    let mut documents = 0u64;
    let mut dimensions: Vec<FacetDimension> = Vec::new();

    let step = Step::new(DOCUMENTS_STEP, WorkUnit::Nodes);
    observe(observer, &step, |observer| {
        super::property_collector::walk_visible(state_root, |node, path| {
            nodes_visited += 1;
            observer.step_advanced(count(nodes_visited as usize));
            if definition.path_filter.filter(path) != PathVerdict::Include {
                return Ok(());
            }
            let Some(rule) = rules
                .applicable_rule(node)
                .map_err(super::property_collector::index_error_to_store_error)?
            else {
                return Ok(());
            };
            // The path the maker is given is the node's own, and the walk's
            // root is the state root, so a definition's documents carry the
            // paths Oak's own editor would have carried.
            let made = maker
                .make(node, absolute(path), rule)
                .map_err(super::property_collector::index_error_to_store_error)?;
            let Some(made) = made else {
                return Ok(());
            };
            documents += 1;
            for dimension in made.facet_dimensions {
                if !dimensions.iter().any(|seen| seen.name == dimension.name) {
                    dimensions.push(dimension);
                } else if dimension.multi_valued
                    && let Some(seen) = dimensions
                        .iter_mut()
                        .find(|seen| seen.name == dimension.name)
                {
                    // One multi-valued node makes the dimension
                    // multi-valued for the whole configuration.
                    seen.multi_valued = true;
                }
            }
            index_writer.add_document(&made.document)
        })
    })?;

    probe(AFTER_LAST_DOCUMENT)?;

    let step = Step::new(SEGMENT_STEP, WorkUnit::IndexDocuments).with_total(documents);
    let (_directory, index) = observe(observer, &step, |_| index_writer.finish())?;
    Ok(WrittenSegment {
        index,
        nodes_visited,
        facet_dimensions: dimensions,
    })
}

/// The walk yields a path relative to the state root, and an empty one for
/// the root itself; Oak's editor sees absolute paths.
fn absolute(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

/// Copies the segment's own file set into a `:data` node.
///
/// **By name, from the set `finish` returned** — never from the directory
/// listing, so a spill file an interrupted merge left behind can never reach
/// the store.
fn copy_segment_into_store<Sink: SegmentSink>(
    definition: &IndexDefinition,
    segment_directory: &Path,
    written: &WrittenIndex,
    writer: &mut RecordWriter<Sink>,
    observer: &mut dyn ProgressObserver,
) -> Result<(RecordIdentifier, u64)> {
    let listing = if definition.lucene.save_directory_listing {
        DirectoryListing::Saved
    } else {
        DirectoryListing::Omitted
    };
    let mut directory = OakDirectoryWriter::new(writer, definition.lucene.blob_size, listing);
    let mut bytes = 0u64;
    let step = Step::new(COPY_STEP, WorkUnit::Files).with_total(written.files.len() as u64);
    let copied: Result<()> = observe(observer, &step, |observer| {
        for (at, name) in written.files.iter().enumerate() {
            if at > 0 {
                probe(MID_FILE_COPY)?;
            }
            let path = segment_directory.join(name);
            let file = std::fs::File::open(&path)?;
            bytes += file.metadata()?.len();
            directory.add_file(name, file)?;
            observer.step_advanced(count(at + 1));
        }
        Ok(())
    });
    copied?;
    Ok((directory.finish()?, bytes))
}

/// The definition bookkeeping Oak's own cycle and its fulltext editor
/// perform around a reindex, composed into the one rewrite `apply.rs`
/// makes.
///
/// `docs/analysis/lucene-oak-documents.md` §7 is what each piece is, and
/// this plan's safety case's retention table is what survives.
///
/// # Errors
///
/// Propagates the writes and the `compatMode` refusal
/// [`fresh_index_format_version`](super::definition_update::fresh_index_format_version)
/// raises.
pub fn lucene_definition_edits<Sink: SegmentSink>(
    provider: &dyn crate::content::provider::SegmentProvider,
    writer: &mut RecordWriter<Sink>,
    definition_node: &NodeState<'_>,
    built: &LuceneRebuild,
    last_updated: Option<&str>,
    edits: &mut super::definition_update::DefinitionEdits,
) -> Result<()> {
    // `refresh` is consumed by the editor when it builds the definition, so
    // a definition that carried one no longer does. `indexImportState` goes
    // the way Oak's reindex step removes it; `corrupt` is the shared
    // rewrite's.
    edits.property_removals.push("refresh".to_owned());
    edits.property_removals.push("indexImportState".to_owned());

    let version = super::definition_update::fresh_index_format_version(definition_node)?;
    let value = writer.write_string(&version.to_string())?;
    edits.property_replacements.push(PropertyToWrite {
        name: super::definition_update::INDEX_VERSION_PROPERTY.to_owned(),
        property_type: crate::PropertyType::Long,
        values: PropertyValuesToWrite::Single(value),
    });

    // The facet configuration is node-state-backed, so it persists into the
    // **visible** definition: a `facets` node as soon as one facet property
    // is indexed, whatever its arity.
    if !built.facet_dimensions.is_empty() {
        let facets = write_facet_configuration(writer, &built.facet_dimensions)?;
        edits
            .visible_children
            .insert(FACETS_CHILD.to_owned(), Some(facets));
    }

    let status = write_status_node(writer, built, last_updated)?;
    edits
        .hidden_children
        .push((STATUS_CHILD.to_owned(), status));

    // `:index-definition` is the clone of the **pre-run** visible state,
    // which is what Oak's editor clones when it enters reindex mode: it
    // carries `reindex = true`, the old `reindexCount`, and no `seed` this
    // run created.
    let stored = super::definition_update::clone_visible_state(provider, writer, definition_node)?;
    edits.hidden_children.push((
        super::definition_update::STORED_DEFINITION_CHILD.to_owned(),
        stored,
    ));
    Ok(())
}

/// `IndexDefinition.STATUS_NODE`.
const STATUS_CHILD: &str = ":status";

/// `FulltextIndexConstants.PROP_FACETS`.
const FACETS_CHILD: &str = "facets";

/// The `:status` node Oak's fulltext editor writes when it closes the
/// writer.
fn write_status_node<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    built: &LuceneRebuild,
    last_updated: Option<&str>,
) -> Result<RecordIdentifier> {
    let now = epoch_milliseconds();
    // The `uid` is a decimal epoch-millisecond string, and it must differ
    // from the previous one: Oak's own reader treats an unchanged `uid` as
    // an unchanged index.
    let unique = writer.write_string(&now.to_string())?;
    let indexed = writer.write_string(&built.documents.to_string())?;
    let completion = writer.write_string(&iso8601_of(now))?;
    let updated = writer.write_string(last_updated.unwrap_or(&iso8601_of(now)))?;
    let properties = vec![
        PropertyToWrite {
            name: "uid".to_owned(),
            property_type: crate::PropertyType::String,
            values: PropertyValuesToWrite::Single(unique),
        },
        PropertyToWrite {
            name: "lastUpdated".to_owned(),
            property_type: crate::PropertyType::Date,
            values: PropertyValuesToWrite::Single(updated),
        },
        PropertyToWrite {
            name: "indexedNodes".to_owned(),
            property_type: crate::PropertyType::Long,
            values: PropertyValuesToWrite::Single(indexed),
        },
        PropertyToWrite {
            name: "reindexCompletionTimestamp".to_owned(),
            property_type: crate::PropertyType::Date,
            values: PropertyValuesToWrite::Single(completion),
        },
    ];
    writer.write_node(
        None,
        &[],
        &crate::writer::record_writer::ChildNodesToWrite::Zero,
        &properties,
    )
}

/// The `facets` subtree the document maker's configuration persists.
///
/// `NodeStateFacetsConfig` writes in exactly two places, and the difference
/// between them is the whole shape of this node:
///
/// * its **constructor** takes `nodeBuilder.child("facets")` and gives it
///   `jcr:primaryType = nt:unstructured` if it has none. So the node exists
///   as soon as one facet property made the maker consult the
///   configuration, whatever that property's arity — which is why the
///   caller writes it for a non-empty dimension list rather than for a
///   non-empty child list.
/// * its `setMultiValued` override writes **only when the value is true**,
///   and then walks `PathUtils.elements(dimension)` from the `facets` node
///   down, creating each element's child, giving it the same primary type
///   if it has none, and setting `multivalued = true` on **every** element
///   along the way — not on the last alone.
///
/// `setIndexFieldName` is not overridden and persists nothing, so a
/// single-valued dimension leaves no child at all. Oak's own rebuild of the
/// interop fixture's faceted definition writes an empty `facets` node, which
/// is what caught an earlier version of this function writing a child per
/// dimension.
fn write_facet_configuration<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    dimensions: &[FacetDimension],
) -> Result<RecordIdentifier> {
    let mut children = Vec::new();
    for dimension in dimensions {
        if !dimension.multi_valued {
            continue;
        }
        let elements: Vec<&str> = dimension
            .name
            .split('/')
            .filter(|element| !element.is_empty())
            .collect();
        let Some((first, below)) = elements.split_first() else {
            continue;
        };
        // Bottom-up, because a record names the children it is written
        // with: the deepest element first, then each element above it
        // carrying the one below under its own name.
        let mut record = write_multi_valued_element(writer, None)?;
        for element in below.iter().rev() {
            record = write_multi_valued_element(writer, Some(((*element).to_owned(), record)))?;
        }
        children.push(((*first).to_owned(), record));
    }
    writer.write_node(
        Some(UNSTRUCTURED_TYPE),
        &[],
        &match children.as_slice() {
            [] => crate::writer::record_writer::ChildNodesToWrite::Zero,
            [(name, node)] => crate::writer::record_writer::ChildNodesToWrite::One {
                name: name.clone(),
                node: *node,
            },
            many => crate::writer::record_writer::ChildNodesToWrite::Many(many.to_vec()),
        },
        &[],
    )
}

/// One element of a multi-valued dimension's path, with the element below
/// it when there is one.
fn write_multi_valued_element<Sink: SegmentSink>(
    writer: &mut RecordWriter<Sink>,
    below: Option<(String, RecordIdentifier)>,
) -> Result<RecordIdentifier> {
    let truth = writer.write_string("true")?;
    let properties = vec![PropertyToWrite {
        name: "multivalued".to_owned(),
        property_type: crate::PropertyType::Boolean,
        values: PropertyValuesToWrite::Single(truth),
    }];
    writer.write_node(
        Some(UNSTRUCTURED_TYPE),
        &[],
        &match below {
            None => crate::writer::record_writer::ChildNodesToWrite::Zero,
            Some((name, node)) => {
                crate::writer::record_writer::ChildNodesToWrite::One { name, node }
            }
        },
        &properties,
    )
}

/// The primary type Oak's facet configuration writes.
const UNSTRUCTURED_TYPE: &str = "nt:unstructured";

/// Now, in epoch milliseconds.
fn epoch_milliseconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(0))
}

/// An epoch millisecond in the form Jackrabbit's own `ISO8601.format`
/// writes, which is the form `java/iso8601.rs` parses.
fn iso8601_of(milliseconds: i64) -> String {
    let (days, remainder) = (
        milliseconds.div_euclid(86_400_000),
        milliseconds.rem_euclid(86_400_000),
    );
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        remainder / 3_600_000,
        remainder / 60_000 % 60,
        remainder / 1_000 % 60,
        remainder % 1_000
    )
}

/// The civil date of a day number, in the proleptic Gregorian calendar —
/// which every date this writes is in, being now.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_shifted + 2) / 5 + 1;
    let month = if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    };
    (year + i64::from(month <= 2), month, day)
}

// The fixture this module's one test builds on lives in the fault-injection
// harness, which forks a child and is therefore Unix-only. A test that
// cannot be built without it is Unix-only too, and saying so here is what
// keeps `cargo check --all-targets` honest on Windows, where `cfg(test)`
// code is still part of the compilation surface.
#[cfg(all(test, unix))]
mod tests {
    use crate::index::lucene::documents::binaries::{BinaryTextFallback, BinaryTextPolicy};
    use crate::writer::fault_injection::lucene_fixture::write_lucene_reindex_fixture;
    use crate::writer::fault_injection::test_support::{TestDirectory, reindex_work_directory};
    use crate::writer::index::{ReindexOptions, WorkDirectory, reindex};

    /// A file no segment names, left in the segment directory, never
    /// reaches `:data`: the copy takes the set `finish` returned.
    #[test]
    fn a_stray_file_in_the_segment_directory_is_not_copied() {
        let directory = TestDirectory::new("lucene-stray-file");
        let store = write_lucene_reindex_fixture(&directory.path);
        super::plant_stray_file(Some("stray-run-0.tmp".to_owned()));
        let options = ReindexOptions::new()
            .with_work_directory(WorkDirectory::OperatorNamed(reindex_work_directory(&store)))
            .with_binary_text_policy(BinaryTextPolicy::new(BinaryTextFallback::Marker));
        let outcome = reindex(&store, options);
        super::plant_stray_file(None);
        outcome.expect("the rebuild runs");

        let repository = crate::store::Repository::open(&store).expect("open the repository");
        let data = repository
            .node_at_path("/oak:index/lucene/:data")
            .expect("resolve :data")
            .expect(":data exists");
        let names: Vec<String> = data
            .child_node_entries()
            .expect("the files")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            !names.iter().any(|name| name.starts_with("stray")),
            "a file no segment names reached the store: {names:?}"
        );
        assert_eq!(names.len(), 5, "{names:?}");
    }
}
