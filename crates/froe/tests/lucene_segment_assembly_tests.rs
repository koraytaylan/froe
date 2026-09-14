//! A whole assembled index, read back with plan 0008's own readers.
//!
//! `docs/analysis/lucene-4-7-codec.md` §3, §4 and §9. The claim is not
//! that the bytes look right — it is that the readers froe already has for
//! real Oak indexes accept what froe writes, and that the file set is the
//! one a fresh single-commit `oakCodec` index carries: `_0.cfs`, `_0.cfe`,
//! `_0.si`, `segments_1` and `segments.gen`.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};

use froe::Result;
use froe::index::lucene::codec::field_infos::{DocValuesType, FieldInfo};
use froe::index::lucene::codec::postings::IndexOptions;
use froe::index::lucene::codec::segment_info::{
    CommittedSegment, LUCENE_VERSION, OAK_CODEC, SegmentDescriptor, SegmentDirectory,
    SegmentOutputs, assemble_segment, write_commit_files, write_segment_info,
};
use froe::index::lucene::compound::read_table_of_contents;
use froe::index::lucene::read::Reader;
use froe::index::lucene::segments::{read_commit_file, read_segment_info, read_segments_gen};

/// A directory that keeps what was written to it.
#[derive(Default)]
struct Collected {
    files: BTreeMap<String, Vec<u8>>,
}

impl SegmentDirectory for Collected {
    fn write_file(
        &mut self,
        name: &str,
        write: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
    ) -> Result<()> {
        let mut bytes = Vec::new();
        write(&mut bytes)?;
        self.files.insert(name.to_owned(), bytes);
        Ok(())
    }
}

impl Collected {
    fn names(&self) -> Vec<&str> {
        self.files.keys().map(String::as_str).collect()
    }

    fn reader(&self, name: &str) -> Reader<Cursor<Vec<u8>>> {
        let bytes = self.files.get(name).expect("the file was written").clone();
        let length = bytes.len() as u64;
        Reader::new(Cursor::new(bytes), name, length)
    }
}

/// One field of each capability a segment can carry.
fn fields() -> Vec<FieldInfo> {
    vec![
        // An untokenized string field: indexed without frequencies, no
        // norms, and a sorted doc value beside it.
        FieldInfo {
            name: ":path".to_owned(),
            number: 0,
            indexed: true,
            options: IndexOptions::Documents,
            omits_norms: true,
            doc_values: Some(DocValuesType::Sorted),
            norms: None,
            doc_values_generation: -1,
            attributes: Vec::new(),
        },
        // A fulltext field: positions and offsets, and norms.
        FieldInfo {
            name: ":fulltext".to_owned(),
            number: 1,
            indexed: true,
            options: IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets,
            omits_norms: false,
            doc_values: None,
            norms: Some(DocValuesType::Numeric),
            doc_values_generation: -1,
            attributes: Vec::new(),
        },
        // A field that is stored and not indexed at all: none of the
        // index-option bits are written for it, whatever its options say.
        FieldInfo {
            name: ":stored".to_owned(),
            number: 2,
            indexed: false,
            // Documents-only, whose bit is `0x40` — and which is not
            // written, because the field is not indexed.
            options: IndexOptions::Documents,
            omits_norms: true,
            doc_values: Some(DocValuesType::Numeric),
            norms: None,
            doc_values_generation: -1,
            attributes: vec![("kind".to_owned(), "example".to_owned())],
        },
    ]
}

/// The per-format files a segment's writers would have produced, standing
/// in for their content — the assembly copies them whole and never looks
/// inside.
fn format_files() -> Vec<(String, Vec<u8>)> {
    [
        ("_0.tim", 40usize),
        ("_0.tip", 24),
        ("_0.doc", 32),
        ("_0.pos", 16),
        ("_0.pay", 8),
        ("_0.fdt", 48),
        ("_0.fdx", 34),
        ("_0.dvd", 30),
        ("_0.dvm", 31),
        ("_0.nvd", 26),
        ("_0.nvm", 26),
    ]
    .into_iter()
    .map(|(name, length)| {
        (
            name.to_owned(),
            (0..length).map(|index| index as u8).collect::<Vec<u8>>(),
        )
    })
    .collect()
}

fn assemble() -> (Collected, Vec<(String, Vec<u8>)>) {
    let mut directory = Collected::default();
    let files = format_files();
    let outputs = SegmentOutputs {
        document_count: 5,
        files: files
            .iter()
            .map(|(name, bytes)| {
                let source: Box<dyn std::io::Read> = Box::new(Cursor::new(bytes.clone()));
                (name.clone(), source)
            })
            .collect(),
    };
    assemble_segment(&mut directory, "_0", &fields(), outputs).expect("assemble");
    write_commit_files(
        &mut directory,
        &[CommittedSegment {
            name: "_0".to_owned(),
            codec_name: OAK_CODEC.to_owned(),
        }],
    )
    .expect("commit");
    (directory, files)
}

