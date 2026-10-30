//! Function-level fingerprints of Python files, and the stat-trusted cache of them. Unit U1.
//!
//! A fingerprint maps each function's CPython `co_qualname` to a hash of its definition with docstrings
//! removed, plus `<module>`: the file with every function body replaced by `...`. Hashes come from
//! `ruff_python_ast::comparable`, so formatting, comments and parentheses do not count.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub type RelPath = Arc<str>;
pub type Hash128 = u128;

pub const MODULE: &str = "<module>";
pub const FILE: &str = "<file>";

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Dep {
    pub path: RelPath,
    /// A qualname, `<module>` or `<file>`.
    pub qualname: Arc<str>,
    pub hash: Hash128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FileKind {
    Python { fingerprint: HashMap<Arc<str>, Hash128>, imports: Vec<ImportStmt> },
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
        todo!("U1")
    }
}

/// The fingerprint and import statements of one source, from one parse.
pub fn analyze(source: &str) -> Option<(HashMap<Arc<str>, Hash128>, Vec<ImportStmt>)> {
    todo!("U1")
}

/// Current infos of files under a root. Safe to share across threads.
pub struct Fingerprints {}

impl Fingerprints {
    /// `known` comes from the store. One is reused without reading the file while its stat matches, and after a
    /// read while its content hash matches. A file changed within 2 s of being read keeps no stat (racy, as git).
    pub fn new(root: &Path, known: HashMap<RelPath, FileInfo>) -> Fingerprints {
        todo!("U1")
    }

    /// `None` for a missing file.
    pub fn info(&self, path: &str) -> Option<Arc<FileInfo>> {
        todo!("U1")
    }

    /// Stat and, where needed, read and parse many files in parallel.
    pub fn prefetch<'a>(&self, paths: impl IntoParallelIterator<Item = &'a str>) {
        todo!("U1")
    }

    /// The dependency a hit on `qualname` in `path` stands for now.
    pub fn dep(&self, path: &str, qualname: &str) -> Option<Dep> {
        todo!("U1")
    }

    /// Infos that changed since `new`, for the store.
    pub fn take_fresh(&self) -> Vec<(RelPath, FileInfo)> {
        todo!("U1")
    }
}

use rayon::iter::IntoParallelIterator;
