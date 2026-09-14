//! The shingle filter, for `:spellcheck`.
//!
//! `docs/analysis/lucene-oak-analysis.md` §6.3, from
//! `analysis/shingle/ShingleFilter.java` under
//! `ShingleAnalyzerWrapper(ANALYZER, 3)`: unigrams beside the shingles, a
//! single space between the tokens of a shingle, `_` where the input left
//! a gap, and the token type `shingle` on anything longer than a unigram.
//!
//! The port keeps Lucene's own shape — a window of at most
//! `maximum_shingle_size` tokens and a circular gram size over it — because
//! the position increments and offsets fall out of that shape and out of
//! nothing simpler: the first output of each window carries 1 and every
//! later one 0, so a shingle shares the position of the unigram it starts
//! at.

use std::collections::VecDeque;

use super::{EndState, Staged, StagedToken, Token};

/// `ShingleFilter.DEFAULT_MIN_SHINGLE_SIZE`.
const MINIMUM_SHINGLE_SIZE: usize = 2;
/// `ShingleFilter.DEFAULT_FILLER_TOKEN`.
const FILLER_TOKEN: &str = "_";
/// `ShingleFilter.DEFAULT_TOKEN_SEPARATOR`.
const TOKEN_SEPARATOR: &str = " ";
/// `ShingleFilter.DEFAULT_TOKEN_TYPE`.
const SHINGLE_TYPE: &str = "shingle";

/// Shingles the stream at `maximum_shingle_size`, unigrams included.
pub(super) fn filter(stream: Staged, maximum_shingle_size: usize) -> Staged {
    let Staged { tokens, end } = stream;
    let mut shingler = Shingler::over(&tokens, end, maximum_shingle_size);
    let mut produced = Vec::new();
    while let Some(token) = shingler.next_token() {
        let stop = shingler.last_stop;
        produced.push(StagedToken { token, stop });
    }
    Staged {
        tokens: produced,
        // `end()` restores the state captured from the wrapped stream's
        // own `end()`, so the end state is the chain's below it.
        end,
    }
}

/// One token of the window, which is a real token or a filler.
#[derive(Clone)]
struct WindowToken {
    term: String,
    start_offset: u32,
    end_offset: u32,
    token_type: String,
    is_filler: bool,
}

/// The filter's own state.
struct Shingler<'stream> {
    input: &'stream [StagedToken],
    /// What the wrapped stream reports once it is drained.
    input_end: EndState,
    /// The next input token to read.
    at: usize,
    exhausted: bool,
    /// How many fillers still owe a place.
    fillers_to_insert: usize,
    /// The token a filler run displaced. `deliver_held` says whether it
    /// is a real token owed to the window — the run the input's own end
    /// state opens holds only a place for the fillers to collapse onto.
    held_token: Option<WindowToken>,
    deliver_held: bool,
    window: VecDeque<WindowToken>,
    maximum_shingle_size: usize,
    /// The circular sequence 1, 2, …, `maximum_shingle_size`.
    gram_size: usize,
    previous_gram_size: usize,
    /// Whether this window has already produced a token.
    is_output_here: bool,
    /// Where the wrapped stream would stop, had it been asked now — the
    /// stop of the last token pulled into the window, or the stream's own
    /// end once it is exhausted. Oak never caps this chain, so nothing
    /// reads it today; it is here because a stage that lied about it
    /// would be a defect waiting for the first caller that does.
    last_stop: EndState,
}

impl<'stream> Shingler<'stream> {
    fn over(
        input: &'stream [StagedToken],
        input_end: EndState,
        maximum_shingle_size: usize,
    ) -> Self {
        Self {
            input,
            input_end,
            at: 0,
            exhausted: false,
            fillers_to_insert: 0,
            held_token: None,
            deliver_held: false,
            window: VecDeque::new(),
            maximum_shingle_size,
            // `outputUnigrams` is true, so the sequence opens at 1.
            gram_size: 1,
            previous_gram_size: 1,
            is_output_here: false,
            last_stop: input_end,
        }
    }

