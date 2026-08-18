//! The segments this session has written: recording them, rereading one
//! from disk, and the certificates proving its finalized archive holds
//! exactly what the writer produced.

use super::*;

/// Walks every archive this session wrote and proves each of its segments
/// is exactly what the session recorded: right archive, right position,
/// right payload, right generation, and trailers that agree.
///
/// Returns the segments and archives actually seen, so the caller can name
/// anything the session expected but the disk does not hold.
pub(crate) fn certify_session_archives<'archives>(
    provider: &crate::store::Repository,
    archives: &'archives [TarArchiveReader],
    base_names: &std::collections::HashSet<&str>,
    expected_segments: &HashMap<SegmentIdentifier, SessionSegment>,
    expected_archive_order: &HashMap<String, Vec<SegmentIdentifier>>,
) -> Result<(
    std::collections::HashSet<SegmentIdentifier>,
    std::collections::HashSet<&'archives str>,
)> {
    let mut seen = std::collections::HashSet::new();
    let mut seen_archives = std::collections::HashSet::new();
    for archive in archives
        .iter()
        .filter(|archive| !base_names.contains(archive.file_name()))
    {
        if archive.is_recovered() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} has no valid index",
                    archive.file_name()
                ),
            });
        }
        if archive.segment_count() == 0 {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} contains no session segments",
                    archive.file_name()
                ),
            });
        }
        let expected_order = expected_archive_order
            .get(archive.file_name())
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "finalized archive {} was not created by the current write session",
                    archive.file_name()
                ),
            })?;
        let mut actual_in_file_order = archive
            .index()
            .expect("a non-recovered session archive has an index")
            .entries()
            .to_vec();
        actual_in_file_order.sort_by_key(|entry| entry.position);
        let actual_order: Vec<_> = actual_in_file_order
            .iter()
            .map(|entry| entry.segment_identifier)
            .collect();
        if actual_order.as_slice() != expected_order.as_slice() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} changed the physical write order or archive boundary of its session segments",
                    archive.file_name()
                ),
            });
        }
        seen_archives.insert(archive.file_name());
        let (expected_graph, expected_binary_references) =
            certify_archive_segments(provider, archive, expected_segments, &mut seen)?;
        validate_exact_archive_trailers(
            archive,
            archive.file_name(),
            &expected_graph,
            &expected_binary_references,
        )?;
    }
    Ok((seen, seen_archives))
}

impl WritableRepository {
    /// Appends one entry to the session's physical write-order ledger.
    ///
    /// The archive name is shared with the other writes to the same archive
    /// rather than allocated per segment: writes go to one archive until it
    /// rotates, so the previous entry almost always already holds it.
    pub(in crate::writer::store_writer) fn record_session_write(
        &self,
        archive_file_name: &str,
        identifier: SegmentIdentifier,
    ) {
        let mut writes = self
            .session_segment_writes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shared_name = match writes.last() {
            Some(previous) if *previous.archive_file_name == *archive_file_name => {
                Arc::clone(&previous.archive_file_name)
            }
            _ => Arc::from(archive_file_name),
        };
        writes.push(SessionSegmentWrite {
            archive_file_name: shared_name,
            identifier,
        });
    }

