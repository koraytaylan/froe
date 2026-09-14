//! Oak's suggest tokenizer.
//!
//! `docs/analysis/lucene-oak-analysis.md` §6.2, from
//! `plugins/index/lucene/util/CRTokenizer.java` over
//! `analysis/util/CharTokenizer.java`: a character tokenizer whose only
//! delimiter is the newline.
//!
//! The maximum word length is checked **after** a code point is appended,
//! so a token whose 255th and 256th units are a surrogate pair is cut at
//! 256 and a value longer than that yields several terms.

use super::{EndState, Staged, StagedToken, Token, code_point_at, units_to_string};

/// `CharTokenizer.MAX_WORD_LEN`.
const MAXIMUM_WORD_LENGTH: usize = 255;

/// `CRTokenizer.isTokenChar`.
const NEWLINE: u32 = '\n' as u32;

/// The type a `Tokenizer` leaves in place.
const DEFAULT_TYPE: &str = "word";

/// Tokenizes, in UTF-16 code units.
pub(super) fn tokenize(units: &[u16]) -> Staged {
    let mut tokens = Vec::new();
    let mut at = 0usize;
    while at < units.len() {
        let mut length = 0usize;
        while at < units.len() {
            let (point, width) = code_point_at(units, at);
            if point == NEWLINE {
                if length > 0 {
                    break;
                }
                at += width;
                continue;
            }
            at += width;
            length += width;
            if length >= MAXIMUM_WORD_LENGTH {
                break;
            }
        }
        if length == 0 {
            break;
        }
        let end = at;
        tokens.push(StagedToken {
            token: Token {
                term: units_to_string(&units[end - length..end]),
                position_increment: 1,
                start_offset: (end - length) as u32,
                end_offset: end as u32,
                token_type: DEFAULT_TYPE.to_owned(),
            },
            // `end()` reports the last token's end offset, and
            // `clearAttributes` has zeroed the increment.
            stop: EndState {
                position_increment: 0,
                offset: end as u32,
            },
        });
    }
    Staged {
        tokens,
        end: EndState {
            position_increment: 0,
            // The call that returns false sets the final offset to
            // everything the reader consumed, which is the whole value —
            // not the end of the last token, which a trailing newline
            // would leave behind.
            offset: units.len() as u32,
        },
    }
}
