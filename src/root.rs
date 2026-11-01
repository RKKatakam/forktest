//! Where the project is, and which of its paths matter. Unit U1.

use std::path::{Component, Path, PathBuf};

pub const CONFIG_FILES: &[&str] = &[
    "pytest.toml", ".pytest.toml", "pytest.ini", ".pytest.ini", "pyproject.toml", "setup.cfg", "tox.ini", "uv.lock",
    "poetry.lock",
];
pub const EXCLUDED_DIRS: &[&str] = &[
    ".git", ".hg", ".venv", "venv", ".tox", ".nox", "__pycache__", ".forktest", ".pytest_cache", ".hypothesis",
    ".mypy_cache", ".ruff_cache", "node_modules", "site-packages", "target",
];

/// The files pytest takes configuration from, in its order of precedence within one directory.
const CONFIG_NAMES: &[&str] = &["pytest.toml", ".pytest.toml", "pytest.ini", ".pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg"];

/// pytest's built-in options that take a value. A plugin's options are registered after the rootdir is set, so pytest
/// reads the value of a plugin option as a test path, and so does this.
const VALUE_OPTIONS: &[&str] = &[
    "-k", "-m", "--maxfail", "-W", "--pythonwarnings", "--max-warnings", "--ignore", "--ignore-glob", "--deselect",
    "--confcutdir", "--import-mode", "-c", "--config-file", "--rootdir", "--basetemp", "--durations", "--durations-min",
    "-p", "-o", "--override-ini", "--verbosity", "-r", "--report-chars", "--tb", "--show-capture", "--color",
    "--code-highlight", "--pdbcls", "--capture", "--pastebin", "--assert", "--junitxml", "--junit-xml", "--junitprefix",
    "--junit-prefix", "--doctest-report", "--doctest-glob", "--lfnf", "--last-failed-no-failures", "--log-level",
    "--log-format", "--log-date-format", "--log-cli-level", "--log-cli-format", "--log-cli-date-format", "--log-file",
    "--log-file-mode", "--log-file-level", "--log-file-format", "--log-file-date-format", "--log-auto-indent",
    "--log-disable",
];
const OPTIONAL_VALUE_OPTIONS: &[&str] = &["--debug", "--cache-show"];
const SHORT_FLAGS: &str = "Vhlqsvx";

/// What pytest's rootdir depends on among its arguments.
#[derive(Default)]
struct Invocation {
    paths: Vec<String>,
    rootdir: Option<String>,
    config_file: Option<String>,
}

/// pytest's rootdir for `args` run from `cwd`, found without Python the way `_pytest.config.findpaths.determine_setup`
/// finds it. `PYTEST_ADDOPTS` comes first, as in pytest.
pub fn find_root(cwd: &Path, args: &[String]) -> PathBuf {
    let mut all: Vec<String> = std::env::var("PYTEST_ADDOPTS").ok().and_then(|a| shlex::split(&a)).unwrap_or_default();
    all.extend_from_slice(args);
    let root = rootdir(cwd, &Invocation::parse(&all));
    std::fs::canonicalize(&root).unwrap_or(root)
}

impl Invocation {
    /// Splits arguments as pytest's argparse does before any plugin is loaded.
    fn parse(args: &[String]) -> Invocation {
        let mut inv = Invocation::default();
        let mut args = args.iter().peekable();
        while let Some(arg) = args.next() {
            let (name, inline) = if arg == "--" {
                inv.paths.extend(args.by_ref().cloned());
                break;
            } else if arg.starts_with("--") {
                arg.split_once('=').map_or((arg.clone(), None), |(n, v)| (n.to_string(), Some(v.to_string())))
            } else if arg.len() > 1 && arg.starts_with('-') {
                match short_option(arg) {
                    Some(opt) => opt,
                    None => continue,
                }
            } else {
                inv.paths.push(arg.clone());
                continue;
            };
            let value = match inline {
                Some(v) => Some(v),
                None if VALUE_OPTIONS.contains(&name.as_str()) => args.next().cloned(),
                None if OPTIONAL_VALUE_OPTIONS.contains(&name.as_str()) => args.next_if(|a| !a.starts_with('-')).cloned(),
                None => None,
            };
            match name.as_str() {
                "--rootdir" => inv.rootdir = value,
                "-c" | "--config-file" => inv.config_file = value,
                _ => {}
            }
        }
        inv
    }
}

