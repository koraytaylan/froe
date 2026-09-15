//! Oak's document maker: from a node state to the exact set of fields.
//!
//! `docs/analysis/lucene-oak-documents.md` §3, from
//! `FulltextDocumentMaker.makeDocument` with `LuceneDocumentMaker`
//! supplying the Lucene half of each branch.
//!
//! # The order is the specification
//!
//! Field order fixes positions, offsets and the stored-field sequence, so
//! §3.10's list is what a reproduction has to match, not a detail of it:
//! `:path`; then per property in the node state's own order, and then the
//! synthetic `:nodeName` — the ordered doc value, the typed fields, and
//! per value `full:<name>`, `:suggest`, `:spellcheck`, `:fulltext`, then
//! the facet field; then the aggregates, the markers, the node name, the
//! node name's `:fulltext` value, `:ancestors` and `:depth`; then the
//! facet build pass re-emits everything with the facet-derived fields
//! first; and the merged `:suggest` is last of all.

use std::collections::BTreeSet;

use crate::content::node::{NodeState, PropertyState, PropertyValues};
use crate::content::{PropertyType, PropertyValue};
use crate::index::lucene::analysis::numeric::{
    NumericTerm, date_to_long, double_terms, integer_terms, long_terms,
};
use crate::index::lucene::analysis::{Analyzer, AnalyzerSettings, TokenStreamResult, field_names};
use crate::index::lucene::codec::postings::IndexOptions;
use crate::index::lucene::documents::aggregate::{Match, Matcher};
use crate::index::lucene::documents::binaries::BinaryTextPolicy;
use crate::index::lucene::documents::facets::{FacetDimension, FacetValue, facet_field_name};
use crate::index::lucene::documents::rules::{
    IndexingRule, IndexingRules, PropertyDefinition, PropertyInclude,
};
use crate::index::lucene::writer::{DocValue, Document, Field, StoredValue, Token};
use crate::index::{IndexError, IndexResult};

mod aggregation;

/// `FieldNames.PATH`.
const PATH_FIELD: &str = ":path";
/// `FieldNames.FULLTEXT`.
const FULLTEXT_FIELD: &str = ":fulltext";
/// `FieldNames.NODE_NAME`.
const NODE_NAME_FIELD: &str = ":nodeName";
/// `FieldNames.NULL_PROPS`.
const NULL_PROPERTIES_FIELD: &str = ":nullProps";
/// `FieldNames.NOT_NULL_PROPS`.
const NOT_NULL_PROPERTIES_FIELD: &str = ":notNullProps";
/// `FieldNames.PATH_DEPTH`.
const DEPTH_FIELD: &str = ":depth";
/// `FieldNames.ANALYZED_FIELD_PREFIX`.
const ANALYZED_PREFIX: &str = "full:";
/// `FieldNames.FULLTEXT_RELATIVE_NODE`.
const RELATIVE_NODE_PREFIX: &str = "fullnode:";
/// `FieldNames.createDocValFieldName`'s prefix.
const DOC_VALUE_PREFIX: &str = ":dv";

/// `IndexDefinition.STRING_PROPERTY_MAX_LENGTH`, the only length guard Oak
/// applies — and to sorted doc values alone.
const STRING_DOC_VALUE_MAXIMUM_LENGTH: usize = 32_766;

/// `LuceneDocumentMaker.getTruncatedBytesRef`: a sorted doc value's UTF-8
/// bytes, cut so that no character is split.
///
/// ```java
/// BytesRef ref = new BytesRef(value);
/// if (ref.length <= maxLength) return ref;
/// int i = maxLength - 1;
/// while ((ref.bytes[i] & 0xC0) == 0x80) i--;   // step off a continuation byte
/// if    ((ref.bytes[i] & 0xC0) == 0xC0) i--;   // and off the lead byte it belongs to
/// byte[] copy = Arrays.copyOf(ref.bytes, i + 1);
/// ```
///
/// The length test is over **bytes**, and the walk back is why a cut that
/// lands inside a character keeps one character less than it could:
/// measured against the pinned image with `abcdefg` followed by three
/// `é`, a limit of 8 keeps 7 bytes, a limit of **9** keeps 7 as well —
/// the lead byte is stepped off even though its character would have fit
/// — and a limit of 10 keeps 9.
fn truncated_doc_value(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    if bytes.len() <= STRING_DOC_VALUE_MAXIMUM_LENGTH {
        return bytes.to_vec();
    }
    let mut at = STRING_DOC_VALUE_MAXIMUM_LENGTH - 1;
    while bytes[at] & 0b1100_0000 == 0b1000_0000 {
        let Some(previous) = at.checked_sub(1) else {
            return Vec::new();
        };
        at = previous;
    }
    if bytes[at] & 0b1100_0000 == 0b1100_0000 {
        // Oak keeps `i + 1` bytes, so stepping off the first byte keeps
        // none at all.
        let Some(previous) = at.checked_sub(1) else {
            return Vec::new();
        };
        at = previous;
    }
    bytes[..=at].to_vec()
}

