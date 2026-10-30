//! Where the project is, and which of its paths matter. Unit U1.

use std::path::{Path, PathBuf};

pub const CONFIG_FILES: &[&str] =
    &["pytest.ini", ".pytest.ini", "pyproject.toml", "setup.cfg", "tox.ini", "uv.lock", "poetry.lock"];
pub const EXCLUDED_DIRS: &[&str] = &[
    ".git", ".hg", ".venv", "venv", ".tox", ".nox", "__pycache__", ".forktest", ".pytest_cache", ".hypothesis",
    ".mypy_cache", ".ruff_cache", "node_modules", "site-packages", "target",
];

const INI_FILES: &[(&str, Option<&str>)] = &[
    ("pytest.ini", None),
    (".pytest.ini", None),
    ("pyproject.toml", Some("[tool.pytest")),
    ("tox.ini", Some("[pytest]")),
    ("setup.cfg", Some("[tool:pytest]")),
];

/// pytest's rootdir, approximated without Python: an ini file with a pytest section, then a project file, then `.git`.
pub fn find_root(start: &Path) -> PathBuf {
    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let ancestors: Vec<&Path> = start.ancestors().collect();
    for d in &ancestors {
        for (name, section) in INI_FILES {
            let path = d.join(name);
            if !path.is_file() {
                continue;
            }
            let Some(section) = section else { return d.to_path_buf() };
            if std::fs::read(&path).is_ok_and(|text| String::from_utf8_lossy(&text).contains(section)) {
                return d.to_path_buf();
            }
        }
    }
    for marker in ["pyproject.toml", "setup.py", ".git"] {
        if let Some(d) = ancestors.iter().find(|d| d.join(marker).exists()) {
            return d.to_path_buf();
        }
    }
    start
}

/// `requirements*.txt` counts too.
pub fn is_config_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    CONFIG_FILES.contains(&name) || (name.starts_with("requirements") && name.ends_with(".txt"))
}

/// The config files present directly under `root`, sorted.
pub fn config_files(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| is_config_file(name) && root.join(name).is_file())
        .collect();
    out.sort();
    out
}

pub fn is_excluded(rel: &str) -> bool {
    rel.starts_with("..") || rel.split('/').any(|part| EXCLUDED_DIRS.contains(&part))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("forktest-root-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(d).unwrap()
    }

    #[test]
    fn an_ini_file_needs_its_pytest_section_and_beats_project_markers() {
        let root = tmp("ini");
        std::fs::write(root.join("pyproject.toml"), "[project]\nname = 'x'\n").unwrap();
        std::fs::create_dir_all(root.join("pkg/sub")).unwrap();
        std::fs::write(root.join("pkg/tox.ini"), "[tox]\n").unwrap();
        assert_eq!(find_root(&root.join("pkg/sub")), root, "no pytest section anywhere: the project file wins");
        std::fs::write(root.join("pkg/setup.cfg"), "[tool:pytest]\naddopts = -q\n").unwrap();
        assert_eq!(find_root(&root.join("pkg/sub")), root.join("pkg"));
        std::fs::write(root.join("pkg/sub/pytest.ini"), "").unwrap();
        assert_eq!(find_root(&root.join("pkg/sub")), root.join("pkg/sub"), "pytest.ini needs no section");
    }

    #[test]
    fn config_files_and_exclusions() {
        assert!(is_config_file("a/b/requirements-dev.txt"));
        assert!(is_config_file("uv.lock"));
        assert!(!is_config_file("requirements.in"));
        let root = tmp("cfg");
        for name in ["tox.ini", "requirements.txt", "setup.py"] {
            std::fs::write(root.join(name), "").unwrap();
        }
        std::fs::create_dir(root.join("pytest.ini")).unwrap();
        assert_eq!(config_files(&root), ["requirements.txt", "tox.ini"]);
        assert!(is_excluded(".venv/lib/x.py"));
        assert!(is_excluded("../outside.py"));
        assert!(is_excluded("a/__pycache__/m.pyc"));
        assert!(!is_excluded("src/venvs.py"));
    }
}
