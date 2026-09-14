//! The standard tokenizer, over the Unicode 6.3.0 tables.
//!
//! `docs/analysis/lucene-oak-analysis.md` §2, from
//! `analysis/standard/StandardTokenizer.java` and the `JFlex` grammar
//! `StandardTokenizerImpl.jflex` its 4.7 scanner is generated from.
//!
//! The grammar is a set of rules over character classes, matched
//! **longest-first with ties broken by rule order** — which is `JFlex`'s own
//! rule. Each rule below is that grammar's production, written out; the
//! quantifiers over `ExtendNumLet` and `(Format|Extend)` are greedy
//! because no rule that follows one of them can start with what it
//! consumes, the `Word_Break` classes being disjoint.

use super::unicode::{
    Script, WordBreak, is_complex_context, is_decimal_digit, is_half_and_full_forms, script,
    word_break,
};
use super::{EndState, Staged, StagedToken, Token, code_point_at, units_to_string};

/// `StandardAnalyzer.DEFAULT_MAX_TOKEN_LENGTH`. A token above it is
/// **dropped**, and its position is carried by the next one.
const MAXIMUM_TOKEN_LENGTH: usize = 255;

const ALPHANUM: &str = "<ALPHANUM>";
const NUM: &str = "<NUM>";
const SOUTHEAST_ASIAN: &str = "<SOUTHEAST_ASIAN>";
const IDEOGRAPHIC: &str = "<IDEOGRAPHIC>";
const HIRAGANA: &str = "<HIRAGANA>";
const KATAKANA: &str = "<KATAKANA>";
const HANGUL: &str = "<HANGUL>";

/// Tokenizes, in UTF-16 code units.
pub(super) fn tokenize(units: &[u16]) -> Staged {
    let mut tokens = Vec::new();
    let mut at = 0usize;
    let mut skipped = 0u32;
    while at < units.len() {
        let Some((end, token_type)) = longest_match(units, at) else {
            // The grammar's last rule: a regional-indicator pair or any one
            // character, consumed and not emitted.
            at += fallback_length(units, at);
            continue;
        };
        let length = end - at;
        if length <= MAXIMUM_TOKEN_LENGTH {
            tokens.push(StagedToken {
                token: Token {
                    term: units_to_string(&units[at..end]),
                    position_increment: skipped + 1,
                    start_offset: at as u32,
                    end_offset: end as u32,
                    token_type: token_type.to_owned(),
                },
                // `incrementToken` zeroes `skippedPositions` on entry, so
                // a stream stopped just after this token reports what was
                // skipped *before* it — already counted in its own
                // increment — and the end of this token.
                stop: EndState {
                    position_increment: skipped,
                    offset: end as u32,
                },
            });
            skipped = 0;
        } else {
            skipped += 1;
        }
        at = end;
    }
    Staged {
        tokens,
        // `end()` clears the attributes — which zeroes the increment — and
        // then adds what was skipped; the offset is how far the scanner
        // read, which is the whole input.
        end: EndState {
            position_increment: skipped,
            offset: units.len() as u32,
        },
    }
}