    /// Re-reads a session segment from the archive it was written to.
    ///
    /// Rotated archives are reopened mappings and answer directly. The
    /// archive still being written answers through the writer's positional
    /// read-back; the cache budget keeps that archive resident, so this is
    /// the path a smaller-than-default budget would take rather than the
    /// ordinary one. Nothing here holds the write-state lock across a
    /// provider call, and no caller reaches a provider read while holding it.
    pub(in crate::writer::store_writer) fn reread_session_segment(
        &self,
        segment_identifier: SegmentIdentifier,
    ) -> Result<SegmentView<'_>> {
        let bytes = {
            let session_archives = self
                .session_archives
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let rotated = session_archives
                .iter()
                .find_map(|archive| archive.segment_data(segment_identifier))
                .map(<[u8]>::to_vec);
            if let Some(bytes) = rotated {
                bytes
            } else {
                let state = self.lock_write_state();
                let open = state
                    .tar_writer
                    .as_ref()
                    .and_then(|writer| writer.read_segment(segment_identifier).transpose())
                    .transpose()?;
                open.ok_or(Error::SegmentNotFound { segment_identifier })?
            }
        };
        let structure = Arc::new(ParsedSegment::parse(segment_identifier, &bytes)?);
        let shared = (structure, Arc::new(bytes));
        self.session_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(segment_identifier, shared.clone());
        Ok(SegmentView {
            structure: shared.0,
            bytes: crate::segment::view::SegmentBytes::Shared(shared.1),
        })
    }

    /// Whether any source of this store holds the segment.
    #[must_use]
    pub fn contains_segment(&self, segment_identifier: SegmentIdentifier) -> bool {
        self.session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&segment_identifier)
            || self
                .base_archives
                .iter()
                .any(|archive| archive.contains_segment(segment_identifier))
    }

    /// The garbage collection generation of an existing segment, from the
    /// archive index or the session state.
    #[must_use]
    pub fn segment_generation(
        &self,
        segment_identifier: SegmentIdentifier,
    ) -> Option<GarbageCollectionGeneration> {
        if let Some(session) = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&segment_identifier)
        {
            return Some(session.generation);
        }
        for archive in &self.base_archives {
            if let Some(entry) = archive.index_entry(segment_identifier) {
                return Some(GarbageCollectionGeneration {
                    generation: entry.generation,
                    full_generation: entry.full_generation,
                    is_compacted: entry.is_compacted,
                });
            }
            if archive.contains_segment(segment_identifier) {
                // Recovered archive without index metadata: parse the header.
                return self.segment(segment_identifier).ok().map(|view| {
                    GarbageCollectionGeneration {
                        generation: view.structure.generation,
                        full_generation: view.structure.full_generation,
                        is_compacted: view.structure.is_compacted,
                    }
                });
            }
        }
        None
    }

    /// The generation new, non-compacting writes must use: the head
    /// segment's generation with the compacted flag cleared.
    pub fn writing_generation(&self) -> Result<GarbageCollectionGeneration> {
        let head = self.head();
        let generation = self
            .segment_generation(head.segment)
            .ok_or(Error::SegmentNotFound {
                segment_identifier: head.segment,
            })?;
        Ok(GarbageCollectionGeneration {
            is_compacted: false,
            ..generation
        })
    }

    /// Persists one built segment: appends it to the current archive
    /// (rotating past the size threshold) and makes it readable.
    pub fn persist_segment(&self, segment: BuiltSegment) -> Result<()> {
        let structure = Arc::new(ParsedSegment::parse(segment.identifier, &segment.bytes)?);

        let mut state = self.lock_write_state();
        if self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&segment.identifier)
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "segment {} was written more than once in the current session",
                    segment.identifier
                ),
            });
        }
        if state.tar_writer.is_none() {
            let archive_number = state.next_archive_number.take().ok_or_else(|| {
                Error::InvalidFormat {
                    details: "the archive-number namespace is exhausted at u32::MAX; refusing to wrap to data00000a.tar"
                        .to_owned(),
                }
            })?;
            state.next_archive_number = archive_number.checked_add(1);
            let file_name = format!("data{archive_number:05}a.tar");
            state.tar_writer = Some(if self.seal_archive_before_head {
                // Prepared cleanup must never truncate unexplained residue
                // that appeared after planning, even at the otherwise-next
                // archive number.
                TarArchiveWriter::new_exclusive(&self.directory, &file_name)
            } else {
                TarArchiveWriter::new(&self.directory, &file_name)
            });
        }
        let tar_writer = state
            .tar_writer
            .as_mut()
            .ok_or_else(|| Error::InvalidFormat {
                details: "the archive writer disappeared while locked".to_owned(),
            })?;
        let archive_file_name = tar_writer
            .path()
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| Error::InvalidFormat {
                details: format!(
                    "session archive path {} has no UTF-8 file name",
                    tar_writer.path().display()
                ),
            })?
            .to_owned();
        let tar_generation = if segment.identifier.is_data_segment() {
            segment.generation
        } else {
            GarbageCollectionGeneration {
                generation: 0,
                full_generation: 0,
                is_compacted: false,
            }
        };
        let length = tar_writer.write_segment(
            segment.identifier,
            &segment.bytes,
            tar_generation,
            &segment.referenced_segments,
            &segment.binary_reference_identifiers,
        )?;
        let finished = (length >= self.maximum_archive_size)
            .then(|| state.tar_writer.take())
            .flatten();
        if let Some(finished) = finished {
            self.close_archive_writer(finished)?;
        }

        self.session_segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                segment.identifier,
                SessionSegment {
                    generation: stored_segment_generation(segment.identifier, &structure),
                    payload_crc: crate::checksum::crc32(&segment.bytes),
                },
            );
        // The payload goes to the read-back cache, not to permanent session
        // state: it is already durable in the archive this call just appended
        // it to, and the cache is sized to keep the open archive resident.
        self.session_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(segment.identifier, (structure, Arc::new(segment.bytes)));
        self.record_session_write(&archive_file_name, segment.identifier);
        drop(state);
        Ok(())
    }

    /// Flushes with Oak's durability ordering: archive fsync first, then
    /// — only when the head moved since the last flush — one appended
    /// journal line, fdatasynced. Pending segment bytes are forced to
    /// disk even when the head is unchanged, exactly like Java's
    /// `flush()`; only the journal line is conditional.
    pub fn flush(&self) -> Result<()> {
        let mut state = self.lock_write_state();
        let head_moved = state.persisted_head != Some(state.head);
        let finalized_session_certificate = if self.seal_archive_before_head && head_moved {
            // Cleanup is an offline maintenance transaction. Its newly
            // checkpoint-free head must never become durable while its TAR
            // still lacks graph/catalog/index trailers. Consume and fully
            // close the writer, persist the directory entry, then traverse
            // the exact head through fresh on-disk archive readers.
            if let Some(tar_writer) = state.tar_writer.take() {
                self.close_archive_writer(tar_writer)?;
            }
            sync_directory_strict(&self.directory)?;
            let certificate = self.validate_finalized_session(Some(state.head))?;
            #[cfg(test)]
            certificate.substitute_first_path_if_armed("checkpoint.tar-durable-before-journal")?;
            #[cfg(test)]
            crate::writer::fault_injection::crash_if_armed("checkpoint.tar-durable-before-journal");
            Some(certificate)
        } else if let Some(tar_writer) = &mut state.tar_writer {
            tar_writer.flush()?;
            None
        } else {
            None
        };
        if !head_moved {
            return Ok(());
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let head = state.head;
        if let Some(certificate) = &finalized_session_certificate {
            certificate.recertify()?;
        }
        let line = format!(
            "{}:{} root {timestamp}\n",
            head.segment, head.record_number as i32
        );
        // A crash may leave the previous journal append without its line
        // terminator. Appending the next head directly would concatenate the
        // two revisions and make the newly committed head invisible to every
        // Oak-compatible reader. Segment bytes are already durable above, so
        // inserting a separator here preserves the write-order contract; a
        // crash after the separator merely turns the torn tail into a line
        // the tolerant reader skips.
        if journal_needs_separator(&self.directory.join("journal.log"))? {
            state.journal_file.write_all(b"\n")?;
        }
        if let Some(certificate) = &finalized_session_certificate {
            certificate.recertify()?;
        }
        state.journal_file.write_all(line.as_bytes())?;
        state.journal_file.sync_data()?;
        if let Some(certificate) = &finalized_session_certificate {
            certificate.recertify()?;
        }
        state.persisted_head = Some(head);
        Ok(())
    }

    /// Certifies every finalized session archive through freshly opened disk
    /// readers before a prepared head can reach the journal or
    /// post-compaction cleanup can mutate a base archive.
    ///
    /// A structurally valid index alone is insufficient: every session UUID
    /// must occur exactly once and in a session-created archive, every entry
    /// name/CRC/payload/generation must match the immutable in-memory write,
    /// no extra UUID may share a session archive, and graph/BRF trailers must
    /// equal a reconstruction through the complete fresh provider.
    pub(in crate::writer::store_writer) fn validate_finalized_session(
        &self,
        head: Option<RecordIdentifier>,
    ) -> Result<FinalizedSessionCertificate> {
        let expected_writes = self
            .session_segment_writes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let certificate = FinalizedSessionCertificate::capture(&self.directory, &expected_writes)?;
        self.validate_finalized_session_semantics(head)?;
        certificate.recertify()?;
        Ok(certificate)
    }

    /// The order this session recorded writing its segments, per archive,
    /// after proving the write-order ledger and the segment set agree.
    pub(super) fn expected_session_archive_order(
        &self,
        expected_segments: &HashMap<SegmentIdentifier, SessionSegment>,
    ) -> Result<HashMap<String, Vec<SegmentIdentifier>>> {
        let expected_writes = self
            .session_segment_writes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if expected_writes.len() != expected_segments.len() {
            return Err(Error::InvalidFormat {
                details: format!(
                    "session write-order ledger contains {} entries for {} distinct segments",
                    expected_writes.len(),
                    expected_segments.len()
                ),
            });
        }
        let mut expected_archive_order: HashMap<String, Vec<SegmentIdentifier>> = HashMap::new();
        let mut ordered_identifiers = std::collections::HashSet::new();
        for write in &expected_writes {
            if !expected_segments.contains_key(&write.identifier)
                || !ordered_identifiers.insert(write.identifier)
            {
                return Err(Error::InvalidFormat {
                    details: format!(
                        "session write-order ledger contains an absent or repeated segment {}",
                        write.identifier
                    ),
                });
            }
            expected_archive_order
                .entry(write.archive_file_name.to_string())
                .or_default()
                .push(write.identifier);
        }
        Ok(expected_archive_order)
    }

    pub(in crate::writer::store_writer) fn validate_finalized_session_semantics(
        &self,
        head: Option<RecordIdentifier>,
    ) -> Result<()> {
        #[cfg(test)]
        self.finalized_session_semantic_validations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let expected_segments: HashMap<SegmentIdentifier, SessionSegment> = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let expected_archive_order = self.expected_session_archive_order(&expected_segments)?;

        if let Some(head) = head
            && !expected_segments.contains_key(&head.segment)
        {
            return Err(Error::InvalidFormat {
                details: format!(
                    "cleanup head {head} is not part of the current write session; refusing to append it to the journal"
                ),
            });
        }

        // A fresh read-only repository gives validation the exact active
        // archive set plus lazy, bounded segment parsing. Building the older
        // eager archive provider parsed every segment in every base archive
        // under the cleanup lock, even though certification normally touches
        // only the new session and its reachable dependencies. Repository
        // opening binds the journal head but indexes all active segments, so
        // finalized session segments remain addressable before their new head
        // is appended to the journal.
        let provider = crate::store::Repository::open(&self.directory)?;
        let archives = provider.archives();
        reject_duplicate_active_segments(archives)?;
        let base_names: std::collections::HashSet<&str> = self
            .base_archives
            .iter()
            .map(TarArchiveReader::file_name)
            .collect();
        let (seen, seen_archives) = certify_session_archives(
            &provider,
            archives,
            &base_names,
            &expected_segments,
            &expected_archive_order,
        )?;
        if seen_archives.len() != expected_archive_order.len() {
            let mut missing_archives: Vec<_> = expected_archive_order
                .keys()
                .filter(|name| !seen_archives.contains(name.as_str()))
                .cloned()
                .collect();
            missing_archives.sort();
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archives omit expected archive(s): {missing_archives:?}"
                ),
            });
        }
        if seen.len() != expected_segments.len() {
            let mut missing: Vec<_> = expected_segments
                .keys()
                .filter(|identifier| !seen.contains(identifier))
                .copied()
                .collect();
            missing.sort_by_key(|identifier| {
                (
                    identifier.most_significant_bits,
                    identifier.least_significant_bits,
                )
            });
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archives omit {} session segment(s): {missing:?}",
                    missing.len()
                ),
            });
        }

        if let Some(head) = head {
            let disk_head = provider.segment(head.segment)?;
            if disk_head.structure.record_type(head.record_number) != Some(RecordType::Node) {
                return Err(Error::InvalidFormat {
                    details: format!("cleanup head {head} is not a finalized node record"),
                });
            }
            crate::tooling::verify_node_tree(&provider, head).map_err(|error| {
                Error::InvalidFormat {
                    details: format!(
                        "finalized cleanup head {head} failed its pre-journal health traversal: {error}"
                    ),
                }
            })?;
        }
        Ok(())
    }

    /// Finalizes one session TAR and, for prepared cleanup sessions, copies
    /// and verifies the active repository archive's uid/gid/mode before the
    /// new archive can become journal-visible.
    pub(in crate::writer::store_writer) fn close_archive_writer(
        &self,
        tar_writer: TarArchiveWriter,
    ) -> Result<()> {
        let path = tar_writer.path().to_owned();
        if !tar_writer.close()? {
            return Ok(());
        }
        if let Some(source_metadata) = &self.cleanup_archive_metadata {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)?;
            preserve_file_metadata(&file, source_metadata)?;
        }
        // Reopen what was just finished. A rotated archive's segments must
        // stay readable for the rest of the session — a later record can
        // reference one — and a mapping is how they stay readable without
        // the session holding their bytes. Reopening also drops the file
        // descriptor: `TarArchiveReader` keeps only the mapping.
        let reopened = TarArchiveReader::open(&path)?;
        self.session_archives
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(reopened);
        Ok(())
    }
}
