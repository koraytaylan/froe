//! Sorting more records than fit in memory.
//!
//! The `nodetype` index of an eighteen-million-node store produces one entry
//! per node per type value. Sorting those in memory is exactly the
//! store-proportional residency plan 0002's bounded-memory case forbids, so
//! they are sorted the way a database does it: fill a run until a declared
//! byte budget is reached, sort and spill it, then merge the runs.
//!
//! Three properties are the point of this module, and each has a test:
//!
//! * **The budget bounds what is resident**, not what is sorted. It is
//!   charged against the bytes actually held, in the unit
//!   [`SpillRecord::resident_size`] declares, and a record that would take
//!   the total past it spills the run first.
//! * **The open-file count is bounded by the fan-in, not by the run count.**
//!   A budget an operator set too low produces thousands of runs; merging
//!   them all at once would open thousands of files. Instead runs are reduced
//!   in passes of [`MAXIMUM_FAN_IN`] until few enough remain, so at most
//!   fan-in plus one file is ever open, whatever budget was chosen.
//! * **Nothing is left behind.** Each spilled run is unlinked when its cursor
//!   is exhausted, and every remaining one on drop. The operation that owns
//!   the run directory can then remove it and find only what other components
//!   left there.
//!
//! The module is generic over its record from the start rather than over
//! this plan's `(key, path)` pair, because plan 0009's Lucene inversion sorts
//! `(field, term, document)` postings and must not have to depend on the
//! segment-store write path to do it. That is also why it lives at the crate
//! root beside [`crate::cache`], [`crate::packed_records`] and
//! [`crate::parallel`] rather than under `writer/`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// How many runs one merge pass may read at once.
///
/// Not the run count: a 16 MiB budget over a 4 GiB input produces 256 runs,
/// and opening 256 files to merge them would make the operator's budget
/// choice a file-descriptor problem. Runs above this many are reduced in
/// passes of this width first.
pub const MAXIMUM_FAN_IN: usize = 64;

/// How many bytes a run buffers before writing, and reads ahead when merging.
const SPILL_BUFFER_BYTES: usize = 256 * 1024;

/// A record an external sort can order, spill and read back.
///
/// The three obligations are separate on purpose. `Ord` is the sort order and
/// is the only thing the merge uses; the encoding is what crosses the file
/// boundary; and the resident size is what the budget is charged against, so
/// a record whose in-memory form is larger than its encoded one is accounted
/// honestly.
pub trait SpillRecord: Ord + Sized {
    /// Appends the record's serialized form to `buffer`.
    fn encode(&self, buffer: &mut Vec<u8>);

    /// Reads one record back from the bytes [`Self::encode`] wrote.
    ///
    /// `bytes` is exactly one record's encoding: the spill format carries the
    /// length, so an implementation never has to find its own end.
    fn decode(bytes: &[u8]) -> Result<Self>;

    /// What holding this record in a run costs, in bytes.
    ///
    /// The accounting unit is **the record's own heap and inline bytes**,
    /// excluding the `Vec` slot that holds it — a per-record constant the
    /// budget would otherwise have to know the layout to include. An
    /// implementation that reports its encoded length is reporting a
    /// lower bound, and should say so.
    fn resident_size(&self) -> usize;
}

/// Where a sort puts its spill files, and under what name.
///
/// Several sorts share one directory — the reference collector's two run
/// sets, plan 0009's postings, doc-value and norm runs — so each names its
/// files under its own prefix and none can collide with another's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunLocation {
    directory: PathBuf,
    name_prefix: String,
}

impl RunLocation {
    /// Spill files named `<prefix>-<number>.run` under `directory`.
    ///
    /// The directory is the caller's: the operation creates one per run and
    /// hands the same one to every sort, and removes it afterwards.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>, name_prefix: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            name_prefix: name_prefix.into(),
        }
    }

    /// The directory the runs go in.
    ///
    /// For a caller that derives a sibling location — the reference
    /// collector puts its two sets under one directory and two prefixes.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The prefix the run files are named under.
    #[must_use]
    pub fn name_prefix(&self) -> &str {
        &self.name_prefix
    }

    fn run_path(&self, number: usize) -> PathBuf {
        self.directory
            .join(format!("{}-{number:06}.run", self.name_prefix))
    }
}

