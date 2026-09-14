//! Oak's analysis chain, replayed against Oak's own output.
//!
//! The vectors in `tests/fixtures/oak-analyzer-vectors-*.tsv` were
//! produced by the judge inside the pinned Sling image, by Oak's own
//! analyzers over `tests/fixtures/oak-analyzer-corpus.txt`. Every tuple
//! here — term, position increment, both offsets, type — and every end
//! state is Oak's, not froe's, so a divergence is froe's defect by
//! construction.
//!
//! `docs/analysis/lucene-oak-analysis.md` is the specification these
//! confirm, and §9's worked examples are asserted by name below.

use froe::index::lucene::analysis::{Analyzer, AnalyzerSettings, Token, field_names};

/// The field the judge analyzes in its two whole-analyzer modes: an
/// ordinary analyzed property field, which takes the definition's own
/// analyzer and its cap.
const ORDINARY_FIELD: &str = "full:body";

/// How many tuples the judge keeps at each end of a long stream.
const ELISION_EDGE: usize = 150;

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {name}: {error}"))
}

/// The corpus, with the judge's own escapes resolved: `\n`, `\t`, `\r`,
/// `\\`, `\uNNNN` for one UTF-16 unit, `\xNNNNNN` for one code point and
/// `\*N*TEXT*` for a repetition.
fn corpus() -> Vec<String> {
    let text = fixture("oak-analyzer-corpus.txt");
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        lines.push(unescape(line));
    }
    lines
}

fn unescape(line: &str) -> String {
    let units: Vec<char> = line.chars().collect();
    let mut built = String::new();
    let mut at = 0;
    while at < units.len() {
        if units[at] != '\\' {
            built.push(units[at]);
            at += 1;
            continue;
        }
        at += 1;
        let escaped = units[at];
        at += 1;
        match escaped {
            'n' => built.push('\n'),
            't' => built.push('\t'),
            'r' => built.push('\r'),
            '\\' => built.push('\\'),
            'u' => {
                let digits: String = units[at..at + 4].iter().collect();
                let point = u32::from_str_radix(&digits, 16).expect("four hexadecimal digits");
                built.push(char::from_u32(point).expect("a code point"));
                at += 4;
            }
            'x' => {
                let digits: String = units[at..at + 6].iter().collect();
                let point = u32::from_str_radix(&digits, 16).expect("six hexadecimal digits");
                built.push(char::from_u32(point).expect("a code point"));
                at += 6;
            }
            '*' => {
                let rest: String = units[at..].iter().collect();
                let first = rest.find('*').expect("a count terminator");
                let second = rest[first + 1..].find('*').expect("a text terminator") + first + 1;
                let count: usize = rest[..first].parse().expect("a repetition count");
                let unit = &rest[first + 1..second];
                for _ in 0..count {
                    built.push_str(unit);
                }
                at += rest[..=second].chars().count();
            }
            other => built.push(other),
        }
    }
    built
}

/// One tuple as the vectors carry it.
#[derive(Debug, PartialEq, Eq)]
struct Tuple {
    term: String,
    position_increment: u32,
    start_offset: u32,
    end_offset: u32,
    token_type: String,
}

impl Tuple {
    fn of(token: &Token) -> Self {
        Self {
            term: token.term.clone(),
            position_increment: token.position_increment,
            start_offset: token.start_offset,
            end_offset: token.end_offset,
            token_type: token.token_type.clone(),
        }
    }
}

/// One corpus line's block: how many tuples the chain produced, the ones
/// the judge kept, and the end state.
struct Block {
    line: usize,
    count: usize,
    head: Vec<Tuple>,
    elided: usize,
    tail: Vec<Tuple>,
    end_position_increment: u32,
    end_offset: u32,
}

fn decode(hexadecimal: &str) -> String {
    let bytes: Vec<u8> = (0..hexadecimal.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hexadecimal[at..at + 2], 16).expect("a byte"))
        .collect();
    String::from_utf8(bytes).expect("a term in UTF-8")
}

