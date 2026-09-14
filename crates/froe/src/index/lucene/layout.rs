//! The filesystem names and metadata files oak-run moves a Lucene index
//! directory through: the index folder's base name, the per-directory
//! subdirectory name, `index-details.txt` and `indexer-info.properties`.
//!
//! `docs/analysis/index-lucene-storage.md` §6 specifies all four. These are
//! pure functions over names and text, with no repository access, because
//! that is what makes them testable against oak-run's own output without a
//! store.
//!
//! **The two name rules are different rules**, which is the mistake to avoid:
//! the index folder's base name strips every character outside `[A-Za-z0-9_]`
//! (`IndexRootDirectory.getFSSafeName`, `e.replaceAll("\\W", "")`), while the
//! per-directory name strips **colons and nothing else**
//! (`DirectoryUtils.createSubDir`, `name.replace(":", "")`). A hyphen
//! survives the second and not the first, so `:suggest-data` becomes
//! `suggest-data` while an index named `my-index` folders as `myindex`.
//!
//! Both metadata files are `java.util.Properties` text, written with
//! `Properties.store(OutputStream, comment)` and read with
//! `Properties.load`. They arrive from an import directory a caller supplies,
//! so every read here is bounded and every malformed input is a typed error
//! naming the file and, where the parser can localize it, the line — never a
//! panic and never an unbounded allocation.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::java::parse_java_properties;

/// `IndexRootDirectory.MAX_NAME_LENGTH`.
pub const MAXIMUM_FOLDER_NAME_LENGTH: usize = 127;

/// `IndexRootDirectory.INDEX_METADATA_FILE_NAME`.
pub const INDEX_DETAILS_FILE_NAME: &str = "index-details.txt";

/// `IndexerInfo.INDEXER_META`.
pub const INDEXER_INFO_FILE_NAME: &str = "indexer-info.properties";

/// `IndexMeta.DIR_PREFIX`.
pub const DIRECTORY_MAPPING_PREFIX: &str = "dir.";

/// The largest metadata file this module will parse.
///
/// These files hold a handful of short lines. The bound exists because they
/// arrive from a directory a caller points at, so an oversized one is a
/// refusal rather than an allocation.
pub const MAXIMUM_METADATA_FILE_BYTES: usize = 1 << 20;

/// The base name of the local directory an index is dumped into, which is
/// `IndexRootDirectory.getIndexFolderBaseName`.
///
/// The path's elements reversed, at most three taken, `oak:index` dropped,
/// each remaining element stripped of every character outside `[A-Za-z0-9_]`,
/// then reversed back and joined with `_`, then truncated to 127 characters.
///
/// `/oak:index/lucene` gives `lucene`; `/content/oak:index/abc` gives
/// `content_abc`.
#[must_use]
pub fn index_folder_base_name(index_path: &str) -> String {
    let mut elements: Vec<&str> = index_path
        .split('/')
        .filter(|element| !element.is_empty())
        .collect();
    elements.reverse();
    let mut result: Vec<String> = elements
        .into_iter()
        .take(3)
        .filter(|element| *element != "oak:index")
        .map(filesystem_safe_name)
        .collect();
    result.reverse();
    let name = result.join("_");
    name.chars().take(MAXIMUM_FOLDER_NAME_LENGTH).collect()
}

/// `IndexRootDirectory.getFSSafeName`: `e.replaceAll("\\W", "")`.
///
/// Java's `\w` is `[a-zA-Z_0-9]`, so the **underscore survives** — against
/// the method's own Javadoc, which claims `a-zA-Z0-9-`, and against the
/// `TODO` above it proposing `[^\W_]`. The code wins.
#[must_use]
pub fn filesystem_safe_name(element: &str) -> String {
    element
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect()
}

/// `DirectoryUtils.createSubDir`: `name.replace(":", "")`, colons and nothing
/// else.
///
/// `:data` becomes `data` and `:suggest-data` becomes `suggest-data`.
#[must_use]
pub fn filesystem_directory_name(jcr_name: &str) -> String {
    jcr_name.replace(':', "")
}

