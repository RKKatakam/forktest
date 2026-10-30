//! Where the project is, and which of its paths matter. Unit U1.

use std::path::{Path, PathBuf};

pub const CONFIG_FILES: &[&str] =
    &["pytest.ini", ".pytest.ini", "pyproject.toml", "setup.cfg", "tox.ini", "uv.lock", "poetry.lock"];
pub const EXCLUDED_DIRS: &[&str] = &[
    ".git", ".hg", ".venv", "venv", ".tox", ".nox", "__pycache__", ".forktest", ".pytest_cache", ".hypothesis",
    ".mypy_cache", ".ruff_cache", "node_modules", "site-packages", "target",
];

/// pytest's rootdir, approximated without Python: an ini file with a pytest section, then a project file, then `.git`.
pub fn find_root(start: &Path) -> PathBuf {
    todo!("U1: port forktest.cli.find_root")
}

/// `requirements*.txt` counts too.
pub fn is_config_file(rel: &str) -> bool {
    todo!("U1")
}

/// The config files present directly under `root`, sorted.
pub fn config_files(root: &Path) -> Vec<String> {
    todo!("U1")
}

pub fn is_excluded(rel: &str) -> bool {
    todo!("U1")
}
