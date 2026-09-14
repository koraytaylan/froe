//! Reading an `index-definitions.json` file back into definition states.
//!
//! This is the consumer side of [`super::definitions_json`], and the half
//! Oak's own `IndexDefinitionUpdater` performs: the file's key set is index
//! paths, and applying it replaces the whole definition node with the file's
//! state. `docs/analysis/index-definitions.md` §7.2 records the protocol.
//!
//! The grammar is exactly what the renderer emits, decoded back: the
//! positional `TypeCodes.split` rule, the `str:` escape, `[0]:<Type>` empty
//! typed arrays, and base64 binaries.
//!
//! **Every size is validated before it is allocated.** The file arrives from
//! a directory a caller points at, so a length or a nesting depth it chose
//! must not be able to exhaust memory. The reader is hand-written rather
//! than taken from a JSON crate for the same reason the renderer is: the
//! grammar is small, the escaping is Oak's rather than JSON's in two places,
//! and a dependency would have to pull its weight against that.

use std::collections::BTreeMap;

use crate::content::property::PropertyType;
use crate::index::{IndexError, IndexResult};

/// The largest `index-definitions.json` this reader will parse.
///
/// A definitions file holds definition nodes, not content; a hundred
/// megabytes of them is a file nobody produced on purpose.
pub const MAXIMUM_DEFINITIONS_FILE_BYTES: usize = 100 * 1024 * 1024;

/// How deeply definitions may nest.
///
/// Oak's own definitions are a handful of levels — `indexRules`, a node
/// type, `properties`, a rule. The bound exists so a crafted file cannot
/// drive the parser's own recursion off the stack.
pub const MAXIMUM_NESTING_DEPTH: usize = 64;

/// One property, as the file carries it.
#[derive(Clone, PartialEq, Debug)]
pub struct ParsedProperty {
    /// The type the file's encoding names, `String` when it names none.
    pub property_type: PropertyType,
    /// The values, in file order. A single value is one entry.
    pub values: Vec<String>,
    /// Whether the file wrote an array rather than a scalar.
    ///
    /// `["a"]` and `"a"` are different properties to Oak — one is
    /// multi-valued — and a comparison that lost the distinction would
    /// excuse real drift.
    pub multiple: bool,
}

/// One definition node, as the file carries it.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ParsedNode {
    /// Its properties, by name.
    pub properties: BTreeMap<String, ParsedProperty>,
    /// Its children, by name.
    pub children: BTreeMap<String, ParsedNode>,
}

/// Every definition in the file, keyed by index path in file order.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ParsedDefinitions {
    /// The definitions, by index path.
    pub definitions: BTreeMap<String, ParsedNode>,
}

/// Parses an `index-definitions.json`.
pub fn parse(content: &str) -> IndexResult<ParsedDefinitions> {
    if content.len() > MAXIMUM_DEFINITIONS_FILE_BYTES {
        return Err(malformed(
            0,
            format!(
                "the definitions file is {} bytes, above the {MAXIMUM_DEFINITIONS_FILE_BYTES} \
                 this reader accepts",
                content.len()
            ),
        ));
    }
    let mut parser = Parser {
        bytes: content.as_bytes(),
        position: 0,
    };
    parser.skip_whitespace();
    let root = parser.parse_node(0)?;
    parser.skip_whitespace();
    if parser.position != parser.bytes.len() {
        return Err(malformed(
            parser.position,
            "trailing content after the definitions object",
        ));
    }
    Ok(ParsedDefinitions {
        definitions: root.children,
    })
}

/// A malformed-file error naming the offset.
fn malformed(offset: usize, details: impl AsRef<str>) -> IndexError {
    IndexError::Record(crate::Error::InvalidFormat {
        details: format!(
            "index-definitions.json is malformed at offset {offset}: {}",
            details.as_ref()
        ),
    })
}

struct Parser<'content> {
    bytes: &'content [u8],
    position: usize,
}

