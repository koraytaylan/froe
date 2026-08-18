//! Turning a step's numbers into one line: the bar, the percentage, the
//! rate, and the counts, clocks, and durations they are spelled with.

use super::{ActiveStep, Duration, Instant, Write, without_sigpipe};

/// The width assumed when the terminal's own width cannot be determined.
pub(crate) const ASSUMED_TERMINAL_WIDTH: usize = 80;

/// The narrowest bar worth drawing; below this the bar is dropped and the
/// counts alone are shown.
pub(crate) const MINIMUM_BAR_WIDTH: usize = 8;

/// The widest bar drawn, however wide the terminal is.
pub(crate) const MAXIMUM_BAR_WIDTH: usize = 32;

/// How long a step must run before its average rate means anything.
pub(crate) const MINIMUM_RATE_INTERVAL: Duration = Duration::from_millis(500);

/// Everything the renderer mutates, behind one lock so the ticker thread
/// and the command thread can never interleave a line.
pub(crate) struct RenderState {
    pub(crate) out: Box<dyn Write + Send>,
    pub(crate) active: Option<ActiveStep>,
    /// The width of the live line currently on screen, so it can be erased
    /// with exactly as many spaces.
    pub(crate) live_line_width: usize,
    /// Set while a prompt or another writer owns the stream.
    pub(crate) suspended: bool,
    /// Set once the stream's reader has gone. Reporting then stops for
    /// good rather than retrying a write that can never succeed.
    pub(crate) stream_closed: bool,
    /// When the operation's first step began, so a long run of short
    /// steps is not silenced by a deferral that only ever measures one.
    pub(crate) first_step_at: Option<Instant>,
    /// When a whole line was last written, throttling the plain style
    /// across steps as well as within one.
    pub(crate) last_line_at: Option<Instant>,
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
    pub(crate) fn write_line(&mut self, text: &str) {
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

/// Whether ending a step also writes its completion line.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionLine {
    /// Write it, for any step that had announced itself.
    Write,
    /// Erase the live line only, because the caller reports the outcome
    /// itself and the completion line would say the same thing twice.
    Suppress,
}

/// How many cells of `inner_width` are filled at `completed`/`total`.
pub(crate) fn filled_cells(completed: u64, total: u64, inner_width: usize) -> usize {
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
pub(crate) fn percentage(completed: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let percentage = u128::from(completed.min(total)) * 100 / u128::from(total);
    u64::try_from(percentage).unwrap_or(100).min(100)
}

/// How much longer the step is expected to run, extrapolating the rate so
/// far. This is an estimate from an average, not a promise.
pub(crate) fn estimate_remaining(elapsed: Duration, completed: u64, total: u64) -> Duration {
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
pub(crate) fn rate_per_second(completed: u64, elapsed: Duration) -> Option<u64> {
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
pub(crate) fn format_clock(elapsed: Duration) -> String {
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
pub(crate) fn display_width(text: &str) -> usize {
    text.chars().count()
}

/// Truncates to `width` cells, marking the cut with an ellipsis so a
/// clipped line does not read as a complete one.
pub(crate) fn truncate_to_width(text: &str, width: usize) -> String {
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
pub(crate) fn terminal_width() -> usize {
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
pub(crate) fn terminal_width_from_ioctl() -> Option<usize> {
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
pub(crate) fn supports_unicode_bar() -> bool {
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

#[cfg(test)]
mod tests {
    use super::{
        estimate_remaining, filled_cells, format_clock, format_count, format_duration, percentage,
        rate_per_second, truncate_to_width,
    };
    use std::time::Duration;

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
}
