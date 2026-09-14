//! The index inventory: one record per definition, aggregating what Oak's own
//! index printer prints and what froe can add read-only.
//!
//! It is assembled from the definition model, the lane state and the three
//! storage readers, so it lands after all of them; the module exists now so
//! that the task which creates it never contends with the one that registers
//! it in the module root.