impl Parser<'_> {
    fn skip_whitespace(&mut self) {
        while self
            .bytes
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn expect(&mut self, byte: u8) -> IndexResult<()> {
        if self.peek() == Some(byte) {
            self.position += 1;
            return Ok(());
        }
        Err(malformed(
            self.position,
            format!(
                "expected {:?}, found {}",
                byte as char,
                self.peek().map_or_else(
                    || "the end of the file".to_owned(),
                    |found| format!("{:?}", found as char)
                )
            ),
        ))
    }

    /// One object: properties and child nodes together, as the renderer
    /// writes them.
    fn parse_node(&mut self, depth: usize) -> IndexResult<ParsedNode> {
        if depth > MAXIMUM_NESTING_DEPTH {
            return Err(malformed(
                self.position,
                format!("definitions nest deeper than {MAXIMUM_NESTING_DEPTH} levels"),
            ));
        }
        self.expect(b'{')?;
        let mut node = ParsedNode::default();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Ok(node);
        }
        loop {
            self.skip_whitespace();
            let name = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            if self.peek() == Some(b'{') {
                let child = self.parse_node(depth + 1)?;
                node.children.insert(name, child);
            } else {
                let property = self.parse_property()?;
                node.properties.insert(name, property);
            }
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    return Ok(node);
                }
                _ => {
                    return Err(malformed(
                        self.position,
                        "expected ',' or '}' after a member",
                    ));
                }
            }
        }
    }

    /// One property: a scalar, or an array of them.
    fn parse_property(&mut self) -> IndexResult<ParsedProperty> {
        if self.peek() == Some(b'[') {
            self.position += 1;
            let mut values = Vec::new();
            self.skip_whitespace();
            if self.peek() == Some(b']') {
                self.position += 1;
                // `[]` is an empty STRING array; a typed empty array is the
                // string `[0]:<TypeName>` and is handled below.
                return Ok(ParsedProperty {
                    property_type: PropertyType::String,
                    values,
                    multiple: true,
                });
            }
            let mut property_type;
            loop {
                self.skip_whitespace();
                let (value_type, text) = self.parse_scalar()?;
                property_type = value_type;
                values.push(text);
                self.skip_whitespace();
                match self.peek() {
                    Some(b',') => self.position += 1,
                    Some(b']') => {
                        self.position += 1;
                        return Ok(ParsedProperty {
                            property_type,
                            values,
                            multiple: true,
                        });
                    }
                    _ => {
                        return Err(malformed(self.position, "expected ',' or ']' in an array"));
                    }
                }
            }
        }

        // A typed empty array renders as the string `[0]:<TypeName>`, and
        // it has to be recognized *before* the positional type-code split,
        // which would otherwise consume its `:` — index 3 of `[0]:Name` is
        // a colon, so the split fires and leaves `Name` behind.
        if self.peek() == Some(b'"') {
            let raw = self.parse_string()?;
            if let Some(name) = raw.strip_prefix("[0]:") {
                return Ok(ParsedProperty {
                    property_type: property_type_named(name).unwrap_or(PropertyType::String),
                    values: Vec::new(),
                    multiple: true,
                });
            }
            let (property_type, text) = split_type_code(&raw);
            return Ok(ParsedProperty {
                property_type,
                values: vec![text],
                multiple: false,
            });
        }

        let (property_type, text) = self.parse_scalar()?;
        Ok(ParsedProperty {
            property_type,
            values: vec![text],
            multiple: false,
        })
    }

    /// One scalar: `true`, `false`, a number, or a string carrying its own
    /// type code.
    fn parse_scalar(&mut self) -> IndexResult<(PropertyType, String)> {
        match self.peek() {
            Some(b'"') => {
                let text = self.parse_string()?;
                Ok(split_type_code(&text))
            }
            Some(b't') => {
                self.expect_literal("true")?;
                Ok((PropertyType::Boolean, "true".to_owned()))
            }
            Some(b'f') => {
                self.expect_literal("false")?;
                Ok((PropertyType::Boolean, "false".to_owned()))
            }
            _ => {
                let start = self.position;
                while self
                    .peek()
                    .is_some_and(|byte| byte.is_ascii_digit() || b"+-.eE".contains(&byte))
                {
                    self.position += 1;
                }
                if start == self.position {
                    return Err(malformed(start, "expected a value"));
                }
                let text = String::from_utf8_lossy(&self.bytes[start..self.position]).into_owned();
                // An unquoted number is a LONG unless it carries a decimal
                // point or an exponent, which is how the renderer writes a
                // DOUBLE.
                let property_type = if text.contains(['.', 'e', 'E']) {
                    PropertyType::Double
                } else {
                    PropertyType::Long
                };
                Ok((property_type, text))
            }
        }
    }

    fn expect_literal(&mut self, literal: &str) -> IndexResult<()> {
        if self.bytes[self.position..].starts_with(literal.as_bytes()) {
            self.position += literal.len();
            return Ok(());
        }
        Err(malformed(self.position, format!("expected {literal:?}")))
    }

    /// A JSON string, with the two-phase escaping the renderer writes.
    fn parse_string(&mut self) -> IndexResult<String> {
        self.expect(b'"')?;
        let mut text = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(malformed(self.position, "unterminated string"));
            };
            self.position += 1;
            match byte {
                b'"' => return Ok(text),
                b'\\' => {
                    let Some(escape) = self.peek() else {
                        return Err(malformed(self.position, "an escape at the end of the file"));
                    };
                    self.position += 1;
                    match escape {
                        b'"' => text.push('"'),
                        b'\\' => text.push('\\'),
                        b'/' => text.push('/'),
                        b'b' => text.push('\u{8}'),
                        b'f' => text.push('\u{c}'),
                        b'n' => text.push('\n'),
                        b'r' => text.push('\r'),
                        b't' => text.push('\t'),
                        b'u' => {
                            let start = self.position;
                            if self.position + 4 > self.bytes.len() {
                                return Err(malformed(start, "a truncated \\u escape"));
                            }
                            let digits =
                                std::str::from_utf8(&self.bytes[self.position..self.position + 4])
                                    .map_err(|_| {
                                        malformed(start, "a \\u escape that is not UTF-8")
                                    })?;
                            let code = u32::from_str_radix(digits, 16).map_err(|_| {
                                malformed(
                                    start,
                                    format!("a \\u escape {digits:?} that is not hexadecimal"),
                                )
                            })?;
                            self.position += 4;
                            text.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        other => {
                            return Err(malformed(
                                self.position,
                                format!("an unknown escape \\{}", other as char),
                            ));
                        }
                    }
                }
                _ => {
                    // Multi-byte UTF-8 arrives byte by byte; collect the
                    // whole sequence rather than one byte of it.
                    let start = self.position - 1;
                    let width = utf8_width(byte);
                    if start + width > self.bytes.len() {
                        return Err(malformed(start, "a truncated UTF-8 sequence"));
                    }
                    self.position = start + width;
                    let slice = std::str::from_utf8(&self.bytes[start..self.position])
                        .map_err(|_| malformed(start, "invalid UTF-8"))?;
                    text.push_str(slice);
                }
            }
        }
    }
}

