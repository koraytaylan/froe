//! Writing a Lucene index: the document model and the writer over it.
//!
//! The formats are `super::codec`, specified in
//! `docs/analysis/lucene-4-7-codec.md`. This module is what turns documents
//! into them: one segment, the `oakCodec` composition, bounded memory.
//!
//! # What a caller supplies
//!
//! A [`Document`] of [`Field`]s. **Analysis is the caller's**: a field
//! arrives already tokenized, because Oak's analyzer chain is Oak's and
//! froe does not reimplement it. A field may also carry a stored value, a
//! doc value, or neither.
//!
//! # Several fields of one name
//!
//! Oak writes one `Field` per value of a multi-valued property, so a
//! document routinely carries several fields of one name. They compose the
//! way Lucene's own inverter composes them: after each value the end
//! state's position increment and end offset are added, then the
//! position-increment gap and the offset gap; boosts multiply into one
//! norm; and the norm's token and overlap counts sum over the values.
//!
//! # What is bounded, and what is not
//!
//! Postings, doc values and norms are spilled through the workspace's
//! external sort against one shared budget, and merged per format at
//! [`LuceneIndexWriter::finish`]. Stored fields stream out as documents
//! arrive, and a merge pass streams too — it holds one record a cursor,
//! not the group it merges.
//!
//! The budget's accounting unit is a record's own heap and inline bytes.
//! Three resident structures are **not** charged against it, each because
//! it is what Lucene's own writer holds as well:
//!
//! * **one document's inverted form** — every distinct term of one field
//!   group with its positions, built before anything is pushed, so a
//!   single enormous field is bounded by that field rather than by the
//!   budget;
//! * **the terms index of the field being written** — the block-tree
//!   writer's root block index and the transducer it builds from it,
//!   which grow with a field's *distinct term count*, not with its block
//!   count, and are dropped when the field ends;
//! * **the field table**, one entry per distinct field name, each owning
//!   its three sorts.
//!
//! Nor is the `Vec` slot holding a resident record, a per-record constant
//! the budget would have to know the layout to charge. For the short
//! records a property rebuild produces that slot is the larger half, so a
//! declared budget of *n* bytes is a resident set of several *n*.
//!
//! What the budget does bound is the un-spilled tail of each run, which is
//! the part that grows with the *repository* rather than with one document
//! or one field.

mod flush;
mod inverted;

use crate::external_sort::{RunLocation, SortBudget};
use crate::index::lucene::codec::field_infos::DocValuesType;
use crate::index::lucene::codec::postings::IndexOptions;
use crate::index::lucene::codec::segment_info::SegmentDirectory;

pub use inverted::{IndexWriterStatistics, LuceneIndexWriter};

/// Lucene's `IndexWriter.MAX_TERM_LENGTH`.
///
/// A term above it is **skipped and its document kept**, which is what
/// Lucene does — Oak feeds `propertyIndex` strings unguarded, and only its
/// sorted doc values are truncated.
pub const MAXIMUM_TERM_LENGTH: usize = 32_766;

/// One token of a field's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The term's bytes.
    pub bytes: Vec<u8>,
    /// How far the position advances before this token. **One** for an
    /// ordinary token; **zero** for one that overlaps the previous, which
    /// the default similarity discounts from the norm's length.
    pub position_increment: u32,
    /// Where the token starts in the value.
    pub start_offset: u32,
    /// Where it ends.
    pub end_offset: u32,
}

/// A value stored verbatim in `.fdt`.
///
/// The owned twin of the codec's borrowed
/// [`StoredValue`](crate::index::lucene::codec::stored_fields::StoredValue),
/// because a document owns what it carries.
#[derive(Clone, Debug, PartialEq)]
pub enum StoredValue {
    /// A string.
    Text(String),
    /// A byte string.
    Binary(Vec<u8>),
    /// A four-byte integer.
    Integer(i32),
    /// An eight-byte integer.
    Long(i64),
    /// A float, stored as its raw bits.
    Float(f32),
    /// A double, stored as its raw bits.
    Double(f64),
}

/// A value in the column store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocValue {
    /// One number per document. A double doc value is the **raw bits** of
    /// the double, which the caller converts.
    Numeric(i64),
    /// One byte string per document, through a dictionary.
    Sorted(Vec<u8>),
    /// Any number of byte strings per document, through a dictionary.
    SortedSet(Vec<Vec<u8>>),
}

