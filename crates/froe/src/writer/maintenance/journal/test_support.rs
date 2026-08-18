//! A directory holding one journal for a rewrite to be pointed at.

use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const FIRST: &str = "11111111-1111-4111-a111-111111111111:1";

pub(crate) const SECOND: &str = "22222222-2222-4222-a222-222222222222:2";

pub(crate) static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestDirectory {
    pub(crate) path: std::path::PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "froe-journal-maintenance-{name}-{}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self { path }
    }

    pub(crate) fn write_journal(&self, bytes: &[u8]) {
        std::fs::write(self.path.join("journal.log"), bytes).expect("write journal");
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
