//! Function-level fingerprints of Python files, and the stat-trusted cache of them. Unit U1.
//!
//! A fingerprint maps each function's CPython `co_qualname` to a hash of its definition with docstrings
//! removed, plus `<module>`: the file with every function body replaced by `...`. Hashes come from
//! `ruff_python_ast::comparable`, so formatting, comments and parentheses do not count.

use rayon::iter::{IntoParallelIterator, ParallelIterator};
use ruff_python_ast::comparable::{ComparableMod, ComparableStmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use thin_vec::ThinVec;
use xxhash_rust::xxh3::{Xxh3, xxh3_128, xxh3_128_with_seed};

pub type RelPath = Arc<str>;
pub type Hash128 = u128;
pub type Fingerprint = HashMap<Arc<str>, Hash128>;

pub const MODULE: &str = "<module>";
pub const FILE: &str = "<file>";

static MODULE_KEY: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from(MODULE));
static FILE_KEY: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from(FILE));

const SYNTHETIC: &[&str] = &["<lambda>", "<genexpr>", "<listcomp>", "<dictcomp>", "<setcomp>"];
const RACY_NS: i64 = 2_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Dep {
    pub path: RelPath,
    /// A qualname, `<module>` or `<file>`.
    pub qualname: Arc<str>,
    pub hash: Hash128,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImportStmt {
    pub module: String,
    pub level: u32,
    pub names: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub size: i64,
}

impl Stat {
    fn of(meta: &std::fs::Metadata) -> Stat {
        Stat {
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
            size: meta.size() as i64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FileKind {
    Python { fingerprint: Fingerprint, imports: Vec<ImportStmt> },
    /// A `.py` file that does not parse. Every dependency on it counts as changed.
    Unparseable,
    /// Anything else: data files, compiled extensions. Its only key is `<file>`, the content hash.
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub sha: Hash128,
    pub stat: Option<Stat>,
    pub kind: FileKind,
}

impl FileInfo {
    /// The current hash of `qualname` in this file, after mapping a lambda or comprehension onto its enclosing
    /// def and anything without a def of its own onto `<module>`.
    pub fn hash_of(&self, qualname: &str) -> Option<(Arc<str>, Hash128)> {
        match &self.kind {
            FileKind::Python { fingerprint, .. } => {
                let mut q = qualname;
                loop {
                    if let Some((k, h)) = fingerprint.get_key_value(q) {
                        return Some((k.clone(), *h));
                    }
                    match q.rfind(".<locals>.") {
                        Some(i) if SYNTHETIC.contains(&&q[i + ".<locals>.".len()..]) => q = &q[..i],
                        _ => return fingerprint.get_key_value(MODULE).map(|(k, h)| (k.clone(), *h)),
                    }
                }
            }
            // The same content keeps the same hash, so an unchanged broken file does not select its tests again.
            FileKind::Unparseable => Some((MODULE_KEY.clone(), xxh3_128_with_seed(&self.sha.to_be_bytes(), 1))),
            FileKind::Other => Some((FILE_KEY.clone(), self.sha)),
        }
    }

    pub fn imports(&self) -> &[ImportStmt] {
        match &self.kind {
            FileKind::Python { imports, .. } => imports,
            _ => &[],
        }
    }

    /// The info of `data` as the content of `path`, with no stat.
    pub fn from_bytes(path: &str, data: &[u8]) -> FileInfo {
        FileInfo::compute(path, data, xxh3_128(data))
    }

    fn compute(path: &str, data: &[u8], sha: Hash128) -> FileInfo {
        let kind = if !path.ends_with(".py") {
            FileKind::Other
        } else {
            match std::str::from_utf8(data).ok().and_then(analyze) {
                Some((fingerprint, imports)) => FileKind::Python { fingerprint, imports },
                None => FileKind::Unparseable,
            }
        };
        FileInfo { sha, stat: None, kind }
    }
}

impl FileKind {
    /// A compact binary form for the store, which reads every file's info on every run.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            FileKind::Python { fingerprint, imports } => {
                out.push(0);
                put_u32(&mut out, fingerprint.len() as u32);
                for (q, h) in fingerprint {
                    put_str(&mut out, q);
                    out.extend_from_slice(&h.to_le_bytes());
                }
                put_u32(&mut out, imports.len() as u32);
                for stmt in imports {
                    put_str(&mut out, &stmt.module);
                    put_u32(&mut out, stmt.level);
                    put_u32(&mut out, stmt.names.len() as u32);
                    for n in &stmt.names {
                        put_str(&mut out, n);
                    }
                }
            }
            FileKind::Unparseable => out.push(1),
            FileKind::Other => out.push(2),
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<FileKind> {
        let mut r = Reader(bytes);
        let kind = match r.take(1)?[0] {
            0 => {
                let n = r.u32()? as usize;
                let mut fingerprint = HashMap::with_capacity(n);
                for _ in 0..n {
                    let q: Arc<str> = Arc::from(r.str()?);
                    fingerprint.insert(q, u128::from_le_bytes(r.take(16)?.try_into().ok()?));
                }
                let m = r.u32()? as usize;
                let mut imports = Vec::with_capacity(m);
                for _ in 0..m {
                    let module = r.str()?.to_string();
                    let level = r.u32()?;
                    let k = r.u32()? as usize;
                    let names = (0..k).map(|_| r.str().map(str::to_string)).collect::<Option<_>>()?;
                    imports.push(ImportStmt { module, level, names });
                }
                FileKind::Python { fingerprint, imports }
            }
            1 => FileKind::Unparseable,
            2 => FileKind::Other,
            _ => return None,
        };
        r.0.is_empty().then_some(kind)
    }
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn str(&mut self) -> Option<&'a str> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.take(n)?).ok()
    }
}

/// The fingerprint and import statements of one source, from one parse.
pub fn analyze(source: &str) -> Option<(Fingerprint, Vec<ImportStmt>)> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut module = ruff_python_parser::parse_module(source).ok()?.into_syntax();
    let mut imports = Vec::new();
    collect_imports(&module.body, &mut imports);
    strip_docstrings(&mut module.body, true);
    let mut fingerprint = HashMap::new();
    collect_defs(&module.body, &mut String::new(), &mut fingerprint);
    stub_function_bodies(&mut module.body);
    fingerprint.insert(MODULE_KEY.clone(), hash128(&ComparableMod::from(&ast::Mod::Module(module))));
    Some((fingerprint, imports))
}

fn hash128(value: &impl Hash) -> Hash128 {
    let mut h = Xxh3::new();
    value.hash(&mut h);
    h.digest128()
}

/// Every statement suite nested directly in `stmt`, in CPython's field order.
fn each_body(stmt: &Stmt, f: &mut dyn FnMut(&[Stmt])) {
    match stmt {
        Stmt::FunctionDef(s) => f(&s.body),
        Stmt::ClassDef(s) => f(&s.body),
        Stmt::If(s) => {
            f(&s.body);
            s.elif_else_clauses.iter().for_each(|c| f(&c.body));
        }
        Stmt::For(s) => {
            f(&s.body);
            f(&s.orelse);
        }
        Stmt::While(s) => {
            f(&s.body);
            f(&s.orelse);
        }
        Stmt::With(s) => f(&s.body),
        Stmt::Try(s) => {
            f(&s.body);
            for ast::ExceptHandler::ExceptHandler(h) in &s.handlers {
                f(&h.body);
            }
            f(&s.orelse);
            f(&s.finalbody);
        }
        Stmt::Match(s) => s.cases.iter().for_each(|c| f(&c.body)),
        _ => {}
    }
}

fn each_body_mut(stmt: &mut Stmt, f: &mut dyn FnMut(&mut ThinVec<Stmt>)) {
    match stmt {
        Stmt::FunctionDef(s) => f(&mut s.body),
        Stmt::ClassDef(s) => f(&mut s.body),
        Stmt::If(s) => {
            f(&mut s.body);
            s.elif_else_clauses.iter_mut().for_each(|c| f(&mut c.body));
        }
        Stmt::For(s) => {
            f(&mut s.body);
            f(&mut s.orelse);
        }
        Stmt::While(s) => {
            f(&mut s.body);
            f(&mut s.orelse);
        }
        Stmt::With(s) => f(&mut s.body),
        Stmt::Try(s) => {
            f(&mut s.body);
            for ast::ExceptHandler::ExceptHandler(h) in &mut s.handlers {
                f(&mut h.body);
            }
            f(&mut s.orelse);
            f(&mut s.finalbody);
        }
        Stmt::Match(s) => s.cases.iter_mut().for_each(|c| f(&mut c.body)),
        _ => {}
    }
}

fn collect_imports(body: &[Stmt], out: &mut Vec<ImportStmt>) {
    for stmt in body {
        match stmt {
            Stmt::Import(s) => out.extend(s.names.iter().map(|a| ImportStmt {
                module: a.name.to_string(),
                level: 0,
                names: Vec::new(),
            })),
            Stmt::ImportFrom(s) => out.push(ImportStmt {
                module: s.module.as_ref().map(|m| m.to_string()).unwrap_or_default(),
                level: s.level,
                names: s.names.iter().map(|a| a.name.to_string()).filter(|n| n != "*").collect(),
            }),
            _ => each_body(stmt, &mut |b| collect_imports(b, out)),
        }
    }
}

fn is_docstring(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Expr(e) if matches!(*e.value, Expr::StringLiteral(_)))
}

