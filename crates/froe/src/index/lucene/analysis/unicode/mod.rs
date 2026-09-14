//! The tables the analysis chain reads, and the lookups over them.
//!
//! Two provenances, and confusing them is the mistake this module exists
//! to prevent — `docs/analysis/lucene-oak-analysis.md` §0.3, §3 and §4.1:
//!
//! * **Unicode 6.3.0** for the tokenizer's word breaks, scripts, line
//!   breaks, block and decimal digits. Those tables were baked into
//!   Lucene's generated scanner when 4.7.2 was released, and a newer
//!   Unicode does not change a single token.
//! * **The consumer's own JVM** for the lower-case mapping and the
//!   word-delimiter character classes, because Lucene calls
//!   `Character.toLowerCase(int)` and `Character.getType(int)` and gets
//!   whatever the running JVM knows.
//!
//! Every table is generated — `cargo run --example generate_unicode_tables`
//! — and each file records the command, its input's checksum and its own.

pub(crate) mod block;
pub(crate) mod character_class;
pub(crate) mod general_category;
pub(crate) mod line_break;
pub(crate) mod lower_case;
pub(crate) mod script;
pub(crate) mod word_break;

/// The `Word_Break` values the grammar names, and `Other` for the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WordBreak {
    Other,
    ALetter,
    Format,
    Numeric,
    Extend,
    Katakana,
    MidLetter,
    MidNum,
    MidNumLet,
    ExtendNumLet,
    SingleQuote,
    DoubleQuote,
    HebrewLetter,
    RegionalIndicator,
    CarriageReturn,
    LineFeed,
    Newline,
}

/// The three scripts the grammar names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Script {
    Other,
    Han,
    Hiragana,
    Hangul,
}

/// Looks a code point up in an ascending, non-overlapping range table.
fn lookup<Value: Copy>(table: &[(u32, u32, Value)], point: u32, default: Value) -> Value {
    let found = table.binary_search_by(|(first, last, _)| {
        if point < *first {
            std::cmp::Ordering::Greater
        } else if point > *last {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    });
    found.map_or(default, |at| table[at].2)
}

/// The code point's `Word_Break` value.
pub(crate) fn word_break(point: u32) -> WordBreak {
    lookup(word_break::TABLE, point, WordBreak::Other)
}

/// The code point's script, for the three the grammar names.
pub(crate) fn script(point: u32) -> Script {
    lookup(script::TABLE, point, Script::Other)
}

/// Whether the code point is `\p{LB:Complex_Context}`.
pub(crate) fn is_complex_context(point: u32) -> bool {
    lookup(line_break::TABLE, point, false)
}

/// Whether the code point is in the half- and full-width forms block.
pub(crate) fn is_half_and_full_forms(point: u32) -> bool {
    lookup(block::TABLE, point, false)
}

/// Whether the code point is `\p{Nd}`.
pub(crate) fn is_decimal_digit(point: u32) -> bool {
    lookup(general_category::TABLE, point, false)
}

/// `Character.toLowerCase(int)`, from the consumer's own table.
pub(crate) fn to_lower_case(point: u32) -> u32 {
    lower_case::TABLE
        .binary_search_by_key(&point, |(from, _)| *from)
        .map_or(point, |at| lower_case::TABLE[at].1)
}

/// `WordDelimiterIterator`'s class for the code point, from the consumer's
/// own table. `SUBWORD_DELIM` is the default.
pub(crate) fn character_class(point: u32) -> u8 {
    lookup(character_class::TABLE, point, 8)
}
