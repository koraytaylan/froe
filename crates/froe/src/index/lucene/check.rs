//! The Lucene consistency check froe can run offline: Oak's **level 1**.
//!
//! `docs/analysis/index-lucene-storage.md` §5 records both of Oak's levels.
//! Level 1 (`IndexConsistencyChecker.Level.BLOBS_ONLY`) asks one question of
//! a definition — can every blob it references be read to its declared
//! length? — and nothing about the Lucene 4.7.2 bytes inside those blobs.
//! Level 2 runs Lucene's own `CheckIndex`, needs the file format, and is
//! plan 0008's; the interop suite gets that verdict from the judge in the
//! meantime.
//!
//! Two details of Oak's implementation are easy to get wrong and are what
//! the check is worth:
//!
//! * **The type is read converting**, not strictly:
//!   `TYPE_LUCENE.equals(type.getValue(Type.STRING))`. A definition whose
//!   `type` is stored as a `NAME` therefore still checks, where the model's
//!   strict read would call it typeless.
//! * **The walk covers hidden children and hidden properties.** Oak reaches
//!   the subtree through `ImmutableTree`, which overrides
//!   `isHidden` to return `false` precisely so nothing is filtered. Every
//!   byte a Lucene index holds lives under `:data` or `:suggest-data` or a
//!   mount-decorated spelling of one, so a walk that skipped hidden children
//!   would check exactly nothing and report a clean index.

use std::io::Read;

use crate::content::node::NodeState;
use crate::content::property::{PropertyType, PropertyValue};
use crate::content::value::{BinaryValue, read_binary_stream};
use crate::content::{PropertyValues, SegmentProvider};
use crate::index::{IndexResult, converting_strings};

/// How many bytes a blob is streamed through at a time. A Lucene compound
/// file runs to megabytes and is never held whole.
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

/// What the level-1 pass found.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LuceneBlobReport {
    /// Whether the definition's `type` read as `lucene` at all. Oak sets
    /// `typeMismatch` and calls the index unclean in this case rather than
    /// skipping it, so a definition pointed at the wrong checker is a
    /// finding rather than a silence.
    pub type_mismatch: bool,
    /// Blobs that could not be read at all, by the path of the property
    /// holding them.
    pub missing_blobs: Vec<BlobFault>,
    /// Blobs whose streamed length disagreed with the length they declare.
    pub invalid_blobs: Vec<BlobFault>,
    /// How many binary values were streamed to their end successfully.
    pub blobs_checked: u64,
    /// How many bytes were read doing it.
    pub bytes_read: u64,
}

impl LuceneBlobReport {
    /// Whether the index passed the level-1 check.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        !self.type_mismatch && self.missing_blobs.is_empty() && self.invalid_blobs.is_empty()
    }
}

/// One blob that could not be read, or could not be read to its length.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct BlobFault {
    /// The path of the node holding the property, relative to the
    /// definition.
    pub node_path: String,
    /// The property name.
    pub property_name: String,
    /// The index of the value inside a multi-valued property; `0` for a
    /// single one.
    pub value_index: usize,
    /// What the blob declares its length to be.
    pub declared_length: u64,
    /// What reading it actually yielded, or `None` when it could not be read
    /// at all.
    pub streamed_length: Option<u64>,
    /// The reason it could not be read, when it could not.
    pub reason: Option<String>,
}

/// Runs Oak's level-1 check over one definition subtree.
///
/// `definition_node` is the definition itself; the walk descends into every
/// child, hidden ones included, exactly as Oak's does.
pub fn check_blobs(
    provider: &dyn SegmentProvider,
    definition_node: &NodeState<'_>,
) -> IndexResult<LuceneBlobReport> {
    let mut report = LuceneBlobReport::default();

    // Oak's own guard, and its converting read: a definition whose `type` is
    // stored as a NAME still passes it.
    let declared_type = definition_node.property("type")?;
    let reads_as_lucene = converting_strings(declared_type.as_ref())
        .first()
        .is_some_and(|text| text == "lucene");
    if !reads_as_lucene {
        report.type_mismatch = true;
        return Ok(report);
    }

    let mut buffer = vec![0u8; STREAM_BUFFER_BYTES];
    let mut stack = vec![(String::new(), *definition_node)];
    while let Some((node_path, node)) = stack.pop() {
        for property in node.properties()? {
            if property.property_type != PropertyType::Binary {
                continue;
            }
            let values = match &property.values {
                PropertyValues::Single(value) => std::slice::from_ref(value),
                PropertyValues::Multiple(values) => values.as_slice(),
            };
            for (value_index, value) in values.iter().enumerate() {
                check_one_blob(
                    provider,
                    &mut buffer,
                    &node_path,
                    &property.name,
                    value_index,
                    value,
                    &mut report,
                );
            }
        }
        // Hidden children included: see the module doc. Reversed, so the
        // stack yields them in stored order and a report reads in the order
        // the store holds.
        for (name, child) in node.child_node_entries()?.into_iter().rev() {
            stack.push((format!("{node_path}/{name}"), child));
        }
    }
    Ok(report)
}

