//! Java's own semantics, reproduced exactly.
//!
//! Oak writes `gc.log` and `store.version` with Java's standard library, so
//! reading them back means matching what that library does rather than what
//! a reasonable parser would do: `Long.parseLong` accepts every Unicode BMP
//! decimal digit, `String.split` drops trailing empty fields, and
//! `java.util.Properties` splices logical lines across odd backslashes.
//!
//! These rules belong to Java, not to either file that depends on them —
//! the decimal-digit table lived in both before this module existed.
//!
//! The tests live with the two callers rather than here: what is worth
//! asserting is that the `gc.log` parser accepts a Unicode digit and that
//! the manifest reader accepts an escaped key, not that a helper does.

mod numbers;
mod properties;
mod split;

pub(crate) use numbers::*;
pub(crate) use properties::*;
pub(crate) use split::*;