fn vectors(mode: &str) -> Vec<Block> {
    let text = fixture(&format!("oak-analyzer-vectors-{mode}.tsv"));
    let mut blocks: Vec<Block> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        match fields[0] {
            "line" => blocks.push(Block {
                line: fields[1].parse().expect("a line number"),
                count: 0,
                head: Vec::new(),
                elided: 0,
                tail: Vec::new(),
                end_position_increment: 0,
                end_offset: 0,
            }),
            "count" => {
                blocks.last_mut().expect("a block").count =
                    fields[1].parse().expect("a token count");
            }
            "token" => {
                let block = blocks.last_mut().expect("a block");
                let tuple = Tuple {
                    term: decode(fields[1]),
                    position_increment: fields[2].parse().expect("an increment"),
                    start_offset: fields[3].parse().expect("a start offset"),
                    end_offset: fields[4].parse().expect("an end offset"),
                    token_type: fields[5].to_owned(),
                };
                if block.elided == 0 && block.tail.is_empty() {
                    block.head.push(tuple);
                } else {
                    block.tail.push(tuple);
                }
            }
            "elided" => {
                blocks.last_mut().expect("a block").elided =
                    fields[1].parse().expect("an elided count");
            }
            "end" => {
                let block = blocks.last_mut().expect("a block");
                block.end_position_increment = fields[1].parse().expect("an increment");
                block.end_offset = fields[2].parse().expect("an offset");
            }
            other => panic!("unknown vector record {other}"),
        }
    }
    blocks
}

/// Replays one mode over the whole corpus.
fn replay(mode: &str, analyzer: Analyzer, field_name: &str) {
    let corpus = corpus();
    let blocks = vectors(mode);
    assert_eq!(
        blocks.len(),
        corpus.len(),
        "{mode}: the vectors cover every corpus line"
    );
    for block in &blocks {
        let text = &corpus[block.line];
        let produced = analyzer.tokens(field_name, text);
        let tuples: Vec<Tuple> = produced.tokens.iter().map(Tuple::of).collect();
        assert_eq!(
            tuples.len(),
            block.count,
            "{mode}: line {} token count",
            block.line
        );
        if block.count <= 2 * ELISION_EDGE {
            assert_eq!(
                tuples.len(),
                block.head.len(),
                "{mode}: line {} was recorded whole",
                block.line
            );
            for (at, (produced, expected)) in tuples.iter().zip(&block.head).enumerate() {
                assert_eq!(produced, expected, "{mode}: line {} tuple {at}", block.line);
            }
        } else {
            assert_eq!(
                block.elided,
                block.count - 2 * ELISION_EDGE,
                "{mode}: line {} elision",
                block.line
            );
            for (at, (produced, expected)) in tuples
                .iter()
                .take(ELISION_EDGE)
                .zip(&block.head)
                .enumerate()
            {
                assert_eq!(produced, expected, "{mode}: line {} head {at}", block.line);
            }
            for (at, (produced, expected)) in tuples[tuples.len() - ELISION_EDGE..]
                .iter()
                .zip(&block.tail)
                .enumerate()
            {
                assert_eq!(produced, expected, "{mode}: line {} tail {at}", block.line);
            }
        }
        assert_eq!(
            produced.final_position_increment, block.end_position_increment,
            "{mode}: line {} end increment",
            block.line
        );
        assert_eq!(
            produced.final_offset, block.end_offset,
            "{mode}: line {} end offset",
            block.line
        );
    }
}

/// A definition with Oak's own defaults: the cap at 10,000 and no flag
/// set.
fn plain() -> Analyzer {
    Analyzer::new(AnalyzerSettings::default())
}

#[test]
fn default_chain_matches_oaks_own_analyzer() {
    replay("default", plain(), ORDINARY_FIELD);
}

#[test]
fn original_term_chain_matches_oaks_own_analyzer() {
    let analyzer = Analyzer::new(AnalyzerSettings {
        index_original_term: true,
        ..AnalyzerSettings::default()
    });
    replay("original-term", analyzer, ORDINARY_FIELD);
}

#[test]
fn ancestors_chain_matches_oaks_own_analyzer() {
    let analyzer = Analyzer::new(AnalyzerSettings {
        evaluate_path_restrictions: true,
        ..AnalyzerSettings::default()
    });
    replay("ancestors", analyzer, field_names::ANCESTORS);
}

#[test]
fn spellcheck_chain_matches_oaks_own_analyzer() {
    // The writer's per-field wrapper replaces the definition's analyzer
    // for `:spellcheck` whole, so no cap reaches it.
    replay("spellcheck", plain(), field_names::SPELLCHECK);
}

