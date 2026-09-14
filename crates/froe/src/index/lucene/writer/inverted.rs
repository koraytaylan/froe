//! Inversion, spilling, and the flush that writes the segment.
//!
//! `docs/analysis/lucene-4-7-codec.md` for the formats;
//! `index/DocInverterPerField.java` for the arithmetic that turns a
//! document's fields into positions, offsets and a norm.
//!
//! # One budget
//!
//! Postings, doc values and norms all spill through
//! [`SortedRuns`](crate::external_sort::SortedRuns) against **one shared
//! [`SortBudget`]**. The field set appears as documents arrive, so no
//! static split between them exists; a single total is the only division
//! that cannot be wrong.
//!
//! # One merge at a time, per format
//!
//! The postings merge is drained first — `.doc`, `.pos`, `.pay` and then
//! `.tim`/`.tip`, field by field — then the doc-value merge, then the
//! norms merge. The doc-value and norms consumers each own a single file
//! pair for the whole segment, so they cannot be interleaved with
//! anything, and the open-file bound is the sort's fan-in plus one for the
//! spill inputs plus the files of the one format being written.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::external_sort::{RunLocation, SortBudget, SortedRuns, SpillRecord};
use crate::index::lucene::codec::doc_values::NumericRecord;
use crate::index::lucene::codec::field_infos::DocValuesType;
use crate::index::lucene::codec::norms::{NormRecord, norm_byte};
use crate::index::lucene::codec::postings::IndexOptions;
use crate::index::lucene::codec::segment_info::SegmentDirectory;
use crate::index::lucene::codec::stored_fields::StoredFieldsWriter;
use crate::index::lucene::writer::{DocValue, Document, Field, MAXIMUM_TERM_LENGTH, borrow_stored};

/// The analyzer's default position-increment gap between two values of one
/// field: **zero**, so the next value continues where the last left off.
const POSITION_INCREMENT_GAP: i64 = 0;

/// The analyzer's default offset gap: **one**.
const OFFSET_GAP: i64 = 1;

/// The segment every index froe writes holds.
const SEGMENT: &str = "_0";

/// How many of the segment's own files are open at once while one format
/// is written: the stored-fields pair while documents arrive, and at most
/// five for the postings — `.doc`, `.pos`, `.pay`, `.tim` and `.tip`.
pub(crate) const MAXIMUM_SEGMENT_FILES_OPEN: usize = 5;

/// One document's postings for one term of one field.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) struct PostingRecord {
    pub(super) field: i32,
    pub(super) term: Vec<u8>,
    pub(super) document: i32,
    pub(super) frequency: i32,
    pub(super) positions: Vec<PostingPosition>,
}

/// One occurrence.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) struct PostingPosition {
    pub(super) position: i32,
    pub(super) start_offset: i32,
    pub(super) end_offset: i32,
}

/// One document's contribution to a `SORTED` or `SORTED_SET` field.
///
/// `None` is a document that does not carry the field, which only a
/// `SORTED` field emits — its ordinal stream is dense and takes
/// [`MISSING_ORD`] there. A sorted set's stream is sparse by design.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) struct ValueRecord {
    pub(super) value: Option<Vec<u8>>,
    pub(super) document: i32,
}

