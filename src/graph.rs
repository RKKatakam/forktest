//! Static import edges between project files, and each test file's closure. Unit U1.
//!
//! Port of `forktest.selection.ImportGraph`: resolution through the persisted `sys.modules` map first, then the
//! file system under the root, `src/` and the importer's top-level directory; Tarjan closures; edges persisted
//! with their inputs (imports digest, directory listing digests, modules digest) and reused while they match.
//!
//! Listings, edges and closures are memoized for the life of the instance, so it describes the tree as it was
//! when first asked. Build a new one per run. A directory is persisted by a digest of its listing rather than its
//! mtime, which is too coarse: build trees inside a project are rewritten on every import.

use crate::fingerprint::{Fingerprints, Hash128, ImportStmt, RelPath};
use crate::root::is_excluded;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;
use std::sync::{Arc, Mutex};
use xxhash_rust::xxh3::Xxh3;

const GRAPH_FORMAT: u32 = 1;

type Edges = Arc<[RelPath]>;
type Reach = Arc<HashSet<RelPath>>;

/// A set of interned path ids. Closures of a large package overlap almost entirely, so unions are word-wide.
#[derive(Default)]
struct Bits(Vec<u64>);

impl Bits {
    fn insert(&mut self, i: u32) {
        let (word, bit) = (i as usize / 64, i % 64);
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << bit;
    }

    fn union(&mut self, other: &Bits) {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        self.0.iter_mut().zip(&other.0).for_each(|(a, b)| *a |= b);
    }

    fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().enumerate().flat_map(|(w, &word)| {
            (0..64).filter(move |b| word & (1 << b) != 0).map(move |b| (w * 64 + b) as u32)
        })
    }
}

pub struct ImportGraph {
    inputs: Inputs,
    state: Mutex<State>,
}

struct Inputs {
    root: std::path::PathBuf,
    fps: Arc<Fingerprints>,
    known: HashMap<String, RelPath>,
    modules_digest: Hash128,
}

#[derive(Default)]
struct State {
    ids: HashMap<RelPath, u32>,
    names: Vec<RelPath>,
    listings: HashMap<String, Arc<Listing>>,
    dir_digests: BTreeMap<String, Hash128>,
    basedirs: HashMap<String, String>,
    targets: HashMap<String, Edges>,
    direct: HashMap<RelPath, Edges>,
    resolved_any: bool,
    reach: HashMap<RelPath, Arc<Bits>>,
    closure: HashMap<RelPath, Reach>,
}

#[derive(Default)]
struct Listing {
    files: HashSet<String>,
    dirs: HashSet<String>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    format: u32,
    modules: String,
    dirs: BTreeMap<String, String>,
    nodes: BTreeMap<String, (String, String, Vec<String>)>,
}

fn hash128(value: &impl Hash) -> Hash128 {
    let mut h = Xxh3::new();
    value.hash(&mut h);
    h.digest128()
}

fn hex(h: Hash128) -> String {
    format!("{h:032x}")
}

fn imports_digest(imports: &[ImportStmt]) -> Hash128 {
    hash128(&imports)
}

fn dirname(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(head, _)| head)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

impl ImportGraph {
    pub fn new(root: &Path, fps: Arc<Fingerprints>, known_modules: HashMap<String, RelPath>) -> ImportGraph {
        let mut entries: Vec<(&String, &RelPath)> = known_modules.iter().collect();
        entries.sort();
        let modules_digest = hash128(&entries);
        ImportGraph {
            inputs: Inputs { root: root.to_path_buf(), fps, known: known_modules, modules_digest },
            state: Mutex::new(State::default()),
        }
    }

    pub fn reusing(
        state: Option<&str>,
        root: &Path,
        fps: Arc<Fingerprints>,
        known_modules: HashMap<String, RelPath>,
    ) -> ImportGraph {
        let graph = ImportGraph::new(root, fps, known_modules);
        let Some(saved) = state.and_then(|s| serde_json::from_str::<Saved>(s).ok()) else { return graph };
        if saved.format != GRAPH_FORMAT || saved.modules != hex(graph.inputs.modules_digest) {
            return graph;
        }
        graph.reuse(saved);
        graph
    }