#[test]
fn suggest_chain_matches_oaks_own_analyzer() {
    replay("suggest", plain(), field_names::SUGGEST);
}

/// Without `evaluatePathRestrictions`, `:ancestors` is an ordinary field.
#[test]
fn ancestors_takes_the_default_chain_without_path_restrictions() {
    let produced = plain().tokens(field_names::ANCESTORS, "/content/interop");
    let terms: Vec<&str> = produced
        .tokens
        .iter()
        .map(|token| token.term.as_str())
        .collect();
    assert_eq!(terms, vec!["content", "interop"]);
}

/// `suggestAnalyzed` gives `:suggest` back to the definition's analyzer,
/// cap included.
#[test]
fn suggest_analyzed_returns_the_field_to_the_definitions_analyzer() {
    let analyzer = Analyzer::new(AnalyzerSettings {
        suggest_analyzed: true,
        ..AnalyzerSettings::default()
    });
    let produced = analyzer.tokens(field_names::SUGGEST, "one\ntwo three");
    let terms: Vec<&str> = produced
        .tokens
        .iter()
        .map(|token| token.term.as_str())
        .collect();
    assert_eq!(terms, vec!["one", "two", "three"]);
}

/// `docs/analysis/lucene-oak-analysis.md` §9.1.
#[test]
fn worked_example_of_oaks_analyzer() {
    let produced = plain().tokens(ORDINARY_FIELD, "Foo-Bar's 3rd j2se PowerShot");
    let tuples: Vec<(String, u32, u32, u32)> = produced
        .tokens
        .iter()
        .map(|token| {
            (
                token.term.clone(),
                token.position_increment,
                token.start_offset,
                token.end_offset,
            )
        })
        .collect();
    assert_eq!(
        tuples,
        vec![
            ("foo".to_owned(), 1, 0, 3),
            ("bar".to_owned(), 1, 4, 7),
            ("3rd".to_owned(), 1, 10, 13),
            ("j2se".to_owned(), 1, 14, 18),
            ("powershot".to_owned(), 1, 19, 28),
        ]
    );
    assert_eq!(produced.final_position_increment, 0);
    assert_eq!(produced.final_offset, 28);
}

/// §9.2: the field holds the node's *parent* path.
#[test]
fn worked_example_of_the_path_hierarchy_chain() {
    let analyzer = Analyzer::new(AnalyzerSettings {
        evaluate_path_restrictions: true,
        ..AnalyzerSettings::default()
    });
    let produced = analyzer.tokens(field_names::ANCESTORS, "/content/interop");
    let tuples: Vec<(String, u32, u32, u32)> = produced
        .tokens
        .iter()
        .map(|token| {
            (
                token.term.clone(),
                token.position_increment,
                token.start_offset,
                token.end_offset,
            )
        })
        .collect();
    assert_eq!(
        tuples,
        vec![
            ("/content".to_owned(), 1, 0, 8),
            ("/content/interop".to_owned(), 0, 0, 16),
        ]
    );
    assert_eq!(produced.final_offset, 16);
}

/// §9.3: unigrams and the shingles that fit.
#[test]
fn worked_example_of_the_shingle_chain() {
    let produced = plain().tokens(field_names::SPELLCHECK, "Foo Bar");
    let tuples: Vec<(String, u32, u32, u32, String)> = produced
        .tokens
        .iter()
        .map(|token| {
            (
                token.term.clone(),
                token.position_increment,
                token.start_offset,
                token.end_offset,
                token.token_type.clone(),
            )
        })
        .collect();
    assert_eq!(
        tuples,
        vec![
            ("foo".to_owned(), 1, 0, 3, "<ALPHANUM>".to_owned()),
            ("foo bar".to_owned(), 0, 0, 7, "shingle".to_owned()),
            ("bar".to_owned(), 1, 4, 7, "<ALPHANUM>".to_owned()),
        ]
    );
}

/// §9.4: the suggest tokenizer splits on the newline alone.
#[test]
fn worked_example_of_the_suggest_chain() {
    let produced = plain().tokens(field_names::SUGGEST, "one\ntwo");
    let terms: Vec<(String, u32, u32, u32)> = produced
        .tokens
        .iter()
        .map(|token| {
            (
                token.term.clone(),
                token.position_increment,
                token.start_offset,
                token.end_offset,
            )
        })
        .collect();
    assert_eq!(
        terms,
        vec![("one".to_owned(), 1, 0, 3), ("two".to_owned(), 1, 4, 7)]
    );
    assert_eq!(produced.final_offset, 7);
}

