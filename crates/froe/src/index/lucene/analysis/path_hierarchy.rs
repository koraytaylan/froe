//! The path-hierarchy tokenizer, for `:ancestors`.
//!
//! `docs/analysis/lucene-oak-analysis.md` §6.1, from
//! `analysis/path/PathHierarchyTokenizer.java` built by its factory with
//! an empty parameter map, so the delimiter is `/`, the replacement is `/`
//! and the skip is 0.
//!
//! One token per ancestor prefix, each starting at offset 0; the first
//! carries a position increment of 1 and every later one 0, so they all
//! sit at position 0. The field's value is the node's **parent** path, so
//! `/content/interop/page` reaches this chain as `/content/interop` and
//! yields `/content` and `/content/interop`.

use super::{EndState, Staged, StagedToken, Token, units_to_string};

/// `PathHierarchyTokenizerFactory`'s default delimiter, which is also its
/// default replacement.
const DELIMITER: u16 = b'/' as u16;

/// The type every `Tokenizer` leaves in place: `TypeAttribute`'s default.
const DEFAULT_TYPE: &str = "word";

/// Tokenizes, in UTF-16 code units.
pub(super) fn tokenize(units: &[u16]) -> Staged {
    let mut tokens: Vec<StagedToken> = Vec::new();
    // `charsRead` — what `end()` reports, and what a stop after any one
    // token reports.
    let mut read = 0usize;
    // The token the previous call left in `resultToken`, which the next
    // one is built on top of.
    let mut carried = 0usize;
    let mut end_delimiter = false;
    while read < units.len() || end_delimiter {
        let mut length = if end_delimiter {
            end_delimiter = false;
            // The delimiter that ended the previous token opens this one.
            1
        } else {
            0
        };
        let mut added = length > 0;
        while read < units.len() {
            let unit = units[read];
            read += 1;
            if !added {
                added = true;
                length += 1;
            } else if unit == DELIMITER {
                end_delimiter = true;
                break;
            } else {
                length += 1;
            }
        }
        if !added {
            break;
        }
        length += carried;
        carried = length;
        tokens.push(StagedToken {
            token: Token {
                term: units_to_string(&units[..length]),
                position_increment: u32::from(tokens.is_empty()),
                start_offset: 0,
                end_offset: length as u32,
                token_type: DEFAULT_TYPE.to_owned(),
            },
            stop: EndState {
                position_increment: 0,
                offset: read as u32,
            },
        });
    }
    Staged {
        tokens,
        end: EndState {
            position_increment: 0,
            offset: units.len() as u32,
        },
    }
}
