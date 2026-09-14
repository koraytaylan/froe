//! Lucene index data stored as repository content: the `:data` directory,
//! the two `jcr:data` encodings, and the filesystem layouts oak-run moves a
//! directory through.
//!
//! `docs/analysis/index-lucene-storage.md` specifies all three. This module
//! deliberately stops at the file boundary: it says what node each Lucene
//! file is stored in and nothing about the bytes inside one, which are the
//! Lucene 4.7.2 format.

pub mod directory;
pub mod layout;

pub use directory::{FileEncoding, OakDirectory, OakIndexFile, OakIndexFileReader};
pub use layout::{IndexDetails, IndexerInfo, filesystem_directory_name, index_folder_base_name};

/// The directory a Lucene index stores its files in, under the definition.
pub const INDEX_DATA_CHILD_NAME: &str = ":data";

/// The directory the suggester stores its files in.
pub const SUGGEST_DATA_CHILD_NAME: &str = ":suggest-data";

/// `MultiplexersLucene.isIndexDirName`: the default name, or a
/// mount-decorated one.
#[must_use]
pub fn is_index_directory_name(name: &str) -> bool {
    name == INDEX_DATA_CHILD_NAME || name.ends_with("-index-data")
}

/// `MultiplexersLucene.isSuggestIndexDirName`.
#[must_use]
pub fn is_suggest_directory_name(name: &str) -> bool {
    name == SUGGEST_DATA_CHILD_NAME || name.ends_with("-suggest-data")
}