/// A cap of zero empties the stream in 4.7.2, silently — §5.1.
#[test]
fn a_cap_of_zero_empties_the_stream() {
    let analyzer = Analyzer::new(AnalyzerSettings {
        maximum_field_length: Some(0),
        ..AnalyzerSettings::default()
    });
    let produced = analyzer.tokens(ORDINARY_FIELD, "the quick brown fox");
    assert!(produced.tokens.is_empty());
    assert_eq!(produced.final_position_increment, 0);
    assert_eq!(produced.final_offset, 0);
}

/// The cap stops pulling, so the end state is where the tokenizer stood —
/// not the end of the value. §7.
#[test]
fn a_cap_reports_the_end_of_the_last_token_it_kept() {
    let capped = Analyzer::new(AnalyzerSettings {
        maximum_field_length: Some(2),
        ..AnalyzerSettings::default()
    });
    let produced = capped.tokens(ORDINARY_FIELD, "the quick brown fox");
    assert_eq!(produced.tokens.len(), 2);
    assert_eq!(produced.final_offset, 9);
    let uncapped = Analyzer::new(AnalyzerSettings {
        maximum_field_length: None,
        ..AnalyzerSettings::default()
    });
    let whole = uncapped.tokens(ORDINARY_FIELD, "the quick brown fox");
    assert_eq!(whole.final_offset, 19);
}

// ------------------------------------------------- the generated tables

fn table_source(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/index/lucene/analysis/unicode")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {name}: {error}"))
}

const TABLES: [&str; 7] = [
    "word_break.rs",
    "script.rs",
    "line_break.rs",
    "block.rs",
    "general_category.rs",
    "lower_case.rs",
    "character_class.rs",
];

/// `scripts/oversized-files.sh` refuses a Rust file above a thousand
/// lines, generated or not, so the generator's chunking is part of the
/// contract.
#[test]
fn every_generated_table_is_under_a_thousand_lines() {
    for name in TABLES {
        let lines = table_source(name).lines().count();
        assert!(lines < 1000, "{name} has {lines} lines");
    }
}

/// Every pair the judge dumped from the image's JVM is in the table, and
/// nothing else is.
#[test]
fn the_case_table_is_the_judges_own() {
    let fixture = fixture("java-lower-case-table.tsv");
    let expected: Vec<(u32, u32)> = fixture
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let (point, lower) = line.split_once('\t').expect("two columns");
            (
                u32::from_str_radix(point.trim(), 16).expect("a code point"),
                u32::from_str_radix(lower.trim(), 16).expect("a code point"),
            )
        })
        .collect();
    let table = pairs_of(&table_source("lower_case.rs"));
    assert_eq!(table.len(), expected.len(), "the case table's size");
    let mut sorted = expected;
    sorted.sort_unstable();
    assert_eq!(table, sorted, "the case table's content");
}

/// The character-class table covers exactly the code points the judge
/// classified as something other than the default.
#[test]
fn the_character_class_table_is_the_judges_own() {
    let fixture = fixture("java-character-class-table.tsv");
    let mut expected: Vec<(u32, u32, u8)> = Vec::new();
    for line in fixture.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        expected.push((
            u32::from_str_radix(fields[0].trim(), 16).expect("a first code point"),
            u32::from_str_radix(fields[1].trim(), 16).expect("a last code point"),
            fields[2].trim().parse().expect("a class"),
        ));
    }
    let table = triples_of(&table_source("character_class.rs"));
    assert_eq!(table, expected, "the character-class table's content");
}

/// Reads `(0x…,0x…), ` pairs out of a generated table.
fn pairs_of(source: &str) -> Vec<(u32, u32)> {
    entries_of(source)
        .into_iter()
        .map(|entry| {
            let numbers = numbers_of(&entry);
            (numbers[0], numbers[1])
        })
        .collect()
}

/// Reads `(0x…,0x…,value)` triples out of a generated table, with the
/// value as the number it is.
fn triples_of(source: &str) -> Vec<(u32, u32, u8)> {
    entries_of(source)
        .into_iter()
        .map(|entry| {
            let numbers = numbers_of(&entry);
            let value = entry.rsplit(',').next().expect("a value");
            (
                numbers[0],
                numbers[1],
                value.trim().parse().expect("a numeric value"),
            )
        })
        .collect()
}