    fn reuse(&self, saved: Saved) {
        let mut st = self.state.lock().unwrap();
        for (d, digest) in &saved.dirs {
            st.listing(&self.inputs, d);
            if st.dir_digests.get(d).map(|h| hex(*h)).as_ref() != Some(digest) {
                return;
            }
        }
        for (path, (sha, imports, targets)) in saved.nodes {
            let Some(info) = self.inputs.fps.info(&path) else { continue };
            if hex(info.sha) == sha || hex(imports_digest(info.imports())) == imports {
                let edges: Edges = targets.iter().map(|t| st.intern(t)).collect();
                let path = st.intern(&path);
                st.direct.insert(path, edges);
            }
        }
    }

    /// Every project file the test file's import, and its conftests' imports, can reach.
    pub fn closure(&self, test_file: &str) -> Arc<HashSet<RelPath>> {
        self.state.lock().unwrap().closure(&self.inputs, test_file)
    }

    #[cfg(test)]
    pub fn conftests_for(&self, path: &str) -> Vec<RelPath> {
        let mut st = self.state.lock().unwrap();
        st.conftests_for(&self.inputs, path)
    }

    /// The state for `reusing`, or `None` when nothing new was resolved.
    pub fn export(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        if !st.resolved_any {
            return None;
        }
        let nodes = st
            .direct
            .iter()
            .filter_map(|(path, targets)| {
                let info = self.inputs.fps.info(path)?;
                let mut targets: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
                targets.sort();
                Some((path.to_string(), (hex(info.sha), hex(imports_digest(info.imports())), targets)))
            })
            .collect();
        let saved = Saved {
            format: GRAPH_FORMAT,
            modules: hex(self.inputs.modules_digest),
            dirs: st.dir_digests.iter().map(|(d, h)| (d.clone(), hex(*h))).collect(),
            nodes,
        };
        Some(serde_json::to_string(&saved).expect("serializable"))
    }
}

impl State {
    fn intern(&mut self, path: &str) -> RelPath {
        if let Some((p, _)) = self.ids.get_key_value(path) {
            return p.clone();
        }
        let p: RelPath = Arc::from(path);
        self.ids.insert(p.clone(), self.names.len() as u32);
        self.names.push(p.clone());
        p
    }

    fn id(&mut self, path: &str) -> u32 {
        let p = self.intern(path);
        self.ids[&p]
    }

    fn paths(&self, bits: &Bits) -> HashSet<RelPath> {
        bits.ids().map(|i| self.names[i as usize].clone()).collect()
    }

    /// The files and subdirectories of `d`. A directory its parent does not list is never scanned.
    fn listing(&mut self, inp: &Inputs, d: &str) -> Arc<Listing> {
        if let Some(l) = self.listings.get(d) {
            return l.clone();
        }
        let mut listing = Listing::default();
        let name = d.rsplit('/').next().unwrap_or(d);
        if d.is_empty() || (!is_excluded(d) && self.listing(inp, dirname(d)).dirs.contains(name)) {
            if let Ok(entries) = std::fs::read_dir(inp.root.join(d)) {
                for e in entries.flatten() {
                    let (Ok(kind), Ok(name)) = (e.file_type(), e.file_name().into_string()) else { continue };
                    // DirEntry::file_type does not follow symlinks, os.scandir's is_file and is_dir do.
                    let kind = if kind.is_symlink() { std::fs::metadata(e.path()).map(|m| m.file_type()) } else { Ok(kind) };
                    match kind {
                        Ok(k) if k.is_file() => listing.files.insert(name),
                        Ok(k) if k.is_dir() => listing.dirs.insert(name),
                        _ => false,
                    };
                }
            }
            let mut files: Vec<&String> = listing.files.iter().collect();
            let mut dirs: Vec<&String> = listing.dirs.iter().collect();
            files.sort();
            dirs.sort();
            self.dir_digests.insert(d.to_string(), hash128(&(files, dirs)));
        }
        let listing = Arc::new(listing);
        self.listings.insert(d.to_string(), listing.clone());
        listing
    }

