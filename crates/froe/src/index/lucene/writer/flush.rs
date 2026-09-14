//! The flush: draining the spilled runs into the segment's files.
//!
//! Split from `inverted.rs` because the two halves are of a size, not
//! because they are two ideas: inversion fills the runs, this empties
//! them.
//!
//! **One merge at a time, per format.** The postings merge is drained
//! first — `.doc`, `.pos`, `.pay` and then `.tim`/`.tip`, field by field —
//! then the doc-value merge, then the norms merge, because the doc-value
//! and norms consumers each own one file pair for the whole segment and
//! cannot be interleaved with anything.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::external_sort::{SortedPasses, SortedRuns};
use crate::index::lucene::codec::doc_values::{
    DictionaryStream, DocValuesConsumer, MISSING_ORD, OrdinalRecord, SetOrdinalRecord,
};
use crate::index::lucene::codec::field_infos::{DocValuesType, FieldInfo as CodecFieldInfo};
use crate::index::lucene::codec::norms::NormsConsumer;
use crate::index::lucene::codec::postings::{PostingsWriter, SegmentShape};
use crate::index::lucene::codec::segment_info::{
    CommittedSegment, OAK_CODEC, SegmentDirectory, SegmentOutputs, assemble_segment,
    write_commit_files,
};
use crate::index::lucene::codec::terms::{
    FieldStatistics, TermStatistics, TermsFieldInfo, TermsWriter,
};
use crate::index::lucene::writer::WrittenIndex;
use crate::index::lucene::writer::inverted::{
    LuceneIndexWriter, MAXIMUM_SEGMENT_FILES_OPEN, ValueRecord,
};

/// One file of the segment, waiting to go into the compound file.
struct Pending {
    name: String,
    path: PathBuf,
}

/// The distinct values of a `SORTED` or `SORTED_SET` field, derived from
/// its `(value, document)` run as it is walked.
///
/// This is why the dictionary is a stream: nothing is held, and the field
/// costs the two run sets its ordinals need and no third.
struct DictionaryFromValues<'source> {
    source: &'source mut SortedPasses<ValueRecord>,
}

impl DictionaryStream for DictionaryFromValues<'_> {
    fn walk(&mut self, visit: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut last: Option<Vec<u8>> = None;
        for record in self.source.pass()? {
            let record = record?;
            let Some(value) = record.value else {
                // A document that does not carry the field. Only a
                // `SORTED` field emits these, and they are not values.
                continue;
            };
            if last.as_deref() != Some(value.as_slice()) {
                visit(&value)?;
                last = Some(value);
            }
        }
        Ok(())
    }
}

impl<Directory: SegmentDirectory> LuceneIndexWriter<Directory> {
    /// Merges every run, writes the segment, commits over it, and hands
    /// back the directory it wrote into.
    pub fn finish(mut self) -> Result<(Directory, WrittenIndex)> {
        if self.document_count == 0 {
            // Closing an index writer forces a commit, and one that
            // received no document flushes no segment.
            write_commit_files(&mut self.directory, &[])?;
            let written = WrittenIndex {
                files: vec!["segments.gen".to_owned(), "segments_1".to_owned()],
                document_count: 0,
                statistics: self.statistics,
            };
            return Ok((self.directory, written));
        }

        let document_count = self.document_count;
        let mut pending: Vec<Pending> = Vec::new();
        self.finish_stored_fields(&mut pending)?;
        self.write_postings(&mut pending)?;
        self.write_doc_values(&mut pending)?;
        self.write_norms(&mut pending)?;

        let field_infos: Vec<CodecFieldInfo> = self
            .fields
            .iter()
            .map(|field| CodecFieldInfo {
                name: field.name.clone(),
                number: field.number,
                indexed: field.indexed,
                options: field.options,
                omits_norms: field.omit_norms || !field.indexed,
                doc_values: field.doc_values,
                norms: (field.indexed && !field.omit_norms).then_some(DocValuesType::Numeric),
                doc_values_generation: -1,
                attributes: Vec::new(),
            })
            .collect();

        let mut sources: Vec<(String, Box<dyn Read>)> = Vec::new();
        for file in &pending {
            let mut source = File::open(&file.path)?;
            source.seek(SeekFrom::Start(0))?;
            sources.push((file.name.clone(), Box::new(source)));
        }
        assemble_segment(
            &mut self.directory,
            "_0",
            &field_infos,
            SegmentOutputs {
                document_count,
                files: sources,
            },
        )?;
        write_commit_files(
            &mut self.directory,
            &[CommittedSegment {
                name: "_0".to_owned(),
                codec_name: OAK_CODEC.to_owned(),
            }],
        )?;

        for path in &self.temporaries {
            let _ = std::fs::remove_file(path);
        }
        let mut files: Vec<String> = vec![
            "_0.cfe".to_owned(),
            "_0.cfs".to_owned(),
            "_0.si".to_owned(),
            "segments.gen".to_owned(),
            "segments_1".to_owned(),
        ];
        files.sort();
        let written = WrittenIndex {
            files,
            document_count,
            statistics: self.statistics,
        };
        Ok((self.directory, written))
    }

