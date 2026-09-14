//! Property-index key derivation: the exact computation that turns a
//! property's values into the child names a `:index` subtree is keyed by.
//!
//! `docs/analysis/index-property-storage.md` §4 specifies it and quotes
//! `PropertyIndexUtil.encode`. In order: read the values through the value
//! pattern, substitute the empty token for an empty value, truncate to 100
//! UTF-16 code units, and URL-encode the result as UTF-8.
//!
//! Two properties of that order are load-bearing and easy to lose:
//!
//! * **The empty token is substituted before encoding**, so the key for an
//!   empty value is the literal one-character hidden name `:`, never `%3A`.
//! * **Truncation counts UTF-16 code units**, so it can cut a surrogate pair
//!   in half, and Java's encoder then emits `%3F` for the orphan — a key with
//!   no `?` anywhere in the source value. The crate-private
//!   `java::url_encode` holds that behaviour, pinned against vectors a real
//!   JDK produced.
//!
//! The result is a **set**. Oak's own encoder returns a `HashSet` and the
//! editor unions the keys of every value, so two values that collapse to one
//! key — two empty values, or two sharing a 100-unit prefix — contribute one
//! entry. A unique index built from such a property is *not* in a duplicate
//! state, and a rebuild that emitted one key per value would wrongly refuse
//! it.

use std::collections::BTreeSet;

use crate::content::node::PropertyState;
use crate::content::property::{PropertyType, PropertyValue};
use crate::index::value_pattern::ValuePattern;
use crate::index::{IndexResult, values_of};

/// The key Oak stores an empty value under.
pub const EMPTY_VALUE_TOKEN: &str = ":";

/// The truncation bound, in UTF-16 code units.
pub const MAXIMUM_KEY_SOURCE_UNITS: usize = 100;

/// The keys `property` contributes to a property index, or an empty set when
/// it contributes none.
///
/// Fallible because the value-pattern test is: a definition whose composed
/// rule reaches a regular expression answers with the typed `Unsupported`
/// error, which this function propagates rather than swallowing. Silently
/// reinterpreting such a definition is exactly what the scope forbids.
pub fn keys_for_property(
    property: &PropertyState,
    pattern: &ValuePattern,
    definition_path: &str,
) -> IndexResult<BTreeSet<String>> {
    // `PropertyIndexEditor.addValueKeys`: a binary contributes nothing,
    // whatever its arity, and neither does an empty multi-valued property.
    if property.property_type == PropertyType::Binary {
        return Ok(BTreeSet::new());
    }
    let mut keys = BTreeSet::new();
    for value in values_of(property) {
        let Some(text) = value.as_text() else {
            continue;
        };
        if pattern.matches(&text, definition_path)? {
            keys.insert(encode_key(&text));
        }
    }
    Ok(keys)
}

/// The key one value is stored under: the empty token, or the value
/// truncated to 100 UTF-16 code units and URL-encoded.
#[must_use]
pub fn encode_key(value: &str) -> String {
    if value.is_empty() {
        return EMPTY_VALUE_TOKEN.to_owned();
    }
    crate::java::url_encode(truncated_units(value))
}

/// The first 100 UTF-16 code units of `value`, which is what
/// `String.substring(0, 100)` yields — including a surrogate pair cut in
/// half, which is why this returns code units rather than a `String`.
fn truncated_units(value: &str) -> impl Iterator<Item = u16> + '_ {
    value.encode_utf16().take(MAXIMUM_KEY_SOURCE_UNITS)
}

/// The value a key was derived from, as far as it can be recovered.
///
/// This is *not* an inverse of [`encode_key`]: truncation and the empty
/// token both lose information, and a key is a name froe reads from a store
/// rather than something it has to invert. What it does is undo the URL
/// encoding, so a consistency check can compare a stored key against the
/// content value that should have produced it without re-encoding every
/// candidate.
#[must_use]
pub fn key_matches_value(key: &str, value: &PropertyValue) -> bool {
    value.as_text().is_some_and(|text| encode_key(&text) == key)
}

