//! The daemon and watch mode: one long-lived process holding a `Project` whose L1 stays collected, a file watcher
//! that keeps it and the fingerprints current, and the client side of its socket. Unit U4.

use crate::cli::Opts;
use crate::events::Event;
use crate::proto;
use crate::report::{HumanReporter, Reporter, Reporters};
use crate::root::{is_config_file, is_excluded};
use crate::run::{Mode, Project, Request};
use crate::zygote::ForkPoint;
use anyhow::Context;
use notify::{RecursiveMode, Watcher};
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A save is often several writes; rebuilding after the first would waste the rebuild.
const REBUILD_DEBOUNCE: Duration = Duration::from_millis(50);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// Far longer than any watcher latency seen, short enough that a lost cookie costs one slow request.
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// Files whose change can mean other installed packages, so L0's preimports are stale too.
const RESTART_FILES: &[&str] = &["pyproject.toml", "uv.lock", "poetry.lock"];

struct Paths {
    root: PathBuf,
}

impl Paths {
    fn dir(&self) -> PathBuf {
        self.root.join(".forktest")
    }

    /// `AF_UNIX` paths are capped near 104 bytes on macOS, so a deep checkout gets a socket under the temp dir.
    fn sock(&self) -> PathBuf {
        let in_project = self.dir().join("daemon.sock");
        if in_project.as_os_str().len() <= 96 {
            return in_project;
        }
        let digest = xxhash_rust::xxh3::xxh3_64(self.root.as_os_str().as_encoded_bytes());
        std::env::temp_dir().join(format!("forktest-{:012x}.sock", digest & 0xffff_ffff_ffff))
    }

    fn pid(&self) -> PathBuf {
        self.dir().join("daemon.pid")
    }

    fn log(&self) -> PathBuf {
        self.dir().join("daemon.log")
    }
}

fn log(msg: &str) {
    let now = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    };
    eprintln!("[forktest {now}] {msg}");
}

fn send(paths: &Paths, msg: &Value) -> Option<UnixStream> {
    let mut stream = UnixStream::connect(paths.sock()).ok()?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT)).ok()?;
    proto::write_frame(&mut stream, msg, &[]).ok()?;
    Some(stream)
}

fn status(root: &Path) -> Option<Value> {
    let stream = send(&Paths { root: root.to_path_buf() }, &json!({"op": "status"}))?;
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// Serves `req` through the daemon running for `root`. `None` when there is none, it cannot serve this request, or it
/// went away twice during the run; the caller then runs it itself.
pub fn run_via_daemon(
    root: &Path,
    python: &Path,
    req: &Request,
    reporters: &mut dyn FnMut() -> Reporters,
) -> Option<i32> {
    let paths = Paths { root: root.to_path_buf() };
    let msg = json!({"op": "run", "python": python, "request": req});
    for attempt in 0..2 {
        let stream = send(&paths, &msg)?;
        let mut out = reporters();
        let mut end: Option<Value> = None;
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            match Event::parse(&line) {
                Ok(event) => out.emit(&event),
                Err(_) => {
                    end = serde_json::from_str(&line).ok();
                    break;
                }
            }
        }
        let end = end.unwrap_or(Value::Null);
        if let Some(reason) = end["refused"].as_str() {
            eprintln!("forktest: the daemon cannot serve this run ({reason}); running without it");
            return None;
        }
        out.close();
        if let Some(code) = end["exit"].as_i64() {
            crate::run::log(|| format!("served by the daemon (pid {})", end["pid"]));
            return Some(out.status.code(code as i32));
        }
        if let Some(error) = end["error"].as_str() {
            eprintln!("forktest: error: {error}");
            return Some(3);
        }
        if attempt == 0 {
            eprintln!("forktest: the daemon went away during the run; retrying");
        }
    }
    eprintln!("forktest: the daemon went away again; running without it");
    None
}

