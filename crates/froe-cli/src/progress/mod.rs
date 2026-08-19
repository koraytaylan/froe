//! One progress reporter for every command.
//!
//! [`Reporter`] is what a `froe` command tells about its work. It renders
//! [`froe::progress`] steps to **standard error**, so standard output stays
//! pure data: piping a JSON lines export or a cleanup plan into a consumer
//! never mixes a progress bar into the stream, and a destructive plan is
//! never interleaved with one.
//!
//! Three rendering styles, chosen once at startup:
//!
//! * **animated** — standard error is a terminal: one live line, rewritten
//!   in place, with a bar, a percentage, counts, elapsed time, and an
//!   estimate of the time remaining. The line is erased before anything
//!   else is written, so a confirmation prompt is never printed over it.
//! * **plain** — standard error is a pipe, a file, or a CI log: the same
//!   information as whole lines, at most one every two seconds, so a
//!   captured log stays readable and finite.
//! * **silent** — `--silent`: nothing at all.
//!
//! A step that finishes quickly reports nothing: rendering is deferred by
//! [`ANNOUNCE_DELAY`], so a command that takes a moment stays quiet and
//! only a command that makes the operator wait explains itself. `--progress
//! always` sets that delay to zero, which is what a script wanting the
//! reports in its log — or a test asserting them — should pass.
//!
//! The reporter emits **no ANSI escape sequences**: it moves the cursor
//! with a carriage return and clears with spaces, and repository-controlled
//! text is sanitized before it is rendered. Terminal escapes can never
//! reach a terminal through a progress line, whatever a repository
//! contains.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use froe::progress::{ProgressObserver, Step, WorkUnit};

use crate::output::sanitize_terminal_text;

mod export_sink;
mod render;
#[cfg(test)]
mod test_support;

pub(crate) use export_sink::*;
pub(crate) use render::*;

mod live;

pub(crate) use live::*;

/// When progress is reported.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum ProgressWhen {
    /// Animate on a terminal, write plain lines elsewhere, and report only
    /// steps that run long enough to be worth mentioning.
    Auto,
    /// Report every step from the moment it begins, in the style the
    /// stream supports. Deterministic, for logs and tests.
    Always,
    /// Report nothing.
    Never,
}

/// How reports are rendered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Style {
    /// A live line rewritten in place.
    Animated,
    /// Whole lines, throttled.
    Plain,
    /// Nothing.
    Silent,
}

/// The stderr reporter a command tells about its work.
///
/// Cloning is cheap and every clone reports to the same stream: the
/// observer handed to the library and the one the command prints its own
/// notes through are the same reporter. The rendering thread stops when
/// the last clone is dropped.
#[derive(Clone)]
pub(crate) struct Reporter {
    pub(crate) inner: Arc<ReporterInner>,
    /// Kept alive by every clone; dropping the last one stops the ticker.
    pub(crate) _ticker: Option<Arc<Ticker>>,
}

/// Suppresses the write-generated SIGPIPE for the duration of `work`, so
/// a write to a closed stream fails with `EPIPE` rather than terminating
/// the process.
///
/// Two mechanisms, because the signal is not directed the same way on
/// every Unix. Linux raises it on the *thread* that wrote (`force_sig`
/// in `pipe_write`), so blocking it on that thread is enough. Darwin
/// raises it on the *process* — XNU's `sys_generic.c` write path does
/// `psignal(vfs_context_proc(ctx), SIGPIPE)` — and `get_signalthread`
/// then hands it to any thread that does not have it masked. froe always
/// has one: the ticker inherits an empty mask from `pthread_create`. A
/// per-thread mask is therefore structurally insufficient there, so the
/// descriptor itself is marked `F_SETNOSIGPIPE`, the flag that same XNU
/// write path tests before raising anything.
///
/// Both are scoped to `work` and restored afterwards, including on
/// unwind, so the conventional terminate-on-closed-*stdout* behaviour the
/// CLI deliberately keeps is untouched.
#[cfg(unix)]
pub(crate) fn without_sigpipe<Value>(work: impl FnOnce() -> Value) -> Value {
    #[cfg(target_vendor = "apple")]
    {
        /// Darwin's per-file-description SIGPIPE disposition, from
        /// `bsd/sys/fcntl.h`:
        ///
        /// ```text
        /// #define F_SETNOSIGPIPE  73  /* set the SIGPIPE disposition */
        /// #define F_GETNOSIGPIPE  74  /* get the SIGPIPE disposition */
        /// ```
        ///
        /// `libc` does not declare them (it has only the socket-level
        /// `SO_NOSIGPIPE`), so they are named here. A wrong value cannot
        /// pass silently: `fcntl` would return -1, no guard would be
        /// built, and the macOS guard test would fail.
        const F_GETNOSIGPIPE: libc::c_int = 74;
        const F_SETNOSIGPIPE: libc::c_int = 73;

        /// Restores standard error's no-SIGPIPE flag to what it was.
        struct RestoreNoSigpipe {
            previous: libc::c_int,
        }

        impl Drop for RestoreNoSigpipe {
            fn drop(&mut self) {
                // SAFETY: `F_SETNOSIGPIPE` takes an int by value and only
                // sets a flag on the already-open standard error
                // description; the value written is the one just read
                // from it.
                unsafe {
                    libc::fcntl(libc::STDERR_FILENO, F_SETNOSIGPIPE, self.previous);
                }
            }
        }

        // SAFETY: `F_GETNOSIGPIPE` only reads the flag of an open
        // descriptor and returns -1 on failure, in which case nothing is
        // changed and no guard is built.
        let previous = unsafe { libc::fcntl(libc::STDERR_FILENO, F_GETNOSIGPIPE) };
        let _restore = (previous >= 0).then(|| {
            // SAFETY: as above; this sets the flag for the duration of
            // the guard only.
            unsafe {
                libc::fcntl(libc::STDERR_FILENO, F_SETNOSIGPIPE, 1);
            }
            RestoreNoSigpipe { previous }
        });
        without_sigpipe_masked(work)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        without_sigpipe_masked(work)
    }
}

