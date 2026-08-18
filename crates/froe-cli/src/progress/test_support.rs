//! A reporter writing into a buffer the test can read back.

use super::{Reporter, Style};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A `Write` the test can read back from another thread.
#[derive(Clone)]
pub(crate) struct SharedBuffer(pub(crate) Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    pub(crate) fn text(&self) -> String {
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
pub(crate) fn reporter(style: Style, captured: &SharedBuffer) -> Reporter {
    Reporter::with_output(
        style,
        Duration::ZERO,
        Duration::ZERO,
        Some(100),
        Box::new(captured.clone()),
    )
}
