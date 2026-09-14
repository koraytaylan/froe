//! Java's own semantics, reproduced exactly.
//!
//! Oak writes `gc.log` and `store.version` with Java's standard library, so
//! reading them back means matching what that library does rather than what
//! a reasonable parser would do: `Long.parseLong` accepts every Unicode BMP
//! decimal digit, `String.split` drops trailing empty fields, and
//! `java.util.Properties` splices logical lines across odd backslashes.
//!
//! Index keys go through a third rule: Java's URL encoding of a string as
//! UTF-8, which the property-index editor applies to every value it indexes.
//! Dates go through a fourth: Jackrabbit's own `ISO8601`, which is what
//! Oak means by a date and is not what ISO 8601 means by one.
//!
//! These rules belong to Java, not to the files that depend on them — the
//! decimal-digit table lived in both of its callers before this module
//! existed.
//!
//! Where the tests live differs between the two kinds of helper here, and the
//! difference is deliberate:
//!
//! * The **file-format helpers** — [`numbers`], [`properties`], [`split`] —
//!   are tested at their callers. What is worth asserting is that the
//!   `gc.log` parser accepts a Unicode digit and that the manifest reader
//!   accepts an escaped key; that a helper in isolation does is not the
//!   property anyone relies on.
//! * The **Java-semantics primitives** the index plans add — [`url_encoder`]
//!   and [`iso8601`] — are tested here, by replaying vectors a real JDK
//!   produced. The
//!   property being pinned *is* "this function equals Java's", so the test
//!   belongs with the function, and the oracle is a file rather than a
//!   hand-written expectation. A caller-side test could not distinguish a
//!   wrong encoder from a wrong caller.

mod iso8601;
mod numbers;
mod properties;
mod split;
mod url_encoder;

pub(crate) use iso8601::*;
pub(crate) use numbers::*;
pub(crate) use properties::*;
pub(crate) use split::*;
pub(crate) use url_encoder::*;