/// The value-taking option in a cluster of short options such as `-xk` or `-cpytest.ini`, with its value if attached.
fn short_option(arg: &str) -> Option<(String, Option<String>)> {
    for (i, c) in arg.char_indices().skip(1) {
        let name = format!("-{c}");
        if VALUE_OPTIONS.contains(&name.as_str()) {
            let rest = &arg[i + c.len_utf8()..];
            return Some((name, (!rest.is_empty()).then(|| rest.trim_start_matches('=').to_string())));
        }
        if !SHORT_FLAGS.contains(c) {
            return None;
        }
    }
    None
}

fn rootdir(cwd: &Path, inv: &Invocation) -> PathBuf {
    if let Some(dir) = inv.rootdir.as_deref().filter(|d| !d.is_empty()) {
        return absolute(cwd, &expand_vars(dir));
    }
    let dirs: Vec<PathBuf> = inv
        .paths
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(|a| absolute(cwd, a.split_once("::").map_or(a, |(path, _)| path)))
        .filter(|p| p.exists())
        .map(|p| if p.is_dir() { p } else { p.parent().map_or(p.clone(), Path::to_path_buf) })
        .collect();
    let ancestor = common_ancestor(cwd, &dirs);
    let unless_fs_root = |dir: PathBuf, fallback: &Path| if dir.parent().is_none() { fallback.to_path_buf() } else { dir };
    if let Some(file) = inv.config_file.as_deref().filter(|f| !f.is_empty()) {
        let path = absolute(cwd, file);
        return match path.parent().filter(|_| path.is_file()) {
            Some(dir) => dir.to_path_buf(),
            None => unless_fs_root(ancestor, cwd),
        };
    }
    locate_config(std::slice::from_ref(&ancestor))
        .or_else(|| ancestor.ancestors().find(|d| d.join("setup.py").is_file()).map(Path::to_path_buf))
        .or_else(|| locate_config(&dirs))
        .unwrap_or_else(|| unless_fs_root(common_ancestor(cwd, &[cwd.to_path_buf(), ancestor.clone()]), &ancestor))
}

/// The nearest directory above any of `dirs`, in order, holding pytest configuration, else the nearest holding a
/// `pyproject.toml`.
fn locate_config(dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut pyproject = None;
    for dir in dirs {
        for base in dir.ancestors() {
            for name in CONFIG_NAMES {
                let path = base.join(name);
                if !path.is_file() {
                    continue;
                }
                if *name == "pyproject.toml" && pyproject.is_none() {
                    pyproject = Some(base.to_path_buf());
                }
                if holds_pytest_config(name, &path) {
                    return Some(base.to_path_buf());
                }
            }
        }
    }
    pyproject
}

fn holds_pytest_config(name: &str, path: &Path) -> bool {
    let text = || std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    match name {
        "pyproject.toml" => has_tool_pytest(&text()),
        "tox.ini" => has_ini_section(&text(), "pytest"),
        "setup.cfg" => has_ini_section(&text(), "tool:pytest"),
        _ => true,
    }
}

/// A section header as `iniconfig` reads one: at the start of a line, with comments after it.
fn has_ini_section(text: &str, section: &str) -> bool {
    text.trim_start_matches('\u{feff}').lines().any(|line| {
        let line = line.split(['#', ';']).next().unwrap_or("").trim_end();
        line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) == Some(section)
    })
}

