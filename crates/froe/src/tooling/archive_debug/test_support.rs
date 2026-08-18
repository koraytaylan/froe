//! The record identifiers and value providers the diagnostic tests build
//! their cases from.

use super::{BLOCK_SIZE, MEDIUM_VALUE_LIMIT};
use crate::content::provider::tests::MemorySegmentProvider;
use crate::segment::parsed_segment::tests::{data_segment_identifier, synthetic_data_segment};
use crate::segment::record::RecordIdentifier;

pub(crate) fn local_record_identifier(record_number: u32) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&record_number.to_be_bytes());
    bytes
}

pub(crate) fn medium_value(content: &[u8]) -> Vec<u8> {
    let mut value = (0x8000u16 | (content.len() as u16 - 128))
        .to_be_bytes()
        .to_vec();
    value.extend_from_slice(content);
    value
}

pub(crate) fn provider_with_long_value(
    content: &[u8],
) -> (MemorySegmentProvider, RecordIdentifier) {
    let segment = data_segment_identifier(41);
    let mut records = Vec::new();
    let mut list = Vec::new();
    for (block_index, block) in content.chunks(BLOCK_SIZE as usize).enumerate() {
        let record_number = 1 + block_index as u32;
        records.push((record_number, 5, block.to_vec()));
        list.extend_from_slice(&local_record_identifier(record_number));
    }
    records.push((20, 2, list));
    let mut value = ((content.len() as u64 - MEDIUM_VALUE_LIMIT) | (0x3 << 62))
        .to_be_bytes()
        .to_vec();
    value.extend_from_slice(&local_record_identifier(20));
    records.push((21, 4, value));
    let mut provider = MemorySegmentProvider::default();
    provider.insert(segment, synthetic_data_segment(&[], &records));
    (provider, RecordIdentifier::new(segment, 21))
}
