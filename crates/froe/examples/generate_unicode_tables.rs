//! Generates the Unicode tables `index::lucene::analysis` reads.
//!
//! `docs/analysis/lucene-oak-analysis.md` §0.3 pins the inputs: the five
//! Unicode Character Database files of **6.3.0**, the version the pinned
//! Lucene grammar's own `%unicode` directive names. They are not committed
//! — they are a megabyte and a half of somebody else's data — so the
//! operator downloads them and passes the directory:
//!
//! ```text
//! mkdir -p /tmp/ucd && cd /tmp/ucd
//! curl -sfLO https://www.unicode.org/Public/6.3.0/ucd/auxiliary/WordBreakProperty.txt
//! curl -sfLO https://www.unicode.org/Public/6.3.0/ucd/Scripts.txt
//! curl -sfLO https://www.unicode.org/Public/6.3.0/ucd/LineBreak.txt
//! curl -sfLO https://www.unicode.org/Public/6.3.0/ucd/Blocks.txt
//! curl -sfLO https://www.unicode.org/Public/6.3.0/ucd/UnicodeData.txt
//! cargo run --example generate_unicode_tables -- /tmp/ucd <fixtures dir>
//! ```
//!
//! Each file is checked against the SHA-256 §0.3 records **before** it is
//! read, by running `sha256sum` — or `shasum -a 256` where coreutils is
//! absent — through a subprocess. froe carries no hash of its own for
//! this: the tool is on every machine that would run the generator, and a
//! second implementation of SHA-256 in the crate would be a second thing
//! to be wrong.
//!
//! The JVM-derived tables — the lower-case mapping and the word-delimiter
//! character classes — come from the committed judge fixtures instead,
//! whose second argument is the fixtures directory. They are the
//! *consumer's* tables and not Unicode 6.3's; §3 and §4.1 say why.
//!
//! Regenerate only deliberately. Each generated file records the command,
//! its input's checksum and its own.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The inputs, with the checksums `lucene-oak-analysis.md` §0.3 pins.
const UNICODE_INPUTS: [(&str, &str); 5] = [
    (
        "WordBreakProperty.txt",
        "4cfdfad5a6bda872f9f3de3dba01896c8d05dda65646d83f3793ac05729734fb",
    ),
    (
        "Scripts.txt",
        "3996337cf6bcf9134a3f0826d82c16e873558f0d020a1799450ceb3edc549651",
    ),
    (
        "LineBreak.txt",
        "6a38069025127a60f4a809e788fbbd1bb6b95ac8d1bd62e6a78d7870357f3486",
    ),
    (
        "Blocks.txt",
        "f0c573bbcc71fcbdab285e4390f49bf711e66bf502d18d8b8c303eaf47661027",
    ),
    (
        "UnicodeData.txt",
        "3f76924f0410ca8ae0e9b5c59bd1ba03196293c32616204b393300f091f52013",
    ),
];

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    if arguments.len() != 3 {
        eprintln!(
            "usage: {} <unicode 6.3.0 data directory> <fixtures directory>",
            arguments[0]
        );
        std::process::exit(2);
    }
    let unicode = PathBuf::from(&arguments[1]);
    let fixtures = PathBuf::from(&arguments[2]);
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/index/lucene/analysis/unicode");

    for (name, expected) in UNICODE_INPUTS {
        verify(&unicode.join(name), expected);
    }

    write_word_break(&unicode, &output);
    write_script(&unicode, &output);
    write_line_break(&unicode, &output);
    write_block(&unicode, &output);
    write_general_category(&unicode, &output);
    write_lower_case(&fixtures, &output);
    write_character_class(&fixtures, &output);
    eprintln!("tables written to {}", output.display());
}

/// Runs the platform's own SHA-256 tool over `path` and compares.
fn verify(path: &Path, expected: &str) {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .or_else(|_| {
            Command::new("shasum")
                .args(["-a", "256"])
                .arg(path)
                .output()
        })
        .unwrap_or_else(|error| {
            panic!("neither sha256sum nor shasum is available: {error}");
        });
    assert!(
        output.status.success(),
        "checksumming {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    let digest = rendered.split_whitespace().next().unwrap_or("");
    assert_eq!(
        digest,
        expected,
        "{} is not the file the specification pins",
        path.display()
    );
}

/// Writes one generated file: the header, the digest of everything below
/// it, and the body.
///
/// The digest covers the **body alone** — every line from the first that
/// is not a `//!` header line — so recording it in the header is not
/// circular, and `lucene_analysis_tests` recomputes it the same way.
fn emit(path: &Path, header: &str, body: &str) {
    let digest = checksum_of(body);
    let rendered = format!(
        "{header}//!\n//! This file's own sha256, over every line below this header:\n\
         //! `{digest}`.\n{body}"
    );
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).expect("create the table directory");
    }
    std::fs::write(path, &rendered).expect("write the table");
    eprintln!(
        "  {}: {} lines",
        path.file_name().unwrap_or_default().to_string_lossy(),
        rendered.lines().count()
    );
}

