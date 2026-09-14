//! Reading a Lucene index directory out of the repository: the `:data` node,
//! its listing, and the two encodings a file's `jcr:data` can have.
//!
//! `docs/analysis/index-lucene-storage.md` specifies all of it. The four
//! rules a reader gets wrong, each with a named test:
//!
//! * **`saveDirectoryListing` gates both sides of `dirListing`.** Under
//!   `false` Oak never reads the property, so a stale listing is *ignored*
//!   and the child names are the listing. Reading it unconditionally would
//!   make froe disagree with Oak on exactly the stores where the flag was
//!   turned off after the property was written.
//! * **The listing is authoritative when it is read**, even where it
//!   disagrees with the children: a file node present as a child but absent
//!   from the listing is invisible to Oak.
//! * **`blobSize` is read from the *file node*, and its fallback is the
//!   buffered reader's own 32 KiB constant** — not the definition's
//!   1,047,552. This is the single most likely place for a port to introduce
//!   a silent, content-dependent corruption, because it shows only on file
//!   nodes that lack the property.
//! * **An absent `uniqueKey` is length-neutral.** Both Oak readers read the
//!   key as null and subtract no length, so the file simply has no key bytes
//!   appended.
//!
//! Streams sit on [`crate::content::read_binary_stream`], so a file of any
//! size costs constant memory, and they implement [`io::Seek`] so that plan
//! 0008's descriptor readers can reach a per-segment file inside a compound
//! file at its recorded offset without buffering the whole `.cfs`.

use std::io::{self, Read, Seek, SeekFrom};

use crate::content::node::{NodeState, PropertyState};
use crate::content::value::{BinaryStream, BinaryValue};
use crate::content::{PropertyValues, SegmentProvider, read_binary_stream};
use crate::index::definition::IndexDefinition;
use crate::index::{IndexError, IndexResult, converting_long, strict_string, values_of};
use crate::segment::record::RecordIdentifier;

/// The property a directory node lists its files in.
pub const DIRECTORY_LISTING_PROPERTY: &str = "dirListing";

/// The property a file node stores its sixteen random key bytes in, as
/// thirty-two lower-case hexadecimal characters.
pub const UNIQUE_KEY_PROPERTY: &str = "uniqueKey";

/// The property a file node stores the chunk size it was written with.
pub const BLOB_SIZE_PROPERTY: &str = "blobSize";

/// The property carrying the file's content.
pub const DATA_PROPERTY: &str = "jcr:data";

/// `OakDirectory.UNIQUE_KEY_SIZE`: the number of key bytes appended to every
/// stored blob.
pub const UNIQUE_KEY_SIZE: u64 = 16;

/// `OakBufferedIndexFile.DEFAULT_BLOB_SIZE`: the chunk size the **reader**
/// falls back to for a file node with no `blobSize`.
///
/// Deliberately not the definition's default. Oak resolves the chunk size
/// from the file node and falls back here; a reader that fell back to the
/// definition's 1,047,552 would compute a wrong length and a wrong chunk
/// boundary for every such node.
pub const READER_DEFAULT_BLOB_SIZE: i64 = 32 * 1024;

/// A Lucene index directory: `:data`, `:suggest-data`, or a mount-decorated
/// name.
pub struct OakDirectory<'provider> {
    provider: &'provider dyn SegmentProvider,
    directory_node: NodeState<'provider>,
    definition_path: String,
    child_name: String,
    file_names: Vec<String>,
    listing_was_read: bool,
}

