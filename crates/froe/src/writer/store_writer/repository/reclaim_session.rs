//! Reclaiming old generations from under a live session: certifying the
//! base archives, sweeping them, and retiring the sources only once the
//! replacements are proven.

use super::*;

/// Proves every segment in one finalized session archive against what the
/// session recorded for it, and rebuilds the graph and binary-reference
/// trailers those segments imply.
///
/// The payload is checked against the CRC in the segment's own tar entry
/// name and then against what the session actually wrote. Together those
/// two establish what comparing against a retained copy of every byte used
/// to, without the session holding its whole output to say it.
pub(crate) fn certify_archive_segments(
    provider: &crate::store::Repository,
    archive: &TarArchiveReader,
    expected_segments: &HashMap<SegmentIdentifier, SessionSegment>,
    seen: &mut std::collections::HashSet<SegmentIdentifier>,
) -> Result<(ExpectedGraph, ExpectedBinaryReferences)> {
    let mut expected_graph = ExpectedGraph::new();
    let mut expected_binary_references = ExpectedBinaryReferences::new();
    for identifier in archive.segment_identifiers() {
        let Some(expected_session) = expected_segments.get(&identifier) else {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} contains non-session segment {identifier}",
                    archive.file_name()
                ),
            });
        };
        if !seen.insert(identifier) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "session segment {identifier} occurs more than once in finalized session archives"
                ),
            });
        }
        // Proves the archive's payload against the CRC in its own
        // tar entry name.
        archive.validate_indexed_segment_entry(identifier)?;
        // And this closes the loop to what the session actually
        // wrote. Together the two are what comparing against a
        // retained copy of every byte used to establish, without the
        // session holding its whole output to say it.
        let actual_crc =
            archive
                .segment_entry_checksum(identifier)
                .ok_or(Error::SegmentNotFound {
                    segment_identifier: identifier,
                })?;
        if actual_crc != expected_session.payload_crc {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} changed the payload of segment {identifier}",
                    archive.file_name()
                ),
            });
        }
        let disk_segment = provider.segment(identifier)?;
        let actual = archive
            .index_entry(identifier)
            .ok_or(Error::SegmentNotFound {
                segment_identifier: identifier,
            })?;
        let expected_generation = stored_segment_generation(identifier, &disk_segment.structure);
        let actual_generation = GarbageCollectionGeneration {
            generation: actual.generation,
            full_generation: actual.full_generation,
            is_compacted: actual.is_compacted,
        };
        if actual_generation != expected_generation {
            return Err(Error::InvalidFormat {
                details: format!(
                    "finalized session archive {} indexes segment {identifier} as {actual_generation:?}, but its header/session generation is {expected_generation:?}",
                    archive.file_name()
                ),
            });
        }
        if !disk_segment.structure.referenced_segments.is_empty() {
            expected_graph
                .entry(identifier)
                .or_default()
                .extend(disk_segment.structure.referenced_segments.iter().copied());
        }
        let binary_references =
            read_blob_identifiers(provider, &disk_segment).map_err(|error| {
                Error::InvalidFormat {
                    details: format!(
                        "cannot reconstruct binary references for finalized session segment {identifier}: {error}"
                    ),
                }
            })?;
        if !binary_references.is_empty() {
            expected_binary_references
                .entry((
                    expected_generation.generation,
                    expected_generation.full_generation,
                    expected_generation.is_compacted,
                ))
                .or_default()
                .entry(identifier)
                .or_default()
                .extend(binary_references);
        }
    }
    Ok((expected_graph, expected_binary_references))
}