fn strip_docstrings(body: &mut ThinVec<Stmt>, owner_takes_docstring: bool) {
    if owner_takes_docstring && body.first().is_some_and(is_docstring) {
        body.remove(0);
    }
    for stmt in body.iter_mut() {
        let owner = matches!(stmt, Stmt::FunctionDef(_) | Stmt::ClassDef(_));
        each_body_mut(stmt, &mut |b| strip_docstrings(b, owner));
    }
}

/// `prefix` is the qualname prefix of `body`'s scope, as CPython builds `co_qualname`. A later def with the same
/// qualname replaces an earlier one.
fn collect_defs(body: &[Stmt], prefix: &mut String, out: &mut Fingerprint) {
    for stmt in body {
        let len = prefix.len();
        match stmt {
            Stmt::FunctionDef(f) => {
                prefix.push_str(f.name.as_str());
                out.insert(Arc::from(prefix.as_str()), hash128(&ComparableStmt::from(stmt)));
                prefix.push_str(".<locals>.");
                collect_defs(&f.body, prefix, out);
            }
            Stmt::ClassDef(c) => {
                prefix.push_str(c.name.as_str());
                prefix.push('.');
                collect_defs(&c.body, prefix, out);
            }
            _ => each_body(stmt, &mut |b| collect_defs(b, prefix, out)),
        }
        prefix.truncate(len);
    }
}

