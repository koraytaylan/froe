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

/// How long a step must run before it is reported at all. Below this, a
/// command that simply did its job stays silent.
const ANNOUNCE_DELAY: Duration = Duration::from_millis(300);

/// How often the animated line is redrawn.
const ANIMATED_REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// How often a plain-style report is written.
const PLAIN_REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// How often the ticker wakes to redraw a step that is not reporting
/// counts of its own, so elapsed time keeps moving.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// The width assumed when the terminal's own width cannot be determined.
const ASSUMED_TERMINAL_WIDTH: usize = 80;

/// The narrowest bar worth drawing; below this the bar is dropped and the
/// counts alone are shown.
const MINIMUM_BAR_WIDTH: usize = 8;

/// The widest bar drawn, however wide the terminal is.
const MAXIMUM_BAR_WIDTH: usize = 32;

/// How long a step must run before its average rate means anything.
const MINIMUM_RATE_INTERVAL: Duration = Duration::from_millis(500);

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
enum Style {
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
    inner: Arc<ReporterInner>,
    /// Kept alive by every clone; dropping the last one stops the ticker.
    _ticker: Option<Arc<Ticker>>,
}

/// The reporter's shared state.
struct ReporterInner {
    style: Style,
    /// How long a step must run before it is announced.
    announce_delay: Duration,
    /// The shortest interval between two reports of the same step.
    redraw_interval: Duration,
    /// Whether the bar is drawn with block characters or ASCII.
    unicode_bar: bool,
    /// The width to lay lines out in. `None` re-queries the terminal on
    /// every render, so the live line keeps following a window the
    /// operator resizes mid-run; a fixed width is for tests, which must
    /// not read `COLUMNS` — it is process global and would race the
    /// ticker thread's own reads of it.
    width: Option<usize>,
    state: Mutex<RenderState>,
}

/// Everything the renderer mutates, behind one lock so the ticker thread
/// and the command thread can never interleave a line.
struct RenderState {
    out: Box<dyn Write + Send>,
    active: Option<ActiveStep>,
    /// The width of the live line currently on screen, so it can be erased
    /// with exactly as many spaces.
    live_line_width: usize,
    /// Set while a prompt or another writer owns the stream.
    suspended: bool,
    /// Set once the stream's reader has gone. Reporting then stops for
    /// good rather than retrying a write that can never succeed.
    stream_closed: bool,
    /// When the operation's first step began, so a long run of short
    /// steps is not silenced by a deferral that only ever measures one.
    first_step_at: Option<Instant>,
    /// When a whole line was last written, throttling the plain style
    /// across steps as well as within one.
    last_line_at: Option<Instant>,
}

