//! Oak's `ValuePattern`: the three value restrictions an index definition
//! may carry, and the composed rule that decides whether a value is indexed.
//!
//! All three — `valueIncludedPrefixes`, `valueExcludedPrefixes` and
//! `valuePattern` — can coexist on one definition, and the rule consults them
//! in a fixed order, so the model carries all three rather than collapsing
//! them. `docs/analysis/index-property-storage.md` §4.1 quotes
//! `ValuePattern.matches` and states the order.
//!
//! froe carries no general regular-expression engine and will not approximate
//! Java's. A `valuePattern` is therefore a typed refusal — but **only when
//! the composed rule actually reaches it**. An include prefix that matches
//! short-circuits before the pattern is consulted, so a definition that
//! stores one is perfectly checkable for the values its prefixes admit.

use crate::content::node::{NodeState, PropertyState};
use crate::index::{IndexError, IndexResult, IndexWarning, strict_string, strict_strings};

/// The value restriction on an index definition.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ValuePattern {
    included_prefixes: Option<Vec<String>>,
    excluded_prefixes: Option<Vec<String>>,
    regular_expression: Option<String>,
}

impl ValuePattern {
    /// Reads the pattern from a definition node the way Oak's *editor* reads
    /// it — an array converting to strings, a single value strictly as a
    /// `STRING` — and reports, through `warnings`, where the query side would
    /// read it differently.
    ///
    /// A single non-`STRING` prefix value is a typed error rather than a
    /// warning, because Oak reads it as `null` and then throws inside
    /// `matches` on the first value it tests: the definition cannot be
    /// indexed at all.
    pub fn from_definition(
        definition: &NodeState<'_>,
        definition_path: &str,
        warnings: &mut Vec<IndexWarning>,
    ) -> IndexResult<Self> {
        let included = read_prefixes(
            definition,
            "valueIncludedPrefixes",
            definition_path,
            warnings,
        )?;
        let excluded = read_prefixes(
            definition,
            "valueExcludedPrefixes",
            definition_path,
            warnings,
        )?;
        let regular_expression = definition
            .property("valuePattern")?
            .as_ref()
            .and_then(|property| strict_string(Some(property)).map(str::to_owned));
        if let Some(pattern) = &regular_expression {
            warnings.push(IndexWarning::ValuePatternNotEvaluated {
                pattern: pattern.clone(),
            });
        }
        Ok(Self {
            included_prefixes: included,
            excluded_prefixes: excluded,
            regular_expression,
        })
    }

    /// Constructs a pattern from its three parts, for tests and for callers
    /// that already hold them.
    #[must_use]
    pub fn new(
        included_prefixes: Option<Vec<String>>,
        excluded_prefixes: Option<Vec<String>>,
        regular_expression: Option<String>,
    ) -> Self {
        Self {
            included_prefixes,
            excluded_prefixes,
            regular_expression,
        }
    }

    /// Whether the pattern admits everything, which is `ValuePattern.matchesAll`:
    /// all three parts absent.
    #[must_use]
    pub fn matches_all(&self) -> bool {
        self.included_prefixes.is_none()
            && self.excluded_prefixes.is_none()
            && self.regular_expression.is_none()
    }

    /// The stored regular expression, when there is one.
    #[must_use]
    pub fn regular_expression(&self) -> Option<&str> {
        self.regular_expression.as_deref()
    }

    /// Whether `value` is indexed, following `ValuePattern.matches` exactly:
    /// an include prefix wins outright; an exclude prefix matches in either
    /// direction; a regular expression applies only when no include prefix
    /// matched; and with include prefixes and no regular expression, a value
    /// matching none of them is rejected.
    ///
    /// The `Unsupported` error is raised only on the one path that reaches
    /// the regular expression.
    pub fn matches(&self, value: &str, definition_path: &str) -> IndexResult<bool> {
        if self.matches_all() {
            return Ok(true);
        }
        if let Some(included) = &self.included_prefixes
            && included.iter().any(|prefix| value.starts_with(prefix))
        {
            return Ok(true);
        }
        if let Some(excluded) = &self.excluded_prefixes
            && excluded
                .iter()
                .any(|prefix| value.starts_with(prefix.as_str()) || prefix.starts_with(value))
        {
            return Ok(false);
        }
        match &self.regular_expression {
            None if self.included_prefixes.is_some() => Ok(false),
            None => Ok(true),
            Some(pattern) => Err(IndexError::UnsupportedValuePattern {
                definition_path: definition_path.to_owned(),
                pattern: pattern.clone(),
            }),
        }
    }
}

