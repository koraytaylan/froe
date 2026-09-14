//! The rest of §3 — the node name, the markers, the empty-document
//! check and finalization — with §4's aggregates, §5's facets and §6's
//! binaries.

use super::*;

/// The node-name term strips everything up to and including the first
/// colon, and forces a document for a node with nothing else in it.
#[test]
fn the_node_name_term_is_the_name_after_its_colon() {
    let mut definition = definition_with(vec![("title", analyzed("jcr:title"))]);
    // `indexNodeName` on the rule.
    definition.children[0].1.children[0].1 = definition.children[0].1.children[0]
        .1
        .clone()
        .boolean("indexNodeName", true);
    let subject = unstructured();
    let directory = TestDirectory::new("node-name");
    // The subject is written at `/content/page`, so the name to strip has
    // to come from the path the maker is given.
    publish(&directory.path, &definition, &subject);
    let repository = Repository::open(&directory.path).expect("open the repository");
    let root = repository.content_root().expect("the content root");
    let node = repository
        .node_at_path("/oak:index/test")
        .expect("resolve the definition")
        .expect("the definition exists");
    let mut warnings: Vec<IndexWarning> = Vec::new();
    let rules = IndexingRules::read(&node, "/oak:index/test", &root, &mut warnings)
        .expect("the definition reads");
    let page = repository
        .node_at_path("/content/page")
        .expect("resolve the node")
        .expect("the node exists");
    let rule = rules
        .applicable_rule(&page)
        .expect("resolve a rule")
        .expect("a rule applies")
        .clone();
    let maker = DocumentMaker::new("/oak:index/test", &rules, marker_policy());
    let made = maker
        .make(&page, "/content/jcr:content", &rule)
        .expect("make the document")
        .expect("the node name forces a document");
    assert_eq!(names(&made), vec![":path", ":nodeName", ":fulltext"]);
    assert_eq!(
        made.document.fields[1].tokens[0].bytes,
        b"content".to_vec(),
        "the term is the name after its colon"
    );
    assert_eq!(
        made.document.fields[2].tokens[0].bytes,
        b"jcr".to_vec(),
        "the node name's own `:fulltext` value keeps the prefix"
    );
}

/// A rule that does not index every node of its type yields no document
/// for a node that contributed nothing.
#[test]
fn a_node_that_contributed_nothing_yields_no_document() {
    let definition = definition_with(vec![("title", analyzed("jcr:title"))]);
    let subject = unstructured();
    let (_directory, made) = make("empty", &definition, &subject, marker_policy());
    assert!(
        made.is_none(),
        "no property matched and no node name is indexed"
    );

    // With a `nodeScopeIndex` definition the rule indexes every node of
    // its type, and the empty document is written.
    let definition = definition_with(vec![(
        "title",
        analyzed("jcr:title").boolean("nodeScopeIndex", true),
    )]);
    let (_directory, made) = make("empty-all", &definition, &subject, marker_policy());
    let made = made.expect("every node of the type is indexed");
    assert_eq!(names(&made), vec![":path", ":fulltext"]);
}

