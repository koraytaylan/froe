//! Oak's document model: from a node state to the exact set of Lucene
//! fields.
//!
//! `docs/analysis/lucene-oak-documents.md`: the indexing rules, the
//! property definitions, the field construction and its order, the
//! aggregates, the facets and the binary text.
//!
//! # What is here
//!
//! [`rules`] for the indexing rules and their property definitions,
//! [`name_pattern`] for `isRegexp` names, [`aggregate`] for the node
//! aggregates, [`document_maker`] for the maker itself, [`facets`] for the
//! facet fields and [`binaries`] for the text a binary property
//! contributes.

pub mod aggregate;
pub mod binaries;
pub mod document_maker;
pub mod facets;
pub mod name_pattern;
pub mod rules;
