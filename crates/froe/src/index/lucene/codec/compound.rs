//! Compound files: `.cfs` and `.cfe`.
//!
//! `docs/analysis/lucene-4-7-codec.md` §9, from
//! `store/CompoundFileWriter.java`.
//!
//! `.cfs` is a codec header followed by the segment's files concatenated
//! with no padding between them; `.cfe` is the directory over them. **`.si`
//! and `segments_N` stay outside**; everything else a segment owns goes in.

use std::io::{Read, Write};

use crate::error::Result;
use crate::index::lucene::codec::data_output::CodecOutput;
use crate::index::lucene::compound::strip_segment_name;

/// `CompoundFileWriter.DATA_CODEC`.
const DATA_CODEC: &str = "CompoundFileWriterData";

/// `CompoundFileWriter.ENTRY_CODEC`.
const ENTRY_CODEC: &str = "CompoundFileWriterEntries";

/// `VERSION_CURRENT`, which is `VERSION_START`.
const VERSION_CURRENT: i32 = 0;

/// Where one file sits inside `.cfs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompoundEntry {
    /// The **segment-stripped** name — `.fdt`, never `_0.fdt`.
    pub name: String,
    /// Its offset from the start of `.cfs`.
    pub offset: u64,
    /// Its length.
    pub length: u64,
}

/// Concatenates a segment's files into `.cfs`, measuring each as it goes.
pub struct CompoundFileWriter<Sink: Write> {
    data: CodecOutput<Sink>,
    entries: Vec<CompoundEntry>,
}

impl<Sink: Write> CompoundFileWriter<Sink> {
    /// Opens `.cfs` and writes its header.
    pub fn new(sink: Sink) -> Result<Self> {
        let mut data = CodecOutput::new(sink);
        data.write_header(DATA_CODEC, VERSION_CURRENT)?;
        Ok(Self {
            data,
            entries: Vec::new(),
        })
    }

    /// Copies one file in, recording where it landed.
    ///
    /// The name is stripped of its segment prefix for the directory, which
    /// is the namespace a reader looks the file up in.
    pub fn add(&mut self, name: &str, source: &mut impl Read) -> Result<()> {
        let offset = self.data.position();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            self.data.write_bytes(&buffer[..read])?;
        }
        self.entries.push(CompoundEntry {
            name: strip_segment_name(name).to_owned(),
            offset,
            length: self.data.position() - offset,
        });
        Ok(())
    }

    /// The finished `.cfs` and the entries its directory will list.
    pub fn finish(self) -> (Sink, Vec<CompoundEntry>) {
        (self.data.into_inner(), self.entries)
    }
}

/// Writes `.cfe`: a count, then each entry's stripped name, offset and
/// length.
pub fn write_entry_table<Sink: Write>(sink: Sink, entries: &[CompoundEntry]) -> Result<Sink> {
    let mut output = CodecOutput::new(sink);
    output.write_header(ENTRY_CODEC, VERSION_CURRENT)?;
    output.write_vint(entries.len() as i32)?;
    for entry in entries {
        output.write_string(&entry.name)?;
        output.write_long(entry.offset as i64)?;
        output.write_long(entry.length as i64)?;
    }
    Ok(output.into_inner())
}