/// One byte budget, shared by every sort charged against it.
///
/// A handle rather than a number, because plan 0009's writer discovers its
/// field set as documents arrive and cannot divide a budget between sorts it
/// has not created yet. Two sorts holding the same handle are bounded
/// *together*; two holding separate handles are bounded separately.
#[derive(Clone, Debug)]
pub struct SortBudget {
    inner: std::rc::Rc<std::cell::Cell<BudgetState>>,
}

#[derive(Clone, Copy, Debug)]
struct BudgetState {
    limit: usize,
    charged: usize,
}

impl SortBudget {
    /// A budget of `limit` resident bytes.
    #[must_use]
    pub fn of_bytes(limit: usize) -> Self {
        Self {
            inner: std::rc::Rc::new(std::cell::Cell::new(BudgetState { limit, charged: 0 })),
        }
    }

    /// The limit, for a caller that reports it.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.inner.get().limit
    }

    /// The bytes currently charged across every holder of this handle.
    #[must_use]
    pub fn charged(&self) -> usize {
        self.inner.get().charged
    }

    /// Charges `bytes`, reporting whether the total is now **over** the
    /// limit.
    ///
    /// Over, not at: a budget of exactly the limit is a budget met, and
    /// spilling there would make the declared number mean one record less
    /// than it says.
    fn charge(&self, bytes: usize) -> bool {
        let mut state = self.inner.get();
        state.charged = state.charged.saturating_add(bytes);
        self.inner.set(state);
        state.charged > state.limit
    }

    fn release(&self, bytes: usize) {
        let mut state = self.inner.get();
        state.charged = state.charged.saturating_sub(bytes);
        self.inner.set(state);
    }
}

/// Records accumulated into bounded runs, spilled when the budget is met.
///
/// Deliberately **not** part of the public surface: the collectors that fill
/// one return a public sorted iterator instead, so a consumer never has to
/// know how the sorting was done. Plan 0009's Lucene writer is in this crate
/// and reaches it as a sibling.
///
pub(crate) struct SortedRuns<Record: SpillRecord> {
    location: RunLocation,
    budget: SortBudget,
    resident: Vec<Record>,
    resident_bytes: usize,
    spilled: Vec<PathBuf>,
    next_run_number: usize,
}

impl<Record: SpillRecord> SortedRuns<Record> {
    /// An empty sort writing its runs at `location`, charged against
    /// `budget`.
    #[must_use]
    pub(crate) fn new(location: RunLocation, budget: SortBudget) -> Self {
        Self {
            location,
            budget,
            resident: Vec::new(),
            resident_bytes: 0,
            spilled: Vec::new(),
            next_run_number: 0,
        }
    }

    /// Appends one record, spilling the current run if the budget is passed.
    pub(crate) fn push(&mut self, record: Record) -> Result<()> {
        let size = record.resident_size();
        self.resident.push(record);
        self.resident_bytes = self.resident_bytes.saturating_add(size);
        if self.budget.charge(size) {
            self.spill()?;
        }
        Ok(())
    }

