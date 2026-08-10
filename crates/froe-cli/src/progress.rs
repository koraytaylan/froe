//! Progress reporting for `froe export`.
//!
//! [`ProgressSink`] wraps any [`ExportSink`] and reports how many nodes
//! have been written, to stderr — standard output stays pure data, so
//! piping a JSON lines export into a consumer never mixes progress into
//! the stream. On a terminal the report is one live line, rewritten in
//! place; elsewhere (a pipe, a file, a CI log) each report is a fresh
//! line, so the progress is still visible in captured output.
//!
//! [`Reporter`] is the same throttling without the sink wrapper, for
//! operations that stream nodes without driving an [`ExportSink`]
//! themselves — the Parquet refresh's delta export reports through it.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use froe_export::{ExportSink, ExportedNode};

/// How often a progress report is emitted.
const REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// How many nodes between reports when the interval has not elapsed.
const REPORT_NODE_STRIDE: u64 = 100_000;

/// A throttled stderr reporter for node-streaming operations. A quiet
/// reporter is disabled outright: it prints nothing, however long the
/// operation runs.
pub(crate) struct Reporter {
    out: Box<dyn Write>,
    enabled: bool,
    live_line: bool,
    started: Instant,
    last_report: Instant,
    last_reported: u64,
    verb: &'static str,
}

impl Reporter {
    /// Creates a reporter describing its operation with `verb`
    /// ("exported", "re-exported"). When `quiet`, nothing is reported
    /// at all.
    pub(crate) fn new(quiet: bool, verb: &'static str) -> Self {
        Self {
            live_line: !quiet && std::io::stderr().is_terminal(),
            enabled: !quiet,
            out: Box::new(std::io::stderr()),
            started: Instant::now(),
            last_report: Instant::now(),
            last_reported: 0,
            verb,
        }
    }

    /// A reporter writing to `out`, for tests: `live_line` selects the
    /// rewritten-line style directly instead of querying the terminal.
    #[cfg(test)]
    pub(crate) fn capturing(quiet: bool, live_line: bool, out: Box<dyn Write>) -> Self {
        Self {
            live_line: live_line && !quiet,
            enabled: !quiet,
            out,
            started: Instant::now(),
            last_report: Instant::now(),
            last_reported: 0,
            verb: "exported",
        }
    }

    /// The elapsed time since the operation started.
    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Reports progress when the interval or node stride has elapsed.
    pub(crate) fn report(&mut self, written: u64) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let due = now.duration_since(self.last_report) >= REPORT_INTERVAL
            || written - self.last_reported >= REPORT_NODE_STRIDE;
        if !due {
            return;
        }
        self.last_report = now;
        self.last_reported = written;
        let elapsed = now.duration_since(self.started);
        let rate = if elapsed.is_zero() {
            0.0
        } else {
            // A node count is a display figure; the precision loss of the
            // cast is irrelevant at the reported scale.
            #[allow(clippy::cast_precision_loss)]
            let rate = written as f64 / elapsed.as_secs_f64();
            rate
        };
        if self.live_line {
            let _ = write!(
                self.out,
                "\rfroe: {} {} nodes ({:.0} nodes/s)",
                self.verb, written, rate
            );
        } else {
            let _ = writeln!(
                self.out,
                "froe: {} {} nodes ({:.0} nodes/s)",
                self.verb, written, rate
            );
        }
        let _ = self.out.flush();
    }

    /// Ends the live line: a newline so the next output starts on a
    /// fresh line.
    pub(crate) fn finish_line(&mut self) {
        if self.live_line {
            let _ = writeln!(self.out);
        }
    }
}

/// Wraps a sink and reports export progress to stderr.
pub(crate) struct ProgressSink<S> {
    inner: S,
    written: u64,
    reporter: Reporter,
}