/// One node's document, and what the facet configuration learned making
/// it.
#[derive(Clone, Debug)]
pub struct MadeDocument {
    /// The document, in field order.
    pub document: Document,
    /// The facet dimensions this node indexed, for the configuration that
    /// persists into the visible definition.
    pub facet_dimensions: Vec<FacetDimension>,
}

/// Oak's document maker over one definition.
pub struct DocumentMaker<'definition> {
    definition_path: &'definition str,
    rules: &'definition IndexingRules,
    analyzer: Analyzer,
    binaries: BinaryTextPolicy,
}

impl<'definition> DocumentMaker<'definition> {
    /// A maker over one definition's rules, with the binary-text policy it
    /// will ask for every binary property — which Oak's own maker likewise
    /// receives at construction.
    #[must_use]
    pub fn new(
        definition_path: &'definition str,
        rules: &'definition IndexingRules,
        binaries: BinaryTextPolicy,
    ) -> Self {
        let analyzer = Analyzer::new(AnalyzerSettings {
            maximum_field_length: rules.maximum_field_length.map_or_else(
                || Some(crate::index::lucene::analysis::DEFAULT_MAXIMUM_FIELD_LENGTH),
                |length| usize::try_from(length).ok(),
            ),
            index_original_term: rules.index_original_term,
            evaluate_path_restrictions: rules.evaluate_path_restrictions,
            suggest_analyzed: rules.suggest_analyzed,
        });
        Self {
            definition_path,
            rules,
            analyzer,
            binaries,
        }
    }

    /// The document for one node, or `None` when the node contributed
    /// nothing — which is Oak's `dirty` flag, and the reason a node under
    /// a rule that does not index every node of its type produces no
    /// document at all.
    ///
    /// # Errors
    ///
    /// [`IndexError::UnparseableDate`] for a `DATE` value Jackrabbit's own
    /// parser refuses, which is the one refusal this stage raises: Oak's
    /// own path throws there and fails the commit.
    pub fn make(
        &self,
        node: &NodeState<'_>,
        path: &str,
        rule: &IndexingRule,
    ) -> IndexResult<Option<MadeDocument>> {
        let mut state = DocumentState::new(path);
        state.add(string_field(PATH_FIELD, path, true));
        self.index_properties(node, path, rule, &mut state)?;
        self.index_aggregates(node, path, rule, &mut state)?;
        Self::index_markers(node, rule, &mut state)?;
        Self::add_node_name_field(path, rule, &mut state);
        // The empty-document check, which stands **between** the node-name
        // field and everything below it: a rule that does not index every
        // node of its type produces no document for a node that
        // contributed nothing.
        if !rule.indexes_all_nodes_of_matching_type && !state.dirty {
            return Ok(None);
        }
        self.index_node_name(path, rule, &mut state);
        if self.rules.evaluate_path_restrictions {
            self.index_ancestors(path, &mut state);
        }
        Ok(Some(self.finalize(state)))
    }
}

/// The document as it is being built, with the two flags Oak's maker
/// carries beside it.
struct DocumentState {
    fields: Vec<Field>,
    /// Oak's `dirty`: whether anything worth a document was added.
    dirty: bool,
    /// Whether a facet field was added, which selects the build pass.
    facet: bool,
    /// The `:dv` names already present, because a duplicate is dropped
    /// rather than overwritten.
    doc_value_names: BTreeSet<String>,
    /// The facet dimensions seen.
    dimensions: Vec<FacetDimension>,
    /// The `:suggest` values, which finalization merges into one field.
    suggest_values: Vec<String>,
    /// The node's own path, for the refusals that name it.
    path: String,
}