fn entries_of(source: &str) -> Vec<String> {
    let body = source.split_once("= &[").expect("a table").1;
    let body = body.rsplit_once("];").expect("a table end").0;
    let mut entries = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for character in body.chars() {
        match character {
            '(' if depth == 0 => depth = 1,
            ')' if depth == 1 => {
                depth = 0;
                entries.push(std::mem::take(&mut current));
            }
            _ if depth == 1 => current.push(character),
            _ => {}
        }
    }
    entries
}

fn numbers_of(entry: &str) -> Vec<u32> {
    entry
        .split(',')
        .take(2)
        .map(|field| {
            let field = field.trim();
            let digits = field.strip_prefix("0x").unwrap_or(field);
            u32::from_str_radix(digits, 16).expect("a hexadecimal number")
        })
        .collect()
}

/// The header a generated table opens with, and the body the digest
/// covers: every line from the first that is not a `//!` header line.
fn header_and_body(source: &str) -> (String, String) {
    let mut header = String::new();
    let mut body = String::new();
    let mut in_header = true;
    for line in source.split_inclusive('\n') {
        if in_header && line.starts_with("//!") {
            header.push_str(line);
        } else {
            in_header = false;
            body.push_str(line);
        }
    }
    (header, body)
}

/// The platform's own SHA-256 over some text.
///
/// froe carries no hash of its own — the generator's header says why, and
/// this test verifies what that generator wrote, so it reads the digest
/// the same way. A machine with neither tool fails loudly rather than
/// passing quietly.
fn checksum_of(text: &str) -> String {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .or_else(|_| {
            Command::new("shasum")
                .args(["-a", "256"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
        })
        .expect("neither sha256sum nor shasum is available");
    child
        .stdin
        .take()
        .expect("the tool's standard input")
        .write_all(text.as_bytes())
        .expect("write to the tool");
    let output = child.wait_with_output().expect("the tool's output");
    assert!(output.status.success(), "checksumming failed");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Each table carries the digest of its own body, so a hand-edit is
/// caught without the Unicode files the generator reads.
#[test]
fn every_generated_table_matches_its_recorded_digest() {
    for name in TABLES {
        let source = table_source(name);
        let (header, body) = header_and_body(&source);
        let marker = "This file's own sha256";
        let recorded = header
            .lines()
            .skip_while(|line| !line.contains(marker))
            .nth(1)
            .and_then(|line| line.trim_start_matches("//!").trim().strip_prefix('`'))
            .and_then(|rest| rest.strip_suffix("`."))
            .unwrap_or_else(|| panic!("{name} records no digest of its own"));
        assert_eq!(checksum_of(&body), recorded, "{name} has been edited");
    }
}

/// Every table is a total function of the code-point space: ascending,
/// non-overlapping ranges with a declared default for the rest.
#[test]
fn every_generated_table_is_ascending_and_disjoint() {
    // `lower_case.rs` is a table of pairs rather than ranges; the test
    // below is its invariant.
    for name in TABLES.into_iter().filter(|name| *name != "lower_case.rs") {
        let source = table_source(name);
        let entries = entries_of(&source);
        assert!(!entries.is_empty(), "{name} is empty");
        let mut previous_last: Option<u32> = None;
        for entry in &entries {
            let numbers = numbers_of(entry);
            let (first, last) = (numbers[0], numbers[1]);
            assert!(first <= last, "{name}: the range {entry} runs backwards");
            assert!(
                last <= 0x10_ffff,
                "{name}: the range {entry} leaves the code-point space"
            );
            if let Some(previous) = previous_last {
                assert!(
                    previous < first,
                    "{name}: {entry} overlaps or repeats what precedes it"
                );
            }
            previous_last = Some(last);
        }
    }
}

/// The two-column tables are pairs, not ranges, so their invariant is
/// that each code point appears once and in order.
#[test]
fn the_case_table_is_ascending_and_single_valued() {
    let table = pairs_of(&table_source("lower_case.rs"));
    let mut previous: Option<u32> = None;
    for (point, lower) in table {
        if let Some(previous) = previous {
            assert!(previous < point, "the case table repeats {point:#x}");
        }
        assert_ne!(point, lower, "{point:#x} maps to itself and is not a row");
        previous = Some(point);
    }
}