impl RenderState {
    /// The reporter's only way onto the stream.
    ///
    /// Writing progress must never change what a command does. Standard
    /// error is a pipe often enough — `froe cleanup --yes 2>&1 | less`,
    /// quit early — and `main` restores SIGPIPE to its terminating
    /// disposition so that piping *data* into `head` ends quietly. Those
    /// two facts together once let a progress line kill a destructive
    /// cleanup between its mutations. So the reporter's writes, and only
    /// the reporter's writes, run with SIGPIPE blocked: a closed stream
    /// returns `EPIPE` here instead of felling the process, and the
    /// reporter falls silent for the rest of the run.
    fn write_line(&mut self, text: &str) {
        if self.stream_closed {
            return;
        }
        let written = without_sigpipe(|| {
            self.out
                .write_all(text.as_bytes())
                .and_then(|()| self.out.flush())
        });
        if let Err(error) = written
            && error.kind() == std::io::ErrorKind::BrokenPipe
        {
            self.stream_closed = true;
        }
    }
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
fn without_sigpipe<Value>(work: impl FnOnce() -> Value) -> Value {
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
fn without_sigpipe_masked<Value>(work: impl FnOnce() -> Value) -> Value {
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
fn without_sigpipe<Value>(work: impl FnOnce() -> Value) -> Value {
    work()
}

/// The step being reported.
struct ActiveStep {
    description: String,
    unit: WorkUnit,
    total: Option<u64>,
    completed: u64,
    started: Instant,
    /// When the last report was written, or `None` before the first.
    last_report: Option<Instant>,
    /// Whether the step has been mentioned on the stream at all. A step
    /// that was never announced prints no completion line either.
    announced: bool,
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
    fn with_output(
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
        self.step_ended_locked(true);
    }

    /// Ends the active step and erases its line without the completion
    /// line. For a step whose caller reports the outcome itself — an
    /// export names its destination, which the step cannot — the
    /// completion line would only say the same thing twice.
    pub(crate) fn end_step_without_completion_line(&self) {
        self.step_ended_locked(false);
    }

    fn step_ended_locked(&self, report_completion: bool) {
        let mut state = self.inner.state.lock().unwrap_or_else(|error| {
            self.inner.state.clear_poison();
            error.into_inner()
        });
        ReporterInner::erase_live_line(&mut state);
        let Some(step) = state.active.take() else {
            return;
        };
        if step.announced && report_completion {
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
        self.step_ended_locked(true);
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
        self.step_ended_locked(true);
    }
}

impl ReporterInner {
    /// Draws the active step if one is due. Every path into the stream
    /// that is not a completion line goes through here.
    fn render(&self, state: &mut RenderState) {
        if self.style == Style::Silent || state.suspended {
            return;
        }
        let now = Instant::now();
        let first_step_at = state.first_step_at;
        let last_line_at = state.last_line_at;
        let Some(active) = state.active.as_ref() else {
            return;
        };
        // The deferral measures the *operation*, not one step. A command
        // that opens a step per item — `check` opens one per revision —
        // would otherwise restart the delay on every one and stay silent
        // however long the whole run took.
        let waiting = first_step_at.map_or_else(
            || now.duration_since(active.started),
            |began| now.duration_since(began),
        );
        if waiting < self.announce_delay {
            return;
        }
        // The first report of an announced step is always written; after
        // that the style's interval throttles the redraw.
        if let Some(last_report) = active.last_report
            && now.duration_since(last_report) < self.redraw_interval
        {
            return;
        }
        // A plain log is throttled across steps as well as within one, or
        // a command that opens a step per item would contribute a line
        // each. `--progress always` is exempt: it exists to report every
        // step from the moment it begins, for logs and for tests.
        if self.style == Style::Plain
            && !self.announce_delay.is_zero()
            && !active.announced
            && let Some(last_line_at) = last_line_at
            && now.duration_since(last_line_at) < self.redraw_interval
        {
            return;
        }
        let line = self.progress_line(active, now.duration_since(active.started));
        match self.style {
            Style::Animated => {
                let padding = state.live_line_width.saturating_sub(display_width(&line));
                let rendered = format!("\r{line}{:padding$}\r", "");
                state.write_line(&rendered);
                state.live_line_width = display_width(&line);
            }
            Style::Plain => {
                state.write_line(&format!("{line}\n"));
                state.last_line_at = Some(now);
            }
            Style::Silent => return,
        }
        if let Some(active) = state.active.as_mut() {
            active.last_report = Some(now);
            active.announced = true;
        }
    }

    /// Erases the live line, leaving the cursor at the start of it.
    fn erase_live_line(state: &mut RenderState) {
        if state.live_line_width == 0 {
            return;
        }
        let width = state.live_line_width;
        state.write_line(&format!("\r{:width$}\r", ""));
        state.live_line_width = 0;
    }

    /// The live report for a step: `froe: <what> [bar] 48% 23/48 0:12 eta
    /// 0:13`, trimmed to the terminal from the right-hand side inward.
    fn progress_line(&self, step: &ActiveStep, elapsed: Duration) -> String {
        // Re-queried per render unless a test fixed it: an operator who
        // resizes the window mid-run must not leave the live line laid
        // out for the old width, which would wrap and defeat both the
        // in-place rewrite and the erase before a confirmation prompt.
        let width = self.width.unwrap_or_else(terminal_width);
        let mut line = format!("froe: {}", step.description);
        let counts = match step.total {
            Some(total) if total != 0 => format!(
                " {:>3}% {}/{}",
                percentage(step.completed, total),
                format_count(step.completed.min(total)),
                format_count(total)
            ),
            _ if step.completed != 0 => format!(
                " {} {}",
                format_count(step.completed),
                step.unit.noun_for(step.completed)
            ),
            _ => String::new(),
        };
        let timing = match step.total {
            Some(total) if total != 0 && step.completed != 0 && step.completed < total => format!(
                " {} eta {}",
                format_clock(elapsed),
                format_clock(estimate_remaining(elapsed, step.completed, total))
            ),
            _ => format!(" {}", format_clock(elapsed)),
        };
        // The bar takes what is left after the text that carries the
        // numbers; a terminal too narrow for a usable bar shows none.
        if let Some(total) = step.total
            && total != 0
            && self.style == Style::Animated
        {
            let fixed = display_width(&line) + display_width(&counts) + display_width(&timing);
            let available = width.saturating_sub(fixed + 3);
            let width = available.min(MAXIMUM_BAR_WIDTH);
            if width >= MINIMUM_BAR_WIDTH {
                line.push(' ');
                line.push_str(&self.bar(step.completed, total, width));
            }
        }
        line.push_str(&counts);
        line.push_str(&timing);
        truncate_to_width(&line, width)
    }

    /// The completion line a reported step leaves behind. A step that
    /// stopped early — a search that reached its limit — reports what it
    /// did, never the total it was planning for.
    fn completion_line(step: &ActiveStep) -> String {
        let elapsed = step.started.elapsed();
        // A step that counted nothing is not a step that did nothing: a
        // phase without a counter of its own reports the time it took
        // rather than an untrue "0 archives".
        if step.completed == 0 {
            return format!(
                "froe: {}: done in {}",
                step.description,
                format_duration(elapsed)
            );
        }
        let rate = rate_per_second(step.completed, elapsed);
        let mut line = format!(
            "froe: {}: {} {} in {}",
            step.description,
            format_count(step.completed),
            step.unit.noun_for(step.completed),
            format_duration(elapsed)
        );
        if let Some(rate) = rate {
            let _ = write!(
                line,
                " ({} {}/s)",
                format_count(rate),
                step.unit.plural_noun()
            );
        }
        line
    }

    /// A `[####----]` bar, in block characters where the terminal is known
    /// to speak UTF-8 and in ASCII everywhere else.
    fn bar(&self, completed: u64, total: u64, width: usize) -> String {
        let inner_width = width.saturating_sub(2);
        let filled = filled_cells(completed, total, inner_width);
        let (full, empty) = if self.unicode_bar {
            ('\u{2588}', '\u{2591}')
        } else {
            ('#', '-')
        };
        let mut bar = String::with_capacity(width);
        bar.push('[');
        for cell in 0..inner_width {
            bar.push(if cell < filled { full } else { empty });
        }
        bar.push(']');
        bar
    }
}

/// Redraws the animated line while a step reports nothing of its own, so
/// a long silent phase still shows time passing rather than a frozen
/// terminal.
struct Ticker {
    /// Dropping the sender wakes the thread's `recv_timeout` with a
    /// disconnect, which is how it learns to stop.
    _stop: mpsc::Sender<()>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Ticker {
    fn start(inner: &Arc<ReporterInner>) -> Self {
        let (sender, receiver) = mpsc::channel::<()>();
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_inner = Arc::clone(inner);
        let thread_stopped = Arc::clone(&stopped);
        let thread = std::thread::Builder::new()
            .name("froe-progress".to_owned())
            .spawn(move || {
                while !thread_stopped.load(Ordering::Relaxed) {
                    if receiver.recv_timeout(TICK_INTERVAL).is_ok() {
                        continue;
                    }
                    if thread_stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(mut state) = thread_inner.state.lock() else {
                        break;
                    };
                    if state.active.is_some() {
                        thread_inner.render(&mut state);
                    }
                }
            })
            .ok();
        Self {
            _stop: sender,
            stopped,
            thread,
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // The thread wakes at most one tick later; joining keeps it
            // from drawing over whatever the process prints on its way
            // out.
            let _ = thread.join();
        }
    }
}

/// How many cells of `inner_width` are filled at `completed`/`total`.
fn filled_cells(completed: u64, total: u64, inner_width: usize) -> usize {
    if total == 0 || inner_width == 0 {
        return 0;
    }
    let completed = u128::from(completed.min(total));
    let cells = completed * (inner_width as u128) / u128::from(total);
    usize::try_from(cells)
        .unwrap_or(inner_width)
        .min(inner_width)
}

/// `completed` as a whole percentage of `total`, capped at 100.
fn percentage(completed: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let percentage = u128::from(completed.min(total)) * 100 / u128::from(total);
    u64::try_from(percentage).unwrap_or(100).min(100)
}

/// How much longer the step is expected to run, extrapolating the rate so
/// far. This is an estimate from an average, not a promise.
fn estimate_remaining(elapsed: Duration, completed: u64, total: u64) -> Duration {
    if completed == 0 {
        return Duration::ZERO;
    }
    let remaining = u128::from(total.saturating_sub(completed));
    let nanoseconds = elapsed.as_nanos().saturating_mul(remaining) / u128::from(completed);
    u64::try_from(nanoseconds).map_or(Duration::MAX, Duration::from_nanos)
}

/// Items per second, or `None` when the step was too brief to have a
/// meaningful rate. A handful of items in a few milliseconds extrapolates
/// to a number that says nothing about throughput, so it is not reported
/// at all rather than reported misleadingly.
fn rate_per_second(completed: u64, elapsed: Duration) -> Option<u64> {
    if completed < 2 || elapsed < MINIMUM_RATE_INTERVAL {
        return None;
    }
    let nanoseconds = elapsed.as_nanos();
    if nanoseconds == 0 {
        return None;
    }
    let rate = u128::from(completed) * 1_000_000_000 / nanoseconds;
    u64::try_from(rate).ok()
}

/// A parenthesised rate suffix — `" (73,486 nodes/s)"` — or the empty
/// string when the run was too brief for the average to mean anything.
/// The caller concatenates it, so a summary carries a rate exactly when a
/// step's completion line would.
pub(crate) fn format_rate(completed: u64, elapsed: Duration) -> String {
    match rate_per_second(completed, elapsed) {
        Some(rate) => format!(" ({} nodes/s)", format_count(rate)),
        None => String::new(),
    }
}

/// A count with thousands separators: `1234567` becomes `1,234,567`.
pub(crate) fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position != 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A running clock: `0:07`, `4:31`, `1:04:31`.
fn format_clock(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

/// A finished duration, for a completion line: `0.4s`, `12.4s`, `4m 31s`,
/// `1h 04m`.
pub(crate) fn format_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return format!("{:.1}s", elapsed.as_secs_f64());
    }
    if seconds < 3600 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!("{}h {:02}m", seconds / 3600, (seconds / 60) % 60)
}

/// The width of `text` in terminal cells, counted as characters. Every
/// character the reporter renders is a single-cell one: descriptions are
/// sanitized, and the only non-ASCII characters are the bar's blocks.
fn display_width(text: &str) -> usize {
    text.chars().count()
}

/// Truncates to `width` cells, marking the cut with an ellipsis so a
/// clipped line does not read as a complete one.
fn truncate_to_width(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return text.chars().take(width).collect();
    }
    let mut truncated: String = text.chars().take(width - 1).collect();
    truncated.push('\u{2026}');
    truncated
}

/// The terminal's width in columns, or [`ASSUMED_TERMINAL_WIDTH`].
fn terminal_width() -> usize {
    if let Ok(columns) = std::env::var("COLUMNS")
        && let Ok(columns) = columns.parse::<usize>()
        && columns > 1
    {
        return columns;
    }
    #[cfg(unix)]
    {
        if let Some(columns) = terminal_width_from_ioctl() {
            return columns;
        }
    }
    ASSUMED_TERMINAL_WIDTH
}

/// The terminal width standard error is attached to, from the kernel.
#[cfg(unix)]
fn terminal_width_from_ioctl() -> Option<usize> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes exactly one `winsize` through the pointer
    // and reads nothing else; `size` is a live, fully initialized local of
    // that type. The call only queries the terminal attached to standard
    // error and never changes it.
    let queried = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &raw mut size) };
    if queried != 0 || size.ws_col == 0 {
        return None;
    }
    Some(usize::from(size.ws_col))
}

