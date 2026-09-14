//! The Lucene index writer, end to end.
//!
//! Documents in, an index out, read back with plan 0008's own readers. The
//! field infos are task 0911's oracle — its judge enumerates every field
//! with its options, and no task in this plan builds a `.fnm` reader — so
//! what is checked here is the file set, the document count, the commit,
//! and that spilling changes nothing about the bytes.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};

use froe::index::lucene::codec::postings::IndexOptions;
use froe::index::lucene::codec::segment_info::SegmentDirectory;
use froe::index::lucene::compound::read_table_of_contents;
use froe::index::lucene::read::Reader;
use froe::index::lucene::segments::{read_commit_file, read_segment_info};
use froe::progress::WorkUnit;
use froe::{
    DocValue, Document, LuceneField as Field, LuceneIndexWriter, Result, SortBudget, StoredValue,
    Token,
};

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
    fn reader(&self, name: &str) -> Reader<Cursor<Vec<u8>>> {
        let bytes = self.files.get(name).expect("the file was written").clone();
        let length = bytes.len() as u64;
        Reader::new(Cursor::new(bytes), name, length)
    }
}

/// A working directory for the spills, removed with the test.
struct Workspace {
    path: std::path::PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-lucene-writer-tests-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the workspace");
        Self { path }
    }

    fn runs(&self, prefix: &str) -> froe::RunLocation {
        froe::RunLocation::new(&self.path, prefix)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn token(text: &str, increment: u32, start: u32) -> Token {
    Token {
        bytes: text.as_bytes().to_vec(),
        position_increment: increment,
        start_offset: start,
        end_offset: start + text.len() as u32,
    }
}

fn tokens(words: &[&str]) -> Vec<Token> {
    let mut at = 0u32;
    words
        .iter()
        .map(|word| {
            let built = token(word, 1, at);
            at += word.len() as u32 + 1;
            built
        })
        .collect()
}

/// A document with every capability the writer has: an untokenized string
/// with a sorted doc value, a fulltext field with positions, offsets and
/// norms, a stored value, a numeric doc value, and two fields of one name.
fn capable_document(index: usize) -> Document {
    let path = format!("/content/node-{index}");
    let mut key = Field::indexed(":path", IndexOptions::Documents, vec![token(&path, 1, 0)]);
    key.omit_norms = true;
    key.doc_value = Some(DocValue::Sorted(path.as_bytes().to_vec()));
    key.stored = Some(StoredValue::Text(path.clone()));

    let fulltext = Field::indexed(
        ":fulltext",
        IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets,
        tokens(&["alpha", "beta", "alpha"]),
    );

    // Two fields of one name, which is what Oak writes for every
    // multi-valued property.
    let first = Field::indexed(
        "tags",
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        tokens(&["red", "blue"]),
    );
    let mut second = Field::indexed(
        "tags",
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        tokens(&["green"]),
    );
    second.omit_norms = false;

    let mut ordering = Field::stored(":depth", StoredValue::Integer(index as i32 + 1));
    ordering.doc_value = Some(DocValue::Numeric(index as i64 * 7 - 3));

    let mut facets = Field::indexed("facets", IndexOptions::Documents, Vec::new());
    facets.indexed = false;
    facets.doc_value = Some(DocValue::SortedSet(vec![
        b"one".to_vec(),
        format!("many-{}", index % 3).into_bytes(),
    ]));

    Document::new()
        .with(key)
        .with(fulltext)
        .with(first)
        .with(second)
        .with(ordering)
        .with(facets)
}

fn write(name: &str, documents: usize, budget_bytes: usize) -> (Collected, froe::WrittenIndex) {
    let workspace = Workspace::new(name);
    let mut writer = LuceneIndexWriter::new(
        Collected::default(),
        workspace.runs("writer"),
        SortBudget::of_bytes(budget_bytes),
    );
    for index in 0..documents {
        writer
            .add_document(&capable_document(index))
            .expect("add the document");
    }
    writer.finish().expect("finish")
}

/// A budget large enough that nothing spills.
const ROOMY: usize = 16 * 1024 * 1024;

#[test]
fn a_corpus_of_every_capability_becomes_a_readable_index() {
    let (directory, written) = write("capable", 12, ROOMY);

    assert_eq!(
        written.files,
        vec!["_0.cfe", "_0.cfs", "_0.si", "segments.gen", "segments_1"]
    );
    assert_eq!(written.document_count, 12);
    assert_eq!(written.statistics.skipped_over_long_terms, 0);

    let info = read_segment_info(&mut directory.reader("_0.si")).expect("read the .si");
    assert_eq!(info.document_count, 12);
    assert!(info.compound);
    assert_eq!(info.files, vec!["_0.cfs", "_0.cfe", "_0.si"]);

    let data_length = directory.files["_0.cfs"].len() as i64;
    let table = read_table_of_contents(&mut directory.reader("_0.cfe"), data_length)
        .expect("read the .cfe");
    let mut names: Vec<&str> = table
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            ".doc", ".dvd", ".dvm", ".fdt", ".fdx", ".fnm", ".nvd", ".nvm", ".pay", ".pos", ".tim",
            ".tip"
        ],
        "every format the corpus exercises is inside the compound file"
    );

    let commit = read_commit_file(&mut directory.reader("segments_1"), "segments_1", |name| {
        Ok(directory.reader(name))
    })
    .expect("read the commit");
    assert_eq!(commit.counter, 1);
    assert_eq!(commit.live_document_count(), 12);
    assert_eq!(commit.segments[0].codec_name, "oakCodec");
}

