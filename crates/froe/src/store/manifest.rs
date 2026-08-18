//! Reading `store.version` out of the manifest, which is a Java
//! properties file: its line continuations, escapes, and duplicate-key
//! rule are Java's, reproduced rather than approximated.

use super::{ArchivePresence, Error, Path, Result};
use crate::java::{java_property_ascii, parse_java_i32, parse_java_properties};

/// The highest store version this reader understands
/// (`store.version` in the manifest; 2 since Oak 1.8).
pub(crate) const MAXIMUM_STORE_VERSION: i64 = 2;

/// Validates the `manifest` file with read-only semantics: never writes,
/// accepts store versions 1 and 2, and rejects a store that has archives
/// but no manifest (that is the legacy pre-tar format).
pub(crate) fn check_manifest(directory: &Path, archives: ArchivePresence) -> Result<()> {
    let manifest_path = directory.join("manifest");
    if !manifest_path.exists() {
        if archives == ArchivePresence::Present {
            return Err(Error::InvalidFormat {
                details: format!(
                    "{} has segment archives but no manifest; this is the legacy \
                     oak-segment format, not segment-tar",
                    directory.display()
                ),
            });
        }
        return Ok(());
    }
    let store_version = read_manifest_store_version(&manifest_path)?;
    if store_version <= 0 {
        return Err(Error::InvalidFormat {
            details: format!("invalid store version {store_version} in manifest"),
        });
    }
    if store_version > MAXIMUM_STORE_VERSION {
        return Err(Error::InvalidFormat {
            details: format!(
                "store version {store_version} is newer than this reader supports \
                 (up to {MAXIMUM_STORE_VERSION})"
            ),
        });
    }
    Ok(())
}

/// Reads the `store.version` key from the manifest, a Java properties file.
/// Logical-line continuation, separators, and character escapes follow
/// `Properties.load(Reader)`, including its last-duplicate-wins behavior. The
/// version is parsed as a Java `int`; an absent or unparseable value defaults
/// to the maximum supported version, like the Java reader.
pub(crate) fn read_manifest_store_version(manifest_path: &Path) -> Result<i64> {
    let content = std::fs::read_to_string(manifest_path)?;
    parse_manifest_store_version(&content)
}

pub(crate) fn parse_manifest_store_version(content: &str) -> Result<i64> {
    let mut version = MAXIMUM_STORE_VERSION;
    let store_version_key = java_property_ascii("store.version");
    for (key, value) in parse_java_properties(content)? {
        if key != store_version_key {
            continue;
        }
        version = parse_java_i32(&value).map_or(MAXIMUM_STORE_VERSION, i64::from);
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::java::parse_java_properties;

    #[test]
    fn manifest_properties_use_maximum_for_absent_or_last_unparseable_version() {
        assert_eq!(
            parse_manifest_store_version("unrelated=value\n").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=1\nstore.version=invalid\n").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=invalid\nstore.version=1\n").unwrap(),
            1,
        );
        assert_eq!(
            parse_manifest_store_version("store.version=1 \n").unwrap(),
            MAXIMUM_STORE_VERSION,
            "Java Integer.parseInt does not trim the decoded value",
        );
        assert_eq!(
            parse_manifest_store_version("store.version=2147483648\n").unwrap(),
            MAXIMUM_STORE_VERSION,
            "values outside a Java int are unparseable",
        );
        assert_eq!(
            parse_manifest_store_version("store.version=-2147483648\n").unwrap(),
            i64::from(i32::MIN),
        );
    }

    #[test]
    fn manifest_properties_accept_java_line_terminators_and_last_duplicate_wins() {
        let manifest = concat!(
            "\u{c}! ignored comment\r",
            "store.version=0\r\n",
            "store.version : 1\n",
        );

        assert_eq!(parse_manifest_store_version(manifest).unwrap(), 1);
    }

    #[test]
    fn manifest_properties_continue_odd_backslashes_and_skip_leading_whitespace() {
        assert_eq!(
            parse_manifest_store_version("store.version=\\\n 1").unwrap(),
            1,
        );

        for terminator in ["\n", "\r", "\r\n"] {
            let manifest = format!("store.version=\\{terminator} \t\u{c}1");
            assert_eq!(
                parse_manifest_store_version(&manifest).unwrap(),
                1,
                "terminator {terminator:?}",
            );
        }

        let properties = parse_java_properties("key=value\\\\\nnext=entry").unwrap();
        assert_eq!(properties[0].1, java_units("value\\"));
        assert_eq!(properties[1].0, java_units("next"));

        let three_backslashes = format!("key=value{}\n  continued", "\\".repeat(3));
        let properties = parse_java_properties(&three_backslashes).unwrap();
        assert_eq!(properties[0].1, java_units("value\\continued"));

        assert_eq!(
            parse_java_properties("\\\n").unwrap(),
            [(Vec::new(), Vec::new())],
            "a continued zero-length logical line at EOF is an empty Java property",
        );
    }

    #[test]
    fn manifest_properties_preserve_java_terminal_backslash_eof_behavior() {
        assert_eq!(
            parse_java_properties("\\").unwrap(),
            vec![(Vec::new(), Vec::new())],
        );
        assert_eq!(
            parse_java_properties("key=\\").unwrap(),
            vec![(java_units("key"), Vec::new())],
        );
        assert_eq!(
            parse_manifest_store_version("store.version=\\").unwrap(),
            MAXIMUM_STORE_VERSION,
        );
    }

    #[test]
    fn manifest_properties_decode_escaped_keys_separators_and_characters() {
        let properties = parse_java_properties(r"escaped\ key\:\=\\tail=\t\n\r\f\\\u0031").unwrap();

        assert_eq!(properties.len(), 1);
        assert_eq!(properties[0].0, java_units("escaped key:=\\tail"));
        assert_eq!(properties[0].1, java_units("\t\n\r\u{c}\\1"));
        assert_eq!(
            parse_manifest_store_version(r"store\.version=\u0031").unwrap(),
            1,
        );
        assert_eq!(
            parse_manifest_store_version(r"store\u002eversion=\u0031").unwrap(),
            1,
        );
    }

    #[test]
    fn manifest_properties_reject_malformed_unicode_escapes() {
        assert!(parse_manifest_store_version(r"store.version=\u12x4").is_err());
        assert!(parse_manifest_store_version(r"unrelated=\u123").is_err());
    }

    fn java_units(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn java_i32_accepts_bmp_decimal_digits_and_checks_signed_overflow() {
        assert_eq!(parse_java_i32(&java_units("١")), Some(1));
        assert_eq!(parse_java_i32(&java_units("２")), Some(2));
        assert_eq!(parse_java_i32(&java_units("١2३")), Some(123));
        assert_eq!(parse_java_i32(&java_units("+١")), Some(1));
        assert_eq!(parse_java_i32(&java_units("-٢")), Some(-2));
        assert_eq!(parse_java_i32(&java_units("٢١٤٧٤٨٣٦٤٧")), Some(i32::MAX));
        assert_eq!(parse_java_i32(&java_units("٢١٤٧٤٨٣٦٤٨")), None);
        assert_eq!(parse_java_i32(&java_units("-٢١٤٧٤٨٣٦٤٨")), Some(i32::MIN));
        assert_eq!(parse_java_i32(&java_units("-٢١٤٧٤٨٣٦٤٩")), None);
        assert_eq!(
            parse_manifest_store_version(r"store.version=\u0661").unwrap(),
            1,
        );
    }
}