pub fn daemon_command(root: &Path, argv: &[String]) -> i32 {
    let paths = Paths { root: root.to_path_buf() };
    match argv.first().map(String::as_str).unwrap_or("status") {
        "status" => match status(root) {
            None => {
                println!("forktest daemon: not running");
                1
            }
            Some(Value::Object(fields)) => {
                for (key, value) in fields {
                    match value {
                        Value::String(s) => println!("{key}: {s}"),
                        Value::Null => println!("{key}: none"),
                        other => println!("{key}: {other}"),
                    }
                }
                0
            }
            Some(other) => {
                println!("{other}");
                0
            }
        },
        "start" => start(&paths, &argv[1..]),
        "stop" => stop(&paths),
        "__serve" => match parse(&argv[1..]).and_then(|opts| serve(root, opts, false)) {
            Ok(()) => 0,
            Err(e) => {
                log(&format!("daemon failed: {e:#}"));
                1
            }
        },
        other => {
            eprintln!("forktest daemon: unknown subcommand {other:?} (start|stop|status)");
            2
        }
    }
}

pub fn watch_command(root: &Path, argv: &[String]) -> i32 {
    if status(root).is_some() {
        eprintln!("forktest watch: a daemon is already running here; stop it first (forktest daemon stop)");
        return 1;
    }
    println!("forktest watch: {}", root.display());
    match parse(argv).and_then(|opts| serve(root, opts, true)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("forktest watch: error: {e:#}");
            1
        }
    }
}

fn parse(argv: &[String]) -> anyhow::Result<Opts> {
    Opts::parse(argv).map_err(|e| anyhow::anyhow!(e))
}

fn start(paths: &Paths, argv: &[String]) -> i32 {
    if let Some(st) = status(&paths.root) {
        println!("forktest daemon: already running (pid {})", st["pid"]);
        return 0;
    }
    if let Err(e) = parse(argv) {
        eprintln!("forktest daemon: error: {e}");
        return 2;
    }
    let spawned = (|| -> anyhow::Result<std::process::Child> {
        std::fs::create_dir_all(paths.dir())?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(paths.log())?;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.args(["daemon", "__serve"])
            .args(argv)
            .current_dir(&paths.root)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // setsid is async-signal-safe, and a new session leaves the terminal's job control and its SIGHUP behind.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        Ok(cmd.spawn()?)
    })();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            eprintln!("forktest daemon: could not start: {e:#}");
            return 1;
        }
    };
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(st) = status(&paths.root) {
            println!("forktest daemon: started (pid {}), log at {}", st["pid"], paths.log().display());
            return 0;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("forktest daemon: did not come up; see {}", paths.log().display());
    1
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn stop(paths: &Paths) -> i32 {
    let Some(pid) = std::fs::read_to_string(paths.pid()).ok().and_then(|s| s.trim().parse::<i32>().ok()) else {
        println!("forktest daemon: not running");
        return 0;
    };
    if !alive(pid) {
        let _ = std::fs::remove_file(paths.sock());
        let _ = std::fs::remove_file(paths.pid());
        println!("forktest daemon: not running (stale pid file removed)");
        return 0;
    }
    if send(paths, &json!({"op": "stop"})).is_none() {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if !alive(pid) {
            println!("forktest daemon: stopped (pid {pid})");
            return 0;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    println!("forktest daemon: killed (pid {pid})");
    0
}

enum Msg {
    Fs(notify::Result<notify::Event>),
    Run(UnixStream, PathBuf, Request),
    Stop,
}

/// What other threads see of the daemon.
struct Shared {
    status: Mutex<Value>,
    started: Instant,
    /// The project's cancel flag.
    cancel: Arc<AtomicBool>,
    /// A watch run is in progress, so a relevant change cancels it.
    watch_run: AtomicBool,
    stopping: AtomicBool,
}

/// The warm L1, as the daemon tracks it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Zygote {
    /// No full collection: none yet, or the last one went stale. A rebuild is due.
    Absent,
    Building,
    Ready,
    /// The last rebuild failed; the next change to the tree retries.
    Failed,
}

impl Zygote {
    fn name(self) -> &'static str {
        match self {
            Zygote::Absent => "absent",
            Zygote::Building => "building",
            Zygote::Ready => "ready",
            Zygote::Failed => "failed",
        }
    }
}

struct Daemon {
    paths: Paths,
    project: Project,
    /// What a rebuilt L1 is collected for: the daemon's own args until a client asks for others.
    args: Vec<String>,
    fork_from: Option<ForkPoint>,
    workers: usize,
    zygote: Zygote,
    rebuild_at: Option<Instant>,
    /// Every `.py` file under the root, to tell an added or removed one from an edit.
    py_files: BTreeSet<String>,
    rx: Receiver<Msg>,
    /// Client messages that arrived while waiting for the watcher.
    backlog: VecDeque<Msg>,
    shared: Arc<Shared>,
    /// `Some` in watch mode: when the last unhandled change arrived.
    watch: Option<Option<Instant>>,
    cookies: u64,
    _watcher: notify::RecommendedWatcher,
}

fn serve(root: &Path, opts: Opts, watch: bool) -> anyhow::Result<()> {
    let signals = block_signals();
    let paths = Paths { root: root.to_path_buf() };
    std::fs::create_dir_all(paths.dir())?;
    let python = match &opts.python {
        Some(p) => p.clone(),
        None => crate::python::find_python(root)?,
    };
    let project = Project::open(root, python)?;
    project.fingerprints().trust_watcher();
    let (tx, rx) = channel();
    let shared = Arc::new(Shared {
        status: Mutex::new(Value::Null),
        started: Instant::now(),
        cancel: project.cancel_flag(),
        watch_run: AtomicBool::new(false),
        stopping: AtomicBool::new(false),
    });
    let watcher = start_watcher(root, tx.clone(), shared.clone())?;
    let sock = paths.sock();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    // Whoever can connect can run code as this user.
    std::fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    std::fs::write(paths.pid(), std::process::id().to_string())?;
    {
        let (tx, shared) = (tx.clone(), shared.clone());
        std::thread::spawn(move || accept(listener, tx, &shared));
    }
    {
        let (tx, shared) = (tx.clone(), shared.clone());
        std::thread::spawn(move || wait_for_signals(signals, tx, &shared));
    }
    let mut daemon = Daemon {
        py_files: py_files(root, ""),
        paths,
        project,
        args: opts.pytest_args,
        fork_from: opts.fork_from,
        workers: opts.workers,
        zygote: Zygote::Absent,
        rebuild_at: Some(Instant::now()),
        rx,
        backlog: VecDeque::new(),
        shared,
        watch: watch.then_some(Some(Instant::now() - WATCH_DEBOUNCE)),
        cookies: 0,
        _watcher: watcher,
    };
    log(&format!("daemon up (pid {}, python {})", std::process::id(), daemon.project.python().display()));
    daemon.publish();
    daemon.run();
    daemon.shutdown();
    Ok(())
}

/// Blocks the stop signals in this thread and every thread it starts, so one thread can wait for them. Spawned
/// interpreters start with an empty mask (std resets it).
fn block_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn wait_for_signals(set: libc::sigset_t, tx: Sender<Msg>, shared: &Shared) {
    loop {
        let mut sig = 0;
        if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
            continue;
        }
        if shared.stopping.swap(true, Ordering::SeqCst) {
            // A second signal: the Python processes kill themselves when their sockets close.
            std::process::exit(128 + sig);
        }
        shared.cancel.store(true, Ordering::SeqCst);
        let _ = tx.send(Msg::Stop);
    }
}

