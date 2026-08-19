//! The line currently on screen: when a step is announced, how often it
//! is redrawn, and the erase that must precede anything else writing to
//! the same stream.

use super::*;

/// How long a step must run before it is reported at all. Below this, a
/// command that simply did its job stays silent.
pub(crate) const ANNOUNCE_DELAY: Duration = Duration::from_millis(300);

/// How often the animated line is redrawn.
pub(crate) const ANIMATED_REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// How often a plain-style report is written.
pub(crate) const PLAIN_REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// How often the ticker wakes to redraw a step that is not reporting
/// counts of its own, so elapsed time keeps moving.
pub(crate) const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// The step being reported.
pub(crate) struct ActiveStep {
    pub(crate) description: String,
    pub(crate) unit: WorkUnit,
    pub(crate) total: Option<u64>,
    pub(crate) completed: u64,
    pub(crate) started: Instant,
    /// When the last report was written, or `None` before the first.
    pub(crate) last_report: Option<Instant>,
    /// Whether the step has been mentioned on the stream at all. A step
    /// that was never announced prints no completion line either.
    pub(crate) announced: bool,
    /// The step's reported conclusion, appended to the completion line.
    pub(crate) conclusion: Option<String>,
}

/// The reporter's shared state.
pub(crate) struct ReporterInner {
    pub(crate) style: Style,
    /// How long a step must run before it is announced.
    pub(crate) announce_delay: Duration,
    /// The shortest interval between two reports of the same step.
    pub(crate) redraw_interval: Duration,
    /// Whether the bar is drawn with block characters or ASCII.
    pub(crate) unicode_bar: bool,
    /// The width to lay lines out in. `None` re-queries the terminal on
    /// every render, so the live line keeps following a window the
    /// operator resizes mid-run; a fixed width is for tests, which must
    /// not read `COLUMNS` — it is process global and would race the
    /// ticker thread's own reads of it.
    pub(crate) width: Option<usize>,
    pub(crate) state: Mutex<RenderState>,
}

impl ReporterInner {
    /// Draws the active step if one is due. Every path into the stream
    /// that is not a completion line goes through here.
    pub(crate) fn render(&self, state: &mut RenderState) {
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
    pub(crate) fn erase_live_line(state: &mut RenderState) {
        if state.live_line_width == 0 {
            return;
        }
        let width = state.live_line_width;
        state.write_line(&format!("\r{:width$}\r", ""));
        state.live_line_width = 0;
    }

    /// The live report for a step: `froe: <what> [bar] 48% 23/48 0:12 eta
    /// 0:13`, trimmed to the terminal from the right-hand side inward.
    pub(crate) fn progress_line(&self, step: &ActiveStep, elapsed: Duration) -> String {
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
    pub(crate) fn completion_line(step: &ActiveStep) -> String {
        let elapsed = step.started.elapsed();
        // A step that counted nothing is not a step that did nothing: a
        // phase without a counter of its own reports the time it took
        // rather than an untrue "0 archives". Its conclusion still
        // matters — a prediction that advances no counter concludes all
        // the same.
        if step.completed == 0 {
            let mut line = format!(
                "froe: {}: done in {}",
                step.description,
                format_duration(elapsed)
            );
            if let Some(conclusion) = &step.conclusion {
                let _ = write!(line, "; {conclusion}");
            }
            return line;
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
        if let Some(conclusion) = &step.conclusion {
            let _ = write!(line, "; {conclusion}");
        }
        line
    }

    /// A `[####----]` bar, in block characters where the terminal is known
    /// to speak UTF-8 and in ASCII everywhere else.
    pub(crate) fn bar(&self, completed: u64, total: u64, width: usize) -> String {
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
pub(crate) struct Ticker {
    /// Dropping the sender wakes the thread's `recv_timeout` with a
    /// disconnect, which is how it learns to stop.
    pub(crate) _stop: mpsc::Sender<()>,
    pub(crate) stopped: Arc<AtomicBool>,
    pub(crate) thread: Option<std::thread::JoinHandle<()>>,
}

impl Ticker {
    pub(crate) fn start(inner: &Arc<ReporterInner>) -> Self {
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

#[cfg(test)]
mod tests {
    use super::ANNOUNCE_DELAY;
    use crate::progress::test_support::{SharedBuffer, reporter};
    use crate::progress::{Reporter, Style};
    use froe::progress::{ProgressObserver, Step, WorkUnit};
    use std::time::{Duration, Instant};

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

    /// A conclusion is appended to the completion line after a semicolon,
    /// for a counted and an uncounted step alike — the uncounted case is
    /// the reclamation prediction, whose completion line is otherwise a
    /// bare duration.
    #[test]
    fn a_concluded_step_appends_its_conclusion_to_the_completion_line() {
        let captured = SharedBuffer::new();
        let mut reporter = reporter(Style::Plain, &captured);
        reporter.step_began(&Step::new("scanning archives", WorkUnit::Archives));
        reporter.step_advanced(2);
        reporter.step_concluded("2 archives hold nothing reclaimable");
        reporter.step_ended();
        reporter.step_began(&Step::new("predicting the reclamation", WorkUnit::Archives));
        reporter.step_concluded("the sweep removes 1 archive (1.0 MiB)");
        reporter.step_ended();
        let printed = captured.text();
        assert!(
            printed.contains("in 0.0s; 2 archives hold nothing reclaimable"),
            "a counted step's conclusion follows its counts: {printed}"
        );
        assert!(
            printed.contains("done in 0.0s; the sweep removes 1 archive (1.0 MiB)"),
            "an uncounted step's conclusion follows its duration: {printed}"
        );
    }

    /// A conclusion is a result, so it must survive the announcement
    /// deferral that silences prompt steps: the operator asked what the
    /// prediction found, not how long it took to find it.
    #[test]
    fn a_conclusion_forces_the_completion_line_of_a_prompt_step() {
        let captured = SharedBuffer::new();
        let mut reporter = Reporter::with_output(
            Style::Plain,
            ANNOUNCE_DELAY,
            Duration::ZERO,
            Some(100),
            Box::new(captured.clone()),
        );
        reporter.step_began(&Step::new("predicting the reclamation", WorkUnit::Archives));
        reporter.step_concluded("the sweep has nothing to reclaim");
        reporter.step_ended();
        let printed = captured.text();
        assert!(
            printed.contains("froe: predicting the reclamation: done in"),
            "the concluded step left no completion line: {printed}"
        );
        assert!(
            printed.contains("; the sweep has nothing to reclaim"),
            "the conclusion was dropped: {printed}"
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
}