#[test]
fn a_corpus_that_spills_produces_the_same_bytes_as_one_that_does_not() {
    let (roomy, roomy_written) = write("roomy", 40, ROOMY);
    // One record at a time: every push spills, every merge runs.
    let (spilled, spilled_written) = write("spilled", 40, 1);

    assert_eq!(roomy_written, spilled_written);
    assert_eq!(
        roomy.files.keys().collect::<Vec<_>>(),
        spilled.files.keys().collect::<Vec<_>>()
    );
    for (name, bytes) in &roomy.files {
        assert_eq!(
            bytes, &spilled.files[name],
            "{name} came out the same whether or not the runs spilled"
        );
    }
}

#[test]
fn a_writer_that_received_no_document_commits_no_segment() {
    let workspace = Workspace::new("empty");
    let writer = LuceneIndexWriter::new(
        Collected::default(),
        workspace.runs("writer"),
        SortBudget::of_bytes(ROOMY),
    );
    let (directory, written) = writer.finish().expect("finish");
    assert_eq!(written.files, vec!["segments.gen", "segments_1"]);
    assert_eq!(written.document_count, 0);
    assert_eq!(
        directory.files.keys().collect::<Vec<_>>(),
        vec!["segments.gen", "segments_1"],
        "no `_0` file was opened at all"
    );

    let commit = read_commit_file(&mut directory.reader("segments_1"), "segments_1", |name| {
        Ok(directory.reader(name))
    })
    .expect("read the commit");
    assert!(commit.segments.is_empty());
    assert_eq!(commit.counter, 0);
}

#[test]
fn an_over_long_term_is_skipped_and_its_document_kept() {
    let workspace = Workspace::new("over-long");
    let mut writer = LuceneIndexWriter::new(
        Collected::default(),
        workspace.runs("writer"),
        SortBudget::of_bytes(ROOMY),
    );
    let long = "x".repeat(froe::MAXIMUM_TERM_LENGTH + 1);
    let mut field = Field::indexed(
        "body",
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        vec![token("short", 1, 0), token(&long, 1, 6)],
    );
    field.omit_norms = false;
    writer
        .add_document(&Document::new().with(field))
        .expect("the document is kept");
    let (_, written) = writer.finish().expect("finish");
    assert_eq!(written.document_count, 1, "the document survived");
    assert_eq!(written.statistics.skipped_over_long_terms, 1);
}