#[test]
fn the_file_set_is_the_one_a_fresh_commit_carries() {
    let (directory, _) = assemble();
    assert_eq!(
        directory.names(),
        vec!["_0.cfe", "_0.cfs", "_0.si", "segments.gen", "segments_1"]
    );
}

#[test]
fn the_descriptor_lists_the_compound_names_and_itself() {
    let (directory, _) = assemble();
    let info = read_segment_info(&mut directory.reader("_0.si")).expect("read the .si");
    assert_eq!(info.lucene_version, LUCENE_VERSION);
    assert_eq!(info.document_count, 5);
    assert!(info.compound, "the flag is the byte 1, not 0");
    // Three names, not the dozen files the segment actually holds: the
    // compound writer replaced them and the descriptor added its own.
    assert_eq!(info.files, vec!["_0.cfs", "_0.cfe", "_0.si"]);
}

#[test]
fn the_compound_directory_names_every_inner_file_stripped() {
    let (directory, files) = assemble();
    let data_length = directory.files["_0.cfs"].len() as i64;
    let table = read_table_of_contents(&mut directory.reader("_0.cfe"), data_length)
        .expect("read the .cfe");

    let mut expected: Vec<String> = vec![".fnm".to_owned()];
    expected.extend(files.iter().map(|(name, _)| name[2..].to_owned()));
    let produced: Vec<String> = table
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    assert_eq!(produced, expected, "the stripped names, in assembly order");

    // Every entry lands where the table says, with no padding between them.
    for entry in &table.entries {
        let start = entry.offset as usize;
        let end = start + entry.length as usize;
        assert!(
            end <= directory.files["_0.cfs"].len(),
            "{} runs off",
            entry.name
        );
        if entry.name != ".fnm" {
            let source = files
                .iter()
                .find(|(name, _)| name[2..] == entry.name)
                .expect("the file went in");
            assert_eq!(
                &directory.files["_0.cfs"][start..end],
                source.1.as_slice(),
                "{} came back byte for byte",
                entry.name
            );
        }
    }
    let last = table.entries.last().expect("an entry");
    assert_eq!(
        last.offset + last.length,
        data_length,
        "the files are concatenated with nothing after them"
    );
}

#[test]
fn the_commit_names_the_segment_and_carries_a_counter_of_one() {
    let (directory, _) = assemble();
    let commit = read_commit_file(&mut directory.reader("segments_1"), "segments_1", |name| {
        Ok(directory.reader(name))
    })
    .expect("read the commit");

    assert_eq!(commit.generation, 1);
    assert_eq!(commit.version, 0);
    // The next segment name Oak would derive is `_1`. A counter of 0 here
    // would have its first flush name a segment `_0` and overwrite this one.
    assert_eq!(commit.counter, 1);
    assert_eq!(commit.segments.len(), 1);
    assert_eq!(commit.segments[0].name, "_0");
    assert_eq!(commit.segments[0].codec_name, OAK_CODEC);
    assert_eq!(commit.segments[0].deletion_generation, -1);
    assert_eq!(commit.segments[0].deletion_count, 0);
    assert_eq!(commit.segments[0].field_infos_generation, -1);
    assert!(commit.user_data.is_empty());
    assert_eq!(commit.live_document_count(), 5);

    let mut referenced = commit.referenced_files();
    referenced.sort();
    assert_eq!(
        referenced,
        vec!["_0.cfe", "_0.cfs", "_0.si", "segments_1"],
        "segments.gen is a hint no commit names"
    );
}

#[test]
fn the_generation_file_says_one_twice() {
    let (directory, _) = assemble();
    assert_eq!(directory.files["segments.gen"].len(), 20);
    let generation = read_segments_gen(&mut directory.reader("segments.gen"));
    assert_eq!(generation, Some(1));
}

#[test]
fn a_commit_with_no_segment_is_accepted() {
    // Closing an index writer forces a commit, and a writer that received
    // no document flushes no segment — so this is what Oak persists for an
    // empty index.
    let mut directory = Collected::default();
    write_commit_files(&mut directory, &[]).expect("commit");
    assert_eq!(directory.names(), vec!["segments.gen", "segments_1"]);

    let commit = read_commit_file(&mut directory.reader("segments_1"), "segments_1", |name| {
        Ok(directory.reader(name))
    })
    .expect("read the commit");
    assert!(commit.segments.is_empty());
    assert_eq!(commit.counter, 0, "no segment name has been consumed");
    assert_eq!(commit.live_document_count(), 0);
    assert_eq!(commit.referenced_files(), vec!["segments_1"]);
}

