//! The token-count cap.
//!
//! `docs/analysis/lucene-oak-analysis.md` §5, from
//! `analysis/miscellaneous/LimitTokenCountFilter.java` under
//! `LimitTokenCountAnalyzer(result, maxFieldLength)`, whose two-argument
//! constructor passes `consumeAllTokens = false`.
//!
//! That false is the whole of this module. On reaching its limit the
//! filter returns `false` **without pulling what is below it**, so the
//! source stops where it stands and `end()` reports the end of the last
//! token it scanned rather than the end of the value — which is why every
//! stage carries a per-token stop. A limit of zero empties the stream and
//! reports nothing at all, silently: 4.7.2 has no refusal here.

use super::Staged;

/// Keeps at most `limit` tokens, and takes the end state the source would
/// have reported there.
pub(super) fn filter(mut stream: Staged, limit: usize) -> Staged {
    if stream.tokens.len() <= limit {
        return stream;
    }
    stream.tokens.truncate(limit);
    stream.end = stream.tokens.last().map_or(
        // A limit of zero pulls nothing at all, so the source is still at
        // the start of the value and `end()` reports its untouched
        // attributes: no increment, no offset.
        super::EndState::default(),
        |staged| staged.stop,
    );
    stream
}