fn stub_function_bodies(body: &mut ThinVec<Stmt>) {
    for stmt in body.iter_mut() {
        match stmt {
            Stmt::FunctionDef(f) => f.body.clear(),
            _ => each_body_mut(stmt, &mut stub_function_bodies),
        }
    }
}

/// Current infos of files under a root. Safe to share across threads.
pub struct Fingerprints {
    root: PathBuf,
    known: RwLock<HashMap<RelPath, Arc<FileInfo>>>,
    fresh: Mutex<HashSet<RelPath>>,
    racy_ns: i64,
    /// With a file watcher reporting every change: files looked up since their last reported change, used without a
    /// stat. `None` without a watcher.
    watched: RwLock<Option<HashMap<RelPath, Arc<FileInfo>>>>,
}

impl Fingerprints {
    /// `known` comes from the store. One is reused without reading the file while its stat matches, and after a
    /// read while its content hash matches. A file changed within 2 s of being read keeps no stat (racy, as git).
    pub fn new(root: &Path, known: HashMap<RelPath, FileInfo>) -> Fingerprints {
        Fingerprints {
            root: root.to_path_buf(),
            known: RwLock::new(known.into_iter().map(|(p, i)| (p, Arc::new(i))).collect()),
            fresh: Mutex::new(HashSet::new()),
            racy_ns: RACY_NS,
            watched: RwLock::new(None),
        }
    }