fn start_watcher(root: &Path, tx: Sender<Msg>, shared: Arc<Shared>) -> anyhow::Result<notify::RecommendedWatcher> {
    let root_owned = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if shared.watch_run.load(Ordering::SeqCst)
            && let Ok(e) = &event
            && e.paths.iter().any(|p| relative(&root_owned, p).is_some())
        {
            shared.cancel.store(true, Ordering::SeqCst);
        }
        let _ = tx.send(Msg::Fs(event));
    })?;
    watcher.watch(root, RecursiveMode::Recursive).with_context(|| format!("watching {}", root.display()))?;
    Ok(watcher)
}

/// The root-relative path of a watched path that matters: not outside the root, not in an excluded directory.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?.to_str()?;
    (!rel.is_empty() && !is_excluded(rel)).then(|| rel.to_string())
}

fn accept(listener: UnixListener, tx: Sender<Msg>, shared: &Shared) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(CLIENT_TIMEOUT));
        let Ok(Some(frame)) = proto::read_frame(&mut stream) else { continue };
        let _ = stream.set_read_timeout(None);
        let reply = |stream: &mut UnixStream, v: Value| {
            let _ = writeln!(stream, "{v}");
        };
        match frame.msg["op"].as_str() {
            Some("status") => {
                let mut st = shared.status.lock().unwrap().clone();
                st["uptime_s"] = json!((shared.started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
                reply(&mut stream, st);
            }
            Some("stop") => {
                shared.stopping.store(true, Ordering::SeqCst);
                shared.cancel.store(true, Ordering::SeqCst);
                let _ = tx.send(Msg::Stop);
                reply(&mut stream, json!({"stopping": std::process::id()}));
            }
            Some("run") if shared.stopping.load(Ordering::SeqCst) => {
                reply(&mut stream, json!({"refused": "the daemon is stopping"}));
            }
            Some("run") => {
                let python = PathBuf::from(frame.msg["python"].as_str().unwrap_or_default());
                match serde_json::from_value::<Request>(frame.msg["request"].clone()) {
                    Ok(req) => {
                        let _ = tx.send(Msg::Run(stream, python, req));
                    }
                    Err(e) => reply(&mut stream, json!({"refused": format!("unreadable request: {e}")})),
                }
            }
            _ => {}
        }
    }
}

