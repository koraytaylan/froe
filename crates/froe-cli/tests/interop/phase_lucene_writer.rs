//! The `lucene_writer_conformance` phase: the plan's oracle.
//!
//! A committed corpus describes documents in the writer's own model. froe
//! writes them with its own writer; the judge writes the same corpus with
//! Lucene's, under the same `oakCodec` composition; both directories are
//! enumerated and the two dumps must be identical. Lucene's own index
//! checker runs over froe's directory first, because a dump that matches
//! is worth nothing if the index it came from is malformed.
//!
//! The phase also runs task 0903's committed transducer corpus through
//! `fst-check`, which is the judge's one verdict-only class.

use std::io::Write;
use std::path::{Path, PathBuf};

use froe::index::lucene::codec::postings::IndexOptions;
use froe::index::lucene::codec::segment_info::SegmentDirectory;
use froe::{
    DocValue, Document, LuceneField, LuceneIndexWriter, RunLocation, SortBudget, StoredValue, Token,
};

use super::*;

/// A restricted JSON value: what the corpus's own grammar needs and no
/// more. A number keeps its text, because the corpus carries a long no
/// double can hold.
#[derive(Clone, Debug)]
enum Json {
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
    Text(String),
    Number(String),
    Bool(bool),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// A string or a number, both of which the corpus reads as text — a
    /// long the corpus carries is beyond a double, and a count may be
    /// written either way.
    fn text(&self) -> &str {
        match self {
            Self::Text(text) | Self::Number(text) => text,
            other => panic!("a string was expected and {other:?} came"),
        }
    }

    fn array(&self) -> &[Json] {
        match self {
            Self::Array(values) => values,
            other => panic!("an array was expected and {other:?} came"),
        }
    }

    fn boolean(&self) -> bool {
        match self {
            Self::Bool(value) => *value,
            other => panic!("a boolean was expected and {other:?} came"),
        }
    }
}

/// Parses one line of the corpus.
fn parse_json(line: &str) -> Json {
    let bytes: Vec<char> = line.chars().collect();
    let mut at = 0usize;
    let value = parse_value(&bytes, &mut at);
    skip_space(&bytes, &mut at);
    assert_eq!(at, bytes.len(), "trailing text in {line}");
    value
}

fn skip_space(bytes: &[char], at: &mut usize) {
    while *at < bytes.len() && bytes[*at].is_whitespace() {
        *at += 1;
    }
}

fn parse_value(bytes: &[char], at: &mut usize) -> Json {
    skip_space(bytes, at);
    match bytes[*at] {
        '{' => {
            *at += 1;
            let mut entries = Vec::new();
            loop {
                skip_space(bytes, at);
                if bytes[*at] == '}' {
                    *at += 1;
                    return Json::Object(entries);
                }
                let key = parse_string(bytes, at);
                skip_space(bytes, at);
                assert_eq!(bytes[*at], ':', "a key is followed by a colon");
                *at += 1;
                entries.push((key, parse_value(bytes, at)));
                skip_space(bytes, at);
                if bytes[*at] == ',' {
                    *at += 1;
                }
            }
        }
        '[' => {
            *at += 1;
            let mut values = Vec::new();
            loop {
                skip_space(bytes, at);
                if bytes[*at] == ']' {
                    *at += 1;
                    return Json::Array(values);
                }
                values.push(parse_value(bytes, at));
                skip_space(bytes, at);
                if bytes[*at] == ',' {
                    *at += 1;
                }
            }
        }
        '"' => Json::Text(parse_string(bytes, at)),
        't' => {
            *at += 4;
            Json::Bool(true)
        }
        'f' => {
            *at += 5;
            Json::Bool(false)
        }
        _ => {
            let start = *at;
            while *at < bytes.len()
                && (bytes[*at].is_ascii_digit()
                    || matches!(bytes[*at], '-' | '+' | '.' | 'e' | 'E'))
            {
                *at += 1;
            }
            Json::Number(bytes[start..*at].iter().collect())
        }
    }
}

