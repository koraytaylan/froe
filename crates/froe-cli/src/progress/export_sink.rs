//! Counting an export's nodes as they pass through to the real sink, so
//! the reporter has something to report.

use super::{Duration, Instant, ProgressObserver, Reporter};

/// Wraps an export sink and advances the reporter's step per written node.
pub(crate) struct ProgressSink<Sink> {
    pub(crate) inner: Sink,
    pub(crate) written: u64,
    pub(crate) reporter: Reporter,
    pub(crate) started: Instant,
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
    use super::ProgressSink;
    use crate::progress::test_support::{SharedBuffer, reporter};
    use crate::progress::{Reporter, Style};
    use froe::progress::{ProgressObserver, Step, WorkUnit};
    use froe_export::{ExportSink, ExportedNode};
    use std::cell::RefCell;
    use std::time::Duration;

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
}
