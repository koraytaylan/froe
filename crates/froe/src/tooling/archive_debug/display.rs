//! Rendering a property the way Oak's diagnostic does, in Java's own
//! string form, with every value bounded before it is materialized.

use super::{
    ArchiveDebugResult, ArchiveDebugWork, BLOCK_SIZE, BoundedDisplay, DisplayBudget, Error,
    MEDIUM_VALUE_LIMIT, PropertyTemplate, PropertyType, RecordIdentifier, Repository,
    SegmentIdentifier, SegmentProvider, WorkBudget, read_counted_list, uncounted_list_entry,
};

/// Oak-style presentation data for one stored property's value.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ArchivePropertyDisplay {
    /// A bounded summary of the first STRING value (also for a non-empty
    /// STRINGS property).
    String {
        /// At most the first 60 Java UTF-16 code units. This can end with an
        /// unpaired high surrogate because Java truncates with `substring`.
        preview_utf16: Vec<u16>,
        /// Full value length in Java UTF-16 code units.
        utf16_length: u64,
    },
    /// An empty STRINGS property. Oak's special STRING rendering prints this
    /// as an unquoted empty value after `name = `.
    EmptyStrings,
    /// The portion following `name = ` in
    /// `AbstractPropertyState.toString`: a scalar, a Java-list-like array,
    /// or binary size/count summary.
    Other(String),
}

impl ArchivePropertyDisplay {
    /// Renders the value portion of Oak's `SegmentPropertyState` diagnostic
    /// line, before terminal sanitization by a presentation layer.
    #[must_use]
    pub fn oak_rendered_value(&self) -> String {
        match self {
            Self::String {
                preview_utf16,
                utf16_length,
            } => java_string_display(preview_utf16, *utf16_length),
            Self::EmptyStrings => String::new(),
            Self::Other(value) => value.clone(),
        }
    }

    pub(crate) fn oak_rendered_value_bytes(&self) -> usize {
        match self {
            Self::String {
                preview_utf16,
                utf16_length,
            } => java_string_display(preview_utf16, *utf16_length).len(),
            Self::EmptyStrings => 0,
            Self::Other(value) => value.len(),
        }
    }
}

pub(crate) fn oak_property_type_name(property_type: PropertyType, is_multiple: bool) -> String {
    let singular = property_type.jcr_name().to_ascii_uppercase();
    if !is_multiple {
        singular
    } else if property_type == PropertyType::Binary {
        "BINARIES".to_owned()
    } else {
        format!("{singular}S")
    }
}

pub(crate) fn java_string_display(preview_utf16: &[u16], utf16_length: u64) -> String {
    use std::fmt::Write as _;

    let mut display = String::from("\"");
    for &unit in preview_utf16 {
        match unit {
            0x08 => display.push_str("\\b"),
            0x09 => display.push_str("\\t"),
            0x0a => display.push_str("\\n"),
            0x0c => display.push_str("\\f"),
            0x0d => display.push_str("\\r"),
            0x22 => display.push_str("\\\""),
            0x5c => display.push_str("\\\\"),
            0x20..=0x7e => display.push(char::from_u32(u32::from(unit)).expect("ASCII unit")),
            _ => write!(display, "\\u{unit:04X}").expect("writing to a String cannot fail"),
        }
    }
    if utf16_length > preview_utf16.len() as u64 {
        write!(display, "... ({utf16_length} chars)").expect("writing to a String cannot fail");
    }
    display.push('"');
    display
}

