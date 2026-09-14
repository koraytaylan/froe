//! Rendering index definitions in Oak's own printer format.
//!
//! Every expectation here is hand-written rather than derived from the
//! renderer, because the whole point of the format is that Oak's own
//! definition updater consumes it: a file that round-trips through froe and
//! nothing else proves nothing. The byte comparison against Oak's printer is
//! the interop phase's acceptance; these pin each rule of the specification
//! separately so a failure there says which one broke.

#![allow(
    dead_code,
    reason = "the shared support module is larger than any one test binary uses"
)]

mod support;

use std::path::PathBuf;

use froe::index::definitions_json::{ChildFilter, RenderOptions, render};
use froe::store::Repository;
use support::property_index_layout::{Node, Property, write_repository_with_tree};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-definitions-json-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test repository directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Renders `/oak:index/test` from a store built around `definition`.
fn rendered(name: &str, definition: Node) -> String {
    rendered_with(name, definition, RenderOptions::default())
}

fn rendered_with(name: &str, definition: Node, options: RenderOptions) -> String {
    let directory = TestDirectory::new(name);
    let root = Node::new().with_child("oak:index", Node::new().with_child("test", definition));
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open the repository");
    let node = repository
        .node_at_path("/oak:index/test")
        .expect("resolve")
        .expect("exists");
    render(
        &repository,
        &[("/oak:index/test".to_owned(), node)],
        options,
    )
    .expect("render")
}

/// Wraps a definition body in the object-of-one-key the printer emits, so a
/// test states only the part it is about.
fn wrapped(body: &str) -> String {
    format!("{{\n  \"/oak:index/test\": {body}\n}}")
}

#[test]
fn a_definition_renders_its_properties_in_stored_order_with_their_type_codes() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with("reindex", Property::Boolean(false))
        .with("reindexCount", Property::Long(1))
        .with(
            "propertyNames",
            Property::Names(vec!["jcr:uuid".to_owned()]),
        );
    assert_eq!(
        rendered("basic", definition),
        wrapped(
            "{\n    \
             \"propertyNames\": [\"nam:jcr:uuid\"],\n    \
             \"reindex\": false,\n    \
             \"reindexCount\": 1,\n    \
             \"type\": \"property\"\n  }"
        ),
        "booleans and longs unquoted, a NAME prefixed nam:, a plain String bare"
    );
}

#[test]
fn a_hidden_property_is_rendered_and_a_hidden_child_is_not() {
    let definition = Node::new()
        .with("type", Property::Text("lucene".to_owned()))
        .with(":version", Property::Long(2))
        .with_child(":data", Node::new())
        .with_child("indexRules", Node::new());
    let output = rendered("hidden", definition);
    assert!(output.contains("\":version\": 2"), "{output}");
    assert!(!output.contains(":data"), "{output}");
    assert!(output.contains("\"indexRules\": {}"), "{output}");
}

#[test]
fn child_order_decides_which_children_render_and_in_what_order() {
    let definition = Node::new()
        .with("type", Property::Text("lucene".to_owned()))
        .with(
            ":childOrder",
            Property::Names(vec!["second".to_owned(), "first".to_owned()]),
        )
        .with_child("first", Node::new())
        .with_child("second", Node::new())
        .with_child("unnamed", Node::new());
    let output = rendered("child-order", definition);
    assert!(
        !output.contains(":childOrder"),
        "the property steers the output without appearing in it — {output}"
    );
    assert!(
        !output.contains("unnamed"),
        "a child :childOrder does not name is not rendered at all — {output}"
    );
    let second = output.find("second").expect("second");
    let first = output.find("\"first\"").expect("first");
    assert!(
        second < first,
        "the order is the property's, not the child map's — {output}"
    );
}

#[test]
fn an_empty_string_array_renders_as_an_empty_array_and_any_other_type_as_a_code() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with("strings", Property::Texts(Vec::new()))
        .with("names", Property::Names(Vec::new()));
    let output = rendered("empty-arrays", definition);
    assert!(output.contains("\"strings\": []"), "{output}");
    assert!(output.contains("\"names\": \"[0]:Name\""), "{output}");
}

