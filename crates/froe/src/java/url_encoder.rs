//! Java's URL encoding of a string as UTF-8, which is
//! `java.net.URLEncoder.encode(String, StandardCharsets.UTF_8)`.
//!
//! This is the last step of Oak's property-index key derivation: the editor
//! reads a value through the value pattern, substitutes the empty token,
//! truncates to 100 UTF-16 code units and then URL-encodes the result.
//! [`docs/analysis/index-property-storage.md`] §4 specifies that derivation
//! and quotes `PropertyIndexUtil.encode`, which calls this encoder.
//!
//! The rules, none of which match a modern percent-encoder:
//!
//! * alphanumerics and `.`, `-`, `*`, `_` pass through;
//! * a space becomes `+`, not `%20`;
//! * every other character is emitted as `%XX` per UTF-8 byte, with
//!   **upper-case** hexadecimal digits — Java's `Character.forDigit` produces
//!   lower-case and the encoder then subtracts the case difference;
//! * `~` and `!` are *not* unreserved here, so they encode.
//!
//! The input is a sequence of UTF-16 code units rather than a `&str` because
//! the truncation that precedes this step can cut a surrogate pair in half,
//! and Rust's `str` cannot hold an unpaired surrogate. Java can, and its
//! `String.getBytes(UTF_8)` replaces one with the single byte `0x3F` — the
//! charset encoder's replacement, which is the ASCII `?` — so a truncated
//! surrogate pair encodes as `%3F`. Reproducing that is the whole reason the
//! contract is stated in code units.
//!
//! [`docs/analysis/index-property-storage.md`]: ../../../../docs/analysis/index-property-storage.md

/// The byte Java's UTF-8 charset encoder substitutes for an unpaired
/// surrogate, which is the ASCII question mark.
const UNPAIRED_SURROGATE_REPLACEMENT: u8 = b'?';

/// The characters `java.net.URLEncoder` leaves alone, minus the space, which
/// it holds in the same set but then rewrites to `+`.
fn passes_through(unit: u16) -> bool {
    let Ok(byte) = u8::try_from(unit) else {
        return false;
    };
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'*' | b'_')
}

/// Encodes `units` the way Java's URL encoder encodes the string they spell.
///
/// `units` are UTF-16 code units, in order; an unpaired surrogate among them
/// is legal input and encodes as `%3F`, exactly as Java's does.
pub(crate) fn url_encode(units: impl IntoIterator<Item = u16>) -> String {
    let mut encoded = String::new();
    let mut units = units.into_iter().peekable();
    while let Some(unit) = units.next() {
        if passes_through(unit) {
            encoded.push(char::from(unit as u8));
        } else if unit == u16::from(b' ') {
            encoded.push('+');
        } else {
            append_percent_encoded(&mut encoded, code_point_bytes(unit, &mut units));
        }
    }
    encoded
}

