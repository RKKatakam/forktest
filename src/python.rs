//! Which interpreter runs the tests, and the shim it runs. Unit U2.

use anyhow::Context;
use std::path::{Path, PathBuf};

pub const SHIM_SOURCE: &str = include_str!("../shim/forktest_shim.py");

/// `FORKTEST_PYTHON`, then `$VIRTUAL_ENV/bin/python`, then `<root>/.venv/bin/python`, then `python3` on PATH.
pub fn find_python(root: &Path) -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("FORKTEST_PYTHON").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let candidates = std::env::var_os("VIRTUAL_ENV")
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(v).join("bin/python"))
        .into_iter()
        .chain([root.join(".venv/bin/python")]);
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|p| p.is_file())
        .context("no Python found: set FORKTEST_PYTHON, activate a virtualenv, or put python3 on PATH")
}

/// Writes the embedded shim once per content hash under the user cache dir and returns its path.
pub fn shim_path() -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("forktest");
    let path = dir.join(format!("forktest_shim-{:016x}.py", xxhash_rust::xxh3::xxh3_64(SHIM_SOURCE.as_bytes())));
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".shim-{}-{:?}.tmp", std::process::id(), std::thread::current().id()));
    std::fs::write(&tmp, SHIM_SOURCE).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(path)
}

fn cache_dir() -> anyhow::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg));
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    Ok(if cfg!(target_os = "macos") { home.join("Library/Caches") } else { home.join(".cache") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_is_written_once_with_the_embedded_source() {
        let a = shim_path().unwrap();
        let b = shim_path().unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), SHIM_SOURCE);
    }
}
