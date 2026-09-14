//! Lucene index data stored as repository content: the `:data` directory,
//! the two `jcr:data` encodings, and the filesystem layouts oak-run moves a
//! directory through.
//!
//! `docs/analysis/index-lucene-storage.md` specifies all three. The directory
//! reader, the layout helpers and the level-1 blob check land in the tasks
//! that fill this module.