#[test]
fn a_plain_string_shaped_like_a_type_code_is_prefixed_str() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with("title", Property::Text("jcr:title".to_owned()))
        .with("unknown", Property::Text("abc:x".to_owned()))
        .with("short", Property::Text("ab:c".to_owned()))
        .with("plain", Property::Text("no colon here".to_owned()));
    let output = rendered("str-prefix", definition);
    assert!(
        output.contains("\"title\": \"str:jcr:title\""),
        "a colon at index 3 triggers the prefix — {output}"
    );
    assert!(
        output.contains("\"unknown\": \"str:abc:x\""),
        "the three characters need not be a known code — {output}"
    );
    assert!(
        output.contains("\"short\": \"ab:c\""),
        "a colon at index 2 does not trigger it — {output}"
    );
    assert!(output.contains("\"plain\": \"no colon here\""), "{output}");
}

#[test]
fn a_double_renders_in_javas_own_form_and_the_non_finite_ones_as_codes() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with("finite", Property::Double(1.5))
        .with("nan", Property::Double(f64::NAN))
        .with("infinite", Property::Double(f64::NEG_INFINITY));
    let output = rendered("doubles", definition);
    assert!(output.contains("\"finite\": 1.5"), "unquoted — {output}");
    assert!(output.contains("\"nan\": \"dou:NaN\""), "{output}");
    assert!(
        output.contains("\"infinite\": \"dou:-Infinity\""),
        "{output}"
    );
}

#[test]
fn a_control_character_a_quote_and_a_backslash_take_the_escapes_java_emits() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with("quoted", Property::Text("a\"b\\c".to_owned()))
        .with("controls", Property::Text("a\tb\nc\u{1}d".to_owned()));
    let output = rendered("escapes", definition);
    assert!(output.contains(r#""quoted": "a\"b\\c""#), "{output}");
    assert!(
        output.contains(r#""controls": "a\tb\nc\u0001d""#),
        "the five named escapes, then lower-case \\uXXXX — {output}"
    );
}

#[test]
fn delete_and_the_c1_range_are_emitted_raw() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with("high", Property::Text("a\u{7f}b\u{85}c".to_owned()));
    let output = rendered("raw-controls", definition);
    assert!(
        output.contains("a\u{7f}b\u{85}c"),
        "the scan looks only below U+0020 — {output}"
    );
}

#[test]
fn a_non_ascii_string_is_emitted_raw() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with(
            "umlaut",
            Property::Text("\u{e4}\u{f6}\u{fc} \u{4e2d}\u{6587}".to_owned()),
        );
    assert!(
        rendered("non-ascii", definition)
            .contains("\"umlaut\": \"\u{e4}\u{f6}\u{fc} \u{4e2d}\u{6587}\"")
    );
}

#[test]
fn a_binary_renders_as_base64_under_the_blob_id_code() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with("blob", Property::Binary(vec![0u8, 1, 2, 3]));
    let output = rendered("binary", definition);
    assert!(
        output.contains("\"blob\": \":blobId:AAECAw==\""),
        "standard alphabet, = padding, no line breaks — {output}"
    );
}

#[test]
fn a_binary_at_the_size_limit_is_refused_and_one_byte_inside_it_is_encoded() {
    let limit = 64u64;
    let options = RenderOptions {
        maximum_blob_size: limit,
        child_filter: ChildFilter::Printer,
    };

    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with("blob", Property::Binary(vec![0u8; limit as usize - 1]));
    let output = rendered_with("binary-inside", definition, options);
    assert!(
        output.contains(":blobId:"),
        "one byte inside the limit encodes — {output}"
    );

    let directory = TestDirectory::new("binary-at-limit");
    let root = Node::new().with_child(
        "oak:index",
        Node::new().with_child(
            "test",
            Node::new()
                .with("type", Property::Text("property".to_owned()))
                .with("blob", Property::Binary(vec![0u8; limit as usize])),
        ),
    );
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open");
    let node = repository
        .node_at_path("/oak:index/test")
        .expect("resolve")
        .expect("exists");
    let error = render(
        &repository,
        &[("/oak:index/test".to_owned(), node)],
        options,
    )
    .expect_err("a blob at the limit is refused, because Oak's test is length < maxSize");
    assert!(
        error.to_string().contains("at or above the limit"),
        "{error}"
    );
}

#[test]
fn a_nested_child_renders_recursively_and_an_empty_one_inline() {
    let definition = Node::new()
        .with("type", Property::Text("lucene".to_owned()))
        .with_child(
            "indexRules",
            Node::new().with_child(
                "nt:base",
                Node::new().with("propertyIndex", Property::Boolean(true)),
            ),
        )
        .with_child("empty", Node::new());
    let output = rendered("nested", definition);
    assert!(output.contains("\"empty\": {}"), "{output}");
    assert!(
        output.contains("\"nt:base\": {\n        \"propertyIndex\": true\n      }"),
        "two spaces per level — {output}"
    );
}

