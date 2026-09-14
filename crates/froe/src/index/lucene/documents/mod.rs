//! Oak's document model: from a node state to the exact set of Lucene
//! fields.
//!
//! `docs/analysis/lucene-oak-documents.md`: the indexing rules, the
//! property definitions, the field construction and its order, the
//! aggregates, the facets and the binary text.
//!
//! # What is here
//!
//! The definition-side model the maker consults: [`rules`] for the
//! indexing rules and their property definitions, [`name_pattern`] for
//! `isRegexp` names, and [`aggregate`] for the node aggregates. The maker
//! itself, the facets and the binaries are task 1006's.

pub mod aggregate;
pub mod name_pattern;
pub mod rules;

/// The document maker: from a node state to the exact set of fields.
/// Task 1006 owns it.
pub mod document_maker {}

/// The facet fields and the configuration that persists beside them.
/// Task 1006 owns it.
pub mod facets {}

/// Binary text extraction, and the marker froe writes in its place.
/// Task 1006 owns it.
pub mod binaries {}
