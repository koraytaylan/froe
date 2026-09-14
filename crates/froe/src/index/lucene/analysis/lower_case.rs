//! The lower-case filter.
//!
//! `docs/analysis/lucene-oak-analysis.md` §3, from
//! `analysis/core/LowerCaseFilter.java` over
//! `analysis/util/CharacterUtils.java`.
//!
//! The filter rewrites each term **per code point** through
//! `Character.toLowerCase(int)` — the running JVM's own table, which
//! [`super::unicode::to_lower_case`] carries as a fixture enumerated from
//! the pinned image. It changes no offset and no position, and it maps one
//! code point to one: the lower case of `İ` is `i` here, not `i` plus a
//! combining dot.

use super::Staged;
use super::unicode::to_lower_case;

/// Lower-cases every term in place.
pub(super) fn filter(mut stream: Staged) -> Staged {
    for staged in &mut stream.tokens {
        staged.token.term = lower_case(&staged.token.term);
    }
    stream
}

/// One term, code point by code point.
fn lower_case(term: &str) -> String {
    term.chars()
        .map(|character| {
            let lowered = to_lower_case(character as u32);
            char::from_u32(lowered).unwrap_or(character)
        })
        .collect()
}