impl SpillRecord for PostingRecord {
    fn encode(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(&self.field.to_be_bytes());
        buffer.extend_from_slice(&(self.term.len() as u32).to_be_bytes());
        buffer.extend_from_slice(&self.term);
        buffer.extend_from_slice(&self.document.to_be_bytes());
        buffer.extend_from_slice(&self.frequency.to_be_bytes());
        buffer.extend_from_slice(&(self.positions.len() as u32).to_be_bytes());
        for position in &self.positions {
            buffer.extend_from_slice(&position.position.to_be_bytes());
            buffer.extend_from_slice(&position.start_offset.to_be_bytes());
            buffer.extend_from_slice(&position.end_offset.to_be_bytes());
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = ByteReader::new(bytes);
        let field = reader.int()?;
        let term_length = reader.int()? as usize;
        let term = reader.take(term_length)?.to_vec();
        let document = reader.int()?;
        let frequency = reader.int()?;
        let count = reader.int()? as usize;
        let mut positions = Vec::with_capacity(count);
        for _ in 0..count {
            positions.push(PostingPosition {
                position: reader.int()?,
                start_offset: reader.int()?,
                end_offset: reader.int()?,
            });
        }
        Ok(Self {
            field,
            term,
            document,
            frequency,
            positions,
        })
    }

    fn resident_size(&self) -> usize {
        // The record's own bytes: the fixed head, the term, and twelve per
        // position.
        20 + self.term.len() + self.positions.len() * 12
    }
}

impl SpillRecord for ValueRecord {
    fn encode(&self, buffer: &mut Vec<u8>) {
        buffer.push(u8::from(self.value.is_some()));
        let value = self.value.as_deref().unwrap_or(&[]);
        buffer.extend_from_slice(&(value.len() as u32).to_be_bytes());
        buffer.extend_from_slice(value);
        buffer.extend_from_slice(&self.document.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = ByteReader::new(bytes);
        let present = reader.take(1)?[0] == 1;
        let length = reader.int()? as usize;
        let value = reader.take(length)?.to_vec();
        Ok(Self {
            value: present.then_some(value),
            document: reader.int()?,
        })
    }

    fn resident_size(&self) -> usize {
        9 + self.value.as_ref().map_or(0, Vec::len)
    }
}

/// A cursor over one record's encoding, which carries no length of its own
/// beyond what the spill format already recorded.
struct ByteReader<'bytes> {
    bytes: &'bytes [u8],
    at: usize,
}

impl<'bytes> ByteReader<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'bytes [u8]> {
        if self.at + count > self.bytes.len() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "a spilled record is {} bytes and {count} more were asked for at {}",
                    self.bytes.len(),
                    self.at
                ),
            });
        }
        let taken = &self.bytes[self.at..self.at + count];
        self.at += count;
        Ok(taken)
    }

    fn int(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }
}

/// What the writer observed that a caller may want to know.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexWriterStatistics {
    /// Terms above [`MAXIMUM_TERM_LENGTH`] that were skipped. Their
    /// documents were kept, and the position each occupied still counted
    /// toward its field's norm length — which is what Lucene does.
    pub skipped_over_long_terms: u64,
}

/// One field of the segment, as the documents seen so far describe it.
pub(super) struct FieldState {
    pub(super) name: String,
    pub(super) number: i32,
    pub(super) indexed: bool,
    pub(super) options: IndexOptions,
    pub(super) omit_norms: bool,
    pub(super) doc_values: Option<DocValuesType>,
    /// Documents that contributed at least one term, which is
    /// `FieldStatistics.document_count` and the one value no accumulation
    /// over terms can produce.
    pub(super) visited_documents: i32,
    /// How many documents have been offered a value for the dense
    /// doc-value and norms streams, so a field first seen late can be
    /// back-filled.
    pub(super) emitted_documents: i32,
    pub(super) numeric: Option<SortedRuns<NumericRecord>>,
    pub(super) values: Option<SortedRuns<ValueRecord>>,
    pub(super) norms: Option<SortedRuns<NormRecord>>,
}

/// Writes one Lucene segment and the commit over it.
pub struct LuceneIndexWriter<Directory: SegmentDirectory> {
    pub(super) directory: Directory,
    pub(super) runs: RunLocation,
    pub(super) budget: SortBudget,
    pub(super) document_count: i32,
    pub(super) fields: Vec<FieldState>,
    by_name: BTreeMap<String, usize>,
    pub(super) postings: SortedRuns<PostingRecord>,
    pub(super) stored: Option<StoredFieldsWriter<File>>,
    pub(super) temporaries: Vec<PathBuf>,
    pub(super) statistics: IndexWriterStatistics,
}

impl<Directory: SegmentDirectory> LuceneIndexWriter<Directory> {
    /// Opens a writer over `directory`, spilling under `runs`.
    #[must_use]
    pub fn new(directory: Directory, runs: RunLocation, budget: SortBudget) -> Self {
        let postings = SortedRuns::new(
            RunLocation::new(runs.directory(), format!("{}-postings", runs.name_prefix())),
            budget.clone(),
        );
        Self {
            directory,
            runs,
            budget,
            document_count: 0,
            fields: Vec::new(),
            by_name: BTreeMap::new(),
            postings,
            stored: None,
            temporaries: Vec::new(),
            statistics: IndexWriterStatistics::default(),
        }
    }

