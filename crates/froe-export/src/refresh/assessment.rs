//! Deciding whether an existing export is a usable base: what its two
//! files are, whether their stamps agree with each other and with the
//! requested scope, and whether a full export may replace them unasked.

use super::{
    ExportProvenance, NODES_FILE_NAME, PROPERTIES_FILE_NAME, Path, Repository, provenance_of,
};

/// Why an existing export is not a refresh base, classified for the
/// caller's replace decision.
pub(crate) struct Rejection {
    /// The operator-facing reason.
    pub(crate) reason: String,
    /// Whether a full export may replace the files uninvited: they are
    /// absent, or verifiably froe's own export of the requested scope
    /// (possibly interrupted). Foreign files and other scopes require
    /// the explicit rebuild flag.
    pub(crate) replaceable: bool,
}

/// One table file's ownership state.
pub(crate) enum TableFile {
    /// No directory entry at the path at all.
    Missing,
    /// A readable, stamped froe export file. The reader is retained
    /// from inspection on, so the bytes a refresh validates are the
    /// bytes it merges — a pathname swap can never substitute them.
    Stamped {
        /// The file's stamped provenance.
        provenance: ExportProvenance,
        /// The open reader the merge consumes.
        reader: ::parquet::file::reader::SerializedFileReader<std::fs::File>,
    },
    /// Present but not a demonstrably froe-owned regular Parquet file:
    /// unreadable, unstamped, a symlink (dangling symlinks fail
    /// `File::open` with `NotFound` and would otherwise masquerade as
    /// [`TableFile::Missing`]), or another non-regular entry such as a
    /// FIFO — which must never reach a blocking `File::open`.
    Foreign(String),
}

/// Inspects one table file, retaining the reader of a stamped file.
pub(crate) fn inspect_table(path: &Path) -> TableFile {
    use ::parquet::file::reader::SerializedFileReader;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return TableFile::Missing;
        }
        Err(error) => {
            return TableFile::Foreign(format!("{} cannot be inspected: {error}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        return TableFile::Foreign(format!("{} is not a regular file", path.display()));
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return TableFile::Foreign(format!("{} cannot be read: {error}", path.display()));
        }
    };
    match SerializedFileReader::new(file) {
        Ok(reader) => match provenance_of(&reader) {
            Some(provenance) => TableFile::Stamped { provenance, reader },
            None => TableFile::Foreign(format!(
                "{} is a Parquet file, but not a froe export — it carries no export stamp",
                path.display()
            )),
        },
        Err(error) => TableFile::Foreign(format!(
            "{} is not readable as a Parquet export: {error}",
            path.display()
        )),
    }
}

/// The classification of an export directory against a requested
/// export, deciding both refreshability and replacement authorization.
pub(crate) enum Classification {
    /// Both files are present, stamped, in scope, and agreeing; the
    /// open readers ride along to the merge.
    Reusable {
        /// The agreed provenance.
        provenance: ExportProvenance,
        /// The nodes and properties readers, open since inspection.
        base: [::parquet::file::reader::SerializedFileReader<std::fs::File>; 2],
    },
    /// Nothing present, or only froe's own in-scope output (possibly
    /// interrupted mid-refresh): a full export may replace it uninvited.
    Replaceable(String),
    /// Foreign, other-repository, or out-of-scope files are present:
    /// replacing them takes the explicit rebuild flag.
    Guarded(String),
}

/// The scope-mismatch reason when `provenance` does not match the
/// requested root path and depth limit.
pub(crate) fn scope_mismatch(
    provenance: &ExportProvenance,
    root_path: &str,
    depth: Option<usize>,
) -> Option<String> {
    let requested = ExportProvenance::new(String::new(), root_path, depth);
    if provenance.root_path() != requested.root_path() {
        return Some(format!(
            "the existing export covers {}, not {}",
            provenance.root_path(),
            requested.root_path()
        ));
    }
    if provenance.depth_limit() != requested.depth_limit() {
        let describe = |limit: Option<usize>| {
            limit.map_or_else(|| "unlimited".to_owned(), |limit| format!("depth {limit}"))
        };
        return Some(format!(
            "the existing export was {}, this request is {}",
            describe(provenance.depth_limit()),
            describe(depth)
        ));
    }
    None
}

/// The ownership check a `TarMK` store supports. The store has no
/// repository UUID, and compaction rewrites the journal to one line, so
/// history cannot prove identity: a stamped revision is this
/// repository's own exactly when its segment still resolves. A foreign
/// repository's segments never collide (random UUIDs); a compacted-away
/// revision conservatively fails the check, so replacing such an export
/// takes the explicit rebuild flag.
pub(crate) fn resolves_here(repository: &Repository, provenance: &ExportProvenance) -> bool {
    froe::journal::parse_record_identifier_text(provenance.revision())
        .is_some_and(|identifier| repository.contains_segment(identifier.segment))
}

