//! Static import edges between project files, and each test file's closure. Unit U1.
//!
//! Port of `forktest.selection.ImportGraph`: resolution through the persisted `sys.modules` map first, then the
//! file system under the root, `src/` and the importer's top-level directory; Tarjan closures; edges persisted
//! with their inputs (imports digest, directory listing digests, modules digest) and reused while they match.

use crate::fingerprint::{Fingerprints, RelPath};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

pub struct ImportGraph {}

impl ImportGraph {
    pub fn reusing(
        state: Option<&str>,
        root: &Path,
        fps: Arc<Fingerprints>,
        known_modules: HashMap<String, RelPath>,
    ) -> ImportGraph {
        todo!("U1")
    }

    /// Every project file the test file's import, and its conftests' imports, can reach.
    pub fn closure(&self, test_file: &str) -> Arc<HashSet<RelPath>> {
        todo!("U1")
    }

    pub fn conftests_for(&self, path: &str) -> Vec<RelPath> {
        todo!("U1")
    }

    /// The state for `reusing`, or `None` when nothing new was resolved.
    pub fn export(&self) -> Option<String> {
        todo!("U1")
    }
}