    /// A working file under the run location, removed when the segment is
    /// assembled.
    pub(super) fn temporary(&mut self, extension: &str) -> Result<File> {
        let path = self
            .runs
            .directory()
            .join(format!("{}-{SEGMENT}.{extension}", self.runs.name_prefix()));
        let file = File::create(&path)?;
        self.temporaries.push(path);
        Ok(file)
    }

    /// A spill location for one field's own runs.
    pub(super) fn field_runs(&self, field: i32, kind: &str) -> RunLocation {
        RunLocation::new(
            self.runs.directory(),
            format!("{}-{kind}-{field}", self.runs.name_prefix()),
        )
    }

    /// The field's index, which the document-order pass already assigned.
    fn index_of(&self, name: &str) -> Result<usize> {
        self.by_name
            .get(name)
            .copied()
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("the field {name} has no number yet"),
            })
    }

    /// The field's number.
    fn number_of(&self, name: &str) -> Result<i32> {
        self.index_of(name).map(|index| self.fields[index].number)
    }

    /// Finds or creates the field, reconciling it with what earlier
    /// documents said (§4.4).
    fn field_state(&mut self, field: &Field) -> Result<usize> {
        if let Some(index) = self.by_name.get(&field.name) {
            let index = *index;
            let state = &mut self.fields[index];
            // Index options downgrade to the lesser, for the whole segment.
            state.options = state.options.min(field.options);
            state.indexed |= field.indexed;
            // `omitNorms` is sticky once true, and the norms already
            // computed are discarded.
            if field.omit_norms && !state.omit_norms {
                state.omit_norms = true;
                state.norms = None;
            }
            if let Some(value) = field.doc_value.as_ref() {
                let kind = value.kind();
                match state.doc_values {
                    Some(existing) if existing != kind => {
                        return Err(Error::InvalidFormat {
                            details: format!(
                                "the field {} is already {existing:?} doc values and this \
                                 document makes it {kind:?}; a type change is refused, not \
                                 reconciled",
                                field.name
                            ),
                        });
                    }
                    Some(_) => {}
                    None => state.doc_values = Some(kind),
                }
            }
            return Ok(index);
        }
        let number = i32::try_from(self.fields.len()).map_err(|_| Error::InvalidFormat {
            details: "more fields than a field number can hold".to_owned(),
        })?;
        self.fields.push(FieldState {
            name: field.name.clone(),
            number,
            indexed: field.indexed,
            options: field.options,
            omit_norms: field.omit_norms,
            doc_values: field.doc_value.as_ref().map(DocValue::kind),
            visited_documents: 0,
            emitted_documents: 0,
            numeric: None,
            values: None,
            norms: None,
        });
        self.by_name
            .insert(field.name.clone(), self.fields.len() - 1);
        Ok(self.fields.len() - 1)
    }
}

impl<Directory: SegmentDirectory> LuceneIndexWriter<Directory> {
    /// Adds one document.
    ///
    /// Stored fields go out immediately; everything else is inverted into
    /// the spilling runs.
    pub fn add_document(&mut self, document: &Document) -> Result<()> {
        let this_document = self.document_count;
        // Field numbers are assigned in **document order**, as Lucene's own
        // field infos assign them, and every field of a group reconciles —
        // not only the first, since it is a later value of a name that can
        // omit norms or narrow the options.
        for field in &document.fields {
            self.field_state(field)?;
        }
        self.write_stored_fields(document)?;

        // Fields of one name compose, and the groups keep the order their
        // first member appeared in.
        let mut order: Vec<String> = Vec::new();
        let mut groups: BTreeMap<&str, Vec<&Field>> = BTreeMap::new();
        for field in &document.fields {
            if !groups.contains_key(field.name.as_str()) {
                order.push(field.name.clone());
            }
            groups.entry(&field.name).or_default().push(field);
        }

        let mut norms: BTreeMap<usize, u8> = BTreeMap::new();
        let mut values: BTreeMap<usize, DocValue> = BTreeMap::new();
        for name in &order {
            let group = &groups[name.as_str()];
            let index = self.index_of(name)?;
            if let Some(norm) = self.invert_group(index, group, this_document)? {
                norms.insert(index, norm);
            }
            if let Some(value) = group.iter().find_map(|field| field.doc_value.as_ref()) {
                values.insert(index, value.clone());
            }
        }

        // Every field the segment has seen owes this document an entry in
        // its dense streams, whether or not the document carried it.
        self.flush_document_streams(this_document, &norms, &values)?;
        self.document_count = this_document
            .checked_add(1)
            .ok_or_else(|| Error::InvalidFormat {
                details: "a segment holds fewer documents than an int can count".to_owned(),
            })?;
        Ok(())
    }

