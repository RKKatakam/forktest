//! One request from selection to events: decide what to collect, collect it, select, run, record.

use crate::events::{Done, Event, Outcome, TestResult};
use crate::fingerprint::{Dep, FILE, Fingerprints, Hash128, MODULE, RelPath};
use crate::graph::ImportGraph;
use crate::root::{config_files, is_excluded};
use crate::select::{self, Affected, Current, Reason, SinceBaseline, WorkingTree};
use crate::store::{DepSet, GitBaseline, InventoryRow, Record, Store, StoreView, dep_hash};
use crate::zygote::{Batch, Collected, Control, ForkPoint, ForkPolicy, Names, Session, SessionConfig, WorkerEvent};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Affected,
    All,
    Since(String),
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub mode: Mode,
    pub verify: bool,
    /// 0 means one per CPU.
    pub workers: usize,
    pub strict: bool,
    /// Lower the priority of the workers running the selection.
    pub nice: bool,
    pub pytest_args: Vec<String>,
    /// `None` decides from thread counts and the `forktest_fork_from` ini option.
    pub fork_from: Option<ForkPoint>,
    pub stdout_to_stderr: bool,
}

/// What the store remembers about the project's Python side between runs.
#[derive(Serialize, Deserialize, Default)]
struct ProjectMeta {
    /// Module name to root-relative path, as `sys.modules` had them after collection.
    modules: BTreeMap<String, String>,
    fixtures: BTreeSet<(String, String)>,
    preimport: Vec<String>,
    /// Top-level packages that started threads when L0 imported them.
    dropped: BTreeSet<String>,
}

/// What the store remembers per set of pytest args, to decide whether a partial collection is sound.
#[derive(Serialize, Deserialize, Default)]
struct ArgsMeta {
    fork_point: Option<String>,
    python_files: Vec<String>,
    /// Every file matching `python_files`, and every `conftest.py`, at the last full collection.
    test_files: Vec<String>,
    /// Config file to content hash at the last full collection.
    config: BTreeMap<String, String>,
}

const PROJECT_META: &str = "project";
const GRAPH_META: &str = "import_graph";

pub struct Project {
    root: PathBuf,
    python: PathBuf,
    store: Store,
    fps: Arc<Fingerprints>,
    session: Option<(SessionKey, Session)>,
    /// The session's L1 when it holds a full collection, which later requests with the same key run from as is.
    warm: Option<Warm>,
    /// Set from another thread to stop the run in progress.
    cancel: Arc<AtomicBool>,
}

/// A full collection whose L1 is still alive.
#[derive(Clone)]
pub struct Warm {
    collected: Arc<Collected>,
    /// Root-relative files whose change makes the collection stale: what L1 imported, the test files, and what
    /// collection read.
    pub inputs: Arc<HashSet<String>>,
    test_files: Arc<Vec<String>>,
    config: Arc<BTreeMap<String, String>>,
}

impl Warm {
    fn new(collected: Arc<Collected>, root: &Path, fps: &Fingerprints) -> Warm {
        let c = &*collected;
        let mut inputs: HashSet<String> = c.modules.values().cloned().collect();
        inputs.extend(c.items.iter().map(|i| i.path.clone()));
        inputs.extend(c.errors.iter().map(|(n, _)| test_file(n).to_string()));
        inputs.extend(c.skips.iter().map(|n| test_file(n).to_string()));
        for (file, (names, reads)) in &c.per_file {
            inputs.insert(file.clone());
            inputs.extend(names.iter().map(|(p, _)| p.clone()));
            inputs.extend(reads.iter().cloned());
        }
        Warm {
            test_files: Arc::new(test_files(root, &c.python_files)),
            config: Arc::new(config_hashes(root, fps)),
            inputs: Arc::new(inputs),
            collected,
        }
    }

    pub fn tests(&self) -> usize {
        self.collected.items.len()
    }

    /// Project modules L1 imported.
    pub fn modules(&self) -> usize {
        self.collected.modules.len()
    }

    pub fn fork_point(&self) -> ForkPoint {
        self.collected.fork_point
    }
}

#[derive(PartialEq)]
struct SessionKey {
    args: Vec<String>,
    policy: ForkPolicy,
    stdout_to_stderr: bool,
}

fn log(msg: impl FnOnce() -> String) {
    if std::env::var_os("FORKTEST_LOG").is_some_and(|v| !v.is_empty() && v != "0") {
        eprintln!("forktest: {}", msg());
    }
}

fn ms(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

fn test_file(nodeid: &str) -> &str {
    nodeid.split("::").next().unwrap_or(nodeid)
}

fn hex(h: u128) -> String {
    format!("{h:032x}")
}

fn args_key(args: &[String]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(args.join("\0").as_bytes()))
}

