//! The tar archive layer: segment archives on disk.
//!
//! A segment store directory holds numbered archives (`data00000a.tar`,
//! `data00001a.tar`, …) that pack segments together with three trailer
//! entries — a binary references catalog (`.brf`), a segment graph (`.gph`),
//! and an index (`.idx`) — laid out so a reader can find everything by
//! scanning backwards from the end of the file.

pub mod archive;
pub mod binary_references;
pub mod entry_header;
pub mod file_name;
pub mod graph;
pub mod index;

pub use archive::TarArchiveReader;
pub use binary_references::{BinaryReferences, GenerationBinaryReferences};
pub use entry_header::{BLOCK_SIZE, TarEntryHeader};
pub use file_name::{ArchiveFileName, select_newest_file_generations};
pub use graph::SegmentGraph;
pub use index::{SegmentIndex, SegmentIndexEntry};
