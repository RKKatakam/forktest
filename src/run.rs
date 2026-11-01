//! One request from selection to events: decide what to collect, collect it, select, run, record.

use crate::events::{Done, Event, Outcome, TestResult};
use crate::replay::{self, InOrder};
use crate::fingerprint::{Dep, FILE, Fingerprints, Hash128, MODULE, RelPath};
use crate::graph::ImportGraph;
use crate::root::{config_files, is_excluded};
use crate::select::{self, Affected, Current, Reason, SinceBaseline, WorkingTree};
use crate::store::{DepSet, GitBaseline, InventoryRow, Record, Store, StoreView, dep_hash};
use crate::zygote::{
    Batch, Collected, CollectedItem, Control, ForkPoint, ForkPolicy, Names, Partial, ReporterEnd, Session, SessionConfig,
    Terminal, ToReporter, WorkerEvent,
};
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
    /// Where pytest's reporter writes its JUnit XML.
    #[serde(default)]
    pub junitxml: Option<PathBuf>,
    /// The asking terminal's variables pytest's colour and width depend on, `None` for unset.
    #[serde(default)]
    pub terminal_env: Vec<(String, Option<String>)>,
}

/// The environment variables pytest's terminal output depends on.
pub const TERMINAL_ENV: &[&str] = &["TERM", "COLUMNS", "LINES", "NO_COLOR", "FORCE_COLOR", "PY_COLORS"];

impl Request {
    fn reporter_args(&self) -> Vec<String> {
        let mut args = self.pytest_args.clone();
        if let Some(path) = &self.junitxml {
            args.push(format!("--junitxml={}", path.display()));
        }
        args
    }
}

/// What the store remembers about the project's Python side between runs.
#[derive(Serialize, Deserialize, Default)]
struct ProjectMeta {
    /// Module name to root-relative path, as `sys.modules` had them after collection.
    modules: BTreeMap<String, String>,
    fixtures: BTreeSet<(String, String)>,
    preimport: Vec<String>,
    /// Top-level packages that started threads, or imported a plugin pytest rewrites, when L0 imported them.
    dropped: BTreeSet<String>,
    /// Modules pytest marked to rewrite, which L0 must leave for pytest to import.
    #[serde(default)]
    avoid: BTreeSet<String>,
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

/// The interpreter cannot run the shim. Nothing has been emitted, so the caller can hand the run to plain pytest.
#[derive(Debug)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

pub struct Project {
    root: PathBuf,
    python: PathBuf,
    store: Store,
    fps: Arc<Fingerprints>,
    session: Option<(SessionKey, Session)>,
    /// The session's L1 when it holds a full collection, which later requests with the same key run from as is.
    warm: Option<Warm>,
    /// What `Warm::seen` will hold for the collection `start_warm_up` started.
    warming: HashMap<String, Hash128>,
    /// A background collection failed with preimports, which are now forgotten; the next one should start at once.
    healed: bool,
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
    /// Content hashes of inputs taken before L1 started, so a write that leaves one as it was keeps the collection.
    seen: Arc<HashMap<String, Hash128>>,
}

impl Warm {
    fn new(collected: Arc<Collected>, root: &Path, fps: &Fingerprints, mut seen: HashMap<String, Hash128>) -> Warm {
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
        seen.retain(|p, _| inputs.contains(p));
        Warm {
            test_files: Arc::new(test_files(root, &c.python_files)),
            config: Arc::new(config_hashes(root, fps)),
            inputs: Arc::new(inputs),
            seen: Arc::new(seen),
            collected,
        }
    }