#[test]
fn a_segment_that_is_not_compound_writes_the_flag_as_minus_one() {
    // `SegmentInfo.NO` is `-1`, not zero, and the read side dispatches on
    // the byte. froe assembles compound segments only, so this is the one
    // place the other byte is written at all.
    let descriptor = SegmentDescriptor {
        document_count: 2,
        compound: false,
        diagnostics: Vec::new(),
        files: vec!["_0.fdt".to_owned(), "_0.si".to_owned()],
    };
    let bytes = write_segment_info(Vec::new(), &descriptor).expect("write the .si");
    // The header, the version string, the four-byte count, then the flag.
    let at = 4 + 1 + "Lucene46SegmentInfo".len() + 4 + 1 + LUCENE_VERSION.len() + 4;
    assert_eq!(bytes[at], 0xff);

    let length = bytes.len() as u64;
    let info = read_segment_info(&mut Reader::new(Cursor::new(bytes), "_0.si", length))
        .expect("read it back");
    assert!(!info.compound);
    assert_eq!(info.files, vec!["_0.fdt", "_0.si"]);
}

#[test]
fn the_field_infos_are_the_bytes_the_specification_gives() {
    // `.fnm` is the one format this task writes that froe has no reader
    // for, so it is pinned by hand.
    let (directory, _) = assemble();
    let data_length = directory.files["_0.cfs"].len() as i64;
    let table = read_table_of_contents(&mut directory.reader("_0.cfe"), data_length)
        .expect("read the .cfe");
    let entry = table
        .entries
        .iter()
        .find(|entry| entry.name == ".fnm")
        .expect("the field infos went into the compound file");
    let start = entry.offset as usize;
    let produced = &directory.files["_0.cfs"][start..start + entry.length as usize];

    let mut expected = vec![0x3f, 0xd7, 0x6c, 0x17, 18];
    expected.extend_from_slice(b"Lucene46FieldInfos");
    expected.extend_from_slice(&0i32.to_be_bytes());
    expected.push(0x03); // three fields

    // `:path` — indexed, documents only, norms omitted, sorted doc values.
    // `IS_INDEXED | OMIT_TERM_FREQ_AND_POSITIONS | OMIT_NORMS`.
    expected.push(0x05);
    expected.extend_from_slice(b":path");
    expected.push(0x00);
    expected.push(0x01 | 0x40 | 0x10);
    // Norms absent in the high nibble, SORTED in the low.
    expected.push(0x03);
    expected.extend_from_slice(&(-1i64).to_be_bytes());
    expected.extend_from_slice(&0i32.to_be_bytes());

    // `:fulltext` — offsets in the postings, and norms.
    expected.push(0x09);
    expected.extend_from_slice(b":fulltext");
    expected.push(0x01);
    expected.push(0x01 | 0x04);
    // NUMERIC norms in the high nibble, no doc values in the low.
    expected.push(0x10);
    expected.extend_from_slice(&(-1i64).to_be_bytes());
    expected.extend_from_slice(&0i32.to_be_bytes());

    // `:stored` — not indexed, so **none** of the index-option bits are
    // written however its options read.
    expected.push(0x07);
    expected.extend_from_slice(b":stored");
    expected.push(0x02);
    expected.push(0x10);
    expected.push(0x01);
    expected.extend_from_slice(&(-1i64).to_be_bytes());
    // One attribute: the count is a four-byte `Int`, not a vint.
    expected.extend_from_slice(&1i32.to_be_bytes());
    expected.push(0x04);
    expected.extend_from_slice(b"kind");
    expected.push(0x07);
    expected.extend_from_slice(b"example");

    assert_eq!(produced, expected.as_slice());
}

/// CRC-32 (IEEE), bit by bit — an independent copy of what
/// `java.util.zip.CRC32` computes and `ChecksumIndexOutput` appends when it
/// closes a commit file.
fn crc32(bytes: &[u8]) -> u32 {
    let mut value = 0xffff_ffffu32;
    for byte in bytes {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = if value & 1 == 1 {
                (value >> 1) ^ 0xedb8_8320
            } else {
                value >> 1
            };
        }
    }
    !value
}

#[test]
fn the_commit_file_closes_with_its_own_checksum() {
    // froe's reader consumes this eight-byte trailer without recomputing
    // it, so nothing else here would notice a wrong one — and Oak's own
    // reader does verify it, and would refuse the commit.
    let sample = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/lucene-4-7-sample-index/segments_1");
    let bytes = std::fs::read(&sample).expect("the sample commit file");
    let (body, trailer) = bytes.split_at(bytes.len() - 8);
    assert_eq!(
        i64::from_be_bytes(trailer.try_into().expect("eight bytes")),
        i64::from(crc32(body)),
        "the rule read off a commit file Oak itself wrote"
    );

    let (directory, _) = assemble();
    let written = &directory.files["segments_1"];
    let (body, trailer) = written.split_at(written.len() - 8);
    assert_eq!(
        i64::from_be_bytes(trailer.try_into().expect("eight bytes")),
        i64::from(crc32(body)),
        "and the one froe writes"
    );
}