/// Runs the platform's own SHA-256 tool over some text.
fn checksum_of(text: &str) -> String {
    use std::io::Write as _;
    use std::process::Stdio;

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
        .unwrap_or_else(|error| panic!("neither sha256sum nor shasum is available: {error}"));
    child
        .stdin
        .take()
        .expect("the tool's standard input")
        .write_all(text.as_bytes())
        .expect("write to the tool");
    let output = child.wait_with_output().expect("the tool's output");
    assert!(output.status.success(), "checksumming failed");
    let rendered = String::from_utf8_lossy(&output.stdout);
    rendered
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// One row of a generated table.
type Range = (u32, u32, String);

/// Parses a `first..last; value` database file, keeping the values a
/// predicate accepts and renaming them.
fn parse_ranges(text: &str, keep: impl Fn(&str) -> Option<String>) -> Vec<Range> {
    let mut ranges: Vec<Range> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(';');
        let points = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        let Some(kept) = keep(value) else {
            continue;
        };
        let (first, last) = match points.split_once("..") {
            Some((first, last)) => (first, last),
            None => (points, points),
        };
        let first = u32::from_str_radix(first.trim(), 16).expect("a code point");
        let last = u32::from_str_radix(last.trim(), 16).expect("a code point");
        ranges.push((first, last, kept));
    }
    ranges.sort_by_key(|(first, _, _)| *first);
    ranges
}

/// Writes one table file.
fn write_table(
    output: &Path,
    file: &str,
    documentation: &str,
    source: &str,
    checksum: &str,
    kind: &str,
    ranges: &[Range],
) {
    // A table of a plain type needs no import; one of an enum names it.
    let import = match kind {
        "bool" | "u8" | "u32" => String::new(),
        _ => format!("use super::{kind};\n\n"),
    };
    let header = format!(
        "{documentation}\
         //!\n//! Generated by `cargo run --example generate_unicode_tables`\n\
         //! from `{source}`, sha256 `{checksum}`.\n"
    );
    let mut body = String::from("\n");
    body.push_str(&import);
    let _ = write!(
        body,
        "/// The ranges, ascending, non-overlapping, and total over the code\n\
             /// point space by way of the default every uncovered point takes.\n\
             #[rustfmt::skip]\n\
             pub(crate) const TABLE: &[(u32, u32, {kind})] = &[\n"
    );
    for chunk in ranges.chunks(4) {
        body.push_str("    ");
        for (first, last, value) in chunk {
            let _ = write!(body, "({first:#07x},{last:#07x},{value}), ");
        }
        end_the_line(&mut body);
    }
    body.push_str("];\n");
    emit(&output.join(file), &header, &body);
}

/// Closes a table line, without the separator the last entry wrote.
///
/// A line ending in a space is a `git diff --check` finding, which the
/// contributing guide runs over every review range — so a generated file
/// that carries one puts a permanent exception in every later range that
/// touches it.
fn end_the_line(body: &mut String) {
    while body.ends_with(' ') {
        body.pop();
    }
    body.push('\n');
}

fn read(directory: &Path, name: &str) -> String {
    std::fs::read_to_string(directory.join(name))
        .unwrap_or_else(|error| panic!("read {name}: {error}"))
}