/// The rules, in the grammar's own order. The longest match wins; on a tie
/// the earlier rule does.
fn longest_match(units: &[u16], at: usize) -> Option<(usize, &'static str)> {
    let candidates = [
        (automaton::number(units, at), NUM),
        (run_of(units, at, is_hangul), HANGUL),
        (run_of(units, at, is_katakana), KATAKANA),
        (automaton::word(units, at), ALPHANUM),
        (complex_context_run(units, at), SOUTHEAST_ASIAN),
        (single(units, at, is_han), IDEOGRAPHIC),
        (single(units, at, is_hiragana), HIRAGANA),
    ];
    let mut best: Option<(usize, &'static str)> = None;
    for (end, token_type) in candidates {
        let Some(end) = end else { continue };
        if end <= at {
            continue;
        }
        if best.is_none_or(|(chosen, _)| end > chosen) {
            best = Some((end, token_type));
        }
    }
    best
}

/// How much the fall-through rule consumes: a run of two or more regional
/// indicators, or one character.
fn fallback_length(units: &[u16], at: usize) -> usize {
    if let Some(first) = atom(units, at).filter(|(point, _)| is_regional_indicator(*point))
        && let Some(second) =
            atom(units, first.1).filter(|(point, _)| is_regional_indicator(*point))
    {
        let mut end = second.1;
        while let Some((point, next)) = atom(units, end) {
            if !is_regional_indicator(point) {
                break;
            }
            end = next;
        }
        return end - at;
    }
    let (_, width) = code_point_at(units, at);
    width
}

// ---------------------------------------------------------------- atoms

/// One `X (Format | Extend)*` of the grammar: a base code point and where
/// its trailing marks end.
///
/// Greedy, and safely so: no element of any rule begins with a `Format` or
/// an `Extend`, the `Word_Break` classes being disjoint, so a shorter tail
/// can never let a longer match through.
fn atom(units: &[u16], at: usize) -> Option<(u32, usize)> {
    if at >= units.len() {
        return None;
    }
    let (point, width) = code_point_at(units, at);
    let mut end = at + width;
    while end < units.len() {
        let (next, next_width) = code_point_at(units, end);
        if matches!(word_break(next), WordBreak::Format | WordBreak::Extend) {
            end += next_width;
        } else {
            break;
        }
    }
    Some((point, end))
}

/// `{X}+` over atoms.
fn run_of(units: &[u16], at: usize, accepts: impl Fn(u32) -> bool) -> Option<usize> {
    let mut end = at;
    while let Some((point, next)) = atom(units, end) {
        if !accepts(point) {
            break;
        }
        end = next;
    }
    (end > at).then_some(end)
}

/// One atom, for the rules that emit a single character.
fn single(units: &[u16], at: usize, accepts: impl Fn(u32) -> bool) -> Option<usize> {
    atom(units, at)
        .filter(|(point, _)| accepts(*point))
        .map(|(_, end)| end)
}

/// `{ComplexContext}+`, which carries no `(Format | Extend)*` tail in the
/// grammar and so is a plain run of code points.
fn complex_context_run(units: &[u16], at: usize) -> Option<usize> {
    let mut end = at;
    while end < units.len() {
        let (point, width) = code_point_at(units, end);
        if !is_complex_context(point) {
            break;
        }
        end += width;
    }
    (end > at).then_some(end)
}

// -------------------------------------------------------------- classes

/// `Numeric = [\p{WB:Numeric}[\p{Blk:HalfAndFullForms}&&\p{Nd}]]`.
fn is_numeric(point: u32) -> bool {
    word_break(point) == WordBreak::Numeric
        || (is_half_and_full_forms(point) && is_decimal_digit(point))
}

fn is_extend_num_let(point: u32) -> bool {
    word_break(point) == WordBreak::ExtendNumLet
}

fn is_katakana(point: u32) -> bool {
    word_break(point) == WordBreak::Katakana
}

fn is_hebrew_letter(point: u32) -> bool {
    word_break(point) == WordBreak::HebrewLetter
}

/// `HebrewOrALetter`, which the word rule's letter run is built on.
fn is_letter(point: u32) -> bool {
    matches!(
        word_break(point),
        WordBreak::ALetter | WordBreak::HebrewLetter
    )
}

/// `MidNumericEx`'s base: `MidNum | MidNumLet | Single_Quote`.
fn is_mid_numeric(point: u32) -> bool {
    matches!(
        word_break(point),
        WordBreak::MidNum | WordBreak::MidNumLet | WordBreak::SingleQuote
    )
}

/// `MidLetterEx`'s base: `MidLetter | MidNumLet | Single_Quote`.
fn is_mid_letter(point: u32) -> bool {
    matches!(
        word_break(point),
        WordBreak::MidLetter | WordBreak::MidNumLet | WordBreak::SingleQuote
    )
}

fn is_single_quote(point: u32) -> bool {
    word_break(point) == WordBreak::SingleQuote
}

fn is_double_quote(point: u32) -> bool {
    word_break(point) == WordBreak::DoubleQuote
}

fn is_regional_indicator(point: u32) -> bool {
    word_break(point) == WordBreak::RegionalIndicator
}

/// `HangulEx`'s base: Hangul that is also a letter.
fn is_hangul(point: u32) -> bool {
    script(point) == Script::Hangul && is_letter(point)
}

fn is_han(point: u32) -> bool {
    script(point) == Script::Han
}

fn is_hiragana(point: u32) -> bool {
    script(point) == Script::Hiragana
}

/// The `<NUM>` and `<ALPHANUM>` rules, as the automaton `JFlex` builds.
///
/// Neither can be matched by taking each alternative as far as it goes.
/// `אב'` is the proof: the letter run must stop after `א` so that the
/// Hebrew-quote alternative — `WB7a` — can take `ב'` and reach one further.
/// `JFlex` resolves that by simulating every alternative at once and keeping
/// the last position an accepting state was live, which is what this does,
/// in one left-to-right pass over the atoms.
mod automaton {
    use super::{
        atom, is_double_quote, is_extend_num_let, is_hebrew_letter, is_katakana, is_letter,
        is_mid_letter, is_mid_numeric, is_numeric, is_single_quote,
    };

    /// A position in the two rules' regular expressions.
    #[derive(Clone, Copy)]
    enum State {
        /// `{ExtendNumLetEx}*` before the word rule's first group.
        LeadingJoiner,
        /// The start of one group: a Katakana run or the alternation.
        GroupStart,
        /// After a `{KatakanaEx}`.
        KatakanaRun,
        /// Inside the Katakana run's `{ExtendNumLetEx}*`.
        KatakanaJoiner,
        /// The start of one alternative of the `( … )+`.
        AlternativeStart,
        /// After `{HebrewLetterEx}`, owing a quote.
        HebrewOpen,
        /// After `{HebrewLetterEx} {DoubleQuoteEx}`, owing a letter.
        HebrewQuoted,
        /// One alternative matched whole.
        AlternativeDone,
        /// After a `{NumericEx}` of the word rule's number run.
        NumberRun,
        /// Inside that run's `{ExtendNumLetEx}*`.
        NumberJoiner,
        /// After its `{MidNumericEx}`, owing a numeric.
        NumberMid,
        /// After a `{HebrewOrALetterEx}`.
        LetterRun,
        /// Inside the letter run's `{ExtendNumLetEx}*`.
        LetterJoiner,
        /// After its `{MidLetterEx}`, owing a letter.
        LetterMid,
        /// A whole group matched.
        GroupEnd,
        /// The `{ExtendNumLetEx}` run after a group, which is both the
        /// rule's trailing star and the outer loop's `{ExtendNumLetEx}+`.
        JoinerAfterGroup,
        /// `{ExtendNumLetEx}*` before the `<NUM>` rule's first numeric.
        NumberLeadingJoiner,
        /// Owing that first numeric.
        NumberOpen,
        /// After a `{NumericEx}` of the `<NUM>` rule.
        NumberOnly,
        /// Its `{ExtendNumLetEx}` run, which is both the inner joiner and
        /// the trailing star.
        NumberOnlyJoiner,
        /// After its `{MidNumericEx}`, owing a numeric.
        NumberOnlyMid,
    }

    const fn bit(state: State) -> u32 {
        1 << state as u32
    }

    /// Where a rule may stop.
    const ACCEPTING: u32 = bit(State::GroupEnd)
        | bit(State::JoinerAfterGroup)
        | bit(State::NumberOnly)
        | bit(State::NumberOnlyJoiner);

    /// The ε-transitions, to a fixed point.
    fn closure(set: u32) -> u32 {
        let mut closed = set;
        loop {
            let mut next = closed;
            if closed & bit(State::LeadingJoiner) != 0 {
                next |= bit(State::GroupStart);
            }
            if closed & bit(State::GroupStart) != 0 {
                next |= bit(State::AlternativeStart);
            }
            if closed & bit(State::JoinerAfterGroup) != 0 {
                next |= bit(State::GroupStart);
            }
            if closed
                & (bit(State::AlternativeDone) | bit(State::NumberRun) | bit(State::LetterRun))
                != 0
            {
                next |= bit(State::AlternativeStart) | bit(State::GroupEnd);
            }
            if closed & bit(State::KatakanaRun) != 0 {
                next |= bit(State::GroupEnd);
            }
            if closed & bit(State::NumberLeadingJoiner) != 0 {
                next |= bit(State::NumberOpen);
            }
            if next == closed {
                return closed;
            }
            closed = next;
        }
    }

    /// One atom's transitions, over the whole live set.
    fn step(set: u32, point: u32) -> u32 {
        let joiner = is_extend_num_let(point);
        let mut next = step_katakana(set, point, joiner)
            | step_hebrew_quote(set, point)
            | step_number_run(set, point, joiner)
            | step_letter_run(set, point, joiner)
            | step_number_rule(set, point);
        if set & bit(State::LeadingJoiner) != 0 && joiner {
            next |= bit(State::LeadingJoiner);
        }
        if set & bit(State::AlternativeStart) != 0 {
            if is_hebrew_letter(point) {
                next |= bit(State::HebrewOpen);
            }
            if is_numeric(point) {
                next |= bit(State::NumberRun);
            }
            if is_letter(point) {
                next |= bit(State::LetterRun);
            }
        }
        if set & (bit(State::GroupEnd) | bit(State::JoinerAfterGroup)) != 0 && joiner {
            next |= bit(State::JoinerAfterGroup);
        }
        next
    }

    /// `{KatakanaEx} ( {ExtendNumLetEx}* {KatakanaEx} )*`.
    fn step_katakana(set: u32, point: u32, joiner: bool) -> u32 {
        let katakana = is_katakana(point);
        let mut next = 0;
        if set & bit(State::GroupStart) != 0 && katakana {
            next |= bit(State::KatakanaRun);
        }
        if set & (bit(State::KatakanaRun) | bit(State::KatakanaJoiner)) != 0 && katakana {
            next |= bit(State::KatakanaRun);
        }
        if set & bit(State::KatakanaRun) != 0 && joiner {
            next |= bit(State::KatakanaJoiner);
        }
        if set & bit(State::KatakanaJoiner) != 0 && joiner {
            next |= bit(State::KatakanaJoiner);
        }
        next
    }

    /// `{HebrewLetterEx} ( {SingleQuoteEx} | {DoubleQuoteEx} {HebrewLetterEx} )`.
    fn step_hebrew_quote(set: u32, point: u32) -> u32 {
        let mut next = 0;
        if set & bit(State::HebrewOpen) != 0 {
            if is_single_quote(point) {
                next |= bit(State::AlternativeDone);
            }
            if is_double_quote(point) {
                next |= bit(State::HebrewQuoted);
            }
        }
        if set & bit(State::HebrewQuoted) != 0 && is_hebrew_letter(point) {
            next |= bit(State::AlternativeDone);
        }
        next
    }

    /// `{NumericEx} ( ( {ExtendNumLetEx}* | {MidNumericEx} ) {NumericEx} )*`,
    /// the word rule's alternative.
    fn step_number_run(set: u32, point: u32, joiner: bool) -> u32 {
        let numeric = is_numeric(point);
        let mut next = 0;
        if set & (bit(State::NumberRun) | bit(State::NumberJoiner) | bit(State::NumberMid)) != 0
            && numeric
        {
            next |= bit(State::NumberRun);
        }
        if set & bit(State::NumberRun) != 0 {
            if joiner {
                next |= bit(State::NumberJoiner);
            }
            if is_mid_numeric(point) {
                next |= bit(State::NumberMid);
            }
        }
        if set & bit(State::NumberJoiner) != 0 && joiner {
            next |= bit(State::NumberJoiner);
        }
        next
    }

    /// `{HebrewOrALetterEx} ( ( {ExtendNumLetEx}* | {MidLetterEx} ) {HebrewOrALetterEx} )*`.
    fn step_letter_run(set: u32, point: u32, joiner: bool) -> u32 {
        let letter = is_letter(point);
        let mut next = 0;
        if set & (bit(State::LetterRun) | bit(State::LetterJoiner) | bit(State::LetterMid)) != 0
            && letter
        {
            next |= bit(State::LetterRun);
        }
        if set & bit(State::LetterRun) != 0 {
            if joiner {
                next |= bit(State::LetterJoiner);
            }
            if is_mid_letter(point) {
                next |= bit(State::LetterMid);
            }
        }
        if set & bit(State::LetterJoiner) != 0 && joiner {
            next |= bit(State::LetterJoiner);
        }
        next
    }

    /// The `<NUM>` rule's own states, which share no transition with the
    /// word rule's.
    fn step_number_rule(set: u32, point: u32) -> u32 {
        let joiner = is_extend_num_let(point);
        let mut next = 0;
        if set & bit(State::NumberLeadingJoiner) != 0 && joiner {
            next |= bit(State::NumberLeadingJoiner);
        }
        if set & bit(State::NumberOpen) != 0 && is_numeric(point) {
            next |= bit(State::NumberOnly);
        }
        if set & bit(State::NumberOnly) != 0 {
            if is_numeric(point) {
                next |= bit(State::NumberOnly);
            }
            if joiner {
                next |= bit(State::NumberOnlyJoiner);
            }
            if is_mid_numeric(point) {
                next |= bit(State::NumberOnlyMid);
            }
        }
        if set & bit(State::NumberOnlyJoiner) != 0 {
            if joiner {
                next |= bit(State::NumberOnlyJoiner);
            }
            if is_numeric(point) {
                next |= bit(State::NumberOnly);
            }
        }
        if set & bit(State::NumberOnlyMid) != 0 && is_numeric(point) {
            next |= bit(State::NumberOnly);
        }
        next
    }

    /// The last position an accepting state was live, over one pass.
    fn simulate(units: &[u16], at: usize, start: State) -> Option<usize> {
        let mut set = closure(bit(start));
        let mut last_accepting = None;
        let mut position = at;
        loop {
            if set & ACCEPTING != 0 {
                last_accepting = Some(position);
            }
            let Some((point, end)) = atom(units, position) else {
                break;
            };
            let next = closure(step(set, point));
            if next == 0 {
                break;
            }
            set = next;
            position = end;
        }
        last_accepting.filter(|end| *end > at)
    }

    /// The `<ALPHANUM>` rule.
    pub(super) fn word(units: &[u16], at: usize) -> Option<usize> {
        simulate(units, at, State::LeadingJoiner)
    }

    /// The `<NUM>` rule.
    pub(super) fn number(units: &[u16], at: usize) -> Option<usize> {
        simulate(units, at, State::NumberLeadingJoiner)
    }
}