/// How many bytes a UTF-8 sequence starting with `byte` occupies.
fn utf8_width(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// `TypeCodes.split` in reverse: the type a string's prefix names, and the
/// text after it.
///
/// The rule is positional, exactly as the renderer's is: `:blobId:`, or a
/// `:` at UTF-16 index 3. A `str:` prefix is the escape the renderer writes
/// for a STRING whose own text would otherwise look like a type code.
fn split_type_code(text: &str) -> (PropertyType, String) {
    if let Some(rest) = text.strip_prefix(":blobId:") {
        return (PropertyType::Binary, rest.to_owned());
    }
    let units: Vec<u16> = text.encode_utf16().take(4).collect();
    if units.len() >= 4 && units[3] == u16::from(b':') {
        let code: String = text.chars().take(3).collect();
        let rest: String = text.chars().skip(4).collect();
        if let Some(property_type) = property_type_for_code(&code) {
            return (property_type, rest);
        }
        // A three-character prefix that names no type is still consumed:
        // the renderer's `str:` escape exists precisely so a STRING whose
        // text looks like this round-trips, and `TypeCodes.split` is
        // positional rather than a lookup.
        return (PropertyType::String, rest);
    }
    (PropertyType::String, text.to_owned())
}

/// The type a three-character code names.
fn property_type_for_code(code: &str) -> Option<PropertyType> {
    EVERY_PROPERTY_TYPE.into_iter().find(|candidate| {
        let candidate_code: String = candidate
            .jcr_name()
            .chars()
            .take(3)
            .flat_map(char::to_lowercase)
            .collect();
        candidate_code == code
    })
}

/// The type a full JCR type name names.
fn property_type_named(name: &str) -> Option<PropertyType> {
    EVERY_PROPERTY_TYPE
        .into_iter()
        .find(|candidate| candidate.jcr_name() == name)
}

/// Every type a definition property can carry.
const EVERY_PROPERTY_TYPE: [PropertyType; 12] = [
    PropertyType::String,
    PropertyType::Binary,
    PropertyType::Long,
    PropertyType::Double,
    PropertyType::Date,
    PropertyType::Boolean,
    PropertyType::Name,
    PropertyType::Path,
    PropertyType::Reference,
    PropertyType::WeakReference,
    PropertyType::Uri,
    PropertyType::Decimal,
];

#[cfg(test)]
mod tests {
    use super::{MAXIMUM_NESTING_DEPTH, ParsedProperty, parse};
    use crate::content::property::PropertyType;

    /// One property of the single definition in `content`.
    fn property(content: &str, name: &str) -> ParsedProperty {
        let parsed = parse(content).expect("parse");
        parsed
            .definitions
            .get("/oak:index/lucene")
            .expect("the definition")
            .properties
            .get(name)
            .expect("the property")
            .clone()
    }

    /// A file holding one definition with `body` as its members.
    fn one_definition(body: &str) -> String {
        format!("{{\n  \"/oak:index/lucene\": {{ {body} }}\n}}")
    }

    #[test]
    fn every_type_code_decodes_to_its_type() {
        // The positional `TypeCodes.split` rule, one case per code the
        // renderer can emit.
        for (encoded, expected, text) in [
            ("\"nam:nt:base\"", PropertyType::Name, "nt:base"),
            ("\"pat:/content\"", PropertyType::Path, "/content"),
            ("\"dat:2026-01-01\"", PropertyType::Date, "2026-01-01"),
            ("\"ref:abc\"", PropertyType::Reference, "abc"),
            ("\"wea:abc\"", PropertyType::WeakReference, "abc"),
            ("\"uri:http://x\"", PropertyType::Uri, "http://x"),
            ("\"dec:1.5\"", PropertyType::Decimal, "1.5"),
            ("\":blobId:AAA=\"", PropertyType::Binary, "AAA="),
        ] {
            let parsed = property(&one_definition(&format!("\"p\": {encoded}")), "p");
            assert_eq!(parsed.property_type, expected, "{encoded}");
            assert_eq!(parsed.values, vec![text.to_owned()], "{encoded}");
            assert!(!parsed.multiple, "{encoded}");
        }
    }

    #[test]
    fn the_str_escape_yields_a_string_carrying_the_rest() {
        // The renderer writes `str:` for a STRING whose own text would
        // otherwise look like a type code, so `str:nam:x` is the STRING
        // `nam:x` and not a NAME.
        let parsed = property(&one_definition("\"p\": \"str:nam:x\""), "p");
        assert_eq!(parsed.property_type, PropertyType::String);
        assert_eq!(parsed.values, vec!["nam:x".to_owned()]);
    }

    #[test]
    fn a_string_that_is_not_a_type_code_is_left_whole() {
        let parsed = property(&one_definition("\"p\": \"plain text\""), "p");
        assert_eq!(parsed.property_type, PropertyType::String);
        assert_eq!(parsed.values, vec!["plain text".to_owned()]);
    }

    #[test]
    fn an_unquoted_number_is_a_long_and_a_decimal_one_is_a_double() {
        let long = property(&one_definition("\"p\": 42"), "p");
        assert_eq!(long.property_type, PropertyType::Long);
        assert_eq!(long.values, vec!["42".to_owned()]);

        let double = property(&one_definition("\"p\": 1.5"), "p");
        assert_eq!(double.property_type, PropertyType::Double);
        assert_eq!(double.values, vec!["1.5".to_owned()]);
    }

    #[test]
    fn booleans_decode_unquoted() {
        let truth = property(&one_definition("\"p\": true"), "p");
        assert_eq!(truth.property_type, PropertyType::Boolean);
        assert_eq!(truth.values, vec!["true".to_owned()]);
    }

    #[test]
    fn an_empty_string_array_and_an_empty_typed_array_are_distinguished() {
        // `[]` is an empty STRING array; a typed one renders as the string
        // `[0]:<TypeName>`. Losing the distinction would make two different
        // properties compare equal.
        let strings = property(&one_definition("\"p\": []"), "p");
        assert_eq!(strings.property_type, PropertyType::String);
        assert!(strings.values.is_empty());
        assert!(strings.multiple);

        let names = property(&one_definition("\"p\": \"[0]:Name\""), "p");
        assert_eq!(names.property_type, PropertyType::Name);
        assert!(names.values.is_empty());
        assert!(names.multiple);
    }

    #[test]
    fn a_single_value_and_a_one_element_array_stay_distinct() {
        // `"a"` and `["a"]` are different properties to Oak — one is
        // multi-valued — so the reader must not flatten them together.
        let single = property(&one_definition("\"p\": \"a\""), "p");
        let array = property(&one_definition("\"p\": [\"a\"]"), "p");
        assert!(!single.multiple);
        assert!(array.multiple);
        assert_eq!(single.values, array.values);
    }

    #[test]
    fn a_hidden_property_is_read_like_any_other() {
        let parsed = property(&one_definition("\":status\": \"ok\""), ":status");
        assert_eq!(parsed.values, vec!["ok".to_owned()]);
    }

    #[test]
    fn child_nodes_nest() {
        let content = one_definition(
            // `jcr:title` is exactly what the renderer escapes as
            // `str:jcr:title`: `TypeCodes.split` is positional, so a raw
            // `jcr:title` would be read as the three-character code `jcr`
            // and the rest. A file holding the raw form is one the renderer
            // could not have written.
            "\"indexRules\": { \"nt:base\": { \"properties\": { \"title\":              { \"name\": \"str:jcr:title\" } } } }",
        );
        let parsed = parse(&content).expect("parse");
        let definition = parsed
            .definitions
            .get("/oak:index/lucene")
            .expect("definition");
        let rule = definition
            .children
            .get("indexRules")
            .and_then(|rules| rules.children.get("nt:base"))
            .and_then(|node| node.children.get("properties"))
            .and_then(|properties| properties.children.get("title"))
            .expect("the nested rule");
        assert_eq!(
            rule.properties.get("name").expect("name").values,
            vec!["jcr:title".to_owned()]
        );
    }

    #[test]
    fn an_escaped_string_decodes() {
        let parsed = property(&one_definition("\"p\": \"a\\\"b\\\\c\\u0041\""), "p");
        assert_eq!(parsed.values, vec!["a\"b\\cA".to_owned()]);
    }

    #[test]
    fn nesting_deeper_than_the_bound_is_refused() {
        // A crafted file must not drive the parser's recursion off the
        // stack, so the depth is bounded and the refusal names the bound.
        let depth = MAXIMUM_NESTING_DEPTH + 5;
        let mut content = String::from("{\"/oak:index/lucene\": ");
        for _ in 0..depth {
            content.push_str("{\"child\": ");
        }
        content.push_str("{}");
        for _ in 0..depth {
            content.push('}');
        }
        content.push('}');

        let error = parse(&content).expect_err("deep nesting must be refused");
        assert!(error.to_string().contains("nest deeper than"), "{error}");
    }

    #[test]
    fn a_malformed_file_names_the_offset() {
        let error = parse("{\"/oak:index/lucene\": }").expect_err("malformed");
        assert!(error.to_string().contains("malformed at offset"), "{error}");
    }

    #[test]
    fn an_unterminated_string_is_refused() {
        let error =
            parse("{\"/oak:index/lucene\": {\"p\": \"unterminated}").expect_err("malformed");
        assert!(error.to_string().contains("unterminated string"), "{error}");
    }

    #[test]
    fn trailing_content_after_the_object_is_refused() {
        let error = parse("{} trailing").expect_err("malformed");
        assert!(error.to_string().contains("trailing content"), "{error}");
    }

    #[test]
    fn an_empty_object_parses_to_no_definitions() {
        assert!(parse("{}").expect("parse").definitions.is_empty());
    }
}