    /// `incrementToken`.
    fn next_token(&mut self) -> Option<Token> {
        let mut gram = String::new();
        let mut built = 0usize;
        if self.gram_size == 1 || self.window.len() < self.gram_size {
            self.shift_window();
        } else {
            // The builder keeps what the previous call left in it: the
            // window has not moved, so those are the same tokens.
            built = self.previous_gram_size;
            for at in 0..built {
                if at > 0 {
                    gram.push_str(TOKEN_SEPARATOR);
                }
                gram.push_str(&self.window[at].term);
            }
        }
        if self.window.len() < self.gram_size {
            return None;
        }
        let mut is_all_filler = true;
        let mut last_end_offset = 0;
        let mut at = 0usize;
        while at < self.window.len() && built < self.gram_size {
            let gram_number = at + 1;
            let token = self.window[at].clone();
            last_end_offset = token.end_offset;
            if built < gram_number {
                if built > 0 {
                    gram.push_str(TOKEN_SEPARATOR);
                }
                gram.push_str(&token.term);
                built += 1;
            }
            if is_all_filler && token.is_filler {
                // A window that is nothing but fillers produces no token,
                // and the gram size moves on so the next call tries a
                // longer one.
                if gram_number == self.gram_size {
                    self.advance();
                }
            } else {
                is_all_filler = false;
            }
            at += 1;
        }
        if is_all_filler || built != self.gram_size {
            return None;
        }
        let head = self.window.front()?;
        let token = Token {
            term: gram,
            position_increment: u32::from(!self.is_output_here),
            start_offset: head.start_offset,
            end_offset: last_end_offset,
            token_type: if self.gram_size > 1 {
                SHINGLE_TYPE.to_owned()
            } else {
                head.token_type.clone()
            },
        };
        self.is_output_here = true;
        self.advance();
        Some(token)
    }

    /// `CircularSequence.advance`.
    fn advance(&mut self) {
        self.previous_gram_size = self.gram_size;
        if self.gram_size == 1 {
            self.gram_size = MINIMUM_SHINGLE_SIZE;
        } else if self.gram_size == self.maximum_shingle_size {
            self.reset_gram_size();
        } else {
            self.gram_size += 1;
        }
    }

    /// `CircularSequence.reset`, to the minimum, which is 1 because
    /// unigrams are output.
    fn reset_gram_size(&mut self) {
        self.gram_size = 1;
        self.previous_gram_size = 1;
    }

    /// `shiftInputWindow`: drop the head, refill to the maximum, and open
    /// a fresh gram sequence over what is left.
    fn shift_window(&mut self) {
        self.window.pop_front();
        while self.window.len() < self.maximum_shingle_size {
            match self.next_window_token() {
                Some(token) => self.window.push_back(token),
                None => break,
            }
        }
        self.reset_gram_size();
        self.is_output_here = false;
    }

    /// `getNextToken`: the input, with a filler inserted wherever a
    /// position increment left a gap. A filler occupies no space, so its
    /// offsets collapse to where the gap opened.
    fn next_window_token(&mut self) -> Option<WindowToken> {
        if self.fillers_to_insert > 0 {
            self.fillers_to_insert -= 1;
            let mut filler = self.held_token.clone()?;
            filler.end_offset = filler.start_offset;
            FILLER_TOKEN.clone_into(&mut filler.term);
            filler.is_filler = true;
            return Some(filler);
        }
        if self.deliver_held {
            self.deliver_held = false;
            return self.held_token.take();
        }
        if self.exhausted {
            return None;
        }
        let Some(staged) = self.input.get(self.at) else {
            // The input is spent: its own end state can still owe
            // positions, and those become trailing fillers at the end
            // offset.
            self.exhausted = true;
            self.last_stop = self.input_end;
            self.fillers_to_insert =
                (self.input_end.position_increment as usize).min(self.maximum_shingle_size - 1);
            if self.fillers_to_insert == 0 {
                return None;
            }
            self.deliver_held = false;
            self.held_token = Some(WindowToken {
                term: String::new(),
                start_offset: self.input_end.offset,
                end_offset: self.input_end.offset,
                token_type: String::new(),
                is_filler: false,
            });
            return self.next_window_token();
        };
        self.at += 1;
        self.last_stop = staged.stop;
        let token = &staged.token;
        let window_token = WindowToken {
            term: token.term.clone(),
            start_offset: token.start_offset,
            end_offset: token.end_offset,
            token_type: token.token_type.clone(),
            is_filler: false,
        };
        if token.position_increment > 1 {
            // Every shingle must hold one real token, so no more than
            // `maximum_shingle_size - 1` fillers are ever inserted.
            self.fillers_to_insert =
                ((token.position_increment - 1) as usize).min(self.maximum_shingle_size - 1) - 1;
            self.held_token = Some(window_token.clone());
            self.deliver_held = true;
            return Some(WindowToken {
                term: FILLER_TOKEN.to_owned(),
                end_offset: window_token.start_offset,
                is_filler: true,
                ..window_token
            });
        }
        Some(window_token)
    }
}
