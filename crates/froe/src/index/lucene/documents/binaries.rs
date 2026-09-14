//! Binary text, and the marker froe writes in its place.
//!
//! `docs/analysis/lucene-oak-documents.md` §6. Oak runs Tika over a binary
//! property's content and adds the extracted text as a **stored**
//! `:fulltext` value — or `fullnode:<include path>` for a `relativeNode`
//! aggregate — with a `TextExtractionError` marker from its exception
//! branches.
//!
//! # The departure
//!
//! **froe runs no Tika and extracts no text.** A text extractor is a
//! dependency tree, a set of parsers whose output changes between
//! versions, and a second thing to be wrong; a reindex that guessed at
//! extracted text would produce an index that answers fulltext queries
//! *differently* from Oak's rather than not at all.
//!
//! What froe does instead, in order:
//!
//! 1. **Nothing at all without `jcr:mimeType`** on the node holding the
//!    binary. That is the gate Oak's own extraction applies first, so a
//!    node whose binary has no declared type contributes no text either
//!    way.
//! 2. **The pre-extracted text** Oak's own store holds, when the blob has
//!    an external identity and the operator points froe at that store.
//!    This is text Oak itself extracted, so it is Oak's answer rather than
//!    froe's.
//! 3. **The fallback**: `TextExtractionError`, the marker Oak writes when
//!    its own extraction failed — which is honest, since froe's extraction
//!    did not happen — or nothing, for an operator who would rather the
//!    field were absent.
//!
//! An **inline** segment blob can never be pre-extracted: the store is
//! keyed by the blob's content identity, which an inlined value does not
//! have.

use std::path::{Path, PathBuf};

use crate::content::BinaryValue;

/// The marker Oak's own extraction writes when Tika threw, and what froe
/// writes in place of an extraction it did not perform.
pub const TEXT_EXTRACTION_ERROR: &str = "TextExtractionError";

/// How many leading characters of a blob identifier name the directory
/// levels of Oak's pre-extracted text store: `stripLength` in
/// `DataStoreTextWriter`, whose store lays a blob out as
/// `<first two>/<next two>/<next two>/<the whole identifier>`.
const STRIP_LENGTH: usize = 6;

/// What froe writes for a binary it did not extract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryTextFallback {
    /// Oak's own `TextExtractionError` marker, which keeps the field
    /// present and makes the absence of text visible to a query.
    Marker,
    /// Nothing: the binary contributes no field.
    Skip,
}

/// Where a binary's text comes from, taken by the document maker at
/// construction the way Oak's own maker takes its extractor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryTextPolicy {
    fallback: BinaryTextFallback,
    pre_extracted_text_directory: Option<PathBuf>,
}

impl BinaryTextPolicy {
    /// A policy with no pre-extracted store.
    #[must_use]
    pub const fn new(fallback: BinaryTextFallback) -> Self {
        Self {
            fallback,
            pre_extracted_text_directory: None,
        }
    }

    /// The same, reading Oak's own pre-extracted text store first.
    #[must_use]
    pub fn with_pre_extracted_text_directory(mut self, directory: PathBuf) -> Self {
        self.pre_extracted_text_directory = Some(directory);
        self
    }

    /// What the fallback is.
    #[must_use]
    pub const fn fallback(&self) -> BinaryTextFallback {
        self.fallback
    }

    /// The pre-extracted store, when one was given.
    #[must_use]
    pub fn pre_extracted_text_directory(&self) -> Option<&Path> {
        self.pre_extracted_text_directory.as_deref()
    }

    /// The text for one binary value, or `None` when it contributes
    /// nothing.
    ///
    /// `has_mime_type` is whether the node holding the property declares
    /// `jcr:mimeType`, which is the gate Oak applies before anything else.
    #[must_use]
    pub fn text_of(&self, value: &BinaryValue, has_mime_type: bool) -> Option<String> {
        if !has_mime_type {
            return None;
        }
        if let BinaryValue::External { blob_identifier } = value
            && let Some(text) = self.pre_extracted_text(blob_identifier)
        {
            return Some(text);
        }
        match self.fallback {
            BinaryTextFallback::Marker => Some(TEXT_EXTRACTION_ERROR.to_owned()),
            BinaryTextFallback::Skip => None,
        }
    }

    /// Reads one blob's text out of Oak's own pre-extracted store.
    fn pre_extracted_text(&self, blob_identifier: &str) -> Option<String> {
        let directory = self.pre_extracted_text_directory.as_ref()?;
        // The identity is the part before the first `#`, which Oak appends
        // the length to.
        let identity = blob_identifier.split('#').next()?;
        if identity.len() <= STRIP_LENGTH {
            return None;
        }
        let mut path = directory.clone();
        for level in 0..STRIP_LENGTH / 2 {
            path.push(&identity[level * 2..level * 2 + 2]);
        }
        path.push(identity);
        std::fs::read_to_string(path).ok()
    }
}
