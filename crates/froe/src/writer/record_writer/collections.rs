//! Writing the two container records: a counted list of buckets, and a
//! map as branches over leaves.

use super::{
    Error, RecordIdentifier, RecordType, RecordWriter, Result, SegmentSink, compare_utf16_strings,
    map_entry_hash,
};

/// Maximum identifiers per list bucket.
pub(crate) const LIST_BUCKET_CAPACITY: usize = 255;

/// Maximum entries in a map leaf below the deepest trie level.
pub(crate) const MAP_LEAF_CAPACITY: usize = 32;

/// The deepest map trie level; records at this level are always leaves.
pub(crate) const MAP_MAXIMUM_LEVEL: u32 = 7;

impl<Sink: SegmentSink> RecordWriter<Sink> {
    /// Writes the body of an uncounted list: `None` for an empty list,
    /// the single element itself for one entry, and a bucket tree above.
    pub fn write_list_body(
        &mut self,
        identifiers: &[RecordIdentifier],
    ) -> Result<Option<RecordIdentifier>> {
        if identifiers.is_empty() {
            return Ok(None);
        }
        let mut level: Vec<RecordIdentifier> = identifiers.to_vec();
        while level.len() > 1 {
            let mut next_level = Vec::with_capacity(level.len().div_ceil(LIST_BUCKET_CAPACITY));
            for chunk in level.chunks(LIST_BUCKET_CAPACITY) {
                if chunk.len() == 1 {
                    // Single-element chunks pass through unwrapped.
                    next_level.push(chunk[0]);
                    continue;
                }
                let record = self.allocate(RecordType::ListBucket, chunk.len() * 6, chunk)?;
                for (position, identifier) in chunk.iter().enumerate() {
                    self.write_identifier_at(record, position * 6, *identifier);
                }
                next_level.push(self.identifier_of(record));
            }
            level = next_level;
        }
        Ok(Some(level[0]))
    }

    /// Writes a counted list record: a size prefix plus the body pointer
    /// when non-empty.
    pub fn write_counted_list(
        &mut self,
        identifiers: &[RecordIdentifier],
    ) -> Result<RecordIdentifier> {
        let body = self.write_list_body(identifiers)?;
        match body {
            None => {
                let record = self.allocate(RecordType::List, 4, &[])?;
                self.current.record_bytes_mut(record)[0..4].copy_from_slice(&0u32.to_be_bytes());
                Ok(self.identifier_of(record))
            }
            Some(body) => {
                let record = self.allocate(RecordType::List, 4 + 6, &[body])?;
                let count = (identifiers.len() as u32).to_be_bytes();
                self.current.record_bytes_mut(record)[0..4].copy_from_slice(&count);
                self.write_identifier_at(record, 4, body);
                Ok(self.identifier_of(record))
            }
        }
    }

    /// Writes a child map: keys become string records, the structure a
    /// hash trie of leaf and branch records. Fails on duplicate names and
    /// on maps of `MapRecord.MAX_SIZE` entries or more — Java's writer
    /// enforces both (its `Map`-typed API makes duplicates impossible),
    /// and packing a larger size would silently corrupt the head's level
    /// bits.
    pub fn write_map(
        &mut self,
        entries: &[(String, RecordIdentifier)],
    ) -> Result<RecordIdentifier> {
        // Java: checkIndex(size, MapRecord.MAX_SIZE) with
        // MAX_SIZE = (1 << 29) - 1, so size == MAX_SIZE is already
        // rejected before any head word is packed.
        if entries.len() >= (1 << 29) - 1 {
            return Err(Error::InvalidFormat {
                details: format!("a child map of {} entries exceeds MAX_SIZE", entries.len()),
            });
        }
        let mut prepared: Vec<(u32, String, RecordIdentifier, RecordIdentifier)> =
            Vec::with_capacity(entries.len());
        let mut names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (name, value) in entries {
            if !names.insert(name.as_str()) {
                return Err(Error::InvalidFormat {
                    details: format!("duplicate child name {name:?} in a map"),
                });
            }
            let key_identifier = self.write_string(name)?;
            prepared.push((map_entry_hash(name), name.clone(), key_identifier, *value));
        }
        self.write_map_bucket(&mut prepared, 0)
    }

