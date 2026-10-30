//! Which interpreter runs the tests, and the shim it runs. Unit U2.

use std::path::{Path, PathBuf};

pub const SHIM_SOURCE: &str = include_str!("../shim/forktest_shim.py");

/// `FORKTEST_PYTHON`, then `$VIRTUAL_ENV/bin/python`, then `<root>/.venv/bin/python`, then `python3` on PATH.
pub fn find_python(root: &Path) -> anyhow::Result<PathBuf> {
    todo!("U2")
}

/// Writes the embedded shim once per content hash under the user cache dir and returns its path.
pub fn shim_path() -> anyhow::Result<PathBuf> {
    todo!("U2")
}
