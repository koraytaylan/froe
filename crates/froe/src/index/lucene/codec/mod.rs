//! Writing the Lucene 4.7.2 `oakCodec` composition.
//!
//! The specification is `docs/analysis/lucene-4-7-codec.md`, extracted from
//! the Lucene sources `oak-lucene` vendors at the pinned Oak commit. Every
//! module below cites the section it implements and the Java it was read
//! from.
//!
//! **Public, because this plan's tests are separate crates.** Plan 0007 gives
//! the reason for `writer::index`, and it holds here: a vector test that
//! replays Lucene's own bytes has to reach the writer it is checking, and an
//! integration test is a different crate.
//!
//! # What this composition is
//!
//! `Lucene46` with the postings format replaced. Oak selects it for a
//! fulltext-enabled definition; every other definition takes plain
//! `Lucene46`. Which files a segment carries, and which of them go inside the
//! compound file, is §3 and §9 of the specification.
//!
//! # What it deliberately does not do
//!
//! **No merging and no deletions.** froe writes one segment and never merges
//! it; a deletions file is a file the commit *references* — plan 0008's
//! transport moves it — and not one froe writes. The feasibility verdict
//! records both as out of scope, and nothing in the consumer needs them.

pub mod data_output;
pub mod fst;
pub mod packed;
pub mod postings;
pub mod terms;

// The later tasks of plan 0009 own these. Each is declared here so the
// module tree is the one the specification describes from the start, and so
// a reader looking for a format finds where it will live rather than
// wondering whether it exists.

/// `.fdx` and `.fdt` (§5). Task 0906.
pub mod stored_fields {}

/// `.dvm` and `.dvd` (§8.1). Task 0907.
pub mod doc_values {}

/// `.nvm` and `.nvd` (§8.2). Task 0908.
pub mod norms {}

/// `.fnm` (§4). Task 0909.
pub mod field_infos {}

/// The `.si` descriptor and the `segments_N` commit file (§3). Task 0909.
pub mod segment_info {}

/// `.cfs` and `.cfe` (§9). Task 0909.
pub mod compound {}