    /// Writes one map trie level: a leaf for small buckets or the deepest
    /// level, a branch of sub-buckets otherwise.
    pub(super) fn write_map_bucket(
        &mut self,
        entries: &mut [(u32, String, RecordIdentifier, RecordIdentifier)],
        level: u32,
    ) -> Result<RecordIdentifier> {
        if entries.len() <= MAP_LEAF_CAPACITY || level == MAP_MAXIMUM_LEVEL {
            return self.write_map_leaf(entries, level);
        }
        // Partition by five hash bits at this level (Java's masked shift).
        let shift = (32i32 - (level as i32 + 1) * 5) & 31;
        let mut buckets: Vec<Vec<(u32, String, RecordIdentifier, RecordIdentifier)>> =
            vec![Vec::new(); 32];
        for entry in entries.iter() {
            let bucket_index = (((entry.0 as i32) >> shift) & 0x1F) as usize;
            buckets[bucket_index].push(entry.clone());
        }
        let mut bitmap = 0u32;
        let mut bucket_identifiers = Vec::new();
        for (bucket_index, mut bucket) in buckets.into_iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            bitmap |= 1 << bucket_index;
            bucket_identifiers.push(self.write_map_bucket(&mut bucket, level + 1)?);
        }
        let record = self.allocate(
            RecordType::MapBranch,
            4 + 4 + bucket_identifiers.len() * 6,
            &bucket_identifiers,
        )?;
        let head = (level << 29) | entries.len() as u32;
        {
            let bytes = self.current.record_bytes_mut(record);
            bytes[0..4].copy_from_slice(&head.to_be_bytes());
            bytes[4..8].copy_from_slice(&bitmap.to_be_bytes());
        }
        for (position, identifier) in bucket_identifiers.iter().enumerate() {
            self.write_identifier_at(record, 8 + position * 6, *identifier);
        }
        Ok(self.identifier_of(record))
    }

    /// Writes a map leaf: sorted hashes, then interleaved key and value
    /// identifiers.
    pub(super) fn write_map_leaf(
        &mut self,
        entries: &mut [(u32, String, RecordIdentifier, RecordIdentifier)],
        level: u32,
    ) -> Result<RecordIdentifier> {
        entries.sort_by(|first, second| {
            first
                .0
                .cmp(&second.0)
                .then_with(|| compare_utf16_strings(&first.1, &second.1))
        });
        let all_identifiers: Vec<RecordIdentifier> = entries
            .iter()
            .flat_map(|entry| [entry.2, entry.3])
            .collect();
        let record = self.allocate(
            RecordType::MapLeaf,
            4 + entries.len() * 4 + entries.len() * 12,
            &all_identifiers,
        )?;
        let head = (level << 29) | entries.len() as u32;
        self.current.record_bytes_mut(record)[0..4].copy_from_slice(&head.to_be_bytes());
        for (position, entry) in entries.iter().enumerate() {
            let hash = entry.0.to_be_bytes();
            self.current.record_bytes_mut(record)[4 + position * 4..8 + position * 4]
                .copy_from_slice(&hash);
        }
        let identifiers_base = 4 + entries.len() * 4;
        for (position, entry) in entries.iter().enumerate() {
            self.write_identifier_at(record, identifiers_base + position * 12, entry.2);
            self.write_identifier_at(record, identifiers_base + position * 12 + 6, entry.3);
        }
        Ok(self.identifier_of(record))
    }
}

#[cfg(test)]
mod tests {
    use crate::content::list::read_counted_list;
    use crate::content::map::{map_entries, map_entry};
    use crate::segment::record::RecordIdentifier;
    use crate::writer::record_writer::test_support::new_writer;

    #[test]
    fn counted_lists_round_trip_across_bucket_boundaries() {
        let mut writer = new_writer();
        let elements: Vec<RecordIdentifier> = (0..600)
            .map(|index| {
                writer
                    .write_string(&format!("element-{index}"))
                    .expect("write")
            })
            .collect();
        let list = writer.write_counted_list(&elements).expect("write list");
        let empty = writer.write_counted_list(&[]).expect("write empty");
        let store = writer.finish().expect("finish");

        let counted = read_counted_list(&store, list).expect("read");
        assert_eq!(counted.size, 600);
        let body = counted.body.expect("non-empty body");
        let read_back =
            crate::content::list::uncounted_list_entries(&store, body, 600).expect("entries");
        assert_eq!(read_back, elements);

        assert_eq!(read_counted_list(&store, empty).expect("read").size, 0);
    }

    #[test]
    fn maps_round_trip_as_branches_and_leaves() {
        let mut writer = new_writer();
        let targets: Vec<(String, RecordIdentifier)> = (0..100)
            .map(|index| {
                let name = format!("child-{index:03}");
                let target = writer
                    .write_string(&format!("target-{index}"))
                    .expect("write");
                (name, target)
            })
            .collect();
        let map = writer.write_map(&targets).expect("write map");
        let store = writer.finish().expect("finish");

        assert_eq!(
            crate::content::map::map_size(&store, map).expect("size"),
            100
        );
        for (name, target) in &targets {
            assert_eq!(
                map_entry(&store, map, name).expect("lookup").as_ref(),
                Some(target),
                "{name}"
            );
        }
        assert_eq!(map_entry(&store, map, "absent").expect("lookup"), None);

        let mut enumerated: Vec<String> = map_entries(&store, map)
            .expect("entries")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        enumerated.sort();
        let mut expected: Vec<String> = targets.iter().map(|(name, _)| name.clone()).collect();
        expected.sort();
        assert_eq!(enumerated, expected);
    }
}
