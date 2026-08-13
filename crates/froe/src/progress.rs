//! Progress observation for long-running operations.
//!
//! Opening a large store, planning a cleanup, compacting, and checking
//! consistency all take minutes on a real repository. Every such operation
//! has a `_with_progress` twin that reports what it is doing to a
//! [`ProgressObserver`], so a caller can render a live report; the plain
//! spelling keeps its original signature and reports to
//! [`DiscardedProgress`].
//!
//! Observation is strictly additive: an observer is told what an operation
//! is doing and can never influence it. The operations produce identical
//! results whichever observer they are given, and nothing an observer does
//! is allowed to change the sequence of file operations — implementations
//! must not mutate the repository, must not panic, and should return
//! quickly, because [`ProgressObserver::step_advanced`] is called from
//! inner loops.
//!
//! # Example
//!
//! ```no_run
//! use froe::progress::{ProgressObserver, Step};
//!
//! /// Prints one line per step.
//! struct Narrator;
//!
//! impl ProgressObserver for Narrator {
//!     fn step_began(&mut self, step: &Step<'_>) {
//!         match step.total() {
//!             Some(total) => eprintln!("{} (0/{total} {})", step.description(), step.unit()),
//!             None => eprintln!("{}", step.description()),
//!         }
//!     }
//!
//!     fn step_advanced(&mut self, _completed: u64) {}
//!
//!     fn step_ended(&mut self) {}
//! }
//!
//! let mut narrator = Narrator;
//! let repository = froe::store::Repository::open_with_progress(
//!     std::path::Path::new("/path/to/segmentstore"),
//!     &mut narrator,
//! )?;
//! # Ok::<(), froe::Error>(())
//! ```

/// What a step counts.
///
/// The unit tells a renderer how to word and scale a report: a count of
/// [`WorkUnit::Bytes`] is rendered as a size, every other unit as a plain
/// count of the named things.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorkUnit {
    /// TAR archive files.
    Archives,
    /// Segments within archives.
    Segments,
    /// Content nodes.
    Nodes,
    /// Journal revisions.
    Revisions,
    /// Lines of `journal.log`.
    JournalLines,
    /// Named checkpoints.
    Checkpoints,
    /// Files in the repository directory.
    Files,
    /// Bytes of content.
    Bytes,
}

impl WorkUnit {
    /// The plural noun a report uses for this unit, for example
    /// `"archives"`. [`WorkUnit::Bytes`] is `"bytes"`; a renderer that
    /// scales byte counts substitutes its own scaled suffix.
    #[must_use]
    pub fn plural_noun(self) -> &'static str {
        match self {
            Self::Archives => "archives",
            Self::Segments => "segments",
            Self::Nodes => "nodes",
            Self::Revisions => "revisions",
            Self::JournalLines => "journal lines",
            Self::Checkpoints => "checkpoints",
            Self::Files => "files",
            Self::Bytes => "bytes",
        }
    }

    /// The singular noun, for a report of exactly one item.
    #[must_use]
    pub fn singular_noun(self) -> &'static str {
        match self {
            Self::Archives => "archive",
            Self::Segments => "segment",
            Self::Nodes => "node",
            Self::Revisions => "revision",
            Self::JournalLines => "journal line",
            Self::Checkpoints => "checkpoint",
            Self::Files => "file",
            Self::Bytes => "byte",
        }
    }

    /// The noun agreeing with `count`.
    #[must_use]
    pub fn noun_for(self, count: u64) -> &'static str {
        if count == 1 {
            self.singular_noun()
        } else {
            self.plural_noun()
        }
    }
}

impl std::fmt::Display for WorkUnit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.plural_noun())
    }
}

/// One named step of a long-running operation.
///
/// The description is a lowercase present-participle phrase naming the work
/// — `"scanning archives"` — so a renderer can present it directly. A step
/// carries a `total` only when the operation can count its work before
/// starting; otherwise the count is open-ended and
/// [`ProgressObserver::step_total_resolved`] may supply it later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step<'description> {
    description: &'description str,
    unit: WorkUnit,
    total: Option<u64>,
}

impl<'description> Step<'description> {
    /// A step counting `unit` items, with an unknown total.
    #[must_use]
    pub fn new(description: &'description str, unit: WorkUnit) -> Self {
        Self {
            description,
            unit,
            total: None,
        }
    }

