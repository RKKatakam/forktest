//! SQLite store of per-test dependency baselines, outcomes, costs, file infos and collection inventory. Unit U1.
//!
//! Same layout ideas as `forktest.store`: each distinct dep is one `dep_keys` row, `test_deps` holds a packed u32
//! key-id array per test, and `key_tests` is the exact inverse, patched on every write so every key is live.

use crate::events::Outcome;
use crate::fingerprint::{Dep, FILE, FileInfo, FileKind, Hash128, MODULE, RelPath, Stat};
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tests(
    id INTEGER PRIMARY KEY, nodeid TEXT NOT NULL UNIQUE, outcome TEXT NOT NULL, cost REAL NOT NULL,
    last_run REAL NOT NULL, modules INTEGER NOT NULL, digest BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS test_deps(test_id INTEGER PRIMARY KEY, deps BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS dep_keys(
    id INTEGER PRIMARY KEY, path TEXT NOT NULL, qualname TEXT NOT NULL, hash BLOB NOT NULL,
    UNIQUE(path, qualname, hash));
CREATE TABLE IF NOT EXISTS key_tests(key_id INTEGER PRIMARY KEY, tests BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS module_sets(id INTEGER PRIMARY KEY, paths TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS files(
    path TEXT PRIMARY KEY, sha BLOB NOT NULL, mtime_ns INTEGER, ctime_ns INTEGER, size INTEGER,
    info BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS inventory(
    args_key TEXT NOT NULL, path TEXT NOT NULL, sha BLOB NOT NULL, closure BLOB NOT NULL, deps TEXT NOT NULL,
    nodeids TEXT NOT NULL, error INTEGER NOT NULL, skipped INTEGER NOT NULL, PRIMARY KEY(args_key, path));
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// A test's dependency set and its order-free digest (the sum of each dep's xxh3-128 mod 2^128).
#[derive(Clone, Debug)]
pub struct DepSet {
    pub deps: Vec<Dep>,
    pub digest: Hash128,
}

impl DepSet {
    pub fn of(mut deps: Vec<Dep>) -> DepSet {
        deps.sort_unstable();
        deps.dedup();
        let digest = deps.iter().fold(0u128, |acc, d| acc.wrapping_add(dep_hash(d)));
        DepSet { deps, digest }
    }
}

/// Order-free digests sum these, so a worker can cache each dep's hash and skip sorting.
pub fn dep_hash(dep: &Dep) -> Hash128 {
    let mut h = Xxh3::new();
    h.update(dep.path.as_bytes());
    h.update(&[0]);
    h.update(dep.qualname.as_bytes());
    h.update(&[0]);
    h.update(&dep.hash.to_be_bytes());
    h.digest128()
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

pub struct Store {
    conn: Connection,
}

fn blob(h: Hash128) -> [u8; 16] {
    h.to_be_bytes()
}

fn unblob(b: &[u8]) -> Hash128 {
    b.try_into().map_or(0, u128::from_be_bytes)
}

fn pack(ids: &[u32]) -> Vec<u8> {
    ids.iter().flat_map(|i| i.to_le_bytes()).collect()
}

fn unpack(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

fn outcome_of(name: &str) -> Outcome {
    Outcome::from_event_name(name).unwrap_or(Outcome::Error)
}

/// Strings shared by many rows, allocated once.
#[derive(Default)]
struct Interner(HashSet<Arc<str>>);

impl Interner {
    fn get(&mut self, s: &str) -> Arc<str> {
        if let Some(a) = self.0.get(s) {
            return a.clone();
        }
        let a: Arc<str> = Arc::from(s);
        self.0.insert(a.clone());
        a
    }
}

/// Per key, the test ids to add to and remove from `key_tests`.
#[derive(Default)]
struct IndexDelta {
    added: BTreeMap<u32, Vec<u32>>,
    removed: BTreeMap<u32, HashSet<u32>>,
}

impl IndexDelta {
    fn replace(&mut self, test_id: u32, previous: &[u32], current: &[u32]) {
        if previous == current {
            return;
        }
        let before: HashSet<u32> = previous.iter().copied().collect();
        let after: HashSet<u32> = current.iter().copied().collect();
        for &k in current.iter().filter(|k| !before.contains(k)) {
            self.added.entry(k).or_default().push(test_id);
        }
        for &k in previous.iter().filter(|k| !after.contains(k)) {
            self.removed.entry(k).or_default().insert(test_id);
        }
    }

    fn apply(self, tx: &Transaction) -> anyhow::Result<()> {
        let mut read = tx.prepare_cached("SELECT tests FROM key_tests WHERE key_id = ?1")?;
        let mut put = tx.prepare_cached("INSERT OR REPLACE INTO key_tests VALUES (?1, ?2)")?;
        let mut drop_index = tx.prepare_cached("DELETE FROM key_tests WHERE key_id = ?1")?;
        let mut drop_key = tx.prepare_cached("DELETE FROM dep_keys WHERE id = ?1")?;
        let mut touched: Vec<u32> = self.added.keys().chain(self.removed.keys()).copied().collect();
        touched.sort_unstable();
        touched.dedup();
        for k in touched {
            let mut tests = read.query_row([k], |r| r.get_ref(0).map(|v| unpack(v.as_blob().unwrap_or(&[])))).optional()?.unwrap_or_default();
            if let Some(removed) = self.removed.get(&k) {
                tests.retain(|t| !removed.contains(t));
            }
            tests.extend(self.added.get(&k).into_iter().flatten());
            if tests.is_empty() {
                drop_index.execute([k])?;
                drop_key.execute([k])?;
            } else {
                put.execute(params![k, pack(&tests)])?;
            }
        }
        tx.execute("DELETE FROM module_sets WHERE id NOT IN (SELECT modules FROM tests)", [])?;
        Ok(())
    }
}

/// A test's id and packed key ids, by nodeid.
fn existing(tx: &Transaction, nodeid: &str) -> anyhow::Result<Option<(u32, Vec<u32>)>> {
    let mut stmt =
        tx.prepare_cached("SELECT t.id, d.deps FROM tests t LEFT JOIN test_deps d ON d.test_id = t.id WHERE t.nodeid = ?1")?;
    Ok(stmt
        .query_row([nodeid], |r| {
            let deps = r.get_ref(1)?.as_blob_or_null()?.map(unpack).unwrap_or_default();
            Ok((r.get(0)?, deps))
        })
        .optional()?)
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

impl Store {
    /// Opens `.forktest/forktest.db` under `root`, creating it. A schema mismatch drops every table.
    pub fn open(root: &Path) -> anyhow::Result<Store> {
        let dir = root.join(".forktest");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut conn = Connection::open(dir.join("forktest.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            let tables: Vec<String> = tx
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for t in tables {
                tx.execute(&format!("DROP TABLE IF EXISTS \"{}\"", t.replace('"', "\"\"")), [])?;
            }
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        tx.execute_batch(SCHEMA)?;
        tx.commit()?;
        Ok(Store { conn })
    }

    pub fn view(&self) -> anyhow::Result<StoreView> {
        let mut strings = Interner::default();
        let mut keys = HashMap::new();
        let mut stmt = self.conn.prepare_cached("SELECT id, path, qualname, hash FROM dep_keys")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let path = strings.get(r.get_ref(1)?.as_str()?);
            let qualname = strings.get(r.get_ref(2)?.as_str()?);
            keys.insert(r.get(0)?, Dep { path, qualname, hash: unblob(r.get_ref(3)?.as_blob()?) });
        }
        let mut module_sets = HashMap::new();
        let mut stmt = self.conn.prepare_cached("SELECT id, paths FROM module_sets")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let paths: Vec<&str> = serde_json::from_str(r.get_ref(1)?.as_str()?)?;
            module_sets.insert(r.get(0)?, Arc::new(paths.into_iter().map(|p| strings.get(p)).collect()));
        }
        let mut tests = HashMap::new();
        let mut stmt = self.conn.prepare_cached("SELECT id, nodeid, outcome, cost, modules, digest FROM tests")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let row = TestRow {
                id: r.get(0)?,
                outcome: outcome_of(r.get_ref(2)?.as_str()?),
                cost: r.get(3)?,
                modules: r.get(4)?,
                digest: unblob(r.get_ref(5)?.as_blob()?),
            };
            tests.insert(r.get(1)?, row);
        }
        Ok(StoreView { tests, keys, module_sets })
    }

    /// Key id to the ids of tests using it, for the given keys only.
    pub fn tests_using(&self, key_ids: &[u32]) -> anyhow::Result<HashMap<u32, Vec<u32>>> {
        let mut stmt = self.conn.prepare_cached("SELECT tests FROM key_tests WHERE key_id = ?1")?;
        let mut out = HashMap::with_capacity(key_ids.len());
        for &k in key_ids {
            if let Some(tests) = stmt.query_row([k], |r| Ok(unpack(r.get_ref(0)?.as_blob()?))).optional()? {
                out.insert(k, tests);
            }
        }
        Ok(out)
    }

    pub fn key_ids_of(&self, nodeid: &str) -> anyhow::Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT deps FROM test_deps JOIN tests ON tests.id = test_id WHERE nodeid = ?1")?;
        Ok(stmt.query_row([nodeid], |r| Ok(unpack(r.get_ref(0)?.as_blob()?))).optional()?.unwrap_or_default())
    }

    pub fn record(&mut self, records: Vec<Record>) -> anyhow::Result<()> {
        let mut latest: Vec<Record> = Vec::with_capacity(records.len());
        let mut position: HashMap<String, usize> = HashMap::new();
        for r in records {
            match position.get(&r.nodeid) {
                Some(&i) => latest[i] = r,
                None => {
                    position.insert(r.nodeid.clone(), latest.len());
                    latest.push(r);
                }
            }
        }
        if latest.is_empty() {
            return Ok(());
        }
        let now = now();
        // The index is read, patched and written back, so take the write lock before the first read.
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut update = tx.prepare_cached("UPDATE tests SET outcome = ?1, cost = ?2, last_run = ?3 WHERE nodeid = ?4")?;
            for r in latest.iter().filter(|r| r.deps.is_none()) {
                update.execute(params![r.outcome.event_name(), r.cost, now, r.nodeid])?;
            }
        }
        if latest.iter().any(|r| r.deps.is_some()) {
            record_deps(&tx, latest.iter().filter(|r| r.deps.is_some()), now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop tests that no longer exist, and every key only they used.
    pub fn forget(&mut self, nodeids: &[String]) -> anyhow::Result<()> {
        if nodeids.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut delta = IndexDelta::default();
        for nodeid in nodeids {
            let Some((test_id, previous)) = existing(&tx, nodeid)? else { continue };
            delta.replace(test_id, &previous, &[]);
            tx.execute("DELETE FROM tests WHERE id = ?1", [test_id])?;
            tx.execute("DELETE FROM test_deps WHERE test_id = ?1", [test_id])?;
        }
        delta.apply(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn file_infos(&self) -> anyhow::Result<HashMap<RelPath, FileInfo>> {
        let mut stmt = self.conn.prepare_cached("SELECT path, sha, mtime_ns, ctime_ns, size, info FROM files")?;
        let mut rows = stmt.query([])?;
        let mut out = HashMap::new();
        while let Some(r) = rows.next()? {
            let Some(kind) = FileKind::decode(r.get_ref(5)?.as_blob()?) else { continue };
            let stat = match (r.get(2)?, r.get(3)?, r.get(4)?) {
                (Some(mtime_ns), Some(ctime_ns), Some(size)) => Some(Stat { mtime_ns, ctime_ns, size }),
                _ => None,
            };
            let path: RelPath = Arc::from(r.get_ref(0)?.as_str()?);
            out.insert(path, FileInfo { sha: unblob(r.get_ref(1)?.as_blob()?), stat, kind });
        }
        Ok(out)
    }

    pub fn save_file_infos(&mut self, infos: Vec<(RelPath, FileInfo)>) -> anyhow::Result<()> {
        if infos.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut put = tx.prepare_cached("INSERT OR REPLACE INTO files VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
            for (path, info) in &infos {
                let stat = info.stat.as_ref();
                put.execute(params![
                    &**path,
                    blob(info.sha),
                    stat.map(|s| s.mtime_ns),
                    stat.map(|s| s.ctime_ns),
                    stat.map(|s| s.size),
                    info.kind.encode()
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Ordered by path.
    pub fn inventory(&self, args_key: &str) -> anyhow::Result<Vec<InventoryRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path, sha, closure, deps, nodeids, error, skipped FROM inventory WHERE args_key = ?1 ORDER BY path",
        )?;
        let mut rows = stmt.query([args_key])?;
        let mut strings = Interner::default();
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let deps: Vec<(&str, &str, &str)> = serde_json::from_str(r.get_ref(3)?.as_str()?)?;
            let deps = deps
                .into_iter()
                .map(|(p, q, h)| Dep { path: strings.get(p), qualname: strings.get(q), hash: u128::from_str_radix(h, 16).unwrap_or(0) })
                .collect();
            out.push(InventoryRow {
                path: strings.get(r.get_ref(0)?.as_str()?),
                sha: unblob(r.get_ref(1)?.as_blob()?),
                closure: unblob(r.get_ref(2)?.as_blob()?),
                deps,
                nodeids: serde_json::from_str(r.get_ref(4)?.as_str()?)?,
                error: r.get(5)?,
                skipped: r.get(6)?,
            });
        }
        Ok(out)
    }

    /// Replace the rows for these paths. `full` replaces the whole inventory of the args key.
    pub fn put_inventory(&mut self, args_key: &str, rows: Vec<InventoryRow>, full: bool) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        if full {
            tx.execute("DELETE FROM inventory WHERE args_key = ?1", [args_key])?;
        }
        {
            let mut put = tx.prepare_cached("INSERT OR REPLACE INTO inventory VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
            for row in &rows {
                let deps: Vec<(&str, &str, String)> =
                    row.deps.iter().map(|d| (&*d.path, &*d.qualname, format!("{:032x}", d.hash))).collect();
                put.execute(params![
                    args_key,
                    &*row.path,
                    blob(row.sha),
                    blob(row.closure),
                    serde_json::to_string(&deps)?,
                    serde_json::to_string(&row.nodeids)?,
                    row.error,
                    row.skipped
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn meta_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")?;
        Ok(stmt.query_row([key], |r| r.get(0)).optional()?)
    }

    pub fn meta_set(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.prepare_cached("INSERT OR REPLACE INTO meta VALUES (?1, ?2)")?.execute([key, value])?;
        Ok(())
    }
}

fn record_deps<'r>(tx: &Transaction, records: impl Iterator<Item = &'r Record>, now: f64) -> anyhow::Result<()> {
    let mut find_key = tx.prepare_cached("SELECT id FROM dep_keys WHERE path = ?1 AND qualname = ?2 AND hash = ?3")?;
    let mut insert_key = tx.prepare_cached("INSERT INTO dep_keys(path, qualname, hash) VALUES (?1, ?2, ?3)")?;
    let mut find_set = tx.prepare_cached("SELECT id FROM module_sets WHERE paths = ?1")?;
    let mut insert_set = tx.prepare_cached("INSERT INTO module_sets(paths) VALUES (?1)")?;
    let mut update = tx.prepare_cached(
        "UPDATE tests SET outcome = ?1, cost = ?2, last_run = ?3, modules = ?4, digest = ?5 WHERE id = ?6",
    )?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO tests(outcome, cost, last_run, modules, digest, nodeid) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut put_deps = tx.prepare_cached("INSERT OR REPLACE INTO test_deps VALUES (?1, ?2)")?;
    let mut key_ids: HashMap<&Dep, u32> = HashMap::new();
    let mut set_ids: HashMap<String, u32> = HashMap::new();
    let mut delta = IndexDelta::default();
    for r in records {
        let deps = r.deps.as_ref().expect("filtered on deps");
        let mut ids = Vec::with_capacity(deps.deps.len());
        for d in &deps.deps {
            let id = match key_ids.get(d) {
                Some(&id) => id,
                None => {
                    let args = params![&*d.path, &*d.qualname, blob(d.hash)];
                    let id = match find_key.query_row(args, |r| r.get(0)).optional()? {
                        Some(id) => id,
                        None => {
                            insert_key.execute(args)?;
                            tx.last_insert_rowid() as u32
                        }
                    };
                    key_ids.insert(d, id);
                    id
                }
            };
            ids.push(id);
        }
        ids.sort_unstable();
        let mut paths: Vec<&str> =
            deps.deps.iter().filter(|d| &*d.qualname == MODULE || &*d.qualname == FILE).map(|d| &*d.path).collect();
        paths.sort_unstable();
        paths.dedup();
        let paths = serde_json::to_string(&paths)?;
        let modules = match set_ids.get(&paths) {
            Some(&id) => id,
            None => {
                let id = match find_set.query_row([&paths], |r| r.get(0)).optional()? {
                    Some(id) => id,
                    None => {
                        insert_set.execute([&paths])?;
                        tx.last_insert_rowid() as u32
                    }
                };
                set_ids.insert(paths, id);
                id
            }
        };
        let outcome = r.outcome.event_name();
        let previous = existing(tx, &r.nodeid)?;
        let test_id = match &previous {
            Some((test_id, _)) => {
                update.execute(params![outcome, r.cost, now, modules, blob(deps.digest), test_id])?;
                *test_id
            }
            None => {
                insert.execute(params![outcome, r.cost, now, modules, blob(deps.digest), r.nodeid])?;
                tx.last_insert_rowid() as u32
            }
        };
        let previous = previous.map(|(_, p)| p).unwrap_or_default();
        if previous != ids {
            put_deps.execute(params![test_id, pack(&ids)])?;
        }
        delta.replace(test_id, &previous, &ids);
    }
    delta.apply(tx)
}

/// Fingerprints of files as they were at the merge base of HEAD and a git ref, for `--since`.
pub struct GitBaseline {
    rev: String,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    cache: HashMap<String, Option<FileInfo>>,
}

impl GitBaseline {
    pub fn new(root: &Path, git_ref: &str) -> anyhow::Result<GitBaseline> {
        let merge_base = Command::new("git").args(["merge-base", "HEAD", git_ref]).current_dir(root).output()?;
        let rev = if merge_base.status.success() {
            String::from_utf8_lossy(&merge_base.stdout).trim().to_string()
        } else {
            git_ref.to_string()
        };
        let mut child = Command::new("git")
            .args(["cat-file", "--batch"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("starting git cat-file")?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        Ok(GitBaseline { rev, child, stdin, stdout, cache: HashMap::new() })
    }

    /// The file's bytes at the baseline. `./` makes the path relative to the project root, which need not be the
    /// repository's top level.
    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        let stdin = self.stdin.as_mut()?;
        writeln!(stdin, "{}:./{path}", self.rev).ok()?;
        stdin.flush().ok()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header).ok()?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        if fields.len() < 3 {
            return None;
        }
        let size: usize = fields[2].parse().ok()?;
        let mut data = vec![0; size + 1];
        self.stdout.read_exact(&mut data).ok()?;
        data.pop();
        Some(data)
    }

    /// `None` when the file or function did not exist there.
    pub fn hash_of(&mut self, path: &str, qualname: &str) -> Option<Hash128> {
        if !self.cache.contains_key(path) {
            let info = self.read(path).map(|data| FileInfo::from_bytes(path, &data));
            self.cache.insert(path.to_string(), info);
        }
        let info = self.cache[path].as_ref()?;
        match &info.kind {
            FileKind::Python { fingerprint, .. } => fingerprint.get(qualname).copied(),
            FileKind::Unparseable => None,
            FileKind::Other => (qualname == FILE).then_some(info.sha),
        }
    }
}

impl Drop for GitBaseline {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fingerprint::tests::tmp_dir;
    use crate::fingerprint::ImportStmt;

    pub(crate) fn dep(path: &str, qualname: &str, hash: u128) -> Dep {
        Dep { path: Arc::from(path), qualname: Arc::from(qualname), hash }
    }

    pub(crate) fn rec(nodeid: &str, outcome: Outcome, cost: f64, deps: Vec<Dep>) -> Record {
        Record { nodeid: nodeid.to_string(), outcome, cost, deps: Some(DepSet::of(deps)) }
    }

    fn deps_of(store: &Store, view: &StoreView, nodeid: &str) -> Vec<Dep> {
        let mut out: Vec<Dep> = store.key_ids_of(nodeid).unwrap().iter().map(|k| view.keys[k].clone()).collect();
        out.sort();
        out
    }

    fn users(store: &Store, view: &StoreView, d: &Dep) -> Vec<String> {
        let by_id: HashMap<u32, &String> = view.tests.iter().map(|(n, r)| (r.id, n)).collect();
        let key = *view.keys.iter().find(|(_, k)| *k == d).expect("live key").0;
        let mut out: Vec<String> =
            store.tests_using(&[key]).unwrap().remove(&key).unwrap().iter().map(|t| by_id[t].clone()).collect();
        out.sort();
        out
    }

    fn sorted(mut v: Vec<Dep>) -> Vec<Dep> {
        v.sort();
        v
    }

    fn module_set(view: &StoreView, nodeid: &str) -> Vec<String> {
        let mut out: Vec<String> = view.module_sets[&view.tests[nodeid].modules].iter().map(|p| p.to_string()).collect();
        out.sort();
        out
    }

    #[test]
    fn record_then_view_round_trips_and_replaces_only_tests_that_ran() {
        let root = tmp_dir("store-roundtrip");
        let mut store = Store::open(&root).unwrap();
        let a = vec![dep("a.py", "f", 1), dep("data.json", FILE, 11)];
        let b = vec![dep("b.py", "g", 2)];
        store
            .record(vec![rec("t.py::a", Outcome::Passed, 0.1, a.clone()), rec("t.py::b", Outcome::Failed, 0.2, b.clone())])
            .unwrap();
        let view = store.view().unwrap();
        assert_eq!(view.tests["t.py::a"].outcome, Outcome::Passed);
        assert_eq!((view.tests["t.py::b"].outcome, view.tests["t.py::b"].cost), (Outcome::Failed, 0.2));
        assert_eq!(deps_of(&store, &view, "t.py::a"), sorted(a.clone()));
        assert_eq!(deps_of(&store, &view, "t.py::b"), b);
        assert_eq!(module_set(&view, "t.py::a"), ["data.json"]);
        assert_eq!(view.tests["t.py::a"].digest, DepSet::of(a).digest);

        store.record(vec![rec("t.py::a", Outcome::Passed, 0.3, vec![dep("a.py", "f", 100)])]).unwrap();
        let view = store.view().unwrap();
        assert_eq!(deps_of(&store, &view, "t.py::a"), [dep("a.py", "f", 100)]);
        assert_eq!(deps_of(&store, &view, "t.py::b"), b);
        assert_eq!(view.tests["t.py::a"].cost, 0.3);
        assert!(module_set(&view, "t.py::a").is_empty());
        drop(store);
        let reopened = Store::open(&root).unwrap();
        let mut names: Vec<String> = reopened.view().unwrap().tests.into_keys().collect();
        names.sort();
        assert_eq!(names, ["t.py::a", "t.py::b"]);
    }

    #[test]
    fn the_index_follows_re_records_and_forgotten_tests() {
        let mut store = Store::open(&tmp_dir("store-index")).unwrap();
        let (shared, only_a, only_b) = (dep("s.py", "f", 1), dep("a.py", "f", 1), dep("b.py", "f", 1));
        store
            .record(vec![
                rec("t::a", Outcome::Passed, 0.0, vec![shared.clone(), only_a.clone()]),
                rec("t::b", Outcome::Passed, 0.0, vec![shared.clone(), only_b.clone()]),
            ])
            .unwrap();
        let view = store.view().unwrap();
        assert_eq!(users(&store, &view, &shared), ["t::a", "t::b"]);
        assert_eq!(users(&store, &view, &only_a), ["t::a"]);

        store.record(vec![rec("t::a", Outcome::Passed, 0.0, vec![only_a.clone(), only_b.clone()])]).unwrap();
        let view = store.view().unwrap();
        assert_eq!(users(&store, &view, &shared), ["t::b"]);
        assert_eq!(users(&store, &view, &only_b), ["t::a", "t::b"]);

        store.forget(&["t::b".to_string()]).unwrap();
        let view = store.view().unwrap();
        assert_eq!(view.tests.keys().collect::<Vec<_>>(), ["t::a"]);
        assert_eq!(sorted(view.keys.values().cloned().collect()), sorted(vec![only_a, only_b.clone()]));
        assert_eq!(users(&store, &view, &only_b), ["t::a"]);
    }

    #[test]
    fn the_index_stays_the_exact_inverse_of_every_test_s_deps() {
        let mut store = Store::open(&tmp_dir("store-inverse")).unwrap();
        let mut seed: u64 = 7;
        let mut rng = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        let universe: Vec<Dep> = (0..40).map(|k| dep(&format!("m{}.py", k % 9), &format!("f{k}"), k % 3)).collect();
        let mut model: BTreeMap<String, HashSet<Dep>> = BTreeMap::new();
        for step in 0..60 {
            if rng(5) == 0 && !model.is_empty() {
                let names: Vec<String> = model.keys().cloned().collect();
                let gone: Vec<String> = names.into_iter().filter(|_| rng(2) == 0).collect();
                store.forget(&gone).unwrap();
                gone.iter().for_each(|n| {
                    model.remove(n);
                });
            } else {
                let mut batch = Vec::new();
                for _ in 0..1 + rng(5) {
                    let nodeid = format!("t::{}", rng(12));
                    let deps: HashSet<Dep> = (0..rng(13)).map(|_| universe[rng(universe.len())].clone()).collect();
                    batch.push(rec(&nodeid, Outcome::Passed, 0.0, deps.iter().cloned().collect()));
                    model.insert(nodeid, deps);
                }
                store.record(batch).unwrap();
            }
            let view = store.view().unwrap();
            let live: HashSet<Dep> = model.values().flatten().cloned().collect();
            assert_eq!(view.keys.values().cloned().collect::<HashSet<_>>(), live, "step {step}: dead or missing keys");
            assert_eq!(view.tests.keys().cloned().collect::<HashSet<_>>(), model.keys().cloned().collect());
            for d in &live {
                let expected: Vec<String> = model.iter().filter(|(_, ds)| ds.contains(d)).map(|(n, _)| n.clone()).collect();
                assert_eq!(users(&store, &view, d), expected, "step {step}: {d:?}");
            }
            for (nodeid, ds) in &model {
                assert_eq!(deps_of(&store, &view, nodeid), sorted(ds.iter().cloned().collect()));
            }
        }
    }

    #[test]
    fn a_record_without_deps_updates_the_outcome_and_keeps_the_baseline() {
        let mut store = Store::open(&tmp_dir("store-nodeps")).unwrap();
        let deps = vec![dep("a.py", "f", 1), dep("m.py", MODULE, 2)];
        store.record(vec![rec("t::a", Outcome::Passed, 0.1, deps.clone())]).unwrap();
        let first = store.view().unwrap().tests["t::a"].clone();
        store.record(vec![Record { nodeid: "t::a".into(), outcome: Outcome::Failed, cost: 0.5, deps: None }]).unwrap();
        let view = store.view().unwrap();
        let row = &view.tests["t::a"];
        assert_eq!((row.outcome, row.cost, row.digest, row.modules), (Outcome::Failed, 0.5, DepSet::of(deps.clone()).digest, first.modules));
        assert_eq!(deps_of(&store, &view, "t::a"), sorted(deps.clone()));
        assert_eq!(users(&store, &view, &deps[0]), ["t::a"]);
    }

    #[test]
    fn the_digest_ignores_order_and_duplicates() {
        let a = vec![dep("a.py", "f", 1), dep("b.py", MODULE, 2), dep("a.py", "f", 1)];
        let b = vec![dep("b.py", MODULE, 2), dep("a.py", "f", 1)];
        assert_eq!(DepSet::of(a).digest, DepSet::of(b.clone()).digest);
        assert_ne!(DepSet::of(b).digest, DepSet::of(vec![dep("a.py", "f", 1)]).digest);
        assert_ne!(dep_hash(&dep("a.py", "f", 1)), dep_hash(&dep("a.p", "yf", 1)));
    }

    #[test]
    fn meta_round_trips() {
        let mut store = Store::open(&tmp_dir("store-meta")).unwrap();
        assert_eq!(store.meta_get("preimport").unwrap(), None);
        store.meta_set("preimport", r#"["json"]"#).unwrap();
        store.meta_set("preimport", r#"["json", "re"]"#).unwrap();
        assert_eq!(store.meta_get("preimport").unwrap().as_deref(), Some(r#"["json", "re"]"#));
    }

    #[test]
    fn file_infos_survive_a_reopen_and_replace_by_path() {
        let root = tmp_dir("store-files");
        let mut store = Store::open(&root).unwrap();
        let stat = Some(Stat { mtime_ns: 1_700_000_000_000_000_001, ctime_ns: 1_700_000_000_000_000_002, size: 42 });
        let fingerprint = HashMap::from([(Arc::from(MODULE), 1), (Arc::from("f"), 2)]);
        let imports = vec![
            ImportStmt { module: "a.b".into(), level: 0, names: vec![] },
            ImportStmt { module: "".into(), level: 1, names: vec!["x".into(), "y".into()] },
        ];
        let old = FileInfo { sha: 10, stat, kind: FileKind::Python { fingerprint, imports } };
        let new = FileInfo { sha: 20, stat: None, kind: FileKind::Unparseable };
        let data = FileInfo { sha: 30, stat: None, kind: FileKind::Other };
        store.save_file_infos(vec![(Arc::from("m.py"), old.clone()), (Arc::from("data.json"), data.clone())]).unwrap();
        store.save_file_infos(vec![(Arc::from("m.py"), new.clone())]).unwrap();
        drop(store);
        let mut store = Store::open(&root).unwrap();
        assert_eq!(store.file_infos().unwrap(), HashMap::from([(Arc::from("m.py"), new), (Arc::from("data.json"), data)]));
        store.save_file_infos(vec![(Arc::from("m.py"), old.clone())]).unwrap();
        assert_eq!(store.file_infos().unwrap()[&Arc::from("m.py")], old);
    }

    fn count(root: &Path, table: &str) -> i64 {
        let conn = Connection::open(root.join(".forktest/forktest.db")).unwrap();
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn superseded_dependency_keys_are_deleted() {
        let root = tmp_dir("store-gc");
        let mut store = Store::open(&root).unwrap();
        let mut deps = Vec::new();
        for version in 0..20 {
            deps = (0..100).map(|k| dep(&format!("m{k}.py"), "f", version)).collect();
            deps.push(dep("shared.py", "g", 7));
            store
                .record(vec![
                    rec("t.py::a", Outcome::Passed, 0.1, deps.clone()),
                    rec("t.py::b", Outcome::Passed, 0.1, deps[deps.len() - 2..].to_vec()),
                ])
                .unwrap();
        }
        let view = store.view().unwrap();
        assert_eq!(deps_of(&store, &view, "t.py::a"), sorted(deps.clone()));
        assert_eq!(deps_of(&store, &view, "t.py::b"), sorted(deps[deps.len() - 2..].to_vec()));
        assert_eq!(count(&root, "dep_keys"), 101);
        assert_eq!(count(&root, "key_tests"), 101);
        assert_eq!(count(&root, "module_sets"), 1);
    }

    #[test]
    fn a_store_with_another_schema_version_starts_empty() {
        let root = tmp_dir("store-old");
        std::fs::create_dir_all(root.join(".forktest")).unwrap();
        let old = Connection::open(root.join(".forktest/forktest.db")).unwrap();
        old.execute_batch(
            "CREATE TABLE tests(nodeid TEXT PRIMARY KEY, outcome TEXT, deps TEXT);
             CREATE TABLE stray(x);
             INSERT INTO tests VALUES ('t.py::a', 'passed', '[1]');
             PRAGMA user_version = 99;",
        )
        .unwrap();
        drop(old);
        let mut store = Store::open(&root).unwrap();
        assert!(store.view().unwrap().tests.is_empty() && store.file_infos().unwrap().is_empty());
        assert_eq!(count(&root, "sqlite_master WHERE name = 'stray'"), 0);
        store.record(vec![rec("t.py::a", Outcome::Failed, 0.2, vec![dep("a.py", "f", 2)])]).unwrap();
        let view = store.view().unwrap();
        assert_eq!(deps_of(&store, &view, "t.py::a"), [dep("a.py", "f", 2)]);
    }

    #[test]
    fn inventory_rows_replace_by_path_or_wholesale() {
        let mut store = Store::open(&tmp_dir("store-inventory")).unwrap();
        let row = |path: &str, sha: u128| InventoryRow {
            path: Arc::from(path),
            sha,
            closure: u128::MAX - sha,
            deps: vec![dep("helpers.py", "params", u128::MAX), dep("cases.json", FILE, 3)],
            nodeids: vec![format!("{path}::test_a"), format!("{path}::test_b[1]")],
            error: false,
            skipped: sha == 2,
        };
        store.put_inventory("k", vec![row("tests/b.py", 1), row("tests/a.py", 2)], true).unwrap();
        store.put_inventory("other", vec![row("tests/c.py", 3)], true).unwrap();
        assert_eq!(store.inventory("k").unwrap(), [row("tests/a.py", 2), row("tests/b.py", 1)]);
        store.put_inventory("k", vec![row("tests/b.py", 5)], false).unwrap();
        assert_eq!(store.inventory("k").unwrap(), [row("tests/a.py", 2), row("tests/b.py", 5)]);
        store.put_inventory("k", vec![row("tests/d.py", 4)], true).unwrap();
        assert_eq!(store.inventory("k").unwrap(), [row("tests/d.py", 4)]);
        assert_eq!(store.inventory("other").unwrap(), [row("tests/c.py", 3)]);
        assert!(store.inventory("none").unwrap().is_empty());
    }

    #[test]
    fn a_git_baseline_reads_files_at_the_merge_base() {
        let root = tmp_dir("store-git");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(root.join("proj")).unwrap();
        std::fs::write(root.join("proj/m.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(root.join("proj/data.json"), "{}").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("proj/m.py"), "def f():\n    return 2\n").unwrap();
        git(&["commit", "-q", "-am", "edit"]);

        let mut base = GitBaseline::new(&root.join("proj"), "main").unwrap();
        let f1 = crate::fingerprint::analyze("def f():\n    return 1\n").unwrap().0["f"];
        assert_eq!(base.hash_of("m.py", "f"), Some(f1));
        assert_eq!(base.hash_of("m.py", "gone"), None);
        assert_eq!(base.hash_of("missing.py", MODULE), None);
        assert_eq!(base.hash_of("data.json", FILE), Some(xxhash_rust::xxh3::xxh3_128(b"{}")));
    }
}