impl<S> ProgressSink<S> {
    /// Wraps `inner`. When `quiet`, no progress is reported at all.
    pub(crate) fn new(inner: S, quiet: bool) -> Self {
        Self {
            inner,
            written: 0,
            reporter: Reporter::new(quiet, "exported"),
        }
    }

    /// The number of nodes written so far.
    #[cfg(test)]
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// The elapsed time since the export started.
    pub(crate) fn elapsed(&self) -> Duration {
        self.reporter.elapsed()
    }
}

impl<S: ExportSink> ExportSink for ProgressSink<S> {
    fn write_node(&mut self, node: &ExportedNode<'_>) -> froe::Result<()> {
        self.inner.write_node(node)?;
        self.written += 1;
        self.reporter.report(self.written);
        Ok(())
    }

    fn finish(&mut self) -> froe::Result<()> {
        self.inner.finish()?;
        self.reporter.report(self.written);
        self.reporter.finish_line();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;
    use std::time::Duration;

    use froe_export::{ExportSink, ExportedNode};

    use super::{ProgressSink, REPORT_NODE_STRIDE, Reporter};

    /// A sink that records every node and finish call.
    struct RecordingSink {
        nodes: RefCell<Vec<String>>,
        finished: RefCell<bool>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                nodes: RefCell::new(Vec::new()),
                finished: RefCell::new(false),
            }
        }
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

    /// A `Write` target tests can read back.
    #[derive(Clone)]
    struct SharedBuffer(Rc<RefCell<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
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
    fn quiet_sinks_report_nothing() {
        let mut sink = ProgressSink::new(RecordingSink::new(), true);
        sink.write_node(&node("/a")).expect("write");
        sink.finish().expect("finish");
        assert_eq!(sink.written(), 1);
    }

    #[test]
    fn quiet_reporters_stay_silent_past_the_stride() {
        let captured = SharedBuffer(Rc::new(RefCell::new(Vec::new())));
        let mut reporter = Reporter::capturing(true, false, Box::new(captured.clone()));
        reporter.report(REPORT_NODE_STRIDE + 1);
        reporter.finish_line();
        assert!(
            captured.0.borrow().is_empty(),
            "a quiet reporter must not print even when a report is due: {}",
            String::from_utf8_lossy(&captured.0.borrow())
        );
    }

    #[test]
    fn due_reports_reach_the_output() {
        let captured = SharedBuffer(Rc::new(RefCell::new(Vec::new())));
        let mut reporter = Reporter::capturing(false, false, Box::new(captured.clone()));
        reporter.report(REPORT_NODE_STRIDE);
        let printed = String::from_utf8_lossy(&captured.0.borrow()).into_owned();
        assert!(
            printed.contains("exported 100000 nodes"),
            "the report names the count: {printed}"
        );
    }

    #[test]
    fn nodes_reach_the_inner_sink() {
        let inner = RecordingSink::new();
        let mut sink = ProgressSink::new(inner, true);
        sink.write_node(&node("/a")).expect("write");
        sink.write_node(&node("/b")).expect("write");
        sink.finish().expect("finish");
        assert_eq!(sink.written(), 2);
        assert_eq!(
            *sink.inner.nodes.borrow(),
            vec!["/a".to_owned(), "/b".to_owned()]
        );
        assert!(*sink.inner.finished.borrow());
    }

    #[test]
    fn the_node_stride_triggers_reports() {
        let mut sink = ProgressSink::new(RecordingSink::new(), false);
        for index in 0..=REPORT_NODE_STRIDE {
            sink.write_node(&node(&format!("/{index}"))).expect("write");
        }
        assert_eq!(sink.written(), REPORT_NODE_STRIDE + 1);
    }

    #[test]
    fn elapsed_measures_the_export() {
        let mut sink = ProgressSink::new(RecordingSink::new(), true);
        std::thread::sleep(Duration::from_millis(5));
        sink.write_node(&node("/a")).expect("write");
        assert!(sink.elapsed() >= Duration::from_millis(5));
    }
}