impl DocumentState {
    fn new(path: &str) -> Self {
        Self {
            fields: Vec::new(),
            dirty: false,
            facet: false,
            doc_value_names: BTreeSet::new(),
            dimensions: Vec::new(),
            suggest_values: Vec::new(),
            path: path.to_owned(),
        }
    }

    fn add(&mut self, field: Field) {
        self.fields.push(field);
    }

    /// Adds a field and marks the document worth writing.
    fn add_dirty(&mut self, field: Field) {
        self.fields.push(field);
        self.dirty = true;
    }
}

// ------------------------------------------------------- the field kinds

/// `IndexHelper.NOT_TOKENIZED`: `jcr:uuid` plus
/// `UserConstants.USER_PROPERTY_NAMES` and `GROUP_PROPERTY_NAMES`, read
/// out of the pinned image rather than from the constant declarations,
/// because the two collections are assembled from named constants across
/// two interfaces.
///
/// Sorted, so a lookup is a binary search and a reader can see the set is
/// complete.
const NOT_TOKENIZED: [&str; 7] = [
    "jcr:uuid",
    "rep:authorizableId",
    "rep:disabled",
    "rep:impersonators",
    "rep:members",
    "rep:password",
    "rep:principalName",
];

/// `PropertyDefinition.skipTokenization`:
///
/// ```java
/// public boolean skipTokenization(String propertyName) {
///     if (isRegexp && IndexHelper.skipTokenization(propertyName)) {
///         return true;
///     }
///     return !analyzed;
/// }
/// ```
///
/// The name list applies to a **regular-expression** definition alone: a
/// definition that names `jcr:uuid` outright and marks it `analyzed`
/// tokenizes it. The `!analyzed` arm is unreachable from the one caller,
/// which is inside the `analyzed` branch.
fn skip_tokenization(property_name: &str, definition: &PropertyDefinition) -> bool {
    definition.is_regexp && NOT_TOKENIZED.binary_search(&property_name).is_ok()
}

/// Lucene's `StringField`: untokenized, `DOCS_ONLY`, norms omitted.
fn string_field(name: &str, value: &str, stored: bool) -> Field {
    let mut field = Field::indexed(
        name,
        IndexOptions::Documents,
        vec![Token {
            bytes: value.as_bytes().to_vec(),
            position_increment: 1,
            start_offset: 0,
            end_offset: value.len() as u32,
        }],
    );
    field.omit_norms = true;
    field.final_offset = value.len() as u32;
    if stored {
        field.stored = Some(StoredValue::Text(value.to_owned()));
    }
    field
}

/// Lucene's `TextField`: tokenized, positions, **norms kept**. Oak uses it
/// for `:fulltext`, `fullnode:<path>` and `:ancestors`.
fn text_field(name: &str, analyzed: &TokenStreamResult, stored: Option<&str>) -> Field {
    let mut field = tokenized(
        name,
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        analyzed,
    );
    field.omit_norms = false;
    if let Some(value) = stored {
        field.stored = Some(StoredValue::Text(value.to_owned()));
    }
    field
}

/// `FieldFactory.OAK_TYPE` and `OAK_TYPE_NOT_STORED`: tokenized, **norms
/// omitted**, and offsets exactly when the field is stored. Oak uses them
/// for `full:<name>`, `:suggest` and `:spellcheck`.
fn oak_text_field(name: &str, analyzed: &TokenStreamResult, stored: Option<&str>) -> Field {
    let options = if stored.is_some() {
        IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets
    } else {
        IndexOptions::DocumentsAndFrequenciesAndPositions
    };
    let mut field = tokenized(name, options, analyzed);
    field.omit_norms = true;
    if let Some(value) = stored {
        field.stored = Some(StoredValue::Text(value.to_owned()));
    }
    field
}

/// A field over an analyzed value, carrying the stream's own end state.
fn tokenized(name: &str, options: IndexOptions, analyzed: &TokenStreamResult) -> Field {
    let mut field = Field::indexed(
        name,
        options,
        analyzed
            .tokens
            .iter()
            .map(|token| Token {
                bytes: token.term.as_bytes().to_vec(),
                position_increment: token.position_increment,
                start_offset: token.start_offset,
                end_offset: token.end_offset,
            })
            .collect(),
    );
    field.final_position_increment = analyzed.final_position_increment;
    field.final_offset = analyzed.final_offset;
    field
}