    /// From now on a file looked up once is trusted until `invalidate` names it. The caller must report every change
    /// under the root, outside excluded directories, before each lookup that has to see it.
    pub fn trust_watcher(&self) {
        self.watched.write().unwrap().get_or_insert_with(HashMap::new);
    }

    /// `rel` changed: a file, or a directory with everything under it.
    pub fn invalidate(&self, rel: &str) {
        if let Some(map) = self.watched.write().unwrap().as_mut()
            && map.remove(rel).is_none()
        {
            let prefix = format!("{rel}/");
            map.retain(|p, _| !p.starts_with(&prefix));
        }
    }

    /// The watcher lost events: every file is stat'ed again.
    pub fn invalidate_all(&self) {
        if let Some(map) = self.watched.write().unwrap().as_mut() {
            map.clear();
        }
    }

    /// A watcher reports changes under the root's real path only, so a file reached through a symlink, or in a
    /// directory the watcher skips, is never trusted.
    fn watchable(&self, path: &str) -> bool {
        let full = self.root.join(path);
        !crate::root::is_excluded(path) && !path.starts_with('/') && std::fs::canonicalize(&full).is_ok_and(|real| real == full)
    }

    /// `None` for a missing file.
    pub fn info(&self, path: &str) -> Option<Arc<FileInfo>> {
        self.entry(path).map(|(_, info)| info)
    }

    /// The info with the interned path it is stored under.
    pub fn entry(&self, path: &str) -> Option<(RelPath, Arc<FileInfo>)> {
        let trusted = match self.watched.read().unwrap().as_ref() {
            None => None,
            Some(map) => Some(map.get_key_value(path).map(|(p, i)| (p.clone(), i.clone()))),
        };
        match trusted {
            None => self.lookup(path),
            Some(Some(hit)) => Some(hit),
            Some(None) => {
                let found = self.lookup(path)?;
                if self.watchable(path)
                    && let Some(map) = self.watched.write().unwrap().as_mut()
                {
                    map.insert(found.0.clone(), found.1.clone());
                }
                Some(found)
            }
        }
    }

    fn lookup(&self, path: &str) -> Option<(RelPath, Arc<FileInfo>)> {
        let full = self.root.join(path);
        let stat = Stat::of(&std::fs::metadata(&full).ok()?);
        let known = self.known.read().unwrap().get_key_value(path).map(|(p, i)| (p.clone(), i.clone()));
        if let Some((p, k)) = &known
            && k.stat == Some(stat)
        {
            return Some((p.clone(), k.clone()));
        }
        let data = std::fs::read(&full).ok()?;
        let sha = xxh3_128(&data);
        let mut info = match &known {
            Some((_, k)) if k.sha == sha => FileInfo { sha, stat: None, kind: k.kind.clone() },
            _ => FileInfo::compute(path, &data, sha),
        };
        if now_ns() - stat.mtime_ns.max(stat.ctime_ns) >= self.racy_ns {
            info.stat = Some(stat);
        }
        if let Some((p, k)) = &known
            && k.sha == info.sha
            && k.stat == info.stat
        {
            return Some((p.clone(), k.clone()));
        }
        let path: RelPath = known.map_or_else(|| Arc::from(path), |(p, _)| p);
        let info = Arc::new(info);
        self.known.write().unwrap().insert(path.clone(), info.clone());
        self.fresh.lock().unwrap().insert(path.clone());
        Some((path, info))
    }

