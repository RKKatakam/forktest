//! Which tests a change affects, and why; which test files need collecting again. Pure over a store view. Unit U1.

use crate::fingerprint::{Dep, FILE, FileInfo, Fingerprints, Hash128, MODULE, RelPath};
use crate::graph::ImportGraph;
use crate::root::is_config_file;
use crate::store::{GitBaseline, InventoryRow, Store, StoreView, dep_hash};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

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

    /// Every distinct path `changed` is about to be asked about, so an implementation can read them in parallel.
    fn prepare(&mut self, _paths: &[&str]) {}
}

/// The files as they are now. Each path is stat'ed once per instance.
pub struct WorkingTree<'a> {
    fps: &'a Fingerprints,
    infos: HashMap<RelPath, Option<Arc<FileInfo>>>,
}

impl<'a> WorkingTree<'a> {
    pub fn new(fps: &'a Fingerprints) -> WorkingTree<'a> {
        WorkingTree { fps, infos: HashMap::new() }
    }

    fn hash_now(&mut self, path: &RelPath, qualname: &str) -> Option<Hash128> {
        let info = match self.infos.get(&**path) {
            Some(info) => info.as_ref(),
            None => self.infos.entry(path.clone()).or_insert_with(|| self.fps.info(path)).as_ref(),
        };
        Some(info?.hash_of(qualname)?.1)
    }
}

impl Current for WorkingTree<'_> {
    fn changed(&mut self, dep: &Dep) -> bool {
        self.hash_now(&dep.path, &dep.qualname) != Some(dep.hash)
    }

    fn prepare(&mut self, paths: &[&str]) {
        let fps = self.fps;
        let found: Vec<(RelPath, Option<Arc<FileInfo>>)> = paths
            .par_iter()
            .filter(|p| !self.infos.contains_key(**p))
            .map(|p| match fps.entry(p) {
                Some((path, info)) => (path, Some(info)),
                None => (Arc::from(*p), None),
            })
            .collect();
        self.infos.extend(found);
    }
}

/// Changes since a git baseline: a dep counts as changed when its hash there differs from its hash now, or it
/// did not exist there.
pub struct SinceBaseline<'a> {
    base: GitBaseline,
    tree: WorkingTree<'a>,
}

impl<'a> SinceBaseline<'a> {
    pub fn new(base: GitBaseline, fps: &'a Fingerprints) -> SinceBaseline<'a> {
        SinceBaseline { base, tree: WorkingTree::new(fps) }
    }
}

impl Current for SinceBaseline<'_> {
    fn changed(&mut self, dep: &Dep) -> bool {
        match self.base.hash_of(&dep.path, &dep.qualname) {
            None => true,
            Some(then) => self.tree.hash_now(&dep.path, &dep.qualname) != Some(then),
        }
    }

    fn prepare(&mut self, paths: &[&str]) {
        self.tree.prepare(paths);
    }
}

const IMPORTABLE_BINARY_SUFFIXES: &[&str] = &[".so", ".pyd", ".pyc"];

pub fn classify(dep: &Dep, fixtures: &HashSet<(RelPath, Arc<str>)>) -> Reason {
    let name = dep.path.rsplit('/').next().unwrap_or(&dep.path);
    match &*dep.qualname {
        FILE if is_config_file(name) => Reason::ConfigChange,
        FILE if IMPORTABLE_BINARY_SUFFIXES.iter().any(|s| name.ends_with(s)) => Reason::ImportGraph,
        FILE => Reason::FileRead,
        MODULE if name == "conftest.py" => Reason::ConfigChange,
        MODULE => Reason::ImportGraph,
        _ if fixtures.contains(&(dep.path.clone(), dep.qualname.clone())) => Reason::Fixture,
        _ => Reason::Coverage,
    }
}

pub struct Affected {
    /// The highest-priority reason per selected nodeid (the `Ord` of `Reason` is the priority).
    pub reasons: HashMap<String, Reason>,
    /// Changed paths that affect a selected test, sorted.
    pub changed: Vec<String>,
}