/// A numeric field: every shift level of the trie at one position,
/// `DOCS_ONLY`, norms omitted.
fn numeric_field(name: &str, terms: Vec<NumericTerm>) -> Field {
    let mut field = Field::indexed(
        name,
        IndexOptions::Documents,
        terms
            .into_iter()
            .map(|term| Token {
                bytes: term.bytes,
                position_increment: term.position_increment,
                start_offset: 0,
                end_offset: 0,
            })
            .collect(),
    );
    field.omit_norms = true;
    field.final_offset = 0;
    field
}

/// A field carrying a doc value and nothing else.
fn doc_value_field(name: &str, value: DocValue) -> Field {
    Field {
        name: name.to_owned(),
        options: IndexOptions::Documents,
        indexed: false,
        tokens: Vec::new(),
        final_position_increment: 0,
        final_offset: 0,
        stored: None,
        doc_value: Some(value),
        omit_norms: true,
        boost: 1.0,
    }
}

// ----------------------------------------------------------- the branches

impl DocumentMaker<'_> {
    /// §3.3: the node's own properties in the order the node state yields
    /// them, then the synthetic `:nodeName`.
    fn index_properties(
        &self,
        node: &NodeState<'_>,
        path: &str,
        rule: &IndexingRule,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        let has_mime_type = node.property("jcr:mimeType")?.is_some();
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let node_name_property = PropertyState {
            name: NODE_NAME_FIELD.to_owned(),
            property_type: PropertyType::String,
            values: PropertyValues::Single(PropertyValue::String(name)),
        };
        let mut properties = node.properties()?;
        properties.push(node_name_property);
        for property in &properties {
            // `isVisible` is `charAt(0) != ':'`, and the one exception is
            // the property the maker itself added.
            if property.name.starts_with(':') && property.name != NODE_NAME_FIELD {
                continue;
            }
            let Some(definition) = rule.config_of(&property.name) else {
                continue;
            };
            if !definition.index {
                continue;
            }
            if definition.ordered {
                self.index_ordered(property, definition, state)?;
            }
            self.index_property(property, definition, rule, has_mime_type, state)?;
        }
        Ok(())
    }

    /// §3.5: the ordered doc value, under `:dv<name>`.
    fn index_ordered(
        &self,
        property: &PropertyState,
        definition: &PropertyDefinition,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        let PropertyValues::Single(value) = &property.values else {
            // Ordered doc values are single-valued: Oak warns and adds
            // nothing at all, the typed and analyzed fields unaffected.
            return Ok(());
        };
        if !self.includes_property_value(property, value, definition) {
            return Ok(());
        }
        let name = format!("{DOC_VALUE_PREFIX}{}", property.name);
        if state.doc_value_names.contains(&name) {
            // `if (doc.getField(f.name()) == null)`: a duplicate is
            // dropped, not overwritten.
            return Ok(());
        }
        // The type is the **rule's** declared type where it has one, not
        // the property's.
        let declared = definition
            .declared_type
            .as_deref()
            .and_then(type_of_name)
            .unwrap_or(property.property_type);
        let doc_value = match declared {
            PropertyType::Long => value
                .as_text()
                .and_then(|text| text.parse().ok())
                .map(DocValue::Numeric),
            PropertyType::Date => Some(DocValue::Numeric(Self::date_value(
                value,
                &property.name,
                &state.path,
            )?)),
            PropertyType::Double => value
                .as_text()
                .and_then(|text| text.parse::<f64>().ok())
                // The doc value is the **raw bits**, where the indexed
                // term is the sortable long.
                .map(|number| DocValue::Numeric(number.to_bits() as i64)),
            PropertyType::Boolean => value
                .as_text()
                .map(|text| DocValue::Sorted(text.into_bytes())),
            PropertyType::String => value
                .as_text()
                .map(|text| DocValue::Sorted(truncated_doc_value(&text))),
            _ => None,
        };
        if let Some(doc_value) = doc_value {
            state.doc_value_names.insert(name.clone());
            state.add_dirty(doc_value_field(&name, doc_value));
        }
        Ok(())
    }

    /// §3.3's `indexProperty`: the typed fields, then the per-value
    /// analyzed, suggest, spellcheck and fulltext loop, then the facet.
    fn index_property(
        &self,
        property: &PropertyState,
        definition: &PropertyDefinition,
        rule: &IndexingRule,
        has_mime_type: bool,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        let included_type = Self::includes_property_type(rule, property.property_type);
        if property.property_type == PropertyType::Binary {
            if included_type && definition.fulltext_enabled() {
                self.index_binary(property, has_mime_type, &[FULLTEXT_FIELD], state);
            }
            // A binary never reaches the per-value loop below.
            return Ok(());
        }
        if definition.property_index && included_type {
            self.index_typed(property, definition, state)?;
        }
        if definition.fulltext_enabled() && included_type {
            for value in values_of(property) {
                let Some(text) = value.as_text() else {
                    continue;
                };
                if !self.includes_value(&text, definition) {
                    continue;
                }
                if definition.analyzed {
                    self.index_analyzed(&property.name, &text, definition, state);
                }
                if definition.use_in_suggest {
                    Self::index_suggest(&text, state);
                }
                if definition.use_in_spellcheck {
                    self.index_spellcheck(&text, state);
                }
                if definition.node_scope_index {
                    self.index_fulltext(FULLTEXT_FIELD, &text, None, state);
                }
                state.dirty = true;
            }
        }
        if definition.facet {
            Self::index_facet(property, state);
        }
        Ok(())
    }
}

