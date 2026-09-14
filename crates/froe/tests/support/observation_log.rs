//! A recording progress observer, shared by the test binaries that assert
//! what an operation reports.
//!
//! It records every call **and asserts the sequence's invariants as it
//! goes**, so a malformed report fails at the call that broke it rather
//! than at some later assertion about a count. Three properties a renderer
//! is entitled to assume: every advance falls inside a begin/end pair,
//! counts never decrease, and a count never overshoots its declared total.
//!
//! Included from `tests/support/` rather than copied, so the reindex tests
//! of plan 0007 and the progress regressions assert against one observer
//! rather than two that could drift.

#![allow(
    dead_code,
    reason = "every binary declaring `support` compiles this, and none uses all of it"
)]
#![allow(
    unreachable_pub,
    reason = "this module is compiled into test binaries, where pub only means module-visible"
)]

use froe::progress::{ProgressObserver, Step, WorkUnit};

/// One reported call, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reported {
    Began {
        description: String,
        unit: WorkUnit,
        total: Option<u64>,
    },
    Advanced(u64),
    TotalResolved(u64),
    Ended,
}

/// Records every call and asserts the sequence's invariants as it goes, so
/// a malformed report fails at the call that broke it rather than at some
/// later assertion.
#[derive(Default)]
pub(crate) struct ObservationLog {
    /// Every call, in order.
    pub(crate) calls: Vec<Reported>,
    active: Option<(String, Option<u64>)>,
    last_count: u64,
}

impl ObservationLog {
    pub(crate) fn descriptions(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Reported::Began { description, .. } => Some(description.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The highest count reported within the step named `description`.
    pub(crate) fn highest_count_of(&self, description: &str) -> Option<u64> {
        let mut within = false;
        let mut highest = None;
        for call in &self.calls {
            match call {
                Reported::Began {
                    description: began, ..
                } => within = began == description,
                Reported::Ended => within = false,
                Reported::Advanced(count) if within => {
                    highest = Some(highest.map_or(*count, |previous: u64| previous.max(*count)));
                }
                _ => {}
            }
        }
        highest
    }

    pub(crate) fn began_and_ended_in_pairs(&self) -> bool {
        let mut open = false;
        for call in &self.calls {
            match call {
                Reported::Began { .. } => open = true,
                Reported::Ended => open = false,
                _ => {}
            }
        }
        !open
    }
}

impl ProgressObserver for ObservationLog {
    fn step_began(&mut self, step: &Step<'_>) {
        assert!(
            !step.description().is_empty(),
            "every step names the work it is doing"
        );
        self.active = Some((step.description().to_owned(), step.total()));
        self.last_count = 0;
        self.calls.push(Reported::Began {
            description: step.description().to_owned(),
            unit: step.unit(),
            total: step.total(),
        });
    }

    fn step_advanced(&mut self, completed: u64) {
        let (description, total) = self
            .active
            .as_ref()
            .expect("an advance outside a step has nothing to advance");
        assert!(
            completed >= self.last_count,
            "{description}: counts must not run backwards ({completed} after {})",
            self.last_count
        );
        if let Some(total) = total {
            assert!(
                completed <= *total,
                "{description}: counted {completed} of a declared {total}"
            );
        }
        self.last_count = completed;
        self.calls.push(Reported::Advanced(completed));
    }

    fn step_total_resolved(&mut self, total: u64) {
        let (description, _) = self
            .active
            .as_ref()
            .expect("a total outside a step belongs to nothing");
        assert!(
            total >= self.last_count,
            "{description}: a resolved total of {total} is below the {} already counted",
            self.last_count
        );
        if let Some(active) = self.active.as_mut() {
            active.1 = Some(total);
        }
        self.calls.push(Reported::TotalResolved(total));
    }

    fn step_ended(&mut self) {
        self.active = None;
        self.calls.push(Reported::Ended);
    }
}