/// The content of an `index-details.txt`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexDetails {
    /// `metaFormatVersion`, which `IndexMeta` writes as the constant 1.
    pub meta_format_version: i64,
    /// `indexPath`, the JCR path of the index this directory holds.
    pub index_path: String,
    /// `creationTime`, epoch milliseconds.
    pub creation_time: i64,
    /// One entry per copied directory, **keyed by the filesystem name** and
    /// holding the JCR name — the direction `getJcrNameFromFSName` reads it.
    pub directory_mappings: BTreeMap<String, String>,
}

impl IndexDetails {
    /// Parses an `index-details.txt`.
    ///
    /// `indexPath` defaults to the empty string and `creationTime` to zero,
    /// because `IndexMeta`'s own constructor does — "the file might be empty,
    /// in which case we ignore it". A caller that needs a real index path
    /// tests for the empty string rather than expecting a refusal here.
    pub fn parse(content: &str) -> Result<Self> {
        let properties = parse_metadata(content, INDEX_DETAILS_FILE_NAME)?;
        let mut directory_mappings = BTreeMap::new();
        for (key, value) in &properties {
            if let Some(filesystem_name) = key.strip_prefix(DIRECTORY_MAPPING_PREFIX) {
                directory_mappings.insert(filesystem_name.to_owned(), value.clone());
            }
        }
        Ok(Self {
            meta_format_version: numeric(&properties, "metaFormatVersion").unwrap_or(1),
            index_path: properties.get("indexPath").cloned().unwrap_or_default(),
            creation_time: numeric(&properties, "creationTime").unwrap_or(0),
            directory_mappings,
        })
    }

    /// Renders an `index-details.txt` the way `IndexMeta.writeTo` does, minus
    /// the timestamp comment `Properties.store` writes, which is wall-clock
    /// and would make two renderings of the same content differ.
    ///
    /// Entries are written in sorted order. Java's own order is a hash order
    /// its documentation does not specify, so there is nothing to reproduce
    /// and a deterministic order is worth more than an arbitrary one.
    #[must_use]
    pub fn render(&self) -> String {
        let mut properties: BTreeMap<String, String> = self
            .directory_mappings
            .iter()
            .map(|(filesystem_name, jcr_name)| {
                (
                    format!("{DIRECTORY_MAPPING_PREFIX}{filesystem_name}"),
                    jcr_name.clone(),
                )
            })
            .collect();
        properties.insert(
            "metaFormatVersion".to_owned(),
            self.meta_format_version.to_string(),
        );
        properties.insert("indexPath".to_owned(), self.index_path.clone());
        properties.insert("creationTime".to_owned(), self.creation_time.to_string());
        render_properties(&properties)
    }

    /// The JCR name a filesystem directory name maps back to.
    #[must_use]
    pub fn jcr_name_for(&self, filesystem_name: &str) -> Option<&str> {
        self.directory_mappings
            .get(filesystem_name)
            .map(String::as_str)
    }
}

/// The content of an `indexer-info.properties`: one property.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexerInfo {
    /// The checkpoint the dumped index data was taken at.
    pub checkpoint: String,
}

impl IndexerInfo {
    /// Parses an `indexer-info.properties`.
    ///
    /// A missing `checkpoint` is a refusal, because `IndexerInfo.fromDirectory`
    /// reads it through `PropUtils.getProp`, which throws on absence — this is
    /// the one field the import protocol cannot proceed without.
    pub fn parse(content: &str) -> Result<Self> {
        let properties = parse_metadata(content, INDEXER_INFO_FILE_NAME)?;
        let checkpoint =
            properties
                .get("checkpoint")
                .cloned()
                .ok_or_else(|| Error::InvalidFormat {
                    details: format!("{INDEXER_INFO_FILE_NAME} has no checkpoint property"),
                })?;
        Ok(Self { checkpoint })
    }

    /// Renders an `indexer-info.properties`.
    #[must_use]
    pub fn render(&self) -> String {
        let mut properties = BTreeMap::new();
        properties.insert("checkpoint".to_owned(), self.checkpoint.clone());
        render_properties(&properties)
    }
}

