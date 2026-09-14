//! The word delimiter filter, under Oak's flags.
//!
//! `docs/analysis/lucene-oak-analysis.md` §4, from
//! `analysis/miscellaneous/WordDelimiterFilter.java` and its
//! `WordDelimiterIterator`.
//!
//! Oak passes `GENERATE_WORD_PARTS | GENERATE_NUMBER_PARTS |
//! STEM_ENGLISH_POSSESSIVE`, plus `PRESERVE_ORIGINAL` under
//! `analyzers/@indexOriginalTerm`. It sets **no** catenate flag, so the
//! filter's concatenation buffers are dead on every path Oak reaches and
//! are not reproduced here; it sets neither split-on-case-change nor
//! split-on-numerics, so `powershot` and `j2se` each stay one part.
//!
//! Classification is per **UTF-16 code unit**, not per code point: the
//! iterator indexes a `char[]`, and a surrogate classifies as
//! `ALPHA|DIGIT` precisely so that an astral character never splits.

use super::unicode::character_class;
use super::{EndState, Staged, StagedToken, Token};

const LOWER: u8 = 0x01;
const UPPER: u8 = 0x02;
const DIGIT: u8 = 0x04;
const SUBWORD_DELIM: u8 = 0x08;
const ALPHA: u8 = 0x03;

/// The iterator's own definition of `ALPHA`, kept honest.
const _: () = assert!(ALPHA == LOWER | UPPER);

/// Where a subword ends when there is none left.
const DONE: usize = usize::MAX;

/// Splits every token, and under `preserve_original` emits the
/// undelimited original beside the parts.
pub(super) fn filter(stream: Staged, preserve_original: bool) -> Staged {
    let Staged { tokens, end } = stream;
    let mut produced = Vec::new();
    let mut accumulated: i64 = 0;
    for staged in tokens {
        split_token(
            &staged.token,
            staged.stop,
            preserve_original,
            &mut accumulated,
            &mut produced,
        );
    }
    Staged {
        tokens: produced,
        // A filter's own `end()` is its input's: the tokenizer is where
        // the stream stops, however many parts one of its tokens became.
        end,
    }
}

/// One input token, through the filter's `incrementToken` loop.
fn split_token(
    token: &Token,
    stop: EndState,
    preserve_original: bool,
    accumulated: &mut i64,
    produced: &mut Vec<StagedToken>,
) {
    let units: Vec<u16> = token.term.encode_utf16().collect();
    *accumulated += i64::from(token.position_increment);

    let mut iterator = Subwords::over(&units);
    iterator.next();

    // A word of no delimiters passes through whole.
    if iterator.current == 0 && iterator.end == units.len() {
        produced.push(StagedToken {
            token: Token {
                position_increment: increment_of(*accumulated),
                ..token.clone()
            },
            stop,
        });
        *accumulated = 0;
        return;
    }

    // A word of nothing but delimiters yields nothing — and gives back the
    // position it would have taken, when it took a plain one.
    if iterator.end == DONE && !preserve_original {
        if token.position_increment == 1 {
            *accumulated -= 1;
        }
        return;
    }

    // `saveState`: the offsets a part is measured against, and the
    // "illegal offsets" check for an input whose offsets do not span its
    // own term — a synonym, which this chain never produces.
    let saved_start = token.start_offset;
    let saved_end = token.end_offset;
    let has_illegal_offsets = (saved_end - saved_start) as usize != units.len();

    let mut has_output_token = false;
    let mut has_output_following_original = !preserve_original;

    if preserve_original {
        produced.push(StagedToken {
            token: Token {
                position_increment: increment_of(*accumulated),
                ..token.clone()
            },
            stop,
        });
        *accumulated = 0;
    }

    while iterator.end != DONE {
        let single_word = iterator.is_single_word();
        let start = saved_start + iterator.current as u32;
        let end = saved_start + iterator.end as u32;
        let (start_offset, end_offset) = if has_illegal_offsets {
            if single_word && start <= saved_end {
                (start, saved_end)
            } else {
                (saved_start, saved_end)
            }
        } else {
            (start, end)
        };
        let position_increment = position(
            accumulated,
            &mut has_output_token,
            &mut has_output_following_original,
        );
        produced.push(StagedToken {
            token: Token {
                term: String::from_utf16_lossy(&units[iterator.current..iterator.end]),
                position_increment,
                start_offset,
                end_offset,
                token_type: token.token_type.clone(),
            },
            stop,
        });
        iterator.next();
    }
}