/// Whether a `pyproject.toml` has a non-empty `tool.pytest` table, read line by line without a TOML parser.
fn has_tool_pytest(text: &str) -> bool {
    let key_path = |s: &str| -> Vec<String> { s.split('.').map(|p| p.trim().trim_matches(['"', '\'']).to_string()).collect() };
    let under_tool_pytest = |path: &[String]| path.len() > 2 && path[0] == "tool" && path[1] == "pytest";
    let mut table = Vec::new();
    let mut in_string = false;
    for line in text.lines() {
        let was_in_string = in_string;
        in_string ^= (line.matches("\"\"\"").count() + line.matches("'''").count()) % 2 == 1;
        let line = line.trim();
        if was_in_string || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let Some((header, _)) = header.trim_start_matches('[').split_once(']') else { continue };
            table = key_path(header);
            if under_tool_pytest(&table) {
                return true;
            }
        } else if let Some((key, value)) = line.split_once('=') {
            let path: Vec<String> = table.iter().cloned().chain(key_path(key)).collect();
            let inline = value.trim().strip_prefix('{').is_some_and(|v| !v.trim_start().starts_with('}'));
            if under_tool_pytest(&path) || (path == ["tool", "pytest"] && inline) {
                return true;
            }
        }
    }
    false
}

