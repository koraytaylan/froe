//! A directory holding one `gc.log` for a read to be pointed at.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestDirectory {
    pub(crate) path: PathBuf,
}

impl TestDirectory {
    pub(crate) fn new(name: &str) -> Self {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "froe-gc-journal-{name}-{}-{timestamp}-{sequence}",
            std::process::id(),
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