    /// Opens `.fdt` and `.fdx` on the first document, so a writer that
    /// receives none leaves no segment file behind, and writes this
    /// document's stored fields.
    fn write_stored_fields(&mut self, document: &Document) -> Result<()> {
        if self.stored.is_none() {
            let data = self.temporary("fdt")?;
            let index = self.temporary("fdx")?;
            self.stored = Some(StoredFieldsWriter::new(data, index)?);
        }
        let stored: Vec<&Field> = document
            .fields
            .iter()
            .filter(|field| field.stored.is_some())
            .collect();
        // The count is written before the fields and cannot be revised, so
        // it is counted first.
        let numbers: Vec<i32> = stored
            .iter()
            .map(|field| self.number_of(&field.name))
            .collect::<Result<_>>()?;
        let writer = self.stored.as_mut().expect("opened above");
        writer.start_document(stored.len())?;
        for (field, number) in stored.iter().zip(numbers) {
            let value = field.stored.as_ref().expect("filtered above");
            writer.write_field(number, borrow_stored(value))?;
        }
        writer.finish_document()
    }

    /// `DocInverterPerField.processFields`, for one field name's values.
    fn invert_group(
        &mut self,
        index: usize,
        group: &[&Field],
        document: i32,
    ) -> Result<Option<u8>> {
        if !self.fields[index].indexed {
            return Ok(None);
        }
        let mut position: i64 = 0;
        let mut offset: i64 = 0;
        let mut length: u32 = 0;
        let mut overlaps: u32 = 0;
        let mut boost: f32 = 1.0;
        let mut terms: BTreeMap<Vec<u8>, (i32, Vec<PostingPosition>)> = BTreeMap::new();
        let field_number = self.fields[index].number;
        let options = self.fields[index].options;
        let mut skipped = 0u64;

        for (value_index, field) in group.iter().enumerate() {
            if field.omit_norms && (field.boost - 1.0).abs() > f32::EPSILON {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "the field {} omits norms, so its boost of {} would be discarded \
                         rather than applied; Lucene refuses the document instead",
                        field.name, field.boost
                    ),
                });
            }
            if value_index > 0 {
                position += POSITION_INCREMENT_GAP;
            }
            for token in &field.tokens {
                let increment = i64::from(token.position_increment);
                if position == 0 && increment == 0 {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "the first position increment of {} is zero, which would put \
                             the token before the field's first position",
                            field.name
                        ),
                    });
                }
                // The decrement mirrors the increment below, so the first
                // token of a field lands at position zero.
                let mut at = position + increment;
                if at > 0 {
                    at -= 1;
                }
                position = at;
                if increment == 0 {
                    overlaps += 1;
                }
                let start = offset + i64::from(token.start_offset);
                let end = offset + i64::from(token.end_offset);
                if start < 0 || end < start {
                    return Err(Error::InvalidFormat {
                        details: format!(
                            "offsets increase and enclose their token: {start}..{end} in {}",
                            field.name
                        ),
                    });
                }
                // The over-long term is dropped and its document kept, and
                // the position it occupied still counts toward the norm.
                if token.bytes.len() <= MAXIMUM_TERM_LENGTH {
                    let entry = terms.entry(token.bytes.clone()).or_insert((0, Vec::new()));
                    entry.0 += 1;
                    if options.has_positions() {
                        entry.1.push(PostingPosition {
                            position: at as i32,
                            start_offset: start as i32,
                            end_offset: end as i32,
                        });
                    }
                } else {
                    skipped += 1;
                }
                length += 1;
                position += 1;
            }
            position += i64::from(field.final_position_increment);
            offset += i64::from(field.final_offset);
            offset += OFFSET_GAP;
            boost *= field.boost;
        }

        self.statistics.skipped_over_long_terms += skipped;
        if !terms.is_empty() {
            self.fields[index].visited_documents += 1;
        }
        for (term, (frequency, positions)) in terms {
            self.postings.push(PostingRecord {
                field: field_number,
                term,
                document,
                frequency,
                positions,
            })?;
        }

        // A document that carries the field takes a norm even when it
        // yielded no token: `1.0 / sqrt(0)` is infinite, which quantizes to
        // `0xff`.
        Ok((!self.fields[index].omit_norms).then(|| norm_byte(boost, length - overlaps)))
    }
}