    /// Stat and, where needed, read and parse many files in parallel.
    pub fn prefetch<'a>(&self, paths: impl IntoParallelIterator<Item = &'a str>) {
        paths.into_par_iter().for_each(|p| {
            self.info(p);
        });
    }

    /// The dependency a hit on `qualname` in `path` stands for now.
    pub fn dep(&self, path: &str, qualname: &str) -> Option<Dep> {
        let (path, info) = self.entry(path)?;
        let (qualname, hash) = info.hash_of(qualname)?;
        Some(Dep { path, qualname, hash })
    }

    /// Infos that changed since `new`, for the store.
    pub fn take_fresh(&self) -> Vec<(RelPath, FileInfo)> {
        let fresh = std::mem::take(&mut *self.fresh.lock().unwrap());
        let known = self.known.read().unwrap();
        fresh.into_iter().filter_map(|p| known.get(&p).map(|i| (p, FileInfo::clone(i)))).collect()
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SRC: &str = r#"
"""Module docstring."""
import os
from decimal import Decimal

RATE = Decimal("0.2")


def top(x):
    """Doc."""
    def inner(y):
        return y + 1
    return inner(x) * 2


class Billing:
    places = 2

    def total(self, n):
        return n * RATE

    @staticmethod
    def helper():
        return 1


if os.name == "posix":
    def guarded():
        return "posix"
"#;

    pub(crate) fn fp(src: &str) -> Fingerprint {
        analyze(src).expect("parses").0
    }

    fn keys(fp: &Fingerprint) -> Vec<&str> {
        let mut out: Vec<&str> = fp.keys().map(|k| &**k).collect();
        out.sort();
        out
    }

    fn changed(a: &Fingerprint, b: &Fingerprint) -> Vec<String> {
        let mut out: Vec<String> = a.keys().filter(|k| a.get(*k) != b.get(*k)).map(|k| k.to_string()).collect();
        out.sort();
        out
    }

    pub(crate) fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("forktest-u1-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn qualnames_match_co_qualname() {
        assert_eq!(
            keys(&fp(SRC)),
            ["<module>", "Billing.helper", "Billing.total", "guarded", "top", "top.<locals>.inner"]
        );
        let nested = "def f():\n    class C:\n        def m(self):\n            def g(): pass\n    try:\n        def t(): pass\n    except E:\n        def e(): pass\n    with x:\n        def w(): pass\n";
        assert_eq!(
            keys(&fp(nested)),
            [
                "<module>",
                "f",
                "f.<locals>.C.m",
                "f.<locals>.C.m.<locals>.g",
                "f.<locals>.e",
                "f.<locals>.t",
                "f.<locals>.w"
            ]
        );
        let matched = "match x:\n    case 1:\n        def a(): pass\nfor i in y:\n    pass\nelse:\n    async def b(): pass\n";
        assert_eq!(keys(&fp(matched)), ["<module>", "a", "b"]);
    }

    #[test]
    fn a_body_edit_changes_only_that_function_and_its_parents() {
        let after = fp(&SRC.replace("return y + 1", "return y + 2"));
        assert_eq!(changed(&fp(SRC), &after), ["top", "top.<locals>.inner"]);
        let after = fp(&SRC.replace("return n * RATE", "return n * RATE * 2"));
        assert_eq!(changed(&fp(SRC), &after), ["Billing.total"]);
    }

    #[test]
    fn the_last_duplicate_def_wins() {
        let src = "@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\ndef f(x):\n    return x\n";
        let last = fp("def f(x):\n    return x\n")["f"];
        assert_ne!(fp(src)["f"], fp(&src.replace("    return x", "    return 2 * x"))["f"]);
        assert_eq!(fp(src)["f"], last);
    }

    #[test]
    fn docstrings_and_formatting_change_no_hash() {
        let base = fp(SRC);
        let docs = SRC.replace(r#""""Doc.""""#, r#""""A different doc.""""#).replace("Module docstring.", "Other.");
        assert_eq!(fp(&docs), base);
        let moved = SRC.replace("\n\nclass Billing", "\n\n\n\nclass Billing");
        assert_eq!(fp(&moved), base);
        let reformatted = SRC
            .replace("return n * RATE", "return (n * RATE)  # tax")
            .replace(r#"RATE = Decimal("0.2")"#, "RATE = Decimal(\n    '0.2',\n)")
            .replace("def top(x):", "def top(\n    x,\n):")
            .replace("from decimal import Decimal", "from decimal import (\n    Decimal,\n)")
            .replace(r#"os.name == "posix""#, "(os.name == 'posix')");
        assert_eq!(fp(&reformatted), base);
        let docless = SRC.replace("    \"\"\"Doc.\"\"\"\n", "").replace("\"\"\"Module docstring.\"\"\"\n", "");
        assert_eq!(fp(&docless), base);
        let tuples = "def f():\n    return 1, 2\nfor a, b in x, y:\n    pass\n";
        assert_eq!(fp(tuples), fp("def f():\n    return (1, 2)\nfor (a, b) in (x, y):\n    pass\n"));
    }

    #[test]
    fn the_module_hash_covers_constants_signatures_imports_decorators() {
        let base = fp(SRC)[MODULE];
        for (old, new) in [
            (r#"RATE = Decimal("0.2")"#, r#"RATE = Decimal("0.25")"#),
            ("places = 2", "places = 3"),
            ("def top(x):", "def top(x, y=1):"),
            ("import os", "import os, sys"),
            ("@staticmethod", "@classmethod"),
        ] {
            assert_ne!(fp(&SRC.replace(old, new))[MODULE], base, "{old} -> {new}");
        }
        assert_eq!(fp(&SRC.replace("return n * RATE", "return n * RATE * 2"))[MODULE], base);
    }

    #[test]
    fn hash_of_folds_synthetic_scopes_into_their_def() {
        let info = FileInfo {
            sha: 0,
            stat: None,
            kind: FileKind::Python { fingerprint: fp(SRC), imports: Vec::new() },
        };
        let q = |name: &str| info.hash_of(name).unwrap().0.to_string();
        assert_eq!(q("top.<locals>.<lambda>"), "top");
        assert_eq!(q("top.<locals>.inner.<locals>.<genexpr>"), "top.<locals>.inner");
        assert_eq!(q("Billing"), MODULE);
        assert_eq!(q("<lambda>"), MODULE);
        assert_eq!(q("removed_function"), MODULE);
        assert_eq!(q("top.<locals>.gone"), MODULE);
        assert_eq!(q("Billing.total"), "Billing.total");
        let other = FileInfo { sha: 7, stat: None, kind: FileKind::Other };
        assert_eq!(other.hash_of(MODULE), Some((Arc::from(FILE), 7)));
    }

    #[test]
    fn imports_include_nested_and_relative_ones() {
        let src = "import a.b, q as r\nfrom c import d, e\nfrom . import f\nfrom ..g import h\nfrom k import *\ndef lazy():\n    import z\n";
        let got: Vec<(String, u32, Vec<String>)> =
            analyze(src).unwrap().1.into_iter().map(|s| (s.module, s.level, s.names)).collect();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            got,
            [
                ("a.b".into(), 0, s(&[])),
                ("q".into(), 0, s(&[])),
                ("c".into(), 0, s(&["d", "e"])),
                ("".into(), 1, s(&["f"])),
                ("g".into(), 2, s(&["h"])),
                ("k".into(), 0, s(&[])),
                ("z".into(), 0, s(&[])),
            ]
        );
    }

    #[test]
    fn kinds_round_trip_through_their_encoding() {
        let (fingerprint, imports) = analyze("from .a import b\ndef f(): pass\n").unwrap();
        for kind in [FileKind::Python { fingerprint, imports }, FileKind::Unparseable, FileKind::Other] {
            assert_eq!(FileKind::decode(&kind.encode()), Some(kind));
        }
        assert_eq!(FileKind::decode(&[0, 1, 0]), None);
    }

    #[test]
    fn the_cache_tracks_edits_and_non_python_files() {
        let root = tmp_dir("cache");
        std::fs::write(root.join("m.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(root.join("data.json"), "{}").unwrap();
        std::fs::write(root.join("broken.py"), "def (:\n").unwrap();
        let fps = Fingerprints::new(&root, HashMap::new());
        let first = fps.info("m.py").unwrap();
        assert!(Arc::ptr_eq(&first, &fps.info("m.py").unwrap()));
        let f1 = fps.dep("m.py", "f").unwrap();
        std::fs::write(root.join("m.py"), "def f():\n    return 2\n").unwrap();
        let f2 = fps.dep("m.py", "f").unwrap();
        assert_ne!(f1.hash, f2.hash);
        assert_eq!(&*fps.dep("data.json", MODULE).unwrap().qualname, FILE);
        assert!(fps.info("missing.py").is_none());
        let broken = fps.info("broken.py").unwrap();
        assert_eq!(broken.kind, FileKind::Unparseable);
        assert_eq!(&*broken.hash_of("f").unwrap().0, MODULE);
        let mut fresh: Vec<String> = fps.take_fresh().into_iter().map(|(p, _)| p.to_string()).collect();
        fresh.sort();
        assert_eq!(fresh, ["broken.py", "data.json", "m.py"]);
        assert!(fps.take_fresh().is_empty());
    }

    #[test]
    fn a_watched_cache_trusts_a_file_until_the_watcher_names_it() {
        let root = std::fs::canonicalize(tmp_dir("watched")).unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/m.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(root.join("n.py"), "def g():\n    return 1\n").unwrap();
        std::os::unix::fs::symlink(root.join("n.py"), root.join("link.py")).unwrap();
        let fps = Fingerprints::new(&root, HashMap::new());
        fps.trust_watcher();
        let before = |p: &str, q: &str| fps.dep(p, q).unwrap().hash;
        let (m1, n1, l1) = (before("pkg/m.py", "f"), before("n.py", "g"), before("link.py", "g"));
        std::fs::write(root.join("pkg/m.py"), "def f():\n    return 22\n").unwrap();
        std::fs::write(root.join("n.py"), "def g():\n    return 22\n").unwrap();
        assert_eq!(before("pkg/m.py", "f"), m1, "unreported edits are not seen");
        assert_ne!(before("link.py", "g"), l1, "a symlinked file is always stat'ed");
        fps.invalidate("pkg");
        assert_ne!(before("pkg/m.py", "f"), m1, "a directory event covers the files under it");
        assert_eq!(before("n.py", "g"), n1);
        fps.invalidate_all();
        assert_ne!(before("n.py", "g"), n1);
    }

    fn planted(info: &FileInfo) -> FileInfo {
        let mut fingerprint = HashMap::new();
        fingerprint.insert(Arc::from(MODULE), 42);
        FileInfo { kind: FileKind::Python { fingerprint, imports: Vec::new() }, ..info.clone() }
    }

    #[test]
    fn known_infos_are_reused_only_while_the_content_matches() {
        let root = tmp_dir("known");
        std::fs::write(root.join("m.py"), "import os\n\ndef f():\n    return 1\n").unwrap();
        let info = FileInfo::clone(&Fingerprints::new(&root, HashMap::new()).info("m.py").unwrap());
        assert_eq!(info.imports(), [ImportStmt { module: "os".into(), level: 0, names: vec![] }]);
        let known = |i: FileInfo| HashMap::from([(Arc::from("m.py"), i)]);
        let warm = Fingerprints::new(&root, known(planted(&info)));
        assert_eq!(warm.dep("m.py", "f").unwrap().hash, 42, "same content, not parsed again");
        std::fs::write(root.join("m.py"), "def f():\n    return 2\n").unwrap();
        let stale = Fingerprints::new(&root, known(planted(&info)));
        assert_eq!(&*stale.dep("m.py", "f").unwrap().qualname, "f");
        assert!(stale.info("m.py").unwrap().imports().is_empty());
        let fresh = stale.take_fresh();
        assert_eq!(fresh.len(), 1);
        assert_ne!(fresh[0].1.sha, info.sha);
    }

    #[test]
    fn a_later_process_trusts_an_unchanged_stat_and_rereads_after_any_write() {
        let root = tmp_dir("stat");
        let m = root.join("m.py");
        std::fs::write(&m, "def f():\n    return 1\n").unwrap();
        let trusting = |known: HashMap<RelPath, FileInfo>| Fingerprints { racy_ns: 0, ..Fingerprints::new(&root, known) };
        let info = FileInfo::clone(&trusting(HashMap::new()).info("m.py").unwrap());
        assert!(info.stat.is_some());
        let known = || HashMap::from([(Arc::from("m.py"), planted(&info))]);
        assert_eq!(trusting(known()).dep("m.py", "f").unwrap().hash, 42, "same stat, not read");

        let before = std::fs::metadata(&m).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&m, "def f():\n    return 2\n").unwrap();
        std::fs::File::options().write(true).open(&m).unwrap().set_modified(before.modified().unwrap()).unwrap();
        let after = std::fs::metadata(&m).unwrap();
        assert_eq!((after.mtime_nsec(), after.size()), (before.mtime_nsec(), before.size()));
        assert_ne!(trusting(known()).dep("m.py", "f").unwrap().hash, 42);
    }

    #[test]
    fn a_file_read_in_the_racy_window_keeps_no_stat() {
        let root = tmp_dir("racy");
        std::fs::write(root.join("m.py"), "def f():\n    return 1\n").unwrap();
        let fps = Fingerprints::new(&root, HashMap::new());
        let first = fps.info("m.py").unwrap();
        assert_eq!(first.stat, None);
        let again = Fingerprints::new(&root, HashMap::from([(Arc::from("m.py"), planted(&first))]));
        assert_eq!(again.dep("m.py", "f").unwrap().hash, 42, "read again, content matched");
        assert!(again.take_fresh().is_empty());
    }

    #[test]
    fn prefetch_fills_the_cache_in_parallel() {
        let root = tmp_dir("prefetch");
        let names: Vec<String> = (0..64).map(|i| format!("m{i}.py")).collect();
        for n in &names {
            std::fs::write(root.join(n), format!("def {}():\n    pass\n", n.trim_end_matches(".py"))).unwrap();
        }
        let fps = Fingerprints::new(&root, HashMap::new());
        fps.prefetch(names.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(fps.take_fresh().len(), 64);
        assert_eq!(&*fps.dep("m7.py", "m7").unwrap().qualname, "m7");
    }
}

#[cfg(test)]
mod parity {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    const PYTHON: &str = "/Users/katakam/repos/faster_pytest/.venv/bin/python";
    const SCRIPT: &str = r#"
import json, sys
from forktest.fingerprint import fingerprint
out = {}
for path in sys.stdin.read().splitlines():
    try:
        out[path] = sorted(fingerprint(open(path, "rb").read()))
    except (SyntaxError, ValueError):
        out[path] = None
print(json.dumps(out))
"#;

    fn py_files(dir: &Path, out: &mut Vec<PathBuf>, recursive: bool) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && recursive {
                py_files(&p, out, true);
            } else if p.extension().is_some_and(|x| x == "py") {
                out.push(p);
            }
        }
    }

    /// `FORKTEST_PARITY_EXTRA` adds a directory tree to compare.
    #[test]
    fn qualnames_match_the_python_reference() {
        if !Path::new(PYTHON).exists() {
            return;
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        py_files(&repo.join("examples"), &mut files, true);
        py_files(&repo.join("src/forktest"), &mut files, false);
        py_files(Path::new("/tmp/forktest-pandas/pandas/pandas/core"), &mut files, false);
        if let Some(extra) = std::env::var_os("FORKTEST_PARITY_EXTRA") {
            py_files(Path::new(&extra), &mut files, true);
        }
        let mut child = Command::new(PYTHON)
            .args(["-c", SCRIPT])
            .env("PYTHONPATH", repo.join("src"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let list: String = files.iter().map(|p| format!("{}\n", p.display())).collect();
        child.stdin.take().unwrap().write_all(list.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let expected: HashMap<String, Option<Vec<String>>> = serde_json::from_slice(&output.stdout).unwrap();
        let mut mismatches = Vec::new();
        for path in &files {
            let data = std::fs::read(path).unwrap();
            let got = std::str::from_utf8(&data).ok().and_then(analyze).map(|(fp, _)| {
                let mut keys: Vec<String> = fp.into_keys().map(|k| k.to_string()).collect();
                keys.sort();
                keys
            });
            if got != expected[&path.display().to_string()] {
                mismatches.push(path.display().to_string());
            }
        }
        eprintln!("parity: {} files compared, {} mismatched", files.len(), mismatches.len());
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    /// `cargo test --release analyze_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn analyze_speed() {
        let path = "/tmp/forktest-pandas/pandas/pandas/core/frame.py";
        let Ok(src) = std::fs::read_to_string(path) else { return };
        let mut times: Vec<f64> = (0..21)
            .map(|_| {
                let t = std::time::Instant::now();
                std::hint::black_box(analyze(&src).unwrap());
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(f64::total_cmp);
        eprintln!("analyze frame.py ({} bytes): median {:.2} ms, min {:.2} ms", src.len(), times[10], times[0]);
    }
}