/// `ValuePattern.getStrings` over a builder: an array converts to strings, a
/// single value is read strictly as a `STRING`, and an absent property is
/// `None` — which is what makes `matchesAll` distinguish "no restriction"
/// from "an empty one".
fn read_prefixes(
    definition: &NodeState<'_>,
    property_name: &str,
    definition_path: &str,
    warnings: &mut Vec<IndexWarning>,
) -> IndexResult<Option<Vec<String>>> {
    let Some(property) = definition.property(property_name)? else {
        return Ok(None);
    };
    if is_array(&property) {
        note_query_side_disagreement(&property, property_name, warnings);
        return Ok(Some(crate::index::converting_strings(Some(&property))));
    }
    match strict_string(Some(&property)) {
        Some(text) => Ok(Some(vec![text.to_owned()])),
        None => Err(IndexError::NonStringPrefixValue {
            definition_path: definition_path.to_owned(),
            property_name: property_name.to_owned(),
        }),
    }
}

fn is_array(property: &PropertyState) -> bool {
    matches!(
        property.values,
        crate::content::node::PropertyValues::Multiple(_)
    )
}

/// The editor converts an array; the query side reads it strictly as
/// `STRINGS`. Where those disagree the consequence differs by property, and
/// the exclusion's is the one that loses data.
fn note_query_side_disagreement(
    property: &PropertyState,
    property_name: &str,
    warnings: &mut Vec<IndexWarning>,
) {
    if strict_strings(Some(property)).is_some() {
        return;
    }
    let stored_type = crate::index::stored_type_name(property);
    if property_name == "valueExcludedPrefixes" {
        warnings.push(IndexWarning::ExcludedPrefixesIgnoredByQueries { stored_type });
    } else {
        warnings.push(IndexWarning::IndexedButNeverSelected {
            property_name: property_name.to_owned(),
            stored_type,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::ValuePattern;

    const PATH: &str = "/oak:index/test";

    fn prefixes(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn an_absent_pattern_admits_everything() {
        let pattern = ValuePattern::default();
        assert!(pattern.matches_all());
        assert!(pattern.matches("anything", PATH).expect("no refusal"));
    }

    #[test]
    fn an_include_prefix_wins_over_an_exclude_prefix() {
        let pattern = ValuePattern::new(Some(prefixes(&["ab"])), Some(prefixes(&["a"])), None);
        assert!(pattern.matches("abc", PATH).expect("no refusal"));
    }

    #[test]
    fn an_exclude_prefix_matches_in_either_direction() {
        let pattern = ValuePattern::new(None, Some(prefixes(&["abc"])), None);
        assert!(!pattern.matches("abcdef", PATH).expect("no refusal"));
        assert!(!pattern.matches("ab", PATH).expect("no refusal"));
        assert!(pattern.matches("b", PATH).expect("no refusal"));
    }

    #[test]
    fn include_prefixes_without_a_pattern_reject_everything_else() {
        let pattern = ValuePattern::new(Some(prefixes(&["ab"])), None, None);
        assert!(!pattern.matches("xy", PATH).expect("no refusal"));
    }

    #[test]
    fn a_regular_expression_is_refused_only_where_the_rule_reaches_it() {
        let pattern = ValuePattern::new(Some(prefixes(&["ab"])), None, Some("x.*".to_owned()));
        assert!(
            pattern
                .matches("abc", PATH)
                .expect("the include prefix short-circuits"),
            "an include prefix must win before the pattern is consulted"
        );
        let error = pattern
            .matches("xy", PATH)
            .expect_err("a value with no include prefix reaches the pattern");
        assert!(
            matches!(
                error,
                crate::index::IndexError::UnsupportedValuePattern { .. }
            ),
            "{error}"
        );
    }

    #[test]
    fn an_exclude_prefix_answers_before_the_pattern_is_reached() {
        let pattern = ValuePattern::new(None, Some(prefixes(&["x"])), Some("x.*".to_owned()));
        assert!(!pattern.matches("xy", PATH).expect("the exclusion answers"));
    }
}
