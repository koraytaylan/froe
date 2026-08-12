//! Downstream-facing regressions for bounded garbage-collection journal reads.

use froe::gc_journal::{
    DEFAULT_MAXIMUM_GC_JOURNAL_ENTRIES, DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES,
    DEFAULT_MAXIMUM_GC_JOURNAL_LINE_BYTES, GarbageCollectionJournalReadError,
    GarbageCollectionJournalReadOptions, read_all_gc_journal, read_gc_journal,
};

struct SparseJournalFixture(std::path::PathBuf);

impl SparseJournalFixture {
    fn new() -> Self {
        let unique = format!(
            "froe-gc-journal-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        );
        Self(std::env::temp_dir().join(unique))
    }
}

impl Drop for SparseJournalFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn downstream_callers_can_construct_and_adjust_read_limits() {
    let explicit = GarbageCollectionJournalReadOptions::new(17, 13, 11);
    assert_eq!(explicit.maximum_file_bytes, 17);
    assert_eq!(explicit.maximum_line_bytes, 13);
    assert_eq!(explicit.maximum_entries, 11);

    let mut adjusted = GarbageCollectionJournalReadOptions::default();
    adjusted.maximum_file_bytes = DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1;
    adjusted.maximum_line_bytes = DEFAULT_MAXIMUM_GC_JOURNAL_LINE_BYTES + 1;
    adjusted.maximum_entries = DEFAULT_MAXIMUM_GC_JOURNAL_ENTRIES + 1;
    assert_eq!(
        adjusted,
        GarbageCollectionJournalReadOptions::new(
            DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1,
            DEFAULT_MAXIMUM_GC_JOURNAL_LINE_BYTES + 1,
            DEFAULT_MAXIMUM_GC_JOURNAL_ENTRIES + 1,
        )
    );
}

#[test]
fn default_readers_report_their_froe_only_limit_to_downstream_callers() {
    let fixture = SparseJournalFixture::new();
    let file = std::fs::File::create(&fixture.0).expect("create sparse journal fixture");
    file.set_len(DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1)
        .expect("extend sparse journal fixture");
    drop(file);

    for error in [
        read_gc_journal(&fixture.0).expect_err("last-entry read must report its default limit"),
        read_all_gc_journal(&fixture.0).expect_err("all-entry read must report its default limit"),
    ] {
        assert!(matches!(
            error,
            GarbageCollectionJournalReadError::FileByteLimitExceeded {
                maximum_file_bytes: DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES,
                observed_file_bytes,
            } if observed_file_bytes == DEFAULT_MAXIMUM_GC_JOURNAL_FILE_BYTES + 1
        ));
    }
}

#[test]
fn default_readers_keep_oaks_optional_file_fallback() {
    let fixture = SparseJournalFixture::new();
    let latest = read_gc_journal(&fixture.0).expect("missing optional journal is not an error");
    let all = read_all_gc_journal(&fixture.0).expect("missing optional journal is not an error");

    assert_eq!(
        latest,
        froe::gc_journal::GarbageCollectionJournalEntry::empty()
    );
    assert!(all.is_empty());
}
