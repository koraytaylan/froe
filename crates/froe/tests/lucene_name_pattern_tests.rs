//! `isRegexp` property names, replayed against Java's own engine.
//!
//! `tests/fixtures/java-regular-expression-vectors.tsv` holds a table of
//! patterns and property paths that the judge evaluated inside the pinned
//! image, through Oak's own `NamePattern` logic — the parent-and-name
//! split and a whole-string match, not a bare regular-expression match.
//!
//! Every pattern froe's bounded subset accepts must answer as Java does,
//! and every one it does not must be a refusal rather than a misreading.
//!
//! `docs/analysis/lucene-oak-documents.md` §2.4 is the specification these
//! confirm.

use froe::index::IndexError;
use froe::index::lucene::documents::name_pattern::{ALL_PROPERTIES, NamePattern};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {name}: {error}"))
}

/// One row: a pattern, a property path, and Java's verdict.
struct Vector {
    pattern: String,
    path: String,
    verdict: String,
}

fn vectors() -> Vec<Vector> {
    let text = fixture("java-regular-expression-vectors.tsv");
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 3, "three columns: {line}");
        rows.push(Vector {
            pattern: unescape(fields[0]),
            path: unescape(fields[1]),
            verdict: fields[2].to_owned(),
        });
    }
    rows
}

fn unescape(text: &str) -> String {
    if text == "\\e" {
        String::new()
    } else {
        text.to_owned()
    }
}

/// The patterns froe refuses, with what it refuses them for. Every other
/// pattern in the fixture must be carried exactly.
const REFUSED: [(&str, &str); 10] = [
    (r"\d+", "the escape"),
    (r"\w+", "the escape"),
    ("a{2,3}", "a counted quantifier"),
    ("(?i)abc", "a group flag"),
    ("(?:ab)c", "a group flag"),
    (r"(a)\1", "the escape"),
    ("(?=a)b", "a group flag"),
    ("[[:alpha:]]", "a nested character class"),
    ("a**", "a doubled quantifier"),
    ("[z-a]", "a range whose end precedes its start"),
];

#[test]
fn every_supported_pattern_answers_as_javas_own_engine_does() {
    let mut refused: Vec<String> = Vec::new();
    for vector in vectors() {
        match NamePattern::parse("/oak:index/test", &vector.pattern) {
            Ok(pattern) => {
                assert_ne!(
                    vector.verdict, "invalid",
                    "froe accepted {:?}, which Java's own parser refuses",
                    vector.pattern
                );
                let matched = pattern.matches(&vector.path);
                assert_eq!(
                    matched,
                    vector.verdict == "match",
                    "{:?} against {:?}: Java says {}",
                    vector.pattern,
                    vector.path,
                    vector.verdict
                );
            }
            Err(refusal) => {
                let expected = REFUSED
                    .iter()
                    .find(|(pattern, _)| *pattern == vector.pattern)
                    .unwrap_or_else(|| {
                        panic!(
                            "froe refuses {:?}, which is in the subset: {refusal}",
                            vector.pattern
                        )
                    });
                assert!(
                    refusal.to_string().contains(expected.1),
                    "{:?} was refused for the wrong reason: {refusal}",
                    vector.pattern
                );
                assert!(
                    matches!(refusal, IndexError::UnsupportedNamePattern { .. }),
                    "{refusal}"
                );
                refused.push(vector.pattern.clone());
            }
        }
    }
    refused.sort_unstable();
    refused.dedup();
    let mut expected: Vec<String> = REFUSED
        .iter()
        .map(|(pattern, _)| (*pattern).to_owned())
        .collect();
    expected.sort_unstable();
    assert_eq!(refused, expected, "exactly these patterns are refused");
}

/// The catch-all is the one pattern whose parent is special-cased, and it
/// is what the fixture's default definition uses.
#[test]
fn the_catch_all_pattern_matches_a_relative_name_and_no_path() {
    let pattern = NamePattern::parse("/oak:index/test", ALL_PROPERTIES).expect("the catch-all");
    assert!(pattern.matches("jcr:title"));
    // A hidden name goes through the patterns too, which is the bug
    // compatibility Oak's own comment records.
    assert!(pattern.matches(":nodeName"));
    assert!(!pattern.matches("jcr:content/jcr:title"));
    assert_eq!(pattern.text(), ALL_PROPERTIES);
}

/// Without the special case, a pattern's parent path has to equal the
/// property's — which is what makes `jcr:content/.*` a relative-name
/// pattern and `/.*` one that matches only an absolute path.
#[test]
fn a_patterns_parent_path_must_equal_the_propertys() {
    let relative = NamePattern::parse("/oak:index/test", "jcr:content/.*").expect("a pattern");
    assert!(relative.matches("jcr:content/jcr:title"));
    assert!(!relative.matches("jcr:title"));
    assert!(!relative.matches("other/jcr:title"));
    let absolute = NamePattern::parse("/oak:index/test", "/.*").expect("a pattern");
    assert!(absolute.matches("/jcr:title"));
    assert!(!absolute.matches("jcr:title"));
}

/// A quantifier over a quantified group is where a backtracking matcher
/// goes exponential; froe refuses it rather than hanging.
#[test]
fn a_quantified_quantifier_is_refused() {
    let Err(refusal) = NamePattern::parse("/oak:index/test", "(a*)*b") else {
        panic!("a quantified quantifier is refused");
    };
    assert!(
        refusal
            .to_string()
            .contains("a quantifier over a quantified group"),
        "{refusal}"
    );
}