fn read_meta<T: for<'de> Deserialize<'de> + Default>(store: &Store, key: &str) -> anyhow::Result<T> {
    Ok(store.meta_get(key)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

fn graph_of(root: &Path, store: &Store, fps: &Arc<Fingerprints>, modules: &BTreeMap<String, String>) -> anyhow::Result<ImportGraph> {
    let known = modules.iter().map(|(k, v)| (k.clone(), RelPath::from(v.as_str()))).collect();
    Ok(ImportGraph::reusing(store.meta_get(GRAPH_META)?.as_deref(), root, fps.clone(), known))
}

fn fixture_set(meta: &ProjectMeta) -> HashSet<(RelPath, Arc<str>)> {
    meta.fixtures.iter().map(|(p, q)| (RelPath::from(p.as_str()), Arc::from(q.as_str()))).collect()
}

/// Every reason `select` would have for one test now, for `--why`. Starts no Python.
pub fn explain(root: &Path, nodeid: &str) -> anyhow::Result<Vec<(Reason, Option<Dep>)>> {
    let store = Store::open(root)?;
    let fps = Arc::new(Fingerprints::new(root, store.file_infos()?));
    let meta: ProjectMeta = read_meta(&store, PROJECT_META)?;
    let graph = graph_of(root, &store, &fps, &meta.modules)?;
    let view = store.view()?;
    select::explain(nodeid, &view, &store, &mut WorkingTree::new(&fps), &graph, &fixture_set(&meta))
}

/// Unix shell-style wildcards on a file name, as pytest's `fnmatch` on `python_files`.
fn fnmatch(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let Some(close) = p.iter().skip(2).position(|&c| c == ']').map(|i| i + 2) else {
                    return n.first() == Some(&'[') && go(&p[1..], &n[1..]);
                };
                let Some(&c) = n.first() else { return false };
                let (negate, set) = match p[1] {
                    '!' => (true, &p[2..close]),
                    _ => (false, &p[1..close]),
                };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        hit |= set[i] <= c && c <= set[i + 2];
                        i += 3;
                    } else {
                        hit |= set[i] == c;
                        i += 1;
                    }
                }
                hit != negate && go(&p[close + 1..], &n[1..])
            }
            Some(&c) => n.first() == Some(&c) && go(&p[1..], &n[1..]),
        }
    }
    go(&p, &n)
}

/// Files pytest could collect as test modules, and every `conftest.py`, under the root. Directories pytest skips by
/// default (hidden ones, virtualenvs, caches) are skipped.
fn test_files(root: &Path, patterns: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&dir)) else { continue };
        for e in entries.flatten() {
            let (Ok(kind), Ok(name)) = (e.file_type(), e.file_name().into_string()) else { continue };
            let rel = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
            if kind.is_dir() {
                if !name.starts_with('.') && !is_excluded(&rel) {
                    stack.push(rel);
                }
            } else if name.ends_with(".py") && (name == "conftest.py" || patterns.iter().any(|p| fnmatch(p, &name))) {
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

fn config_hashes(root: &Path, fps: &Fingerprints) -> BTreeMap<String, String> {
    config_files(root).into_iter().filter_map(|p| fps.info(&p).map(|i| (p, hex(i.sha)))).collect()
}

/// Why every test has to be collected, or `None` when the stored inventory can be trusted for the files it holds.
fn full_collection_reason(
    req: &Request,
    root: &Path,
    fps: &Fingerprints,
    inventory: &[InventoryRow],
    meta: &ArgsMeta,
) -> Option<&'static str> {
    if inventory.is_empty() || meta.python_files.is_empty() {
        return Some("no inventory for these arguments");
    }
    if matches!(req.mode, Mode::All) {
        return Some("--all");
    }
    if req.verify {
        return Some("--verify-selection");
    }
    if config_hashes(root, fps) != meta.config {
        return Some("a config file changed");
    }
    if test_files(root, &meta.python_files) != meta.test_files {
        return Some("a test file or conftest.py was added or removed");
    }
    None
}

/// Turns a test's recorded names and reads into dependencies. Each distinct name is resolved and hashed once per
/// run, and a test whose digest matches the stored one never has its full set built.
struct Deps<'a> {
    fps: &'a Fingerprints,
    graph: &'a ImportGraph,
    config: Vec<Dep>,
    base: HashMap<String, Arc<Base>>,
    names: Vec<Option<Option<(Dep, Hash128)>>>,
    reads: HashMap<String, Option<(Dep, Hash128)>>,
}

/// What every test of one file depends on: the config files and the `<module>` of its whole import closure.
struct Base {
    deps: HashSet<Dep>,
    digest: Hash128,
}