impl DocumentMaker<'_> {
    /// §3.6: one typed field per value that passes the property form of
    /// the inclusion test, under the property's own name.
    fn index_typed(
        &self,
        property: &PropertyState,
        definition: &PropertyDefinition,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        for value in values_of(property) {
            if !self.includes_property_value(property, value, definition) {
                continue;
            }
            let field = match property.property_type {
                PropertyType::Long => value
                    .as_text()
                    .and_then(|text| text.parse::<i64>().ok())
                    .map(|number| numeric_field(&property.name, long_terms(number))),
                PropertyType::Date => Some(numeric_field(
                    &property.name,
                    long_terms(Self::date_value(value, &property.name, &state.path)?),
                )),
                PropertyType::Double => value
                    .as_text()
                    .and_then(|text| text.parse::<f64>().ok())
                    .map(|number| numeric_field(&property.name, double_terms(number))),
                // A binary yields no field at all: Oak never calls
                // `getValue(Type.STRING)` on one.
                PropertyType::Binary => None,
                _ => value
                    .as_text()
                    .map(|text| string_field(&property.name, &text, false)),
            };
            if let Some(field) = field {
                state.add_dirty(field);
            }
        }
        Ok(())
    }

    /// `full:<name>`, analyzed, stored exactly when `useInExcerpt` — unless
    /// the name is one `skipTokenization` refuses to tokenize.
    ///
    /// `LuceneDocumentMaker.indexAnalyzedProperty` passes
    /// `!pd.skipTokenization(pname)` as `newPropertyField`'s `tokenized`
    /// flag, and that factory ignores its `stored` argument entirely when
    /// the flag is false:
    ///
    /// ```java
    /// public static Field newPropertyField(String name, String value, boolean tokenized, boolean stored) {
    ///     if (tokenized) return new OakTextField(name, value, stored);
    ///     return new StringField(name, value, Field.Store.NO);
    /// }
    /// ```
    ///
    /// So such a name yields one untokenized `DOCS_ONLY` term of the whole
    /// value, unstored whatever `useInExcerpt` says — §3.3.
    fn index_analyzed(
        &self,
        property_name: &str,
        value: &str,
        definition: &PropertyDefinition,
        state: &mut DocumentState,
    ) {
        let name = format!("{ANALYZED_PREFIX}{property_name}");
        if skip_tokenization(property_name, definition) {
            state.add_dirty(string_field(&name, value, false));
            return;
        }
        let analyzed = self.analyzer.tokens(&name, value);
        // **No boost.** `indexAnalyzedProperty` passes the value and the
        // two flags to `newPropertyField` and nothing else, and the field
        // it builds is `FieldFactory.OAK_TYPE`, which omits norms — so
        // Lucene's own `Field.setBoost` would throw on it. The one place
        // Oak sets a property definition's boost is the aggregate value
        // below, whose `TextField` keeps norms. §3.2 and §4.
        state.add_dirty(oak_text_field(
            &name,
            &analyzed,
            definition.use_in_excerpt.then_some(value),
        ));
    }

    /// `:suggest`, which the writer configuration analyzes with the
    /// suggest helper's newline tokenizer unless `suggestAnalyzed`.
    fn index_suggest(value: &str, state: &mut DocumentState) {
        state.suggest_values.push(value.to_owned());
        state.dirty = true;
    }

    /// `:spellcheck`, which it analyzes with Oak's own analyzer under a
    /// shingle filter.
    fn index_spellcheck(&self, value: &str, state: &mut DocumentState) {
        let analyzed = self.analyzer.tokens(field_names::SPELLCHECK, value);
        state.add_dirty(oak_text_field(field_names::SPELLCHECK, &analyzed, None));
    }

    /// `:fulltext`, or `fullnode:<path>` for a relative-node aggregate.
    /// Stored only for binary text, which is the one caller that passes a
    /// value to store.
    fn index_fulltext(
        &self,
        name: &str,
        value: &str,
        stored: Option<&str>,
        state: &mut DocumentState,
    ) {
        let analyzed = self.analyzer.tokens(name, value);
        state.add_dirty(text_field(name, &analyzed, stored));
    }

    /// §6: a binary's text, which is **stored** where a `nodeScopeIndex`
    /// value is not.
    fn index_binary(
        &self,
        property: &PropertyState,
        has_mime_type: bool,
        names: &[&str],
        state: &mut DocumentState,
    ) {
        for value in values_of(property) {
            let PropertyValue::Binary(binary) = value else {
                continue;
            };
            if let Some(text) = self.binaries.text_of(binary, has_mime_type) {
                // The same field set an aggregated *text* value takes, a
                // `relativeNode` include's `fullnode:<path>` beside
                // `:fulltext` rather than instead of it. The fixture
                // carries no binary under an aggregate, so only the text
                // half of that is pinned by Oak's own rebuild.
                for name in names {
                    self.index_fulltext(name, &text, Some(&text), state);
                }
            }
        }
        // Oak's binary branch marks the document dirty whatever the
        // extraction returned, so a node whose only indexable property is
        // an unextractable binary still yields a document.
        state.dirty = true;
    }

    /// §5.1: one facet value per non-empty value, which the build pass
    /// turns into three fields.
    ///
    /// Both of `indexFacetProperty`'s arms test the property's **type
    /// tag**:
    ///
    /// ```java
    /// if (tag == Type.STRINGS.tag() && property.isArray()) { … } else if (tag == Type.STRING.tag()) { … }
    /// ```
    ///
    /// and `Type.STRINGS.tag()` is `Type.STRING.tag()` — the array shares
    /// its scalar's tag. So a faceted property of any other type adds no
    /// facet field at all, while the configuration is still consulted for
    /// it and its `facets` node still appears.
    fn index_facet(property: &PropertyState, state: &mut DocumentState) {
        let string_valued = property.property_type == PropertyType::String;
        let multi_valued = matches!(property.values, PropertyValues::Multiple(_)) && string_valued;
        // The configuration is consulted for every facet property, so the
        // dimension is recorded before the type test that keeps a
        // non-string one from contributing a field.
        state.dimensions.push(FacetDimension {
            name: property.name.clone(),
            multi_valued,
        });
        if !string_valued {
            return;
        }
        let mut values = Vec::new();
        for value in values_of(property) {
            let Some(text) = value.as_text() else {
                continue;
            };
            if text.is_empty() {
                // An empty value is skipped rather than refused.
                continue;
            }
            values.push(FacetValue {
                dimension: property.name.clone(),
                label: text,
            });
        }
        if values.is_empty() {
            return;
        }
        state.facet = true;
        for value in values {
            state.add_dirty(pending_facet_field(&property.name, value));
        }
    }
}

