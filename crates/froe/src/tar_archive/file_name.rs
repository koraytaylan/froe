//! Archive file naming: `data00000a.tar`, `data00001a.tar`, …
//!
//! The number is the archive's position in write order. The trailing letter
//! is the *file generation*: when garbage collection rewrites an archive to
//! drop reclaimed segments, it bumps the letter (`a` → `b` → …). A reader
//! must only ever open the highest letter of each number — lower letters are
//! superseded leftovers that a crashed cleanup may not have deleted.

use std::collections::BTreeMap;

/// The parsed name of one segment archive.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ArchiveFileName {
    /// The complete file name, for example `data00012b.tar`.
    pub file_name: String,
    /// The archive number parsed from the digits, for example `12`.
    pub archive_number: u32,
    /// The file generation letter; a missing letter means `'a'`.
    pub file_generation: char,
}

impl ArchiveFileName {
    /// Parses a segment archive file name.
    ///
    /// Accepted shape (mirroring the Java pattern
    /// `(data)((0|[1-9][0-9]*)[0-9]{4})([a-z])?.tar`): the prefix `data`,
    /// at least five digits without redundant leading zeros beyond the
    /// five-digit padding, an optional lowercase generation letter, and the
    /// suffix `.tar`. Anything else returns `None` and is ignored during
    /// repository discovery.
    #[must_use]
    pub fn parse(file_name: &str) -> Option<Self> {
        let middle = file_name.strip_prefix("data")?.strip_suffix(".tar")?;
        let (digits, file_generation) = match middle.as_bytes().last()? {
            b'a'..=b'z' => (
                &middle[..middle.len() - 1],
                middle.as_bytes()[middle.len() - 1] as char,
            ),
            b'0'..=b'9' => (middle, 'a'),
            _ => return None,
        };
        if digits.len() < 5 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        // The digits are the concatenation of a number without leading zeros
        // and exactly four more digits, so a run longer than five digits must
        // not start with zero.
        if digits.len() > 5 && digits.starts_with('0') {
            return None;
        }
        let archive_number: u32 = digits.parse().ok()?;
        Some(Self {
            file_name: file_name.to_owned(),
            archive_number,
            file_generation,
        })
    }
}

/// Selects the archives a reader must open from a directory listing:
/// for every archive number, only the highest file generation letter.
/// The result is sorted by archive number *descending* (newest first),
/// which is the probe order for segment lookups.
///
/// Two files mapping to the same number *and* letter (possible because a
/// missing letter means `'a'`, so `data00000.tar` and `data00000a.tar`
/// collide) are a fatal state error, exactly as in the Java store —
/// silently picking one of them would make opens nondeterministic. The
/// check covers every pair, not only the currently winning letter, and
/// never depends on directory listing order.
pub fn select_newest_file_generations(
    file_names: &[String],
) -> crate::error::Result<Vec<ArchiveFileName>> {
    Ok(group_file_generations_newest_first(file_names)?
        .into_iter()
        .filter_map(|mut group| {
            if group.is_empty() {
                None
            } else {
                Some(group.remove(0))
            }
        })
        .collect())
}

/// Groups every archive file by number — newest number first, and within
/// each number the generation letters newest first — after the same
/// duplicate validation as [`select_newest_file_generations`]. A reader
/// walks each group in order and takes the first file whose index is
/// valid.
pub fn group_file_generations_newest_first(
    file_names: &[String],
) -> crate::error::Result<Vec<Vec<ArchiveFileName>>> {
    let mut seen_names: BTreeMap<(u32, char), String> = BTreeMap::new();
    let mut by_number: BTreeMap<u32, Vec<ArchiveFileName>> = BTreeMap::new();
    for file_name in file_names {
        let Some(parsed) = ArchiveFileName::parse(file_name) else {
            continue;
        };
        if let Some(existing_name) = seen_names.insert(
            (parsed.archive_number, parsed.file_generation),
            parsed.file_name.clone(),
        ) {
            return Err(crate::error::Error::InvalidFormat {
                details: format!(
                    "archives {} and {} both claim number {} generation {:?}",
                    existing_name, parsed.file_name, parsed.archive_number, parsed.file_generation
                ),
            });
        }
        by_number
            .entry(parsed.archive_number)
            .or_default()
            .push(parsed);
    }
    Ok(by_number
        .into_values()
        .rev()
        .map(|mut group| {
            group.sort_by_key(|name| std::cmp::Reverse(name.file_generation));
            group
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{ArchiveFileName, select_newest_file_generations};

    #[test]
    fn parses_standard_names() {
        let parsed = ArchiveFileName::parse("data00000a.tar").expect("valid name");
        assert_eq!(parsed.archive_number, 0);
        assert_eq!(parsed.file_generation, 'a');

        let parsed = ArchiveFileName::parse("data00012b.tar").expect("valid name");
        assert_eq!(parsed.archive_number, 12);
        assert_eq!(parsed.file_generation, 'b');
    }

    #[test]
    fn missing_generation_letter_defaults_to_a() {
        let parsed = ArchiveFileName::parse("data00003.tar").expect("valid name");
        assert_eq!(parsed.archive_number, 3);
        assert_eq!(parsed.file_generation, 'a');
    }

    #[test]
    fn parses_six_digit_numbers() {
        let parsed = ArchiveFileName::parse("data123456a.tar").expect("valid name");
        assert_eq!(parsed.archive_number, 123_456);
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "data0000a.tar",   // only four digits
            "data000000a.tar", // redundant leading zero in a six-digit run
            "data00000A.tar",  // uppercase generation letter
            "data00000a.bak",  // wrong suffix
            "info00000a.tar",  // wrong prefix
            "data00000a.tar.bak",
            "journal.log",
        ] {
            assert!(
                ArchiveFileName::parse(name).is_none(),
                "{name} must be rejected"
            );
        }
    }

    #[test]
    fn selects_highest_generation_letter_newest_first() {
        let file_names = vec![
            "data00000a.tar".to_owned(),
            "data00000b.tar".to_owned(),
            "data00001a.tar".to_owned(),
            "journal.log".to_owned(),
        ];
        let selected = select_newest_file_generations(&file_names).expect("no duplicates");
        let names: Vec<&str> = selected
            .iter()
            .map(|entry| entry.file_name.as_str())
            .collect();
        assert_eq!(names, ["data00001a.tar", "data00000b.tar"]);
    }

    #[test]
    fn duplicate_number_and_generation_is_fatal() {
        // A missing letter means 'a', so these two names collide.
        let file_names = vec!["data00000.tar".to_owned(), "data00000a.tar".to_owned()];
        assert!(select_newest_file_generations(&file_names).is_err());
    }
}
