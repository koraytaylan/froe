//! Lucene's numeric encodings, for the fields Oak writes as numbers.
//!
//! `docs/analysis/lucene-oak-analysis.md` §8, from
//! `org/apache/lucene/util/NumericUtils.java` and
//! `analysis/NumericTokenStream.java` as `oak-lucene` vendors them, and
//! `plugins/index/lucene/FieldFactory.java` for the `DATE` conversion.
//!
//! A numeric field is not one term but a **trie**: one term per shift
//! level at the field type's precision step, all at the same position,
//! which is what makes a range query cheap. Oak builds its numeric fields
//! with the value alone, so all three take Lucene's default step of 4 —
//! sixteen terms for a long or a double, eight for an integer.

use crate::index::IndexError;
use crate::java::parse_epoch_milliseconds;

/// `NumericUtils.PRECISION_STEP_DEFAULT`, which `LongField`, `IntField`
/// and `DoubleField` all document as their own.
pub const PRECISION_STEP: u32 = 4;

/// `NumericUtils.SHIFT_START_LONG`.
const SHIFT_START_LONG: u8 = 0x20;

/// `NumericUtils.SHIFT_START_INT`.
const SHIFT_START_INT: u8 = 0x60;

/// One term of a numeric field, with the position increment
/// `NumericTokenStream` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumericTerm {
    /// The term's bytes: the shift byte, then the value in seven-bit
    /// groups, most significant first.
    pub bytes: Vec<u8>,
    /// 1 on the first term and 0 on every other, because all of them
    /// occupy one position.
    pub position_increment: u32,
}

/// Every term of a `LongField`, in the order the token stream emits them.
#[must_use]
pub fn long_terms(value: i64) -> Vec<NumericTerm> {
    terms(63, SHIFT_START_LONG, (value ^ i64::MIN) as u64)
}

/// Every term of an `IntField`, which `:depth` is the one of.
#[must_use]
pub fn integer_terms(value: i32) -> Vec<NumericTerm> {
    // The sortable bits of an integer are 32 wide, and Lucene shifts them
    // unsigned; widening the signed value instead would carry its sign
    // into the top group.
    terms(31, SHIFT_START_INT, u64::from((value ^ i32::MIN) as u32))
}

/// Every term of a `DoubleField`, which is a long field over the
/// **sortable** long — not the raw bits the doc value carries.
#[must_use]
pub fn double_terms(value: f64) -> Vec<NumericTerm> {
    long_terms(double_to_sortable_long(value))
}

/// `NumericUtils.doubleToSortableLong`: the raw bits, with every bit below
/// the sign flipped for a negative double so that the byte order is the
/// numeric order.
#[must_use]
pub fn double_to_sortable_long(value: f64) -> i64 {
    let bits = value.to_bits() as i64;
    if bits < 0 { bits ^ i64::MAX } else { bits }
}

/// `FieldFactory.dateToLong`: the epoch millisecond of a date, through
/// Jackrabbit's own `ISO8601` — §8.4, and not the ISO 8601 a reasonable
/// reader would write.
///
/// # Errors
///
/// [`IndexError::UnparseableDate`] for a value that parser refuses. Oak
/// throws an unchecked exception there and its fulltext editor does not
/// catch it, so the indexing commit fails rather than the document being
/// skipped; a *missing* value is the caller's absence and not an error.
pub fn date_to_long(value: &str) -> Result<i64, IndexError> {
    parse_epoch_milliseconds(value).ok_or_else(|| IndexError::UnparseableDate {
        value: value.to_owned(),
    })
}

/// The shift levels of one value, already sign-flipped into its sortable
/// form.
fn terms(highest_bit: u32, shift_start: u8, sortable: u64) -> Vec<NumericTerm> {
    let mut produced = Vec::new();
    let mut shift = 0u32;
    while shift <= highest_bit {
        produced.push(NumericTerm {
            bytes: prefix_coded(highest_bit, shift_start, sortable, shift),
            position_increment: u32::from(produced.is_empty()),
        });
        shift += PRECISION_STEP;
    }
    produced
}

/// `NumericUtils.longToPrefixCodedBytes`, which the integer form differs
/// from only in its highest bit and its shift-start marker.
fn prefix_coded(highest_bit: u32, shift_start: u8, sortable: u64, shift: u32) -> Vec<u8> {
    let group_count = ((highest_bit - shift) / 7 + 1) as usize;
    let mut bytes = vec![0u8; group_count + 1];
    bytes[0] = shift_start + shift as u8;
    // The groups are filled from the end, so the last byte holds the
    // lowest seven bits and the first holds whatever is left at the top.
    let mut remaining = sortable >> shift;
    for at in (1..=group_count).rev() {
        bytes[at] = (remaining & 0x7f) as u8;
        remaining >>= 7;
    }
    bytes
}