/// A facet value, parked under its dimension until the build pass turns
/// it into three fields. Oak's own `SortedSetDocValuesFacetField` is the
/// same placeholder.
fn pending_facet_field(property_name: &str, value: FacetValue) -> Field {
    let mut field = Field::stored(
        format!("{FACET_PENDING_PREFIX}{property_name}"),
        StoredValue::Text(value.label),
    );
    field.indexed = false;
    field
}

/// The name a parked facet value carries, which no document ever holds:
/// the build pass replaces every one of them.
const FACET_PENDING_PREFIX: &str = "\u{0}facet:";

/// Oak's `Type.fromString` for the names a `type` property carries.
fn type_of_name(name: &str) -> Option<PropertyType> {
    match name {
        "String" => Some(PropertyType::String),
        "Binary" => Some(PropertyType::Binary),
        "Long" => Some(PropertyType::Long),
        "Double" => Some(PropertyType::Double),
        "Date" => Some(PropertyType::Date),
        "Boolean" => Some(PropertyType::Boolean),
        "Name" => Some(PropertyType::Name),
        "Path" => Some(PropertyType::Path),
        "Reference" => Some(PropertyType::Reference),
        "WeakReference" => Some(PropertyType::WeakReference),
        "URI" => Some(PropertyType::Uri),
        "Decimal" => Some(PropertyType::Decimal),
        _ => None,
    }
}

