//! Progress reporting for `froe export`.
//!
//! [`ProgressSink`] wraps any [`ExportSink`] and reports how many nodes
//! have been written, to stderr — standard output stays pure data, so
//! piping a JSON lines export into a consumer never mixes progress into
//! the stream. On a terminal the report is one live line, rewritten in
//! place; elsewhere (a pipe, a file, a CI log) each report is a fresh
//! line, so the progress is still visible in captured output.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use froe_export::{ExportSink, ExportedNode};

/// How often a progress report is emitted.
const REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// How many nodes between reports when the interval has not elapsed.
const REPORT_NODE_STRIDE: u64 = 100_000;

/// Wraps a sink and reports export progress to stderr.
pub(crate) struct ProgressSink<S> {
    inner: S,
    written: u64,
    started: Instant,
    last_report: Instant,
    last_reported: u64,
    live_line: bool,
}

impl<S> ProgressSink<S> {
    /// Wraps `inner`. When `quiet`, no progress is reported at all.
    pub(crate) fn new(inner: S, quiet: bool) -> Self {
        let stderr = std::io::stderr();
        Self {
            inner,
            written: 0,
            started: Instant::now(),
            last_report: Instant::now(),
            last_reported: 0,
            live_line: !quiet && stderr.is_terminal(),
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

    /// Reports progress when the interval or node stride has elapsed.
    fn report_if_due(&mut self) {
        let now = Instant::now();
        let due = now.duration_since(self.last_report) >= REPORT_INTERVAL
            || self.written - self.last_reported >= REPORT_NODE_STRIDE;
        if !due {
            return;
        }
        self.last_report = now;
        self.last_reported = self.written;
        let elapsed = now.duration_since(self.started);
        let rate = if elapsed.is_zero() {
            0.0
        } else {
            // A node count is a display figure; the precision loss of the
            // cast is irrelevant at the reported scale.
            #[allow(clippy::cast_precision_loss)]
            let rate = self.written as f64 / elapsed.as_secs_f64();
            rate
        };
        let mut stderr = std::io::stderr();
        if self.live_line {
            let _ = write!(
                stderr,
                "\rfroe: exported {} nodes ({:.0} nodes/s)",
                self.written, rate
            );
        } else {
            let _ = writeln!(
                stderr,
                "froe: exported {} nodes ({:.0} nodes/s)",
                self.written, rate
            );
        }
        let _ = stderr.flush();
    }

    /// Ends the live line: a newline so the next output starts on a fresh
    /// line, and a final report when the last one is stale.
    pub(crate) fn finish_report(&mut self) {
        if self.live_line {
            let _ = writeln!(std::io::stderr());
        }
        self.report_if_due();
    }
}

impl<S: ExportSink> ExportSink for ProgressSink<S> {
    fn write_node(&mut self, node: &ExportedNode<'_>) -> froe::Result<()> {
        self.inner.write_node(node)?;
        self.written += 1;
        self.report_if_due();
        Ok(())
    }

    fn finish(&mut self) -> froe::Result<()> {
        self.inner.finish()?;
        self.finish_report();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::Duration;

    use froe_export::{ExportSink, ExportedNode};

    use super::{ProgressSink, REPORT_NODE_STRIDE};

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