/// Every `.py` file under `dir`, skipping the directories pytest and the watcher skip.
fn py_files(root: &Path, dir: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_string()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&dir)) else { continue };
        for e in entries.flatten() {
            let (Ok(kind), Ok(name)) = (e.file_type(), e.file_name().into_string()) else { continue };
            let rel = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
            if kind.is_dir() {
                if !is_excluded(&rel) {
                    stack.push(rel);
                }
            } else if name.ends_with(".py") {
                out.insert(rel);
            }
        }
    }
    out
}

impl Daemon {
    fn run(&mut self) {
        loop {
            let msg = match self.backlog.pop_front() {
                Some(m) => Some(m),
                None => match self.rx.recv_timeout(self.idle_for()) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            match msg {
                Some(Msg::Stop) => return,
                Some(Msg::Fs(event)) => self.on_fs(event),
                Some(Msg::Run(stream, python, req)) => self.serve_client(stream, &python, req),
                None => {}
            }
            if self.shared.stopping.load(Ordering::SeqCst) {
                return;
            }
            let now = Instant::now();
            if self.zygote == Zygote::Absent && self.rebuild_at.is_some_and(|t| t <= now) {
                self.rebuild();
            }
            if let Some(Some(at)) = self.watch
                && now.duration_since(at) >= WATCH_DEBOUNCE
            {
                self.watch_run();
            }
        }
    }

    fn idle_for(&self) -> Duration {
        let now = Instant::now();
        let rebuild = self.rebuild_at.filter(|_| self.zygote == Zygote::Absent);
        let watch = self.watch.flatten().map(|t| t + WATCH_DEBOUNCE);
        match rebuild.into_iter().chain(watch).min() {
            Some(t) => t.saturating_duration_since(now),
            None => Duration::from_secs(3600),
        }
    }

    fn request(&self, verify: bool) -> Request {
        Request {
            mode: Mode::Affected,
            verify,
            workers: self.workers,
            strict: false,
            nice: false,
            pytest_args: self.args.clone(),
            fork_from: self.fork_from,
            stdout_to_stderr: false,
        }
    }

    fn publish(&self) {
        let warm = self.project.warm();
        let ready = self.zygote == Zygote::Ready;
        *self.shared.status.lock().unwrap() = json!({
            "pid": std::process::id(),
            "root": self.paths.root,
            "uptime_s": 0.0,
            "preimported": self.project.preimported(),
            "zygote": self.zygote.name(),
            "zygote_pid": if ready { self.project.l1_pid() } else { None },
            "imported": warm.map(|w| w.modules()),
            "tests": warm.map(|w| w.tests()),
            "fork_point": warm.map(|w| w.fork_point()),
            "pytest_args": self.args,
            "watch": self.watch.is_some(),
        });
    }

    fn set_zygote(&mut self, z: Zygote) {
        self.zygote = z;
        if z == Zygote::Absent {
            self.rebuild_at.get_or_insert_with(|| Instant::now() + REBUILD_DEBOUNCE);
        } else {
            self.rebuild_at = None;
        }
        self.publish();
    }

    fn rebuild(&mut self) {
        self.set_zygote(Zygote::Building);
        log(&format!("zygote building (pytest args {:?})", self.args));
        let started = Instant::now();
        let req = self.request(false);
        let next = match self.project.warm_up(&req) {
            Ok(warm) => {
                let (tests, modules, fork_point) = (warm.tests(), warm.modules(), warm.fork_point());
                log(&format!(
                    "zygote ready (pid {}, {tests} tests, {modules} project modules imported, workers fork from {fork_point:?}) in {:.2}s",
                    self.project.l1_pid().unwrap_or(0),
                    started.elapsed().as_secs_f64()
                ));
                Zygote::Ready
            }
            Err(e) => {
                log(&format!("zygote failed: {e:#}; see {}", self.paths.log().display()));
                Zygote::Failed
            }
        };
        self.set_zygote(next);
    }

    /// The watcher lost track: nothing it said before can be trusted.
    fn rescan(&mut self, why: &str) {
        log(&format!("{why}: checking every file again"));
        self.project.fingerprints().invalidate_all();
        self.py_files = py_files(&self.paths.root, "");
        self.project.drop_warm();
        self.set_zygote(Zygote::Absent);
        if let Some(pending) = &mut self.watch {
            *pending = Some(Instant::now());
        }
    }

    fn on_fs(&mut self, event: notify::Result<notify::Event>) {
        let event = match event {
            Ok(e) => e,
            Err(e) => return self.rescan(&format!("file watcher error: {e}")),
        };
        if event.need_rescan() {
            return self.rescan("the file watcher dropped events");
        }
        let (mut stale, mut restart) = (None, None);
        let mut changed = Vec::new();
        for path in &event.paths {
            let Some(rel) = relative(&self.paths.root, path) else { continue };
            self.project.fingerprints().invalidate(&rel);
            let name = rel.rsplit('/').next().unwrap_or(&rel);
            if RESTART_FILES.contains(&name) || (name.starts_with("requirements") && name.ends_with(".txt")) {
                restart.get_or_insert_with(|| rel.clone());
            } else if is_config_file(&rel) {
                stale.get_or_insert_with(|| format!("{rel} changed"));
            }
            if self.tree_changed(&rel) {
                stale.get_or_insert_with(|| format!("{rel} added or removed"));
            }
            if self.project.warm().is_some_and(|w| w.stale_after_change(&rel, self.project.fingerprints())) {
                stale.get_or_insert_with(|| format!("{rel} changed"));
            }
            changed.push(rel);
        }
        if changed.is_empty() {
            return;
        }
        if let Some(rel) = restart {
            log(&format!("{rel} changed: restarting the interpreters"));
            self.project.restart();
            self.set_zygote(Zygote::Absent);
        } else if let Some(why) = stale {
            if self.zygote == Zygote::Ready {
                log(&format!("zygote {} stale: {why}", self.project.l1_pid().unwrap_or(0)));
            }
            self.project.drop_warm();
            self.set_zygote(Zygote::Absent);
        } else if self.zygote == Zygote::Failed {
            self.set_zygote(Zygote::Absent);
        }
        if let Some(pending) = &mut self.watch {
            log(&format!("changed: {}{}", changed[..changed.len().min(5)].join(", "), if changed.len() > 5 { " ..." } else { "" }));
            *pending = Some(Instant::now());
        }
    }

    /// Updates the known `.py` files for a reported path; true when one was added or removed.
    fn tree_changed(&mut self, rel: &str) -> bool {
        let prefix = format!("{rel}/");
        match std::fs::symlink_metadata(self.paths.root.join(rel)) {
            Ok(m) if m.is_dir() => {
                let before: BTreeSet<String> = self.py_files.iter().filter(|p| p.starts_with(&prefix)).cloned().collect();
                let after = py_files(&self.paths.root, rel);
                if before == after {
                    return false;
                }
                self.py_files.retain(|p| !p.starts_with(&prefix));
                self.py_files.extend(after);
                true
            }
            Ok(_) => rel.ends_with(".py") && self.py_files.insert(rel.to_string()),
            Err(_) => {
                let n = self.py_files.len();
                self.py_files.retain(|p| p != rel && !p.starts_with(&prefix));
                n != self.py_files.len()
            }
        }
    }

    /// Returns once the watcher has reported every change made before the call. Client messages that arrive
    /// meanwhile wait in the backlog.
    fn sync(&mut self) {
        self.cookies += 1;
        let cookie = self.paths.dir().join(format!("sync-{}-{}", std::process::id(), self.cookies));
        if let Err(e) = std::fs::write(&cookie, b"") {
            return self.rescan(&format!("writing {}: {e}", cookie.display()));
        }
        let deadline = Instant::now() + SYNC_TIMEOUT;
        loop {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Msg::Fs(e)) => {
                    let seen = e.as_ref().is_ok_and(|e| e.paths.contains(&cookie));
                    self.on_fs(e);
                    if seen {
                        break;
                    }
                }
                Ok(other) => self.backlog.push_back(other),
                Err(_) => {
                    self.rescan("the file watcher did not report a sync file in time");
                    break;
                }
            }
        }
        let _ = std::fs::remove_file(&cookie);
    }

    fn serve_client(&mut self, mut stream: UnixStream, python: &Path, mut req: Request) {
        if python != self.project.python() {
            let reason = format!("it runs {}, this run asks for {}", self.project.python().display(), python.display());
            let _ = writeln!(stream, "{}", json!({"refused": reason}));
            return;
        }
        let t = Instant::now();
        self.sync();
        crate::run::log(|| format!("synced with the file watcher in {} us", t.elapsed().as_micros()));
        if self.shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        req.stdout_to_stderr = false;
        let previous = (std::mem::replace(&mut self.args, req.pytest_args.clone()), self.fork_from);
        self.fork_from = req.fork_from;
        self.shared.cancel.store(false, Ordering::SeqCst);
        let cancel = self.shared.cancel.clone();
        let mut writer = std::io::BufWriter::new(&stream);
        let result = self.project.run(&req, &mut |e: &Event| {
            let sent = writeln!(writer, "{}", e.to_line()).and_then(|_| writer.flush());
            if sent.is_err() {
                cancel.store(true, Ordering::SeqCst);
            }
        });
        drop(writer);
        let result_code = result.as_ref().ok().copied();
        if !self.shared.stopping.load(Ordering::SeqCst) {
            let end = match result {
                Ok(code) => json!({"exit": code, "pid": std::process::id()}),
                Err(e) => json!({"error": format!("{e:#}")}),
            };
            let _ = writeln!(stream, "{end}");
        }
        // pytest exiting before it reported anything means it rejected these args: keep warming the old ones.
        if matches!(result_code, Some(code) if code != 0) {
            (self.args, self.fork_from) = previous;
            if self.project.warm().is_none() {
                self.zygote = Zygote::Absent;
                self.rebuild_at = Some(Instant::now());
            }
        }
        self.after_run();
    }

    fn after_run(&mut self) {
        match (self.project.warm().is_some(), self.zygote) {
            (true, _) => self.set_zygote(Zygote::Ready),
            (false, Zygote::Failed) => {}
            (false, _) => {
                self.rebuild_at = Some(Instant::now());
                self.set_zygote(Zygote::Absent);
            }
        }
    }

    fn watch_run(&mut self) {
        self.watch = Some(None);
        self.sync();
        if self.shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let req = self.request(true);
        let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
        let mut reporter = HumanReporter::new(std::io::stdout(), tty, true);
        self.shared.cancel.store(false, Ordering::SeqCst);
        self.shared.watch_run.store(true, Ordering::SeqCst);
        let result = self.project.run(&req, &mut |e: &Event| {
            let _ = reporter.handle(e);
        });
        self.shared.watch_run.store(false, Ordering::SeqCst);
        if let Err(e) = result {
            log(&format!("run failed: {e:#}"));
        }
        if self.shared.cancel.load(Ordering::SeqCst) {
            if !self.shared.stopping.load(Ordering::SeqCst) {
                log("run cancelled by a new change");
            }
        } else {
            println!();
        }
        self.after_run();
    }

    fn shutdown(self) {
        log("shutting down");
        let Daemon { paths, project, backlog, .. } = self;
        drop(backlog);
        drop(project);
        let ours = std::fs::read_to_string(paths.pid()).is_ok_and(|s| s.trim() == std::process::id().to_string());
        if ours {
            let _ = std::fs::remove_file(paths.sock());
            let _ = std::fs::remove_file(paths.pid());
        }
    }
}