    /// Whether a reported change to `rel` makes the collection stale: an input whose content differs from what it
    /// was before L1 started, or one with no hash from then.
    pub fn stale_after_change(&self, rel: &str, fps: &Fingerprints) -> bool {
        self.inputs.contains(rel) && self.seen.get(rel).is_none_or(|h| fps.info(rel).is_none_or(|i| i.sha != *h))
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

pub fn log(msg: impl FnOnce() -> String) {
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
        Ok(Project { root: root.to_path_buf(), python, store, fps, session: None, warm: None, warming: HashMap::new(), healed: false, cancel })
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
    /// With a `terminal`, pytest's own reporter prints the run there.
    pub fn run(&mut self, req: &Request, terminal: Option<Terminal>, emit: &mut dyn FnMut(&Event)) -> anyhow::Result<i32> {
        let mut learned = Learned::default();
        let result = self.pipeline(req, terminal.as_ref(), emit, &mut learned);
        if let Some((_, session)) = &mut self.session {
            session.drop_reporter();
        }
        if req.strict || self.l1_pid().is_none() {
            self.warm = None;
        }
        let saved = self.save(&args_key(&req.pytest_args), learned);
        let code = result?;
        saved?;
        Ok(code)
    }

    /// Starts collecting everything these args collect in an L1 that `finish_warm_up` keeps, so a later `run` of
    /// the same args collects nothing. Runs meanwhile collect what they need in L1s of their own.
    pub fn start_warm_up(&mut self, req: &Request) -> anyhow::Result<()> {
        self.warm = None;
        let meta: ProjectMeta = read_meta(&self.store, PROJECT_META)?;
        self.warming = self.content_hashes(&meta);
        self.session_for(req, full_policy(req), &meta)?.start_background()
    }

    /// The warm collection once the one `start_warm_up` started has finished. `None` while it runs, or when a run
    /// replaced it. A failure with preimports forgets them and the session, so `take_healed` tells the caller to
    /// start again at once.
    pub fn finish_warm_up(&mut self) -> Option<anyhow::Result<()>> {
        let collected = match self.session.as_mut()?.1.finish_background()? {
            Ok(c) => c,
            Err(e) => {
                let mut meta: ProjectMeta = read_meta(&self.store, PROJECT_META).unwrap_or_default();
                if self.forget_preimports(&mut meta) {
                    self.healed = serde_json::to_string(&meta).is_ok_and(|m| self.store.meta_set(PROJECT_META, &m).is_ok());
                }
                return Some(Err(e));
            }
        };
        if let Some(code) = collected.exit_code {
            return Some(Err(anyhow::anyhow!("pytest exited with code {code} before running tests")));
        }
        let warm = Warm::new(Arc::new(collected), &self.root, &self.fps, std::mem::take(&mut self.warming));
        self.warm = Some(warm);
        Some(self.store.save_file_infos(self.fps.take_fresh()))
    }

    pub fn warming_up(&self) -> bool {
        self.session.as_ref().is_some_and(|(_, s)| s.collecting_in_background())
    }

    pub fn warm(&self) -> Option<&Warm> {
        self.warm.as_ref()
    }

    /// Kills L1 and any warm-up in progress, and keeps L0.
    pub fn drop_warm(&mut self) {
        self.warm = None;
        if let Some((_, session)) = &mut self.session {
            session.drop_l1();
            session.drop_background();
        }
    }

    /// Starts the pytest reporter the next run with this request's args will claim, so its startup is done by then.
    /// Only with a session for these args already running.
    pub fn prepare_reporter(&mut self, req: &Request) {
        if let Some((key, session)) = &mut self.session
            && *key == session_key(req, full_policy(req))
            && let Err(e) = session.prepare_reporter(req.reporter_args())
        {
            log(|| format!("preparing pytest's reporter: {e:#}"));
        }
    }

    /// Kills a prepared reporter: a conftest or config file it read changed.
    pub fn drop_reporter(&mut self) {
        if let Some((_, session)) = &mut self.session {
            session.drop_reporter();
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

    /// Content hashes of the project modules the last full collection imported, as they are now.
    fn content_hashes(&self, meta: &ProjectMeta) -> HashMap<String, Hash128> {
        let paths: Vec<&str> = meta.modules.values().map(String::as_str).collect();
        self.fps.prefetch(paths.clone());
        paths.into_iter().filter_map(|p| self.fps.info(p).map(|i| (p.to_string(), i.sha))).collect()
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

    fn pipeline(
        &mut self,
        req: &Request,
        terminal: Option<&Terminal>,
        emit: &mut dyn FnMut(&Event),
        learned: &mut Learned,
    ) -> anyhow::Result<i32> {
        let t0 = Instant::now();
        let started = std::time::SystemTime::now();
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
        // Root-relative paths of the files collected only for the selected tests in them.
        let mut narrowed: BTreeSet<String> = BTreeSet::new();
        let mut partial: Option<Partial> = match full {
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
                let file_of = |n: &str| path_of.get(n).copied().unwrap_or(test_file(n)).to_string();
                let mut whole: BTreeSet<String> = BTreeSet::new();
                for row in &inventory {
                    if let Some(why) = select::recollect(row, &fps, &graph) {
                        log(|| format!("re-collecting {}: {why}", row.path));
                        whole.insert(row.path.to_string());
                    }
                }
                let only: Vec<String> = affected.reasons.keys().filter(|n| !whole.contains(&file_of(n))).cloned().collect();
                narrowed = only.iter().map(|n| file_of(n)).collect();
                let files: BTreeSet<String> = narrowed.iter().cloned().chain(whole).collect();
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
                Some(Partial { files: files.into_iter().collect(), only })
            }
        };

        // The reporter's startup overlaps collection and the run.
        let mut reporter = match terminal {
            Some(t) => Some(self.session_for(req, full_policy(req), &project)?.start_reporter(
                req.reporter_args(),
                &req.terminal_env,
                t.try_clone()?,
            )?),
            None => None,
        };
        let (collected, warm) = match warm {
            Some(warm) => {
                log(|| format!("served by the warm collection of {} tests", warm.tests()));
                (warm.collected.clone(), warm)
            }
            None => {
                if let Some(p) = &partial {
                    log(|| format!("collecting {} files: {}", p.files.len(), p.files.join(" ")));
                }
                // Where the last full collection of these args could fork from. A partial run that needs no L1 trusts
                // it; any L1 is still thread-checked, and only a full collection records a new answer.
                let known_fork_point = match (req.fork_from, full, args_meta.fork_point.as_deref()) {
                    (Some(p), _, _) => Some(p),
                    (None, None, Some("imported")) => Some(ForkPoint::Imported),
                    (None, None, Some("spawn")) => Some(ForkPoint::Spawn),
                    _ => None,
                };
                self.warm = None;
                let seen = if partial.is_none() { self.content_hashes(&project) } else { HashMap::new() };
                // Workers that collect their own files need no L1 when the inventory already names every test to run.
                let no_l1 = known_fork_point.filter(|p| matches!(p, ForkPoint::Imported | ForkPoint::Spawn))
                    .filter(|_| !req.strict && partial.as_ref().is_some_and(|p| p.files.iter().all(|f| narrowed.contains(f))));
                let collected = if let Some(fork_point) = no_l1 {
                    let session = self.session_for(req, full_policy(req), &project)?;
                    session.drop_l1();
                    session.assume_fork_point(fork_point);
                    let only = &partial.as_ref().expect("checked above").only;
                    let path_of: HashMap<&str, &str> =
                        inventory.iter().flat_map(|r| r.nodeids.iter().map(|n| (n.as_str(), &*r.path))).collect();
                    let items = only
                        .iter()
                        .map(|n| CollectedItem { nodeid: n.clone(), path: path_of.get(n.as_str()).copied().unwrap_or(test_file(n)).to_string() })
                        .collect();
                    log(|| format!("no L1: {} tests from the inventory; workers fork from {} and collect their files", only.len(), fork_point_name(session.fork_point())));
                    Arc::new(Collected::known(items, session.fork_point()))
                } else {
                    let mut collected = self.collect_healing(req, &mut project, partial.as_ref())?;
                    if let Some(p) = partial.as_mut().filter(|_| collected.exit_code.is_none())
                        && let Some(n) = not_collected(&collected, &p.only)
                    {
                        log(|| format!("{n} was not collected with its file narrowed to the selected tests; collecting whole files"));
                        p.only.clear();
                        narrowed.clear();
                        collected = self.collect_healing(req, &mut project, Some(p))?;
                    }
                    let collected = Arc::new(collected);
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
                    collected
                };
                let warm = Warm::new(collected.clone(), &root, &fps, seen);
                if partial.is_none() {
                    self.warm = Some(warm.clone());
                }
                (collected, warm)
            }
        };
        let session = &mut self.session.as_mut().expect("collected above").1;
        if let Some(t) = terminal
            && !session.has_reporter()
        {
            // Collection healed by starting the interpreters again, and the reporter went with them.
            reporter = Some(session.start_reporter(req.reporter_args(), &req.terminal_env, t.try_clone()?)?);
        }

        let files = partial.as_ref().map(|p| &p.files[..]);
        let partial = partial.is_some();
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

        let (rows, universe) = self.update_inventory(collected, &inventory, files, &narrowed, &view, graph, learned);
        learned.inventory = Some(rows);
        learned.graph = graph.export();

        let mut collect_errors: HashSet<String> = HashSet::new();
        for (nodeid, message) in &collected.errors {
            collect_errors.insert(nodeid.clone());
            emit(&Event::CollectError { nodeid: nodeid.clone(), message: message.clone() });
        }
        if !collected.errors.is_empty() && !req.pytest_args.iter().any(|a| a == "--continue-on-collection-errors") {
            if let Some(to) = reporter.take() {
                let items: Vec<&str> = collected.items.iter().map(|i| i.nodeid.as_str()).collect();
                to.send(&replay::begin(None, &items, collected.shown.deselected, &collected.shown, true, started));
                to.send(&replay::end());
                let session = &mut self.session.as_mut().expect("collected above").1;
                if let Some(event) = reporter_event(session.finish_reporter()) {
                    emit(&event);
                }
            }
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
        let reason_counts = select::reason_counts(&reasons);
        let headline = crate::report::headline(changed.len(), selected.len() as u64, universe.len() as u64, &reason_counts, ms(t0));
        emit(&Event::Selection {
            changed,
            selected: selected.len() as u64,
            total: universe.len() as u64,
            reasons: reason_counts,
            duration_ms: ms(t0),
        });
        // A run that collected and ran everything reports what plain pytest would, module-level skips included.
        let ran_everything = matches!(req.mode, Mode::All) || (!partial && selected.len() == universe.len());
        let session = &mut self.session.as_mut().expect("started above").1;
        if selected.is_empty() {
            // Nothing runs: forktest's own one-line report says so, and the exit code stays forktest's.
            reporter = None;
            session.drop_reporter();
        }
        if let Some(to) = &reporter {
            let deselected = (universe.len() - selected.len()) as u64 + collected.shown.deselected;
            to.send(&replay::begin(Some(headline), &selected, deselected, &collected.shown, ran_everything, started));
        }

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
        let mut tally = Tally::default();
        let executed = run.execute(session, &selected, req.nice, reporter.as_ref(), learned, emit, &mut |r| tally.add(r.outcome));
        if !partial && let Some(args) = &mut learned.args {
            args.fork_point = Some(fork_point_name(session.fork_point()).into());
        }
        executed?;
        log(|| format!("every test reported {} ms into the run", ms(t0)));
        if self.cancel.load(Ordering::Relaxed) {
            return Ok(0);
        }
        if let Some(to) = reporter.take() {
            to.send(&replay::end());
            if let Some(event) = reporter_event(session.finish_reporter()) {
                emit(&event);
            }
        }
        let collect_skips = if ran_everything { collected.skips.len() as u64 } else { 0 };
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
        run.execute(session, &rest, true, None, learned, &mut on_unselected, &mut |_| {})?;
        if self.cancel.load(Ordering::Relaxed) {
            return Ok(0);
        }
        emit(&Event::Verified { checked: rest.len() as u64, misses, duration_ms: ms(t1) });
        Ok(0)
    }

    /// Preimports come from an earlier run and can break collection (a package that pulls in a plugin pytest
    /// rewrites, under -W error). Without them the run is slower but sound, and teaches the next one better.
    /// A collection that fails while preimports are in use is tried once more without them: a store can hold a list
    /// from before L0 learned to avoid modules that pull in a plugin pytest must import itself.
    fn collect_healing(&mut self, req: &Request, meta: &mut ProjectMeta, partial: Option<&Partial>) -> anyhow::Result<Collected> {
        match self.session_for(req, full_policy(req), meta)?.collect(partial) {
            Err(e) if self.forget_preimports(meta) => {
                log(|| format!("collection failed with preimports, retrying without them: {e:#}"));
                self.session_for(req, full_policy(req), meta)?.collect(partial)
            }
            other => other,
        }
    }

    /// Clears the preimports and drops the session that used them. False when there were none to blame.
    fn forget_preimports(&mut self, meta: &mut ProjectMeta) -> bool {
        if meta.preimport.is_empty() {
            return false;
        }
        meta.preimport.clear();
        self.warm = None;
        self.session = None;
        true
    }

    /// Whether the last background collection failed with preimports that are now forgotten, so a retry should start
    /// at once rather than wait for a file change.
    pub fn take_healed(&mut self) -> bool {
        std::mem::take(&mut self.healed)
    }

    /// The warm session for these args, or a new one. Other args only need a new L1. Fails with `Unsupported`
    /// before anything is emitted when the interpreter cannot run the shim.
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
            if let Some(why) = crate::python::check(&self.python).explain(&self.python) {
                return Err(Unsupported(why).into());
            }
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
                avoid: meta.avoid.iter().cloned().collect(),
                policy,
                stdout_to_stderr: req.stdout_to_stderr,
            })?;
            self.session = Some((key, session));
        }
        Ok(&mut self.session.as_mut().expect("just set").1)
    }

    /// The inventory rows after this collection, and the nodeid universe in collection order. Records which tests
    /// vanished. A `narrowed` file's collection saw only some of its tests, so its stored row stands.
    fn update_inventory(
        &self,
        collected: &Collected,
        previous: &[InventoryRow],
        requested: Option<&[String]>,
        narrowed: &BTreeSet<String>,
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
            .filter(|p| !narrowed.contains(*p) && seen.insert(p))
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

/// What the human reporter needs to know of how pytest's reporter ended. A session that ended before it printed
/// forktest's headline printed nothing the user saw, so forktest's own output stands in.
fn reporter_event(end: Option<(ReporterEnd, bool)>) -> Option<Event> {
    let (end, began) = end?;
    Some(match end {
        Ok(exit) if began => Event::Reported { exit },
        Ok(exit) => Event::ReporterLost { reason: format!("pytest's reporter exited with code {exit} before the run"), headline_shown: false },
        Err(reason) => Event::ReporterLost { reason, headline_shown: began },
    })
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

/// A selected test the narrowed collection did not make, outside the files that failed to collect.
fn not_collected<'a>(collected: &Collected, only: &'a [String]) -> Option<&'a String> {
    let made: HashSet<&str> = collected.items.iter().map(|i| i.nodeid.as_str()).collect();
    let failed: HashSet<&str> = collected.errors.iter().map(|(n, _)| test_file(n)).collect();
    only.iter().find(|n| !made.contains(n.as_str()) && !failed.contains(test_file(n)))
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
    if !partial {
        meta.avoid.clear();
    }
    meta.avoid.extend(collected.rewrite.iter().cloned());
}

/// What one test leaves for release in collection order: its message for pytest's reporter, and its result when
/// results are emitted in order.
type Held = (Option<serde_json::Value>, Option<TestResult>);

/// Seconds one more worker adds before its first test: its fork, measured at about 3 ms on the demo.
const WORKER_COST: f64 = 0.003;

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
        reporter: Option<&ToReporter>,
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
        // With `--maxfail` the run stops after a failure in collection order, so results are emitted in that order too
        // and none past the stop is reported. Otherwise they stream as they arrive.
        let mut order: InOrder<Held> = InOrder::new(nodeids, self.maxfail);
        let mut warned: HashSet<String> = HashSet::new();
        let (view, path_of, collect_errors) = (self.view, self.path_of, &mut self.collect_errors);
        let records = &mut learned.records;
        let send = |msg: serde_json::Value| {
            if let Some(to) = reporter {
                to.send(&msg);
            }
        };
        let mut arrive = |order: &mut InOrder<Held>, result: TestResult, pytest: Option<serde_json::Value>, emit: &mut dyn FnMut(&Event)| {
            let (nodeid, failed) = (result.nodeid.clone(), result.outcome.is_failure());
            let mut report = |result: TestResult| {
                on_result(&result);
                emit(&Event::Result(result));
            };
            let held = match order.maxfail {
                0 => {
                    report(result);
                    None
                }
                _ => Some(result),
            };
            for (msg, result) in order.arrived(&nodeid, failed, (pytest, held)) {
                msg.map(send);
                result.map(&mut report);
            }
        };
        let mut sink = |event: WorkerEvent, names: &Names| -> Control {
            match event {
                WorkerEvent::Result { result, cost, hits, reads, pytest, .. } => {
                    let file = path_of.get(result.nodeid.as_str()).copied().unwrap_or(test_file(&result.nodeid));
                    let stored = view.tests.get(&result.nodeid).map(|t| t.digest);
                    let set = deps.of(file, &hits, &reads, names, stored);
                    records.push(Record { nodeid: result.nodeid.clone(), outcome: result.outcome, cost, deps: set });
                    arrive(&mut order, result, pytest.map(replay::test), emit);
                }
                WorkerEvent::Started { maxfail, warnings } => {
                    order.maxfail = maxfail;
                    // Every worker that collects a file records its collection warnings; pytest shows them once.
                    let new: Vec<_> = warnings.into_iter().filter(|w| warned.insert(w.to_string())).collect();
                    if !new.is_empty() {
                        send(replay::warnings(new));
                    }
                }
                WorkerEvent::CollectError { nodeid, message, report } => {
                    if collect_errors.insert(nodeid.clone()) {
                        report.map(replay::collect).map(send);
                        emit(&Event::CollectError { nodeid, message });
                    }
                }
                WorkerEvent::Lost { nodeids, message, .. } => {
                    for nodeid in nodeids {
                        let lost = reporter.map(|_| replay::lost(&nodeid, &message));
                        let result = TestResult {
                            nodeid,
                            outcome: Outcome::Error,
                            duration: 0.0,
                            file: None,
                            line: None,
                            message: Some(message.clone()),
                            trace: Vec::new(),
                        };
                        arrive(&mut order, result, lost, emit);
                    }
                }
            }
            if order.stopped() { Control::Stop } else { Control::Continue }
        };
        let replay = reporter.is_some();
        if self.req.strict {
            return session.run_in_process(nodeids.iter().map(|n| n.to_string()).collect(), replay, &mut names, &mut sink);
        }
        let workers = match self.req.workers {
            0 => std::thread::available_parallelism().map_or(2, |n| n.get()),
            n => n,
        };
        let items: Vec<(&str, f64)> = nodeids.iter().map(|n| (*n, view.tests.get(*n).map_or(0.01, |t| t.cost))).collect();
        let total: f64 = items.iter().map(|(_, cost)| cost).sum();
        // Workers fork one after another, so k of them finish near k * WORKER_COST + total / k: fewest at the root.
        let workers = workers.min(((total / WORKER_COST).sqrt().round() as usize).max(1));
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
        session.run(batches, nice, replay, &mut names, self.cancel, &mut sink)
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
