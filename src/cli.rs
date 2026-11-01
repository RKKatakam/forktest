//! Argument parsing and dispatch.

use crate::events::Event;
use crate::fingerprint::{FILE, MODULE};
use crate::report::{HumanReporter, JsonlReporter, Reporter, Reporters};
use crate::run::{Mode, Project, Request, TERMINAL_ENV, Unsupported};
use crate::zygote::{ForkPoint, Terminal};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

/// Modes forktest cannot serve, handed to plain pytest.
const PYTEST_ONLY: &[&str] = &[
    "--collect-only", "--co", "--fixtures", "--fixtures-per-test", "--markers", "--version", "-V", "-h", "--help",
    "--pdb", "--trace", "--setup-plan", "--setup-only", "--setup-show", "--lf", "--last-failed", "--ff",
    "--failed-first", "--sw", "--stepwise", "--stepwise-skip",
];

const USAGE: &str = "usage: forktest [--all] [--since REF] [--json] [--junitxml PATH] [-n N|auto] [--strict-pytest]
                [--verify-selection] [--why NODEID] [--no-daemon] [--python PATH]
                [--fork-from auto|collected|imported|spawn] [pytest args...]";

#[derive(Debug, Default, PartialEq)]
pub struct Opts {
    pub all: bool,
    pub since: Option<String>,
    pub json: bool,
    pub junitxml: Option<PathBuf>,
    /// 0 means one per CPU.
    pub workers: usize,
    pub strict: bool,
    pub verify: bool,
    pub why: Option<String>,
    pub no_daemon: bool,
    pub python: Option<PathBuf>,
    pub fork_from: Option<ForkPoint>,
    /// Everything else, in order.
    pub pytest_args: Vec<String>,
}

impl Opts {
    pub fn parse(argv: &[String]) -> Result<Opts, String> {
        let mut o = Opts::default();
        let mut args = argv.iter();
        while let Some(arg) = args.next() {
            let (name, inline) = match arg.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
                _ if arg.starts_with("-n") && arg.len() > 2 && !arg.starts_with("--") => ("-n", Some(arg[2..].to_string())),
                _ => (arg.as_str(), None),
            };
            let mut value = |what: &str| match inline.clone() {
                Some(v) => Ok(v),
                None => args.next().cloned().ok_or(format!("argument {what}: expected one argument")),
            };
            match name {
                "--all" => o.all = true,
                "--json" => o.json = true,
                "--strict-pytest" => o.strict = true,
                "--verify-selection" => o.verify = true,
                "--no-daemon" => o.no_daemon = true,
                "--since" => o.since = Some(value("--since")?),
                "--junitxml" => o.junitxml = Some(PathBuf::from(value("--junitxml")?)),
                "--why" => o.why = Some(value("--why")?),
                "--python" => o.python = Some(PathBuf::from(value("--python")?)),
                "-n" | "--workers" => {
                    let v = value("-n/--workers")?;
                    o.workers = match v.as_str() {
                        "auto" => 0,
                        n => n.parse().map_err(|_| format!("argument -n/--workers: invalid int value: '{n}'"))?,
                    };
                }
                "--fork-from" => {
                    o.fork_from = match value("--fork-from")?.as_str() {
                        "auto" => None,
                        "collected" => Some(ForkPoint::Collected),
                        "imported" => Some(ForkPoint::Imported),
                        "spawn" => Some(ForkPoint::Spawn),
                        other => return Err(format!("argument --fork-from: invalid choice: '{other}'")),
                    };
                }
                _ => o.pytest_args.push(arg.clone()),
            }
        }
        Ok(o)
    }

    fn pytest_only(&self) -> bool {
        self.pytest_args.iter().any(|a| PYTEST_ONLY.contains(&a.as_str()))
    }
}

pub fn main(argv: Vec<String>) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("forktest: error: {e}");
            return 2;
        }
    };
    let root = crate::root::find_root(&cwd);
    match argv.first().map(String::as_str) {
        Some("daemon") => return crate::daemon::daemon_command(&root, &argv[1..]),
        Some("watch") => return crate::daemon::watch_command(&root, &argv[1..]),
        _ => {}
    }
    let mut opts = match Opts::parse(&argv) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{USAGE}\nforktest: error: {e}");
            return 2;
        }
    };
    opts.junitxml = opts.junitxml.map(|p| cwd.join(p));
    // The daemon's interpreters run in the root, so relative pytest args only mean the same there.
    let from_root = std::fs::canonicalize(&cwd).is_ok_and(|c| c == root);
    match dispatch(opts, root, from_root) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("forktest: error: {e:#}");
            3
        }
    }
}