    /// How many runs have been spilled so far. For a caller that reports it.
    ///
    /// Read by this module's own tests today; task 0707's plan reports it in
    /// the work-directory estimate.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "task 0707's plan is the first production caller")
    )]
    pub(crate) fn spilled_run_count(&self) -> usize {
        self.spilled.len()
    }

    /// Sorts and writes the resident run, releasing its charge.
    fn spill(&mut self) -> Result<()> {
        if self.resident.is_empty() {
            return Ok(());
        }
        self.resident.sort_unstable();
        let path = self.location.run_path(self.next_run_number);
        self.next_run_number += 1;
        write_run(&path, self.resident.drain(..).map(Ok))?;
        self.spilled.push(path);
        self.budget.release(self.resident_bytes);
        self.resident_bytes = 0;
        Ok(())
    }

    /// The sorted sequence, consuming the sort.
    ///
    /// Each spilled run is unlinked as its cursor is exhausted, and any
    /// remaining one when the iterator is dropped.
    pub(crate) fn into_sorted(mut self) -> Result<SortedPass<'static, Record>> {
        self.spill()?;
        let spilled = std::mem::take(&mut self.spilled);
        let runs = self.reduce_to_fan_in(spilled)?;
        SortedPass::over_owned(&runs)
    }

    /// The sorted sequence, walkable more than once.
    ///
    /// Plan 0009's doc-value and norms writers make a statistics pass, a
    /// missing-bitset pass and a write pass over the same input, so the runs
    /// are kept until the [`SortedPasses`] is dropped rather than unlinked as
    /// each cursor is exhausted.
    pub(crate) fn into_sorted_passes(mut self) -> Result<SortedPasses<Record>> {
        self.spill()?;
        let spilled = std::mem::take(&mut self.spilled);
        let runs = self.reduce_to_fan_in(spilled)?;
        Ok(SortedPasses {
            runs,
            resident: Vec::new(),
        })
    }

    /// Merges runs in passes of [`MAXIMUM_FAN_IN`] until few enough remain.
    ///
    /// This is what keeps the open-file count off the run count. Each pass
    /// unlinks the inputs it merged, so the spill directory holds at most one
    /// level's worth of extra bytes at a time.
    ///
    /// A pass **streams**: each record is encoded as the heap yields it, so
    /// what a merge holds is one record a cursor — the fan-in — rather than
    /// the group it is merging. Collecting a group first would put the
    /// budget's own multiple back in memory at exactly the scale the budget
    /// exists for, which is what
    /// `a_merge_pass_holds_one_record_a_cursor_rather_than_its_whole_group`
    /// pins.
    fn reduce_to_fan_in(&mut self, mut runs: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
        while runs.len() > MAXIMUM_FAN_IN {
            record_merge_pass();
            let mut reduced = Vec::with_capacity(runs.len().div_ceil(MAXIMUM_FAN_IN));
            for group in runs.chunks(MAXIMUM_FAN_IN) {
                let path = self.location.run_path(self.next_run_number);
                self.next_run_number += 1;
                let merged = SortedPass::<Record>::over_borrowed(group)?;
                write_run(&path, merged)?;
                for input in group {
                    let _ = std::fs::remove_file(input);
                }
                reduced.push(path);
            }
            runs = reduced;
        }
        Ok(runs)
    }
}