    /// Closes `.fdt` and `.fdx`, which streamed out as documents arrived.
    fn finish_stored_fields(&mut self, pending: &mut Vec<Pending>) -> Result<()> {
        let writer = self.stored.take().ok_or_else(|| Error::InvalidFormat {
            details: "a segment with documents has stored-fields files open".to_owned(),
        })?;
        let count = u64::try_from(self.document_count).unwrap_or(0);
        writer.finish(count)?;
        self.take_temporary("fdt", pending);
        self.take_temporary("fdx", pending);
        Ok(())
    }

    /// Moves one written temporary onto the pending list under its segment
    /// name.
    fn take_temporary(&self, extension: &str, pending: &mut Vec<Pending>) {
        let suffix = format!(".{extension}");
        if let Some(path) = self
            .temporaries
            .iter()
            .find(|path| path.to_string_lossy().ends_with(&suffix))
        {
            pending.push(Pending {
                name: format!("_0.{extension}"),
                path: path.clone(),
            });
        }
    }
}

impl<Directory: SegmentDirectory> LuceneIndexWriter<Directory> {
    /// Drains the postings run into `.doc`, `.pos`, `.pay`, `.tim` and
    /// `.tip`, field by field in field-number order.
    ///
    /// Lucene's own writer happens to go in field-name order; the reader
    /// keys fields by number and holds them in a name-ordered map of its
    /// own, so neither order is visible to it.
    fn write_postings(&mut self, pending: &mut Vec<Pending>) -> Result<()> {
        let shape = SegmentShape {
            document_count: self.document_count,
            any_field_has_positions: self
                .fields
                .iter()
                .any(|field| field.indexed && field.options.has_positions()),
            any_field_has_offsets: self
                .fields
                .iter()
                .any(|field| field.indexed && field.options.has_offsets()),
        };
        let document = self.temporary("doc")?;
        let position = shape
            .any_field_has_positions
            .then(|| self.temporary("pos"))
            .transpose()?;
        let payload = shape
            .any_field_has_offsets
            .then(|| self.temporary("pay"))
            .transpose()?;
        // `.doc`, `.tim`, `.tip` and at most `.pos` and `.pay`: the widest
        // the segment's own files ever open, which is the constant the
        // accounting adds to the sort's fan-in.
        let opened = 3 + usize::from(position.is_some()) + usize::from(payload.is_some());
        debug_assert!(opened <= MAXIMUM_SEGMENT_FILES_OPEN);
        let mut postings = PostingsWriter::new(shape, document, position, payload)?;
        let terms_file = self.temporary("tim")?;
        let index_file = self.temporary("tip")?;
        let mut terms = TermsWriter::new(terms_file, index_file)?;

        let drained = SortedRuns::new(self.field_runs(-1, "drained"), self.budget.clone());
        let runs = std::mem::replace(&mut self.postings, drained);
        let mut passes = runs.into_sorted_passes()?;

        let mut open_field: Option<usize> = None;
        let mut open_term: Option<Vec<u8>> = None;
        let mut sums = (0i64, 0i64);
        for record in passes.pass()? {
            let record = record?;
            let index = self.field_index(record.field)?;
            if open_field != Some(index) {
                if let Some(previous) = open_field {
                    Self::close_term(&mut postings, &mut terms, &mut open_term)?;
                    self.close_field(&mut terms, previous, sums)?;
                }
                sums = (0, 0);
                let field = &self.fields[index];
                terms.start_field(
                    TermsFieldInfo {
                        number: field.number,
                        options: field.options,
                    },
                    &mut postings,
                )?;
                open_field = Some(index);
            }
            if open_term.as_deref() != Some(record.term.as_slice()) {
                Self::close_term(&mut postings, &mut terms, &mut open_term)?;
                postings.start_term();
                open_term = Some(record.term.clone());
            }
            postings.start_document(record.document, record.frequency)?;
            // A field whose options were downgraded after the positions
            // were buffered writes none of them, which is what Lucene's own
            // flush does: it reads the field's options at flush time and
            // leaves what it buffered unwritten.
            if self.fields[index].options.has_positions() {
                for position in &record.positions {
                    postings.add_position(
                        position.position,
                        position.start_offset,
                        position.end_offset,
                    )?;
                }
            }
            postings.finish_document();
            sums.0 += i64::from(record.frequency);
            sums.1 += 1;
        }
        if let Some(previous) = open_field {
            Self::close_term(&mut postings, &mut terms, &mut open_term)?;
            self.close_field(&mut terms, previous, sums)?;
        }

        terms.finish()?;
        postings.finish();
        for extension in ["doc", "pos", "pay", "tim", "tip"] {
            self.take_temporary(extension, pending);
        }
        Ok(())
    }