fn hashed(dep: Option<Dep>) -> Option<(Dep, Hash128)> {
    dep.map(|d| {
        let h = dep_hash(&d);
        (d, h)
    })
}

impl<'a> Deps<'a> {
    fn new(root: &Path, fps: &'a Fingerprints, graph: &'a ImportGraph) -> Deps<'a> {
        let config = config_files(root).iter().filter_map(|p| fps.dep(p, FILE)).collect();
        Deps { fps, graph, config, base: HashMap::new(), names: Vec::new(), reads: HashMap::new() }
    }

    fn base(&mut self, test_file: &str) -> Arc<Base> {
        if let Some(b) = self.base.get(test_file) {
            return b.clone();
        }
        let mut deps: HashSet<Dep> = self.config.iter().cloned().collect();
        deps.extend(self.graph.closure(test_file).iter().filter_map(|p| self.fps.dep(p, MODULE)));
        let digest = deps.iter().fold(0u128, |acc, d| acc.wrapping_add(dep_hash(d)));
        let base = Arc::new(Base { deps, digest });
        self.base.insert(test_file.to_string(), base.clone());
        base
    }

    /// `None` when the test's dependency set has the `stored` digest.
    fn of(&mut self, test_file: &str, hits: &[u32], reads: &[String], names: &Names, stored: Option<Hash128>) -> Option<DepSet> {
        let base = self.base(test_file);
        let fps = self.fps;
        let mut extra: HashMap<&Dep, Hash128> = HashMap::new();
        for &id in hits {
            let i = id as usize;
            if self.names.len() <= i {
                self.names.resize(i + 1, None);
            }
            self.names[i].get_or_insert_with(|| {
                let (path, qual) = &names.list[i];
                hashed(fps.dep(path, qual))
            });
        }
        for path in reads {
            self.reads.entry(path.clone()).or_insert_with(|| hashed(fps.dep(path, FILE)));
        }
        let resolved = hits.iter().filter_map(|&id| self.names[id as usize].as_ref().and_then(Option::as_ref));
        for (dep, h) in resolved.chain(reads.iter().filter_map(|p| self.reads[p].as_ref())) {
            if !base.deps.contains(dep) {
                extra.insert(dep, *h);
            }
        }
        let digest = extra.values().fold(base.digest, |acc, h| acc.wrapping_add(*h));
        if stored == Some(digest) {
            return None;
        }
        Some(DepSet::of(base.deps.iter().cloned().chain(extra.into_keys().cloned()).collect()))
    }
}

#[derive(Default)]
struct Tally {
    passed: u64,
    failed: u64,
    errors: u64,
    skipped: u64,
    xfailed: u64,
    xpassed: u64,
}

impl Tally {
    fn add(&mut self, outcome: Outcome) {
        *match outcome {
            Outcome::Passed => &mut self.passed,
            Outcome::Failed => &mut self.failed,
            Outcome::Error => &mut self.errors,
            Outcome::Skipped => &mut self.skipped,
            Outcome::Xfailed => &mut self.xfailed,
            Outcome::Xpassed => &mut self.xpassed,
        } += 1;
    }
}

/// What one run learned, written to the store whether or not the run got to the end.
#[derive(Default)]
struct Learned {
    records: Vec<Record>,
    forget: Vec<String>,
    inventory: Option<(Vec<InventoryRow>, bool)>,
    project: Option<ProjectMeta>,
    args: Option<ArgsMeta>,
    graph: Option<String>,
}

impl Project {
    pub fn open(root: &Path, python: PathBuf) -> anyhow::Result<Project> {
        let store = Store::open(root)?;
        let fps = Arc::new(Fingerprints::new(root, store.file_infos()?));
        let cancel = Arc::new(AtomicBool::new(false));
        Ok(Project { root: root.to_path_buf(), python, store, fps, session: None, warm: None, cancel })
    }

    pub fn python(&self) -> &Path {
        &self.python
    }

