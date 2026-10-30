//! Which interpreter runs the tests, and the shim it runs. Unit U2.

use anyhow::Context;
use std::path::{Path, PathBuf};

pub const SHIM_SOURCE: &str = include_str!("../shim/forktest_shim.py");

/// The environment variables discovery reads, so the search itself is a pure function of them.
#[derive(Default)]
pub struct Env {
    pub forktest_python: Option<PathBuf>,
    pub virtual_env: Option<PathBuf>,
    pub conda_prefix: Option<PathBuf>,
    pub path: Option<std::ffi::OsString>,
}

impl Env {
    pub fn current() -> Env {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        Env {
            forktest_python: var("FORKTEST_PYTHON").map(PathBuf::from),
            virtual_env: var("VIRTUAL_ENV").map(PathBuf::from),
            conda_prefix: var("CONDA_PREFIX").map(PathBuf::from),
            path: var("PATH"),
        }
    }
}

const VENV_DIRS: &[&str] = &[".venv", "venv", "env", ".env"];

/// The interpreter of the project at `root`, without starting Python: `FORKTEST_PYTHON`; an activated virtualenv
/// (`uv run` and `poetry run` activate one); a virtualenv directory in the root or an ancestor (a monorepo keeps
/// one at the top); an activated conda env; Poetry's out-of-tree env; `python3` or `python` on PATH.
pub fn find_python(root: &Path) -> anyhow::Result<PathBuf> {
    find_in(root, &Env::current()).context(
        "no Python found: create a virtualenv in the project (uv venv), activate one, or set FORKTEST_PYTHON",
    )
}

pub fn find_in(root: &Path, env: &Env) -> Option<PathBuf> {
    if let Some(p) = &env.forktest_python {
        return Some(p.clone());
    }
    if let Some(p) = env.virtual_env.as_deref().and_then(venv_python) {
        return Some(p);
    }
    for dir in root.ancestors() {
        if let Some(p) = VENV_DIRS.iter().find_map(|name| venv_python(&dir.join(name))) {
            return Some(p);
        }
    }
    if let Some(p) = env.conda_prefix.as_ref().map(|c| c.join("bin/python")).filter(|p| p.is_file()) {
        return Some(p);
    }
    if let Some(p) = poetry_env(root).as_deref().and_then(venv_python) {
        return Some(p);
    }
    let path = env.path.clone().unwrap_or_default();
    ["python3", "python"]
        .iter()
        .find_map(|name| std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file()))
}

/// `dir/bin/python` when `dir` is a virtualenv. Requiring `pyvenv.cfg` keeps a dotenv file or an unrelated `env/`
/// directory from counting.
fn venv_python(dir: &Path) -> Option<PathBuf> {
    let python = dir.join("bin/python");
    (dir.join("pyvenv.cfg").is_file() && python.is_file()).then_some(python)
}

/// Poetry keeps its envs outside the project unless `virtualenvs.in-project` is set. Asking it costs a process,
/// so only a Poetry project with no local env pays it.
fn poetry_env(root: &Path) -> Option<PathBuf> {
    let pyproject = std::fs::read_to_string(root.join("pyproject.toml")).ok()?;
    if !pyproject.contains("[tool.poetry") {
        return None;
    }
    let out = std::process::Command::new("poetry")
        .args(["env", "info", "--path"])
        .current_dir(root)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    (out.status.success() && !path.trim().is_empty()).then(|| PathBuf::from(path.trim()))
}

/// Whether an interpreter can run the shim, which needs `sys.monitoring` (3.12) and pytest.
#[derive(Clone, Debug, PartialEq)]
pub enum Support {
    Ready,
    TooOld { version: String },
    NoPytest,
    /// The interpreter did not start or answer.
    Broken { message: String },
}

impl Support {
    /// What to tell the user before running plain pytest instead.
    pub fn explain(&self, python: &Path) -> Option<String> {
        let python = python.display();
        match self {
            Support::Ready => None,
            Support::TooOld { version } => {
                Some(format!("{python} is Python {version} and forktest needs 3.12 or newer; running plain pytest"))
            }
            Support::NoPytest => Some(format!("pytest is not installed for {python} (try `uv add --dev pytest`)")),
            Support::Broken { message } => Some(format!("{python} did not start: {message}")),
        }
    }
}

const PROBE: &str = "import sys, importlib.util\n\
print('%d.%d.%d' % sys.version_info[:3], importlib.util.find_spec('pytest') is not None)";