fn parse_string(bytes: &[char], at: &mut usize) -> String {
    skip_space(bytes, at);
    assert_eq!(bytes[*at], '"', "a string opens with a quote");
    *at += 1;
    let mut text = String::new();
    while bytes[*at] != '"' {
        if bytes[*at] == '\\' {
            *at += 1;
            text.push(match bytes[*at] {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
        } else {
            text.push(bytes[*at]);
        }
        *at += 1;
    }
    *at += 1;
    text
}

/// Where the corpus lives, beside the other fixtures.
fn corpus_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../froe/tests/fixtures/lucene-writer-corpus.jsonl")
}

/// The corpus, expanded into the documents both writers are given.
fn corpus_documents() -> Vec<Document> {
    let text = std::fs::read_to_string(corpus_path()).expect("read the corpus");
    let mut documents = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let entry = parse_json(line);
        let count: usize = entry
            .get("count")
            .map_or(1, |value| value.text().parse().expect("a count"));
        for _ in 0..count {
            let number = documents.len();
            documents.push(build_document(&entry, number));
        }
    }
    documents
}

/// `{n}` stands for the document's number, which is how the corpus reaches
/// eight thousand documents in one line.
fn substitute(text: &str, number: usize) -> String {
    text.replace("{n}", &number.to_string())
}

fn build_document(entry: &Json, number: usize) -> Document {
    let mut document = Document::new();
    let Some(fields) = entry.get("fields") else {
        return document;
    };
    for field in fields.array() {
        document = document.with(build_field(field, number));
    }
    document
}

fn options_named(name: &str) -> IndexOptions {
    match name {
        "docs" => IndexOptions::Documents,
        "freqs" => IndexOptions::DocumentsAndFrequencies,
        "positions" => IndexOptions::DocumentsAndFrequenciesAndPositions,
        "offsets" => IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets,
        other => panic!("unknown index options {other}"),
    }
}

fn decode_hexadecimal(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "hex comes in pairs: {text}");
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("a hex pair"))
        .collect()
}

fn build_field(field: &Json, number: usize) -> LuceneField {
    let name = field.get("name").expect("a field name").text().to_owned();
    let options = field
        .get("options")
        .map_or(IndexOptions::DocumentsAndFrequenciesAndPositions, |value| {
            options_named(value.text())
        });
    let indexed = field.get("indexed").is_none_or(Json::boolean);

    let mut tokens = Vec::new();
    let mut at = 0u32;
    if let Some(list) = field.get("tokens") {
        for token in list.array() {
            let repeat: usize = token
                .get("repeat")
                .map_or(1, |value| value.text().parse().expect("a repeat"));
            let text = substitute(token.get("t").expect("a token").text(), number).repeat(repeat);
            let increment: u32 = token
                .get("i")
                .map_or(1, |value| value.text().parse().expect("an increment"));
            let start: u32 = token
                .get("s")
                .map_or(at, |value| value.text().parse().expect("a start offset"));
            let end: u32 = token.get("e").map_or(start + text.len() as u32, |value| {
                value.text().parse().expect("an end offset")
            });
            at = end + 1;
            tokens.push(Token {
                bytes: text.into_bytes(),
                position_increment: increment,
                start_offset: start,
                end_offset: end,
            });
        }
    }

    let final_offset = field.get("final_offset").map_or_else(
        || tokens.last().map_or(0, |token| token.end_offset),
        |value| value.text().parse().expect("a final offset"),
    );
    let mut built = LuceneField::indexed(name, options, tokens);
    built.indexed = indexed;
    built.final_offset = final_offset;
    built.final_position_increment = field
        .get("final_increment")
        .map_or(0, |value| value.text().parse().expect("a final increment"));
    built.omit_norms = field.get("omit_norms").is_some_and(Json::boolean);
    built.boost = field
        .get("boost")
        .map_or(1.0, |value| value.text().parse().expect("a boost"));

    if let Some(stored) = field.get("stored") {
        built.stored = Some(build_stored(stored, number));
    }
    if let Some(value) = field.get("doc_value") {
        built.doc_value = Some(build_doc_value(value, number));
    }
    built
}

