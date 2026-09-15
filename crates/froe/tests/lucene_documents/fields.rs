//! The field kinds of §3.1 and §3.2, and the per-property pass of §3.3:
//! what one property becomes, in what order, with which options.

use super::*;

/// The worked example of §8, one property at a time: `:path` first, then
/// the property's own analyzed field and its `:fulltext` value, then the
/// synthetic `:nodeName`'s pair, then the node name's own `:fulltext`
/// value, then the ancestors.
#[test]
fn the_default_definition_writes_the_worked_example() {
    let subject = Node::new()
        .single("jcr:primaryType", PropertyType::Name, "nt:unstructured")
        .string("jcr:title", "Hello World");
    let (_directory, made) = make("default", &default_definition(), &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert_eq!(
        names(&made),
        vec![
            ":path",
            // `jcr:primaryType` reaches the catch-all too, being an
            // ordinary visible property.
            "full:jcr:primaryType",
            ":fulltext",
            "full:jcr:title",
            ":fulltext",
            "full::nodeName",
            ":fulltext",
            // The node name's own value, from the third branch of §3.7.
            ":fulltext",
            ":ancestors",
            ":depth",
        ]
    );
    // `:path` is a stored, untokenized term.
    assert_eq!(
        describe(&made.document.fields[0]),
        (
            ":path".to_owned(),
            IndexOptions::Documents,
            true,
            false,
            vec!["/content/page".to_owned()]
        )
    );
    // An analyzed property field omits norms and, unstored, carries
    // positions without offsets.
    assert_eq!(
        describe(&made.document.fields[3]),
        (
            "full:jcr:title".to_owned(),
            IndexOptions::DocumentsAndFrequenciesAndPositions,
            false,
            false,
            vec!["hello".to_owned(), "world".to_owned()]
        )
    );
    // `:fulltext` keeps norms.
    assert_eq!(
        describe(&made.document.fields[4]),
        (
            ":fulltext".to_owned(),
            IndexOptions::DocumentsAndFrequenciesAndPositions,
            false,
            true,
            vec!["hello".to_owned(), "world".to_owned()]
        )
    );
    // `:ancestors` is the **parent** path through the path-hierarchy
    // chain, and `:depth` the node's own depth.
    assert_eq!(
        describe(&made.document.fields[8]),
        (
            ":ancestors".to_owned(),
            IndexOptions::DocumentsAndFrequenciesAndPositions,
            false,
            true,
            vec!["/content".to_owned()]
        )
    );
    assert_eq!(made.document.fields[9].name, ":depth");
    assert_eq!(
        made.document.fields[9].tokens.len(),
        8,
        "eight shift levels"
    );
}

/// `useInExcerpt` stores the value, and a stored analyzed field carries
/// offsets where the unstored one does not.
#[test]
fn use_in_excerpt_stores_the_value_and_adds_offsets() {
    let definition = Node::new()
        .single(
            "jcr:primaryType",
            PropertyType::Name,
            "oak:QueryIndexDefinition",
        )
        .string("type", "lucene")
        .child(
            "indexRules",
            Node::new().child(
                "nt:base",
                Node::new().child(
                    "properties",
                    Node::new().child(
                        "title",
                        Node::new()
                            .string("name", "jcr:title")
                            .boolean("analyzed", true)
                            .boolean("useInExcerpt", true),
                    ),
                ),
            ),
        );
    let subject = Node::new()
        .single("jcr:primaryType", PropertyType::Name, "nt:unstructured")
        .string("jcr:title", "Hello");
    let (_directory, made) = make("excerpt", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert_eq!(
        describe(&made.document.fields[1]),
        (
            "full:jcr:title".to_owned(),
            IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets,
            true,
            false,
            vec!["hello".to_owned()]
        )
    );
    assert_eq!(
        made.document.fields[1].stored,
        Some(StoredValue::Text("Hello".to_owned()))
    );
}

#[test]
fn a_typed_property_and_its_ordered_doc_value() {
    let definition = definition_with(vec![
        ("title", analyzed("jcr:title")),
        (
            "count",
            Node::new()
                .string("name", "count")
                .boolean("propertyIndex", true)
                .boolean("ordered", true)
                .string("type", "Long"),
        ),
    ]);
    let subject = unstructured().single("count", PropertyType::Long, "42");
    let (_directory, made) = make("typed", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    // The last `:fulltext` is the node name's own value, which every
    // fulltext-enabled rule adds.
    assert_eq!(
        names(&made),
        vec![":path", ":dvcount", "count", ":fulltext"]
    );
    // The doc value comes **before** the typed field.
    assert_eq!(
        made.document.fields[1].doc_value,
        Some(DocValue::Numeric(42))
    );
    // The typed field is the trie, at one position, with no offsets.
    assert_eq!(made.document.fields[2].options, IndexOptions::Documents);
    assert_eq!(made.document.fields[2].tokens.len(), 16);
    assert!(made.document.fields[2].omit_norms);
}

/// An ordered doc value is single-valued: a multi-valued property yields
/// none at all, while its typed fields are unaffected.
#[test]
fn a_multi_valued_ordered_property_yields_no_doc_value() {
    let definition = definition_with(vec![(
        "tags",
        Node::new()
            .string("name", "tags")
            .boolean("propertyIndex", true)
            .boolean("ordered", true)
            .boolean("analyzed", true),
    )]);
    let subject = unstructured().multiple("tags", PropertyType::String, &["a", "b"]);
    let (_directory, made) = make("multi-ordered", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert!(
        !names(&made).contains(&":dvtags"),
        "no doc value: {:?}",
        names(&made)
    );
    assert_eq!(
        names(&made),
        vec![
            ":path",
            "tags",
            "tags",
            "full:tags",
            "full:tags",
            ":fulltext"
        ]
    );
}

/// The value pattern's prefixes gate every branch: an excluded value
/// contributes no typed, ordered, analyzed, suggest, spellcheck or
/// fulltext field, while its sibling contributes all of them.
#[test]
fn an_excluded_value_contributes_no_field_of_any_kind() {
    let definition = definition_with(vec![(
        "tags",
        Node::new()
            .string("name", "tags")
            .boolean("propertyIndex", true)
            .boolean("analyzed", true)
            .boolean("nodeScopeIndex", true)
            .boolean("useInSuggest", true)
            .boolean("useInSpellcheck", true)
            .multiple("valueIncludedPrefixes", PropertyType::String, &["keep"]),
    )]);
    let subject = unstructured().multiple("tags", PropertyType::String, &["keepme", "dropme"]);
    let (_directory, made) = make("excluded", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    assert_eq!(
        names(&made),
        vec![
            ":path",
            "tags",
            "full:tags",
            ":spellcheck",
            // The included value's own `:fulltext`, then the node name's.
            ":fulltext",
            ":fulltext",
            ":suggest"
        ]
    );
    assert_eq!(
        made.document.fields[1].tokens[0].bytes,
        b"keepme".to_vec(),
        "the included value is the only typed field"
    );
}

/// The one refusal this stage raises, which Oak's own path also fails on.
#[test]
fn an_unparseable_date_is_refused_by_name() {
    let definition = definition_with(vec![(
        "created",
        Node::new()
            .string("name", "jcr:created")
            .boolean("propertyIndex", true)
            .boolean("analyzed", true),
    )]);
    let subject = unstructured().single("jcr:created", PropertyType::Date, "not a date");
    let directory = TestDirectory::new("bad-date");
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
    let Err(refusal) = maker.make(&page, "/content/page", &rule) else {
        panic!("an unparseable date is refused");
    };
    let message = refusal.to_string();
    assert!(message.contains("/content/page"), "{message}");
    assert!(message.contains("jcr:created"), "{message}");
    assert!(message.contains("not a date"), "{message}");
}

/// §3.3's `skipTokenization`: a **regular-expression** definition never
/// tokenizes one of `IndexHelper.NOT_TOKENIZED`, and the field it writes
/// instead is `newPropertyField`'s untokenized arm — one `DOCS_ONLY` term
/// of the whole value, unstored whatever `useInExcerpt` says.
///
/// Oak's own rebuild of the interop fixture writes `full:jcr:uuid` as
/// `DOCS_ONLY`, which is what caught this: every `jcr:uuid` in a Sling
/// repository reaches the default definition's catch-all pattern.
#[test]
fn a_regular_expression_definition_does_not_tokenize_the_names_oak_excludes() {
    let definition = definition_with(vec![(
        "all",
        Node::new()
            .string("name", ALL_PROPERTIES)
            .boolean("isRegexp", true)
            .boolean("analyzed", true)
            .boolean("useInExcerpt", true),
    )]);
    let subject = Node::new()
        .single("jcr:primaryType", PropertyType::Name, "nt:unstructured")
        .string("jcr:uuid", "b4f8474f-885f-4725-a389-ce6deb1c3532")
        .string("jcr:title", "Hello World");
    let (_directory, made) = make("not-tokenized", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");

    let uuid = made
        .document
        .fields
        .iter()
        .find(|field| field.name == "full:jcr:uuid")
        .expect("the excluded name still reaches the catch-all");
    assert_eq!(
        describe(uuid),
        (
            "full:jcr:uuid".to_owned(),
            IndexOptions::Documents,
            // Unstored, because the untokenized arm passes `Store.NO`
            // whatever `useInExcerpt` said.
            false,
            false,
            vec!["b4f8474f-885f-4725-a389-ce6deb1c3532".to_owned()]
        )
    );
    // Every other name under the same pattern is analyzed as before, so
    // this is the name list and not the pattern.
    let title = made
        .document
        .fields
        .iter()
        .find(|field| field.name == "full:jcr:title")
        .expect("an ordinary name is still analyzed");
    assert_eq!(
        describe(title),
        (
            "full:jcr:title".to_owned(),
            IndexOptions::DocumentsAndFrequenciesAndPositionsAndOffsets,
            true,
            false,
            vec!["hello".to_owned(), "world".to_owned()]
        )
    );
}

/// The same name under a definition that names it outright **is**
/// tokenized: `skipTokenization`'s first arm is guarded by `isRegexp`.
#[test]
fn a_named_definition_tokenizes_a_name_the_pattern_arm_would_exclude() {
    let definition = definition_with(vec![("uuid", analyzed("jcr:uuid"))]);
    let subject = Node::new()
        .single("jcr:primaryType", PropertyType::Name, "nt:unstructured")
        .string("jcr:uuid", "b4f8474f-885f-4725-a389-ce6deb1c3532");
    let (_directory, made) = make("named-uuid", &definition, &subject, marker_policy());
    let made = made.expect("the node yields a document");
    let uuid = made
        .document
        .fields
        .iter()
        .find(|field| field.name == "full:jcr:uuid")
        .expect("the named definition indexes it");
    assert_eq!(
        uuid.options,
        IndexOptions::DocumentsAndFrequenciesAndPositions,
        "a named definition tokenizes, so the field is the analyzed kind"
    );
}
