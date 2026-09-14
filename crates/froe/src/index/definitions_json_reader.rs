//! Reading an `index-definitions.json` file back into definition node states.
//!
//! This is the consumer side of [`super::definitions_json`], and the half
//! Oak's own `IndexDefinitionUpdater` performs: the file's key set is index
//! paths, and applying it replaces the whole definition node with the file's
//! state. `docs/analysis/index-definitions.md` §7.2 records the protocol.
//! The reader lands with the importer that needs it, in a later plan; the
//! module exists now so the module root is written once.