impl<'provider> OakDirectory<'provider> {
    /// Opens `child_name` under a definition node, or `None` when it is
    /// absent — which `:suggest-data` legitimately is on a store whose
    /// suggestions Oak has not built.
    pub fn open(
        provider: &'provider dyn SegmentProvider,
        definition_node: &NodeState<'provider>,
        definition: &IndexDefinition,
        child_name: &str,
    ) -> IndexResult<Option<Self>> {
        let Some(directory_node) = definition_node.child_node(child_name)? else {
            return Ok(None);
        };
        let listing = if definition.lucene.save_directory_listing {
            directory_node.property(DIRECTORY_LISTING_PROPERTY)?
        } else {
            None
        };
        let (file_names, listing_was_read) = match listing {
            Some(property) => (listing_names(&property), true),
            None => (
                directory_node
                    .child_node_entries()?
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect(),
                false,
            ),
        };
        Ok(Some(Self {
            provider,
            directory_node,
            definition_path: definition.path.clone(),
            child_name: child_name.to_owned(),
            file_names,
            listing_was_read,
        }))
    }

    /// The file names, in the order the listing or the child map gives them.
    ///
    /// **Compare this as a set, never as a sequence.** Oak writes the
    /// property from a concurrent hash set, so its order is that set's
    /// iteration order and carries no meaning.
    #[must_use]
    pub fn file_names(&self) -> &[String] {
        &self.file_names
    }

    /// Whether the names came from `dirListing` rather than from the child
    /// map, which is what `saveDirectoryListing` decides.
    #[must_use]
    pub const fn listing_was_read(&self) -> bool {
        self.listing_was_read
    }

    /// The names present as children but absent from the listing, and the
    /// reverse — a disagreement Oak resolves in the listing's favour and
    /// froe reports.
    pub fn listing_disagreements(&self) -> IndexResult<(Vec<String>, Vec<String>)> {
        let children: Vec<String> = self
            .directory_node
            .child_node_entries()?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let only_children = children
            .iter()
            .filter(|name| !self.file_names.contains(name))
            .cloned()
            .collect();
        let only_listed = self
            .file_names
            .iter()
            .filter(|name| !children.contains(name))
            .cloned()
            .collect();
        Ok((only_children, only_listed))
    }

    /// Opens one file by name.
    pub fn file(&self, name: &str) -> IndexResult<OakIndexFile<'provider>> {
        let Some(file_node) = self.directory_node.child_node(name)? else {
            return Err(self.malformed(name, "the file node is absent"));
        };
        OakIndexFile::open(
            self.provider,
            &file_node,
            &self.definition_path,
            &self.child_name,
            name,
        )
    }

    fn malformed(&self, file_name: &str, detail: &str) -> IndexError {
        IndexError::Record(crate::Error::InvalidFormat {
            details: format!(
                "the Lucene index file {}/{}/{file_name} is malformed: {detail}",
                self.definition_path, self.child_name
            ),
        })
    }
}

/// Which of the two encodings a file's `jcr:data` uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileEncoding {
    /// A single `BINARY`: one blob with the unique key appended once. What
    /// Oak's own Lucene editor writes, because
    /// `oak.lucene.enableSingleBlobIndexFiles` defaults to true.
    Streaming,
    /// A `BINARIES` of `blobSize` chunks, each with the key appended. What
    /// Oak's own Lucene **importer** writes.
    Buffered,
}

/// One file of a Lucene index directory.
pub struct OakIndexFile<'provider> {
    provider: &'provider dyn SegmentProvider,
    /// One entry per stored chunk: the value record and its **stored**
    /// length, which includes the trailing unique key.
    chunks: Vec<(RecordIdentifier, u64)>,
    encoding: FileEncoding,
    blob_size: u64,
    unique_key_length: u64,
    length: u64,
    last_modified: Option<i64>,
    description: String,
}