/// `:nullProps` names every `nullCheckEnabled` property the node lacks;
/// `:notNullProps` every `notNullCheckEnabled` one it has.
#[test]
fn the_markers_name_the_properties() {
    // An `nt:base` rule with a `nullCheckEnabled` property is one Oak's
    // own validation refuses, so the rule is over the subject's own type.
    let definition = definition_over(
        "nt:unstructured",
        vec![
            ("title", analyzed("jcr:title")),
            (
                "absent",
                Node::new()
                    .string("name", "jcr:description")
                    .boolean("nullCheckEnabled", true),
            ),
            (
                "present",
                Node::new()
                    .string("name", "jcr:title")
                    .boolean("analyzed", true)
                    .boolean("notNullCheckEnabled", true),
            ),
        ],
    );
    let subject = unstructured().string("jcr:title", "here");
    let (_directory, made) = make("markers", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert_eq!(
        names(&made),
        vec![
            ":path",
            "full:jcr:title",
            ":nullProps",
            ":notNullProps",
            ":fulltext"
        ]
    );
    assert_eq!(
        made.document.fields[2].tokens[0].bytes,
        b"jcr:description".to_vec()
    );
    assert_eq!(
        made.document.fields[3].tokens[0].bytes,
        b"jcr:title".to_vec()
    );
}

/// §5: three fields per facet value, all under `<property>_facet`, and the
/// build pass puts them first.
#[test]
fn a_facet_property_becomes_three_fields_under_one_name() {
    let definition = definition_with(vec![
        ("title", analyzed("jcr:title")),
        (
            "tags",
            Node::new().string("name", "tags").boolean("facets", true),
        ),
    ]);
    let subject = unstructured().multiple("tags", PropertyType::String, &["red", "blue"]);
    let (_directory, made) = make("facets", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert_eq!(
        names(&made),
        vec![
            // The build pass re-emits the document with the facet-derived
            // fields first.
            "tags_facet",
            "tags_facet",
            "tags_facet",
            "tags_facet",
            "tags_facet",
            "tags_facet",
            ":path",
            ":fulltext",
        ]
    );
    // Per value: the counting doc value, the escaped full path, the bare
    // dimension.
    assert_eq!(
        made.document.fields[0].doc_value,
        Some(DocValue::SortedSet(vec![b"tags\x1fred".to_vec()]))
    );
    assert_eq!(
        made.document.fields[1].tokens[0].bytes,
        b"tags\x1fred".to_vec()
    );
    assert_eq!(made.document.fields[2].tokens[0].bytes, b"tags".to_vec());
    assert_eq!(
        made.facet_dimensions.len(),
        1,
        "the configuration learns the dimension"
    );
    assert!(made.facet_dimensions[0].multi_valued);
}

/// LUCENE-5833: every `:suggest` value is joined with a newline and
/// analyzed once, so one field holds them all and it is the document's
/// last.
#[test]
fn the_suggest_values_merge_into_one_last_field() {
    let definition = definition_with(vec![(
        "tags",
        Node::new()
            .string("name", "tags")
            .boolean("analyzed", true)
            .boolean("useInSuggest", true),
    )]);
    let subject = unstructured().multiple("tags", PropertyType::String, &["one two", "three"]);
    let (_directory, made) = make("suggest", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    let last = made.document.fields.last().expect("a last field");
    assert_eq!(last.name, ":suggest");
    // The suggest tokenizer splits on newlines alone, so each value is one
    // term however many words it holds.
    assert_eq!(
        last.tokens
            .iter()
            .map(|token| String::from_utf8_lossy(&token.bytes).into_owned())
            .collect::<Vec<_>>(),
        vec!["one two".to_owned(), "three".to_owned()]
    );
    assert_eq!(
        names(&made)
            .iter()
            .filter(|name| **name == ":suggest")
            .count(),
        1
    );
}

/// `:spellcheck` is Oak's analyzer under a shingle filter, which the
/// writer configuration installs and the definition's cap never reaches.
#[test]
fn the_spellcheck_field_is_shingled() {
    let definition = definition_with(vec![(
        "title",
        analyzed("jcr:title").boolean("useInSpellcheck", true),
    )]);
    let subject = unstructured().string("jcr:title", "Foo Bar");
    let (_directory, made) = make("spellcheck", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    let field = made
        .document
        .fields
        .iter()
        .find(|field| field.name == ":spellcheck")
        .expect("the spellcheck field");
    assert_eq!(
        field
            .tokens
            .iter()
            .map(|token| String::from_utf8_lossy(&token.bytes).into_owned())
            .collect::<Vec<_>>(),
        vec!["foo".to_owned(), "foo bar".to_owned(), "bar".to_owned()]
    );
    assert!(field.omit_norms, "an Oak text field omits norms");
}

/// §6: a binary's text is a **stored** `:fulltext` value, and the branch
/// marks the document dirty whatever the extraction returned.
#[test]
fn a_binary_contributes_a_stored_fulltext_value_or_nothing() {
    let definition = definition_with(vec![(
        "data",
        Node::new()
            .string("name", "jcr:data")
            .boolean("analyzed", true)
            .boolean("nodeScopeIndex", true),
    )]);
    let subject = unstructured().string("jcr:mimeType", "text/plain").single(
        "jcr:data",
        PropertyType::Binary,
        "some bytes",
    );
    let (_directory, made) = make("binary", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    let field = made
        .document
        .fields
        .iter()
        .find(|field| field.name == ":fulltext")
        .expect("the fulltext field");
    assert_eq!(
        field.stored,
        Some(StoredValue::Text("TextExtractionError".to_owned())),
        "the marker takes the slot Oak's extracted text would"
    );

    // Under the skipping fallback the field is absent, and the document is
    // still written: the binary branch is dirty either way.
    let (_directory, made) = make(
        "binary-skip",
        &definition,
        &subject,
        BinaryTextPolicy::new(BinaryTextFallback::Skip),
    );
    let made = made.expect("the binary marks the document dirty even so");
    // The only `:fulltext` left is the node name's own value, unstored.
    assert_eq!(names(&made), vec![":path", ":fulltext"]);
    assert_eq!(made.document.fields[1].stored, None);

    // Without `jcr:mimeType` there is no text either way, which is the
    // gate Oak's own extraction applies first.
    let without_type = unstructured().single("jcr:data", PropertyType::Binary, "some bytes");
    let (_directory, made) = make(
        "binary-untyped",
        &definition,
        &without_type,
        marker_policy(),
    );
    let made = made.expect("the binary marks the document dirty");
    assert_eq!(names(&made), vec![":path", ":fulltext"]);
    assert_eq!(made.document.fields[1].stored, None);
}

/// §4: an aggregated node's values land in `:fulltext`, or in
/// `fullnode:<include path>` for a `relativeNode` include.
#[test]
fn an_aggregate_contributes_its_childrens_values() {
    let mut definition = definition_with(vec![("title", analyzed("jcr:title"))]);
    definition = definition.child(
        "aggregates",
        Node::new().child(
            "nt:base",
            Node::new().child("include0", Node::new().string("path", "jcr:content")),
        ),
    );
    let subject = unstructured().child(
        "jcr:content",
        unstructured().string("jcr:title", "inner text"),
    );
    let (_directory, made) = make("aggregate", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert_eq!(
        names(&made),
        vec![":path", ":fulltext", ":fulltext", ":fulltext"],
        "the aggregated primary type, its title, and the node name"
    );

    // A `relativeNode` include moves the values to their own field.
    let mut relative = definition_with(vec![("title", analyzed("jcr:title"))]);
    relative = relative.child(
        "aggregates",
        Node::new().child(
            "nt:base",
            Node::new().child(
                "include0",
                Node::new()
                    .string("path", "jcr:content")
                    .boolean("relativeNode", true),
            ),
        ),
    );
    let (_directory, made) = make("aggregate-relative", &relative, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert_eq!(
        names(&made),
        vec![
            ":path",
            "fullnode:jcr:content",
            "fullnode:jcr:content",
            ":fulltext"
        ]
    );
}

/// §3.5: the doc value's type is the **rule's** declared type, not the
/// property's.
#[test]
fn an_ordered_doc_value_takes_the_rules_declared_type() {
    let definition = definition_with(vec![
        ("title", analyzed("jcr:title")),
        (
            "count",
            Node::new()
                .string("name", "count")
                .boolean("ordered", true)
                .string("type", "String"),
        ),
    ]);
    let subject = unstructured().single("count", PropertyType::Long, "42");
    let (_directory, made) = make("declared-type", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert_eq!(
        made.document.fields[1].doc_value,
        Some(DocValue::Sorted(b"42".to_vec())),
        "a LONG property typed String by the rule is a sorted doc value"
    );
}

/// `isVisible` is `charAt(0) != ':'`, and the one exception is the
/// synthetic `:nodeName` the maker itself added.
#[test]
fn a_hidden_property_is_skipped_and_the_node_name_is_not() {
    let subject = unstructured().string(":hidden", "invisible");
    let (_directory, made) = make("hidden", &default_definition(), &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert!(
        !names(&made).contains(&"full::hidden"),
        "{:?}",
        names(&made)
    );
    assert!(
        names(&made).contains(&"full::nodeName"),
        "the synthetic property is the exception: {:?}",
        names(&made)
    );
}

/// The binary branch marks the document dirty whatever the extraction
/// returned, so a node whose only indexable property is an unextractable
/// binary still yields a document — under a rule that does **not** index
/// every node of its type, where nothing else would.
#[test]
fn an_unextractable_binary_still_yields_a_document() {
    let definition = definition_with(vec![(
        "data",
        Node::new()
            .string("name", "jcr:data")
            .boolean("analyzed", true),
    )]);
    let subject = unstructured()
        .string("jcr:mimeType", "application/octet-stream")
        .single("jcr:data", PropertyType::Binary, "some bytes");
    let (_directory, made) = make(
        "binary-dirty",
        &definition,
        &subject,
        BinaryTextPolicy::new(BinaryTextFallback::Skip),
    );
    let made = made.expect("the binary alone makes the document");
    assert_eq!(
        names(&made),
        vec![":path", ":fulltext"],
        "only the node name's own value, which follows the empty check"
    );
}

/// A relative name is resolved by walking the subtree, not by looking a
/// slash-bearing name up as a property.
#[test]
fn a_relative_marker_walks_its_ancestors() {
    let definition = definition_over(
        "nt:unstructured",
        vec![
            ("title", analyzed("jcr:title")),
            (
                "inner",
                Node::new()
                    .string("name", "jcr:content/jcr:title")
                    .boolean("notNullCheckEnabled", true),
            ),
        ],
    );
    let subject = unstructured().child(
        "jcr:content",
        unstructured().string("jcr:title", "inner text"),
    );
    let (_directory, made) = make("relative-marker", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert!(
        names(&made).contains(&":notNullProps"),
        "the relative property was found: {:?}",
        names(&made)
    );
}