    pub fn fingerprints(&self) -> &Fingerprints {
        &self.fps
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Runs one request, emitting its events, and returns pytest's exit code when it stopped before reporting.
    pub fn run(&mut self, req: &Request, emit: &mut dyn FnMut(&Event)) -> anyhow::Result<i32> {
        let mut learned = Learned::default();
        let result = self.pipeline(req, emit, &mut learned);
        if req.strict || self.l1_pid().is_none() {
            self.warm = None;
        }
        let saved = self.save(&args_key(&req.pytest_args), learned);
        let code = result?;
        saved?;
        Ok(code)
    }

    /// Collects everything these args collect and keeps L1, so a later `run` of the same args collects nothing.
    pub fn warm_up(&mut self, req: &Request) -> anyhow::Result<&Warm> {
        self.warm = None;
        let meta: ProjectMeta = read_meta(&self.store, PROJECT_META)?;
        let session = self.session_for(req, full_policy(req), &meta)?;
        let collected = session.collect(None)?;
        if let Some(code) = collected.exit_code {
            anyhow::bail!("pytest exited with code {code} before running tests");
        }
        let warm = Warm::new(Arc::new(collected), &self.root, &self.fps);
        self.store.save_file_infos(self.fps.take_fresh())?;
        Ok(self.warm.insert(warm))
    }

    pub fn warm(&self) -> Option<&Warm> {
        self.warm.as_ref()
    }

    /// Kills L1 and keeps L0.
    pub fn drop_warm(&mut self) {
        self.warm = None;
        if let Some((_, session)) = &mut self.session {
            session.drop_l1();
        }
    }

    /// Kills every Python process, so the next run starts from a fresh interpreter.
    pub fn restart(&mut self) {
        self.warm = None;
        self.session = None;
    }

    pub fn l1_pid(&self) -> Option<i32> {
        self.session.as_ref().and_then(|(_, s)| s.l1_pid())
    }

    pub fn preimported(&self) -> usize {
        self.session.as_ref().map_or(0, |(_, s)| s.preimported())
    }

    /// The warm collection when it was made for this request's session.
    fn warm_for(&self, req: &Request) -> Option<Warm> {
        let (key, _) = self.session.as_ref()?;
        (*key == session_key(req, full_policy(req))).then(|| self.warm.clone()).flatten()
    }

    fn save(&mut self, key: &str, learned: Learned) -> anyhow::Result<()> {
        let store = &mut self.store;
        store.record(learned.records)?;
        store.forget(&learned.forget)?;
        if let Some((rows, full)) = learned.inventory {
            store.put_inventory(key, rows, full)?;
        }
        store.save_file_infos(self.fps.take_fresh())?;
        if let Some(graph) = learned.graph {
            store.meta_set(GRAPH_META, &graph)?;
        }
        if let Some(meta) = learned.project {
            store.meta_set(PROJECT_META, &serde_json::to_string(&meta)?)?;
        }
        if let Some(meta) = learned.args {
            store.meta_set(&format!("args:{key}"), &serde_json::to_string(&meta)?)?;
        }
        Ok(())
    }

    fn current<'a>(&self, req: &Request, fps: &'a Fingerprints) -> anyhow::Result<Box<dyn Current + 'a>> {
        Ok(match &req.mode {
            Mode::Since(r) => Box::new(SinceBaseline::new(GitBaseline::new(&self.root, r)?, fps)),
            _ => Box::new(WorkingTree::new(fps)),
        })
    }

    fn pipeline(&mut self, req: &Request, emit: &mut dyn FnMut(&Event), learned: &mut Learned) -> anyhow::Result<i32> {
        let t0 = Instant::now();
        let root = self.root.clone();
        let key = args_key(&req.pytest_args);
        let fps = self.fps.clone();
        let mut project: ProjectMeta = read_meta(&self.store, PROJECT_META)?;
        let mut args_meta: ArgsMeta = read_meta(&self.store, &format!("args:{key}"))?;
        let inventory = self.store.inventory(&key)?;
        let view = self.store.view()?;
        let mut current = self.current(req, &fps)?;
        let graph = graph_of(&root, &self.store, &fps, &project.modules)?;

        let warm = self.warm_for(req);
        let full = match &warm {
            Some(_) => Some("warm"),
            None => full_collection_reason(req, &root, &fps, &inventory, &args_meta),
        };
        let files: Option<Vec<String>> = match full {
            Some("warm") => None,
            Some(why) => {
                log(|| format!("full collection: {why}"));
                None
            }
            None => {
                let universe: Vec<String> = inventory.iter().flat_map(|r| r.nodeids.iter().cloned()).collect();
                let path_of: HashMap<&str, &str> =
                    inventory.iter().flat_map(|r| r.nodeids.iter().map(|n| (n.as_str(), &*r.path))).collect();
                let affected = select::select(&universe, &view, &self.store, &mut *current, &graph, &fixture_set(&project))?;
                let mut files: BTreeSet<String> =
                    affected.reasons.keys().map(|n| path_of.get(n.as_str()).copied().unwrap_or(test_file(n)).to_string()).collect();
                for row in &inventory {
                    if let Some(why) = select::recollect(row, &fps, &graph) {
                        log(|| format!("re-collecting {}: {why}", row.path));
                        files.insert(row.path.to_string());
                    }
                }
                if files.is_empty() {
                    log(|| "no python started".into());
                    learned.graph = graph.export();
                    emit(&Event::Selection {
                        changed: affected.changed,
                        selected: 0,
                        total: universe.len() as u64,
                        reasons: BTreeMap::new(),
                        duration_ms: ms(t0),
                    });
                    emit(&Event::Done(Done { duration_ms: ms(t0), ..Done::default() }));
                    return Ok(0);
                }
                Some(files.into_iter().collect())
            }
        };

        let (collected, warm) = match warm {
            Some(warm) => {
                log(|| format!("served by the warm collection of {} tests", warm.tests()));
                (warm.collected.clone(), warm)
            }
            None => {
                if let Some(files) = &files {
                    log(|| format!("collecting {} files: {}", files.len(), files.join(" ")));
                }
                let policy = match (req.fork_from, full, args_meta.fork_point.as_deref()) {
                    (Some(p), _, _) => ForkPolicy::Force(p),
                    // A partial run trusts where the last run of these args could fork from; a full one checks again.
                    (None, None, Some("imported")) => ForkPolicy::Force(ForkPoint::Imported),
                    (None, None, Some("spawn")) => ForkPolicy::Force(ForkPoint::Spawn),
                    _ => ForkPolicy::Auto,
                };
                self.warm = None;
                let session = self.session_for(req, policy, &project)?;
                let collected = Arc::new(session.collect(files.as_deref())?);
                if let Some(code) = collected.exit_code {
                    return Ok(code);
                }
                log(|| format!("collected {} tests; workers fork from {}", collected.items.len(), fork_point_name(collected.fork_point)));
                if std::fs::canonicalize(&collected.rootdir).is_ok_and(|r| r != root) {
                    eprintln!(
                        "forktest: pytest's rootdir is {} but forktest's is {}; affected-test selection may be off",
                        collected.rootdir.display(),
                        root.display()
                    );
                }
                let warm = Warm::new(collected.clone(), &root, &fps);
                if files.is_none() {
                    self.warm = Some(warm.clone());
                }
                (collected, warm)
            }
        };
        let session = &mut self.session.as_mut().expect("collected above").1;

        let partial = files.is_some();
        let stored_modules = project.modules.clone();
        merge_project_meta(&mut project, &collected, session.dropped_preimports(), partial);
        let collected = &*collected;
        let rebuilt;
        let graph = if partial && project.modules == stored_modules {
            &graph
        } else {
            rebuilt = graph_of(&root, &self.store, &fps, &project.modules)?;
            &rebuilt
        };
        if !partial {
            args_meta.python_files = collected.python_files.clone();
            args_meta.test_files = warm.test_files.to_vec();
            args_meta.config = (*warm.config).clone();
        }
        let fixtures = fixture_set(&project);
        learned.project = Some(project);
        learned.args = Some(args_meta);

        let (rows, universe) = self.update_inventory(collected, &inventory, files.as_deref(), &view, graph, learned);
        learned.inventory = Some(rows);
        learned.graph = graph.export();

        let mut collect_errors: HashSet<String> = HashSet::new();
        for (nodeid, message) in &collected.errors {
            collect_errors.insert(nodeid.clone());
            emit(&Event::CollectError { nodeid: nodeid.clone(), message: message.clone() });
        }
        if !collected.errors.is_empty() && !req.pytest_args.iter().any(|a| a == "--continue-on-collection-errors") {
            emit(&Event::Done(Done { errors: collected.errors.len() as u64, duration_ms: ms(t0), ..Done::default() }));
            return Ok(0);
        }

        let Affected { mut reasons, changed } = match req.mode {
            Mode::All => Affected { reasons: universe.iter().map(|n| (n.clone(), Reason::All)).collect(), changed: Vec::new() },
            _ => select::select(&universe, &view, &self.store, &mut *current, graph, &fixtures)?,
        };
        let path_of: HashMap<&str, &str> = collected.items.iter().map(|i| (i.nodeid.as_str(), i.path.as_str())).collect();
        reasons.retain(|n, _| path_of.contains_key(n.as_str()));
        let (selected, rest): (Vec<&str>, Vec<&str>) =
            collected.items.iter().map(|i| i.nodeid.as_str()).partition(|n| reasons.contains_key(*n));
        emit(&Event::Selection {
            changed,
            selected: selected.len() as u64,
            total: universe.len() as u64,
            reasons: select::reason_counts(&reasons),
            duration_ms: ms(t0),
        });

        let mut run = Execution {
            root: &root,
            fps: &fps,
            graph,
            view: &view,
            path_of: &path_of,
            maxfail: collected.maxfail,
            collect_errors,
            req,
            cancel: &self.cancel,
        };
        let session = &mut self.session.as_mut().expect("started above").1;
        let mut tally = Tally::default();
        let executed = run.execute(session, &selected, req.nice, learned, emit, &mut |r| tally.add(r.outcome));
        if let Some(args) = &mut learned.args {
            args.fork_point = Some(fork_point_name(session.fork_point()).into());
        }
        executed?;
        if self.cancel.load(Ordering::Relaxed) {
            return Ok(0);
        }
        let collect_skips = if matches!(req.mode, Mode::All) { collected.skips.len() as u64 } else { 0 };
        emit(&Event::Done(Done {
            passed: tally.passed,
            failed: tally.failed,
            errors: tally.errors,
            skipped: tally.skipped + collect_skips,
            xfailed: tally.xfailed,
            xpassed: tally.xpassed,
            duration_ms: ms(t0),
        }));
        if !req.verify {
            return Ok(0);
        }
        let t1 = Instant::now();
        if req.strict {
            session.collect(None)?;
        }
        let mut misses = 0;
        let mut on_unselected = |e: &Event| {
            if let Event::Result(r) = e
                && r.outcome.is_failure()
            {
                misses += 1;
                emit(&Event::Miss { nodeid: r.nodeid.clone(), message: r.message.clone().unwrap_or_default() });
            }
        };
        run.execute(session, &rest, true, learned, &mut on_unselected, &mut |_| {})?;
        if self.cancel.load(Ordering::Relaxed) {
            return Ok(0);
        }
        emit(&Event::Verified { checked: rest.len() as u64, misses, duration_ms: ms(t1) });
        Ok(0)
    }

    /// The warm session for these args, or a new one. Other args only need a new L1.
    fn session_for(&mut self, req: &Request, policy: ForkPolicy, meta: &ProjectMeta) -> anyhow::Result<&mut Session> {
        let key = session_key(req, policy);
        if let Some((k, session)) = &mut self.session
            && *k != key
            && (k.policy, k.stdout_to_stderr) == (key.policy, key.stdout_to_stderr)
        {
            self.warm = None;
            session.set_args(key.args.clone());
            k.args = key.args.clone();
        }
        if self.session.as_ref().is_none_or(|(k, _)| *k != key) {
            self.warm = None;
            self.session = None;
            let preimport = meta
                .preimport
                .iter()
                .filter(|m| !meta.dropped.contains(m.split('.').next().unwrap_or(m)))
                .cloned()
                .collect();
            let session = Session::start(SessionConfig {
                root: self.root.clone(),
                python: self.python.clone(),
                pytest_args: req.pytest_args.clone(),
                preimport,
                policy,
                stdout_to_stderr: req.stdout_to_stderr,
            })?;
            self.session = Some((key, session));
        }
        Ok(&mut self.session.as_mut().expect("just set").1)
    }

    /// The inventory rows after this collection, and the nodeid universe in collection order. Records which tests
    /// vanished.
    fn update_inventory(
        &self,
        collected: &Collected,
        previous: &[InventoryRow],
        requested: Option<&[String]>,
        view: &StoreView,
        graph: &ImportGraph,
        learned: &mut Learned,
    ) -> ((Vec<InventoryRow>, bool), Vec<String>) {
        let fps = &*self.fps;
        let mut per_file: Vec<&str> = collected.per_file.keys().map(String::as_str).collect();
        per_file.sort();
        let mut seen: HashSet<&str> = HashSet::new();
        let order: Vec<&str> = collected
            .items
            .iter()
            .map(|i| i.path.as_str())
            .chain(per_file)
            .chain(collected.errors.iter().map(|(n, _)| test_file(n)))
            .chain(collected.skips.iter().map(|n| test_file(n)))
            .chain(requested.into_iter().flatten().map(String::as_str))
            .filter(|p| seen.insert(p))
            .collect();
        let mut nodeids: HashMap<&str, Vec<String>> = HashMap::new();
        for item in &collected.items {
            nodeids.entry(&item.path).or_default().push(item.nodeid.clone());
        }
        let errors: HashSet<&str> = collected.errors.iter().map(|(n, _)| test_file(n)).collect();
        let skips: HashSet<&str> = collected.skips.iter().map(|n| test_file(n)).collect();
        let mut rows: Vec<InventoryRow> = Vec::new();
        let mut unattributed = false;
        for path in order {
            let Some((rel, info)) = fps.entry(path).filter(|_| !path.starts_with('/') && !is_excluded(path)) else {
                unattributed |= errors.contains(path);
                continue;
            };
            let deps = collected.per_file.get(path).map_or_else(Vec::new, |(names, reads)| {
                names.iter().filter_map(|(p, q)| fps.dep(p, q)).chain(reads.iter().filter_map(|p| fps.dep(p, FILE))).collect()
            });
            rows.push(InventoryRow {
                path: rel,
                sha: info.sha,
                closure: select::closure_digest(path, fps, graph),
                deps,
                nodeids: nodeids.remove(path).unwrap_or_default(),
                error: errors.contains(path),
                skipped: skips.contains(path),
            });
        }

        let recollected: HashSet<&str> = rows.iter().map(|r| &*r.path).collect();
        let now: HashSet<&str> = rows.iter().flat_map(|r| r.nodeids.iter().map(String::as_str)).collect();
        let mut gone: Vec<String> = previous
            .iter()
            .filter(|r| requested.is_none() || recollected.contains(&*r.path))
            .flat_map(|r| r.nodeids.iter())
            .filter(|n| !now.contains(n.as_str()))
            .cloned()
            .collect();
        let mut exists: HashMap<&str, bool> = HashMap::new();
        gone.extend(
            view.tests
                .keys()
                .filter(|n| !*exists.entry(test_file(n)).or_insert_with(|| self.root.join(test_file(n)).exists()))
                .cloned(),
        );
        learned.forget = gone;

        let universe: Vec<String> = match requested {
            None => collected.items.iter().map(|i| i.nodeid.clone()).collect(),
            Some(_) => {
                let mut merged: BTreeMap<RelPath, &InventoryRow> = previous.iter().map(|r| (r.path.clone(), r)).collect();
                for r in &rows {
                    merged.insert(r.path.clone(), r);
                }
                merged.values().filter(|r| self.root.join(&*r.path).exists()).flat_map(|r| r.nodeids.iter().cloned()).collect()
            }
        };
        if unattributed {
            // An error pytest cannot pin on one file (a broken directory conftest) is only seen again in full.
            return ((Vec::new(), true), universe);
        }
        ((rows, requested.is_none()), universe)
    }
}