impl<'provider> OakIndexFile<'provider> {
    fn open(
        provider: &'provider dyn SegmentProvider,
        file_node: &NodeState<'provider>,
        definition_path: &str,
        child_name: &str,
        file_name: &str,
    ) -> IndexResult<Self> {
        let description = format!("{definition_path}/{child_name}/{file_name}");
        let malformed = |detail: &str| {
            IndexError::Record(crate::Error::InvalidFormat {
                details: format!("the Lucene index file {description} is malformed: {detail}"),
            })
        };

        let unique_key_length = match file_node.property(UNIQUE_KEY_PROPERTY)? {
            None => 0,
            Some(property) => {
                let Some(text) = strict_string(Some(&property)) else {
                    return Err(malformed(
                        "its uniqueKey is not stored as a single String, so Oak reads it as \
                         null and fails on the first value it tests",
                    ));
                };
                (text.len() / 2) as u64
            }
        };
        let blob_size = converting_long(file_node.property(BLOB_SIZE_PROPERTY)?.as_ref())
            .unwrap_or(READER_DEFAULT_BLOB_SIZE);
        let blob_size =
            u64::try_from(blob_size).map_err(|_| malformed("its blobSize is negative"))?;
        if blob_size == 0 {
            return Err(malformed("its blobSize is zero"));
        }

        let Some(data) = file_node.property(DATA_PROPERTY)? else {
            return Err(malformed("it has no jcr:data property"));
        };
        let (encoding, chunks) = read_chunks(&data, &malformed)?;
        let length = declared_length(encoding, &chunks, blob_size, unique_key_length, &malformed)?;

        Ok(Self {
            provider,
            chunks,
            encoding,
            blob_size,
            unique_key_length,
            length,
            last_modified: converting_long(file_node.property("jcr:lastModified")?.as_ref()),
            description,
        })
    }

    /// The file's length as Oak computes it.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }

    /// Which encoding the file uses, which says which tool last wrote it.
    #[must_use]
    pub const fn encoding(&self) -> FileEncoding {
        self.encoding
    }

    /// The chunk size resolved from the file node.
    #[must_use]
    pub const fn blob_size(&self) -> u64 {
        self.blob_size
    }

    /// `jcr:lastModified`, the wall-clock millisecond of the flush. Not
    /// reproducible and not comparable between two builds of the same
    /// content.
    #[must_use]
    pub const fn last_modified(&self) -> Option<i64> {
        self.last_modified
    }

    /// A reader over the file's bytes, with the trailing key of every chunk
    /// withheld.
    #[must_use]
    pub fn reader(&self) -> OakIndexFileReader<'provider, '_> {
        OakIndexFileReader {
            file: self,
            position: 0,
            chunk: None,
        }
    }
}

impl std::fmt::Debug for OakIndexFile<'_> {
    /// Names the file and what was read of it. The provider has no `Debug`
    /// and would say nothing useful anyway; what a failing assertion wants is
    /// which file, how long, and in which encoding.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OakIndexFile")
            .field("path", &self.description)
            .field("encoding", &self.encoding)
            .field("length", &self.length)
            .field("blob_size", &self.blob_size)
            .field("chunks", &self.chunks.len())
            .finish()
    }
}

/// A `Read + Seek` view of one index file.
///
/// The seek is over the *chunk sequence*: the buffered encoding's `jcr:data`
/// is an array of separate binary values, so positioning means choosing a
/// chunk and seeking inside it. That is why this type exists rather than the
/// caller seeking a [`BinaryStream`] directly.
pub struct OakIndexFileReader<'provider, 'file> {
    file: &'file OakIndexFile<'provider>,
    position: u64,
    chunk: Option<(usize, BinaryStream<'provider>)>,
}

impl<'provider> OakIndexFileReader<'provider, '_> {
    /// The current position.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    /// Reads into `buffer`, preserving froe's typed errors.
    ///
    /// A read stops at the current chunk's boundary even when `buffer` has
    /// more room, as [`io::Read`] permits.
    pub fn read_chunk(&mut self, buffer: &mut [u8]) -> IndexResult<usize> {
        if buffer.is_empty() || self.position >= self.file.length {
            return Ok(0);
        }
        let (chunk_index, offset_in_chunk) = self.file.locate(self.position);
        let payload = self.file.payload_bytes(chunk_index)?;
        let available = payload.saturating_sub(offset_in_chunk);
        if available == 0 {
            return Ok(0);
        }
        let wanted = (buffer.len() as u64)
            .min(available)
            .min(self.file.length - self.position) as usize;

        let stream = self.stream_for(chunk_index, offset_in_chunk)?;
        let read = stream.read(&mut buffer[..wanted]).map_err(io_to_index)?;
        self.position += read as u64;
        Ok(read)
    }

