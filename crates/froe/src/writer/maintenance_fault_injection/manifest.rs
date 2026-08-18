//! Every boundary of the manifest replacement: whichever way a run dies,
//! the file left behind is exactly the old manifest or exactly the new.

#[cfg(test)]
mod tests {
    use crate::writer::commit::create_checkpoint;
    use crate::writer::maintenance_fault_injection::test_support::{
        MANIFEST_SCENARIO, RepositorySnapshot, TestDirectory, assert_exact_snapshot_reopens,
        run_crash_child, run_error_child, snapshot_repository,
    };
    use crate::writer::store_writer::WritableRepository;
    use std::path::Path;

    fn create_manifest_upgrade_fixture(
        directory: &TestDirectory,
    ) -> (RepositorySnapshot, Vec<u8>, Vec<u8>) {
        {
            let store = WritableRepository::open(&directory.path).expect("open manifest fixture");
            create_checkpoint(&store, 60_000, &[])
                .expect("create deterministically unreferenced checkpoint");
            store.close().expect("close manifest fixture writer");
        }
        let old_manifest = b"custom.property=kept\nstore.version=1\n".to_vec();
        std::fs::write(directory.path.join("manifest"), &old_manifest)
            .expect("install version-one manifest fixture");
        let mut upgraded_manifest = old_manifest.clone();
        upgraded_manifest.push(b'\n');
        upgraded_manifest
            .extend_from_slice(b"# upgraded atomically by froe cleanup\nstore.version=2\n");
        (
            snapshot_repository(&directory.path),
            old_manifest,
            upgraded_manifest,
        )
    }

    /// What a fault at a manifest-replacement cutpoint must leave on disk.
    #[derive(Clone, Copy)]
    struct ExpectedManifestResidue {
        /// The upgraded manifest is in place under its final name.
        replacement_installed: bool,
        /// The staging temporary is still present.
        temporary_exists: bool,
    }

    fn assert_manifest_residue(
        directory: &Path,
        snapshot: &RepositorySnapshot,
        old_manifest: &[u8],
        upgraded_manifest: &[u8],
        residue: ExpectedManifestResidue,
        cutpoint: &str,
    ) {
        let ExpectedManifestResidue {
            replacement_installed,
            temporary_exists,
        } = residue;
        assert_exact_snapshot_reopens(directory, snapshot);
        assert_eq!(
            std::fs::read(directory.join("manifest")).expect("read canonical manifest"),
            if replacement_installed {
                upgraded_manifest
            } else {
                old_manifest
            },
            "canonical manifest must be one complete valid generation at {cutpoint}"
        );
        let temporary = directory.join("manifest.cleaning.000");
        assert_eq!(temporary.exists(), temporary_exists, "{cutpoint}");
        if temporary_exists {
            assert_eq!(
                std::fs::read(temporary).expect("read staged manifest residue"),
                upgraded_manifest,
                "manifest staging residue must contain the exact valid upgrade"
            );
        }
    }

    #[test]
    fn manifest_replacement_crash_boundaries_keep_an_exact_old_or_new_manifest() {
        let cutpoints = [
            ("manifest.temporary-durable", false, true),
            ("manifest.before-rename", false, true),
            ("manifest.renamed-before-directory-sync", true, false),
            ("manifest.before-post-rename-directory-sync", true, false),
            ("manifest.rename-durable", true, false),
        ];

        for (cutpoint, replacement_installed, temporary_exists) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, old_manifest, upgraded_manifest) =
                create_manifest_upgrade_fixture(&directory);

            run_crash_child(&directory.path, MANIFEST_SCENARIO, cutpoint);

            assert_manifest_residue(
                &directory.path,
                &snapshot,
                &old_manifest,
                &upgraded_manifest,
                ExpectedManifestResidue {
                    replacement_installed,
                    temporary_exists,
                },
                cutpoint,
            );
        }
    }

    #[test]
    fn manifest_replacement_errors_keep_an_exact_old_or_new_manifest() {
        let cutpoints = [
            ("manifest.temporary-durable", false),
            ("manifest.before-rename", false),
            ("manifest.renamed-before-directory-sync", true),
            ("manifest.before-post-rename-directory-sync", true),
            ("manifest.rename-durable", true),
        ];

        for (cutpoint, replacement_installed) in cutpoints {
            let directory = TestDirectory::repository(cutpoint);
            let (snapshot, old_manifest, upgraded_manifest) =
                create_manifest_upgrade_fixture(&directory);

            run_error_child(&directory.path, MANIFEST_SCENARIO, cutpoint);

            assert_manifest_residue(
                &directory.path,
                &snapshot,
                &old_manifest,
                &upgraded_manifest,
                ExpectedManifestResidue {
                    replacement_installed,
                    temporary_exists: false,
                },
                cutpoint,
            );
        }
    }
}