    /// The same step with a known total.
    #[must_use]
    pub fn with_total(mut self, total: u64) -> Self {
        self.total = Some(total);
        self
    }

    /// The phrase naming the work.
    #[must_use]
    pub fn description(&self) -> &'description str {
        self.description
    }

    /// What the step counts.
    #[must_use]
    pub fn unit(&self) -> WorkUnit {
        self.unit
    }

    /// The total number of items, when known before the step starts.
    #[must_use]
    pub fn total(&self) -> Option<u64> {
        self.total
    }
}

/// Receives the steps of a long-running operation.
///
/// An operation calls [`Self::step_began`], then [`Self::step_advanced`]
/// zero or more times, then [`Self::step_ended`]. Steps are sequential:
/// implementations need not handle nesting, and both edges are forgiving
/// so no caller can leave an implementation in a broken state —
/// `step_began` while a step is active ends that step first, and
/// `step_ended` without an active step does nothing.
///
/// Counts passed to `step_advanced` are *cumulative* completed items
/// within the current step, never deltas, and never decrease.
pub trait ProgressObserver {
    /// A new step began.
    fn step_began(&mut self, step: &Step<'_>);

    /// `completed` items of the current step are done, cumulatively.
    fn step_advanced(&mut self, completed: u64);

    /// The current step's total became known after it began. The default
    /// implementation ignores it.
    fn step_total_resolved(&mut self, total: u64) {
        let _ = total;
    }

    /// The current step finished. An operation that fails mid-step still
    /// ends the step before returning its error.
    fn step_ended(&mut self);
}

/// The observer that discards every report.
///
/// This is what the plain spelling of every observable operation passes,
/// so observation costs nothing when nobody is watching.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiscardedProgress;

impl ProgressObserver for DiscardedProgress {
    fn step_began(&mut self, _step: &Step<'_>) {}

    fn step_advanced(&mut self, _completed: u64) {}

    fn step_ended(&mut self) {}
}

/// Forwards to a borrowed observer, so `&mut dyn ProgressObserver` can be
/// passed on where an `impl ProgressObserver` is expected.
impl<Observer: ProgressObserver + ?Sized> ProgressObserver for &mut Observer {
    fn step_began(&mut self, step: &Step<'_>) {
        (**self).step_began(step);
    }

    fn step_advanced(&mut self, completed: u64) {
        (**self).step_advanced(completed);
    }

    fn step_total_resolved(&mut self, total: u64) {
        (**self).step_total_resolved(total);
    }

    fn step_ended(&mut self) {
        (**self).step_ended();
    }
}

/// A collection length as a progress count. Saturating rather than
/// casting keeps the conversion exact on every target width without an
/// unproved `as`; a repository with more than `u64::MAX` of anything
/// cannot exist.
pub(crate) fn count(items: usize) -> u64 {
    u64::try_from(items).unwrap_or(u64::MAX)
}

/// Runs `work` bracketed by a step: the step ends whether `work` succeeds
/// or fails, so an error can never leave a step open.
pub(crate) fn observe<Value, Error>(
    observer: &mut dyn ProgressObserver,
    step: &Step<'_>,
    work: impl FnOnce(&mut dyn ProgressObserver) -> std::result::Result<Value, Error>,
) -> std::result::Result<Value, Error> {
    observer.step_began(step);
    let outcome = work(observer);
    observer.step_ended();
    outcome
}

/// Reports cumulative progress at most every `stride` items, so an inner
/// loop over millions of items does not call an observer millions of
/// times. The final count is reported by the step's end, not here.
pub(crate) struct StrideCounter {
    completed: u64,
    last_reported: u64,
    stride: u64,
}

impl StrideCounter {
    /// A counter reporting every `stride` items. A zero stride is treated
    /// as one, so every item is reported.
    pub(crate) fn new(stride: u64) -> Self {
        Self::resuming(stride, 0)
    }

    /// A counter continuing from `already`, for a step whose work is
    /// split across several calls. Counts within one step are cumulative,
    /// so a second call must not restart at zero and make them decrease.
    pub(crate) fn resuming(stride: u64, already: u64) -> Self {
        Self {
            completed: already,
            last_reported: already,
            stride: stride.max(1),
        }
    }