#[test]
fn the_out_of_band_filter_keeps_status_and_drops_the_other_three() {
    let definition = Node::new()
        .with("type", Property::Text("lucene".to_owned()))
        .with_child(
            ":status",
            Node::new().with("uid", Property::Text("1".to_owned())),
        )
        .with_child(":index-definition", Node::new())
        .with_child(":data", Node::new())
        .with_child(":suggest-data", Node::new());
    let output = rendered_with(
        "out-of-band",
        definition,
        RenderOptions {
            maximum_blob_size: 1 << 20,
            child_filter: ChildFilter::OutOfBandBuild,
        },
    );
    assert!(output.contains("\":status\""), "{output}");
    assert!(!output.contains(":index-definition"), "{output}");
    assert!(!output.contains("\":data\""), "{output}");
    assert!(!output.contains(":suggest-data"), "{output}");
}

#[test]
fn two_renderings_of_one_store_are_byte_identical() {
    let definition = || {
        Node::new()
            .with("type", Property::Text("lucene".to_owned()))
            .with(
                "names",
                Property::Names(vec!["a".to_owned(), "b".to_owned()]),
            )
            .with_child("indexRules", Node::new())
    };
    assert_eq!(
        rendered("determinism-one", definition()),
        rendered("determinism-two", definition())
    );
}

#[test]
fn an_empty_definition_set_renders_as_an_empty_object() {
    let directory = TestDirectory::new("empty-set");
    write_repository_with_tree(&directory.path, &Node::new());
    let repository = Repository::open(&directory.path).expect("open");
    assert_eq!(
        render(&repository, &[], RenderOptions::default()).expect("render"),
        "{}"
    );
}

#[test]
fn the_rendering_ends_at_the_closing_brace_with_no_trailing_newline() {
    // `IndexDefinitionPrinter.print` ends at
    // `printWriter.print(JsopBuilder.prettyPrint(...))`, and `prettyPrint`
    // closes the outermost object with `'\n' + space + '}'` and returns.
    // `PrinterDumper.dump` flushes the writer without adding anything, so
    // `index-definitions.json` has no final newline. A renderer that adds
    // one fails task 0615's byte comparison on its very last byte.
    let definition = Node::new().with("type", Property::Text("property".to_owned()));
    let output = rendered("no-trailing-newline", definition);
    assert!(output.ends_with("\n}"), "{output:?}");
}

#[test]
fn several_definitions_keep_the_callers_order_rather_than_a_sort() {
    let directory = TestDirectory::new("several");
    let body = |name: &str| Node::new().with("type", Property::Text(name.to_owned()));
    let root = Node::new().with_child(
        "oak:index",
        Node::new()
            .with_child("aaa", body("property"))
            .with_child("zzz", body("lucene")),
    );
    write_repository_with_tree(&directory.path, &root);
    let repository = Repository::open(&directory.path).expect("open");
    let node = |path: &str| {
        repository
            .node_at_path(path)
            .expect("resolve")
            .expect("exists")
    };
    let output = render(
        &repository,
        &[
            ("/oak:index/zzz".to_owned(), node("/oak:index/zzz")),
            ("/oak:index/aaa".to_owned(), node("/oak:index/aaa")),
        ],
        RenderOptions::default(),
    )
    .expect("render");
    let zzz = output.find("zzz").expect("zzz");
    let aaa = output.find("aaa").expect("aaa");
    assert!(
        zzz < aaa,
        "the printer writes the paths in the order it is handed — {output}"
    );
    assert!(output.contains("},\n  \"/oak:index/aaa\""), "{output}");
}

#[test]
fn a_multi_valued_property_renders_on_one_line_with_comma_space() {
    let definition = Node::new()
        .with("type", Property::Text("property".to_owned()))
        .with(
            "propertyNames",
            Property::Names(vec!["jcr:uuid".to_owned(), "jcr:title".to_owned()]),
        );
    let output = rendered("multi-valued", definition);
    assert!(
        output.contains("\"propertyNames\": [\"nam:jcr:uuid\", \"nam:jcr:title\"]"),
        "an array is never broken across lines — {output}"
    );
}