impl<Record: SpillRecord> Drop for SortedRuns<Record> {
    fn drop(&mut self) {
        self.budget.release(self.resident_bytes);
        for path in &self.spilled {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A sorted sequence that can be walked more than once.
pub struct SortedPasses<Record: SpillRecord> {
    runs: Vec<PathBuf>,
    resident: Vec<Record>,
}

impl<Record: SpillRecord> SortedPasses<Record> {
    /// Wraps an already-sorted in-memory sequence, spilling nothing.
    ///
    /// Without this no downstream crate could obtain a `SortedPasses` at all:
    /// the only other producer is the sort itself, which is crate-internal,
    /// and plan 0009's doc-value and norms tests live in separate
    /// integration-test crates that must be able to call the consumers they
    /// cover.
    ///
    /// The caller's ordering is trusted, and a caller that passes an unsorted
    /// sequence gets that sequence back — this is a wrapper, not a sort.
    #[must_use]
    pub fn from_sorted_records(records: Vec<Record>) -> Self {
        Self {
            runs: Vec::new(),
            resident: records,
        }
    }

    /// The sorted sequence, from the beginning.
    pub fn pass(&mut self) -> Result<SortedPass<'_, Record>> {
        if self.runs.is_empty() {
            return Ok(SortedPass::over_slice(&self.resident));
        }
        SortedPass::over_borrowed(&self.runs)
    }
}

impl<Record: SpillRecord> Drop for SortedPasses<Record> {
    fn drop(&mut self) {
        for path in &self.runs {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One walk of a sorted sequence.
///
/// Yields `Result`, because a run is read from disk as it is merged: a
/// truncated or unreadable spill file is a failure of the walk rather than
/// something that could have been reported when it began.
pub struct SortedPass<'records, Record: SpillRecord> {
    heap: BinaryHeap<Reverse<HeapEntry<Record>>>,
    cursors: Vec<RunCursor>,
    /// Set when the pass owns its runs and must unlink them as it goes.
    unlink_exhausted: bool,
    resident: std::slice::Iter<'records, Record>,
    resident_only: bool,
}

impl<Record: SpillRecord> SortedPass<'static, Record> {
    fn over_owned(runs: &[PathBuf]) -> Result<Self> {
        let mut pass = Self::open(runs, true)?;
        pass.unlink_exhausted = true;
        Ok(pass)
    }
}

impl<'records, Record: SpillRecord> SortedPass<'records, Record> {
    fn over_borrowed(runs: &[PathBuf]) -> Result<Self> {
        Self::open(runs, false)
    }

    fn over_slice(records: &'records [Record]) -> Self {
        Self {
            heap: BinaryHeap::new(),
            cursors: Vec::new(),
            unlink_exhausted: false,
            resident: records.iter(),
            resident_only: true,
        }
    }

    fn open(runs: &[PathBuf], owned: bool) -> Result<Self> {
        let mut cursors = Vec::with_capacity(runs.len());
        let mut heap = BinaryHeap::with_capacity(runs.len());
        for path in runs {
            let mut cursor = RunCursor::open(path)?;
            if let Some(record) = cursor.next_record::<Record>()? {
                heap.push(Reverse(HeapEntry {
                    record,
                    run: cursors.len(),
                }));
            }
            cursors.push(cursor);
        }
        record_open_files(cursors.len());
        Ok(Self {
            heap,
            cursors,
            unlink_exhausted: owned,
            resident: [].iter(),
            resident_only: false,
        })
    }
}

impl<Record: SpillRecord> Iterator for SortedPass<'_, Record> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.resident_only {
            // A borrowed in-memory sequence cannot be moved out of, and
            // `SpillRecord` deliberately does not require `Clone`, so the
            // record is round-tripped through its own encoding — the same
            // path a spilled one takes.
            let record = self.resident.next()?;
            let mut buffer = Vec::new();
            record.encode(&mut buffer);
            return Some(Record::decode(&buffer));
        }
        let Reverse(HeapEntry { record, run }) = self.heap.pop()?;
        match self.cursors[run].next_record::<Record>() {
            Ok(Some(next)) => self.heap.push(Reverse(HeapEntry { record: next, run })),
            Ok(None) => {
                if self.unlink_exhausted {
                    self.cursors[run].unlink();
                }
            }
            Err(error) => return Some(Err(error)),
        }
        Some(Ok(record))
    }
}

impl<Record: SpillRecord> Drop for SortedPass<'_, Record> {
    fn drop(&mut self) {
        if self.unlink_exhausted {
            for cursor in &mut self.cursors {
                cursor.unlink();
            }
        }
    }
}

/// The heap orders by record, and by run only to break ties deterministically
/// so a pass over the same runs yields the same sequence every time.
struct HeapEntry<Record: SpillRecord> {
    record: Record,
    run: usize,
}

impl<Record: SpillRecord> Ord for HeapEntry<Record> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.record
            .cmp(&other.record)
            .then_with(|| self.run.cmp(&other.run))
    }
}

impl<Record: SpillRecord> PartialOrd for HeapEntry<Record> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<Record: SpillRecord> PartialEq for HeapEntry<Record> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl<Record: SpillRecord> Eq for HeapEntry<Record> {}

/// One spilled run, read back record by record.
struct RunCursor {
    path: PathBuf,
    reader: Option<BufReader<File>>,
}