/// The unresolvable-stamp rejection reason.
pub(crate) fn unresolvable_reason(provenance: &ExportProvenance) -> String {
    format!(
        "the stamped revision {} does not resolve against this repository; the store was \
         likely compacted since the export, or the export belongs to a different repository",
        provenance.revision()
    )
}

/// Classifies the export directory. The authorization rule, in one
/// place: automatic replacement is safe only when both files are
/// absent, or when every present file is demonstrably froe-owned —
/// stamped, in scope, and resolving against this repository — with a
/// foreign, other-repository, or out-of-scope file anywhere guarding
/// the directory. Two stamps may disagree only in their revision for
/// the pair to count as interrupted-refresh residue; any other
/// disagreement is out of scope by construction and never reaches the
/// residue branch.
pub(crate) fn classify(
    repository: &Repository,
    output_directory: &Path,
    root_path: &str,
    depth: Option<usize>,
) -> Classification {
    let nodes_path = output_directory.join(NODES_FILE_NAME);
    let properties_path = output_directory.join(PROPERTIES_FILE_NAME);
    let nodes = inspect_table(&nodes_path);
    let properties = inspect_table(&properties_path);
    for file in [&nodes, &properties] {
        if let TableFile::Foreign(reason) = file {
            return Classification::Guarded(reason.clone());
        }
    }
    match (nodes, properties) {
        (TableFile::Missing, TableFile::Missing) => Classification::Replaceable(format!(
            "there is no export at {} yet",
            output_directory.display()
        )),
        (TableFile::Stamped { provenance, .. }, TableFile::Missing)
        | (TableFile::Missing, TableFile::Stamped { provenance, .. }) => {
            if let Some(reason) = scope_mismatch(&provenance, root_path, depth) {
                return Classification::Guarded(reason);
            }
            if !resolves_here(repository, &provenance) {
                return Classification::Guarded(unresolvable_reason(&provenance));
            }
            Classification::Replaceable("one of the export's two files is missing".to_owned())
        }
        (
            TableFile::Stamped {
                provenance: first,
                reader: first_reader,
            },
            TableFile::Stamped {
                provenance: second,
                reader: second_reader,
            },
        ) => {
            for provenance in [&first, &second] {
                if let Some(reason) = scope_mismatch(provenance, root_path, depth) {
                    return Classification::Guarded(reason);
                }
            }
            if !resolves_here(repository, &first) {
                return Classification::Guarded(unresolvable_reason(&first));
            }
            if !resolves_here(repository, &second) {
                return Classification::Guarded(unresolvable_reason(&second));
            }
            if first == second {
                Classification::Reusable {
                    provenance: first,
                    base: [first_reader, second_reader],
                }
            } else {
                Classification::Replaceable(
                    "the export's two files carry different revisions; an earlier refresh \
                     must have been interrupted"
                        .to_owned(),
                )
            }
        }
        (TableFile::Foreign(_), _) | (_, TableFile::Foreign(_)) => {
            unreachable!("foreign files returned above")
        }
    }
}

/// The assessment of an export directory's contents before a full
/// export replaces them; see [`assess_export`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportAssessment {
    /// A valid, own, in-scope export stands there — refresh it rather
    /// than replace it.
    Reusable,
    /// Nothing stands there, or only froe's own in-scope residue —
    /// replacement is safe; the string says why it is not Reusable.
    Replaceable(String),
    /// Foreign, other-repository, or out-of-scope files stand there —
    /// replacement needs the explicit rebuild flag.
    Guarded(String),
}

/// Assesses the export directory's contents ahead of a full export.
/// Callers replacing files should hold the export directory lock
/// ([`crate::lock_export_directory`]) across the assessment and the
/// replacement, so the verdict cannot go stale.
#[must_use]
pub fn assess_export(
    repository: &Repository,
    output_directory: &Path,
    root_path: &str,
    depth: Option<usize>,
) -> ExportAssessment {
    match classify(repository, output_directory, root_path, depth) {
        Classification::Reusable { .. } => ExportAssessment::Reusable,
        Classification::Replaceable(reason) => ExportAssessment::Replaceable(reason),
        Classification::Guarded(reason) => ExportAssessment::Guarded(reason),
    }
}

