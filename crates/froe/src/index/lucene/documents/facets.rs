//! Facet fields: what a facet property becomes.
//!
//! `docs/analysis/lucene-oak-documents.md` §5, from
//! `LuceneDocumentMaker.indexFacetProperty` and — for the build pass —
//! the pinned `lucene-facet` artifact's `FacetsConfig`.
//!
//! The document maker adds a `SortedSetDocValuesFacetField` per value; the
//! build pass then re-emits the document, turning each into **three**
//! fields under `<property>_facet`: a sorted-set doc value for counting
//! and two unstored drill-down terms, the escaped full path and the bare
//! dimension.

use crate::index::IndexError;

/// `FacetsConfig.DELIM_CHAR`, which joins a path's components.
const DELIMITER: char = '\u{1f}';

/// `FacetsConfig.ESCAPE_CHAR`, which precedes a component's own
/// delimiter or escape.
const ESCAPE: char = '\u{1e}';

/// `FieldNames.createFacetFieldName`.
#[must_use]
pub fn facet_field_name(property_name: &str) -> String {
    format!("{property_name}_facet")
}

/// One facet value: the dimension it was indexed under, and the label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetValue {
    /// The property name, which is the dimension.
    pub dimension: String,
    /// The value.
    pub label: String,
}

impl FacetValue {
    /// `FacetsConfig.pathToString` over the dimension and the label.
    ///
    /// # Errors
    ///
    /// [`IndexError::UnsupportedDefinition`] for an empty component, which
    /// `pathToString` throws an `IllegalArgumentException` for — froe
    /// never reaches it, the maker skipping an empty value, and the check
    /// is here so that a later caller cannot.
    pub fn to_path(&self, definition_path: &str) -> Result<String, IndexError> {
        let mut rendered = String::new();
        for component in [&self.dimension, &self.label] {
            if component.is_empty() {
                return Err(IndexError::UnsupportedDefinition {
                    definition_path: definition_path.to_owned(),
                    feature: "a facet path component is empty, which Lucene's own facet \
                              configuration refuses"
                        .to_owned(),
                });
            }
            for character in component.chars() {
                if character == DELIMITER || character == ESCAPE {
                    rendered.push(ESCAPE);
                }
                rendered.push(character);
            }
            rendered.push(DELIMITER);
        }
        // The last delimiter is trimmed rather than not written, which is
        // what `pathToString` does.
        rendered.pop();
        Ok(rendered)
    }
}

/// What one facet property contributed, for the configuration that
/// persists into the visible definition — `docs/analysis/…` §5.3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetDimension {
    /// The property name.
    pub name: String,
    /// Whether any node indexed it as a multi-valued `STRINGS` property,
    /// which writes `multivalued = true` beside it.
    pub multi_valued: bool,
}