fn write_word_break(unicode: &Path, output: &Path) {
    let names: BTreeMap<&str, &str> = BTreeMap::from([
        ("ALetter", "WordBreak::ALetter"),
        ("Format", "WordBreak::Format"),
        ("Numeric", "WordBreak::Numeric"),
        ("Extend", "WordBreak::Extend"),
        ("Katakana", "WordBreak::Katakana"),
        ("MidLetter", "WordBreak::MidLetter"),
        ("MidNum", "WordBreak::MidNum"),
        ("MidNumLet", "WordBreak::MidNumLet"),
        ("ExtendNumLet", "WordBreak::ExtendNumLet"),
        ("Single_Quote", "WordBreak::SingleQuote"),
        ("Double_Quote", "WordBreak::DoubleQuote"),
        ("Hebrew_Letter", "WordBreak::HebrewLetter"),
        ("Regional_Indicator", "WordBreak::RegionalIndicator"),
        ("CR", "WordBreak::CarriageReturn"),
        ("LF", "WordBreak::LineFeed"),
        ("Newline", "WordBreak::Newline"),
    ]);
    let text = read(unicode, "WordBreakProperty.txt");
    let ranges = parse_ranges(&text, |value| {
        names.get(value).map(|name| (*name).to_owned())
    });
    write_table(
        output,
        "word_break.rs",
        "//! The `Word_Break` property of Unicode 6.3.0.\n//!\n\
         //! The grammar's `\\p{WB:…}` classes, which are the tokenizer's whole\n\
         //! notion of where a word ends. A code point no range covers is\n\
         //! `WordBreak::Other`.\n",
        "WordBreakProperty.txt",
        UNICODE_INPUTS[0].1,
        "WordBreak",
        &ranges,
    );
}

fn write_script(unicode: &Path, output: &Path) {
    let names: BTreeMap<&str, &str> = BTreeMap::from([
        ("Han", "Script::Han"),
        ("Hiragana", "Script::Hiragana"),
        ("Hangul", "Script::Hangul"),
    ]);
    let text = read(unicode, "Scripts.txt");
    let ranges = parse_ranges(&text, |value| {
        names.get(value).map(|name| (*name).to_owned())
    });
    write_table(
        output,
        "script.rs",
        "//! The three scripts the grammar names, from Unicode 6.3.0.\n//!\n\
         //! `\\p{Script:Han}`, `\\p{Script:Hiragana}` and `\\p{Script:Hangul}` —\n\
         //! the only three of the property the tokenizer reads. A code point\n\
         //! no range covers is `Script::Other`.\n",
        "Scripts.txt",
        UNICODE_INPUTS[1].1,
        "Script",
        &ranges,
    );
}

fn write_line_break(unicode: &Path, output: &Path) {
    let text = read(unicode, "LineBreak.txt");
    let ranges = parse_ranges(&text, |value| (value == "SA").then(|| "true".to_owned()));
    write_table(
        output,
        "line_break.rs",
        "//! `\\p{LB:Complex_Context}` of Unicode 6.3.0.\n//!\n\
         //! The South and South-East Asian scripts the grammar keeps together\n\
         //! as one `<SOUTHEAST_ASIAN>` token, as ICU's own implementation of\n\
         //! UAX#29 does. The database spells the value `SA`.\n",
        "LineBreak.txt",
        UNICODE_INPUTS[2].1,
        "bool",
        &ranges,
    );
}

fn write_block(unicode: &Path, output: &Path) {
    let text = read(unicode, "Blocks.txt");
    let ranges = parse_ranges(&text, |value| {
        (value == "Halfwidth and Fullwidth Forms").then(|| "true".to_owned())
    });
    write_table(
        output,
        "block.rs",
        "//! `\\p{Blk:HalfAndFullForms}` of Unicode 6.3.0.\n//!\n\
         //! The one block the grammar names, in the `Numeric` macro's\n\
         //! `[\\p{WB:Numeric}[\\p{Blk:HalfAndFullForms}&&\\p{Nd}]]` — the\n\
         //! full-width digits, which the `Word_Break` property does not call\n\
         //! numeric and the grammar does.\n",
        "Blocks.txt",
        UNICODE_INPUTS[3].1,
        "bool",
        &ranges,
    );
}

