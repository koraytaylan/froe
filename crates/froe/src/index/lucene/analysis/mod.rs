//! Oak's analysis chain: what a string becomes on its way to terms.
//!
//! `docs/analysis/lucene-oak-analysis.md`, from the `lucene-core` Oak
//! vendors, the published `lucene-analyzers-common` artifact pinned there,
//! and Oak's own `OakAnalyzer`, `LuceneIndexDefinition.createAnalyzer` and
//! `IndexWriterUtils.getIndexWriterConfig`.
//!
//! # What a caller gets
//!
//! [`Analyzer::tokens`] returns the tokens **and the end state** — the
//! position increment and offset Lucene reads from a stream once its
//! tokens are consumed. Plan 0009's writer needs both: a field of several
//! values composes them by adding each value's end state and then the
//! gaps.
//!
//! # The five chains
//!
//! One per [`Chain`], and [`Analyzer::chain_of`] routes a field to one
//! exactly as Oak's two wrappers do: the definition's own
//! `PerFieldAnalyzerWrapper` sends `:ancestors` to the path-hierarchy
//! chain, and the writer configuration's second wrapper replaces the
//! analyzer whole for `:spellcheck` and `:suggest` — which is why those
//! two are never reached by the definition's token-count cap.

pub(crate) mod unicode;

mod lower_case;
mod path_hierarchy;
mod shingle;
mod standard_tokenizer;
mod suggest_tokenizer;
mod token_limit;
mod word_delimiter;

pub mod numeric;

/// One token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The term's text.
    pub term: String,
    /// How far the position advances before this token.
    pub position_increment: u32,
    /// Where it starts, in **UTF-16 code units** — Lucene's offsets are
    /// char offsets, so an astral character counts two.
    pub start_offset: u32,
    /// Where it ends, in UTF-16 code units.
    pub end_offset: u32,
    /// The type attribute, such as `<ALPHANUM>` or `shingle`.
    pub token_type: String,
}

/// What a stream reports once its tokens are consumed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EndState {
    /// The position increment `end()` reports.
    pub position_increment: u32,
    /// The offset it reports, both start and end.
    pub offset: u32,
}

/// A token as the stages exchange it: the token, and the end state the
/// **source** would report if the pipeline stopped just after it.
///
/// The second half exists for the token-count cap alone. The cap is built
/// with `consumeAllTokens` false, so on reaching its limit it stops
/// pulling without draining what is below it, and `end()` then reports
/// where the tokenizer happened to be — the end of the last token it
/// scanned, not the end of the value. §7 of the specification.
pub(crate) struct StagedToken {
    /// The token itself.
    pub token: Token,
    /// Where the source would stop.
    pub stop: EndState,
}

/// A stream between two stages.
#[derive(Default)]
pub(crate) struct Staged {
    /// The tokens, in order.
    pub tokens: Vec<StagedToken>,
    /// What the source reports once they are all consumed.
    pub end: EndState,
}

impl From<Staged> for TokenStreamResult {
    fn from(staged: Staged) -> Self {
        Self {
            tokens: staged
                .tokens
                .into_iter()
                .map(|staged| staged.token)
                .collect(),
            final_position_increment: staged.end.position_increment,
            final_offset: staged.end.offset,
        }
    }
}

/// A whole token stream, tokens and end state together.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TokenStreamResult {
    /// The tokens, in order.
    pub tokens: Vec<Token>,
    /// What the stream's `end()` reports as a position increment.
    pub final_position_increment: u32,
    /// What it reports as an offset.
    pub final_offset: u32,
}

/// The fields the writer configuration gives an analyzer of their own,
/// and the one the definition routes when path restrictions are on.
/// `plugins/index/lucene/FieldNames.java`.
pub mod field_names {
    /// `FieldNames.ANCESTORS`.
    pub const ANCESTORS: &str = ":ancestors";
    /// `FieldNames.SPELLCHECK`.
    pub const SPELLCHECK: &str = ":spellcheck";
    /// `FieldNames.SUGGEST`.
    pub const SUGGEST: &str = ":suggest";
}

/// Which chain a field goes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chain {
    /// Oak's own analyzer: the standard tokenizer, the lower-case filter
    /// and the word delimiter filter.
    Default,
    /// The same with `analyzers/@indexOriginalTerm`, which adds the
    /// undelimited original beside the generated parts.
    OriginalTerm,
    /// The path-hierarchy chain `:ancestors` takes when
    /// `evaluatePathRestrictions` is on.
    Ancestors,
    /// What the writer configuration installs for `:spellcheck`: Oak's own
    /// analyzer under a shingle filter at a maximum size of 3.
    Spellcheck,
    /// What it installs for `:suggest`: a tokenizer that splits on
    /// newlines and nothing else.
    Suggest,
}

/// `ShingleAnalyzerWrapper(ANALYZER, 3)`, the size the writer configures.
const MAXIMUM_SHINGLE_SIZE: usize = 3;

