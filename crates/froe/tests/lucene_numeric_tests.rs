//! Lucene's numeric encodings, replayed against Lucene's own output.
//!
//! The vectors in `tests/fixtures/lucene-numeric-vectors.tsv` were written
//! by the judge inside the pinned Sling image: the terms each field type's
//! own token stream produced, and what Oak's `FieldFactory.dateToLong`
//! made of a table of date strings. Both halves are Java's, so a
//! divergence here is froe's defect by construction.
//!
//! `docs/analysis/lucene-oak-analysis.md` §8 is the specification these
//! confirm.

use froe::index::IndexError;
use froe::index::lucene::analysis::numeric::{
    NumericTerm, date_to_long, double_terms, double_to_sortable_long, integer_terms, long_terms,
};

fn vectors() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/lucene-numeric-vectors.tsv");
    std::fs::read_to_string(&path).expect("read the numeric vectors")
}

fn hexadecimal(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut rendered, byte| {
        let _ = write!(rendered, "{byte:02x}");
        rendered
    })
}

/// A date's text, with the fixture's escapes resolved.
fn unescape(text: &str) -> String {
    if text == "\\e" {
        return String::new();
    }
    let mut built = String::new();
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            built.push(character);
            continue;
        }
        match characters.next() {
            Some('s') => built.push(' '),
            Some(other) => built.push(other),
            None => built.push('\\'),
        }
    }
    built
}

/// One block of the fixture: a heading and the terms under it.
struct Block {
    heading: Vec<String>,
    terms: Vec<(String, u32)>,
}

fn blocks() -> (Vec<Block>, Vec<(String, String)>) {
    let text = vectors();
    let mut produced: Vec<Block> = Vec::new();
    let mut dates = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        match fields[0] {
            "term" => {
                let block = produced.last_mut().expect("a heading before a term");
                block.terms.push((
                    fields[1].to_owned(),
                    fields[2].parse().expect("an increment"),
                ));
            }
            "date" => dates.push((unescape(fields[1]), fields[2].to_owned())),
            _ => produced.push(Block {
                heading: fields.iter().map(|field| (*field).to_owned()).collect(),
                terms: Vec::new(),
            }),
        }
    }
    (produced, dates)
}

fn assert_terms(produced: &[NumericTerm], expected: &[(String, u32)], what: &str) {
    assert_eq!(produced.len(), expected.len(), "{what}: term count");
    for (at, (term, (bytes, increment))) in produced.iter().zip(expected).enumerate() {
        assert_eq!(&hexadecimal(&term.bytes), bytes, "{what}: term {at} bytes");
        assert_eq!(
            term.position_increment, *increment,
            "{what}: term {at} position increment"
        );
    }
}

#[test]
fn every_numeric_field_produces_lucenes_own_terms() {
    let (blocks, _) = blocks();
    let mut seen = 0;
    for block in &blocks {
        let what = block.heading.join(" ");
        match block.heading[0].as_str() {
            "long" => {
                let value: i64 = block.heading[1].parse().expect("a long");
                assert_terms(&long_terms(value), &block.terms, &what);
            }
            "integer" => {
                let value: i32 = block.heading[1].parse().expect("an integer");
                assert_terms(&integer_terms(value), &block.terms, &what);
            }
            "double" => {
                // The heading carries the raw bits, because a hexadecimal
                // float is Java's spelling and not Rust's, and because the
                // bits are what the doc value would hold.
                let bits = u64::from_str_radix(&block.heading[2], 16).expect("the raw bits");
                assert_terms(&double_terms(f64::from_bits(bits)), &block.terms, &what);
            }
            other => panic!("unknown heading {other}"),
        }
        seen += 1;
    }
    assert!(seen >= 40, "the fixture covers {seen} values");
}

/// Sixteen terms for a long, eight for an integer, all at one position.
#[test]
fn a_numeric_field_is_a_trie_at_one_position() {
    let long = long_terms(1);
    assert_eq!(long.len(), 16);
    let integer = integer_terms(1);
    assert_eq!(integer.len(), 8);
    for terms in [long, integer] {
        assert_eq!(terms[0].position_increment, 1);
        assert!(
            terms[1..].iter().all(|term| term.position_increment == 0),
            "every later term shares the first term's position"
        );
    }
}

/// The sortable long is not the raw bits, and the difference is every
/// negative double — §8.3.
#[test]
fn the_sortable_long_differs_from_the_raw_bits_where_it_must() {
    for value in [0.0f64, 1.0, 2.75, f64::MAX, f64::INFINITY] {
        assert_eq!(
            double_to_sortable_long(value),
            value.to_bits() as i64,
            "a positive double is its own bits"
        );
    }
    for value in [-0.0f64, -1.0, -2.75, f64::MIN, f64::NEG_INFINITY] {
        assert_ne!(
            double_to_sortable_long(value),
            value.to_bits() as i64,
            "a negative double is not"
        );
    }
    // The order is the numeric order, which is the whole point.
    let mut sortable: Vec<i64> = [-2.75f64, -1.0, -0.0, 0.0, 1.0, 2.75]
        .iter()
        .map(|value| double_to_sortable_long(*value))
        .collect();
    let ordered = sortable.clone();
    sortable.sort_unstable();
    assert_eq!(sortable, ordered);
}

#[test]
fn every_date_converts_as_oaks_own_conversion_does() {
    let (_, dates) = blocks();
    assert!(
        dates.len() >= 40,
        "the fixture covers {} dates",
        dates.len()
    );
    for (text, expected) in &dates {
        let produced = date_to_long(text);
        if expected == "refused" {
            let Err(refusal) = produced else {
                panic!("{text:?} was accepted, and Oak refuses it");
            };
            assert!(
                matches!(refusal, IndexError::UnparseableDate { .. }),
                "{text:?}: {refusal}"
            );
        } else {
            let milliseconds: i64 = expected.parse().expect("an epoch millisecond");
            assert_eq!(produced.ok(), Some(milliseconds), "for {text:?}");
        }
    }
}

/// The refusal names the value, because an operator reading it has to find
/// the property in a repository of millions.
#[test]
fn a_refused_date_names_itself() {
    let Err(refusal) = date_to_long("2012-03-01T12:30:45Z") else {
        panic!("a date without milliseconds is refused");
    };
    assert!(
        refusal.to_string().contains("2012-03-01T12:30:45Z"),
        "{refusal}"
    );
}
