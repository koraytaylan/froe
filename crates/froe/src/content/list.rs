//! List records: immutable lists of record identifiers.
//!
//! Lists appear in two shapes:
//!
//! * an *uncounted* list is just a pointer whose meaning depends on a size
//!   the parent record knows: with size 1 the pointer *is* the single
//!   element, otherwise it points at a bucket of up to 255 identifiers,
//!   recursively nested for larger lists. Templates (property names), node
//!   records (property values), and long values (block lists) use this
//!   shape;
//! * a *counted* list prefixes the pointer with its size as a 32-bit
//!   integer, and omits the pointer entirely for an empty list.
//!   Multi-valued properties use this shape.

use crate::content::provider::SegmentProvider;
use crate::error::{Error, Result};
use crate::segment::record::RecordIdentifier;

/// Maximum number of record identifiers per bucket.
const BUCKET_CAPACITY: u64 = 255;

/// Maximum number of elements a list can hold (`255³`).
pub const MAXIMUM_LIST_SIZE: u64 = BUCKET_CAPACITY * BUCKET_CAPACITY * BUCKET_CAPACITY;

/// Validates a list size against the format limit.
fn validate_size(size: u64) -> Result<()> {
    if size > MAXIMUM_LIST_SIZE {
        return Err(Error::InvalidFormat {
            details: format!("list size {size} exceeds the maximum of {MAXIMUM_LIST_SIZE}"),
        });
    }
    Ok(())
}

/// The bucket capacity at the top level of a list of `size` elements:
/// the largest power of 255 that is still below `size`.
fn top_bucket_capacity(size: u64) -> u64 {
    let mut capacity = 1;
    while capacity * BUCKET_CAPACITY < size {
        capacity *= BUCKET_CAPACITY;
    }
    capacity
}

/// Fetches element `index` of the uncounted list `list_identifier` whose
/// size the caller knows from context.
pub fn uncounted_list_entry<Provider: SegmentProvider + ?Sized>(
    provider: &Provider,
    list_identifier: RecordIdentifier,
    size: u64,
    index: u64,
) -> Result<RecordIdentifier> {
    validate_size(size)?;
    if index >= size {
        return Err(Error::InvalidFormat {
            details: format!("list index {index} is out of bounds for a list of {size} elements"),
        });
    }
    let mut current = list_identifier;
    let mut current_size = size;
    let mut current_index = index;
    loop {
        if current_size == 1 {
            // A one-element list points directly at its element.
            return Ok(current);
        }
        let bucket_capacity = top_bucket_capacity(current_size);
        let bucket_index = current_index / bucket_capacity;
        let view = provider.segment(current.segment)?;
        let child = view.read_record_identifier(current.record_number, 0, bucket_index as usize)?;
        current_size = (current_size - bucket_index * bucket_capacity).min(bucket_capacity);
        current_index -= bucket_index * bucket_capacity;
        current = child;
    }
}

/// Reads all elements of the uncounted list `list_identifier` whose size
/// the caller knows from context, in list order.
pub fn uncounted_list_entries(
    provider: &dyn SegmentProvider,
    list_identifier: RecordIdentifier,
    size: u64,
) -> Result<Vec<RecordIdentifier>> {
    validate_size(size)?;
    let mut entries = Vec::with_capacity((size as usize).min(1 << 16));
    collect_entries(provider, list_identifier, size, &mut entries)?;
    Ok(entries)
}

fn collect_entries(
    provider: &dyn SegmentProvider,
    list_identifier: RecordIdentifier,
    size: u64,
    entries: &mut Vec<RecordIdentifier>,
) -> Result<()> {
    if size == 0 {
        return Ok(());
    }
    if size == 1 {
        entries.push(list_identifier);
        return Ok(());
    }
    let bucket_capacity = top_bucket_capacity(size);
    let bucket_count = size.div_ceil(bucket_capacity);
    let view = provider.segment(list_identifier.segment)?;
    for bucket_index in 0..bucket_count {
        let child =
            view.read_record_identifier(list_identifier.record_number, 0, bucket_index as usize)?;
        let child_size = (size - bucket_index * bucket_capacity).min(bucket_capacity);
        collect_entries(provider, child, child_size, entries)?;
    }
    Ok(())
}

/// A parsed counted list: the size prefix plus the optional body pointer.
#[derive(Clone, Copy, Debug)]
pub struct CountedList {
    /// The number of elements.
    pub size: u32,
    /// The uncounted list holding the elements; `None` when the list is
    /// empty.
    pub body: Option<RecordIdentifier>,
}