fn build_stored(stored: &Json, number: usize) -> StoredValue {
    if let Some(text) = stored.get("text") {
        return StoredValue::Text(substitute(text.text(), number));
    }
    if let Some(binary) = stored.get("binary") {
        return StoredValue::Binary(decode_hexadecimal(binary.text()));
    }
    if let Some(value) = stored.get("integer") {
        return StoredValue::Integer(
            substitute(value.text(), number)
                .parse()
                .expect("an integer"),
        );
    }
    if let Some(value) = stored.get("long") {
        return StoredValue::Long(substitute(value.text(), number).parse().expect("a long"));
    }
    panic!("a stored value names no type: {stored:?}");
}

fn build_doc_value(value: &Json, number: usize) -> DocValue {
    if let Some(numeric) = value.get("numeric") {
        return DocValue::Numeric(
            substitute(numeric.text(), number)
                .parse()
                .expect("a numeric doc value"),
        );
    }
    if let Some(sorted) = value.get("sorted_text") {
        return DocValue::Sorted(substitute(sorted.text(), number).into_bytes());
    }
    if let Some(sorted) = value.get("sorted") {
        return DocValue::Sorted(decode_hexadecimal(sorted.text()));
    }
    if let Some(set) = value.get("sorted_set") {
        return DocValue::SortedSet(
            set.array()
                .iter()
                .map(|entry| substitute(entry.text(), number).into_bytes())
                .collect(),
        );
    }
    panic!("a doc value names no type: {value:?}");
}

/// A directory of real files, which is what the judge mounts.
struct FileDirectory {
    path: PathBuf,
}

impl SegmentDirectory for FileDirectory {
    fn write_file(
        &mut self,
        name: &str,
        write: &mut dyn FnMut(&mut dyn Write) -> froe::Result<()>,
    ) -> froe::Result<()> {
        let mut file = std::fs::File::create(self.path.join(name))?;
        write(&mut file)?;
        file.sync_all()?;
        Ok(())
    }
}

/// Writes the corpus with froe's own writer.
fn write_with_froe(work: &Path, documents: &[Document]) -> PathBuf {
    let directory = work.join("froe-index");
    let runs = work.join("runs");
    for path in [&directory, &runs] {
        let _ = std::fs::remove_dir_all(path);
        std::fs::create_dir_all(path).expect("create a working directory");
    }
    let mut writer = LuceneIndexWriter::new(
        FileDirectory {
            path: directory.clone(),
        },
        RunLocation::new(&runs, "conformance"),
        // Small enough that the corpus spills, because the spilled path is
        // the one a real reindex takes.
        SortBudget::of_bytes(256 * 1024),
    );
    for document in documents {
        writer.add_document(document).expect("add the document");
    }
    let (_, written) = writer.finish().expect("finish the index");
    assert_eq!(
        written.document_count as usize,
        documents.len(),
        "every corpus document reached the segment"
    );
    assert_eq!(
        written.statistics.skipped_over_long_terms, 1,
        "the corpus carries exactly one term above the maximum length"
    );
    directory
}

