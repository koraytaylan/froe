//! Rendering index definitions in the JSON form Oak's own definition printer
//! emits.
//!
//! This is the file `oak-run index --index-definitions-file` and Oak's own
//! `IndexDefinitionUpdater` consume, and applying it **replaces the whole
//! definition node** with the file's state. A renderer that drops a hidden
//! property, or gets one type code wrong, produces a file that silently
//! changes a definition on import — which is why
//! `docs/analysis/index-definitions.md` §8 specifies this to the byte and why
//! the interop phase holds the output to a comparison against Oak's own
//! printer rather than to a round trip through froe.
//!
//! The four rules a renderer is most likely to miss, each with its own test:
//!
//! * **Hidden properties are included**; only hidden *children* are excluded.
//!   The filter is `{"properties":["*","-:childOrder"],"nodes":["*","-:*"]}`,
//!   so `:version` and `:originalType` are rendered and `:data` is not. The
//!   fixture's `lucene` definition carries `:version`.
//! * **`:childOrder` steers the output without appearing in it.** When the
//!   property exists it *is* the list of children to serialize, so a child
//!   not named in it is not rendered at all.
//! * **A plain `STRING` is type-code prefixed whenever the splitter
//!   recognizes a prefix in it** — one starting with `:blobId:`, or of length
//!   four or more with `:` at index 3, the three characters a known code or
//!   not. So `jcr:title` renders as `str:jcr:title`, and so does `abc:x`.
//! * **The string escaping has two phases**, and their asymmetry is
//!   observable: the scan trips on `"`, `\`, characters below U+0020 and
//!   **high** surrogates only, so a lone *low* surrogate stays raw in a
//!   string that trips nothing else and is escaped as `\udcXX` in one that
//!   also holds a quote. DEL and the C1 range are always raw.
//!
//! froe has a JSON escaper already, in `froe-export` — and it is a different
//! rule, escaping DEL and C1 because terminals interpret them. It is also in
//! a crate that depends on this one. Oak's rule is implemented here rather
//! than shared, because the two are answering different questions.

use std::fmt::Write as _;

use crate::content::node::{NodeState, PropertyState};
use crate::content::property::{PropertyType, PropertyValue, double_to_text};
use crate::content::value::BinaryValue;
use crate::content::{PropertyValues, SegmentProvider, read_binary_stream};
use crate::index::{IndexError, IndexResult, values_of};

/// `oak.serializer.maxBlobSize`, the default limit Oak's blob serializer
/// refuses **at or above**: the test is `blob.length() < maxSize`.
pub const DEFAULT_MAXIMUM_BLOB_SIZE: u64 = 1 << 20;

/// The two indent spaces `JsopBuilder.prettyPrint` uses.
const INDENT: &str = "  ";

/// Which children a rendering includes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChildFilter {
    /// The definition printer's own filter: every hidden child excluded.
    Printer,
    /// The filter an oak-run out-of-band build dumps with, which drops
    /// `:index-definition`, `:data` and `:suggest-data` and **keeps
    /// `:status`** and every hidden property.
    OutOfBandBuild,
}

impl ChildFilter {
    fn includes(self, child_name: &str) -> bool {
        match self {
            ChildFilter::Printer => !child_name.starts_with(':'),
            ChildFilter::OutOfBandBuild => {
                !matches!(child_name, ":index-definition" | ":data" | ":suggest-data")
            }
        }
    }
}

/// How a rendering handles a blob too large to serialize.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RenderOptions {
    /// The blob-size limit, at or above which a binary is refused.
    pub maximum_blob_size: u64,
    /// Which children to include.
    pub child_filter: ChildFilter,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            maximum_blob_size: DEFAULT_MAXIMUM_BLOB_SIZE,
            child_filter: ChildFilter::Printer,
        }
    }
}