/// Reads the header of the counted list stored at `list_identifier`:
/// a 32-bit size followed, only when the size is positive, by the record
/// identifier of the element list.
pub fn read_counted_list(
    provider: &dyn SegmentProvider,
    list_identifier: RecordIdentifier,
) -> Result<CountedList> {
    let view = provider.segment(list_identifier.segment)?;
    let size = view.read_u32(list_identifier.record_number, 0)?;
    if size as i32 >= 0 && u64::from(size) <= MAXIMUM_LIST_SIZE {
        let body = if size == 0 {
            None
        } else {
            Some(view.read_record_identifier(list_identifier.record_number, 4, 0)?)
        };
        Ok(CountedList { size, body })
    } else {
        Err(Error::InvalidFormat {
            details: format!("invalid counted list size {}", size as i32),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{uncounted_list_entries, uncounted_list_entry};
    use crate::content::provider::tests::MemorySegmentProvider;
    use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
    use crate::segment::record::RecordIdentifier;

    /// Serializes record identifiers referencing the same segment.
    fn identifier_array(record_numbers: &[u32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &record_number in record_numbers {
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(&record_number.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn one_element_list_is_the_element_itself() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &[(0, 4, vec![0])]));
        let element = RecordIdentifier::new(segment, 0);
        let entry = uncounted_list_entry(&provider, element, 1, 0).expect("entry");
        assert_eq!(entry, element);
        assert_eq!(
            uncounted_list_entries(&provider, element, 1).expect("entries"),
            vec![element]
        );
    }

    #[test]
    fn small_list_reads_from_a_single_bucket() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        // Record 10 is a bucket of three identifiers: records 1, 2, 3.
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, vec![0]),
                    (2, 4, vec![0]),
                    (3, 4, vec![0]),
                    (10, 2, identifier_array(&[1, 2, 3])),
                ],
            ),
        );
        let list = RecordIdentifier::new(segment, 10);
        for (index, expected) in [1u32, 2, 3].iter().enumerate() {
            let entry = uncounted_list_entry(&provider, list, 3, index as u64).expect("entry");
            assert_eq!(entry, RecordIdentifier::new(segment, *expected));
        }
        let all = uncounted_list_entries(&provider, list, 3).expect("entries");
        assert_eq!(all.len(), 3);
        assert_eq!(all[2], RecordIdentifier::new(segment, 3));
        assert!(uncounted_list_entry(&provider, list, 3, 3).is_err());
    }

    #[test]
    fn nested_buckets_resolve_recursively() {
        // A list of 256 elements needs two levels: the top bucket holds two
        // sub-buckets of 255 and 1 elements. The one-element sub-bucket is
        // the element itself (pass-through, no bucket record).
        let segment = data_segment_identifier(1);
        let element_records: Vec<u32> = (0..256).collect();

        // Bucket of the first 255 elements is record 300; element 255 (the
        // 256th) is passed through directly as record 255.
        let mut records: Vec<(u32, u8, Vec<u8>)> = element_records
            .iter()
            .map(|&record_number| (record_number, 4u8, vec![0u8]))
            .collect();
        records.push((300, 2, identifier_array(&(0..255).collect::<Vec<_>>())));
        records.push((301, 2, identifier_array(&[300, 255])));

        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &records));

        let list = RecordIdentifier::new(segment, 301);
        assert_eq!(
            uncounted_list_entry(&provider, list, 256, 0).expect("first"),
            RecordIdentifier::new(segment, 0)
        );
        assert_eq!(
            uncounted_list_entry(&provider, list, 256, 254).expect("last of first bucket"),
            RecordIdentifier::new(segment, 254)
        );
        assert_eq!(
            uncounted_list_entry(&provider, list, 256, 255).expect("pass-through element"),
            RecordIdentifier::new(segment, 255)
        );
        let all = uncounted_list_entries(&provider, list, 256).expect("entries");
        assert_eq!(all.len(), 256);
        assert_eq!(all[255], RecordIdentifier::new(segment, 255));
    }

    #[test]
    fn oversized_lists_are_rejected() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        provider.insert(segment, synthetic_data_segment(&[], &[(0, 4, vec![0])]));
        let list = RecordIdentifier::new(segment, 0);
        let oversized = super::MAXIMUM_LIST_SIZE + 1;
        assert!(uncounted_list_entry(&provider, list, oversized, 0).is_err());
        assert!(uncounted_list_entries(&provider, list, oversized).is_err());
    }

    #[test]
    fn counted_lists_expose_size_and_body() {
        let segment = data_segment_identifier(1);
        let mut provider = MemorySegmentProvider::default();
        // Record 5: counted list of two elements, body bucket at record 6.
        let mut counted = 2u32.to_be_bytes().to_vec();
        counted.extend_from_slice(&[0, 0]);
        counted.extend_from_slice(&6u32.to_be_bytes());
        // Record 7: empty counted list.
        provider.insert(
            segment,
            synthetic_data_segment(
                &[],
                &[
                    (1, 4, vec![0]),
                    (2, 4, vec![0]),
                    (5, 3, counted),
                    (6, 2, identifier_array(&[1, 2])),
                    (7, 3, 0u32.to_be_bytes().to_vec()),
                ],
            ),
        );
        let counted = super::read_counted_list(&provider, RecordIdentifier::new(segment, 5))
            .expect("counted list");
        assert_eq!(counted.size, 2);
        assert_eq!(counted.body, Some(RecordIdentifier::new(segment, 6)));

        let empty = super::read_counted_list(&provider, RecordIdentifier::new(segment, 7))
            .expect("empty counted list");
        assert_eq!(empty.size, 0);
        assert_eq!(empty.body, None);
    }
}