/// The phase.
#[test]
#[ignore = "requires podman and the apache/sling:14 image; run `generate` first"]
pub(crate) fn lucene_writer_conformance() {
    let judge = Judge::compile();
    let work = work_root().join("lucene-writer");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create the phase's work directory");

    let documents = corpus_documents();
    eprintln!("  writer: {} documents from the corpus", documents.len());
    assert!(
        documents.len() > 8192,
        "the corpus reaches past the third skip level"
    );

    let froe_index = write_with_froe(&work, &documents);

    eprintln!("  writer: checkindex over froe's index");
    judge.run(
        "LuceneJudge",
        &["checkindex", "/index"],
        vec![Mount::read_only(&froe_index, "/index")],
    );

    eprintln!("  writer: Lucene writes the same corpus");
    let lucene_index = work.join("lucene-index");
    std::fs::create_dir_all(&lucene_index).expect("create Lucene's directory");
    judge.run(
        "Corpus",
        &["build-corpus", "/corpus", "/lucene"],
        vec![
            Mount::read_only(corpus_path(), "/corpus"),
            Mount::writable(&lucene_index, "/lucene"),
        ],
    );

    eprintln!("  writer: enumerate both indexes");
    let dumps = work.join("dumps");
    std::fs::create_dir_all(&dumps).expect("create the dump directory");
    judge.run(
        "Corpus",
        &["enumerate", "/index", "/dumps/froe.txt"],
        vec![
            Mount::read_only(&froe_index, "/index"),
            Mount::writable(&dumps, "/dumps"),
        ],
    );
    judge.run(
        "Corpus",
        &["enumerate", "/index", "/dumps/lucene.txt"],
        vec![
            Mount::read_only(&lucene_index, "/index"),
            Mount::writable(&dumps, "/dumps"),
        ],
    );

    let ours = std::fs::read_to_string(dumps.join("froe.txt")).expect("read froe's dump");
    let theirs = std::fs::read_to_string(dumps.join("lucene.txt")).expect("read Lucene's dump");
    compare_dumps(&ours, &theirs);

    eprintln!("  writer: every transducer enumerates back");
    let corpus =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../froe/tests/fixtures/lucene-fst-corpus.tsv");
    let enumerated = judge.run(
        "FstCheck",
        &["fst-check", "/corpus"],
        vec![Mount::read_only(&corpus, "/corpus")],
    );
    compare_transducer_pairs(
        &std::fs::read_to_string(&corpus).expect("read the transducer corpus"),
        &enumerated,
    );

    eprintln!(
        "  lucene_writer_conformance: {} documents, {} dump lines, identical",
        documents.len(),
        ours.lines().count()
    );
}

/// Compares what Lucene enumerated out of each transducer with the pairs
/// the corpus says froe put in.
///
/// `FstCheck` renders a verdict of its own for nothing: it prints
/// `<name>\t<pairs>` and leaves the judging here, so a transducer whose
/// bytes Lucene *parses* while yielding the wrong keys — or none — is
/// caught by this comparison and by nothing else. Reading its exit status
/// alone would prove only that `new FST<>` did not throw.
fn compare_transducer_pairs(corpus: &str, enumerated: &str) {
    let mut found = enumerated.lines();
    let mut checked = 0usize;
    for row in corpus.lines() {
        if row.starts_with('#') || row.trim().is_empty() {
            continue;
        }
        let mut fields = row.split('\t');
        let name = fields.next().expect("a corpus row names its transducer");
        let _serialized = fields.next().expect("a corpus row carries its bytes");
        let expected = fields
            .next()
            .unwrap_or_else(|| panic!("the corpus row for {name} carries no expected pairs"));
        let line = found
            .next()
            .unwrap_or_else(|| panic!("Lucene enumerated nothing for {name}"));
        let (enumerated_name, pairs) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("the judge's line for {name} has no pairs: {line}"));
        assert_eq!(
            enumerated_name, name,
            "the judge enumerated the transducers in another order"
        );
        assert_eq!(
            pairs, expected,
            "Lucene read {name} back as other pairs than froe put in"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "the transducer corpus carried no row to compare"
    );
    assert!(
        found.next().is_none(),
        "the judge enumerated more transducers than the corpus holds"
    );
    eprintln!("  writer: {checked} transducers enumerate back to their own pairs");
}

/// Compares two enumerations, naming the first line that differs.
fn compare_dumps(ours: &str, theirs: &str) {
    if ours == theirs {
        return;
    }
    let mut our_lines = ours.lines();
    let mut their_lines = theirs.lines();
    let mut at = 0usize;
    loop {
        at += 1;
        match (our_lines.next(), their_lines.next()) {
            (Some(ours), Some(theirs)) if ours == theirs => {}
            // Reached only when the two differ outside their lines, since
            // the equality above has already returned: `lines()` drops a
            // trailing newline, so identical line sequences can still come
            // from dumps that are not the same string.
            (None, None) => panic!(
                "the two enumerations hold the same {} lines and differ \
                 outside them, in trailing whitespace or a final newline",
                at - 1
            ),
            (ours, theirs) => panic!(
                "the two enumerations part at line {at}\n  froe:   {}\n  lucene: {}",
                ours.unwrap_or("<end of dump>"),
                theirs.unwrap_or("<end of dump>")
            ),
        }
    }
}
