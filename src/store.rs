//! SQLite store of per-test dependency baselines, outcomes, costs, file infos and collection inventory. Unit U1.
//!
//! Same layout ideas as `forktest.store`: each distinct dep is one `dep_keys` row, `test_deps` holds a packed u32
//! key-id array per test, and `key_tests` is the exact inverse, patched on every write so every key is live.

use crate::events::Outcome;
use crate::fingerprint::{Dep, FileInfo, Hash128, RelPath};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// A test's dependency set and its order-free digest (the sum of each dep's xxh3-128 mod 2^128).
#[derive(Clone, Debug)]
pub struct DepSet {
    pub deps: Vec<Dep>,
    pub digest: Hash128,
}

impl DepSet {
    pub fn of(deps: Vec<Dep>) -> DepSet {
        todo!("U1")
    }
}

pub fn dep_hash(dep: &Dep) -> Hash128 {
    todo!("U1")
}

pub struct Record {
    pub nodeid: String,
    pub outcome: Outcome,
    /// What the test cost its worker in seconds, protocol plus recording. The scheduler plans with this.
    pub cost: f64,
    /// `None` when the test's dependency set has the digest already stored for it.
    pub deps: Option<DepSet>,
}

#[derive(Clone, Debug)]
pub struct TestRow {
    pub id: u32,
    pub outcome: Outcome,
    pub cost: f64,
    pub modules: u32,
    pub digest: Hash128,
}

/// What selection reads up front. The key-to-tests index and each test's key list are read on demand.
pub struct StoreView {
    pub tests: HashMap<String, TestRow>,
    pub keys: HashMap<u32, Dep>,
    /// The distinct sets of `<module>` and `<file>` paths tests recorded. `TestRow::modules` points in here.
    pub module_sets: HashMap<u32, Arc<std::collections::HashSet<RelPath>>>,
}

/// One collected test file under one args key.
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryRow {
    pub path: RelPath,
    pub sha: Hash128,
    /// Digest of the `<module>` hashes of everything `ImportGraph::closure(path)` reaches.
    pub closure: Hash128,
    /// Functions and data files recorded while this file was collected.
    pub deps: Vec<Dep>,
    /// After deselection, in collection order.
    pub nodeids: Vec<String>,
    pub error: bool,
    pub skipped: bool,
}

pub struct Store {}

impl Store {
    /// Opens `.forktest/forktest.db` under `root`, creating it. A schema mismatch drops every table.
    pub fn open(root: &Path) -> anyhow::Result<Store> {
        todo!("U1")
    }

    pub fn view(&self) -> anyhow::Result<StoreView> {
        todo!("U1")
    }

    /// Key id to the ids of tests using it, for the given keys only.
    pub fn tests_using(&self, key_ids: &[u32]) -> anyhow::Result<HashMap<u32, Vec<u32>>> {
        todo!("U1")
    }

    pub fn key_ids_of(&self, nodeid: &str) -> anyhow::Result<Vec<u32>> {
        todo!("U1")
    }

    pub fn record(&mut self, records: Vec<Record>) -> anyhow::Result<()> {
        todo!("U1")
    }

    /// Drop tests that no longer exist, and every key only they used.
    pub fn forget(&mut self, nodeids: &[String]) -> anyhow::Result<()> {
        todo!("U1")
    }

    pub fn file_infos(&self) -> anyhow::Result<HashMap<RelPath, FileInfo>> {
        todo!("U1")
    }

    pub fn save_file_infos(&mut self, infos: Vec<(RelPath, FileInfo)>) -> anyhow::Result<()> {
        todo!("U1")
    }

    pub fn inventory(&self, args_key: &str) -> anyhow::Result<Vec<InventoryRow>> {
        todo!("U1")
    }

    /// Replace the rows for these paths. `full` replaces the whole inventory of the args key.
    pub fn put_inventory(&mut self, args_key: &str, rows: Vec<InventoryRow>, full: bool) -> anyhow::Result<()> {
        todo!("U1")
    }

    pub fn meta_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        todo!("U1")
    }

    pub fn meta_set(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        todo!("U1")
    }
}

/// Fingerprints of files as they were at the merge base of HEAD and a git ref, for `--since`.
pub struct GitBaseline {}

impl GitBaseline {
    pub fn new(root: &Path, git_ref: &str) -> anyhow::Result<GitBaseline> {
        todo!("U1")
    }

    /// `None` when the file or function did not exist there.
    pub fn hash_of(&mut self, path: &str, qualname: &str) -> Option<Hash128> {
        todo!("U1")
    }
}
