//! The external sort's own tests.
//!
//! Split out of `mod.rs` when the thousand-line gate found the seam: the
//! sort's mechanism and the properties that pin it are two subjects, and
//! the properties are the longer half.

use super::{
    MAXIMUM_FAN_IN, RunLocation, SortBudget, SortedPasses, SortedRuns, SpillRecord, merge_passes,
    peak_open_run_files, reset_sort_accounting,
};
use crate::Result;

/// A record whose ordering is its bytes, for the generic-surface test.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct ByteString(Vec<u8>);

impl SpillRecord for ByteString {
    fn encode(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(&self.0);
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(Self(bytes.to_vec()))
    }

    fn resident_size(&self) -> usize {
        self.0.len()
    }
}

struct TestDirectory {
    path: std::path::PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "froe-external-sort-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the run directory");
        Self { path }
    }

    fn run_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.path)
            .expect("read the run directory")
            .map(|entry| {
                entry
                    .expect("read an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn record(value: u32) -> ByteString {
    ByteString(format!("{value:08}").into_bytes())
}

thread_local! {
    static LIVE_COUNTED_RECORDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEAK_COUNTED_RECORDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A record that counts how many of its kind exist at once.
///
/// The budget counts bytes a caller pushed, which says nothing about
/// what a *merge* holds: the merge decodes records the budget has
/// already released. Counting live instances is the only way a test can
/// tell a pass that streams from one that materializes what it merges.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
struct CountedRecord(u32);

impl CountedRecord {
    fn new(value: u32) -> Self {
        LIVE_COUNTED_RECORDS.with(|live| {
            let alive = live.get() + 1;
            live.set(alive);
            PEAK_COUNTED_RECORDS.with(|peak| peak.set(peak.get().max(alive)));
        });
        Self(value)
    }
}

impl Drop for CountedRecord {
    fn drop(&mut self) {
        LIVE_COUNTED_RECORDS.with(|live| live.set(live.get().saturating_sub(1)));
    }
}

impl SpillRecord for CountedRecord {
    fn encode(&self, buffer: &mut Vec<u8>) {
        // Big-endian, so the encoding orders the way the record does.
        buffer.extend_from_slice(&self.0.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let value: [u8; 4] = bytes.try_into().map_err(|_| crate::Error::InvalidFormat {
            details: format!("a counted record is four bytes, not {}", bytes.len()),
        })?;
        Ok(Self::new(u32::from_be_bytes(value)))
    }

    fn resident_size(&self) -> usize {
        size_of::<u32>()
    }
}

fn reset_counted_records() {
    LIVE_COUNTED_RECORDS.with(|live| live.set(0));
    PEAK_COUNTED_RECORDS.with(|peak| peak.set(0));
}

fn peak_counted_records() -> usize {
    PEAK_COUNTED_RECORDS.with(std::cell::Cell::get)
}

/// A record's resident size, so a budget can be stated in records.
const RECORD_BYTES: usize = 8;

#[test]
fn a_run_spills_when_the_budget_is_passed_and_not_when_it_is_met() {
    // Exactly the limit is a budget *met*, and spilling there would make
    // a declared budget hold one record less than it says.
    let directory = TestDirectory::new("limit");
    let location = RunLocation::new(&directory.path, "entries");
    let budget = SortBudget::of_bytes(RECORD_BYTES * 4);
    let mut runs = SortedRuns::new(location, budget.clone());
    for value in 0..4 {
        runs.push(record(value)).expect("push");
    }
    assert_eq!(runs.spilled_run_count(), 0, "four records exactly meet it");
    assert_eq!(budget.charged(), RECORD_BYTES * 4);

    runs.push(record(4)).expect("push");
    assert_eq!(
        runs.spilled_run_count(),
        1,
        "the fifth passes the limit and spills the run"
    );
    assert_eq!(budget.charged(), 0, "spilling releases the charge");
}

#[test]
fn one_budget_handle_bounds_every_sort_that_holds_it() {
    // Plan 0009's writer discovers its field set as documents arrive and
    // cannot divide a budget between sorts it has not created yet.
    let directory = TestDirectory::new("shared-budget");
    let budget = SortBudget::of_bytes(RECORD_BYTES * 4);
    let mut first = SortedRuns::new(RunLocation::new(&directory.path, "first"), budget.clone());
    let mut second = SortedRuns::new(RunLocation::new(&directory.path, "second"), budget.clone());
    for value in 0..3 {
        first.push(record(value)).expect("push");
    }
    assert_eq!(first.spilled_run_count(), 0);
    for value in 0..2 {
        second.push(record(value)).expect("push");
    }
    assert_eq!(
        second.spilled_run_count(),
        1,
        "the two sorts are bounded together, not one budget each"
    );
}

#[test]
fn the_sorted_sequence_matches_a_naive_sort_over_many_spills() {
    let directory = TestDirectory::new("order");
    // A deterministic shuffle: a full-period LCG over 100,000 values, so
    // the input is unsorted without a dependency on a random generator.
    let count: u32 = 100_000;
    let mut values: Vec<u32> = (0..count).collect();
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    for index in (1..values.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let pick = (state >> 33) as usize % (index + 1);
        values.swap(index, pick);
    }

    // A budget forcing at least ten spills over the input.
    let budget = SortBudget::of_bytes(RECORD_BYTES * (count as usize / 12));
    let mut runs = SortedRuns::new(RunLocation::new(&directory.path, "entries"), budget);
    for value in &values {
        runs.push(record(*value)).expect("push");
    }
    assert!(
        runs.spilled_run_count() >= 10,
        "the budget must force at least ten spills, not {}",
        runs.spilled_run_count()
    );

    let sorted: Vec<ByteString> = runs
        .into_sorted()
        .expect("merge")
        .collect::<Result<Vec<_>>>()
        .expect("read every record");
    let mut naive: Vec<ByteString> = values.iter().map(|value| record(*value)).collect();
    naive.sort();
    assert_eq!(sorted, naive);
    assert_eq!(sorted.len(), count as usize, "no record was lost");
    assert!(
        directory.run_files().is_empty(),
        "the spill files outlived the iterator: {:?}",
        directory.run_files()
    );
}

#[test]
fn a_single_run_never_spills_and_still_sorts() {
    let directory = TestDirectory::new("single");
    let mut runs = SortedRuns::new(
        RunLocation::new(&directory.path, "entries"),
        SortBudget::of_bytes(1 << 20),
    );
    for value in [3u32, 1, 2] {
        runs.push(record(value)).expect("push");
    }
    assert_eq!(runs.spilled_run_count(), 0);
    let sorted: Vec<ByteString> = runs
        .into_sorted()
        .expect("merge")
        .collect::<Result<Vec<_>>>()
        .expect("read");
    assert_eq!(sorted, vec![record(1), record(2), record(3)]);
}

#[test]
fn more_runs_than_the_fan_in_merge_in_passes_with_a_bounded_open_file_count() {
    // This is the property an operator's budget choice must not be able
    // to break: a budget set very low produces many runs, and merging
    // them all at once would open one file per run.
    let directory = TestDirectory::new("fan-in");
    reset_sort_accounting();
    let record_count = (MAXIMUM_FAN_IN + 5) * 3;
    let mut runs = SortedRuns::new(
        RunLocation::new(&directory.path, "entries"),
        // One record per run: every push after the first spills.
        SortBudget::of_bytes(RECORD_BYTES),
    );
    for value in 0..record_count {
        runs.push(record(value as u32)).expect("push");
    }
    assert!(
        runs.spilled_run_count() > MAXIMUM_FAN_IN,
        "the test needs more runs than the fan-in, not {}",
        runs.spilled_run_count()
    );

    let sorted: Vec<ByteString> = runs
        .into_sorted()
        .expect("merge")
        .collect::<Result<Vec<_>>>()
        .expect("read");
    let expected: Vec<ByteString> = (0..record_count).map(|v| record(v as u32)).collect();
    assert_eq!(sorted, expected);
    assert!(
        merge_passes() >= 1,
        "more runs than the fan-in must trigger a reduction pass"
    );
    assert!(
        peak_open_run_files() <= MAXIMUM_FAN_IN + 1,
        "{} run files were open at once, over the fan-in plus one",
        peak_open_run_files()
    );
    assert!(
        directory.run_files().is_empty(),
        "{:?}",
        directory.run_files()
    );
}

#[test]
fn a_merge_pass_holds_one_record_a_cursor_rather_than_its_whole_group() {
    // The fan-in bounds open files; this bounds the memory behind them.
    // A pass that collected its group before writing it would hold
    // `MAXIMUM_FAN_IN` runs' worth of records — the budget's own
    // multiple — at exactly the scale the budget exists for.
    let directory = TestDirectory::new("merge-residency");
    let records_a_run = 8;
    // One less than a full run, so the run spills on its eighth push.
    let budget = SortBudget::of_bytes((records_a_run - 1) * size_of::<u32>());
    let record_count = (MAXIMUM_FAN_IN + 6) * records_a_run;
    reset_counted_records();
    let mut runs = SortedRuns::new(RunLocation::new(&directory.path, "counted"), budget);
    for value in 0..record_count {
        runs.push(CountedRecord::new(
            u32::try_from(value).expect("a test value fits"),
        ))
        .expect("push");
    }
    assert!(
        runs.spilled_run_count() > MAXIMUM_FAN_IN,
        "the test needs more runs than the fan-in, not {}",
        runs.spilled_run_count()
    );

    // The reduction happens inside `into_sorted`. What a caller does
    // with the sequence afterwards is the caller's residency, not the
    // merge's, so the peak is read before the walk.
    let sorted = runs.into_sorted().expect("merge");
    let peak = peak_counted_records();
    let ceiling = MAXIMUM_FAN_IN + records_a_run + 2;
    assert!(
        peak <= ceiling,
        "a merge pass held {peak} records at once, over the {ceiling} \
         a streaming pass needs: one a cursor, the run being filled, and \
         the record in hand"
    );

    let values: Vec<u32> = sorted
        .map(|record| record.expect("read").0)
        .collect::<Vec<_>>();
    let expected: Vec<u32> = (0..u32::try_from(record_count).expect("a test count fits")).collect();
    assert_eq!(values, expected, "the merge must still sort");
}

#[test]
fn a_spill_refuses_to_overwrite_a_file_that_already_exists() {
    let directory = TestDirectory::new("overwrite");
    // The name the first spill will choose.
    std::fs::write(directory.path.join("entries-000000.run"), b"left over")
        .expect("write the leftover");
    let mut runs = SortedRuns::new(
        RunLocation::new(&directory.path, "entries"),
        SortBudget::of_bytes(RECORD_BYTES),
    );
    runs.push(record(0)).expect("the first push fits");
    let error = runs.push(record(1)).expect_err("the spill must refuse");
    assert!(
        error.to_string().contains("already exists"),
        "the refusal names what it found: {error}"
    );
}

#[test]
fn several_passes_over_one_sequence_yield_the_same_records() {
    // Plan 0009's doc-value and norms writers make a statistics pass, a
    // missing-bitset pass and a write pass over the same input.
    let directory = TestDirectory::new("passes");
    let mut runs = SortedRuns::new(
        RunLocation::new(&directory.path, "entries"),
        SortBudget::of_bytes(RECORD_BYTES * 2),
    );
    for value in [5u32, 1, 4, 2, 3] {
        runs.push(record(value)).expect("push");
    }
    let mut passes = runs.into_sorted_passes().expect("passes");
    let expected: Vec<ByteString> = (1..=5).map(record).collect();
    for attempt in 0..3 {
        let walked: Vec<ByteString> = passes
            .pass()
            .expect("open a pass")
            .collect::<Result<Vec<_>>>()
            .expect("read");
        assert_eq!(walked, expected, "pass {attempt} differs");
    }
    assert!(
        !directory.run_files().is_empty(),
        "the runs must survive until the SortedPasses is dropped"
    );
    drop(passes);
    assert!(
        directory.run_files().is_empty(),
        "{:?}",
        directory.run_files()
    );
}

#[test]
fn an_in_memory_sequence_walks_the_same_way_and_spills_nothing() {
    let directory = TestDirectory::new("in-memory");
    let expected: Vec<ByteString> = (1..=5).map(record).collect();
    let mut passes = SortedPasses::from_sorted_records(expected.clone());
    for attempt in 0..3 {
        let walked: Vec<ByteString> = passes
            .pass()
            .expect("open a pass")
            .collect::<Result<Vec<_>>>()
            .expect("read");
        assert_eq!(walked, expected, "pass {attempt} differs");
    }
    assert!(
        directory.run_files().is_empty(),
        "from_sorted_records must spill nothing"
    );
}

#[test]
fn two_sorts_share_one_directory_under_different_prefixes() {
    let directory = TestDirectory::new("prefixes");
    let budget = SortBudget::of_bytes(RECORD_BYTES);
    let mut first = SortedRuns::new(RunLocation::new(&directory.path, "keys"), budget.clone());
    let mut second = SortedRuns::new(RunLocation::new(&directory.path, "weak"), budget.clone());
    for value in 0..3 {
        first.push(record(value)).expect("push");
        second.push(record(value + 100)).expect("push");
    }
    let names = directory.run_files();
    assert!(
        names.iter().any(|name| name.starts_with("keys-"))
            && names.iter().any(|name| name.starts_with("weak-")),
        "each sort names its files under its own prefix: {names:?}"
    );
    let first: Vec<ByteString> = first
        .into_sorted()
        .expect("merge")
        .collect::<Result<Vec<_>>>()
        .expect("read");
    assert_eq!(first, vec![record(0), record(1), record(2)]);
}