    /// The stream for `chunk_index`, positioned at `offset_in_chunk`,
    /// reusing the open one when the position stayed inside it.
    fn stream_for(
        &mut self,
        chunk_index: usize,
        offset_in_chunk: u64,
    ) -> IndexResult<&mut BinaryStream<'provider>> {
        let reopen = match &self.chunk {
            Some((open_index, stream)) => {
                *open_index != chunk_index || stream.position() != offset_in_chunk
            }
            None => true,
        };
        if reopen {
            let (record, _) = *self
                .file
                .chunks
                .get(chunk_index)
                .ok_or_else(|| self.file.malformed("a chunk is missing"))?;
            let mut stream =
                read_binary_stream(self.file.provider, record).map_err(IndexError::Record)?;
            stream
                .seek(SeekFrom::Start(offset_in_chunk))
                .map_err(io_to_index)?;
            self.chunk = Some((chunk_index, stream));
        }
        let (_, stream) = self.chunk.as_mut().expect("the chunk was just opened");
        Ok(stream)
    }
}

impl Read for OakIndexFileReader<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_chunk(buffer).map_err(index_to_io)
    }
}

impl Seek for OakIndexFileReader<'_, '_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let (origin, offset) = match position {
            SeekFrom::Start(offset) => {
                self.position = refuse_past_end(offset, self.file.length)?;
                return Ok(self.position);
            }
            SeekFrom::End(offset) => (self.file.length, offset),
            SeekFrom::Current(offset) => (self.position, offset),
        };
        let absolute = origin.checked_add_signed(offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seeking to a negative position in a Lucene index file",
            )
        })?;
        self.position = refuse_past_end(absolute, self.file.length)?;
        Ok(self.position)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.position)
    }
}

fn refuse_past_end(position: u64, length: u64) -> io::Result<u64> {
    if position > length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("seeking to {position} in a Lucene index file of {length} bytes"),
        ));
    }
    Ok(position)
}

fn io_to_index(error: io::Error) -> IndexError {
    IndexError::Record(crate::Error::InputOutput(error))
}

fn index_to_io(error: IndexError) -> io::Error {
    match error {
        IndexError::Record(crate::Error::InputOutput(source)) => source,
        other => io::Error::other(other),
    }
}

impl OakIndexFile<'_> {
    fn malformed(&self, detail: &str) -> IndexError {
        IndexError::Record(crate::Error::InvalidFormat {
            details: format!(
                "the Lucene index file {} is malformed: {detail}",
                self.description
            ),
        })
    }

    /// Which chunk a file position falls in, and where inside it.
    ///
    /// **`blobSize` is the buffered encoding's chunking and nothing else.** A
    /// streaming file is one blob of whatever length it has, which may be far
    /// larger than the `blobSize` its node records — the real Sling fixture's
    /// `_0.cfs` is a 1.9 MB single blob beside a `blobSize` of 1,047,552.
    /// Dividing its position by `blobSize` would address a second chunk that
    /// does not exist.
    const fn locate(&self, position: u64) -> (usize, u64) {
        match self.encoding {
            FileEncoding::Streaming => (0, position),
            FileEncoding::Buffered => (
                (position / self.blob_size) as usize,
                position % self.blob_size,
            ),
        }
    }

    /// How many bytes of chunk `index` belong to the file, with its trailing
    /// key excluded.
    fn payload_bytes(&self, index: usize) -> IndexResult<u64> {
        let (_, stored) = *self
            .chunks
            .get(index)
            .ok_or_else(|| self.malformed("a chunk is missing"))?;
        let payload = stored
            .checked_sub(self.unique_key_length)
            .ok_or_else(|| self.malformed("a chunk is shorter than its unique key"))?;
        match self.encoding {
            // One blob, whatever its length.
            FileEncoding::Streaming => Ok(payload),
            // The last chunk may be short; every other one carries a full
            // `blobSize` of payload.
            FileEncoding::Buffered => Ok(payload.min(self.blob_size)),
        }
    }
}