/// Every value of a property, single or multiple.
fn values_of(property: &PropertyState) -> &[PropertyValue] {
    match &property.values {
        PropertyValues::Single(value) => std::slice::from_ref(value),
        PropertyValues::Multiple(values) => values,
    }
}

impl DocumentMaker<'_> {
    /// §3.8: `:nullProps` for every `nullCheckEnabled` property the node
    /// lacks, `:notNullProps` for every `notNullCheckEnabled` one it has.
    fn index_markers(
        node: &NodeState<'_>,
        rule: &IndexingRule,
        state: &mut DocumentState,
    ) -> IndexResult<()> {
        for definition in rule.definitions() {
            if definition.null_check_enabled && property_for(node, definition)?.is_none() {
                state.add_dirty(string_field(NULL_PROPERTIES_FIELD, &definition.name, false));
            }
        }
        for definition in rule.definitions() {
            if definition.not_null_check_enabled && property_for(node, definition)?.is_some() {
                state.add_dirty(string_field(
                    NOT_NULL_PROPERTIES_FIELD,
                    &definition.name,
                    false,
                ));
            }
        }
        Ok(())
    }

    /// §3.7: the `:nodeName` term, and the node name's own `:fulltext`
    /// value.
    fn index_node_name(&self, path: &str, rule: &IndexingRule, state: &mut DocumentState) {
        let name = path.rsplit('/').next().unwrap_or(path);
        if rule.fulltext_enabled {
            // The definition-level `valueRegex` would gate this value, and
            // a definition carrying one is refused at load.
            self.index_fulltext(FULLTEXT_FIELD, name, None, state);
        }
    }

    /// `addNodeNameField`, which runs **before** the empty-document check
    /// and forces the document dirty: a node under a rule that indexes the
    /// node name yields a document even with nothing else in it.
    fn add_node_name_field(path: &str, rule: &IndexingRule, state: &mut DocumentState) {
        if !rule.node_name_indexed {
            return;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        // Everything up to and including the first colon is stripped, so
        // `jcr:content` yields the term `content` while the node name's
        // own `:fulltext` value keeps `jcr:content`.
        let value = name.split_once(':').map_or(name, |(_, rest)| rest);
        state.add_dirty(string_field(NODE_NAME_FIELD, value, false));
    }

    /// §3.8's last branch: `:ancestors` over the node's **parent** path
    /// and `:depth` over its own.
    fn index_ancestors(&self, path: &str, state: &mut DocumentState) {
        let parent = parent_path_of(path);
        let analyzed = self.analyzer.tokens(field_names::ANCESTORS, &parent);
        state.add(text_field(field_names::ANCESTORS, &analyzed, None));
        let depth = i32::try_from(path.split('/').filter(|step| !step.is_empty()).count())
            .unwrap_or(i32::MAX);
        state.add(numeric_field(DEPTH_FIELD, integer_terms(depth)));
    }

    /// §3.9: the facet build pass, then the merged `:suggest`.
    ///
    /// `finalizeDoc` ignores the dirty flag — the two early returns above
    /// are the only ones — so a rule that indexes every node of its type
    /// writes a document holding `:path` and the node name alone.
    fn finalize(&self, state: DocumentState) -> MadeDocument {
        let DocumentState {
            fields,
            facet,
            dimensions,
            suggest_values,
            ..
        } = state;
        let mut fields = if facet { build_facets(fields) } else { fields };
        // LUCENE-5833: every `:suggest` value is joined with a newline and
        // analyzed once, so the field is the document's last and the
        // suggest tokenizer splits the values apart again.
        if !suggest_values.is_empty() {
            let joined = suggest_values.join("\n");
            let analyzed = self.analyzer.tokens(field_names::SUGGEST, &joined);
            fields.push(oak_text_field(field_names::SUGGEST, &analyzed, None));
        }
        MadeDocument {
            document: Document { fields },
            facet_dimensions: dimensions,
        }
    }
}