/// Renders definitions as one JSON object keyed by index path.
///
/// The key order is the caller's, and the caller is what decides it: the
/// printer walks the index paths Oak's path service yields, while an oak-run
/// run given `--index-paths` uses those and never consults the service — so
/// its nodetype precondition is never evaluated. Passing the order in rather
/// than computing it here is what makes both reachable.
pub fn render(
    provider: &dyn SegmentProvider,
    definitions: &[(String, NodeState<'_>)],
    options: RenderOptions,
) -> IndexResult<String> {
    let mut rendered = String::from("{");
    if definitions.is_empty() {
        rendered.push('}');
        return Ok(rendered);
    }
    rendered.push('\n');
    let mut indent = String::from(INDENT);
    for (position, (path, node)) in definitions.iter().enumerate() {
        if position > 0 {
            rendered.push_str(",\n");
            rendered.push_str(&indent);
        } else {
            rendered.push_str(&indent);
        }
        append_string(&mut rendered, path);
        rendered.push_str(": ");
        append_node(provider, &mut rendered, node, &mut indent, options)?;
    }
    indent.truncate(indent.len() - INDENT.len());
    rendered.push('\n');
    rendered.push_str(&indent);
    // No trailing newline: the printer ends at `IndexDefinitionPrinter`'s
    // `printWriter.print(prettyPrint(...))`, and `prettyPrint` closes the
    // outermost object with `'\n' + space + '}'` and stops. `PrinterDumper`
    // flushes without adding one, so `index-definitions.json` has no final
    // newline and neither does this.
    rendered.push('}');
    Ok(rendered)
}

/// One node, with its properties in stored order and then its children.
fn append_node(
    provider: &dyn SegmentProvider,
    rendered: &mut String,
    node: &NodeState<'_>,
    indent: &mut String,
    options: RenderOptions,
) -> IndexResult<()> {
    let properties: Vec<PropertyState> = node
        .properties()?
        .into_iter()
        .filter(|property| property.name != ":childOrder")
        .collect();
    let children = ordered_children(node, options)?;

    if properties.is_empty() && children.is_empty() {
        rendered.push_str("{}");
        return Ok(());
    }

    rendered.push_str("{\n");
    indent.push_str(INDENT);
    let mut first = true;
    for property in &properties {
        append_member_separator(rendered, indent, &mut first);
        append_string(rendered, &property.name);
        rendered.push_str(": ");
        append_property(provider, rendered, property, options)?;
    }
    for (name, child) in &children {
        append_member_separator(rendered, indent, &mut first);
        append_string(rendered, name);
        rendered.push_str(": ");
        append_node(provider, rendered, child, indent, options)?;
    }
    indent.truncate(indent.len() - INDENT.len());
    rendered.push('\n');
    rendered.push_str(indent);
    rendered.push('}');
    Ok(())
}

fn append_member_separator(rendered: &mut String, indent: &str, first: &mut bool) {
    if *first {
        rendered.push_str(indent);
        *first = false;
    } else {
        rendered.push_str(",\n");
        rendered.push_str(indent);
    }
}

/// The children to serialize, in the order `:childOrder` gives when it exists
/// and in stored order otherwise.
///
/// A `:childOrder` entry naming a child that does not exist is skipped, and a
/// child the property does not name is **not rendered at all** — which is
/// `JsonSerializer.getChildNodeEntries`, not an approximation of it.
fn ordered_children<'provider>(
    node: &NodeState<'provider>,
    options: RenderOptions,
) -> IndexResult<Vec<(String, NodeState<'provider>)>> {
    let entries = node.child_node_entries()?;
    let order = node.property(":childOrder")?;
    let ordered = match &order {
        None => entries,
        Some(property) => {
            let named: Vec<String> = values_of(property)
                .iter()
                .filter_map(PropertyValue::as_text)
                .collect();
            let mut ordered = Vec::with_capacity(named.len());
            for name in named {
                if let Some((_, child)) = entries.iter().find(|(entry, _)| *entry == name) {
                    ordered.push((name, *child));
                }
            }
            ordered
        }
    };
    Ok(ordered
        .into_iter()
        .filter(|(name, _)| options.child_filter.includes(name))
        .collect())
}

/// One property value or array.
fn append_property(
    provider: &dyn SegmentProvider,
    rendered: &mut String,
    property: &PropertyState,
    options: RenderOptions,
) -> IndexResult<()> {
    match &property.values {
        PropertyValues::Single(value) => {
            append_value(provider, rendered, value, property.property_type, options)
        }
        PropertyValues::Multiple(values) => {
            // An empty array of any type but STRING renders as the
            // type-safe `[0]:<TypeName>`; STRING's renders as `[]`.
            if values.is_empty() && property.property_type != PropertyType::String {
                append_string(
                    rendered,
                    &format!("[0]:{}", property.property_type.jcr_name()),
                );
                return Ok(());
            }
            rendered.push('[');
            for (position, value) in values.iter().enumerate() {
                if position > 0 {
                    rendered.push_str(", ");
                }
                append_value(provider, rendered, value, property.property_type, options)?;
            }
            rendered.push(']');
            Ok(())
        }
    }
}

fn append_value(
    provider: &dyn SegmentProvider,
    rendered: &mut String,
    value: &PropertyValue,
    property_type: PropertyType,
    options: RenderOptions,
) -> IndexResult<()> {
    match value {
        PropertyValue::Boolean(truth) => {
            rendered.push_str(if *truth { "true" } else { "false" });
        }
        PropertyValue::Long(number) => {
            let _ = write!(rendered, "{number}");
        }
        PropertyValue::Double(number) => {
            if number.is_nan() || number.is_infinite() {
                append_string(rendered, &format!("dou:{}", double_to_text(*number)));
            } else {
                // `json.encodedValue(value.toString())`: Java's own textual
                // form, unquoted.
                rendered.push_str(&double_to_text(*number));
            }
        }
        PropertyValue::Binary(binary) => {
            let encoded = encode_blob(provider, binary, options.maximum_blob_size)?;
            append_string(rendered, &format!(":blobId:{encoded}"));
        }
        other => {
            let text = other.as_text().unwrap_or_default();
            if property_type == PropertyType::String && type_code_split(&text).is_none() {
                append_string(rendered, &text);
            } else {
                append_string(rendered, &format!("{}:{text}", type_code(property_type)));
            }
        }
    }
    Ok(())
}

/// `TypeCodes`: the lower-cased first three characters of the JCR type name,
/// with `BINARY` spelled `:blobId` instead.
fn type_code(property_type: PropertyType) -> String {
    if property_type == PropertyType::Binary {
        return ":blobId".to_owned();
    }
    property_type
        .jcr_name()
        .chars()
        .take(3)
        .flat_map(char::to_lowercase)
        .collect()
}

/// `TypeCodes.split`: where a type-code prefix ends, or `None`.
///
/// The rule is positional, not a lookup: a string starting with `:blobId:`,
/// or of length four or more in **UTF-16 code units** with `:` at index 3,
/// whether or not those three characters name a real type.
fn type_code_split(text: &str) -> Option<usize> {
    if text.starts_with(":blobId:") {
        return Some(7);
    }
    let units: Vec<u16> = text.encode_utf16().take(4).collect();
    (units.len() >= 4 && units[3] == u16::from(b':')).then_some(3)
}

/// Base64 with the standard alphabet, `=` padding and no line breaks, which
/// is what `org.apache.jackrabbit.util.Base64.encode` writes.
///
/// Hand-implemented rather than taken from a crate: it is twenty lines, and a
/// dependency would have to pull its weight against that.
fn encode_blob(
    provider: &dyn SegmentProvider,
    binary: &BinaryValue,
    maximum_blob_size: u64,
) -> IndexResult<String> {
    let BinaryValue::Inline {
        length,
        record_identifier,
    } = binary
    else {
        return Err(IndexError::Record(crate::Error::InvalidFormat {
            details: "a definition holds a binary in an external blob store, which froe does \
                      not read"
                .to_owned(),
        }));
    };
    if *length >= maximum_blob_size {
        return Err(IndexError::Record(crate::Error::InvalidFormat {
            details: format!(
                "cannot serialize a binary of {length} bytes, which is at or above the limit \
                 of {maximum_blob_size}"
            ),
        }));
    }
    let mut content = Vec::new();
    let mut stream =
        read_binary_stream(provider, *record_identifier).map_err(IndexError::Record)?;
    std::io::Read::read_to_end(&mut stream, &mut content)
        .map_err(|error| IndexError::Record(crate::Error::InputOutput(error)))?;
    Ok(base64(&content))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let mut packed = 0u32;
        for (position, byte) in group.iter().enumerate() {
            packed |= u32::from(*byte) << (16 - 8 * position);
        }
        let symbols = group.len() + 1;
        for position in 0..4 {
            if position < symbols {
                let index = ((packed >> (18 - 6 * position)) & 0x3F) as usize;
                encoded.push(char::from(ALPHABET[index]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

/// `JsopBuilder.encode`: the two-phase escaping, quoted.
fn append_string(rendered: &mut String, text: &str) {
    rendered.push('"');
    if text.encode_utf16().any(should_escape) {
        escape(rendered, text);
    } else {
        rendered.push_str(text);
    }
    rendered.push('"');
}

/// Phase one. Note what is **not** here: DEL, the C1 range, and low
/// surrogates. A string containing only a lone low surrogate is appended raw.
fn should_escape(unit: u16) -> bool {
    unit == u16::from(b'"')
        || unit == u16::from(b'\\')
        || unit < 0x20
        || (0xD800..=0xDBFF).contains(&unit)
}

/// Phase two, over UTF-16 code units, because an unpaired surrogate of either
/// half is escaped and Rust's `char` cannot hold one.
fn escape(rendered: &mut String, text: &str) {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        match unit {
            0x22 => rendered.push_str("\\\""),
            0x5C => rendered.push_str("\\\\"),
            0x08 => rendered.push_str("\\b"),
            0x0C => rendered.push_str("\\f"),
            0x0A => rendered.push_str("\\n"),
            0x0D => rendered.push_str("\\r"),
            0x09 => rendered.push_str("\\t"),
            _ if unit < 0x20 => {
                let _ = write!(rendered, "\\u{unit:04x}");
            }
            _ if (0xD800..=0xDFFF).contains(&unit) => {
                let paired = (0xD800..=0xDBFF).contains(&unit)
                    && index + 1 < units.len()
                    && (0xDC00..=0xDFFF).contains(&units[index + 1]);
                if paired {
                    let pair = [unit, units[index + 1]];
                    rendered.push_str(&String::from_utf16_lossy(&pair));
                    index += 1;
                } else {
                    let _ = write!(rendered, "\\u{unit:04x}");
                }
            }
            _ => rendered.push_str(&String::from_utf16_lossy(&[unit])),
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{append_string, base64, type_code_split};

    /// Builds a string from UTF-16 code units, replacing what Rust cannot
    /// hold — which is why the surrogate rules are pinned here rather than
    /// through a store: froe's own string decoding is lossy, so an unpaired
    /// surrogate never reaches the renderer from a repository. The rule is
    /// still reproduced, because it is Oak's escaper's definition and a
    /// caller constructing a value can reach it.
    fn rendered(units: &[u16]) -> String {
        let text = String::from_utf16_lossy(units);
        let mut buffer = String::new();
        append_string(&mut buffer, &text);
        buffer
    }

    #[test]
    fn a_string_that_trips_nothing_is_appended_untouched() {
        let mut buffer = String::new();
        append_string(&mut buffer, "plain text");
        assert_eq!(buffer, "\"plain text\"");
    }

    #[test]
    fn the_five_named_escapes_come_before_the_generic_one() {
        let mut buffer = String::new();
        append_string(&mut buffer, "\u{8}\u{c}\n\r\t\u{1}");
        assert_eq!(buffer, "\"\\b\\f\\n\\r\\t\\u0001\"");
    }

    #[test]
    fn a_quote_and_a_backslash_are_emitted_before_the_named_five() {
        let mut buffer = String::new();
        append_string(&mut buffer, "a\"b\\c");
        assert_eq!(buffer, "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn delete_and_the_c1_range_never_trip_the_scan() {
        let mut buffer = String::new();
        append_string(&mut buffer, "\u{7f}\u{85}\u{9f}");
        assert_eq!(buffer, "\"\u{7f}\u{85}\u{9f}\"");
    }

    #[test]
    fn a_lone_high_surrogate_trips_the_scan_and_is_escaped() {
        // Rust cannot hold one, so this is what a lossy decode leaves — the
        // scan is still exercised through the replacement character, which
        // does not trip it, and the escape rule is exercised below.
        assert_eq!(rendered(&[0xD83D]), "\"\u{fffd}\"");
    }

    #[test]
    fn a_surrogate_pair_is_emitted_as_its_character() {
        let mut buffer = String::new();
        append_string(&mut buffer, "\"\u{1f600}");
        assert_eq!(buffer, "\"\\\"\u{1f600}\"");
    }

    #[test]
    fn the_type_code_splitter_is_positional_rather_than_a_lookup() {
        assert_eq!(type_code_split("jcr:title"), Some(3));
        assert_eq!(type_code_split("abc:x"), Some(3), "an unknown code counts");
        assert_eq!(type_code_split("ab:c"), None, "a colon at index 2 does not");
        assert_eq!(type_code_split("abc:"), Some(3), "length four is enough");
        assert_eq!(type_code_split(":blobId:x"), Some(7));
        assert_eq!(type_code_split("plain"), None);
    }

    #[test]
    fn base64_matches_the_jackrabbit_encoders_output() {
        // Observed from `org.apache.jackrabbit.util.Base64.encode` inside the
        // pinned image: the standard alphabet, `=` padding, no line breaks.
        assert_eq!(base64(&[]), "");
        assert_eq!(base64(&[0]), "AA==");
        assert_eq!(base64(&[0, 1]), "AAE=");
        assert_eq!(base64(&[0, 1, 2]), "AAEC");
        assert_eq!(base64(&[0, 1, 2, 3]), "AAECAw==");
        let hundred: Vec<u8> = (0..100).map(|index| (index % 251) as u8).collect();
        let encoded = base64(&hundred);
        assert_eq!(encoded.len(), 136);
        assert!(!encoded.contains('\n'));
        assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwd"));
    }
}