impl RunCursor {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: Some(BufReader::with_capacity(SPILL_BUFFER_BYTES, file)),
        })
    }

    fn next_record<Record: SpillRecord>(&mut self) -> Result<Option<Record>> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(None);
        };
        let mut length = [0u8; 4];
        match reader.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Closed here rather than at drop: a cursor that reached the
                // end has released its descriptor, and the accounting has to
                // say so or a multi-pass merge appears to hold every file it
                // ever opened.
                self.reader = None;
                record_closed_file();
                return Ok(None);
            }
            Err(error) => return Err(Error::InputOutput(error)),
        }
        let length = u32::from_le_bytes(length) as usize;
        let mut bytes = vec![0u8; length];
        reader.read_exact(&mut bytes)?;
        Record::decode(&bytes).map(Some)
    }

    /// Closes the file and removes it.
    ///
    /// The descriptor is dropped **first**, because on Windows a file cannot
    /// be unlinked while it is open — and because leaving it open would keep
    /// the merge's file count above the fan-in it promises.
    fn unlink(&mut self) {
        if self.reader.take().is_some() {
            record_closed_file();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for RunCursor {
    fn drop(&mut self) {
        if self.reader.is_some() {
            record_closed_file();
        }
    }
}

/// Writes one sorted run: a length-prefixed sequence, fsynced before the run
/// is considered spilled.
///
/// Refuses to overwrite an existing file. Whether a leftover run from an
/// earlier run is a refusal or a warning is the *operation's* policy — it
/// knows whether the operator named the work directory — and this module only
/// declines to write over one.
fn write_run<Record: SpillRecord>(
    path: &Path,
    records: impl Iterator<Item = Result<Record>>,
) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::InvalidFormat {
                    details: format!(
                        "the spill file {} already exists; a run directory must not be \
                         shared with an earlier run",
                        path.display()
                    ),
                }
            } else {
                Error::InputOutput(error)
            }
        })?;
    let mut writer = BufWriter::with_capacity(SPILL_BUFFER_BYTES, file);
    let mut buffer = Vec::new();
    for record in records {
        buffer.clear();
        record?.encode(&mut buffer);
        let length = u32::try_from(buffer.len()).map_err(|_| Error::InvalidFormat {
            details: format!(
                "a spill record of {} bytes exceeds the format",
                buffer.len()
            ),
        })?;
        writer.write_all(&length.to_le_bytes())?;
        writer.write_all(&buffer)?;
    }
    let mut file = writer
        .into_inner()
        .map_err(|error| Error::InputOutput(std::io::Error::other(error.to_string())))?;
    file.flush()?;
    file.sync_all()?;
    // Rewound so a caller that reopens by path reads from the start; the
    // handle itself is dropped here.
    let _ = file.seek(SeekFrom::Start(0));
    Ok(())
}

// ---------------------------------------------------------------------------
// Test-observable accounting
// ---------------------------------------------------------------------------
//
// `#[cfg(test)] pub(crate)` rather than module-private, the shape
// `repository_lock/publication.rs`'s cutpoint takes: task 0704's merge
// regression and plan 0009's writer both read these without editing this
// file, and nothing in the lib-only build of the `-D warnings` gate reads an
// item only tests use.

#[cfg(test)]
thread_local! {
    static OPEN_RUN_FILES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEAK_OPEN_RUN_FILES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static MERGE_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_open_files(count: usize) {
    OPEN_RUN_FILES.with(|open| {
        let total = open.get() + count;
        open.set(total);
        PEAK_OPEN_RUN_FILES.with(|peak| peak.set(peak.get().max(total)));
    });
}

#[cfg(not(test))]
fn record_open_files(_count: usize) {}

#[cfg(test)]
fn record_closed_file() {
    OPEN_RUN_FILES.with(|open| open.set(open.get().saturating_sub(1)));
}

#[cfg(not(test))]
fn record_closed_file() {}

#[cfg(test)]
fn record_merge_pass() {
    MERGE_PASSES.with(|passes| passes.set(passes.get() + 1));
}

#[cfg(not(test))]
fn record_merge_pass() {}

/// Resets and returns the accounting, for a test that is about to sort.
#[cfg(test)]
pub(crate) fn reset_sort_accounting() {
    OPEN_RUN_FILES.with(|open| open.set(0));
    PEAK_OPEN_RUN_FILES.with(|peak| peak.set(0));
    MERGE_PASSES.with(|passes| passes.set(0));
}

/// The most run files open at once since the last reset.
#[cfg(test)]
pub(crate) fn peak_open_run_files() -> usize {
    PEAK_OPEN_RUN_FILES.with(std::cell::Cell::get)
}

/// How many reduction passes ran since the last reset.
#[cfg(test)]
pub(crate) fn merge_passes() -> usize {
    MERGE_PASSES.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests;