pub(crate) fn property_display(
    repository: &Repository,
    property: &PropertyTemplate,
    property_identifier: RecordIdentifier,
    work_budget: &mut WorkBudget,
    display_budget: DisplayBudget,
) -> ArchiveDebugResult<ArchivePropertyDisplay> {
    // DebugTars checks the JCR tag rather than scalar-vs-array identity, so
    // both STRING and STRINGS use its special first-value-only rendering.
    if property.property_type == PropertyType::String {
        let value_identifier = if property.is_multiple {
            work_budget.charge_one()?;
            let counted = read_counted_list(repository, property_identifier)?;
            let Some(body) = counted.body else {
                return Ok(ArchivePropertyDisplay::EmptyStrings);
            };
            work_budget.charge_one()?;
            uncounted_list_entry(repository, body, u64::from(counted.size), 0)?
        } else {
            property_identifier
        };
        let (preview_utf16, utf16_length) =
            streamed_string_summary(repository, value_identifier, work_budget)?;
        display_budget
            .check_display_bytes(java_string_display(&preview_utf16, utf16_length).len())?;
        return Ok(ArchivePropertyDisplay::String {
            preview_utf16,
            utf16_length,
        });
    }

    if property.is_multiple {
        work_budget.charge_one()?;
        let counted = read_counted_list(repository, property_identifier)?;
        if property.property_type == PropertyType::Binary {
            let text = format!("[{} binaries]", counted.size);
            display_budget.check_display_bytes(text.len())?;
            return Ok(ArchivePropertyDisplay::Other(text));
        }
        let size = u64::from(counted.size);
        let mut display = display_budget.builder();
        display.push_str("[")?;
        let Some(body) = counted.body else {
            display.push_str("]")?;
            return Ok(ArchivePropertyDisplay::Other(display.into_string()));
        };
        for value_index in 0..size {
            if value_index > 0 {
                display.push_str(", ")?;
            }
            work_budget.charge_one()?;
            let value_identifier = uncounted_list_entry(repository, body, size, value_index)?;
            append_scalar_display(
                repository,
                value_identifier,
                property.property_type,
                work_budget,
                &mut display,
            )?;
        }
        display.push_str("]")?;
        return Ok(ArchivePropertyDisplay::Other(display.into_string()));
    }

    if property.property_type == PropertyType::Binary {
        let text = binary_scalar_display(repository, property_identifier, work_budget)?;
        display_budget.check_display_bytes(text.len())?;
        return Ok(ArchivePropertyDisplay::Other(text));
    }

    let mut display = display_budget.builder();
    append_scalar_display(
        repository,
        property_identifier,
        property.property_type,
        work_budget,
        &mut display,
    )?;
    Ok(ArchivePropertyDisplay::Other(display.into_string()))
}

pub(crate) fn append_scalar_display(
    repository: &Repository,
    value_identifier: RecordIdentifier,
    property_type: PropertyType,
    work_budget: &mut WorkBudget,
    display: &mut BoundedDisplay,
) -> ArchiveDebugResult<()> {
    match property_type {
        PropertyType::Binary | PropertyType::String => Err(Error::InvalidFormat {
            details: format!(
                "property type {} cannot use ordinary scalar rendering at {value_identifier}",
                property_type.jcr_name()
            ),
        }
        .into()),
        PropertyType::Boolean => {
            let mut position = 0usize;
            let mut is_true = true;
            decode_value_utf8(repository, value_identifier, work_budget, |character| {
                const TRUE: [char; 4] = ['t', 'r', 'u', 'e'];
                is_true &= position < TRUE.len() && character.eq_ignore_ascii_case(&TRUE[position]);
                position = position.saturating_add(1);
                Ok(())
            })?;
            if position != 4 {
                is_true = false;
            }
            display.push_str(if is_true { "true" } else { "false" })
        }
        PropertyType::Long => {
            let start = display.text.len();
            decode_value_utf8(repository, value_identifier, work_budget, |character| {
                display.push_char(character)
            })?;
            let stored_length = display.text.len() - start;
            let parsed = display.text[start..].parse::<i64>().map_err(|_| {
                Error::InvalidFormat {
                    details: format!(
                        "stored long value at {value_identifier} has {stored_length} UTF-8 bytes \
                         and cannot be decoded"
                    ),
                }
            })?;
            display.text.truncate(start);
            display.push_str(&parsed.to_string())
        }
        PropertyType::Double => {
            let start = display.text.len();
            decode_value_utf8(repository, value_identifier, work_budget, |character| {
                display.push_char(character)
            })?;
            let stored_length = display.text.len() - start;
            display.text[start..]
                .parse::<f64>()
                .map_err(|_| Error::InvalidFormat {
                    details: format!(
                        "stored double value at {value_identifier} has {stored_length} UTF-8 \
                         bytes and cannot be decoded"
                    ),
                })?;
            // Oak stores Double.toString's canonical spelling. Keeping that
            // validated spelling preserves values such as Double.MIN_VALUE
            // (`4.9E-324`) that Rust's formatter spells differently.
            Ok(())
        }
        PropertyType::Date
        | PropertyType::Name
        | PropertyType::Path
        | PropertyType::Reference
        | PropertyType::WeakReference
        | PropertyType::Uri
        | PropertyType::Decimal => {
            decode_value_utf8(repository, value_identifier, work_budget, |character| {
                display.push_char(character)
            })
        }
    }
}