/// Blocks SIGPIPE on this thread for the duration of `work`, draining one
/// raised while it was blocked before the previous mask is restored — it
/// would otherwise simply fire a moment later.
#[cfg(unix)]
pub(crate) fn without_sigpipe_masked<Value>(work: impl FnOnce() -> Value) -> Value {
    /// Restores the thread's signal mask however the guarded section
    /// leaves — return or unwind. A mask that outlived a panic would
    /// leave SIGPIPE blocked for the rest of the thread's life, and the
    /// conventional terminate-on-closed-stdout behaviour with it.
    struct RestoreMask {
        blocked: libc::sigset_t,
        previous: libc::sigset_t,
    }

    impl Drop for RestoreMask {
        fn drop(&mut self) {
            // SAFETY: both sets were initialized before this guard was
            // built. `sigpending` only reads, and `sigwait` consumes at
            // most the SIGPIPE this guard itself blocked — it cannot
            // block, because it runs only when that signal is already
            // pending. The mask is then returned to exactly what it was.
            unsafe {
                let mut pending: libc::sigset_t = std::mem::zeroed();
                if libc::sigpending(&raw mut pending) == 0
                    && libc::sigismember(&raw const pending, libc::SIGPIPE) == 1
                {
                    let mut consumed: libc::c_int = 0;
                    libc::sigwait(&raw const self.blocked, &raw mut consumed);
                }
                libc::pthread_sigmask(
                    libc::SIG_SETMASK,
                    &raw const self.previous,
                    std::ptr::null_mut(),
                );
            }
        }
    }

    // SAFETY: every call operates on this thread's own signal mask, and
    // the sets are fully initialized by `sigemptyset`/`sigaddset` before
    // any use. Blocking is skipped entirely if either the set or the mask
    // call fails, so the guard is only built once the mask really changed.
    let _restore = unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&raw mut blocked) != 0
            || libc::sigaddset(&raw mut blocked, libc::SIGPIPE) != 0
        {
            return work();
        }
        let mut previous: libc::sigset_t = std::mem::zeroed();
        if libc::pthread_sigmask(libc::SIG_BLOCK, &raw const blocked, &raw mut previous) != 0 {
            return work();
        }
        RestoreMask { blocked, previous }
    };
    work()
}

/// Non-Unix targets do not raise SIGPIPE; a closed stream already
/// surfaces as an ordinary write error.
#[cfg(not(unix))]
pub(crate) fn without_sigpipe<Value>(work: impl FnOnce() -> Value) -> Value {
    work()
}

impl Reporter {
    /// The reporter for a command line: `silent` wins over `when`.
    pub(crate) fn new(when: ProgressWhen, silent: bool) -> Self {
        let style = if silent || when == ProgressWhen::Never {
            Style::Silent
        } else if std::io::stderr().is_terminal() {
            Style::Animated
        } else {
            Style::Plain
        };
        let announce_delay = if when == ProgressWhen::Always {
            Duration::ZERO
        } else {
            ANNOUNCE_DELAY
        };
        let redraw_interval = match style {
            Style::Animated => ANIMATED_REDRAW_INTERVAL,
            Style::Plain | Style::Silent => PLAIN_REPORT_INTERVAL,
        };
        Self::with_output(
            style,
            announce_delay,
            redraw_interval,
            None,
            Box::new(std::io::stderr()),
        )
    }