fn test_file(nodeid: &str) -> &str {
    nodeid.split("::").next().unwrap_or(nodeid)
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
    let paths: Vec<&str> = view.keys.values().map(|d| &*d.path).collect::<HashSet<_>>().into_iter().collect();
    current.prepare(&paths);
    let changed: HashMap<u32, &Dep> = view.keys.iter().filter(|(_, d)| current.changed(d)).map(|(&i, d)| (i, d)).collect();
    let by_id: HashMap<u32, &str> =
        nodeids.iter().filter_map(|n| view.tests.get(n).map(|row| (row.id, n.as_str()))).collect();
    let mut reasons: HashMap<&str, Reason> = HashMap::new();
    let mut changed_paths: BTreeSet<&str> = BTreeSet::new();
    let key_ids: Vec<u32> = changed.keys().copied().collect();
    for (key_id, test_ids) in store.tests_using(&key_ids)? {
        let dep = changed[&key_id];
        let reason = classify(dep, fixtures);
        for t in test_ids {
            if let Some(nodeid) = by_id.get(&t) {
                offer(&mut reasons, nodeid, reason);
                changed_paths.insert(&dep.path);
            }
        }
    }
    let mut groups: HashMap<(&str, u32), Vec<&str>> = HashMap::new();
    for nodeid in nodeids {
        let Some(row) = view.tests.get(nodeid) else {
            offer(&mut reasons, nodeid, Reason::New);
            continue;
        };
        if row.outcome.is_failure() {
            offer(&mut reasons, nodeid, Reason::LastFailed);
        }
        groups.entry((test_file(nodeid), row.modules)).or_default().push(nodeid);
    }
    for ((file, modules), members) in groups {
        if !recorded_covers(view, modules, &graph.closure(file)) {
            for nodeid in members {
                offer(&mut reasons, nodeid, Reason::ImportGraph);
            }
        }
    }
    Ok(Affected {
        reasons: reasons.into_iter().map(|(n, r)| (n.to_string(), r)).collect(),
        changed: changed_paths.into_iter().map(str::to_string).collect(),
    })
}

fn offer<'a>(reasons: &mut HashMap<&'a str, Reason>, nodeid: &'a str, reason: Reason) {
    let held = reasons.entry(nodeid).or_insert(reason);
    *held = (*held).min(reason);
}

fn recorded_covers(view: &StoreView, modules: u32, closure: &HashSet<RelPath>) -> bool {
    match view.module_sets.get(&modules) {
        Some(set) => closure.iter().all(|p| set.contains(p)),
        None => closure.is_empty(),
    }
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
    let Some(row) = view.tests.get(nodeid) else { return Ok(vec![(Reason::New, None)]) };
    let mut out = Vec::new();
    if row.outcome.is_failure() {
        out.push((Reason::LastFailed, None));
    }
    for k in store.key_ids_of(nodeid)? {
        if let Some(dep) = view.keys.get(&k)
            && current.changed(dep)
        {
            out.push((classify(dep, fixtures), Some(dep.clone())));
        }
    }
    let recorded = view.module_sets.get(&row.modules);
    let mut unrecorded: Vec<RelPath> =
        graph.closure(test_file(nodeid)).iter().filter(|p| !recorded.is_some_and(|s| s.contains(*p))).cloned().collect();
    unrecorded.sort();
    let module: Arc<str> = Arc::from(MODULE);
    out.extend(unrecorded.into_iter().map(|path| (Reason::ImportGraph, Some(Dep { path, qualname: module.clone(), hash: 0 }))));
    Ok(out)
}

/// Why a test file has to be collected again, or `None` when its inventory row still holds: `error` (it failed to
/// collect last time), `missing`, `content`, `imports` (its closure digest moved) or `collection_deps`.
pub fn recollect(row: &InventoryRow, fps: &Fingerprints, graph: &ImportGraph) -> Option<&'static str> {
    if row.error {
        return Some("error");
    }
    let Some(info) = fps.info(&row.path) else { return Some("missing") };
    if info.sha != row.sha {
        return Some("content");
    }
    if closure_digest(&row.path, fps, graph) != row.closure {
        return Some("imports");
    }
    if row.deps.iter().any(|d| fps.dep(&d.path, &d.qualname).map(|now| now.hash) != Some(d.hash)) {
        return Some("collection_deps");
    }
    None
}

/// The closure digest an inventory row stores.
pub fn closure_digest(test_file: &str, fps: &Fingerprints, graph: &ImportGraph) -> u128 {
    graph.closure(test_file).iter().filter_map(|p| fps.dep(p, MODULE)).fold(0u128, |acc, d| acc.wrapping_add(dep_hash(&d)))
}