fn check_one_blob(
    provider: &dyn SegmentProvider,
    buffer: &mut [u8],
    node_path: &str,
    property_name: &str,
    value_index: usize,
    value: &PropertyValue,
    report: &mut LuceneBlobReport,
) {
    let fault = |declared_length, streamed_length, reason| BlobFault {
        node_path: node_path.to_owned(),
        property_name: property_name.to_owned(),
        value_index,
        declared_length,
        streamed_length,
        reason,
    };
    let (declared_length, record_identifier) = match value {
        PropertyValue::Binary(BinaryValue::Inline {
            length,
            record_identifier,
        }) => (*length, *record_identifier),
        PropertyValue::Binary(BinaryValue::External { blob_identifier }) => {
            // Oak asks its blob store; froe has none, so the honest answer
            // is that the blob is unreadable rather than that it is fine.
            report.missing_blobs.push(fault(
                0,
                None,
                Some(format!(
                    "the value names the external blob {blob_identifier:?}, which lives \
                     outside the segment store"
                )),
            ));
            return;
        }
        _ => return,
    };

    let mut stream = match read_binary_stream(provider, record_identifier) {
        Ok(stream) => stream,
        Err(error) => {
            report
                .missing_blobs
                .push(fault(declared_length, None, Some(error.to_string())));
            return;
        }
    };
    let mut streamed_length = 0u64;
    loop {
        match stream.read(buffer) {
            Ok(0) => break,
            Ok(count) => streamed_length += count as u64,
            Err(error) => {
                report.invalid_blobs.push(fault(
                    declared_length,
                    Some(streamed_length),
                    Some(error.to_string()),
                ));
                return;
            }
        }
    }
    report.bytes_read += streamed_length;
    if streamed_length == declared_length {
        report.blobs_checked += 1;
    } else {
        report
            .invalid_blobs
            .push(fault(declared_length, Some(streamed_length), None));
    }
}

// ---------------------------------------------------------------------------
// The structural check, between oak-run's level 1 and Lucene's own CheckIndex
// ---------------------------------------------------------------------------

/// What the structural check found.
///
/// `docs/analysis/index-lucene-storage.md` §8.7 states each claim. This sits
/// between oak-run's level 1 — the blobs resolve, which [`check_blobs`] above
/// answers — and its level 2, which is Lucene's own `CheckIndex` and needs a
/// JVM. The question here is narrower and answerable without one: *is this
/// directory a coherent set of Lucene files?*
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LuceneStructuralReport {
    /// The commit generation the directory resolves to.
    pub generation: Option<i64>,
    /// The commit file that generation names.
    pub commit_file: Option<String>,
    /// The codec name every segment carries, when they agree.
    pub codec_name: Option<String>,
    /// Codec names outside the set Oak's own registration carries.
    ///
    /// Reported rather than refused: Oak would fail on one at open, and
    /// saying so is exactly what this check is for.
    pub unregistered_codecs: Vec<String>,
    /// How many segments the commit names.
    pub segment_count: usize,
    /// Documents that are neither deleted nor absent, as Oak's own document
    /// count over a directory computes it.
    pub live_document_count: i64,
    /// Files the segments name that the listing does not hold.
    pub missing_files: Vec<String>,
    /// Files the listing holds that no segment names.
    pub unreferenced_files: Vec<String>,
    /// Files whose structure could not be read, with the reason.
    pub unreadable_files: Vec<(String, String)>,
}

impl LuceneStructuralReport {
    /// Whether the directory is structurally coherent.
    ///
    /// An unregistered codec is **not** a structural fault: the files are
    /// coherent, and it is Oak that will refuse them. It is reported so an
    /// operator learns that before Oak does.
    #[must_use]
    pub fn is_coherent(&self) -> bool {
        self.missing_files.is_empty()
            && self.unreferenced_files.is_empty()
            && self.unreadable_files.is_empty()
            && self.commit_file.is_some()
    }
}

/// The codec names Oak's own `META-INF/services` registration carries.
///
/// `oakCodec` for a fulltext-enabled definition or an explicit
/// `codec = oakCodec`, `Lucene46` for every other definition, and
/// `compressingCodec` under the `oak.lucene.compressing-codec` system
/// property.
pub const REGISTERED_CODEC_NAMES: [&str; 3] = ["oakCodec", "Lucene46", "compressingCodec"];

/// Names a commit's aggregate file set never holds, and which may legitimately
/// be present.
///
/// `segments.gen` is the generation hint: no commit names it, it is written
/// best-effort, and its absence is never a finding either.
fn is_never_referenced(name: &str) -> bool {
    name == crate::index::lucene::segments::SEGMENTS_GEN_FILE_NAME
}