#[cfg(test)]
mod tests {
    use super::{encode_key, keys_for_property};
    use crate::content::node::{PropertyState, PropertyValues};
    use crate::content::property::{PropertyType, PropertyValue};
    use crate::index::value_pattern::ValuePattern;

    const PATH: &str = "/oak:index/test";

    fn strings(values: &[&str]) -> PropertyState {
        PropertyState {
            name: "foo".to_owned(),
            property_type: PropertyType::String,
            values: PropertyValues::Multiple(
                values
                    .iter()
                    .map(|value| PropertyValue::String((*value).to_owned()))
                    .collect(),
            ),
        }
    }

    fn keys(property: &PropertyState) -> Vec<String> {
        keys_for_property(property, &ValuePattern::default(), PATH)
            .expect("no pattern refusal")
            .into_iter()
            .collect()
    }

    #[test]
    fn an_empty_value_keys_as_the_empty_token() {
        assert_eq!(encode_key(""), ":");
    }

    #[test]
    fn a_colon_in_a_value_is_encoded_rather_than_left_as_the_empty_token() {
        assert_eq!(encode_key(":"), "%3A");
    }

    #[test]
    fn a_value_of_exactly_a_hundred_units_is_not_truncated() {
        let value = "a".repeat(100);
        assert_eq!(encode_key(&value), value);
    }

    #[test]
    fn a_value_of_a_hundred_and_one_units_is_truncated_to_a_hundred() {
        let value = "a".repeat(101);
        assert_eq!(encode_key(&value), "a".repeat(100));
    }

    #[test]
    fn truncation_inside_a_surrogate_pair_encodes_the_orphan_as_a_question_mark() {
        // 99 filler units, then an astral code point whose two units
        // straddle the bound: the high surrogate is kept, the low one is
        // cut, and Java's encoder replaces the orphan.
        let value = format!("{}{}", "a".repeat(99), '\u{1f600}');
        let encoded = encode_key(&value);
        assert_eq!(encoded, format!("{}%3F", "a".repeat(99)));
    }

    #[test]
    fn two_values_collapsing_to_one_key_contribute_one_entry() {
        let shared = "b".repeat(100);
        let property = strings(&[&format!("{shared}one"), &format!("{shared}two")]);
        assert_eq!(keys(&property), [shared]);
    }

    #[test]
    fn two_empty_values_contribute_one_entry() {
        assert_eq!(keys(&strings(&["", ""])), [":"]);
    }

    #[test]
    fn a_multi_valued_property_contributes_every_distinct_key() {
        assert_eq!(keys(&strings(&["a", "b"])), ["a", "b"]);
    }

    #[test]
    fn a_binary_property_contributes_nothing() {
        let property = PropertyState {
            name: "jcr:data".to_owned(),
            property_type: PropertyType::Binary,
            values: PropertyValues::Single(PropertyValue::String("ignored".to_owned())),
        };
        assert!(keys(&property).is_empty());
    }

    #[test]
    fn an_empty_multi_valued_property_contributes_nothing() {
        assert!(keys(&strings(&[])).is_empty());
    }

    #[test]
    fn a_value_the_pattern_rejects_contributes_nothing() {
        let pattern = ValuePattern::new(Some(vec!["keep".to_owned()]), None, None);
        let property = strings(&["keep-me", "drop-me"]);
        let keys: Vec<String> = keys_for_property(&property, &pattern, PATH)
            .expect("no refusal")
            .into_iter()
            .collect();
        assert_eq!(keys, ["keep-me"]);
    }

    #[test]
    fn a_regular_expression_the_rule_reaches_is_propagated_rather_than_swallowed() {
        let pattern = ValuePattern::new(None, None, Some("x.*".to_owned()));
        let error = keys_for_property(&strings(&["anything"]), &pattern, PATH)
            .expect_err("the pattern is reached");
        assert!(
            matches!(
                error,
                crate::index::IndexError::UnsupportedValuePattern { .. }
            ),
            "{error}"
        );
    }
}