    /// A reporter writing to `out` in `style`. Tests pass a zero redraw
    /// interval so every report is observable without waiting for one,
    /// and an explicit width so no test touches the environment.
    pub(super) fn with_output(
        style: Style,
        announce_delay: Duration,
        redraw_interval: Duration,
        width: Option<usize>,
        out: Box<dyn Write + Send>,
    ) -> Self {
        let inner = Arc::new(ReporterInner {
            style,
            announce_delay,
            redraw_interval,
            unicode_bar: style == Style::Animated && supports_unicode_bar(),
            width,
            state: Mutex::new(RenderState {
                out,
                active: None,
                live_line_width: 0,
                suspended: false,
                stream_closed: false,
                first_step_at: None,
                last_line_at: None,
            }),
        });
        // Both reporting styles tick. The animated one redraws its live
        // line; the plain one needs the tick even more, because a step
        // whose body makes no observer call — a phase with no counter of
        // its own — would otherwise never render at all, and so would
        // never announce itself or print a completion line. A silent
        // reporter has nothing to tick for.
        let ticker = (style != Style::Silent).then(|| Arc::new(Ticker::start(&inner)));
        Self {
            inner,
            _ticker: ticker,
        }
    }

    /// A reporter that discards everything. Used where a live report
    /// would corrupt the command's own output — an export streaming to a
    /// terminal's standard output shares the screen with it.
    pub(crate) fn silent() -> Self {
        Self::with_output(
            Style::Silent,
            ANNOUNCE_DELAY,
            PLAIN_REPORT_INTERVAL,
            None,
            Box::new(std::io::sink()),
        )
    }

    /// Writes one informational line — what the command is about to do,
    /// or a note about how it behaves. Suppressed by `--silent`; never
    /// used for warnings, errors, or results, which are not progress.
    pub(crate) fn status(&self, message: &str) {
        if self.inner.style == Style::Silent {
            return;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|error| {
            self.inner.state.clear_poison();
            error.into_inner()
        });
        ReporterInner::erase_live_line(&mut state);
        let line = format!("froe: {}\n", sanitize_terminal_text(message));
        state.write_line(&line);
    }

    /// Runs `work` with the stream to itself: the live line is erased
    /// first and nothing is drawn until `work` returns. Every write that
    /// is not the reporter's own — a confirmation prompt above all — goes
    /// through here, so a prompt is never printed over a progress line.
    pub(crate) fn while_suspended<Value>(&self, work: impl FnOnce() -> Value) -> Value {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|error| {
                self.inner.state.clear_poison();
                error.into_inner()
            });
            ReporterInner::erase_live_line(&mut state);
            state.suspended = true;
        }
        let value = work();
        let mut state = self.inner.state.lock().unwrap_or_else(|error| {
            self.inner.state.clear_poison();
            error.into_inner()
        });
        state.suspended = false;
        value
    }

    /// Ends any active step and erases the live line, so whatever the
    /// command prints next — a result, an error — starts on a clean line.
    pub(crate) fn finish(&self) {
        self.step_ended_locked(CompletionLine::Write);
    }

    /// Ends the active step and erases its line without the completion
    /// line. For a step whose caller reports the outcome itself — an
    /// export names its destination, which the step cannot — the
    /// completion line would only say the same thing twice.
    pub(crate) fn end_step_without_completion_line(&self) {
        self.step_ended_locked(CompletionLine::Suppress);
    }

    pub(super) fn step_ended_locked(&self, completion_line: CompletionLine) {
        let mut state = self.inner.state.lock().unwrap_or_else(|error| {
            self.inner.state.clear_poison();
            error.into_inner()
        });
        ReporterInner::erase_live_line(&mut state);
        let Some(step) = state.active.take() else {
            return;
        };
        if step.announced && completion_line == CompletionLine::Write {
            let line = format!("{}\n", ReporterInner::completion_line(&step));
            state.write_line(&line);
            state.last_line_at = Some(Instant::now());
        }
    }
}

impl ProgressObserver for Reporter {
    fn step_began(&mut self, step: &Step<'_>) {
        // A step beginning while another is active ends it first: the
        // trait promises callers never have to pair the edges themselves.
        self.step_ended_locked(CompletionLine::Write);
        if self.inner.style == Style::Silent {
            return;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|error| {
            self.inner.state.clear_poison();
            error.into_inner()
        });
        state.first_step_at.get_or_insert_with(Instant::now);
        state.active = Some(ActiveStep {
            description: sanitize_terminal_text(step.description()),
            unit: step.unit(),
            total: step.total(),
            completed: 0,
            started: Instant::now(),
            last_report: None,
            announced: false,
            conclusion: None,
        });
        self.inner.render(&mut state);
    }