    /// Counts one item and reports when the stride has elapsed.
    pub(crate) fn advance(&mut self, observer: &mut dyn ProgressObserver) {
        self.completed += 1;
        if self.completed - self.last_reported >= self.stride {
            self.last_reported = self.completed;
            observer.step_advanced(self.completed);
        }
    }

    /// Reports the exact final count. Without this the last partial
    /// stride would never be reported, and a step that counted fewer
    /// items than one stride would report none at all.
    pub(crate) fn finish(&mut self, observer: &mut dyn ProgressObserver) {
        if self.completed != self.last_reported {
            self.last_reported = self.completed;
            observer.step_advanced(self.completed);
        }
    }

    /// How many items have been counted.
    pub(crate) fn completed(&self) -> u64 {
        self.completed
    }
}

#[cfg(test)]
mod tests {
    use super::{DiscardedProgress, ProgressObserver, Step, StrideCounter, WorkUnit, observe};

    /// Records every call, so tests can assert the exact sequence.
    #[derive(Default)]
    struct RecordingObserver {
        calls: Vec<String>,
    }

    impl ProgressObserver for RecordingObserver {
        fn step_began(&mut self, step: &Step<'_>) {
            self.calls.push(match step.total() {
                Some(total) => format!("began {} of {total} {}", step.description(), step.unit()),
                None => format!("began {} of {}", step.description(), step.unit()),
            });
        }

        fn step_advanced(&mut self, completed: u64) {
            self.calls.push(format!("advanced {completed}"));
        }

        fn step_total_resolved(&mut self, total: u64) {
            self.calls.push(format!("total {total}"));
        }

        fn step_ended(&mut self) {
            self.calls.push("ended".to_owned());
        }
    }

    #[test]
    fn a_step_carries_its_description_unit_and_total() {
        let step = Step::new("scanning archives", WorkUnit::Archives).with_total(48);
        assert_eq!(step.description(), "scanning archives");
        assert_eq!(step.unit(), WorkUnit::Archives);
        assert_eq!(step.total(), Some(48));
        assert_eq!(Step::new("tracing", WorkUnit::Segments).total(), None);
    }

    #[test]
    fn units_name_themselves_in_the_plural() {
        assert_eq!(WorkUnit::Archives.plural_noun(), "archives");
        assert_eq!(WorkUnit::JournalLines.to_string(), "journal lines");
    }

    #[test]
    fn the_discarding_observer_accepts_every_call() {
        let mut observer = DiscardedProgress;
        observer.step_began(&Step::new("working", WorkUnit::Files));
        observer.step_advanced(7);
        observer.step_total_resolved(9);
        observer.step_ended();
    }

    #[test]
    fn bracketing_ends_the_step_after_a_failure() {
        let mut observer = RecordingObserver::default();
        let outcome: std::result::Result<(), &str> = observe(
            &mut observer,
            &Step::new("failing", WorkUnit::Files),
            |_observer| Err("no"),
        );
        assert_eq!(outcome, Err("no"));
        assert_eq!(observer.calls, ["began failing of files", "ended"]);
    }

    #[test]
    fn a_borrowed_observer_forwards_every_call() {
        let mut observer = RecordingObserver::default();
        let borrowed: &mut dyn ProgressObserver = &mut observer;
        borrowed.step_began(&Step::new("copying", WorkUnit::Nodes).with_total(2));
        borrowed.step_advanced(1);
        borrowed.step_total_resolved(3);
        borrowed.step_ended();
        assert_eq!(
            observer.calls,
            ["began copying of 2 nodes", "advanced 1", "total 3", "ended"]
        );
    }

    #[test]
    fn the_stride_counter_reports_only_on_the_stride() {
        let mut observer = RecordingObserver::default();
        let mut counter = StrideCounter::new(4);
        for _ in 0..9 {
            counter.advance(&mut observer);
        }
        assert_eq!(counter.completed(), 9);
        assert_eq!(observer.calls, ["advanced 4", "advanced 8"]);
    }

    #[test]
    fn a_zero_stride_reports_every_item() {
        let mut observer = RecordingObserver::default();
        let mut counter = StrideCounter::new(0);
        counter.advance(&mut observer);
        counter.advance(&mut observer);
        assert_eq!(observer.calls, ["advanced 1", "advanced 2"]);
    }
}