    fn field_index(&self, number: i32) -> Result<usize> {
        self.fields
            .iter()
            .position(|field| field.number == number)
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("a posting names the field {number}, which the segment has not"),
            })
    }

    /// Ends the open term, if any, and hands its metadata to the terms
    /// dictionary.
    fn close_term(
        postings: &mut PostingsWriter<File>,
        terms: &mut TermsWriter<File>,
        open: &mut Option<Vec<u8>>,
    ) -> Result<()> {
        let Some(term) = open.take() else {
            return Ok(());
        };
        let metadata = postings.finish_term()?;
        terms.add_term(
            &term,
            TermStatistics {
                document_frequency: metadata.document_frequency,
                total_term_frequency: metadata.total_term_frequency,
            },
            metadata,
        )
    }

    /// Ends the open field with the three statistics its directory entry
    /// carries.
    fn close_field(
        &self,
        terms: &mut TermsWriter<File>,
        index: usize,
        sums: (i64, i64),
    ) -> Result<()> {
        let field = &self.fields[index];
        terms.finish_field(FieldStatistics {
            sum_total_term_frequency: if field.options.has_frequencies() {
                sums.0
            } else {
                -1
            },
            sum_document_frequency: sums.1,
            // The one value no accumulation over terms can produce: the
            // cardinality of the documents the field appeared in, counted
            // as they arrived.
            document_count: field.visited_documents,
        })
    }
}