/// Oak's mark phase (§3 of the cleanup specification).
///
/// Archives are walked newest first and entries within each archive in
/// reverse file order, with one references set shared across all archives
/// so a kept data segment in a newer archive protects bulk segments in
/// older ones. The seed set is otherwise empty — sanctioned for an offline
/// tool on a quiescent store, which the exclusive repository lock
/// guarantees — and the dangling-future rule runs with a null compacted
/// root, i.e. disabled, which the specification calls always safe.
pub(crate) fn mark_reclaimable_segments(
    session_archives: &[TarArchiveReader],
    base_archives: &[TarArchiveReader],
    rule: ReclaimRule,
) -> Result<std::collections::HashSet<SegmentIdentifier>> {
    // Oak's mark phase (§3 of the cleanup specification): archives
    // newest first, entries within each archive in reverse file
    // order, one references set shared across all archives so a kept
    // data segment in a newer archive protects bulk segments in
    // older ones. The seed set is otherwise empty — sanctioned for an
    // offline tool on a quiescent store, which the exclusive
    // repository lock guarantees — and the dangling-future rule runs
    // with a null compacted root, i.e. disabled, which the
    // specification calls always safe.
    let mut references: std::collections::HashSet<SegmentIdentifier> =
        std::collections::HashSet::new();
    for archive in session_archives {
        seed_references_from_archive(archive, &mut references)?;
    }
    let protected_data_segments = std::collections::HashSet::new();
    let mut reclaimable = std::collections::HashSet::new();
    // Post-compaction cleanup has no dangling-future root: the caller
    // just committed the newly compacted head, so every compacted
    // segment written by that run belongs at or before that head.
    let mut ahead_of_root = None;
    for archive in base_archives {
        mark_one_archive(
            archive,
            ReclaimPolicy {
                rule,
                protected_data_segments: &protected_data_segments,
            },
            &mut references,
            &mut reclaimable,
            &mut ahead_of_root,
        )?;
    }

    Ok(reclaimable)
}

impl SegmentSweepOutcome {
    /// Folds one archive's sweep into the run's totals, returning the
    /// segments that sweep made unavailable.
    fn record_swept_archive(
        &mut self,
        outcome: ArchiveSweepOutcome,
    ) -> std::collections::HashSet<SegmentIdentifier> {
        match outcome.disposition {
            ArchiveSweepDisposition::Removed => self.removed_archives += 1,
            ArchiveSweepDisposition::Rewritten => self.rewritten_archives += 1,
            ArchiveSweepDisposition::Unchanged => {}
        }
        self.removed_segments += outcome.newly_unavailable.len();
        self.deletion_failures.extend(outcome.deletion_failures);
        outcome.newly_unavailable
    }
}