/// Validates an existing export as a refresh base. Every failure is a
/// [`Rejection`], not an error — nothing about a reusable-or-not
/// verdict is exceptional.
pub(crate) fn validate(
    repository: &Repository,
    output_directory: &Path,
    root_path: &str,
    depth: Option<usize>,
) -> Result<ValidatedBase, Rejection> {
    match classify(repository, output_directory, root_path, depth) {
        Classification::Reusable { provenance, base } => Ok(ValidatedBase { provenance, base }),
        Classification::Replaceable(reason) => Err(Rejection {
            reason,
            replaceable: true,
        }),
        Classification::Guarded(reason) => Err(Rejection {
            reason,
            replaceable: false,
        }),
    }
}

/// A refresh base that passed validation: the agreed provenance and the
/// two table readers, held open from inspection through the merge.
pub(crate) struct ValidatedBase {
    /// The agreed provenance of the two files.
    pub(crate) provenance: ExportProvenance,
    /// The nodes and properties readers the merge consumes.
    pub(crate) base: [::parquet::file::reader::SerializedFileReader<std::fs::File>; 2],
}

#[cfg(test)]
mod tests {
    use crate::refresh::ParquetRefresh;
    use crate::refresh::test_support::*;
    use froe::store::Repository;

    #[test]
    fn a_missing_export_is_not_reusable() {
        let directory = TestDirectory::new("missing");
        populate_first(&directory.store());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("no files, no refresh: {outcome:?}");
        };
        assert!(reason.contains("no export"), "the reason: {reason}");
        assert!(replaceable, "there is nothing to destroy");
    }

    #[test]
    fn a_stampless_export_is_not_reusable() {
        let directory = TestDirectory::new("stampless");
        populate_first(&directory.store());
        full_export_without_stamp(&directory.store(), "/content", &directory.export());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("an unstamped file is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("no export stamp"), "the reason: {reason}");
        assert!(
            !replaceable,
            "foreign files wait for the explicit rebuild flag"
        );
    }

    #[test]
    fn disagreeing_stamps_are_not_reusable() {
        let directory = TestDirectory::new("disagreeing");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        // The residue an interrupted refresh leaves: one file stamped
        // with the next real revision, one with the previous — both
        // this repository's own.
        populate_second(&directory.store());
        let other = directory.path.join("other");
        full_export(&directory.store(), "/content", None, &other, None);
        std::fs::copy(
            other.join("nodes.parquet"),
            directory.export().join("nodes.parquet"),
        )
        .expect("copy");

        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("disagreeing stamps are no refresh base: {outcome:?}");
        };
        assert!(
            reason.contains("different revisions"),
            "the reason: {reason}"
        );
        assert!(replaceable, "interrupted-refresh residue is froe's own");
    }

    #[test]
    fn an_unresolvable_stamped_revision_is_guarded() {
        let directory = TestDirectory::new("stale-revision");
        populate_first(&directory.store());
        // A well-formed revision naming a segment the store never held
        // stands in for both a compacted-away revision and a foreign
        // repository's: unresolvable is unresolvable, and replacing the
        // files takes the explicit flag.
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            Some("00000000-0000-0000-0000-000000000000.00000001".to_owned()),
        );
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("an unresolvable stamp is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("does not resolve"), "the reason: {reason}");
        assert!(
            !replaceable,
            "compaction cannot be told from a foreign repository; --full decides"
        );
    }

    #[test]
    fn a_different_root_path_is_not_reusable() {
        let directory = TestDirectory::new("other-root");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        let outcome = refresh(&directory.store(), "/", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("a different subtree is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("covers /content"), "the reason: {reason}");
        assert!(
            !replaceable,
            "a different scope waits for the explicit rebuild flag"
        );
    }

    #[test]
    fn a_different_depth_limit_is_not_reusable() {
        let directory = TestDirectory::new("other-depth");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        let outcome = refresh(&directory.store(), "/content", Some(2), &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("a different depth limit is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("unlimited"), "the reason: {reason}");
        assert!(
            !replaceable,
            "a different scope waits for the explicit rebuild flag"
        );
    }

    #[test]
    fn a_row_corrupt_base_is_not_reusable() {
        let directory = TestDirectory::new("corrupt-base");
        populate_first(&directory.store());
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        // Corrupt the first data page of the nodes table: the footer —
        // and with it the stamp — stays readable, but the rows do not
        // decode.
        {
            use ::parquet::file::reader::{FileReader, SerializedFileReader};
            let nodes_path = directory.export().join("nodes.parquet");
            let offset = {
                let reader =
                    SerializedFileReader::new(std::fs::File::open(&nodes_path).expect("open"))
                        .expect("reader");
                reader.metadata().row_group(0).column(0).file_offset() as usize
            };
            let mut bytes = std::fs::read(&nodes_path).expect("read");
            for byte in &mut bytes[offset..offset + 16] {
                *byte ^= 0xFF;
            }
            std::fs::write(&nodes_path, bytes).expect("write");
        }

        populate_second(&directory.store());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("a base whose rows do not decode rebuilds: {outcome:?}");
        };
        assert!(reason.contains("not readable"), "the reason: {reason}");
        assert!(replaceable, "a corrupt froe-owned base rebuilds uninvited");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_are_foreign_never_missing() {
        let directory = TestDirectory::new("dangling-symlinks");
        populate_first(&directory.store());
        std::fs::create_dir_all(directory.export()).expect("create export directory");
        for name in ["nodes.parquet", "properties.parquet"] {
            std::os::unix::fs::symlink(
                directory.path.join("no-such-target"),
                directory.export().join(name),
            )
            .expect("symlink");
        }
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("a dangling symlink is not an absent file: {outcome:?}");
        };
        assert!(
            reason.contains("not a regular file"),
            "the reason: {reason}"
        );
        assert!(
            !replaceable,
            "foreign filesystem objects are never replaced uninvited"
        );
        let repository = Repository::open(&directory.store()).expect("open");
        assert!(
            matches!(
                super::assess_export(&repository, &directory.export(), "/content", None),
                super::ExportAssessment::Guarded(_)
            ),
            "the assessment agrees"
        );
    }

    #[test]
    fn an_export_from_another_repository_is_guarded() {
        let directory = TestDirectory::new("cross-repository");
        // A complete, valid, in-scope export — of a *different* store.
        let foreign = TestDirectory::new("cross-repository-foreign");
        populate_first(&foreign.store());
        full_export(
            &foreign.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );

        populate_first(&directory.store());
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("another repository's export is no refresh base: {outcome:?}");
        };
        assert!(reason.contains("does not resolve"), "the reason: {reason}");
        assert!(
            !replaceable,
            "a foreign repository's export is indistinguishable from a compacted base"
        );
    }

    #[test]
    fn a_missing_table_with_a_foreign_peer_is_not_replaceable() {
        let directory = TestDirectory::new("foreign-peer");
        populate_first(&directory.store());
        // No nodes.parquet; a foreign properties.parquet.
        std::fs::create_dir_all(directory.export()).expect("create export directory");
        std::fs::write(directory.export().join("properties.parquet"), b"foreign").expect("seed");
        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason: _,
            replaceable,
        } = outcome
        else {
            panic!("a foreign peer must guard the directory: {outcome:?}");
        };
        assert!(
            !replaceable,
            "the missing table must not authorize replacing the surviving one"
        );
    }

    #[test]
    fn a_missing_table_with_an_out_of_scope_peer_is_not_replaceable() {
        let directory = TestDirectory::new("out-of-scope-peer");
        populate_first(&directory.store());
        // A stamped /content export with its properties table removed,
        // queried as a / export.
        full_export(
            &directory.store(),
            "/content",
            None,
            &directory.export(),
            None,
        );
        std::fs::remove_file(directory.export().join("properties.parquet")).expect("remove");
        let outcome = refresh(&directory.store(), "/", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("an out-of-scope peer must guard the directory: {outcome:?}");
        };
        assert!(reason.contains("covers /content"), "the reason: {reason}");
        assert!(
            !replaceable,
            "the missing peer must not launder the surviving file's scope"
        );
    }

    #[test]
    fn mixed_scope_stamps_are_not_replaceable() {
        let directory = TestDirectory::new("mixed-scope");
        populate_first(&directory.store());
        // nodes.parquet from a / export, properties.parquet from a
        // /content export of the same revision: disagreement, but not
        // interrupted-refresh residue.
        full_export(&directory.store(), "/", None, &directory.export(), None);
        let other = directory.path.join("other");
        full_export(&directory.store(), "/content", None, &other, None);
        std::fs::copy(
            other.join("properties.parquet"),
            directory.export().join("properties.parquet"),
        )
        .expect("copy");

        let outcome = refresh(&directory.store(), "/content", None, &directory.export());
        let ParquetRefresh::NotReusable {
            reason,
            replaceable,
        } = outcome
        else {
            panic!("mixed scopes are not residue: {outcome:?}");
        };
        assert!(reason.contains("covers /"), "the reason: {reason}");
        assert!(
            !replaceable,
            "only the revision may differ for the residue classification"
        );
    }
}
