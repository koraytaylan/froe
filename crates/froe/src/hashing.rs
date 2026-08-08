//! Hash functions that must match their Java counterparts bit for bit.
//!
//! Map records place child entries by a hash of the entry name, so reading
//! a map requires reproducing the exact hash the Java implementation used
//! when the map was written: `java.lang.String::hashCode` (computed over
//! UTF-16 code units with wrapping 32-bit arithmetic) scrambled by the
//! constants from `MapRecord`.

/// Multiplier taken from `MapRecord.M`, "a magic constant from a random
/// number generator, used to generate good hash values".
const MAP_HASH_MULTIPLIER: i32 = 0xDEEC_E66Du32 as i32;

/// Increment taken from `MapRecord.A`.
const MAP_HASH_INCREMENT: i32 = 0xB;

/// Computes `java.lang.String::hashCode` for `text`.
///
/// Java hashes the UTF-16 representation: `hash = 31 * hash + code_unit`
/// for every code unit, with all arithmetic wrapping at 32 bits. Characters
/// outside the basic multilingual plane contribute their two surrogate
/// halves individually.
#[must_use]
pub fn utf16_string_hash(text: &str) -> i32 {
    text.encode_utf16().fold(0i32, |hash, code_unit| {
        hash.wrapping_mul(31).wrapping_add(i32::from(code_unit))
    })
}

/// Computes the hash under which a map record stores the entry named `name`,
/// matching `MapRecord.getHash`: `(name.hashCode() ^ M) * M + A`.
///
/// The result is returned as `u32` because readers compare and sort these
/// hashes as unsigned 32-bit values (Java masks them with `0xFFFFFFFFL`).
#[must_use]
pub fn map_entry_hash(name: &str) -> u32 {
    (utf16_string_hash(name) ^ MAP_HASH_MULTIPLIER)
        .wrapping_mul(MAP_HASH_MULTIPLIER)
        .wrapping_add(MAP_HASH_INCREMENT) as u32
}

/// Compares two strings the way `java.lang.String::compareTo` does: by
/// UTF-16 code units. This differs from Rust's `str` ordering for
/// supplementary characters, which sort *after* the basic multilingual
/// plane in Rust but *between* U+D7FF and U+E000 (as surrogates) in Java.
/// Map leaf entries and template properties are ordered this way on disk.
#[must_use]
pub fn compare_utf16_strings(first: &str, second: &str) -> std::cmp::Ordering {
    first.encode_utf16().cmp(second.encode_utf16())
}

#[cfg(test)]
mod tests {
    use super::{compare_utf16_strings, map_entry_hash, utf16_string_hash};

    #[test]
    fn matches_known_hash_values() {
        assert_eq!(utf16_string_hash(""), 0);
        assert_eq!(utf16_string_hash("a"), 97);
        assert_eq!(utf16_string_hash("hello"), 99_162_322);
        // "Aa" and "BB" are a famous Java hash collision.
        assert_eq!(utf16_string_hash("Aa"), utf16_string_hash("BB"));
        assert_eq!(utf16_string_hash("jcr:primaryType"), 1_317_115_067);
    }

    #[test]
    fn hashes_surrogate_pairs_individually() {
        // U+1D54A is encoded as the surrogate pair D835 DD4A, so the hash is
        // 31 * 0xD835 + 0xDD4A = 1772469.
        assert_eq!(utf16_string_hash("\u{1D54A}"), 1_772_469);
    }

    #[test]
    fn utf16_order_differs_from_rust_for_supplementary_characters() {
        // U+FFFD (basic plane) versus U+1D54A (supplementary): Rust orders
        // by code point, Java by UTF-16 units where the surrogate pair of
        // U+1D54A (starting 0xD835) sorts *before* 0xFFFD.
        assert_eq!(
            compare_utf16_strings("\u{1D54A}", "\u{FFFD}"),
            std::cmp::Ordering::Less,
            "Java sorts the surrogate pair first"
        );
        assert!(
            "\u{1D54A}" > "\u{FFFD}",
            "Rust sorts the supplementary character last"
        );
        assert_eq!(
            compare_utf16_strings("alpha", "beta"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_utf16_strings("same", "same"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn map_entry_hash_matches_externally_computed_vectors() {
        // Literal outputs computed outside this codebase from
        // `(String.hashCode(name) ^ 0xDEECE66D) * 0xDEECE66D + 0xB` with
        // wrapping 32-bit arithmetic — never recomputed here, so a
        // mistranscribed constant cannot hide by agreeing with itself.
        assert_eq!(map_entry_hash(""), 0xB460_0A74);
        assert_eq!(map_entry_hash("a"), 0x3C9C_BB27);
        assert_eq!(map_entry_hash("root"), 0xC289_24EE);
        assert_eq!(map_entry_hash("content"), 0x9646_6A8F);
        assert_eq!(map_entry_hash("jcr:content"), 0x88CE_F29C);
        // The Java string-hash collision carries into the scrambled hash.
        assert_eq!(map_entry_hash("Aa"), 0x6059_D734);
        assert_eq!(map_entry_hash("BB"), 0x6059_D734);
    }
}