pub(crate) fn streamed_string_summary(
    provider: &dyn SegmentProvider,
    value_identifier: RecordIdentifier,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<(Vec<u16>, u64)> {
    let mut preview_utf16 = Vec::with_capacity(60);
    let mut utf16_length = 0u64;
    decode_value_utf8(provider, value_identifier, work_budget, |character| {
        let mut encoded = [0u16; 2];
        let units = character.encode_utf16(&mut encoded);
        utf16_length = utf16_length
            .checked_add(units.len() as u64)
            .ok_or_else(|| Error::InvalidFormat {
                details: format!("UTF-16 length overflows for string value {value_identifier}"),
            })?;
        let remaining = 60usize.saturating_sub(preview_utf16.len());
        preview_utf16.extend_from_slice(&units[..units.len().min(remaining)]);
        Ok(())
    })?;
    Ok((preview_utf16, utf16_length))
}

pub(crate) fn decode_value_utf8(
    provider: &dyn SegmentProvider,
    value_identifier: RecordIdentifier,
    work_budget: &mut WorkBudget,
    mut consume: impl FnMut(char) -> ArchiveDebugResult<()>,
) -> ArchiveDebugResult<()> {
    use crate::content::value::read_binary_stream;

    // String records use a 62-bit long-length mask and Java's signed-int
    // length limit, while the generic binary stream uses a 61-bit mask.
    // Preflight the `110xxxxx` string form and reject binary markers before
    // opening that generic stream.
    work_budget.charge_one()?;
    let view = provider.segment(value_identifier.segment)?;
    let head = view.read_u8(value_identifier.record_number, 0)?;
    let is_long = head & 0xe0 == 0xc0;
    if is_long {
        let stored = view.read_u64(value_identifier.record_number, 0)?;
        let string_length = (stored & 0x3fff_ffff_ffff_ffff) + MEDIUM_VALUE_LIMIT;
        if string_length >= i32::MAX as u64 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "string of {string_length} bytes in record {value_identifier} is too long"
                ),
            }
            .into());
        }
    } else if head & 0x80 != 0 && head & 0x40 != 0 {
        return Err(Error::InvalidFormat {
            details: format!(
                "record {value_identifier} starts with binary marker {head:#04x} and is not a \
                 string"
            ),
        }
        .into());
    }

    work_budget.charge_one()?;
    let mut stream = read_binary_stream(provider, value_identifier)?;
    let mut buffer = [0u8; BLOCK_SIZE as usize];
    let mut pending = Vec::with_capacity(buffer.len() + 3);
    while stream.position() < stream.len() {
        let remaining = stream.len() - stream.position();
        let requested_bytes = if is_long {
            remaining
                .min(BLOCK_SIZE - stream.position() % BLOCK_SIZE)
                .min(buffer.len() as u64)
        } else {
            remaining.min(buffer.len() as u64)
        };
        let lookup_work = if is_long { 2 } else { 1 };
        work_budget.charge_amount(requested_bytes.saturating_add(lookup_work))?;
        let read_length = stream.read_chunk(&mut buffer)?;
        if read_length == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..read_length]);
        consume_utf8_prefix(&mut pending, ValueRemainder::MoreToCome, &mut consume)?;
    }
    consume_utf8_prefix(&mut pending, ValueRemainder::EndOfValue, &mut consume)
}

/// Whether more bytes of the value can still arrive after this chunk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueRemainder {
    /// More bytes follow, so a truncated multi-byte sequence at the end is
    /// held back rather than replaced.
    MoreToCome,
    /// The last chunk: a truncated sequence can only be replacement text.
    EndOfValue,
}

pub(crate) fn consume_utf8_prefix(
    pending: &mut Vec<u8>,
    remainder: ValueRemainder,
    consume: &mut impl FnMut(char) -> ArchiveDebugResult<()>,
) -> ArchiveDebugResult<()> {
    let mut consumed = 0usize;
    while consumed < pending.len() {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(text) => {
                for character in text.chars() {
                    consume(character)?;
                }
                consumed = pending.len();
            }
            Err(error) => {
                let valid_end = consumed + error.valid_up_to();
                let valid = std::str::from_utf8(&pending[consumed..valid_end])
                    .expect("from_utf8 validated this prefix");
                for character in valid.chars() {
                    consume(character)?;
                }
                consumed = valid_end;
                if let Some(error_length) = error.error_len() {
                    consume('\u{fffd}')?;
                    consumed = consumed.saturating_add(error_length);
                } else if remainder == ValueRemainder::EndOfValue {
                    consume('\u{fffd}')?;
                    consumed = pending.len();
                } else {
                    break;
                }
            }
        }
    }
    pending.drain(..consumed);
    Ok(())
}