    fn step_advanced(&mut self, completed: u64) {
        if self.inner.style == Style::Silent {
            return;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|error| {
            self.inner.state.clear_poison();
            error.into_inner()
        });
        if let Some(active) = state.active.as_mut() {
            // Counts are cumulative and never decrease; a caller that
            // reports out of order must not make the bar run backwards.
            active.completed = active.completed.max(completed);
        }
        self.inner.render(&mut state);
    }

    fn step_concluded(&mut self, conclusion: &str) {
        if self.inner.style == Style::Silent {
            return;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|error| {
            self.inner.state.clear_poison();
            error.into_inner()
        });
        if let Some(active) = state.active.as_mut() {
            active.conclusion = Some(sanitize_terminal_text(conclusion));
            // A concluded step is worth its completion line even when it
            // finished inside the announcement delay: the conclusion is a
            // result, not a heartbeat.
            active.announced = true;
        }
    }

    fn step_total_resolved(&mut self, total: u64) {
        if self.inner.style == Style::Silent {
            return;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|error| {
            self.inner.state.clear_poison();
            error.into_inner()
        });
        if let Some(active) = state.active.as_mut() {
            active.total = Some(total);
            // A total appearing changes the report from a bare count to a
            // bar; that is worth showing at once rather than at the end of
            // the current redraw interval.
            active.last_report = None;
        }
    }

    fn step_ended(&mut self) {
        self.step_ended_locked(CompletionLine::Write);
    }
}

#[cfg(test)]
mod tests {
    use super::Style;
    use crate::progress::test_support::{SharedBuffer, reporter};
    use froe::progress::{ProgressObserver, Step, WorkUnit};
    use std::rc::Rc;

    #[test]
    fn a_silent_reporter_writes_nothing_at_all() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Silent, &captured);
        reporter.status("opening the repository");
        reporter.step_began(&Step::new("scanning archives", WorkUnit::Archives).with_total(4));
        reporter.step_advanced(2);
        reporter.step_ended();
        reporter.finish();
        assert_eq!(captured.text(), "");
    }

    #[test]
    fn a_plain_step_reports_its_start_and_its_completion() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_began(&Step::new("scanning archives", WorkUnit::Archives).with_total(4));
        reporter.step_advanced(4);
        reporter.step_ended();
        let printed = captured.text();
        assert!(
            printed.contains("froe: scanning archives"),
            "the step names itself: {printed}"
        );
        assert!(
            printed.contains("4 archives in"),
            "the completion line reports the work done: {printed}"
        );
    }

    #[test]
    fn beginning_a_step_ends_the_previous_one() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_began(&Step::new("first", WorkUnit::Files).with_total(1));
        reporter.step_advanced(1);
        reporter.step_began(&Step::new("second", WorkUnit::Files).with_total(1));
        reporter.step_ended();
        let printed = captured.text();
        assert!(
            printed.contains("froe: first: 1 file in"),
            "the first step completed, with an agreeing noun: {printed}"
        );
        assert!(printed.contains("froe: second"), "{printed}");
    }

    #[test]
    fn ending_without_a_step_is_ignored() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_ended();
        reporter.step_advanced(3);
        reporter.finish();
        assert_eq!(captured.text(), "");
    }

    #[test]
    fn counts_never_run_backwards() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_began(&Step::new("scanning", WorkUnit::Archives).with_total(10));
        reporter.step_advanced(6);
        reporter.step_advanced(2);
        reporter.step_ended();
        let printed = captured.text();
        assert!(
            printed.contains("6 archives in"),
            "the highest count reached is the one reported: {printed}"
        );
    }

    #[test]
    fn a_resolved_total_replaces_an_unknown_one() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_began(&Step::new("scanning", WorkUnit::Files));
        reporter.step_total_resolved(8);
        reporter.step_advanced(4);
        reporter.step_ended();
        assert!(captured.text().contains("50%"), "{}", captured.text());
    }

    #[test]
    fn a_reporter_clone_shares_one_stream() {
        let captured = SharedBuffer::new();
        let reporter = reporter(Style::Plain, &captured);
        let mut clone = reporter.clone();
        clone.step_began(&Step::new("scanning", WorkUnit::Archives).with_total(2));
        clone.step_advanced(2);
        reporter.finish();
        assert!(
            captured.text().contains("froe: scanning: 2 archives in"),
            "a clone's step is finished by the original: {}",
            captured.text()
        );
        let _unused = Rc::new(());
    }
}