/// Whether the block-drawing bar is safe to emit. A terminal whose locale
/// does not declare UTF-8 gets the ASCII bar rather than mojibake.
fn supports_unicode_bar() -> bool {
    #[cfg(unix)]
    {
        for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
            let Ok(value) = std::env::var(name) else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            let value = value.to_ascii_uppercase();
            return value.contains("UTF-8") || value.contains("UTF8");
        }
        false
    }
    #[cfg(not(unix))]
    {
        // Rust writes to a Windows console through `WriteConsoleW`, so the
        // console's code page cannot mangle these characters.
        true
    }
}

/// Wraps an export sink and advances the reporter's step per written node.
pub(crate) struct ProgressSink<Sink> {
    inner: Sink,
    written: u64,
    reporter: Reporter,
    started: Instant,
}

impl<Sink> ProgressSink<Sink> {
    /// Wraps `inner`, reporting to `reporter`. The caller has already
    /// begun the step this sink advances.
    pub(crate) fn new(inner: Sink, reporter: Reporter) -> Self {
        Self {
            inner,
            written: 0,
            reporter,
            started: Instant::now(),
        }
    }

    /// The number of nodes written so far.
    #[cfg(test)]
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// The elapsed time since the export started.
    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

impl<Sink: froe_export::ExportSink> froe_export::ExportSink for ProgressSink<Sink> {
    fn write_node(&mut self, node: &froe_export::ExportedNode<'_>) -> froe::Result<()> {
        self.inner.write_node(node)?;
        self.written += 1;
        self.reporter.step_advanced(self.written);
        Ok(())
    }