impl<Directory: SegmentDirectory> LuceneIndexWriter<Directory> {
    /// Drains every doc-value field into `.dvd` and `.dvm`, in field-number
    /// order, or writes neither file when the segment has none.
    fn write_doc_values(&mut self, pending: &mut Vec<Pending>) -> Result<()> {
        if self.fields.iter().all(|field| field.doc_values.is_none()) {
            return Ok(());
        }
        let data = self.temporary("dvd")?;
        let metadata = self.temporary("dvm")?;
        let mut consumer = DocValuesConsumer::new(data, metadata, i64::from(self.document_count))?;

        for index in 0..self.fields.len() {
            let Some(kind) = self.fields[index].doc_values else {
                continue;
            };
            let number = self.fields[index].number;
            match kind {
                DocValuesType::Numeric => {
                    let Some(runs) = self.fields[index].numeric.take() else {
                        continue;
                    };
                    let mut passes = runs.into_sorted_passes()?;
                    consumer.add_numeric(number, &mut passes)?;
                }
                DocValuesType::Sorted => {
                    let Some(runs) = self.fields[index].values.take() else {
                        continue;
                    };
                    let mut passes = runs.into_sorted_passes()?;
                    let location = self.field_runs(number, "ordinals");
                    let mut ordinals = SortedRuns::new(location, self.budget.clone());
                    let mut ordinal = -1i64;
                    let mut last: Option<Vec<u8>> = None;
                    for record in passes.pass()? {
                        let record = record?;
                        match record.value {
                            None => ordinals.push(OrdinalRecord {
                                document: record.document,
                                ordinal: MISSING_ORD,
                            })?,
                            Some(value) => {
                                if last.as_deref() != Some(value.as_slice()) {
                                    ordinal += 1;
                                    last = Some(value);
                                }
                                ordinals.push(OrdinalRecord {
                                    document: record.document,
                                    ordinal,
                                })?;
                            }
                        }
                    }
                    let mut ordinal_passes = ordinals.into_sorted_passes()?;
                    let mut dictionary = DictionaryFromValues {
                        source: &mut passes,
                    };
                    consumer.add_sorted(number, &mut dictionary, &mut ordinal_passes)?;
                }
                DocValuesType::SortedSet => {
                    let Some(runs) = self.fields[index].values.take() else {
                        continue;
                    };
                    let mut passes = runs.into_sorted_passes()?;
                    let location = self.field_runs(number, "ordinals");
                    let mut ordinals = SortedRuns::new(location, self.budget.clone());
                    let mut ordinal = -1i64;
                    let mut last: Option<Vec<u8>> = None;
                    for record in passes.pass()? {
                        let record = record?;
                        let Some(value) = record.value else {
                            continue;
                        };
                        if last.as_deref() != Some(value.as_slice()) {
                            ordinal += 1;
                            last = Some(value);
                        }
                        ordinals.push(SetOrdinalRecord {
                            document: record.document,
                            ordinal,
                        })?;
                    }
                    let mut ordinal_passes = ordinals.into_sorted_passes()?;
                    let mut dictionary = DictionaryFromValues {
                        source: &mut passes,
                    };
                    consumer.add_sorted_set(number, &mut dictionary, &mut ordinal_passes)?;
                }
                DocValuesType::Binary => {
                    return Err(Error::InvalidFormat {
                        details: "Oak writes no BINARY doc values, and froe has no branch \
                                  for them"
                            .to_owned(),
                    });
                }
            }
        }

        consumer.finish()?;
        self.take_temporary("dvd", pending);
        self.take_temporary("dvm", pending);
        Ok(())
    }

    /// Drains every field with norms into `.nvd` and `.nvm`, or writes
    /// neither file when no field has them.
    fn write_norms(&mut self, pending: &mut Vec<Pending>) -> Result<()> {
        if self.fields.iter().all(|field| field.norms.is_none()) {
            return Ok(());
        }
        let data = self.temporary("nvd")?;
        let metadata = self.temporary("nvm")?;
        let mut consumer = NormsConsumer::new(data, metadata, i64::from(self.document_count))?;
        for index in 0..self.fields.len() {
            let Some(runs) = self.fields[index].norms.take() else {
                continue;
            };
            let number = self.fields[index].number;
            let mut passes = runs.into_sorted_passes()?;
            consumer.add_field(number, &mut passes)?;
        }
        consumer.finish()?;
        self.take_temporary("nvd", pending);
        self.take_temporary("nvm", pending);
        Ok(())
    }
}
