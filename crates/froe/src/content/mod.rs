//! The content layer: decoding records into the content tree.
//!
//! Everything above raw segments lives here: value records (strings and
//! binaries), list records, map records (child node maps), template
//! records, and node records, plus the [`SegmentProvider`] interface
//! through which the decoders reach segment content, path utilities, and
//! the depth-first tree traversal.

pub mod list;
pub mod map;
pub mod node;
pub mod path;
pub mod property;
pub mod provider;
pub mod template;
pub mod traversal;
pub mod value;

pub use map::MapEntry;
pub use node::{NodeState, PropertyState, PropertyValues};
pub use path::normalized_path;
pub use property::{PropertyType, PropertyValue};
pub use provider::SegmentProvider;
pub use template::{ChildNodeArity, PropertyTemplate, Template};
pub use traversal::{DepthFirstTraversal, VisitedNode};
pub use value::{BinaryStream, BinaryValue, read_binary_stream};