impl WritableRepository {
    /// The total size of the store's archive files on disk.
    pub fn archive_size_on_disk(&self) -> Result<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && ArchiveFileName::parse(name).is_some()
            {
                total += entry.metadata()?.len();
            }
        }
        Ok(total)
    }

    /// The file names of the archives that existed when this session opened.
    pub(in crate::writer::store_writer) fn base_archive_names(
        &self,
    ) -> std::collections::HashSet<String> {
        self.base_archives
            .iter()
            .map(|archive| archive.file_name().to_owned())
            .collect()
    }

    /// Proves that every archive which a subsequent compaction cleanup may
    /// mutate has complete, self-consistent payloads and trailers.
    ///
    /// Compaction calls this before writing its deep copy, so a pre-existing
    /// defect is refused before the run appends a full copy that a retry
    /// would then append again. Each source is certified once more through a
    /// fresh no-follow descriptor immediately before it is mutated, because
    /// an out-of-process pathname or byte change must still fail closed even
    /// while froe holds its advisory repository lock.
    ///
    /// The pass parses every data segment of every base archive, so
    /// compaction would otherwise begin with a long silence before its first
    /// reported step. That cost is also why the returned proof exists: see
    /// [`CertifiedReclaimSources`].
    pub(crate) fn preflight_reclaim_sources_with_progress(
        &self,
        observer: &mut dyn crate::progress::ProgressObserver,
    ) -> Result<CertifiedReclaimSources> {
        drop(self.open_base_repository_with_progress(BaseSourceCertification::Derive, observer)?);
        Ok(CertifiedReclaimSources {
            base_names: self.base_archive_names(),
        })
    }

    /// Opens a fresh, lazy provider over this session's base archives,
    /// deriving the full certificate for each one unless `certification`
    /// says the caller already holds it.
    ///
    /// The base-name check below runs either way. It is the cheap half — it
    /// proves the fresh open still sees every archive the session is about
    /// to reclaim from — and nothing may skip it.
    pub(in crate::writer::store_writer) fn open_base_repository_with_progress(
        &self,
        certification: BaseSourceCertification,
        observer: &mut dyn crate::progress::ProgressObserver,
    ) -> Result<crate::store::Repository> {
        let base_names = self.base_archive_names();
        let repository = crate::store::Repository::open_with_progress(&self.directory, observer)?;
        reject_duplicate_active_segments(repository.archives())?;
        let base_archives: Vec<&TarArchiveReader> = repository
            .archives()
            .iter()
            .filter(|archive| base_names.contains(archive.file_name()))
            .collect();
        // Before certifying, not after: an archive that has gone missing is
        // the cheaper refusal, and there is no reason to prove the ones that
        // remain first.
        let opened_base_names: std::collections::HashSet<String> = base_archives
            .iter()
            .map(|archive| archive.file_name().to_owned())
            .collect();
        if opened_base_names != base_names {
            let mut missing: Vec<_> = base_names.difference(&opened_base_names).cloned().collect();
            missing.sort();
            return Err(Error::InvalidFormat {
                details: format!(
                    "fresh reclamation source provider omitted active base archive(s) {missing:?}"
                ),
            });
        }
        if matches!(certification, BaseSourceCertification::Derive) {
            certify_archives_in_parallel(&repository, &base_archives, observer)?;
        }
        Ok(repository)
    }

    /// Reclaims segments older than `reference_generation` after a
    /// compaction: Oak's mark phase decides what goes, then each base
    /// archive is swept. Data segments are retained purely by the
    /// generation predicate with a single retained generation, selected
    /// by `kind`; bulk segments are
    /// retained purely by reachability from kept data segments, through
    /// a references set shared across all archives. A base archive whose
    /// segments all reclaim is deleted; one with survivors is rewritten
    /// to the next generation letter with only the survivors.
    ///
    /// This is safe only when every record reachable from the current
    /// head lives in `reference_generation` — which compaction's deep
    /// copy guarantees.
    ///
    /// Scope: only the archives that existed when this session opened are
    /// swept. Archives written during this session participate in the
    /// *mark* — their retained data segments protect the bulk segments
    /// they reference, wherever those live — but are never swept
    /// themselves; the next compaction run sees them as base archives.
    pub fn reclaim_old_generations(
        &mut self,
        reference_generation: GarbageCollectionGeneration,
        kind: CompactionKind,
    ) -> Result<()> {
        self.reclaim_old_generations_with(GenerationReclaimRequest {
            rule: ReclaimRule {
                reference: reference_generation,
                kind,
                retained_generations: RETAINED_GENERATIONS,
            },
            rewrite_policy: ArchiveRewritePolicy::EveryReclaimableArchive,
            certified_sources: None,
            expected: None,
        })
        .map(|_| ())
    }

    /// Refuses a store in which a segment this session wrote also occurs in
    /// an active base archive.
    ///
    /// The mark result is one store-wide UUID set, so an old-generation
    /// occurrence could put that UUID in the set even though a newer
    /// occurrence must stay, and sweep or trailer filtering would then
    /// remove the authoritative copy. Refusing here — before the current
    /// writer is closed or any base reader is taken — keeps the preflight
    /// fail-closed and non-mutating.
    ///
    /// The location map is scoped, not held: it is a store-wide identifier
    /// map built for a preflight that ends in milliseconds, and leaving it
    /// bound for the rest of the reclaim pinned hundreds of megabytes
    /// across the expensive phase for no reader.
    pub(super) fn reject_session_segments_already_in_base_archives(&self) -> Result<()> {
        let base_locations = unique_active_segment_locations(&self.base_archives)?;
        let session_segments = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((identifier, previous)) = session_segments.keys().find_map(|identifier| {
            base_locations
                .get(identifier)
                .map(|name| (*identifier, *name))
        }) {
            return Err(Error::InvalidFormat {
                details: format!(
                    "segment {identifier} occurs in active base archive {previous} and the current write session; refusing global reclamation"
                ),
            });
        }
        Ok(())
    }

    /// Opens the archives this session wrote: everything active that is not
    /// a base archive.
    ///
    /// Sorted newest number first, because the mark phase walks archives in
    /// that order. Only names matching the Oak archive pattern participate;
    /// unrelated files are ignored exactly as the write open ignores them.
    pub(super) fn open_session_archives(
        &self,
        base_names: &std::collections::HashSet<String>,
    ) -> Result<Vec<TarArchiveReader>> {
        let mut session_archives = Vec::new();
        for file_name in crate::store::list_archive_file_names(&self.directory)? {
            if ArchiveFileName::parse(&file_name).is_none() || base_names.contains(&file_name) {
                continue;
            }
            let path = self.directory.join(&file_name);
            // A zero-length archive is not something this session wrote: it
            // is the residue of a writer killed inside its own lazy
            // next-archive creation, which the write open deliberately
            // serves no archive for. Opening it would fail outright, so the
            // skip has to hold here too or compaction inherits the failure
            // that opening was fixed to avoid.
            if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == 0) {
                continue;
            }
            session_archives.push(TarArchiveReader::open(&path)?);
        }
        session_archives.sort_by_key(|archive| {
            std::cmp::Reverse(
                ArchiveFileName::parse(archive.file_name())
                    .map_or(0, |parsed| parsed.archive_number),
            )
        });

        Ok(session_archives)
    }

    /// Plans the sweep of every base archive against what the mark phase
    /// found reclaimable.
    ///
    /// Compaction has already paid for a full deep copy of the live tree.
    /// Declining to move the survivors of an archive whose data segments all
    /// died would hand the operator a store the very next maintenance run
    /// reports as dirty and equally cannot clean, which is the field report
    /// this default policy fixes.
    pub(super) fn plan_base_archive_sweeps(
        &self,
        reclaimable: &std::collections::HashSet<SegmentIdentifier>,
        rewrite_policy: ArchiveRewritePolicy,
    ) -> Result<HashMap<String, PlannedArchiveSweep>> {
        let mut planned_base_sweeps = HashMap::new();
        for archive in &self.base_archives {
            // Compaction has already paid for a full deep copy of the live
            // tree. Declining to move the survivors of an archive whose data
            // segments all died would hand the operator a store the very next
            // maintenance run reports as dirty and equally cannot clean, which
            // is the field report this default policy fixes.
            if let Some(planned) = plan_archive_sweep(
                &self.directory,
                archive,
                reclaimable,
                rewrite_policy,
                &std::collections::HashSet::new(),
            )? {
                planned_base_sweeps.insert(archive.file_name().to_owned(), planned);
            }
        }
        Ok(planned_base_sweeps)
    }

    /// Closes this session's archive, makes it durable, and certifies what
    /// it wrote before any base archive may be removed.
    ///
    /// The compacted head is already journal-visible at this point, so its
    /// finalized TAR link and trailers must be durable and independently
    /// traversable before deleting any base archive it may replace.
    pub(super) fn finalize_and_certify_session(&mut self) -> Result<FinalizedSessionCertificate> {
        // Finalize the session archive so its new-generation segments are
        // complete on disk before old archives are removed.
        {
            let mut state = self.lock_write_state();
            if let Some(tar_writer) = state.tar_writer.take() {
                drop(state);
                self.close_archive_writer(tar_writer)?;
            }
        }
        // The compacted head is already journal-visible at this point. Its
        // finalized TAR link and trailers must be durable and independently
        // traversable before deleting any base archive it may replace.
        sync_directory_strict(&self.directory)?;
        let head = self.head();
        let head_is_in_session = self
            .session_segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&head.segment);
        let finalized_session_certificate =
            self.validate_finalized_session(head_is_in_session.then_some(head))?;

        Ok(finalized_session_certificate)
    }

    /// Releases the base archives and their parsed segments.
    ///
    /// Called only after every immediate source certificate and sweep has
    /// completed: keeping `self` intact until here lets the mark and sweep
    /// phases retain their original immutable source views.
    #[cfg_attr(not(test), allow(clippy::unnecessary_wraps))]
    pub(super) fn retire_base_archives(
        &mut self,
        #[cfg(test)] parsed_cache_entries_before_reclaim: usize,
    ) -> Result<()> {
        #[cfg(test)]
        if self
            .parsed_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
            > parsed_cache_entries_before_reclaim
        {
            return Err(Error::InvalidFormat {
                details: "post-compaction certification and sweeping grew the writable base-segment cache"
                    .to_owned(),
            });
        }
        let base_archives = std::mem::take(&mut self.base_archives);
        self.parsed_segment_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        drop(base_archives);
        Ok(())
    }

    /// Reclaims exactly like [`Self::reclaim_old_generations`], accepting a
    /// proof that the caller already certified these sources under the
    /// currently held lock.
    pub(crate) fn reclaim_old_generations_with(
        &mut self,
        request: GenerationReclaimRequest<'_>,
    ) -> Result<SegmentSweepOutcome> {
        let GenerationReclaimRequest {
            rule,
            rewrite_policy,
            certified_sources,
            expected,
        } = request;
        let mut sweep_outcome = SegmentSweepOutcome::default();
        #[cfg(test)]
        let parsed_cache_entries_before_reclaim = self
            .parsed_segment_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();

        // The mark result is one store-wide UUID set. If two active base
        // archives contain the same UUID, an old-generation occurrence can
        // put that UUID in the set even though a newer occurrence must stay,
        // causing sweep/trailer filtering to remove the authoritative copy.
        // Refuse before closing the current writer or taking base readers so
        // the caller observes a true fail-closed, non-mutating preflight.
        // Scoped, not held: this is a store-wide identifier map built for a
        // preflight that ends in milliseconds, and leaving it bound for the
        // rest of the reclaim pinned hundreds of megabytes across the
        // expensive phase for no reader.
        self.reject_session_segments_already_in_base_archives()?;

        let finalized_session_certificate = self.finalize_and_certify_session()?;

        // Use one fresh read-only repository for every base-source
        // certificate in this reclaim pass. Its parsed-segment cache is
        // bounded, unlike the writable store's session cache: certifying all
        // base archives through `self` would otherwise pin the parsed record
        // table of every live and garbage segment until sweeping completed.
        // Keeping this provider alive also gives each immediate reopened-source
        // certificate a complete, stable cross-archive fallback without
        // repopulating `self.parsed_segment_cache`.
        //
        // Deriving the certificate here is what a caller's proof can excuse,
        // and only that: the provider is still opened fresh, still rejects
        // duplicate active segments, and still proves it sees every base
        // archive. What the proof stands in for is re-reading bytes this same
        // locked run already read, which for compaction is a second full
        // parse and CRC of the whole store between its preflight and its
        // sweeps. The certificate that guards each mutation is neither of
        // these: it is the per-archive one `sweep_one_archive` derives
        // through a fresh no-follow descriptor, immediately before acting.
        let base_names = self.base_archive_names();
        let certification =
            if certified_sources.is_some_and(|proof| proof.certifies_exactly(&base_names)) {
                BaseSourceCertification::AlreadyProven
            } else {
                BaseSourceCertification::Derive
            };
        let certification_repository = self.open_base_repository_with_progress(
            certification,
            &mut crate::progress::DiscardedProgress,
        )?;

        // Archives this session wrote (now closed and complete on disk):
        // newer than every base archive. They are never swept, so every
        // data segment they hold stays on disk regardless of generation —
        // and each one therefore seeds the references set with the bulk
        // segments it points at, including pre-existing bulk segments in
        // base archives, which the empty seed alone would miss. Only
        // names matching the Oak archive pattern participate; unrelated
        // `*.tar` files in the directory are ignored, exactly as the
        // write open ignores them.
        let session_archives = self.open_session_archives(&base_names)?;

        let reclaimable = mark_reclaimable_segments(&session_archives, &self.base_archives, rule)?;

        // Store-wide fallback provider for catalog reconstruction, built
        // only if some swept archive turns out to have no readable
        // catalog. Newest first — session archives before base archives —
        // so a duplicated segment resolves to the copy live lookups
        // serve.
        let provider_order: Vec<&TarArchiveReader> = session_archives
            .iter()
            .chain(self.base_archives.iter())
            .collect();
        let mut fallback_provider: Option<ArchiveSegmentsProvider<'_>> = None;
        let planned_base_sweeps = self.plan_base_archive_sweeps(&reclaimable, rewrite_policy)?;
        // Nothing has been unlinked yet. This is the last instant at which a
        // disagreement between what the operator confirmed and what the store
        // now says can be answered by refusing rather than by explaining, so
        // it is where the comparison belongs — the same position the
        // directory-level engine puts it in.
        if let Some(expected) = expected {
            let replanned = sorted_sweep_plan(&planned_base_sweeps, &reclaimable);
            if replanned != *expected {
                return Err(Error::InvalidFormat {
                    details: "the archive sweep changed after confirmation; refusing to apply an \
                              unconfirmed archive mutation"
                        .to_owned(),
                });
            }
        }
        let mut actually_unavailable = std::collections::HashSet::new();
        finalized_session_certificate.recertify()?;
        // Whole removals run before rewrites. Only a removal that actually
        // unlinked its source contributes graph-filter targets; a failed
        // unlink leaves the edge conservatively intact. Each rewrite adds its
        // own removed entries while it is built, then makes them unavailable
        // through the published higher generation before the next rewrite.
        for rewrite_phase in [false, true] {
            for archive in &self.base_archives {
                let Some(planned) = planned_base_sweeps.get(archive.file_name()) else {
                    continue;
                };
                let is_rewrite = matches!(planned, PlannedArchiveSweep::Rewrite { .. });
                let is_remove = matches!(planned, PlannedArchiveSweep::Remove { .. });
                if (!rewrite_phase && !is_remove) || (rewrite_phase && !is_rewrite) {
                    continue;
                }
                finalized_session_certificate.recertify()?;
                let outcome = sweep_one_archive(
                    &self.directory,
                    archive,
                    &reclaimable,
                    &actually_unavailable,
                    &provider_order,
                    &mut fallback_provider,
                    Some(&certification_repository),
                    rewrite_policy,
                )?;
                finalized_session_certificate.recertify()?;
                actually_unavailable.extend(sweep_outcome.record_swept_archive(outcome));
            }
            #[cfg(test)]
            if !rewrite_phase
                && planned_base_sweeps
                    .values()
                    .any(|planned| matches!(planned, PlannedArchiveSweep::Rewrite { .. }))
            {
                probe_archive_sweep_phase_boundary(
                    "postcomp-sweep.removals-complete-before-rewrites",
                )?;
            }
        }
        drop(fallback_provider);
        drop(provider_order);
        drop(session_archives);
        drop(certification_repository);
        self.retire_base_archives(
            #[cfg(test)]
            parsed_cache_entries_before_reclaim,
        )?;
        finalized_session_certificate.recertify()?;
        // Make the archive deletions and any swept replacements durable
        // before the caller proceeds to the journal rewrite.
        sync_directory_strict(&self.directory)?;
        Ok(sweep_outcome)
    }
}