fn dispatch(opts: Opts, root: PathBuf, from_root: bool) -> anyhow::Result<i32> {
    if let Some(nodeid) = &opts.why {
        return why(&root, nodeid);
    }
    let python = match &opts.python {
        Some(p) => p.clone(),
        None => crate::python::find_python(&root)?,
    };
    if opts.pytest_only() {
        return plain_pytest(&python, &opts.pytest_args);
    }
    let mut reporters = || {
        let live: Box<dyn Reporter> = if opts.json {
            Box::new(JsonlReporter::new(std::io::stdout()))
        } else {
            let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
            Box::new(HumanReporter::new(std::io::stdout(), tty, opts.verify, true))
        };
        Reporters::new(vec![live])
    };
    // pytest's own reporter prints the run, and writes the JUnit XML, unless the run is JSON lines alone.
    let terminal = if opts.json && opts.junitxml.is_none() { None } else { Some(Terminal::ours(opts.json)?) };
    let request = Request {
        mode: match (&opts.since, opts.all) {
            (_, true) => Mode::All,
            (Some(r), false) => Mode::Since(r.clone()),
            (None, false) => Mode::Affected,
        },
        verify: opts.verify,
        workers: opts.workers,
        strict: opts.strict,
        nice: false,
        pytest_args: opts.pytest_args.clone(),
        fork_from: opts.fork_from,
        stdout_to_stderr: opts.json,
        junitxml: opts.junitxml.clone(),
        terminal_env: TERMINAL_ENV.iter().map(|k| (k.to_string(), std::env::var(k).ok())).collect(),
    };
    if !opts.no_daemon
        && from_root
        && let Some(code) = crate::daemon::run_via_daemon(&root, &python, &request, terminal.as_ref(), &mut reporters)
    {
        return Ok(code);
    }
    let mut reporters = reporters();
    let mut project = Project::open(&root, python.clone())?;
    let result = project.run(&request, terminal, &mut |e: &Event| reporters.emit(e));
    if let Err(e) = &result
        && let Some(unsupported) = e.downcast_ref::<Unsupported>()
    {
        eprintln!("forktest: {unsupported}");
        drop(project);
        return plain_pytest(&python, &request.pytest_args);
    }
    reporters.close();
    drop(project);
    Ok(reporters.status.code(result?))
}

fn plain_pytest(python: &std::path::Path, args: &[String]) -> anyhow::Result<i32> {
    let err = std::process::Command::new(python).args(["-m", "pytest"]).args(args).exec();
    anyhow::bail!("running {} -m pytest: {err}", python.display());
}

/// Why a test would be selected now, from the store alone.
fn why(root: &std::path::Path, nodeid: &str) -> anyhow::Result<i32> {
    let mut reasons = crate::run::explain(root, nodeid)?;
    if reasons.is_empty() {
        println!("{nodeid}: not selected (no recorded dependency changed)");
        return Ok(0);
    }
    reasons.sort_by_key(|(r, _)| *r);
    println!("{nodeid}: selected as {}", reasons[0].0.as_str());
    for (reason, dep) in reasons {
        match dep {
            None => println!("  {}", reason.as_str()),
            Some(d) if &*d.qualname == MODULE || &*d.qualname == FILE => println!("  {}: {} changed", reason.as_str(), d.path),
            Some(d) => println!("  {}: {}::{} changed", reason.as_str(), d.path, d.qualname),
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Opts {
        Opts::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn worker_counts_in_every_spelling() {
        for args in [&["-n4"][..], &["-n", "4"], &["--workers=4"], &["--workers", "4"]] {
            assert_eq!(parse(args).workers, 4, "{args:?}");
        }
        assert_eq!(parse(&["-n", "auto"]).workers, 0);
        assert_eq!(parse(&["-nauto"]).workers, 0);
        assert!(Opts::parse(&["-n".into(), "x".into()]).is_err());
        assert!(Opts::parse(&["--since".into()]).is_err());
    }

    #[test]
    fn own_flags_are_taken_and_the_rest_passes_through_in_order() {
        let o = parse(&["-x", "--all", "-k", "tax and not slow", "--json", "--since=main", "tests", "--fork-from", "spawn", "-p", "no:x"]);
        assert!(o.all && o.json);
        assert_eq!(o.since.as_deref(), Some("main"));
        assert_eq!(o.fork_from, Some(ForkPoint::Spawn));
        assert_eq!(o.pytest_args, ["-x", "-k", "tax and not slow", "tests", "-p", "no:x"]);
        assert!(!o.pytest_only());
        assert!(parse(&["--co", "-q"]).pytest_only());
        assert!(parse(&["--lf"]).pytest_only());
        assert!(!parse(&["--lfnf=all"]).pytest_only());
    }
}
