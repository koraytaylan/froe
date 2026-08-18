//! Where the head is and how it moves: the compare-and-set a caller
//! commits through, and the persisted value a rollback restores.

use super::{File, NodeState, Path, RecordIdentifier, Result, WritableRepository};

/// Whether appending a journal entry first needs a line separator.
pub(in crate::writer::store_writer) fn journal_needs_separator(path: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(!matches!(last[0], b'\n' | b'\r'))
}

impl WritableRepository {
    /// The current in-memory head record.
    #[must_use]
    pub fn head(&self) -> RecordIdentifier {
        self.lock_write_state().head
    }

    /// Compare-and-set of the head. Returns whether the head moved.
    pub fn compare_and_set_head(
        &self,
        expected: RecordIdentifier,
        new_head: RecordIdentifier,
    ) -> bool {
        let mut state = self.lock_write_state();
        if state.head == expected {
            state.head = new_head;
            true
        } else {
            false
        }
    }

    /// Replaces the head unconditionally (the compaction primitive).
    pub fn replace_head(&self, new_head: RecordIdentifier) {
        self.lock_write_state().head = new_head;
    }

    /// Marks `head` as the persisted head after an out-of-band journal
    /// rewrite (compaction), so the next flush does not re-append a line —
    /// and reopens the journal handle onto the freshly written file, so a
    /// later head-moving flush in the same session appends to the live
    /// journal rather than the unlinked old inode.
    pub fn reset_persisted_head(&self, head: RecordIdentifier) -> Result<()> {
        let mut state = self.lock_write_state();
        state.head = head;
        state.persisted_head = Some(head);
        state.journal_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("journal.log"))?;
        Ok(())
    }

    /// The head node state (the super-root).
    #[must_use]
    pub fn head_node(&self) -> NodeState<'_> {
        NodeState::new(self, self.head())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Repository;
    use crate::writer::record_writer::ChildNodesToWrite;
    use crate::writer::store_writer::test_support::*;
    use std::io::Write as _;

    #[test]
    fn flush_without_head_movement_syncs_segments_but_appends_no_journal_line() {
        let directory = TestDirectory::new("flush-pending");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        // Write a segment without moving the head, then flush: the
        // archive fsync must run (flush succeeds with a pending writer)
        // while the journal stays untouched.
        let generation = store.writing_generation().expect("generation");
        let mut writer = store.record_writer(generation);
        writer
            .write_node(Some("nt:unstructured"), &[], &ChildNodesToWrite::Zero, &[])
            .expect("node");
        writer.finish().expect("finish");
        store.flush().expect("flush with pending segments");
        let journal = std::fs::read_to_string(directory.path.join("journal.log")).expect("journal");
        assert_eq!(
            journal.lines().count(),
            1,
            "only the bootstrap line: an unchanged head appends nothing"
        );
        store.close().expect("close");
    }

    #[test]
    fn flushing_without_head_movement_writes_no_journal_line() {
        let directory = TestDirectory::new("no-movement");
        let store = WritableRepository::open(&directory.path).expect("bootstrap");
        store.flush().expect("first flush");
        store.flush().expect("second flush");
        store.close().expect("close");
        let journal = std::fs::read_to_string(directory.path.join("journal.log")).expect("journal");
        assert_eq!(journal.lines().count(), 1);
    }

    #[test]
    fn head_moving_flush_separates_an_unterminated_malformed_journal_tail() {
        let directory = TestDirectory::new("torn-journal-tail");
        {
            let store = WritableRepository::open(&directory.path).expect("bootstrap");
            store.close().expect("close bootstrap");
        }
        let journal_path = directory.path.join("journal.log");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open journal for simulated torn append")
            .write_all(b"malformed-unterminated-tail")
            .expect("append torn tail");

        let committed_head = {
            let store = WritableRepository::open(&directory.path).expect("bind before torn tail");
            crate::writer::commit::create_checkpoint(&store, 10_000_000, &[])
                .expect("head-moving checkpoint");
            let head = store.head();
            store.close().expect("close after checkpoint");
            head
        };

        let journal = std::fs::read(&journal_path).expect("read journal");
        assert!(
            journal
                .windows(b"malformed-unterminated-tail\n".len())
                .any(|window| window == b"malformed-unterminated-tail\n"),
            "the new durable revision must not be concatenated to a malformed tail"
        );
        let committed_prefix = format!(
            "{}:{} root ",
            committed_head.segment, committed_head.record_number
        );
        assert!(
            journal
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(committed_prefix.as_bytes())),
            "the exact committed head must occupy its own journal line"
        );

        let repository = Repository::open(&directory.path).expect("reopen healthy repository");
        assert_eq!(repository.head_record_identifier(), committed_head);
        repository
            .content_root()
            .expect("content root remains readable");
        assert_eq!(repository.checkpoints().expect("checkpoints").len(), 1);
    }
}