fn write_general_category(unicode: &Path, output: &Path) {
    // `UnicodeData.txt` is one line per code point, with a `First`/`Last`
    // pair for a range, so it is folded rather than parsed as ranges.
    let text = read(unicode, "UnicodeData.txt");
    let mut points: Vec<u32> = Vec::new();
    let mut range_start: Option<u32> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(';').collect();
        if fields.len() < 3 {
            continue;
        }
        let point = u32::from_str_radix(fields[0], 16).expect("a code point");
        let name = fields[1];
        let category = fields[2];
        if category != "Nd" {
            range_start = None;
            continue;
        }
        if name.ends_with(", First>") {
            range_start = Some(point);
            continue;
        }
        if name.ends_with(", Last>") {
            let first = range_start.take().expect("a First before a Last");
            points.extend(first..=point);
            continue;
        }
        points.push(point);
    }
    points.sort_unstable();
    let mut ranges: Vec<Range> = Vec::new();
    for point in points {
        match ranges.last_mut() {
            Some((_, last, _)) if *last + 1 == point => *last = point,
            _ => ranges.push((point, point, "true".to_owned())),
        }
    }
    write_table(
        output,
        "general_category.rs",
        "//! `\\p{Nd}` of Unicode 6.3.0: the decimal digits.\n//!\n\
         //! The one general category the grammar names, intersected with the\n\
         //! half- and full-width block in the `Numeric` macro. The database\n\
         //! gives one line per code point, with a `First`/`Last` pair for a\n\
         //! range, so the generator folds it back into ranges.\n",
        "UnicodeData.txt",
        UNICODE_INPUTS[4].1,
        "bool",
        &ranges,
    );
}

fn write_lower_case(fixtures: &Path, output: &Path) {
    let text = read(fixtures, "java-lower-case-table.tsv");
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let (point, lower) = line.split_once('\t').expect("two columns");
        pairs.push((
            u32::from_str_radix(point.trim(), 16).expect("a code point"),
            u32::from_str_radix(lower.trim(), 16).expect("a code point"),
        ));
    }
    pairs.sort_unstable();
    let header = String::from(
        "//! `Character.toLowerCase(int)` as the pinned image's own JVM computes it.\n\
         //!\n\
         //! **Not a Unicode file's table.** Lucene's lower-case filter calls the\n\
         //! JVM's own mapping per code point, so the consumer's tables are the\n\
         //! truth — `docs/analysis/lucene-oak-analysis.md` §3.\n\
         //!\n\
         //! Generated by `cargo run --example generate_unicode_tables` from\n\
         //! `tests/fixtures/java-lower-case-table.tsv`, which the judge wrote\n\
         //! inside the image. Only code points whose lower case differs from\n\
         //! themselves are listed.\n",
    );
    let mut body = String::from(
        "\n/// The mapping, ascending by code point.\n\
         #[rustfmt::skip]\n\
         pub(crate) const TABLE: &[(u32, u32)] = &[\n",
    );
    for chunk in pairs.chunks(6) {
        body.push_str("    ");
        for (point, lower) in chunk {
            let _ = write!(body, "({point:#07x},{lower:#07x}), ");
        }
        end_the_line(&mut body);
    }
    body.push_str("];\n");
    emit(&output.join("lower_case.rs"), &header, &body);
}

fn write_character_class(fixtures: &Path, output: &Path) {
    let text = read(fixtures, "java-character-class-table.tsv");
    let mut ranges: Vec<Range> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 3, "three columns: {line}");
        ranges.push((
            u32::from_str_radix(fields[0].trim(), 16).expect("a code point"),
            u32::from_str_radix(fields[1].trim(), 16).expect("a code point"),
            fields[2].trim().to_owned(),
        ));
    }
    ranges.sort_by_key(|(first, _, _)| *first);
    let header = String::from(
        "//! `WordDelimiterIterator`'s character classes, as the pinned image's\n\
         //! own JVM computes them.\n\
         //!\n\
         //! **Not a Unicode file's table** either, and for the same reason as\n\
         //! the case mapping beside it — `docs/analysis/lucene-oak-analysis.md`\n\
         //! §4.1. `LOWER = 1`, `UPPER = 2`, `ALPHA = 3`, `DIGIT = 4`,\n\
         //! `ALPHANUM = 7`; a code point no range covers is `SUBWORD_DELIM = 8`.\n\
         //!\n\
         //! Generated by `cargo run --example generate_unicode_tables` from\n\
         //! `tests/fixtures/java-character-class-table.tsv`.\n",
    );
    let mut body = String::from(
        "\n/// The classes, ascending and non-overlapping.\n\
         #[rustfmt::skip]\n\
         pub(crate) const TABLE: &[(u32, u32, u8)] = &[\n",
    );
    for chunk in ranges.chunks(5) {
        body.push_str("    ");
        for (first, last, class) in chunk {
            let _ = write!(body, "({first:#07x},{last:#07x},{class}), ");
        }
        end_the_line(&mut body);
    }
    body.push_str("];\n");
    emit(&output.join("character_class.rs"), &header, &body);
}
