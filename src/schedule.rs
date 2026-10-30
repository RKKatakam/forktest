//! Batches for workers: module groups, failed first, longest processing time. Unit U1.
//!
//! A module costing more than an even share of the run is split by class, then into consecutive runs, so
//! module- and class-scoped fixtures are rebuilt as rarely as the share allows.

use std::collections::HashSet;

/// `items` are `(nodeid, cost in seconds)`. Returns per-worker lists of indices into `items`, each in order.
pub fn schedule(items: &[(&str, f64)], failed: &HashSet<&str>, workers: usize) -> Vec<Vec<usize>> {
    todo!("U1: port forktest.zygote.schedule")
}