#[test]
fn a_boost_on_a_field_that_omits_norms_is_refused() {
    let workspace = Workspace::new("boost");
    let mut writer = LuceneIndexWriter::new(
        Collected::default(),
        workspace.runs("writer"),
        SortBudget::of_bytes(ROOMY),
    );
    let mut field = Field::indexed(
        "body",
        IndexOptions::DocumentsAndFrequencies,
        vec![token("a", 1, 0)],
    );
    field.omit_norms = true;
    field.boost = 2.0;
    let refusal = writer
        .add_document(&Document::new().with(field))
        .expect_err("the boost would be discarded");
    assert!(refusal.to_string().contains("omits norms"), "{refusal}");
}

#[test]
fn a_first_token_at_position_increment_zero_is_refused() {
    let workspace = Workspace::new("increment");
    let mut writer = LuceneIndexWriter::new(
        Collected::default(),
        workspace.runs("writer"),
        SortBudget::of_bytes(ROOMY),
    );
    let field = Field::indexed(
        "body",
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        vec![token("a", 0, 0)],
    );
    let refusal = writer
        .add_document(&Document::new().with(field))
        .expect_err("it would sit before the field's first position");
    assert!(
        refusal.to_string().contains("first position increment"),
        "{refusal}"
    );
}

#[test]
fn a_second_doc_value_type_for_one_field_is_refused() {
    let workspace = Workspace::new("doc-value-type");
    let mut writer = LuceneIndexWriter::new(
        Collected::default(),
        workspace.runs("writer"),
        SortBudget::of_bytes(ROOMY),
    );
    let mut first = Field::stored("value", StoredValue::Integer(1));
    first.doc_value = Some(DocValue::Numeric(1));
    writer
        .add_document(&Document::new().with(first))
        .expect("the first document");

    let mut second = Field::stored("value", StoredValue::Integer(2));
    second.doc_value = Some(DocValue::Sorted(b"two".to_vec()));
    let refusal = writer
        .add_document(&Document::new().with(second))
        .expect_err("a type change is refused, not reconciled");
    assert!(refusal.to_string().contains("type change"), "{refusal}");
}

#[test]
fn the_index_document_work_unit_reads_in_both_numbers() {
    assert_eq!(WorkUnit::IndexDocuments.plural_noun(), "index documents");
    assert_eq!(WorkUnit::IndexDocuments.singular_noun(), "index document");
    assert_eq!(WorkUnit::IndexDocuments.noun_for(1), "index document");
    assert_eq!(WorkUnit::IndexDocuments.noun_for(0), "index documents");
    assert_eq!(WorkUnit::IndexDocuments.noun_for(12), "index documents");
}

#[test]
fn a_field_whose_options_downgrade_writes_none_of_the_positions_it_buffered() {
    // The first document indexes with positions and the second without, so
    // the segment-wide options end as the lesser — and the positions the
    // first buffered are never written, which is what Lucene's own flush
    // does when it reads the field's options at flush time.
    let workspace = Workspace::new("downgrade");
    let mut writer = LuceneIndexWriter::new(
        Collected::default(),
        workspace.runs("writer"),
        SortBudget::of_bytes(ROOMY),
    );
    let mut first = Field::indexed(
        "body",
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        tokens(&["alpha", "beta"]),
    );
    first.omit_norms = true;
    writer
        .add_document(&Document::new().with(first))
        .expect("the first document");

    let mut second = Field::indexed(
        "body",
        IndexOptions::DocumentsAndFrequencies,
        tokens(&["alpha"]),
    );
    second.omit_norms = true;
    writer
        .add_document(&Document::new().with(second))
        .expect("the second document");

    let (directory, written) = writer.finish().expect("finish");
    assert_eq!(written.document_count, 2);
    let data_length = directory.files["_0.cfs"].len() as i64;
    let table = read_table_of_contents(&mut directory.reader("_0.cfe"), data_length)
        .expect("read the .cfe");
    let names: Vec<&str> = table
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(
        !names.contains(&".pos"),
        "no field ends with positions, so there is no .pos at all: {names:?}"
    );
}