/// Reads a directory's table of contents and reports its coherence.
pub fn check_structure(
    directory: &crate::index::lucene::OakDirectory<'_>,
) -> IndexResult<LuceneStructuralReport> {
    let mut report = LuceneStructuralReport::default();
    let listing: Vec<String> = directory.file_names().to_vec();

    // The hint first, because it can only raise the generation the listing
    // gives and never lowers it.
    let hint = listing
        .iter()
        .any(|name| name == crate::index::lucene::segments::SEGMENTS_GEN_FILE_NAME)
        .then(|| {
            let file = directory
                .file(crate::index::lucene::segments::SEGMENTS_GEN_FILE_NAME)
                .ok()?;
            let length = file.length();
            let mut reader = crate::index::lucene::read::Reader::new(
                file.reader(),
                crate::index::lucene::segments::SEGMENTS_GEN_FILE_NAME,
                length,
            );
            crate::index::lucene::segments::read_segments_gen(&mut reader)
        })
        .flatten();

    let Some(generation) = crate::index::lucene::segments::commit_generation(&listing, hint) else {
        // No commit file at all. Every listed file is unreferenced, which is
        // the honest report: there is no commit to refer to them.
        report.unreferenced_files = listing
            .iter()
            .filter(|name| !is_never_referenced(name))
            .cloned()
            .collect();
        report.unreferenced_files.sort();
        return Ok(report);
    };
    report.generation = Some(generation);

    let commit_name: Option<String> = listing
        .iter()
        .filter(|name| crate::index::lucene::segments::is_commit_file_name(name))
        .filter(|name| {
            crate::index::lucene::segments::generation_from_commit_file_name(name)
                == Some(generation)
        })
        .max()
        .cloned();
    let Some(commit_name) = commit_name else {
        // The hint named a generation the listing does not hold. Oak would
        // fall back to the listing; froe reports it rather than guessing.
        report.unreadable_files.push((
            format!("segments_{generation:x}"),
            "the generation hint names a commit file the directory does not hold".to_owned(),
        ));
        return Ok(report);
    };

    let commit = match read_commit(directory, &commit_name) {
        Ok(commit) => commit,
        Err(details) => {
            report.unreadable_files.push((commit_name, details));
            return Ok(report);
        }
    };
    report.commit_file = Some(commit.file_name.clone());
    report.segment_count = commit.segments.len();
    report.live_document_count = commit.live_document_count();

    let mut codec_names: Vec<String> = commit
        .segments
        .iter()
        .map(|segment| segment.codec_name.clone())
        .collect();
    codec_names.sort();
    codec_names.dedup();
    report.unregistered_codecs = codec_names
        .iter()
        .filter(|name| !REGISTERED_CODEC_NAMES.contains(&name.as_str()))
        .cloned()
        .collect();
    if codec_names.len() == 1 {
        report.codec_name = codec_names.into_iter().next();
    }

    let referenced = commit.referenced_files();
    report.missing_files = referenced
        .iter()
        .filter(|name| !listing.contains(name))
        .cloned()
        .collect();
    report.unreferenced_files = listing
        .iter()
        .filter(|name| !referenced.contains(name) && !is_never_referenced(name))
        .cloned()
        .collect();
    report.missing_files.sort();
    report.unreferenced_files.sort();

    Ok(report)
}

/// Reads one commit file and every `.si` it names, through the directory.
fn read_commit(
    directory: &crate::index::lucene::OakDirectory<'_>,
    commit_name: &str,
) -> std::result::Result<crate::index::lucene::segments::CommitFile, String> {
    let file = directory
        .file(commit_name)
        .map_err(|error| error.to_string())?;
    let length = file.length();
    let mut reader = crate::index::lucene::read::Reader::new(file.reader(), commit_name, length);
    crate::index::lucene::segments::read_commit_file(&mut reader, commit_name, |info_name| {
        let info = directory.file(info_name).map_err(|error| {
            crate::index::lucene::read::LuceneReadError::Source {
                file: info_name.to_owned(),
                details: error.to_string(),
            }
        })?;
        let info_length = info.length();
        // The file must outlive the reader, so its bytes are drained here
        // and read from memory: a `.si` is a few hundred bytes, and this
        // keeps the borrow local rather than threading a lifetime through
        // the whole commit read.
        let mut bytes = Vec::with_capacity(info_length as usize);
        let mut chunk = info.reader();
        std::io::Read::read_to_end(&mut chunk, &mut bytes).map_err(|error| {
            crate::index::lucene::read::LuceneReadError::Source {
                file: info_name.to_owned(),
                details: error.to_string(),
            }
        })?;
        Ok(crate::index::lucene::read::Reader::new(
            std::io::Cursor::new(bytes),
            info_name,
            info_length,
        ))
    })
    .map_err(|error| error.to_string())
}