    fn finish(&mut self) -> froe::Result<()> {
        self.inner.finish()?;
        self.reporter.step_advanced(self.written);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use froe::progress::{ProgressObserver, Step, WorkUnit};
    use froe_export::{ExportSink, ExportedNode};

    use super::{
        ANNOUNCE_DELAY, ProgressSink, Reporter, Style, estimate_remaining, filled_cells,
        format_clock, format_count, format_duration, percentage, rate_per_second,
        truncate_to_width,
    };

    /// A `Write` the test can read back from another thread.
    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("captured output")).into_owned()
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("captured output")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A reporter with no deferral and no throttle, laid out in a fixed
    /// 100 columns. The width is a parameter rather than `COLUMNS`
    /// because libtest runs these on several threads while the ticker
    /// thread reads the environment, and `set_var` alongside a
    /// concurrent `getenv` is undefined behaviour.
    fn reporter(style: Style, captured: &SharedBuffer) -> Reporter {
        Reporter::with_output(
            style,
            Duration::ZERO,
            Duration::ZERO,
            Some(100),
            Box::new(captured.clone()),
        )
    }

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
    fn a_step_below_the_announce_delay_reports_nothing() {
        let captured = SharedBuffer::new();
        let mut reporter = Reporter::with_output(
            Style::Plain,
            ANNOUNCE_DELAY,
            Duration::ZERO,
            Some(100),
            Box::new(captured.clone()),
        );
        reporter.step_began(&Step::new("scanning archives", WorkUnit::Archives).with_total(4));
        reporter.step_advanced(4);
        reporter.step_ended();
        assert_eq!(
            captured.text(),
            "",
            "a step that finished at once must stay quiet"
        );
    }