/// `position(false)`: the first part of a token carries what the input
/// accumulated, the first part *after* a preserved original carries 0, and
/// every later part carries at least 1.
fn position(
    accumulated: &mut i64,
    has_output_token: &mut bool,
    has_output_following_original: &mut bool,
) -> u32 {
    let carried = *accumulated;
    if *has_output_token {
        *accumulated = 0;
        return increment_of(carried).max(1);
    }
    *has_output_token = true;
    if !*has_output_following_original {
        *has_output_following_original = true;
        return 0;
    }
    *accumulated = 0;
    increment_of(carried).max(1)
}

/// The accumulator is signed — the delimiters-only branch decrements it —
/// and Lucene's own attribute refuses a negative increment, so a run of
/// such tokens at the start of a stream can only reach zero.
fn increment_of(accumulated: i64) -> u32 {
    u32::try_from(accumulated.max(0)).unwrap_or(u32::MAX)
}

/// The type of one UTF-16 code unit.
fn class_of(unit: u16) -> u8 {
    character_class(u32::from(unit))
}

const fn is_alpha(class: u8) -> bool {
    class & ALPHA != 0
}

const fn is_digit(class: u8) -> bool {
    class & DIGIT != 0
}

const fn is_subword_delim(class: u8) -> bool {
    class & SUBWORD_DELIM != 0
}

const fn is_upper(class: u8) -> bool {
    class & UPPER != 0
}

/// `WordDelimiterIterator` under Oak's flags: neither split-on-case-change
/// nor split-on-numerics, possessive stemming on.
struct Subwords<'units> {
    text: &'units [u16],
    start_bounds: usize,
    end_bounds: usize,
    current: usize,
    end: usize,
    skip_possessive: bool,
    has_final_possessive: bool,
}

impl<'units> Subwords<'units> {
    /// `setText`, which runs `setBounds`: the leading and trailing
    /// delimiters are cut away, and a final possessive is noted but left
    /// in place.
    fn over(text: &'units [u16]) -> Self {
        let mut iterator = Self {
            text,
            start_bounds: 0,
            end_bounds: text.len(),
            current: 0,
            end: 0,
            skip_possessive: false,
            has_final_possessive: false,
        };
        while iterator.start_bounds < text.len()
            && is_subword_delim(class_of(text[iterator.start_bounds]))
        {
            iterator.start_bounds += 1;
        }
        while iterator.end_bounds > iterator.start_bounds
            && is_subword_delim(class_of(text[iterator.end_bounds - 1]))
        {
            iterator.end_bounds -= 1;
        }
        iterator.has_final_possessive = iterator.ends_with_possessive(iterator.end_bounds);
        iterator.current = iterator.start_bounds;
        iterator
    }

    /// Advances to the next subword.
    fn next(&mut self) {
        self.current = self.end;
        if self.current == DONE {
            return;
        }
        if self.skip_possessive {
            self.current += 2;
            self.skip_possessive = false;
        }
        let mut last_class = 0u8;
        while self.current < self.end_bounds && {
            last_class = class_of(self.text[self.current]);
            is_subword_delim(last_class)
        } {
            self.current += 1;
        }
        if self.current >= self.end_bounds {
            self.end = DONE;
            return;
        }
        self.end = self.current + 1;
        while self.end < self.end_bounds {
            let class = class_of(self.text[self.end]);
            if is_break(last_class, class) {
                break;
            }
            last_class = class;
            self.end += 1;
        }
        if self.end + 1 < self.end_bounds && self.ends_with_possessive(self.end + 2) {
            self.skip_possessive = true;
        }
    }

    /// Whether the current subword is the whole word, possessive aside.
    fn is_single_word(&self) -> bool {
        if self.has_final_possessive {
            self.current == self.start_bounds && self.end + 2 == self.end_bounds
        } else {
            self.current == self.start_bounds && self.end == self.end_bounds
        }
    }

    /// Whether `at` sits just past an English possessive: an apostrophe
    /// and an `s`, after a letter, at a subword boundary.
    fn ends_with_possessive(&self, at: usize) -> bool {
        at > 2
            && at <= self.text.len()
            && self.text[at - 2] == u16::from(b'\'')
            && (self.text[at - 1] == u16::from(b's') || self.text[at - 1] == u16::from(b'S'))
            && is_alpha(class_of(self.text[at - 3]))
            && (at == self.end_bounds
                || (at < self.text.len() && is_subword_delim(class_of(self.text[at]))))
    }
}

/// `isBreak`, with both split flags off: a class change breaks unless it
/// is letter to letter, upper to letter, or letter to digit either way.
const fn is_break(last_class: u8, class: u8) -> bool {
    if class & last_class != 0 {
        return false;
    }
    if is_alpha(last_class) && is_alpha(class) {
        // ALPHA -> ALPHA, with case changes ignored.
        return false;
    }
    if is_upper(last_class) && is_alpha(class) {
        return false;
    }
    if (is_alpha(last_class) && is_digit(class)) || (is_digit(last_class) && is_alpha(class)) {
        return false;
    }
    true
}