pub fn reason_counts(reasons: &HashMap<String, Reason>) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for r in reasons.values() {
        *out.entry(r.as_str().to_string()).or_insert(0) += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Outcome;
    use crate::graph::tests::{edit, project};
    use crate::store::tests::rec;
    use std::path::Path;
    use std::process::Command;

    const NODE: &str = "tests/test_core.py::test_f";
    const DEEP: &str = "tests/sub/test_deep.py::test_deep";
    const DEEP2: &str = "tests/sub/test_deep.py::test_deep2";
    const NEW: &str = "tests/test_core.py::test_new";

    fn fps(root: &Path) -> Arc<Fingerprints> {
        Arc::new(Fingerprints::new(root, HashMap::new()))
    }

    fn graph(root: &Path, fps: &Arc<Fingerprints>) -> ImportGraph {
        ImportGraph::new(root, fps.clone(), HashMap::new())
    }

    /// Records in which every dep hash matches the files as they are now.
    fn baseline(root: &Path, extra: &[Dep]) -> Vec<(&'static str, Vec<Dep>)> {
        let fps = fps(root);
        let g = graph(root, &fps);
        let dep = |p: &str, q: &str| fps.dep(p, q).unwrap();
        let mut deps = vec![dep("pkg/core.py", "f"), dep("pkg/util.py", "g"), dep("conftest.py", "fx"), dep("data.json", FILE)];
        deps.extend(g.closure("tests/test_core.py").iter().map(|p| dep(p, MODULE)));
        deps.push(dep("pyproject.toml", FILE));
        deps.extend(extra.iter().cloned());
        let deep: Vec<Dep> = g.closure("tests/sub/test_deep.py").iter().map(|p| dep(p, MODULE)).collect();
        vec![(NODE, deps), (DEEP, deep.clone()), (DEEP2, deep)]
    }

    fn recorded(root: &Path, outcome: Outcome, only: Option<&[&str]>, extra: &[Dep]) -> (Store, StoreView) {
        let mut store = Store::open(root).unwrap();
        let records = baseline(root, extra)
            .into_iter()
            .filter(|(n, _)| only.is_none_or(|o| o.contains(n)))
            .map(|(n, ds)| rec(n, if n == NODE { outcome } else { Outcome::Passed }, 0.1, ds))
            .collect();
        store.record(records).unwrap();
        let view = store.view().unwrap();
        (store, view)
    }

    fn fixtures() -> HashSet<(RelPath, Arc<str>)> {
        HashSet::from([(Arc::from("conftest.py"), Arc::from("fx"))])
    }

    fn nodeids(of: &[&str]) -> Vec<String> {
        of.iter().map(|n| n.to_string()).collect()
    }

    /// Selection on every test, checked against `explain` for each one.
    fn run(root: &Path, store: &Store, view: &StoreView, current: &mut dyn Current) -> Affected {
        let fps = fps(root);
        let all = nodeids(&[NODE, DEEP, DEEP2, NEW]);
        let affected = select(&all, view, store, current, &graph(root, &fps), &fixtures()).unwrap();
        for nodeid in &all {
            let reasons = explain(nodeid, view, store, current, &graph(root, &fps), &fixtures()).unwrap();
            let top = reasons.iter().map(|(r, _)| *r).min();
            assert_eq!(affected.reasons.get(nodeid).copied(), top, "--why disagrees with selection for {nodeid}");
        }
        affected
    }

    fn run_here(root: &Path, store: &Store, view: &StoreView) -> Affected {
        let fps = fps(root);
        run(root, store, view, &mut WorkingTree::new(&fps))
    }

    #[test]
    fn unchanged_selects_only_new_tests() {
        let root = project("sel-unchanged");
        let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
        let affected = run_here(&root, &store, &view);
        assert_eq!(affected.reasons, HashMap::from([(NEW.to_string(), Reason::New)]));
        assert!(affected.changed.is_empty());
    }

    #[test]
    fn last_failed_reruns() {
        let root = project("sel-failed");
        let (store, view) = recorded(&root, Outcome::Failed, None, &[]);
        assert_eq!(run_here(&root, &store, &view).reasons[NODE], Reason::LastFailed);
    }

    #[test]
    fn each_kind_of_change_selects_with_its_reason() {
        let cases = [
            ("pkg/util.py", "return 2", "return 20", Reason::Coverage),
            ("conftest.py", "helpers.h()", "helpers.h() + 1", Reason::Fixture),
            ("data.json", "{}", r#"{"k": 1}"#, Reason::FileRead),
            ("config.py", "X = 1", "X = 2", Reason::ImportGraph),
            ("src/lib/thing.py", "def t():", "def t(a=1):", Reason::ImportGraph),
            ("conftest.py", "import helpers", "import helpers, os", Reason::ConfigChange),
            ("pyproject.toml", "ini_options", "ini_options2", Reason::ConfigChange),
        ];
        for (k, (rel, old, new, reason)) in cases.into_iter().enumerate() {
            let root = project(&format!("sel-kind{k}"));
            let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
            edit(&root, rel, old, new);
            let affected = run_here(&root, &store, &view);
            assert_eq!(affected.reasons[NODE], reason, "{rel}");
            assert_eq!(affected.changed, [rel]);
        }
    }

    #[test]
    fn classify_maps_binaries_to_the_import_graph() {
        let d = |p: &str, q: &str| Dep { path: Arc::from(p), qualname: Arc::from(q), hash: 0 };
        let none = HashSet::new();
        assert_eq!(classify(&d("pkg/_ext.cpython-312-darwin.so", FILE), &none), Reason::ImportGraph);
        assert_eq!(classify(&d("pkg/__pycache__/m.cpython-312.pyc", FILE), &none), Reason::ImportGraph);
        assert_eq!(classify(&d("requirements-dev.txt", FILE), &none), Reason::ConfigChange);
        assert_eq!(classify(&d("tests/conftest.py", MODULE), &none), Reason::ConfigChange);
        assert_eq!(classify(&d("cases.csv", FILE), &none), Reason::FileRead);
    }

    #[test]
    fn docstring_and_unrelated_body_edits_select_nothing() {
        let root = project("sel-docstring");
        let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
        edit(&root, "pkg/util.py", "def g():\n", "def g():\n    \"\"\"Doc.\"\"\"\n");
        edit(&root, "tests/sub/local_helper.py", "return 4", "return 40");
        assert!(!run_here(&root, &store, &view).reasons.contains_key(NODE));
    }

    #[test]
    fn a_removed_function_and_a_deleted_file_count_as_changed() {
        let root = project("sel-removed");
        let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
        std::fs::remove_file(root.join("data.json")).unwrap();
        assert_eq!(run_here(&root, &store, &view).reasons[NODE], Reason::FileRead);
        std::fs::write(root.join("data.json"), "{}").unwrap();
        let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
        edit(&root, "pkg/util.py", "def g():", "def g2():");
        assert_eq!(run_here(&root, &store, &view).reasons[NODE], Reason::Coverage);
    }

    #[test]
    fn a_file_that_stops_parsing_counts_as_changed() {
        let root = project("sel-broken");
        let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
        edit(&root, "pkg/util.py", "def g():", "def g(:");
        assert_eq!(run_here(&root, &store, &view).reasons[NODE], Reason::Coverage);
    }

    #[test]
    fn a_raw_lambda_qualname_is_normalized_before_comparison() {
        let root = project("sel-lambda");
        let g = fps(&root).dep("pkg/util.py", "g").unwrap();
        let lambda = Dep { qualname: Arc::from("g.<locals>.<lambda>"), ..g };
        let (store, view) = recorded(&root, Outcome::Passed, None, &[lambda]);
        assert!(!run_here(&root, &store, &view).reasons.contains_key(NODE));
    }

    #[test]
    fn a_newly_created_module_that_an_import_now_resolves_to_selects() {
        let root = project("sel-newmod");
        let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
        std::fs::write(root.join("tests/sub/newmod.py"), "Y = 1\n").unwrap();
        assert_eq!(run_here(&root, &store, &view).reasons[DEEP], Reason::ImportGraph);
    }

    #[test]
    fn a_new_import_reaches_every_test_of_the_file_that_did_not_rerun() {
        let root = project("sel-sibling");
        recorded(&root, Outcome::Passed, None, &[]);
        std::fs::write(root.join("tests/sub/newmod.py"), "Y = 1\n").unwrap();
        let (store, view) = recorded(&root, Outcome::Passed, Some(&[DEEP]), &[]);
        let affected = run_here(&root, &store, &view);
        assert!(!affected.reasons.contains_key(DEEP));
        assert_eq!(affected.reasons[DEEP2], Reason::ImportGraph);
    }

    #[test]
    fn priority_puts_config_before_last_failed_before_coverage() {
        let root = project("sel-priority");
        let (store, view) = recorded(&root, Outcome::Failed, None, &[]);
        edit(&root, "pkg/util.py", "return 2", "return 20");
        assert_eq!(run_here(&root, &store, &view).reasons[NODE], Reason::LastFailed);
        edit(&root, "pyproject.toml", "ini_options", "ini_options2");
        assert_eq!(run_here(&root, &store, &view).reasons[NODE], Reason::ConfigChange);
    }

    #[test]
    fn changed_paths_name_only_changes_that_reach_the_given_tests() {
        let root = project("sel-paths");
        let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
        edit(&root, "pkg/util.py", "return 2", "return 20");
        let fps = fps(&root);
        let affected =
            select(&nodeids(&[DEEP]), &view, &store, &mut WorkingTree::new(&fps), &graph(&root, &fps), &HashSet::new()).unwrap();
        assert!(affected.reasons.is_empty() && affected.changed.is_empty());
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn since_compares_the_merge_base_with_the_working_tree() {
        let root = project("sel-since");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["checkout", "-q", "-b", "feature"]);
        edit(&root, "pkg/util.py", "return 2", "return 20");
        git(&root, &["commit", "-q", "-am", "edit"]);
        let (store, view) = recorded(&root, Outcome::Passed, None, &[]);
        let fps = fps(&root);
        let mut since = SinceBaseline::new(GitBaseline::new(&root, "main").unwrap(), &fps);
        let affected = run(&root, &store, &view, &mut since);
        assert_eq!(affected.reasons[NODE], Reason::Coverage);
        assert_eq!(affected.changed, ["pkg/util.py"]);
        assert!(!run_here(&root, &store, &view).reasons.contains_key(NODE));
    }

    fn inventory_row(root: &Path, path: &str, deps: Vec<Dep>, error: bool) -> InventoryRow {
        let fps = fps(root);
        InventoryRow {
            path: Arc::from(path),
            sha: fps.info(path).unwrap().sha,
            closure: closure_digest(path, &fps, &graph(root, &fps)),
            deps,
            nodeids: vec![format!("{path}::test_f")],
            error,
            skipped: false,
        }
    }

    fn recollect_now(root: &Path, row: &InventoryRow) -> Option<&'static str> {
        let fps = fps(root);
        recollect(row, &fps, &graph(root, &fps))
    }

    #[test]
    fn a_test_file_is_collected_again_when_its_content_imports_or_collection_deps_change() {
        let root = project("sel-recollect");
        let core = "tests/test_core.py";
        let params = fps(&root).dep("helpers.py", "h").unwrap();
        let row = inventory_row(&root, core, vec![params], false);
        assert_eq!(recollect_now(&root, &row), None);
        assert_eq!(recollect_now(&root, &InventoryRow { error: true, ..row.clone() }), Some("error"));

        edit(&root, "pkg/util.py", "return 2", "return 20");
        assert_eq!(recollect_now(&root, &row), None, "a body edit leaves collection alone");
        edit(&root, "config.py", "X = 1", "X = 2");
        assert_eq!(recollect_now(&root, &row), Some("imports"));
        let row = inventory_row(&root, core, row.deps.clone(), false);
        edit(&root, "helpers.py", "return 1", "return 10");
        assert_eq!(recollect_now(&root, &row), Some("collection_deps"));
        let row = inventory_row(&root, core, vec![], false);
        edit(&root, core, "assert f()", "assert f() == 3");
        assert_eq!(recollect_now(&root, &row), Some("content"));
        let row = inventory_row(&root, core, vec![], false);
        std::fs::write(root.join("tests/conftest.py"), "import config\n").unwrap();
        assert_eq!(recollect_now(&root, &row), Some("imports"), "conftests count");
        std::fs::remove_file(root.join(core)).unwrap();
        assert_eq!(recollect_now(&root, &row), Some("missing"));
    }
}