impl<Directory: SegmentDirectory> LuceneIndexWriter<Directory> {
    /// Closes one document out of every stream the segment keeps.
    ///
    /// A numeric doc value, a `SORTED` ordinal and a norm are all **dense**
    /// — one entry per document of the segment — so a field first seen at
    /// document *k* is back-filled with *k* absences before its first real
    /// value, and a field this document did not carry takes an absence
    /// here. A sorted set is the exception: its stream is sparse by
    /// design.
    fn flush_document_streams(
        &mut self,
        document: i32,
        norms: &BTreeMap<usize, u8>,
        values: &BTreeMap<usize, DocValue>,
    ) -> Result<()> {
        for index in 0..self.fields.len() {
            let number = self.fields[index].number;
            let value = values.get(&index);
            if let Some(DocValue::SortedSet(entries)) = value {
                let location = self.field_runs(number, "values");
                let budget = self.budget.clone();
                let runs = self.fields[index]
                    .values
                    .get_or_insert_with(|| SortedRuns::new(location, budget));
                // A repeated value in one document is one ordinal, as
                // Lucene's own set writer makes it.
                let mut seen: Vec<&Vec<u8>> = Vec::new();
                for bytes in entries {
                    if seen.contains(&bytes) {
                        continue;
                    }
                    seen.push(bytes);
                    runs.push(ValueRecord {
                        value: Some(bytes.clone()),
                        document,
                    })?;
                }
            }

            let numeric = match value {
                Some(DocValue::Numeric(number_value)) => Some(*number_value),
                _ => None,
            };
            if numeric.is_some() || self.fields[index].numeric.is_some() {
                let location = self.field_runs(number, "numeric");
                let budget = self.budget.clone();
                let from = self.fields[index].emitted_documents;
                let runs = self.fields[index]
                    .numeric
                    .get_or_insert_with(|| SortedRuns::new(location, budget));
                for absent in from..document {
                    runs.push(NumericRecord {
                        document: absent,
                        value: None,
                    })?;
                }
                runs.push(NumericRecord {
                    document,
                    value: numeric,
                })?;
            }

            let sorted = match value {
                Some(DocValue::Sorted(bytes)) => Some(bytes.clone()),
                _ => None,
            };
            if sorted.is_some() || self.fields[index].doc_values == Some(DocValuesType::Sorted) {
                let location = self.field_runs(number, "values");
                let budget = self.budget.clone();
                let from = self.fields[index].emitted_documents;
                let runs = self.fields[index]
                    .values
                    .get_or_insert_with(|| SortedRuns::new(location, budget));
                for absent in from..document {
                    runs.push(ValueRecord {
                        value: None,
                        document: absent,
                    })?;
                }
                runs.push(ValueRecord {
                    value: sorted,
                    document,
                })?;
            }

            let norm = norms.get(&index).copied();
            if norm.is_some() || self.fields[index].norms.is_some() {
                let location = self.field_runs(number, "norms");
                let budget = self.budget.clone();
                let from = self.fields[index].emitted_documents;
                let runs = self.fields[index]
                    .norms
                    .get_or_insert_with(|| SortedRuns::new(location, budget));
                for absent in from..document {
                    runs.push(NormRecord {
                        document: absent,
                        norm: 0,
                    })?;
                }
                runs.push(NormRecord {
                    document,
                    // A document that does not carry the field takes the
                    // byte `0`, which is what Lucene's own accumulation
                    // pads a missing document with.
                    norm: norm.unwrap_or(0),
                })?;
            }

            self.fields[index].emitted_documents = document + 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;

    use super::{LuceneIndexWriter, MAXIMUM_SEGMENT_FILES_OPEN};
    use crate::error::Result;
    use crate::external_sort::{
        MAXIMUM_FAN_IN, RunLocation, SortBudget, merge_passes, peak_open_run_files,
        reset_sort_accounting,
    };
    use crate::index::lucene::codec::postings::IndexOptions;
    use crate::index::lucene::codec::segment_info::SegmentDirectory;
    use crate::index::lucene::writer::{Document, Field, Token};

    /// A directory that keeps what it is given.
    #[derive(Default)]
    struct Collected {
        files: BTreeMap<String, Vec<u8>>,
    }

    impl SegmentDirectory for Collected {
        fn write_file(
            &mut self,
            name: &str,
            write: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
        ) -> Result<()> {
            let mut bytes = Vec::new();
            write(&mut bytes)?;
            self.files.insert(name.to_owned(), bytes);
            Ok(())
        }
    }

    struct TestDirectory {
        path: std::path::PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "froe-lucene-writer-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the run directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// One document holding one term of one documents-only field, which
    /// spills as one posting record and nothing else — no positions, no
    /// doc values, no norms — so the budget's arithmetic is the record's.
    fn one_term_document(term: &str) -> Document {
        let mut field = Field::indexed(
            "body",
            IndexOptions::Documents,
            vec![Token {
                bytes: term.as_bytes().to_vec(),
                position_increment: 1,
                start_offset: 0,
                end_offset: term.len() as u32,
            }],
        );
        field.omit_norms = true;
        Document::new().with(field)
    }

    /// What one such record costs resident.
    const RECORD_BYTES: usize = 20 + 2;

    #[test]
    fn the_shared_budget_spills_one_record_past_its_limit() {
        let directory = TestDirectory::new("budget");
        let budget = SortBudget::of_bytes(RECORD_BYTES * 4);
        let mut writer = LuceneIndexWriter::new(
            Collected::default(),
            RunLocation::new(&directory.path, "writer"),
            budget.clone(),
        );
        for index in 0..4 {
            writer
                .add_document(&one_term_document(&format!("a{index}")))
                .expect("add");
        }
        assert_eq!(
            budget.charged(),
            RECORD_BYTES * 4,
            "four records exactly meet the limit"
        );
        writer
            .add_document(&one_term_document("a4"))
            .expect("add the fifth");
        assert_eq!(
            budget.charged(),
            0,
            "the fifth passes it and the run spills, releasing what it held"
        );
    }

    #[test]
    fn the_open_run_files_stay_inside_the_sort_s_fan_in() {
        let directory = TestDirectory::new("open-files");
        reset_sort_accounting();
        // A budget of one record spills on every push, so the merge has as
        // many runs as there are documents.
        let budget = SortBudget::of_bytes(1);
        let mut writer = LuceneIndexWriter::new(
            Collected::default(),
            RunLocation::new(&directory.path, "writer"),
            budget,
        );
        let document_count = MAXIMUM_FAN_IN * 2 + 5;
        for index in 0..document_count {
            writer
                .add_document(&one_term_document(&format!("t{index:04}")))
                .expect("add");
        }
        writer.finish().expect("finish");

        assert!(
            peak_open_run_files() <= MAXIMUM_FAN_IN + 1,
            "the merge opens at most the fan-in plus the one it writes, and opened {}",
            peak_open_run_files()
        );
        // The segment's own files are the other half of the bound: at most
        // five at once, which is the postings step.
        assert_eq!(MAXIMUM_SEGMENT_FILES_OPEN, 5);
    }

    #[test]
    fn the_merge_takes_one_pass_for_a_corpus_just_over_the_fan_in() {
        let directory = TestDirectory::new("merge-passes");
        reset_sort_accounting();
        let budget = SortBudget::of_bytes(1);
        let mut writer = LuceneIndexWriter::new(
            Collected::default(),
            RunLocation::new(&directory.path, "writer"),
            budget,
        );
        for index in 0..MAXIMUM_FAN_IN + 5 {
            writer
                .add_document(&one_term_document(&format!("t{index:04}")))
                .expect("add");
        }
        writer.finish().expect("finish");
        assert_eq!(
            merge_passes(),
            1,
            "one pass reduces {} runs to two",
            MAXIMUM_FAN_IN + 5
        );
    }
}
