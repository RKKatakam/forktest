//! Which tests a change affects, and why; which test files need collecting again. Pure over a store view. Unit U1.

use crate::fingerprint::{Dep, Fingerprints, RelPath};
use crate::graph::ImportGraph;
use crate::store::{InventoryRow, Store, StoreView};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reason {
    All,
    ConfigChange,
    New,
    LastFailed,
    Coverage,
    Fixture,
    FileRead,
    ImportGraph,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::All => "all",
            Reason::ConfigChange => "config_change",
            Reason::New => "new",
            Reason::LastFailed => "last_failed",
            Reason::Coverage => "coverage",
            Reason::Fixture => "fixture",
            Reason::FileRead => "file_read",
            Reason::ImportGraph => "import_graph",
        }
    }
}

/// Where a dependency's current hash comes from: the working tree, or a git baseline for `--since`.
pub trait Current {
    fn changed(&mut self, dep: &Dep) -> bool;
}

pub struct Affected {
    /// The highest-priority reason per selected nodeid (the `Ord` of `Reason` is the priority).
    pub reasons: HashMap<String, Reason>,
    /// Changed paths that affect a selected test, sorted.
    pub changed: Vec<String>,
}

/// Each distinct key is checked once, only changed keys' index rows are read, and the import-graph rule runs once
/// per distinct (test file, recorded module set) pair. `fixtures` are the fixture definitions' (path, qualname).
pub fn select(
    nodeids: &[String],
    view: &StoreView,
    store: &Store,
    current: &mut dyn Current,
    graph: &ImportGraph,
    fixtures: &HashSet<(RelPath, std::sync::Arc<str>)>,
) -> anyhow::Result<Affected> {
    todo!("U1: port forktest.selection.select")
}

/// Every reason `select` would have for one test, for `--why`.
pub fn explain(
    nodeid: &str,
    view: &StoreView,
    store: &Store,
    current: &mut dyn Current,
    graph: &ImportGraph,
    fixtures: &HashSet<(RelPath, std::sync::Arc<str>)>,
) -> anyhow::Result<Vec<(Reason, Option<Dep>)>> {
    todo!("U1")
}

/// Why a test file has to be collected again, or `None` when its inventory row still holds.
pub fn recollect(row: &InventoryRow, fps: &Fingerprints, graph: &ImportGraph) -> Option<&'static str> {
    todo!("U1")
}

/// The closure digest an inventory row stores.
pub fn closure_digest(test_file: &str, fps: &Fingerprints, graph: &ImportGraph) -> u128 {
    todo!("U1")
}

pub fn reason_counts(reasons: &HashMap<String, Reason>) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for r in reasons.values() {
        *out.entry(r.as_str().to_string()).or_insert(0) += 1;
    }
    out
}