pub(crate) fn binary_scalar_display(
    provider: &dyn SegmentProvider,
    value_identifier: RecordIdentifier,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<String> {
    work_budget.charge_one()?;
    // Oak's AbstractPropertyState.getBinarySize catches every exception from
    // SegmentPropertyState.size. Preserve that last-resort diagnostic
    // behavior for corrupt records as well as unavailable external blobs.
    let length = binary_scalar_length(provider, value_identifier).unwrap_or(None);
    Ok(length.map_or_else(
        || "{-1 bytes}".to_owned(),
        |length| format!("{{{length} bytes}}"),
    ))
}

pub(crate) fn binary_scalar_length(
    provider: &dyn SegmentProvider,
    value_identifier: RecordIdentifier,
) -> crate::error::Result<Option<u64>> {
    let view = provider.segment(value_identifier.segment)?;
    let head = view.read_u8(value_identifier.record_number, 0)?;
    let length = if head & 0x80 == 0 {
        Some(u64::from(head))
    } else if head & 0x40 == 0 {
        Some(u64::from(view.read_u16(value_identifier.record_number, 0)? & 0x3fff) + 128)
    } else if head & 0x20 == 0 {
        Some(
            (view.read_u64(value_identifier.record_number, 0)? & 0x1fff_ffff_ffff_ffff)
                + MEDIUM_VALUE_LIMIT,
        )
    } else if head & 0x10 == 0 || head & 0x08 == 0 {
        None
    } else {
        return Err(Error::InvalidFormat {
            details: format!(
                "unexpected value record marker {head:#04x} in record {value_identifier}"
            ),
        });
    };
    Ok(length)
}

pub(crate) fn has_matching_binary_block_segment(
    repository: &Repository,
    property_identifier: RecordIdentifier,
    is_multiple: bool,
    archive: &crate::tar_archive::TarArchiveReader,
    work: &mut ArchiveDebugWork,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<bool> {
    let mut matches_archive = false;
    if is_multiple {
        work_budget.charge_one()?;
        let counted = read_counted_list(repository, property_identifier)?;
        let Some(body) = counted.body else {
            return Ok(false);
        };
        let size = u64::from(counted.size);
        for value_index in 0..size {
            work_budget.charge_one()?;
            let value_identifier = uncounted_list_entry(repository, body, size, value_index)?;
            matches_archive |= long_binary_has_matching_block_segment(
                repository,
                value_identifier,
                property_identifier.segment,
                archive,
                work,
                work_budget,
            )?;
        }
    } else {
        matches_archive = long_binary_has_matching_block_segment(
            repository,
            property_identifier,
            property_identifier.segment,
            archive,
            work,
            work_budget,
        )?;
    }
    Ok(matches_archive)
}

pub(crate) fn long_binary_has_matching_block_segment(
    repository: &Repository,
    value_identifier: RecordIdentifier,
    property_segment: SegmentIdentifier,
    archive: &crate::tar_archive::TarArchiveReader,
    work: &mut ArchiveDebugWork,
    work_budget: &mut WorkBudget,
) -> ArchiveDebugResult<bool> {
    work_budget.charge_one()?;
    let view = repository.segment(value_identifier.segment)?;
    let head = view.read_u8(value_identifier.record_number, 0)?;
    // `110xxxxx` is the only encoding backed by block records. Small and
    // medium values live in the value record; `111xxxxx` are external blob
    // identifiers with no segment block records.
    if head & 0xe0 != 0xc0 {
        return Ok(false);
    }
    let length = (view.read_u64(value_identifier.record_number, 0)? & 0x1fff_ffff_ffff_ffff)
        + MEDIUM_VALUE_LIMIT;
    let block_count = length.div_ceil(BLOCK_SIZE);
    let list_identifier = view.read_record_identifier(value_identifier.record_number, 8, 0)?;
    let mut matches_archive = false;
    for block_index in 0..block_count {
        work.inspected_binary_blocks += 1;
        work_budget.charge_one()?;
        let block_identifier =
            uncounted_list_entry(repository, list_identifier, block_count, block_index)?;
        if block_identifier.segment != property_segment
            && archive.contains_segment(block_identifier.segment)
        {
            matches_archive = true;
        }
    }
    Ok(matches_archive)
}

#[cfg(test)]
mod tests {
    use super::streamed_string_summary;
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;
    use crate::tooling::archive_debug::options::WorkBudget;
    use crate::tooling::archive_debug::outcome::ArchiveDebugError;
    use crate::tooling::archive_debug::test_support::{medium_value, provider_with_long_value};

    #[test]
    fn string_summary_streams_medium_and_long_boundaries_in_java_utf16_units() {
        let medium_segment = data_segment_identifier(40);
        let medium_text = vec![b'x'; 16_511];
        let mut medium_provider = MemorySegmentProvider::default();
        medium_provider.insert(
            medium_segment,
            synthetic_data_segment(&[], &[(1, 4, medium_value(&medium_text))]),
        );
        let mut work_budget = WorkBudget::new(u64::MAX);
        let (preview, length) = streamed_string_summary(
            &medium_provider,
            RecordIdentifier::new(medium_segment, 1),
            &mut work_budget,
        )
        .expect("16,511-byte medium string");
        assert_eq!(preview, vec![u16::from(b'x'); 60]);
        assert_eq!(length, 16_511);

        // The emoji straddles Java's 60-char preview boundary (only its
        // high surrogate is retained), while `é` straddles a 4 KiB block
        // boundary in UTF-8. The complete byte length is the first long
        // value boundary.
        let mut long_text = "a".repeat(59).into_bytes();
        long_text.extend_from_slice("\u{1f600}".as_bytes());
        long_text.extend(std::iter::repeat_n(b'x', 4_095 - long_text.len()));
        long_text.extend_from_slice("\u{e9}".as_bytes());
        long_text.extend(std::iter::repeat_n(b'x', 16_512 - long_text.len()));
        let (long_provider, long_identifier) = provider_with_long_value(&long_text);
        let mut work_budget = WorkBudget::new(u64::MAX);
        let (preview, length) =
            streamed_string_summary(&long_provider, long_identifier, &mut work_budget)
                .expect("16,512-byte long string");
        assert_eq!(&preview[..59], &vec![u16::from(b'a'); 59]);
        assert_eq!(
            preview[59], 0xd83d,
            "Java substring can split a surrogate pair"
        );
        assert_eq!(preview.len(), 60);
        assert_eq!(length, 16_509);
    }

    #[test]
    fn string_summary_preflights_payload_bytes_against_the_work_budget() {
        let content = vec![b'x'; 16_512];
        let (provider, identifier) = provider_with_long_value(&content);
        let mut work_budget = WorkBudget::new(100);

        assert!(matches!(
            streamed_string_summary(&provider, identifier, &mut work_budget),
            Err(ArchiveDebugError::WorkBudgetExceeded {
                maximum_work_units: 100,
                attempted_work_units: 4_100,
            })
        ));
        assert_eq!(
            work_budget.consumed, 2,
            "the first 4 KiB payload block is refused before it is read"
        );
    }

    #[test]
    fn string_summary_enforces_the_java_long_string_limit_before_streaming() {
        let segment = data_segment_identifier(42);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[(1, 4, 0xdfff_ffff_ffff_ffffu64.to_be_bytes().to_vec())],
            ),
        );
        let mut work_budget = WorkBudget::new(u64::MAX);

        match streamed_string_summary(
            &provider,
            RecordIdentifier::new(segment, 1),
            &mut work_budget,
        ) {
            Err(ArchiveDebugError::Repository(crate::Error::InvalidFormat { details })) => {
                assert!(details.contains("2305843009213710463 bytes"), "{details}");
                assert!(details.contains("is too long"), "{details}");
            }
            other => panic!("expected the Java string-length limit, got {other:?}"),
        }
        assert_eq!(work_budget.consumed, 1);
    }

    #[test]
    fn string_summary_rejects_external_binary_markers_canonically() {
        let segment = data_segment_identifier(43);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(
            segment,
            synthetic_data_segment(&[], &[(1, 4, vec![0xe0, 0, 0, 0, 0, 0, 0, 0])]),
        );
        let mut work_budget = WorkBudget::new(u64::MAX);

        match streamed_string_summary(
            &provider,
            RecordIdentifier::new(segment, 1),
            &mut work_budget,
        ) {
            Err(ArchiveDebugError::Repository(crate::Error::InvalidFormat { details })) => {
                assert!(details.contains("binary marker 0xe0"), "{details}");
                assert!(details.contains("is not a string"), "{details}");
            }
            other => panic!("expected a string marker error, got {other:?}"),
        }
        assert_eq!(work_budget.consumed, 1);
    }
}