/// `IndexDefinition.DEFAULT_MAX_FIELD_LENGTH`.
pub const DEFAULT_MAXIMUM_FIELD_LENGTH: usize = 10_000;

/// What a definition contributes to the analyzer it builds.
///
/// `LuceneIndexDefinition.createAnalyzer` and
/// `IndexWriterUtils.getIndexWriterConfig` between them read exactly these
/// four things off the definition; everything else about the chain is
/// fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalyzerSettings {
    /// `maxFieldLength`. `None` is Oak's negative value, which means no
    /// cap at all rather than a cap of zero.
    pub maximum_field_length: Option<usize>,
    /// `analyzers/@indexOriginalTerm`.
    pub index_original_term: bool,
    /// `evaluatePathRestrictions`, without which `:ancestors` goes
    /// through the default analyzer like any other field.
    pub evaluate_path_restrictions: bool,
    /// `suggestAnalyzed`, which leaves `:suggest` to the definition's own
    /// analyzer instead of the suggest helper's.
    pub suggest_analyzed: bool,
}

impl Default for AnalyzerSettings {
    fn default() -> Self {
        Self {
            maximum_field_length: Some(DEFAULT_MAXIMUM_FIELD_LENGTH),
            index_original_term: false,
            evaluate_path_restrictions: false,
            suggest_analyzed: false,
        }
    }
}

/// The analyzer one definition builds, with the writer configuration's
/// per-field entries over it.
///
/// Two wrappers, in Oak's own order: the definition's
/// `PerFieldAnalyzerWrapper` routes `:ancestors`, and the whole of it goes
/// under the token-count cap; then the writer's second wrapper replaces
/// the analyzer *whole* for `:spellcheck` and `:suggest`, which is why
/// those two are never capped.
#[derive(Clone, Copy, Debug)]
pub struct Analyzer {
    settings: AnalyzerSettings,
}

impl Analyzer {
    /// The analyzer of a definition with these settings.
    #[must_use]
    pub const fn new(settings: AnalyzerSettings) -> Self {
        Self { settings }
    }

    /// Which chain a field takes.
    #[must_use]
    pub const fn chain_of(&self, field_name: &str) -> Chain {
        // `const` string comparison, which `==` is not.
        if equal(field_name, field_names::SPELLCHECK) {
            return Chain::Spellcheck;
        }
        if equal(field_name, field_names::SUGGEST) && !self.settings.suggest_analyzed {
            return Chain::Suggest;
        }
        if equal(field_name, field_names::ANCESTORS) && self.settings.evaluate_path_restrictions {
            return Chain::Ancestors;
        }
        if self.settings.index_original_term {
            Chain::OriginalTerm
        } else {
            Chain::Default
        }
    }

    /// Whether the cap reaches a field. The writer's per-field entries
    /// bypass it; every other field keeps the definition's analyzer.
    const fn limit_of(&self, chain: Chain) -> Option<usize> {
        match chain {
            Chain::Spellcheck | Chain::Suggest => None,
            _ => self.settings.maximum_field_length,
        }
    }

    /// Analyzes one value of one field.
    ///
    /// **One value**, not one field: Lucene's inverter opens one token
    /// stream per field instance and Oak adds one field per property
    /// value, so a cap of 10,000 over three values admits 30,000 tokens.
    #[must_use]
    pub fn tokens(&self, field_name: &str, text: &str) -> TokenStreamResult {
        let chain = self.chain_of(field_name);
        let units: Vec<u16> = text.encode_utf16().collect();
        let produced = match chain {
            Chain::Default | Chain::OriginalTerm => word_delimiter::filter(
                lower_case::filter(standard_tokenizer::tokenize(&units)),
                chain == Chain::OriginalTerm,
            ),
            Chain::Ancestors => path_hierarchy::tokenize(&units),
            Chain::Spellcheck => shingle::filter(
                word_delimiter::filter(
                    lower_case::filter(standard_tokenizer::tokenize(&units)),
                    false,
                ),
                MAXIMUM_SHINGLE_SIZE,
            ),
            Chain::Suggest => suggest_tokenizer::tokenize(&units),
        };
        let capped = match self.limit_of(chain) {
            Some(limit) => token_limit::filter(produced, limit),
            None => produced,
        };
        capped.into()
    }
}

/// Two field names, compared in a `const` context.
const fn equal(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut at = 0;
    while at < left.len() {
        if left[at] != right[at] {
            return false;
        }
        at += 1;
    }
    true
}

/// Decodes the code point at `at` in a UTF-16 sequence, with its length in
/// code units.
pub(crate) fn code_point_at(units: &[u16], at: usize) -> (u32, usize) {
    let first = units[at];
    if (0xd800..0xdc00).contains(&first) && at + 1 < units.len() {
        let second = units[at + 1];
        if (0xdc00..0xe000).contains(&second) {
            let point =
                0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
            return (point, 2);
        }
    }
    (u32::from(first), 1)
}

/// The UTF-16 units of a code-point range, as a string.
pub(crate) fn units_to_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}