    #[test]
    fn the_animated_line_carries_a_bar_and_is_erased_on_completion() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Animated, &captured);
        reporter.step_began(&Step::new("scanning archives", WorkUnit::Archives).with_total(4));
        reporter.step_advanced(2);
        reporter.step_ended();
        let printed = captured.text();
        assert!(printed.contains('['), "the bar is drawn: {printed}");
        assert!(
            printed.contains("50%"),
            "the percentage is shown: {printed}"
        );
        assert!(printed.contains('\r'), "the line is rewritten: {printed}");
        // The live line is rewritten in place, so what a terminal finally
        // shows is the text after the last carriage return.
        let visible = printed.rsplit('\r').next().unwrap_or_default();
        assert!(
            visible.contains("froe: scanning archives: 2 archives in"),
            "the completion line is what remains on screen: {printed}"
        );
        assert!(
            !visible.contains('['),
            "the live line was not erased before the completion line: {printed}"
        );
    }

    #[test]
    fn no_report_ever_contains_an_escape_sequence() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Animated, &captured);
        reporter.status("opening \u{1b}]8;;evil\u{7} and \u{202e}reversed");
        reporter.step_began(&Step::new("scanning \u{1b}[2Jarchives", WorkUnit::Archives));
        reporter.step_advanced(1);
        reporter.step_ended();
        let printed = captured.text();
        assert!(!printed.contains('\u{1b}'), "raw escape reached the stream");
        assert!(
            !printed.contains('\u{202e}'),
            "raw bidi control reached the stream"
        );
        assert!(
            printed.contains(r"\u{1b}"),
            "the escape is shown: {printed}"
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

    /// A phase with no counter of its own — compaction's reclamation
    /// sweep, the checkpoint removal — must still prove it is running
    /// when standard error is a log rather than a terminal. Without the
    /// ticker in the plain style, such a step renders never, so it
    /// neither announces itself nor prints a completion line, and a
    /// ten-minute phase leaves no trace at all.
    #[test]
    fn a_plain_step_that_never_advances_still_announces_and_completes() {
        let captured = SharedBuffer::new();
        let mut reporter = Reporter::with_output(
            Style::Plain,
            Duration::from_millis(50),
            Duration::from_millis(50),
            Some(100),
            Box::new(captured.clone()),
        );
        reporter.step_began(&Step::new("reclaiming old generations", WorkUnit::Archives));
        // No advance of any kind: only the ticker can render this.
        std::thread::sleep(Duration::from_millis(300));
        let while_running = captured.text();
        reporter.step_ended();
        let printed = captured.text();

        assert!(
            while_running.contains("froe: reclaiming old generations"),
            "a long uncounted step must announce itself while it runs: {while_running:?}"
        );
        assert!(
            printed.contains("froe: reclaiming old generations: done in"),
            "and must print a completion line: {printed:?}"
        );
    }

    /// A command that opens a step per item — `check` opens one per
    /// revision — must neither be silenced by a deferral that restarts on
    /// every step, nor spam a log with one line each. The deferral
    /// measures the whole operation; the plain style then throttles new
    /// steps across the run as well as within one.
    #[test]
    fn many_short_steps_are_neither_silenced_nor_spammed() {
        const STEPS: u64 = 200;
        const REDRAW: Duration = Duration::from_millis(200);
        let captured = SharedBuffer::new();
        let mut reporter = Reporter::with_output(
            Style::Plain,
            Duration::from_millis(50),
            REDRAW,
            Some(100),
            Box::new(captured.clone()),
        );
        // Two hundred steps, each far shorter than the deferral, spanning
        // well past it in total.
        let started = Instant::now();
        for index in 0..STEPS {
            reporter.step_began(&Step::new("checking revision", WorkUnit::Nodes));
            reporter.step_advanced(index);
            std::thread::sleep(Duration::from_millis(3));
            reporter.step_ended();
        }
        let elapsed = started.elapsed();
        let lines = captured.text().lines().count();
        assert!(
            lines > 0,
            "a run of short steps lasting well past the deferral must report something"
        );
        // The ceiling follows from how long the run actually took, not from
        // how many steps it ran: a line is written only once `REDRAW` has
        // passed since the last one, and each one an announced step writes is
        // followed by that step's completion line. Asserting a fixed count
        // instead would be asserting how fast the host runs 200 sleeps —
        // `thread::sleep` overshoots badly on a loaded CI runner, which is
        // not a fact about the throttle.
        let windows = elapsed.as_millis() / REDRAW.as_millis() + 1;
        let ceiling = usize::try_from(windows * 2 + 4).expect("a small ceiling");
        assert!(
            lines <= ceiling,
            "{STEPS} short steps must be throttled by time, not contribute a line each; \
             got {lines} over {elapsed:?}, which allows at most {ceiling}"
        );
    }

    #[test]
    fn a_step_that_counted_nothing_reports_its_time_not_a_zero() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_began(&Step::new("reclaiming old generations", WorkUnit::Archives));
        reporter.step_ended();
        let printed = captured.text();
        assert!(
            printed.contains("froe: reclaiming old generations: done in"),
            "a phase without a counter reports its duration: {printed}"
        );
        assert!(
            !printed.contains("0 archives"),
            "an uncounted phase must not claim it did nothing: {printed}"
        );
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
    fn a_suspended_reporter_draws_nothing_while_the_stream_is_borrowed() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_began(&Step::new("scanning", WorkUnit::Archives).with_total(4));
        let before = captured.text().len();
        reporter.while_suspended(|| {
            // Nothing the reporter does may reach the stream here; a
            // prompt owns it.
        });
        let mut borrowed = reporter.clone();
        reporter.while_suspended(|| {
            borrowed.step_advanced(2);
        });
        assert_eq!(
            captured.text().len(),
            before,
            "a suspended reporter wrote to the stream: {}",
            captured.text()
        );
    }

    #[test]
    fn suspending_erases_the_live_line_before_the_borrower_writes() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Animated, &captured);
        reporter.step_began(&Step::new("scanning archives", WorkUnit::Archives).with_total(4));
        reporter.step_advanced(2);
        assert!(
            captured.text().contains("50%"),
            "the live line is on screen before suspending: {}",
            captured.text()
        );
        reporter.while_suspended(|| {});
        let visible = captured.text();
        let visible = visible.rsplit('\r').next().unwrap_or_default();
        assert!(
            visible.trim().is_empty(),
            "a prompt must find the line erased, not a bar under it: {visible:?}"
        );
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
    fn percentages_and_bars_stay_within_their_bounds() {
        assert_eq!(percentage(0, 10), 0);
        assert_eq!(percentage(5, 10), 50);
        assert_eq!(percentage(10, 10), 100);
        assert_eq!(percentage(99, 0), 0, "a zero total is not a division");
        assert_eq!(
            percentage(u64::MAX, 3),
            100,
            "an overshooting count is capped, never wrapped"
        );
        assert_eq!(filled_cells(0, 10, 8), 0);
        assert_eq!(filled_cells(10, 10, 8), 8);
        assert_eq!(filled_cells(u64::MAX, 10, 8), 8);
        assert_eq!(filled_cells(5, 0, 8), 0);
        assert_eq!(filled_cells(u64::MAX, u64::MAX, 8), 8);
    }

    #[test]
    fn estimates_and_rates_survive_extreme_inputs() {
        assert_eq!(
            estimate_remaining(Duration::from_secs(1), 0, 10),
            Duration::ZERO
        );
        assert_eq!(
            estimate_remaining(Duration::from_secs(10), 5, 10),
            Duration::from_secs(10)
        );
        assert_eq!(
            estimate_remaining(Duration::from_secs(10), 10, 10),
            Duration::ZERO
        );
        assert_eq!(rate_per_second(0, Duration::from_secs(1)), None);
        assert_eq!(rate_per_second(10, Duration::ZERO), None);
        assert_eq!(rate_per_second(10, Duration::from_secs(2)), Some(5));
        assert_eq!(
            rate_per_second(1, Duration::from_secs(2)),
            None,
            "one item is not a throughput measurement"
        );
        assert_eq!(
            rate_per_second(4, Duration::from_millis(1)),
            None,
            "a millisecond is too short to extrapolate a rate from"
        );
    }

    #[test]
    fn counts_clocks_and_durations_are_formatted_for_reading() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_clock(Duration::from_secs(7)), "0:07");
        assert_eq!(format_clock(Duration::from_secs(271)), "4:31");
        assert_eq!(format_clock(Duration::from_secs(3871)), "1:04:31");
        assert_eq!(format_duration(Duration::from_millis(412)), "0.4s");
        assert_eq!(format_duration(Duration::from_secs(271)), "4m 31s");
        assert_eq!(format_duration(Duration::from_secs(3871)), "1h 04m");
    }

    #[test]
    fn long_lines_are_truncated_with_an_ellipsis() {
        assert_eq!(truncate_to_width("froe: short", 40), "froe: short");
        assert_eq!(
            truncate_to_width("froe: quite long", 10),
            "froe: qui\u{2026}"
        );
        assert_eq!(truncate_to_width("froe", 1), "f");
        assert_eq!(truncate_to_width("froe", 0), "");
    }

    /// A sink that records every node and finish call.
    struct RecordingSink {
        nodes: RefCell<Vec<String>>,
        finished: RefCell<bool>,
    }

    impl ExportSink for RecordingSink {
        fn write_node(&mut self, node: &ExportedNode<'_>) -> froe::Result<()> {
            self.nodes.borrow_mut().push(node.path.to_owned());
            Ok(())
        }

        fn finish(&mut self) -> froe::Result<()> {
            *self.finished.borrow_mut() = true;
            Ok(())
        }
    }

    fn node(path: &str) -> ExportedNode<'_> {
        ExportedNode {
            path,
            depth: 0,
            properties: &[],
        }
    }

    #[test]
    fn nodes_reach_the_inner_sink_and_the_reporter() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_began(&Step::new("exporting nodes", WorkUnit::Nodes));
        let inner = RecordingSink {
            nodes: RefCell::new(Vec::new()),
            finished: RefCell::new(false),
        };
        let mut sink = ProgressSink::new(inner, reporter.clone());
        sink.write_node(&node("/a")).expect("write");
        sink.write_node(&node("/b")).expect("write");
        sink.finish().expect("finish");
        assert_eq!(sink.written(), 2);
        assert_eq!(
            *sink.inner.nodes.borrow(),
            vec!["/a".to_owned(), "/b".to_owned()]
        );
        assert!(*sink.inner.finished.borrow());
        reporter.step_ended();
        assert!(
            captured.text().contains("2 nodes in"),
            "{}",
            captured.text()
        );
    }

    #[test]
    fn a_silent_export_reports_nothing_but_still_writes_every_node() {
        let reporter = Reporter::silent();
        let inner = RecordingSink {
            nodes: RefCell::new(Vec::new()),
            finished: RefCell::new(false),
        };
        let mut sink = ProgressSink::new(inner, reporter);
        sink.write_node(&node("/a")).expect("write");
        sink.finish().expect("finish");
        assert_eq!(sink.written(), 1);
        assert_eq!(*sink.inner.nodes.borrow(), vec!["/a".to_owned()]);
    }

    #[test]
    fn the_export_sink_measures_its_own_elapsed_time() {
        let mut sink = ProgressSink::new(
            RecordingSink {
                nodes: RefCell::new(Vec::new()),
                finished: RefCell::new(false),
            },
            Reporter::silent(),
        );
        std::thread::sleep(Duration::from_millis(5));
        sink.write_node(&node("/a")).expect("write");
        assert!(sink.elapsed() >= Duration::from_millis(5));
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