/// A ready interpreter is remembered until its environment changes, so only the first run pays the probe.
pub fn check(python: &Path) -> Support {
    let marker = probe_marker(python);
    if marker.as_ref().is_some_and(|m| m.is_file()) {
        return Support::Ready;
    }
    let out = match std::process::Command::new(python).args(["-I", "-c", PROBE]).output() {
        Ok(out) if out.status.success() => out,
        Ok(out) => return Support::Broken { message: String::from_utf8_lossy(&out.stderr).trim().to_string() },
        Err(e) => return Support::Broken { message: e.to_string() },
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    let version = fields.next().unwrap_or_default().to_string();
    let has_pytest = fields.next() == Some("True");
    let mut parts = version.split('.').map(|n| n.parse::<u32>().unwrap_or(0));
    if (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) < (3, 12) {
        return Support::TooOld { version };
    }
    if !has_pytest {
        return Support::NoPytest;
    }
    if let Some(m) = marker {
        let _ = std::fs::create_dir_all(m.parent().expect("in the cache dir")).and_then(|_| std::fs::write(&m, ""));
    }
    Support::Ready
}

/// Keyed by the interpreter path and the stat of its env's `pyvenv.cfg` (or the binary outside a venv), which a
/// recreated env or a reinstalled interpreter changes. Uninstalling pytest later is not seen here; the shim then
/// fails to import it with pytest's own error.
fn probe_marker(python: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let cfg = python.parent()?.parent()?.join("pyvenv.cfg");
    let meta = std::fs::metadata(&cfg).or_else(|_| std::fs::metadata(python)).ok()?;
    let key = format!("{}\0{}\0{}\0{}", python.display(), meta.ino(), meta.mtime_nsec(), meta.size());
    Some(cache_dir().ok()?.join("forktest").join(format!("python-ok-{:016x}", xxhash_rust::xxh3::xxh3_64(key.as_bytes()))))
}

/// Writes the embedded shim once per content hash under the user cache dir and returns its path.
pub fn shim_path() -> anyhow::Result<PathBuf> {
    cached("forktest_shim", "py", SHIM_SOURCE.as_bytes())
}

#[cfg(target_os = "macos")]
const TZFIX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tzfix.dylib"));

/// macOS only: a dylib for `DYLD_INSERT_LIBRARIES` that keeps libc's cached time zone in forked children. Without
/// it a child cannot reach notifyd, libc treats every `localtime()` as a possible zone change and rereads the zone
/// file, and `datetime.now()` gets about 700 times slower than in pytest's own unforked process.
pub fn tzfix_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return cached("forktest_tzfix", "dylib", TZFIX).ok();
    #[cfg(not(target_os = "macos"))]
    None
}

fn cached(stem: &str, ext: &str, content: &[u8]) -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("forktest");
    let path = dir.join(format!("{stem}-{:016x}.{ext}", xxhash_rust::xxh3::xxh3_64(content)));
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{stem}-{}-{:?}.tmp", std::process::id(), std::thread::current().id()));
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
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

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("forktest-python-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn venv(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        std::fs::write(dir.join("bin/python"), "").unwrap();
        dir.join("bin/python")
    }

    #[test]
    fn an_explicit_or_activated_interpreter_wins_over_the_project_venv() {
        let root = tmp("explicit");
        venv(&root.join(".venv"));
        let active = venv(&root.join("elsewhere"));
        let env = Env { virtual_env: Some(root.join("elsewhere")), ..Env::default() };
        assert_eq!(find_in(&root, &env), Some(active));
        let env = Env { forktest_python: Some("/opt/py".into()), virtual_env: Some(root.join("elsewhere")), ..Env::default() };
        assert_eq!(find_in(&root, &env), Some(PathBuf::from("/opt/py")));
    }

    #[test]
    fn a_venv_directory_in_the_root_or_an_ancestor_is_found() {
        let top = tmp("ancestor");
        let pkg = top.join("packages/core");
        std::fs::create_dir_all(&pkg).unwrap();
        let shared = venv(&top.join("venv"));
        assert_eq!(find_in(&pkg, &Env::default()), Some(shared));
        let own = venv(&pkg.join(".venv"));
        assert_eq!(find_in(&pkg, &Env::default()), Some(own));
    }

    #[test]
    fn a_directory_without_pyvenv_cfg_is_not_a_venv() {
        let root = tmp("dotenv");
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        std::fs::create_dir_all(root.join("env/bin")).unwrap();
        std::fs::write(root.join("env/bin/python"), "").unwrap();
        let conda = tmp("conda");
        std::fs::create_dir_all(conda.join("bin")).unwrap();
        std::fs::write(conda.join("bin/python"), "").unwrap();
        let env = Env { conda_prefix: Some(conda.clone()), ..Env::default() };
        assert_eq!(find_in(&root, &env), Some(conda.join("bin/python")));
    }

    #[test]
    fn path_is_the_last_resort() {
        let root = tmp("path");
        let bin = tmp("path-bin");
        std::fs::write(bin.join("python"), "").unwrap();
        let env = Env { path: Some(bin.clone().into_os_string()), ..Env::default() };
        assert_eq!(find_in(&root, &env), Some(bin.join("python")));
    }

    #[test]
    fn check_reports_what_an_interpreter_lacks() {
        let python = Path::new("/Users/katakam/repos/faster_pytest/.venv/bin/python");
        if python.is_file() {
            assert_eq!(check(python), Support::Ready);
            assert_eq!(check(python), Support::Ready, "remembered");
        }
        let fake = tmp("fake").join("python");
        std::fs::write(&fake, "#!/bin/sh\necho 3.11.9 True\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(check(&fake), Support::TooOld { version: "3.11.9".into() });
        std::fs::write(&fake, "#!/bin/sh\necho 3.13.1 False\n").unwrap();
        assert_eq!(check(&fake), Support::NoPytest);
        assert!(matches!(check(Path::new("/nonexistent/python")), Support::Broken { .. }));
    }

    #[test]
    fn the_shim_is_written_once_with_the_embedded_source() {
        let a = shim_path().unwrap();
        let b = shim_path().unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), SHIM_SOURCE);
    }
}