fn session_key(req: &Request, policy: ForkPolicy) -> SessionKey {
    SessionKey { args: req.pytest_args.clone(), policy, stdout_to_stderr: req.stdout_to_stderr }
}

/// Where a full collection may fork from: anywhere its thread counts allow, unless the request says.
fn full_policy(req: &Request) -> ForkPolicy {
    req.fork_from.map_or(ForkPolicy::Auto, ForkPolicy::Force)
}

fn fork_point_name(p: ForkPoint) -> &'static str {
    match p {
        ForkPoint::Collected => "collected",
        ForkPoint::Imported => "imported",
        ForkPoint::Spawn => "spawn",
    }
}

/// A partial collection saw only some files, so what it found adds to what the last full one found.
fn merge_project_meta(meta: &mut ProjectMeta, collected: &Collected, dropped: &[String], partial: bool) {
    if !partial {
        meta.modules.clear();
        meta.fixtures.clear();
        meta.preimport.clear();
    }
    meta.modules.extend(collected.modules.iter().map(|(k, v)| (k.clone(), v.clone())));
    meta.fixtures.extend(collected.fixtures.iter().cloned());
    let preimport: BTreeSet<String> = meta.preimport.drain(..).chain(collected.third_party.iter().cloned()).collect();
    meta.preimport = preimport.into_iter().collect();
    meta.dropped.extend(dropped.iter().cloned());
}