/// The property a definition names, through its ancestors when the name
/// is relative — `FulltextDocumentMaker.relativeProperty`, which walks the
/// subtree rather than looking a slash-bearing name up as a property.
fn property_for(
    node: &NodeState<'_>,
    definition: &PropertyDefinition,
) -> IndexResult<Option<PropertyState>> {
    let mut current = *node;
    for ancestor in &definition.ancestors {
        let Some(child) = current.child_node(ancestor)? else {
            return Ok(None);
        };
        current = child;
    }
    let name = definition
        .name
        .rsplit('/')
        .next()
        .unwrap_or(&definition.name);
    Ok(current.property(name)?)
}

/// Oak's `PathUtils.getParentPath`.
fn parent_path_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(at) => path[..at].to_owned(),
        None => String::new(),
    }
}

/// §5.2: the build pass re-emits the document with the facet-derived
/// fields first and the rest after, in their original order.
fn build_facets(fields: Vec<Field>) -> Vec<Field> {
    let mut facet_fields = Vec::new();
    let mut rest = Vec::new();
    for field in fields {
        let Some(property_name) = field.name.strip_prefix(FACET_PENDING_PREFIX) else {
            rest.push(field);
            continue;
        };
        let Some(StoredValue::Text(label)) = field.stored else {
            continue;
        };
        let index_field = facet_field_name(property_name);
        let value = FacetValue {
            dimension: property_name.to_owned(),
            label,
        };
        // The path is refused only for an empty component, which the
        // maker already skipped.
        let Ok(path) = value.to_path("") else {
            continue;
        };
        facet_fields.push(doc_value_field(
            &index_field,
            DocValue::SortedSet(vec![path.clone().into_bytes()]),
        ));
        facet_fields.push(string_field(&index_field, &path, false));
        facet_fields.push(string_field(&index_field, &value.dimension, false));
    }
    facet_fields.extend(rest);
    facet_fields
}

impl DocumentMaker<'_> {
    /// §3.4's **property form**: a binary and a match-all pattern are
    /// included outright, and every other value is matched as a string.
    fn includes_property_value(
        &self,
        property: &PropertyState,
        value: &PropertyValue,
        definition: &PropertyDefinition,
    ) -> bool {
        if property.property_type == PropertyType::Binary {
            return true;
        }
        if definition.value_pattern.matches_all() {
            return true;
        }
        value
            .as_text()
            .is_some_and(|text| self.includes_value(&text, definition))
    }

    /// §3.4's **bare form**, which is the same pattern over a value that
    /// is already a string.
    fn includes_value(&self, value: &str, definition: &PropertyDefinition) -> bool {
        definition
            .value_pattern
            .matches(value, self.definition_path)
            .unwrap_or(true)
    }

    /// The epoch millisecond of a `DATE` value, which is the one refusal
    /// this stage raises.
    fn date_value(value: &PropertyValue, property_name: &str, path: &str) -> IndexResult<i64> {
        let text = value.as_text().unwrap_or_default();
        date_to_long(&text).map_err(|_| IndexError::UnparseableDate {
            value: format!("{path}@{property_name} = {text:?}"),
        })
    }

    /// `IndexingRule.includePropertyType`: the rule's
    /// `includePropertyTypes`, which defaults to all of them.
    fn includes_property_type(rule: &IndexingRule, property_type: PropertyType) -> bool {
        if rule.include_property_types.is_empty() {
            return true;
        }
        rule.include_property_types
            .iter()
            .any(|name| type_of_name(name) == Some(property_type))
    }
}