impl DocValue {
    /// The type `.fnm` records for the field.
    const fn kind(&self) -> DocValuesType {
        match self {
            Self::Numeric(_) => DocValuesType::Numeric,
            Self::Sorted(_) => DocValuesType::Sorted,
            Self::SortedSet(_) => DocValuesType::SortedSet,
        }
    }
}

/// One field of one document.
#[derive(Clone, Debug)]
pub struct Field {
    /// The field's name.
    pub name: String,
    /// Its index options. Ignored when it carries no token and is not
    /// indexed.
    pub options: IndexOptions,
    /// Whether it is indexed at all.
    pub indexed: bool,
    /// Its tokens, already analyzed.
    pub tokens: Vec<Token>,
    /// The position increment the token stream reports **after** its last
    /// token, which the next value of the same name starts from.
    ///
    /// Lucene's base `TokenStream.end()` reports **zero**; a filter that
    /// dropped trailing tokens — a stop filter at the end of the value —
    /// overrides it to report what it dropped, so the next value starts
    /// past the hole.
    pub final_position_increment: u32,
    /// The offset the token stream reports after its last token, which a
    /// tokenizer sets to the length of the value it read.
    pub final_offset: u32,
    /// The value stored in `.fdt`, if any.
    pub stored: Option<StoredValue>,
    /// The value in the column store, if any.
    pub doc_value: Option<DocValue>,
    /// Whether norms are omitted. **Sticky across the segment**: one
    /// document that omits them omits them for every document of the
    /// field.
    pub omit_norms: bool,
    /// The field's boost on this document, which multiplies into its norm.
    pub boost: f32,
}

impl Field {
    /// A field carrying only tokens, with the end state an analyzer chain
    /// that dropped nothing would report: no trailing increment, and the
    /// last token's end as the value's length.
    #[must_use]
    pub fn indexed(name: impl Into<String>, options: IndexOptions, tokens: Vec<Token>) -> Self {
        let final_offset = tokens.last().map_or(0, |token| token.end_offset);
        Self {
            name: name.into(),
            options,
            indexed: true,
            tokens,
            final_position_increment: 0,
            final_offset,
            stored: None,
            doc_value: None,
            omit_norms: false,
            boost: 1.0,
        }
    }

    /// A field carrying only a stored value.
    #[must_use]
    pub fn stored(name: impl Into<String>, value: StoredValue) -> Self {
        Self {
            name: name.into(),
            options: IndexOptions::Documents,
            indexed: false,
            tokens: Vec::new(),
            final_position_increment: 0,
            final_offset: 0,
            stored: Some(value),
            doc_value: None,
            omit_norms: true,
            boost: 1.0,
        }
    }
}

/// One document.
#[derive(Clone, Debug, Default)]
pub struct Document {
    /// Its fields, in the order the caller built them. Several of one name
    /// compose.
    pub fields: Vec<Field>,
}

impl Document {
    /// An empty document.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a field.
    #[must_use]
    pub fn with(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }
}

/// What [`LuceneIndexWriter::finish`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenIndex {
    /// The files in the segment directory, sorted: the five-file set of a
    /// single compound segment, or the two-file set of a commit with no
    /// segment at all.
    pub files: Vec<String>,
    /// How many documents the segment holds.
    pub document_count: i32,
    /// What the writer skipped or observed on the way.
    pub statistics: IndexWriterStatistics,
}

/// Opens a writer over `directory`, spilling under `runs`.
///
/// The spills never land in the segment directory: `runs` is the caller's
/// working location and the segment directory holds only the index.
pub fn index_writer<Directory: SegmentDirectory>(
    directory: Directory,
    runs: RunLocation,
    budget: SortBudget,
) -> LuceneIndexWriter<Directory> {
    LuceneIndexWriter::new(directory, runs, budget)
}

/// The codec's borrowed view of a stored value.
pub(crate) fn borrow_stored(
    value: &StoredValue,
) -> crate::index::lucene::codec::stored_fields::StoredValue<'_> {
    use crate::index::lucene::codec::stored_fields::StoredValue as Borrowed;
    match value {
        StoredValue::Text(text) => Borrowed::Text(text),
        StoredValue::Binary(bytes) => Borrowed::Binary(bytes),
        StoredValue::Integer(value) => Borrowed::Integer(*value),
        StoredValue::Long(value) => Borrowed::Long(*value),
        StoredValue::Float(value) => Borrowed::Float(*value),
        StoredValue::Double(value) => Borrowed::Double(*value),
    }
}