/// `OakIndexFile.getOakIndexFile`: a single `BINARY` reads as streaming, and
/// anything else as buffered.
///
/// > **froe deviation (stricter, never permissive).** Oak takes the buffered
/// > branch for a `jcr:data` of any third type, finds it is not `BINARIES`,
/// > and reads the file as **zero-length with no error at all**. froe raises
/// > a typed error instead. The deviation loses no data Oak would return —
/// > Oak returns nothing for such a node either — and writes no bytes; it
/// > only refuses to present a silent zero where the store is malformed.
fn read_chunks(
    data: &PropertyState,
    malformed: &impl Fn(&str) -> IndexError,
) -> IndexResult<(FileEncoding, Vec<(RecordIdentifier, u64)>)> {
    let mut chunks = Vec::new();
    for value in values_of(data) {
        match value {
            crate::content::PropertyValue::Binary(BinaryValue::Inline {
                length,
                record_identifier,
            }) => chunks.push((*record_identifier, *length)),
            crate::content::PropertyValue::Binary(BinaryValue::External { .. }) => {
                return Err(malformed(
                    "its content is in an external blob store, which froe does not read",
                ));
            }
            _ => {
                return Err(malformed(
                    "its jcr:data is neither a Binary nor a Binary[]; Oak reads such a file as \
                     zero-length without erroring, and froe refuses rather than presenting a \
                     silent zero",
                ));
            }
        }
    }
    match &data.values {
        PropertyValues::Single(_) => Ok((FileEncoding::Streaming, chunks)),
        PropertyValues::Multiple(_) => Ok((FileEncoding::Buffered, chunks)),
    }
}

/// The length Oak computes for each encoding.
fn declared_length(
    encoding: FileEncoding,
    chunks: &[(RecordIdentifier, u64)],
    blob_size: u64,
    unique_key_length: u64,
    malformed: &impl Fn(&str) -> IndexError,
) -> IndexResult<u64> {
    match encoding {
        // `OakStreamingIndexFile`: the blob length minus the key length,
        // never below zero.
        FileEncoding::Streaming => {
            let stored = chunks.first().map_or(0, |(_, length)| *length);
            Ok(stored.saturating_sub(unique_key_length))
        }
        // `OakBufferedIndexFile`: `n * blobSize - (blobSize - last.length())
        // - key length`, which reduces to `(n - 1) * blobSize + payload(last)`
        // because `last.length()` is the *stored* length and so includes the
        // key. An empty array is a zero-length file.
        FileEncoding::Buffered => {
            let Some((_, last_stored)) = chunks.last().copied() else {
                return Ok(0);
            };
            let chunk_count = chunks.len() as u64;
            let full = chunk_count
                .checked_mul(blob_size)
                .ok_or_else(|| malformed("its chunk arithmetic overflows"))?;
            let corrected = full
                .checked_add(last_stored)
                .and_then(|value| value.checked_sub(blob_size))
                .and_then(|value| value.checked_sub(unique_key_length))
                .ok_or_else(|| {
                    malformed(
                        "its last chunk is shorter than its unique key, or the length \
                               does not fit the chunk arithmetic",
                    )
                })?;
            Ok(corrected)
        }
    }
}

/// `dirListing` read as Oak reads it: `listing.getValue(Type.STRINGS)`,
/// which converts.
fn listing_names(property: &PropertyState) -> Vec<String> {
    values_of(property)
        .iter()
        .filter_map(crate::content::PropertyValue::as_text)
        .collect()
}