/// Runs lists of tests through the session and turns what they report into events and records.
struct Execution<'a> {
    root: &'a Path,
    fps: &'a Fingerprints,
    graph: &'a ImportGraph,
    view: &'a StoreView,
    path_of: &'a HashMap<&'a str, &'a str>,
    maxfail: u32,
    collect_errors: HashSet<String>,
    req: &'a Request,
    cancel: &'a AtomicBool,
}

impl Execution<'_> {
    fn execute(
        &mut self,
        session: &mut Session,
        nodeids: &[&str],
        nice: bool,
        learned: &mut Learned,
        emit: &mut dyn FnMut(&Event),
        on_result: &mut dyn FnMut(&TestResult),
    ) -> anyhow::Result<()> {
        if nodeids.is_empty() {
            return Ok(());
        }
        let files: HashSet<&str> = nodeids.iter().map(|n| self.path_of.get(n).copied().unwrap_or(test_file(n))).collect();
        let closures: HashSet<RelPath> = files.iter().flat_map(|f| self.graph.closure(f).iter().cloned().collect::<Vec<_>>()).collect();
        self.fps.prefetch(closures.iter().map(|p| &**p).collect::<Vec<_>>());
        let mut deps = Deps::new(self.root, self.fps, self.graph);
        let mut names = Names::default();
        let mut failures = 0u32;
        let maxfail = self.maxfail;
        let (view, path_of, collect_errors) = (self.view, self.path_of, &mut self.collect_errors);
        let records = &mut learned.records;
        let mut report = |result: TestResult, emit: &mut dyn FnMut(&Event)| {
            on_result(&result);
            emit(&Event::Result(result));
        };
        let mut sink = |event: WorkerEvent, names: &Names| -> Control {
            match event {
                WorkerEvent::Result { result, cost, hits, reads, .. } => {
                    let file = path_of.get(result.nodeid.as_str()).copied().unwrap_or(test_file(&result.nodeid));
                    let stored = view.tests.get(&result.nodeid).map(|t| t.digest);
                    let set = deps.of(file, &hits, &reads, names, stored);
                    records.push(Record { nodeid: result.nodeid.clone(), outcome: result.outcome, cost, deps: set });
                    let failed = result.outcome.is_failure();
                    report(result, emit);
                    if failed {
                        failures += 1;
                        if maxfail > 0 && failures >= maxfail {
                            return Control::Stop;
                        }
                    }
                }
                WorkerEvent::CollectError { nodeid, message, .. } => {
                    if collect_errors.insert(nodeid.clone()) {
                        emit(&Event::CollectError { nodeid, message });
                    }
                }
                WorkerEvent::Lost { nodeids, message, .. } => {
                    for nodeid in nodeids {
                        let result = TestResult {
                            nodeid,
                            outcome: Outcome::Error,
                            duration: 0.0,
                            file: None,
                            line: None,
                            message: Some(message.clone()),
                            trace: Vec::new(),
                        };
                        report(result, emit);
                    }
                }
            }
            Control::Continue
        };
        if self.req.strict {
            return session.run_in_process(nodeids.iter().map(|n| n.to_string()).collect(), &mut names, &mut sink);
        }
        let workers = match self.req.workers {
            0 => std::thread::available_parallelism().map_or(2, |n| n.get()),
            n => n,
        };
        let items: Vec<(&str, f64)> = nodeids.iter().map(|n| (*n, view.tests.get(*n).map_or(0.01, |t| t.cost))).collect();
        let failed: HashSet<&str> =
            view.tests.iter().filter(|(_, t)| t.outcome.is_failure()).map(|(n, _)| n.as_str()).collect();
        let batches = crate::schedule::schedule(&items, &failed, workers)
            .into_iter()
            .map(|batch| {
                let nodeids: Vec<String> = batch.iter().map(|&i| items[i].0.to_string()).collect();
                let mut files: Vec<String> = Vec::new();
                for n in &nodeids {
                    let f = path_of.get(n.as_str()).copied().unwrap_or(test_file(n)).to_string();
                    if !files.contains(&f) {
                        files.push(f);
                    }
                }
                Batch { nodeids, files }
            })
            .collect();
        session.run(batches, nice, &mut names, self.cancel, &mut sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_whose_digest_is_stored_is_not_built_again() {
        let root = crate::graph::tests::project("run-digest");
        let fps = Fingerprints::new(&root, HashMap::new());
        let graph = ImportGraph::new(&root, Arc::new(Fingerprints::new(&root, HashMap::new())), HashMap::new());
        let mut deps = Deps::new(&root, &fps, &graph);
        let names = Names::default();
        let reads = ["data.json".to_string(), "pyproject.toml".to_string()];
        let set = deps.of("tests/test_core.py", &[], &reads, &names, None).expect("nothing stored");
        assert!(set.deps.iter().any(|d| &*d.path == "data.json" && &*d.qualname == FILE));
        assert!(set.deps.iter().any(|d| &*d.path == "pkg/core.py" && &*d.qualname == MODULE));
        assert!(deps.of("tests/test_core.py", &[], &reads, &names, Some(set.digest)).is_none());
        assert!(deps.of("tests/test_core.py", &[], &reads[..1], &names, Some(set.digest)).is_none(), "pyproject.toml is in every base");
        assert!(deps.of("tests/test_core.py", &[], &[], &names, Some(set.digest)).is_some());
    }

    #[test]
    fn fnmatch_as_python() {
        assert!(fnmatch("test_*.py", "test_a.py"));
        assert!(fnmatch("*_test.py", "a_test.py"));
        assert!(!fnmatch("test_*.py", "atest_a.py"));
        assert!(fnmatch("check_?.py", "check_1.py"));
        assert!(fnmatch("t[a-c].py", "tb.py"));
        assert!(!fnmatch("t[!a-c].py", "tb.py"));
        assert!(fnmatch("t[!a-c].py", "td.py"));
    }

    #[test]
    fn test_files_skip_hidden_and_excluded_dirs() {
        let root = crate::fingerprint::tests::tmp_dir("walk");
        for rel in ["tests/test_a.py", "tests/helper.py", "conftest.py", ".hidden/test_h.py", ".venv/test_v.py", "pkg/b_test.py"] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        let patterns = vec!["test_*.py".to_string(), "*_test.py".to_string()];
        assert_eq!(test_files(&root, &patterns), ["conftest.py", "pkg/b_test.py", "tests/test_a.py"]);
    }
}
