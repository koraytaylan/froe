//! Writing a Lucene index directory into the repository.
//!
//! `docs/analysis/index-lucene-storage.md` §1–§3 specify the shape: a
//! directory node carrying `dirListing`, one child per file carrying
//! `uniqueKey`, `blobSize`, `jcr:lastModified` and `jcr:data`.
//!
//! **The single-blob encoding, and only that one.** §3.4 says which writer
//! produces which: Oak's buffered directory writes one binary per chunk,
//! and its single-blob form writes the file as one binary with the key
//! appended once. froe writes the single-blob form because that is what the
//! consumer build produces by default — `oak.lucene.enableSingleBlobIndexFiles`
//! defaults to true. The buffered form is *read* and never written, which is
//! a departure from the oak-run importer froe replaces, recorded here: it is
//! why plan 0008's round-trip comparison is against the reader's view of the
//! files rather than against the blobs.
//!
//! **`dirListing`'s order is froe's, not Oak's.** Oak stores it in the
//! iteration order of a concurrent hash set and reads it back as a set, so
//! there is no order to reproduce. froe writes it in name order — a
//! deviation recorded here — and every comparison against an Oak-written
//! listing is a set comparison.
//!
//! **No `unsafeForActiveDeletion`.** Oak sets it only under a blob-deletion
//! callback that marks active deletion unsafe, and that callback is a no-op
//! when the store has no external blob store. froe writes no external blobs,
//! so the flag would be a claim about a configuration froe does not create.

use std::collections::BTreeMap;
use std::io::Read;

use crate::content::property::PropertyType;
use crate::error::{Error, Result};
use crate::index::lucene::directory::{
    BLOB_SIZE_PROPERTY, DIRECTORY_LISTING_PROPERTY, UNIQUE_KEY_PROPERTY, UNIQUE_KEY_SIZE,
};
use crate::segment::record::RecordIdentifier;
use crate::writer::record_writer::{
    ChildNodesToWrite, PropertyToWrite, PropertyValuesToWrite, RecordWriter, SegmentSink,
};

/// `OakDirectory`'s own default, and what a definition without an explicit
/// `blobSize` gets.
pub const DEFAULT_BLOB_SIZE: i64 = 1_048_576 - 4096;

/// Whether the directory node carries `dirListing`.
///
/// Oak writes it under `saveDirectoryListing`, which defaults to true; a
/// definition that turned it off is one whose listing must be derived from
/// the child nodes instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirectoryListing {
    /// Write `dirListing`, in name order.
    Saved,
    /// Write none.
    Omitted,
}

/// Builds a `:data`-shaped subtree, one file at a time.
pub struct OakDirectoryWriter<'writer, Sink: SegmentSink> {
    writer: &'writer mut RecordWriter<Sink>,
    blob_size: i64,
    listing: DirectoryListing,
    /// The written children, by name, which is also the listing's order.
    files: BTreeMap<String, RecordIdentifier>,
}

impl<'writer, Sink: SegmentSink> OakDirectoryWriter<'writer, Sink> {
    /// Starts a directory whose files are chunked at `blob_size`.
    pub fn new(
        writer: &'writer mut RecordWriter<Sink>,
        blob_size: i64,
        listing: DirectoryListing,
    ) -> Self {
        Self {
            writer,
            blob_size,
            listing,
            files: BTreeMap::new(),
        }
    }

    /// Adds one file, streaming it.
    ///
    /// The file's bytes and then sixteen key bytes go into a single binary,
    /// which is the single-blob encoding of §3.1. The key is appended
    /// **once**, after the whole file — not per chunk, which is what the
    /// buffered form does.
    pub fn add_file(&mut self, name: &str, reader: impl Read) -> Result<()> {
        if self.files.contains_key(name) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "the index directory already holds a file named {name}; a directory \
                     with two files of one name is not one Oak can read"
                ),
            });
        }

        let mut key = [0u8; UNIQUE_KEY_SIZE as usize];
        crate::writer::identifier_generator::random_bytes(&mut key)?;
        let mut key_text = String::with_capacity(key.len() * 2);
        for byte in &key {
            use std::fmt::Write as _;
            let _ = write!(key_text, "{byte:02x}");
        }

        // The file, then the key: `Read::chain` keeps this streaming, so a
        // file of any size costs one block of memory rather than its length.
        let value = self.writer.write_binary_stream(reader.chain(&key[..]))?;

        let properties = vec![
            PropertyToWrite {
                name: UNIQUE_KEY_PROPERTY.to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Single(self.writer.write_string(&key_text)?),
            },
            PropertyToWrite {
                name: BLOB_SIZE_PROPERTY.to_owned(),
                property_type: PropertyType::Long,
                values: PropertyValuesToWrite::Single(
                    self.writer.write_string(&self.blob_size.to_string())?,
                ),
            },
            PropertyToWrite {
                name: "jcr:lastModified".to_owned(),
                property_type: PropertyType::Long,
                values: PropertyValuesToWrite::Single(
                    self.writer
                        .write_string(&epoch_milliseconds().to_string())?,
                ),
            },
            PropertyToWrite {
                name: "jcr:data".to_owned(),
                property_type: PropertyType::Binary,
                values: PropertyValuesToWrite::Single(value),
            },
        ];
        let node = self
            .writer
            .write_node(None, &[], &ChildNodesToWrite::Zero, &properties)?;
        self.files.insert(name.to_owned(), node);
        Ok(())
    }

    /// Finishes the directory node.
    pub fn finish(self) -> Result<RecordIdentifier> {
        let children: Vec<(String, RecordIdentifier)> = self
            .files
            .iter()
            .map(|(name, record)| (name.clone(), *record))
            .collect();

        let mut properties = Vec::new();
        if self.listing == DirectoryListing::Saved {
            let names: Vec<RecordIdentifier> = self
                .files
                .keys()
                .map(|name| self.writer.write_string(name))
                .collect::<Result<_>>()?;
            properties.push(PropertyToWrite {
                name: DIRECTORY_LISTING_PROPERTY.to_owned(),
                property_type: PropertyType::String,
                values: PropertyValuesToWrite::Multiple(names),
            });
        }

        self.writer.write_node(
            None,
            &[],
            &match children.as_slice() {
                [] => ChildNodesToWrite::Zero,
                [(name, node)] => ChildNodesToWrite::One {
                    name: name.clone(),
                    node: *node,
                },
                many => ChildNodesToWrite::Many(many.to_vec()),
            },
            &properties,
        )
    }
}

/// Now, in epoch milliseconds.
fn epoch_milliseconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(0))
}