    fn exists(&mut self, inp: &Inputs, relpath: &str) -> bool {
        let (d, name) = relpath.rsplit_once('/').unwrap_or(("", relpath));
        self.listing(inp, d).files.contains(name)
    }

    fn basedir(&mut self, inp: &Inputs, importer: &str) -> String {
        let start = dirname(importer);
        if let Some(b) = self.basedirs.get(start) {
            return b.clone();
        }
        let mut d = start;
        while !d.is_empty() && self.exists(inp, &format!("{d}/__init__.py")) {
            d = dirname(d);
        }
        self.basedirs.insert(start.to_string(), d.to_string());
        d.to_string()
    }

    /// Most imports name things, not modules (`from m import a, b`), and many files import the same ones.
    fn files_for(&mut self, inp: &Inputs, dotted: &str, search_dirs: &[&str]) -> Edges {
        let key = format!("{}\x01{dotted}", search_dirs.join("\0"));
        if let Some(t) = self.targets.get(&key) {
            return t.clone();
        }
        let rel = dotted.replace('.', "/");
        let mut out: Vec<RelPath> = Vec::new();
        for d in search_dirs {
            for cand in [join(d, &format!("{rel}.py")), join(d, &format!("{rel}/__init__.py"))] {
                if self.exists(inp, &cand) {
                    let p = self.intern(&cand);
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        }
        let out: Edges = out.into();
        self.targets.insert(key, out.clone());
        out
    }

    fn resolve(&mut self, inp: &Inputs, stmt: &ImportStmt, importer: &str, out: &mut HashSet<RelPath>) {
        let module = stmt.module.as_str();
        let mut targets: Vec<String> = Vec::new();
        if !module.is_empty() {
            targets.push(module.to_string());
        }
        targets.extend(stmt.names.iter().map(|n| if module.is_empty() { n.clone() } else { format!("{module}.{n}") }));
        if !module.is_empty() {
            let parts: Vec<&str> = module.split('.').collect();
            targets.extend((1..parts.len()).map(|i| parts[..i].join(".")));
        }
        if stmt.level > 0 {
            let mut base = dirname(importer);
            for _ in 1..stmt.level {
                base = dirname(base);
            }
            for t in &targets {
                out.extend(self.files_for(inp, t, &[base]).iter().cloned());
            }
            let init = join(base, "__init__.py");
            if self.exists(inp, &init) {
                out.insert(self.intern(&init));
            }
            return;
        }
        let basedir = self.basedir(inp, importer);
        let search_dirs = ["", "src", basedir.as_str()];
        for t in &targets {
            match inp.known.get(t) {
                Some(known) if self.exists(inp, known) => {
                    out.insert(known.clone());
                }
                _ => out.extend(self.files_for(inp, t, &search_dirs).iter().cloned()),
            }
        }
    }

    fn direct(&mut self, inp: &Inputs, path: &RelPath) -> Edges {
        if let Some(e) = self.direct.get(path) {
            return e.clone();
        }
        let mut edges: HashSet<RelPath> = HashSet::new();
        if path.ends_with(".py")
            && let Some(info) = inp.fps.info(path)
        {
            for stmt in info.imports() {
                self.resolve(inp, stmt, path, &mut edges);
            }
        }
        edges.remove(path);
        let edges: Edges = edges.into_iter().collect();
        self.direct.insert(path.clone(), edges.clone());
        self.resolved_any = true;
        edges
    }

    fn conftests_for(&mut self, inp: &Inputs, path: &str) -> Vec<RelPath> {
        let d = dirname(path);
        let parts: Vec<&str> = if d.is_empty() { Vec::new() } else { d.split('/').collect() };
        let mut out = Vec::new();
        for i in 0..=parts.len() {
            let cand = join(&parts[..i].join("/"), "conftest.py");
            if self.exists(inp, &cand) {
                out.push(self.intern(&cand));
            }
        }
        out
    }

    fn closure(&mut self, inp: &Inputs, path: &str) -> Reach {
        if let Some(c) = self.closure.get(path) {
            return c.clone();
        }
        let path = self.intern(path);
        let mut roots = vec![path.clone()];
        roots.extend(self.conftests_for(inp, &path));
        let mut bits = Bits::default();
        for r in roots {
            if self.exists(inp, &r) {
                bits.union(&self.reachable(inp, &r));
            }
        }
        let out = Arc::new(self.paths(&bits));
        self.closure.insert(path, out.clone());
        out
    }

    /// `start` and every file it imports, transitively. Tarjan's algorithm, iteratively: every member of an
    /// import cycle gets the same set, so a package whose `__init__` imports most of it computes that set once.
    fn reachable(&mut self, inp: &Inputs, start: &RelPath) -> Arc<Bits> {
        if let Some(r) = self.reach.get(start) {
            return r.clone();
        }
        let mut index: HashMap<RelPath, usize> = HashMap::from([(start.clone(), 0)]);
        let mut low: HashMap<RelPath, usize> = HashMap::from([(start.clone(), 0)]);
        let mut open: Vec<RelPath> = vec![start.clone()];
        let mut on_open: HashSet<RelPath> = HashSet::from([start.clone()]);
        let mut calls: Vec<(RelPath, Edges, usize)> = vec![(start.clone(), self.direct(inp, start), 0)];
        while let Some((node, edges, pos)) = calls.last_mut() {
            let mut descend = None;
            while *pos < edges.len() {
                let nxt = &edges[*pos];
                *pos += 1;
                if self.reach.contains_key(nxt) {
                    continue;
                }
                match index.get(nxt) {
                    None => {
                        descend = Some(nxt.clone());
                        break;
                    }
                    Some(&i) if on_open.contains(nxt) => {
                        let l = low.get_mut(node).expect("visited");
                        *l = (*l).min(i);
                    }
                    Some(_) => {}
                }
            }
            if let Some(nxt) = descend {
                let i = index.len();
                index.insert(nxt.clone(), i);
                low.insert(nxt.clone(), i);
                open.push(nxt.clone());
                on_open.insert(nxt.clone());
                let edges = self.direct(inp, &nxt);
                calls.push((nxt, edges, 0));
                continue;
            }
            let (node, _, _) = calls.pop().expect("non-empty");
            let node_low = low[&node];
            if let Some((parent, _, _)) = calls.last() {
                let l = low.get_mut(parent).expect("visited");
                *l = (*l).min(node_low);
            }
            if node_low != index[&node] {
                continue;
            }
            let mut members: HashSet<RelPath> = HashSet::new();
            loop {
                let m = open.pop().expect("node is open");
                on_open.remove(&m);
                let done = m == node;
                members.insert(m);
                if done {
                    break;
                }
            }
            let mut reach = Bits::default();
            for m in &members {
                reach.insert(self.id(m));
                for nxt in self.direct.get(m).expect("resolved").iter() {
                    if !members.contains(nxt) {
                        reach.union(&self.reach[nxt]);
                    }
                }
            }
            let reach = Arc::new(reach);
            for m in members {
                self.reach.insert(m, reach.clone());
            }
        }
        self.reach[start].clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fingerprint::tests::tmp_dir;
    use std::path::PathBuf;

    pub(crate) const FILES: &[(&str, &str)] = &[
        ("pyproject.toml", "[tool.pytest.ini_options]\n"),
        ("conftest.py", "import helpers\n\ndef fx():\n    return helpers.h()\n"),
        ("helpers.py", "def h():\n    return 1\n"),
        ("config.py", "X = 1\n"),
        ("pkg/__init__.py", ""),
        ("pkg/core.py", "from pkg import util\nfrom config import X\n\ndef f():\n    return util.g() + X\n"),
        ("pkg/util.py", "def g():\n    return 2\n"),
        ("src/lib/__init__.py", ""),
        ("src/lib/thing.py", "def t():\n    return 3\n"),
        ("tests/conftest.py", ""),
        ("tests/test_core.py", "from pkg.core import f\nimport lib.thing\n\ndef test_f():\n    assert f()\n"),
        ("tests/sub/local_helper.py", "def lh():\n    return 4\n"),
        ("tests/sub/test_deep.py", "import local_helper\nimport newmod\n\ndef test_deep():\n    assert local_helper.lh()\n"),
        ("data.json", "{}"),
    ];

    pub(crate) fn project(name: &str) -> PathBuf {
        let root = tmp_dir(name);
        for (rel, text) in FILES {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        root
    }

    pub(crate) fn edit(root: &Path, rel: &str, old: &str, new: &str) {
        let p = root.join(rel);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains(old), "{rel} has no {old:?}");
        std::fs::write(p, text.replace(old, new)).unwrap();
    }

    fn graph(root: &Path, known: &[(&str, &str)]) -> ImportGraph {
        let known = known.iter().map(|(m, p)| (m.to_string(), Arc::from(*p))).collect();
        ImportGraph::new(root, Arc::new(Fingerprints::new(root, HashMap::new())), known)
    }

    fn sorted(set: &HashSet<RelPath>) -> Vec<String> {
        let mut out: Vec<String> = set.iter().map(|p| p.to_string()).collect();
        out.sort();
        out
    }

    #[test]
    fn closure_follows_absolute_relative_src_and_conftest_imports() {
        let root = project("closure");
        assert_eq!(
            sorted(&graph(&root, &[]).closure("tests/test_core.py")),
            [
                "config.py",
                "conftest.py",
                "helpers.py",
                "pkg/__init__.py",
                "pkg/core.py",
                "pkg/util.py",
                "src/lib/__init__.py",
                "src/lib/thing.py",
                "tests/conftest.py",
                "tests/test_core.py"
            ]
        );
        std::fs::write(root.join("pkg/rel.py"), "from . import util\nfrom .sub.x import y\n").unwrap();
        std::fs::create_dir_all(root.join("pkg/sub")).unwrap();
        std::fs::write(root.join("pkg/sub/x.py"), "from .. import core\n").unwrap();
        assert_eq!(
            sorted(&graph(&root, &[]).closure("pkg/rel.py")),
            ["config.py", "conftest.py", "helpers.py", "pkg/__init__.py", "pkg/core.py", "pkg/rel.py", "pkg/sub/x.py", "pkg/util.py"]
        );
    }

    #[test]
    fn closure_resolves_through_the_test_directory_and_known_modules() {
        let root = project("graph-known");
        let closure = graph(&root, &[]).closure("tests/sub/test_deep.py");
        assert!(closure.contains("tests/sub/local_helper.py"));
        assert!(closure.contains("conftest.py") && closure.contains("tests/conftest.py"));
        assert!(!closure.iter().any(|p| p.starts_with("pkg/")));
        let known = graph(&root, &[("newmod", "src/lib/thing.py")]);
        assert!(known.closure("tests/sub/test_deep.py").contains("src/lib/thing.py"));
        assert_eq!(
            graph(&root, &[]).conftests_for("tests/sub/test_deep.py").iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            ["conftest.py", "tests/conftest.py"]
        );
    }

    #[test]
    fn reachable_matches_a_plain_walk_on_graphs_with_cycles() {
        let root = tmp_dir("tarjan");
        let mut seed: u64 = 3;
        let mut rng = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for trial in 0..200 {
            let nodes: Vec<RelPath> = (0..1 + rng(25)).map(|k| Arc::from(format!("m{k}.py"))).collect();
            let edges: HashMap<RelPath, Vec<RelPath>> = nodes
                .iter()
                .map(|n| {
                    let mut out: Vec<RelPath> = (0..rng(5)).map(|_| nodes[rng(nodes.len())].clone()).collect();
                    out.retain(|m| m != n);
                    out.dedup();
                    (n.clone(), out)
                })
                .collect();
            let g = graph(&root, &[]);
            let mut st = g.state.lock().unwrap();
            for (n, e) in &edges {
                st.direct.insert(n.clone(), e.iter().cloned().collect());
            }
            for start in &nodes {
                let mut seen: HashSet<RelPath> = HashSet::new();
                let mut stack = vec![start.clone()];
                while let Some(n) = stack.pop() {
                    if seen.insert(n.clone()) {
                        stack.extend(edges[&n].iter().cloned());
                    }
                }
                let reach = st.reachable(&g.inputs, start);
                assert_eq!(st.paths(&reach), seen, "trial {trial} from {start}");
            }
        }
    }

    #[test]
    fn a_persisted_graph_is_reused_until_its_directories_imports_or_modules_change() {
        let root = project("persist");
        let deep = "tests/sub/test_deep.py";
        let fresh = || Arc::new(Fingerprints::new(&root, HashMap::new()));
        let planted_state = || {
            let g = graph(&root, &[]);
            g.closure(deep);
            let mut state: serde_json::Value = serde_json::from_str(&g.export().unwrap()).unwrap();
            state["nodes"][deep][2].as_array_mut().unwrap().push("planted.py".into());
            assert!(g.export().is_some());
            state.to_string()
        };
        let reused = |state: &str, known: &[(&str, &str)]| {
            let known = known.iter().map(|(m, p)| (m.to_string(), Arc::from(*p))).collect();
            let g = ImportGraph::reusing(Some(state), &root, fresh(), known);
            sorted(&g.closure(deep))
        };
        let has = |c: &[String], p: &str| c.iter().any(|x| x == p);

        let state = planted_state();
        assert!(has(&reused(&state, &[]), "planted.py"), "a matching state is reused");
        assert!(!has(&reused(&state, &[("other", "config.py")]), "planted.py"));
        edit(&root, "tests/sub/local_helper.py", "def lh():", "import helpers\n\ndef lh():");
        let c = reused(&state, &[]);
        assert!(has(&c, "planted.py") && has(&c, "helpers.py"), "only the file whose imports changed is resolved again");
        std::fs::write(root.join("tests/sub/newmod.py"), "Y = 1\n").unwrap();
        assert_eq!(reused(&state, &[]), sorted(&graph(&root, &[]).closure(deep)));
        assert!(has(&reused(&state, &[]), "tests/sub/newmod.py"));

        let state = planted_state();
        edit(&root, deep, "import newmod", "import newmod, config");
        let c = reused(&state, &[]);
        assert!(!has(&c, "planted.py") && has(&c, "config.py"));

        let g = ImportGraph::reusing(Some(&planted_state()), &root, fresh(), HashMap::new());
        g.conftests_for(deep);
        assert_eq!(g.export(), None, "nothing resolved, nothing to persist");
    }
}

#[cfg(test)]
mod parity {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    const ROOT: &str = "/tmp/forktest-pandas/pandas";
    const SCRIPT: &str = r#"
import json, sys
from forktest.fingerprint import Fingerprints
from forktest.selection import ImportGraph
root = sys.argv[1]
graph = ImportGraph(root, Fingerprints(root))
print(json.dumps({p: sorted(graph.closure(p)) for p in sys.stdin.read().splitlines()}))
"#;

    /// Closures of pandas' frame tests against the Python `ImportGraph`.
    #[test]
    fn closures_match_the_python_reference() {
        let tests_dir = Path::new(ROOT).join("pandas/tests/frame");
        let Some(reference) = crate::fingerprint::parity::python_reference() else {
            eprintln!("skipped: set FORKTEST_PY_SRC to a checkout of main's src to compare with the Python forktest");
            return;
        };
        if !tests_dir.exists() {
            return;
        }
        let mut files = Vec::new();
        let mut stack = vec![tests_dir];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                if p.is_dir() {
                    stack.push(p);
                } else if name.starts_with("test_") && name.ends_with(".py") {
                    files.push(p.strip_prefix(ROOT).unwrap().to_string_lossy().to_string());
                }
            }
        }
        let mut child = Command::new(crate::python::dev_python())
            .args(["-c", SCRIPT, ROOT])
            .env("PYTHONPATH", &reference)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(files.join("\n").as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let expected: HashMap<String, Vec<String>> = serde_json::from_slice(&output.stdout).unwrap();
        let graph = ImportGraph::new(Path::new(ROOT), Arc::new(Fingerprints::new(Path::new(ROOT), HashMap::new())), HashMap::new());
        let mut sizes = 0;
        for f in &files {
            let mut got: Vec<String> = graph.closure(f).iter().map(|p| p.to_string()).collect();
            got.sort();
            assert_eq!(&got, &expected[f], "{f}");
            sizes += got.len();
        }
        eprintln!("graph parity: {} test files, {} closure entries, all equal", files.len(), sizes);
    }
}
