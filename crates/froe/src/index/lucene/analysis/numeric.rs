//! Lucene's numeric encodings, for the fields Oak writes as numbers.
//!
//! `docs/analysis/lucene-oak-analysis.md` §8: the prefix coding of a long
//! and of an integer, the sortable-long encoding of a double, and the
//! precision steps the long, integer and double field types use at 4.7,
//! each shift level becoming a term at the same position with
//! `DOCS_ONLY`.
//!
//! **Task 1004 owns this module.** It is declared here so that 1004 and
//! 1005 touch no file this task also touches.