/// The UTF-8 bytes Java would produce for `unit`, consuming the trailing half
/// of a surrogate pair from `rest` when `unit` opens one.
///
/// A high surrogate followed by a low surrogate is one code point; a surrogate
/// that is not part of a pair is the replacement byte. Java reaches the same
/// result by collecting a run of characters needing encoding and calling
/// `String.getBytes(UTF_8)` over it, which is equivalent because UTF-8 encodes
/// each code point independently.
fn code_point_bytes(
    unit: u16,
    rest: &mut std::iter::Peekable<impl Iterator<Item = u16>>,
) -> ([u8; 4], usize) {
    let mut bytes = [0u8; 4];
    let scalar = if is_high_surrogate(unit) {
        match rest.peek().copied() {
            Some(low) if is_low_surrogate(low) => {
                rest.next();
                combine_surrogates(unit, low)
            }
            _ => None,
        }
    } else if is_low_surrogate(unit) {
        None
    } else {
        char::from_u32(u32::from(unit))
    };
    if let Some(character) = scalar {
        let length = character.encode_utf8(&mut bytes).len();
        (bytes, length)
    } else {
        bytes[0] = UNPAIRED_SURROGATE_REPLACEMENT;
        (bytes, 1)
    }
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

fn combine_surrogates(high: u16, low: u16) -> Option<char> {
    let code_point = 0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
    char::from_u32(code_point)
}

/// Appends `%XX` per byte with upper-case hexadecimal digits.
fn append_percent_encoded(encoded: &mut String, (bytes, length): ([u8; 4], usize)) {
    for &byte in &bytes[..length] {
        encoded.push('%');
        encoded.push(upper_case_hexadecimal_digit(byte >> 4));
        encoded.push(upper_case_hexadecimal_digit(byte & 0x0F));
    }
}

fn upper_case_hexadecimal_digit(nibble: u8) -> char {
    char::from(b"0123456789ABCDEF"[usize::from(nibble)])
}

#[cfg(test)]
mod tests {
    use super::url_encode;

    /// The vectors this module is pinned against, produced by the JDK inside
    /// the digest-pinned Sling image; the file's header records the exact
    /// command and the program that produced them.
    const VECTORS: &str = include_str!("../../tests/fixtures/java-url-encoder-vectors.tsv");

    fn encode(text: &str) -> String {
        url_encode(text.encode_utf16())
    }

    #[test]
    fn alphanumerics_pass_through() {
        assert_eq!(encode("azAZ09"), "azAZ09");
    }

    #[test]
    fn the_four_unreserved_punctuation_characters_pass_through() {
        assert_eq!(encode(".-*_"), ".-*_");
    }

    #[test]
    fn a_space_becomes_a_plus_sign() {
        assert_eq!(encode("a b"), "a+b");
    }

    #[test]
    fn a_plus_sign_is_encoded_rather_than_kept() {
        assert_eq!(encode("a+b"), "a%2Bb");
    }

    #[test]
    fn hexadecimal_digits_are_upper_case() {
        assert_eq!(encode("ä"), "%C3%A4");
    }

    #[test]
    fn a_colon_encodes_as_the_property_index_key_separator() {
        assert_eq!(encode("sling:Folder"), "sling%3AFolder");
    }

    #[test]
    fn tilde_and_exclamation_mark_are_not_unreserved_here() {
        assert_eq!(encode("~!"), "%7E%21");
    }

    #[test]
    fn a_control_character_encodes_as_its_own_byte() {
        assert_eq!(encode("\u{0}\u{1f}\u{7f}"), "%00%1F%7F");
    }

    #[test]
    fn an_astral_code_point_encodes_as_four_bytes() {
        assert_eq!(encode("\u{1f600}"), "%F0%9F%98%80");
    }

    #[test]
    fn a_lone_high_surrogate_encodes_as_a_question_mark() {
        assert_eq!(url_encode([0xD83Du16]), "%3F");
    }

    #[test]
    fn a_lone_low_surrogate_encodes_as_a_question_mark() {
        assert_eq!(url_encode([0xDE00u16]), "%3F");
    }

    #[test]
    fn two_unpaired_surrogates_encode_as_two_question_marks() {
        assert_eq!(url_encode([0xD83Du16, 0xD83Du16]), "%3F%3F");
    }

    #[test]
    fn a_pair_followed_by_a_lone_high_surrogate_keeps_the_pair() {
        assert_eq!(url_encode([0xD83Du16, 0xDE00, 0xD83D]), "%F0%9F%98%80%3F");
    }

    #[test]
    fn the_empty_string_encodes_as_the_empty_string() {
        assert_eq!(encode(""), "");
    }

    #[test]
    fn every_committed_vector_round_trips() {
        let mut replayed = 0usize;
        for line in VECTORS.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let mut fields = line.split('\t');
            let name = fields.next().expect("vector line has a name");
            let units = fields.next().expect("vector line has an input column");
            let expected = fields.next().expect("vector line has an expected column");
            assert!(fields.next().is_none(), "{name}: vector has extra columns");
            let input: Vec<u16> = units
                .split_whitespace()
                .map(|unit| {
                    u16::from_str_radix(unit, 16)
                        .unwrap_or_else(|_| panic!("{name}: {unit} is not a code unit"))
                })
                .collect();
            assert_eq!(url_encode(input), expected, "vector {name}");
            replayed += 1;
        }
        assert!(
            replayed >= 20,
            "expected the committed vector set, replayed {replayed}"
        );
    }
}
