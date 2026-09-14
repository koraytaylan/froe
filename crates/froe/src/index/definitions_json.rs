//! Rendering index definitions in the JSON form Oak's own definition printer
//! emits, which is the file `oak-run index --index-definitions-file` and
//! Oak's definition updater consume.
//!
//! `docs/analysis/index-definitions.md` §8 specifies it to the byte: the
//! filter, the child-order rule, the type codes, the two-phase string
//! escaping and the pretty-printed layout. The renderer lands in the task
//! that fills this module.