/// `os.path.abspath`: joined to `cwd` and normalized without touching the filesystem.
fn absolute(cwd: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in cwd.join(path).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The deepest directory holding every existing path in `paths`, or `cwd` when none exists.
fn common_ancestor(cwd: &Path, paths: &[PathBuf]) -> PathBuf {
    let common = |a: PathBuf, b: &PathBuf| a.components().zip(b.components()).take_while(|(x, y)| x == y).map(|(x, _)| x).collect();
    let mut existing = paths.iter().filter(|p| p.exists());
    let Some(first) = existing.next() else { return cwd.to_path_buf() };
    let ancestor: PathBuf = existing.fold(first.clone(), common);
    if ancestor.is_file() { ancestor.parent().map_or(ancestor.clone(), Path::to_path_buf) } else { ancestor }
}

/// `os.path.expandvars`: `$name` and `${name}`, leaving unset variables as written.
fn expand_vars(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let (name, len) = match after.strip_prefix('{') {
            Some(braced) => braced.find('}').map_or(("", 0), |j| (&braced[..j], j + 2)),
            None => {
                let j = after.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(after.len());
                (&after[..j], j)
            }
        };
        match std::env::var(name).ok().filter(|_| len > 0) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[i..=i + len]),
        }
        rest = &rest[i + 1 + len..];
    }
    out.push_str(rest);
    out
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

    fn layout(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = tmp(name);
        for (rel, text) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        root
    }

    fn root_of(cwd: &Path, args: &[&str]) -> PathBuf {
        find_root(cwd, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn an_ini_file_needs_its_pytest_section_and_beats_project_markers() {
        let root = layout("ini", &[("pyproject.toml", "[project]\nname = 'x'\n"), ("pkg/tox.ini", "[tox]\n# [pytest]\n"), ("pkg/sub/t.py", "")]);
        assert_eq!(root_of(&root.join("pkg/sub"), &[]), root, "no pytest section anywhere: the pyproject.toml wins");
        std::fs::write(root.join("pkg/setup.cfg"), "[tool:pytest]\naddopts = -q\n").unwrap();
        assert_eq!(root_of(&root.join("pkg/sub"), &[]), root.join("pkg"));
        std::fs::write(root.join("pkg/sub/pytest.ini"), "").unwrap();
        assert_eq!(root_of(&root.join("pkg/sub"), &[]), root.join("pkg/sub"), "pytest.ini needs no section");
    }

    #[test]
    fn pyproject_counts_as_config_only_with_a_non_empty_tool_pytest_table() {
        let root = layout("toml", &[("tox.ini", "[pytest]\n"), ("pkg/t/x.py", "")]);
        let cwd = root.join("pkg/t");
        for (text, counts) in [
            ("[tool.pytest]\n", false),
            ("# [tool.pytest.ini_options]\n", false),
            ("readme = \"\"\"\n[tool.pytest.ini_options]\n\"\"\"\n", false),
            ("[tool.pytest.ini_options]\n", true),
            ("[tool.pytest]\naddopts = ['-q']\n", true),
            ("[tool]\npytest.ini_options.addopts = '-q'\n", true),
        ] {
            std::fs::write(root.join("pkg/pyproject.toml"), text).unwrap();
            assert_eq!(root_of(&cwd, &[]), if counts { root.join("pkg") } else { root.clone() }, "{text:?}");
        }
    }

    #[test]
    fn without_config_setup_py_marks_the_root_and_git_does_not() {
        let root = layout("markers", &[("tests/a/t.py", "")]);
        std::fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(root_of(&root.join("tests"), &[]), root.join("tests"), "the invocation dir");
        assert_eq!(root_of(&root.join("tests"), &["../tests/a"]), root.join("tests"), "the invocation dir holds the arg");
        assert_eq!(root_of(&root.join("tests/a"), &["../../tests"]), root.join("tests"), "the arg holds the invocation dir");
        std::fs::write(root.join("setup.py"), "").unwrap();
        assert_eq!(root_of(&root.join("tests"), &[]), root);
    }

    #[test]
    fn test_path_args_decide_where_the_search_starts() {
        let root = layout("args", &[("pytest.ini", ""), ("pkg/pytest.ini", ""), ("pkg/test_a.py", ""), ("b/pytest.ini", "")]);
        assert_eq!(root_of(&root, &[]), root);
        assert_eq!(root_of(&root, &["pkg/test_a.py"]), root.join("pkg"));
        assert_eq!(root_of(&root, &["pkg/test_a.py::test_x[1]"]), root.join("pkg"), "a node id's path");
        assert_eq!(root_of(&root, &["pkg/missing.py"]), root, "a missing path is not an arg");
        assert_eq!(root_of(&root, &["-x", "--deselect", "pkg/test_a.py", "-k", "pkg", "-xkpkg", "--tb=short"]), root, "option values");
        assert_eq!(root_of(&root, &["--plugin-opt", "pkg"]), root.join("pkg"), "pytest reads a plugin option's value as a path");
        assert_eq!(root_of(&root, &["--", "-k", "pkg"]), root.join("pkg"));
        assert_eq!(root_of(&root.join("b"), &["../pkg", "."]), root, "two args: config above both wins");
        std::fs::remove_file(root.join("pytest.ini")).unwrap();
        assert_eq!(root_of(&root.join("b"), &["../pkg", "."]), root.join("pkg"), "then config in the first arg");
    }

    #[test]
    fn rootdir_and_config_file_options_override_the_search() {
        let root = layout("opts", &[("pytest.ini", ""), ("sub/t.py", ""), ("cfg/other.ini", "[pytest]\n")]);
        assert_eq!(root_of(&root, &["--rootdir=sub"]), root.join("sub"));
        assert_eq!(root_of(&root.join("cfg"), &["--rootdir", "../sub"]), root.join("sub"));
        assert_eq!(root_of(&root, &["--rootdir="]), root, "an empty --rootdir is ignored");
        assert_eq!(root_of(&root.join("sub"), &["-c", "../cfg/other.ini"]), root.join("cfg"));
        assert_eq!(root_of(&root, &["-ccfg/other.ini"]), root.join("cfg"));
        assert_eq!(root_of(&root, &["--config-file=cfg/other.ini", "--rootdir=sub"]), root.join("sub"));
        assert_eq!(root_of(&root.join("sub"), &["-c", "/dev/null"]), root.join("sub"), "a config file that is not a file");
    }

    #[test]
    fn rootdir_expands_variables_as_python_does() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_vars("$HOME/a/${HOME}"), format!("{home}/a/{home}"));
        assert_eq!(expand_vars("$FORKTEST_ROOT_UNSET/${}/$/${x"), "$FORKTEST_ROOT_UNSET/${}/$/${x");
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