/// Parses a metadata file, bounding its size first.
fn parse_metadata(content: &str, file_name: &str) -> Result<BTreeMap<String, String>> {
    if content.len() > MAXIMUM_METADATA_FILE_BYTES {
        return Err(Error::InvalidFormat {
            details: format!(
                "{file_name} is {} bytes, above the {MAXIMUM_METADATA_FILE_BYTES}-byte limit \
                 for an index metadata file",
                content.len()
            ),
        });
    }
    let mut properties = BTreeMap::new();
    for (key, value) in parse_java_properties(content)? {
        // Java's own rule is last-duplicate-wins, which `insert` reproduces.
        properties.insert(from_units(&key), from_units(&value));
    }
    Ok(properties)
}

/// A Java string as Rust text, with an unpaired surrogate replaced.
///
/// `java.util.Properties` can hold one through a `\uXXXX` escape, and Rust's
/// `String` cannot. These files carry filesystem names and JCR paths, so an
/// unpaired surrogate is a malformed file rather than data to preserve; the
/// replacement keeps the parse total instead of adding a refusal nothing can
/// act on.
fn from_units(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

fn numeric(properties: &BTreeMap<String, String>, key: &str) -> Option<i64> {
    properties.get(key)?.trim().parse().ok()
}

/// Renders properties the way `Properties.store(OutputStream, …)` does, minus
/// its wall-clock timestamp comment.
///
/// The escaping was read off the JDK inside the pinned image rather than off
/// the specification: in a **key**, a space, `=`, `:`, `#`, `!` and `\` are
/// escaped and so are the control characters; in a **value** the same
/// characters are escaped except that a non-leading space is not; and because
/// Oak writes through an `OutputStream`, the text is ISO-8859-1 and every
/// character above U+00FF becomes `\uXXXX`.
fn render_properties(properties: &BTreeMap<String, String>) -> String {
    let mut rendered = String::new();
    for (key, value) in properties {
        rendered.push_str(&escape(key, true));
        rendered.push('=');
        rendered.push_str(&escape(value, false));
        rendered.push('\n');
    }
    rendered
}

fn escape(text: &str, is_key: bool) -> String {
    let mut escaped = String::new();
    for (index, character) in text.chars().enumerate() {
        match character {
            ' ' if is_key || index == 0 => escaped.push_str("\\ "),
            ' ' => escaped.push(' '),
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\u{c}' => escaped.push_str("\\f"),
            '=' | ':' | '#' | '!' => {
                escaped.push('\\');
                escaped.push(character);
            }
            character if !(' '..='\u{ff}').contains(&character) => {
                for unit in character.encode_utf16(&mut [0u16; 2]) {
                    use std::fmt::Write as _;
                    let _ = write!(escaped, "\\u{unit:04X}");
                }
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{
        IndexDetails, IndexerInfo, MAXIMUM_METADATA_FILE_BYTES, filesystem_directory_name,
        filesystem_safe_name, index_folder_base_name,
    };

    #[test]
    fn the_fixtures_index_path_folders_as_its_last_element() {
        assert_eq!(index_folder_base_name("/oak:index/lucene"), "lucene");
    }

    #[test]
    fn a_non_root_index_path_joins_its_elements_with_an_underscore() {
        assert_eq!(
            index_folder_base_name("/content/oak:index/abc"),
            "content_abc"
        );
    }

    #[test]
    fn at_most_three_trailing_elements_are_taken() {
        assert_eq!(
            index_folder_base_name("/a/b/c/d/oak:index/e"),
            "d_e",
            "the limit of three is applied to the reversed elements *before* oak:index is \
             dropped — [e, oak:index, d] — so only two survive"
        );
    }

    #[test]
    fn the_underscore_survives_the_folder_name_rule_and_the_hyphen_does_not() {
        assert_eq!(filesystem_safe_name("my_index"), "my_index");
        assert_eq!(filesystem_safe_name("my-index"), "myindex");
        assert_eq!(filesystem_safe_name("oak:index"), "oakindex");
    }

    #[test]
    fn the_directory_name_rule_strips_colons_and_nothing_else() {
        assert_eq!(filesystem_directory_name(":data"), "data");
        assert_eq!(
            filesystem_directory_name(":suggest-data"),
            "suggest-data",
            "the hyphen survives here, unlike in the folder base name"
        );
        assert_eq!(
            filesystem_directory_name(":oak-libs-index-data"),
            "oak-libs-index-data"
        );
    }

    #[test]
    fn a_long_folder_name_is_truncated_to_a_hundred_and_twenty_seven_characters() {
        let long = "a".repeat(200);
        assert_eq!(
            index_folder_base_name(&format!("/oak:index/{long}")).len(),
            127
        );
    }

    #[test]
    fn index_details_round_trip_through_their_rendering() {
        let details = IndexDetails {
            meta_format_version: 1,
            index_path: "/oak:index/lucene".to_owned(),
            creation_time: 1_700_000_000_000,
            directory_mappings: [
                ("data".to_owned(), ":data".to_owned()),
                ("suggest-data".to_owned(), ":suggest-data".to_owned()),
            ]
            .into_iter()
            .collect(),
        };
        let rendered = details.render();
        assert!(
            rendered.contains("indexPath=/oak\\:index/lucene"),
            "a colon in a value is escaped — {rendered}"
        );
        assert_eq!(IndexDetails::parse(&rendered).expect("re-parse"), details);
    }

    #[test]
    fn a_comment_line_and_an_escaped_colon_parse_the_way_java_reads_them() {
        let content = "#Index metadata\n\
                       !another comment\n\
                       metaFormatVersion=1\n\
                       indexPath=/oak\\:index/lucene\n\
                       creationTime=1700000000000\n\
                       dir.data=\\:data\n";
        let details = IndexDetails::parse(content).expect("parse");
        assert_eq!(details.index_path, "/oak:index/lucene");
        assert_eq!(details.creation_time, 1_700_000_000_000);
        assert_eq!(details.jcr_name_for("data"), Some(":data"));
    }

    #[test]
    fn an_empty_details_file_parses_to_the_defaults_rather_than_refusing() {
        // `IndexMeta`'s own constructor says so: "the file might be empty -
        // in which case we ignore it".
        let details = IndexDetails::parse("").expect("parse");
        assert_eq!(details.index_path, "");
        assert_eq!(details.meta_format_version, 1);
        assert!(details.directory_mappings.is_empty());
    }

    #[test]
    fn a_details_file_above_the_size_bound_is_refused_by_name() {
        let oversized = "x".repeat(MAXIMUM_METADATA_FILE_BYTES + 1);
        let error = IndexDetails::parse(&oversized).expect_err("refused");
        assert!(error.to_string().contains("index-details.txt"), "{error}");
    }

    #[test]
    fn indexer_info_round_trips_and_refuses_a_file_with_no_checkpoint() {
        let info = IndexerInfo {
            checkpoint: "8b3d5f2a-1c4e-4a7b-9f01-2d3e4f5a6b7c".to_owned(),
        };
        assert_eq!(IndexerInfo::parse(&info.render()).expect("re-parse"), info);

        let error = IndexerInfo::parse("#only a comment\n").expect_err("refused");
        assert!(error.to_string().contains("checkpoint"), "{error}");
    }

    #[test]
    fn a_line_continuation_is_spliced_the_way_javas_reader_splices_it() {
        let content = "indexPath=/oak\\:index/\\\n    lucene\ncreationTime=1\n";
        let details = IndexDetails::parse(content).expect("parse");
        assert_eq!(
            details.index_path, "/oak:index/lucene",
            "the continuation's leading whitespace is dropped"
        );
    }

    #[test]
    fn a_key_with_a_space_is_escaped_on_the_key_side_only() {
        let mut properties = std::collections::BTreeMap::new();
        properties.insert("with space".to_owned(), "a b".to_owned());
        let rendered = super::render_properties(&properties);
        assert_eq!(rendered, "with\\ space=a b\n");
    }

    #[test]
    fn a_character_above_latin1_is_written_as_an_escape() {
        let mut properties = std::collections::BTreeMap::new();
        properties.insert("cjk".to_owned(), "中".to_owned());
        properties.insert("umlaut".to_owned(), "ä".to_owned());
        let rendered = super::render_properties(&properties);
        assert!(rendered.contains("cjk=\\u4E2D"), "{rendered}");
        assert!(
            rendered.contains("umlaut=ä"),
            "a Latin-1 character is written raw — {rendered}"
        );
    }
}
