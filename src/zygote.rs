//! The process layer: L0, L1 and workers, and where workers fork from. Unit U2.
//!
//! A `Session` owns a unix socket server and the Python processes of one root and one set of pytest args.
//! One-shot runs use a session once; the daemon keeps one warm.

use crate::events::{Frame as TraceFrame, Outcome, TestResult};
use crate::{proto, python, threads};
use anyhow::{Context, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkPoint {
    /// Workers fork from L1 and inherit its collected session.
    Collected,
    /// L1 had threads: workers fork from L0 and collect their own batch's files.
    Imported,
    /// L0 had threads: each worker is a fresh interpreter that collects its own files.
    Spawn,
}

impl ForkPoint {
    fn parse(s: &str) -> Option<ForkPoint> {
        match s.trim() {
            "collected" => Some(ForkPoint::Collected),
            "imported" => Some(ForkPoint::Imported),
            "spawn" => Some(ForkPoint::Spawn),
            _ => None,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            ForkPoint::Collected => "workers fork from the collected session",
            ForkPoint::Imported => "workers fork from the interpreter before collection and collect their own files",
            ForkPoint::Spawn => "each worker is a fresh interpreter",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkPolicy {
    Auto,
    Force(ForkPoint),
}

pub struct SessionConfig {
    pub root: PathBuf,
    pub python: PathBuf,
    pub pytest_args: Vec<String>,
    pub preimport: Vec<String>,
    pub policy: ForkPolicy,
    /// Child stdout goes to our stderr, so `-s` output cannot corrupt JSON lines.
    pub stdout_to_stderr: bool,
}

#[derive(Clone, Debug)]
pub struct CollectedItem {
    pub nodeid: String,
    /// Root-relative path of the item's file.
    pub path: String,
}

pub struct Collected {
    pub items: Vec<CollectedItem>,
    pub errors: Vec<(String, String)>,
    /// Modules that skipped themselves at import.
    pub skips: Vec<String>,
    pub maxfail: u32,
    pub rootdir: PathBuf,
    pub python_files: Vec<String>,
    /// Name to root-relative path of every project module in `sys.modules`.
    pub modules: HashMap<String, String>,
    /// Third-party modules worth preimporting next time (minus those pytest wants to rewrite).
    pub third_party: Vec<String>,
    /// (path, qualname) of every fixture definition in the project the collected items use.
    pub fixtures: Vec<(String, String)>,
    /// Per collected file: names executed and files read while collecting it.
    pub per_file: HashMap<String, (Vec<(String, String)>, Vec<String>)>,
    /// Where workers will fork from, decided by the thread check and the policy.
    pub fork_point: ForkPoint,
    /// pytest's exit code when it stopped before the run loop (usage error, interrupted collection).
    pub exit_code: Option<i32>,
}

/// One batch, run in order by one worker.
pub struct Batch {
    pub nodeids: Vec<String>,
    /// The files its items live in, for fork points that collect per worker.
    pub files: Vec<String>,
}

/// Run-global interned `(path, qualname)` names, so hits are plain ids.
#[derive(Default)]
pub struct Names {
    pub list: Vec<(Arc<str>, Arc<str>)>,
    index: HashMap<(Arc<str>, Arc<str>), u32>,
}

impl Names {
    fn intern(&mut self, path: &str, qual: &str) -> u32 {
        let key: (Arc<str>, Arc<str>) = (path.into(), qual.into());
        if let Some(&id) = self.index.get(&key) {
            return id;
        }
        let id = self.list.len() as u32;
        self.list.push(key.clone());
        self.index.insert(key, id);
        id
    }
}

pub enum WorkerEvent {
    Result {
        worker: usize,
        result: TestResult,
        cost: f64,
        /// Ids into `Names::list`, fixture locations included.
        hits: Vec<u32>,
        reads: Vec<String>,
    },
    CollectError { worker: usize, nodeid: String, message: String },
    /// The worker died or crashed; these nodeids of its batch never reported.
    Lost { worker: usize, nodeids: Vec<String>, message: String },
}

pub enum Control {
    Continue,
    /// Kill every worker now (`-x`, `--maxfail`), and report nothing more.
    Stop,
}

const HELLO_TIMEOUT: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_millis(20);
/// How long a worker that exited without connecting gets for its connection to show up after all.
const EXIT_GRACE: Duration = Duration::from_millis(500);

pub struct Session {
    config: SessionConfig,
    shim: PathBuf,
    conns: HashMap<u64, Conn>,
    /// `None` when L0 had threads even without preimports, or the policy forces spawning.
    l0: Option<Proc>,
    l1: Option<Proc>,
    tolerated: u32,
    dropped: Vec<String>,
    fork_point: ForkPoint,
    serial: u64,
    /// Where children's stderr goes instead of ours.
    stderr: Option<File>,
    server: Server,
}

struct Proc {
    pid: i32,
    conn: u64,
    /// Set when we spawned it rather than one of our processes forking it.
    child: Option<Child>,
}

struct Conn {
    stream: UnixStream,
    /// This connection's name ids to `Names` ids.
    names: Vec<u32>,
}

enum In {
    Frame(u64, Value, Vec<u8>),
    Closed(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Who {
    L0,
    L1,
}

impl Session {
    /// Starts L0 and waits until its preimports are done and its threads are counted.
    pub fn start(config: SessionConfig) -> anyhow::Result<Session> {
        Session::start_with(config, None)
    }

    fn start_with(config: SessionConfig, stderr: Option<File>) -> anyhow::Result<Session> {
        let server = Server::bind(&config.root)?;
        let mut s = Session {
            shim: python::shim_path()?,
            conns: HashMap::new(),
            l0: None,
            l1: None,
            tolerated: 0,
            dropped: Vec::new(),
            fork_point: ForkPoint::Collected,
            serial: 0,
            stderr,
            server,
            config,
        };
        if s.config.policy == ForkPolicy::Force(ForkPoint::Spawn) {
            s.fork_point = ForkPoint::Spawn;
            return Ok(s);
        }
        let mut preimport = s.config.preimport.clone();
        loop {
            let ready = s.start_l0(&preimport)?;
            let pid = s.l0.as_ref().unwrap().pid;
            s.tolerated = ready["tolerated_threads"].as_u64().unwrap_or(0) as u32;
            let dropped = strings(&ready["dropped"]);
            let os_threads = threads::os_threads(pid);
            let safe = s.fork_safe(os_threads, ready["py_threads"].as_u64());
            let first_drop = !dropped.is_empty() && s.dropped.is_empty();
            preimport.retain(|m| !dropped.iter().any(|d| m.split('.').next() == Some(d)));
            s.dropped.extend(dropped);
            if !first_drop && !safe && !preimport.is_empty() {
                preimport.clear();
            } else if !first_drop {
                if !safe && s.config.policy == ForkPolicy::Auto {
                    eprintln!(
                        "forktest: {} OS threads in a bare interpreter; {}",
                        os_threads.map_or("?".into(), |n| n.to_string()),
                        ForkPoint::Spawn.describe()
                    );
                    s.kill_l0();
                    s.fork_point = ForkPoint::Spawn;
                }
                return Ok(s);
            }
            s.kill_l0();
        }
    }

    /// Top-level preimports L0 dropped because importing them started threads.
    pub fn dropped_preimports(&self) -> &[String] {
        &self.dropped
    }

    /// Forks L1 from L0 and collects, restricted to `files` when given.
    pub fn collect(&mut self, files: Option<&[String]>) -> anyhow::Result<Collected> {
        self.drop_l1();
        self.serial += 1;
        let cid = self.serial;
        let collect = json!({"type": "collect", "cid": cid, "args": self.config.pytest_args, "files": files});
        let child = match &self.l0 {
            Some(l0) => {
                let conn = l0.conn;
                self.send(conn, &collect)?;
                None
            }
            None => Some(self.spawn(&["l1", &cid.to_string()])?),
        };
        let spawned = child.is_some();
        self.l1 = Some(Proc { pid: child.as_ref().map_or(0, |c| c.id() as i32), conn: u64::MAX, child });
        if spawned {
            groups::add(self.l1.as_ref().unwrap().pid);
        }
        let (conn, pid) = self.wait("L1 to connect", Who::L1, |_, conn, msg, _| {
            (msg["type"] == "hello" && msg["role"] == "l1" && msg["cid"] == cid)
                .then(|| (conn, msg["pid"].as_i64().unwrap_or(0) as i32))
        })?;
        let l1 = self.l1.as_mut().unwrap();
        l1.conn = conn;
        if !spawned {
            l1.pid = pid;
            groups::add(pid);
        } else {
            self.send(conn, &json!({"type": "config", "preimport": [], "root": self.config.root}))?;
            self.send(conn, &collect)?;
        }
        let msg = self.wait("collection", Who::L1, |_, c, msg, _| {
            (c == conn && matches!(msg["type"].as_str(), Some("collected" | "failed" | "crash"))).then(|| msg.clone())
        })?;
        match msg["type"].as_str() {
            Some("failed") => {
                self.drop_l1();
                Ok(Collected::failed(msg["code"].as_i64().unwrap_or(1) as i32, self.fork_point))
            }
            Some("crash") => {
                self.drop_l1();
                bail!("collection crashed: {}", msg["message"].as_str().unwrap_or_default())
            }
            _ => {
                let py_threads = msg["py_threads"].as_u64();
                let os_threads = threads::os_threads(pid);
                let requested = match self.config.policy {
                    ForkPolicy::Force(p) => Some(p),
                    ForkPolicy::Auto => msg["fork_from"].as_str().and_then(ForkPoint::parse),
                };
                self.fork_point = match requested {
                    Some(ForkPoint::Imported) if self.l0.is_none() => ForkPoint::Spawn,
                    Some(p) => p,
                    None if self.fork_safe(os_threads, py_threads) => ForkPoint::Collected,
                    None => {
                        let fallback = self.fallback();
                        eprintln!(
                            "forktest: {} OS threads ({} Python) after collection; {}",
                            os_threads.map_or("?".into(), |n| n.to_string()),
                            py_threads.unwrap_or(0),
                            fallback.describe()
                        );
                        fallback
                    }
                };
                Ok(Collected::parse(&msg, self.fork_point))
            }
        }
    }

    /// Runs batches from the current fork point, in parallel, streaming events. `nice` lowers worker priority.
    /// Returns when every worker has finished or been killed, or `cancel` was set.
    pub fn run(
        &mut self,
        batches: Vec<Batch>,
        nice: bool,
        names: &mut Names,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(WorkerEvent, &Names) -> Control,
    ) -> anyhow::Result<()> {
        if batches.is_empty() {
            return Ok(());
        }
        self.serial += 1;
        let rid = self.serial;
        let fork_point = self.checked_fork_point()?;
        let mut slots: Vec<Slot> = batches.iter().map(Slot::new).collect();
        let parent = match fork_point {
            ForkPoint::Collected => {
                let conn = self.l1.as_ref().unwrap().conn;
                let batches: Vec<_> = batches.iter().map(|b| &b.nodeids).collect();
                self.send(conn, &json!({"type": "run", "rid": rid, "batches": batches, "nice": nice}))?;
                Some(conn)
            }
            ForkPoint::Imported => {
                let conn = self.l0.as_ref().unwrap().conn;
                let batches: Vec<_> =
                    batches.iter().map(|b| json!({"nodeids": b.nodeids, "files": b.files})).collect();
                let args = &self.config.pytest_args;
                self.send(conn, &json!({"type": "run_workers", "rid": rid, "args": args, "batches": batches, "nice": nice}))?;
                Some(conn)
            }
            ForkPoint::Spawn => {
                for (index, slot) in slots.iter_mut().enumerate() {
                    let child = self.spawn(&["worker", &rid.to_string(), &index.to_string()])?;
                    groups::add(child.id() as i32);
                    slot.child = Some(child);
                }
                None
            }
        };
        let mut by_conn: HashMap<u64, usize> = HashMap::new();
        let mut parent_gone = false;
        let mut halted = false;
        let halt = |slots: &mut [Slot], halted: &mut bool| {
            *halted = true;
            for slot in slots.iter_mut().filter(|s| !s.finished) {
                slot.kill();
            }
        };
        while slots.iter().any(|s| !s.finished) {
            if !halted && cancel.load(Ordering::Relaxed) {
                halt(&mut slots, &mut halted);
            }
            match self.next(TICK) {
                Some(In::Frame(conn, msg, blob)) => {
                    let kind = msg["type"].as_str().unwrap_or_default();
                    if kind == "hello" {
                        let index = msg["index"].as_u64().unwrap_or(u64::MAX) as usize;
                        if msg["role"] != "worker" || msg["rid"] != rid || index >= slots.len() {
                            continue;
                        }
                        let slot = &mut slots[index];
                        slot.conn = Some(conn);
                        slot.pid = msg["pid"].as_i64().map(|p| p as i32);
                        by_conn.insert(conn, index);
                        if halted || slot.finished {
                            slot.kill();
                        } else if fork_point == ForkPoint::Spawn {
                            let b = &batches[index];
                            self.send(conn, &json!({"type": "config", "preimport": [], "root": self.config.root}))?;
                            let args = &self.config.pytest_args;
                            self.send(conn, &json!({"type": "work", "args": args, "nodeids": b.nodeids, "files": b.files, "nice": nice}))?;
                        }
                    } else if kind == "exited" && Some(conn) == parent && msg["rid"] == rid {
                        if let Some(slot) = msg["index"].as_u64().and_then(|i| slots.get_mut(i as usize)) {
                            slot.exited.get_or_insert((msg["status"].as_i64().unwrap_or(-1), Instant::now()));
                        }
                    } else if let Some(&index) = by_conn.get(&conn) {
                        if slots[index].finished {
                            continue;
                        }
                        match kind {
                            "result" | "collect_error" if !halted => {
                                let event = self.worker_event(index, conn, &msg, &blob, names, &mut slots[index]);
                                if let Some(event) = event {
                                    if let Control::Stop = sink(event, names) {
                                        halt(&mut slots, &mut halted);
                                    }
                                }
                            }
                            "done" => {
                                let message = "its worker did not collect it";
                                if slots[index].finish(index, halted, message, sink, names) {
                                    halt(&mut slots, &mut halted);
                                }
                            }
                            "crash" => slots[index].crash = msg["message"].as_str().map(str::to_string),
                            _ => {}
                        }
                    }
                }
                Some(In::Closed(conn)) => {
                    if let Some(&index) = by_conn.get(&conn) {
                        slots[index].closed.get_or_insert_with(Instant::now);
                    } else if Some(conn) == parent {
                        parent_gone = true;
                    }
                }
                None => {}
            }
            for index in 0..slots.len() {
                let slot = &mut slots[index];
                if let Some(child) = slot.child.as_mut().filter(|_| slot.exited.is_none()) {
                    if let Ok(Some(status)) = child.try_wait() {
                        slot.exited = Some((status_code(status), Instant::now()));
                    }
                }
                if !slot.finished && slot.settled(halted, parent_gone) {
                    let message = slot.exit_message(parent_gone);
                    if slots[index].finish(index, halted, &message, sink, names) {
                        halt(&mut slots, &mut halted);
                    }
                }
            }
        }
        drop(slots);
        if parent_gone && fork_point == ForkPoint::Collected {
            self.drop_l1();
        }
        for conn in by_conn.keys() {
            self.conns.remove(conn);
        }
        Ok(())
    }

    /// Runs every batch in this process's L1 without forking, for `--strict-pytest`.
    /// L1's session has run tests afterwards, so it is dropped and the next run collects again.
    pub fn run_in_process(
        &mut self,
        nodeids: Vec<String>,
        names: &mut Names,
        sink: &mut dyn FnMut(WorkerEvent, &Names) -> Control,
    ) -> anyhow::Result<()> {
        let conn = self.l1.as_ref().context("run_in_process before collect")?.conn;
        self.serial += 1;
        let rid = self.serial;
        self.send(conn, &json!({"type": "run_in_process", "rid": rid, "nodeids": nodeids}))?;
        let mut slot = Slot::new(&Batch { nodeids, files: Vec::new() });
        loop {
            match self.next(TICK) {
                Some(In::Frame(c, msg, blob)) if c == conn => match msg["type"].as_str() {
                    Some("result") => {
                        let Some(event) = self.worker_event(0, conn, &msg, &blob, names, &mut slot) else { continue };
                        if let Control::Stop = sink(event, names) {
                            break;
                        }
                    }
                    Some("done") if msg["rid"] == rid => {
                        slot.finish(0, false, "pytest did not collect it", sink, names);
                        break;
                    }
                    _ => {}
                },
                Some(In::Closed(c)) if c == conn => {
                    slot.finish(0, false, "the in-process session exited before running this test", sink, names);
                    break;
                }
                _ => {}
            }
        }
        self.drop_l1();
        Ok(())
    }

    pub fn fork_point(&self) -> ForkPoint {
        self.fork_point
    }

    /// Kills L1 (a project file it imported changed) and keeps L0.
    pub fn drop_l1(&mut self) {
        if let Some(mut l1) = self.l1.take() {
            kill_group(l1.pid);
            groups::remove(l1.pid);
            self.conns.remove(&l1.conn);
            if let Some(child) = l1.child.as_mut() {
                let _ = child.wait();
            }
        }
    }

    /// Ends L0, L1 and any worker.
    pub fn shutdown(self) {}

    fn fork_safe(&self, os_threads: Option<u32>, py_threads: Option<u64>) -> bool {
        os_threads.is_some_and(|n| n <= 1 + self.tolerated) && py_threads == Some(1)
    }

    fn fallback(&self) -> ForkPoint {
        if self.l0.is_some() { ForkPoint::Imported } else { ForkPoint::Spawn }
    }

    /// The fork point, after counting the threads of the process about to fork, which may have started some since.
    fn checked_fork_point(&mut self) -> anyhow::Result<ForkPoint> {
        if self.fork_point == ForkPoint::Collected {
            let l1 = self.l1.as_ref().context("run before collect")?;
            let n = threads::os_threads(l1.pid);
            if self.config.policy == ForkPolicy::Auto && !n.is_some_and(|n| n <= 1 + self.tolerated) {
                self.fork_point = self.fallback();
                eprintln!("forktest: the collected session now has {n:?} OS threads; {}", self.fork_point.describe());
            }
        }
        if self.fork_point == ForkPoint::Imported {
            let n = self.l0.as_ref().and_then(|l0| threads::os_threads(l0.pid));
            if !n.is_some_and(|n| n <= 1 + self.tolerated) {
                self.fork_point = ForkPoint::Spawn;
                eprintln!("forktest: the interpreter workers fork from has {n:?} OS threads; {}", self.fork_point.describe());
            }
        }
        Ok(self.fork_point)
    }

    fn worker_event(
        &mut self,
        index: usize,
        conn: u64,
        msg: &Value,
        blob: &[u8],
        names: &mut Names,
        slot: &mut Slot,
    ) -> Option<WorkerEvent> {
        let nodeid = msg["nodeid"].as_str().unwrap_or_default().to_string();
        if msg["type"] == "collect_error" {
            let message = msg["message"].as_str().unwrap_or_default().to_string();
            return Some(WorkerEvent::CollectError { worker: index, nodeid, message });
        }
        let local = &mut self.conns.get_mut(&conn)?.names;
        for name in msg["names"].as_array().into_iter().flatten() {
            local.push(names.intern(name[0].as_str().unwrap_or_default(), name[1].as_str().unwrap_or_default()));
        }
        let hits = blob
            .chunks_exact(4)
            .filter_map(|b| local.get(u32::from_le_bytes(b.try_into().unwrap()) as usize).copied())
            .collect();
        slot.pending.remove(&nodeid);
        let result = TestResult {
            outcome: serde_json::from_value::<Outcome>(msg["outcome"].clone()).unwrap_or(Outcome::Error),
            duration: msg["duration"].as_f64().unwrap_or(0.0),
            file: msg["file"].as_str().map(str::to_string),
            line: msg["line"].as_u64().map(|l| l as u32),
            message: msg["message"].as_str().map(str::to_string),
            trace: serde_json::from_value::<Vec<TraceFrame>>(msg["trace"].clone()).unwrap_or_default(),
            nodeid,
        };
        Some(WorkerEvent::Result {
            worker: index,
            result,
            cost: msg["cost"].as_f64().unwrap_or(0.0),
            hits,
            reads: strings(&msg["reads"]),
        })
    }

    fn start_l0(&mut self, preimport: &[String]) -> anyhow::Result<Value> {
        let child = self.spawn(&["l0"])?;
        let pid = child.id() as i32;
        groups::add(pid);
        self.l0 = Some(Proc { pid, conn: u64::MAX, child: Some(child) });
        let conn = self.wait("L0 to connect", Who::L0, |_, conn, msg, _| {
            (msg["type"] == "hello" && msg["role"] == "l0").then_some(conn)
        })?;
        self.l0.as_mut().unwrap().conn = conn;
        self.send(conn, &json!({"type": "config", "preimport": preimport, "root": self.config.root}))?;
        self.wait("L0 to preimport", Who::L0, |_, c, msg, _| (c == conn && msg["type"] == "ready").then(|| msg.clone()))
    }

    fn kill_l0(&mut self) {
        if let Some(mut l0) = self.l0.take() {
            kill_group(l0.pid);
            groups::remove(l0.pid);
            self.conns.remove(&l0.conn);
            if let Some(child) = l0.child.as_mut() {
                let _ = child.wait();
            }
        }
    }

    fn spawn(&self, args: &[&str]) -> anyhow::Result<Child> {
        let mut cmd = Command::new(&self.config.python);
        cmd.arg(&self.shim)
            .args(args)
            .env("FORKTEST_SOCK", &self.server.path)
            .process_group(0)
            .stdin(Stdio::null())
            .stderr(self.stderr_stdio()?)
            .stdout(if self.config.stdout_to_stderr { self.stderr_stdio()? } else { Stdio::inherit() });
        if cfg!(target_os = "macos") {
            cmd.env("OBJC_DISABLE_INITIALIZE_FORK_SAFETY", "YES");
        }
        cmd.spawn().with_context(|| format!("starting {}", self.config.python.display()))
    }

    fn stderr_stdio(&self) -> anyhow::Result<Stdio> {
        Ok(match &self.stderr {
            Some(f) => Stdio::from(f.try_clone()?),
            None => Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?),
        })
    }

    fn send(&mut self, conn: u64, msg: &Value) -> anyhow::Result<()> {
        let c = self.conns.get_mut(&conn).context("the Python process closed its connection")?;
        proto::write_frame(&mut c.stream, msg, &[]).context("writing to a Python process")
    }

    fn next(&mut self, timeout: Duration) -> Option<In> {
        loop {
            match self.server.rx.recv_timeout(timeout) {
                Ok(Incoming::Open(conn, stream)) => {
                    self.conns.insert(conn, Conn { stream, names: Vec::new() });
                }
                Ok(Incoming::Frame(conn, frame)) => return Some(In::Frame(conn, frame.msg, frame.blob)),
                Ok(Incoming::Closed(conn)) => {
                    self.conns.remove(&conn);
                    return Some(In::Closed(conn));
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Waits for the frame `pick` accepts, failing when `who` dies first or never connects.
    fn wait<T>(
        &mut self,
        what: &str,
        who: Who,
        mut pick: impl FnMut(&Self, u64, &Value, &[u8]) -> Option<T>,
    ) -> anyhow::Result<T> {
        let started = Instant::now();
        loop {
            let conn = self.proc(who).map(|p| p.conn).unwrap_or(u64::MAX);
            let l0 = self.l0.as_ref().map(|p| p.conn);
            match self.next(TICK) {
                Some(In::Frame(c, msg, blob)) => {
                    if let Some(t) = pick(self, c, &msg, &blob) {
                        return Ok(t);
                    }
                }
                Some(In::Closed(c)) if c == conn || Some(c) == l0 => {
                    bail!("waiting for {what}: the Python process exited")
                }
                _ => {}
            }
            if let Some(child) = self.proc_mut(who).and_then(|p| p.child.as_mut()) {
                if let Ok(Some(status)) = child.try_wait() {
                    bail!("waiting for {what}: the Python process exited ({status})");
                }
            }
            if conn == u64::MAX && started.elapsed() > HELLO_TIMEOUT {
                bail!("waiting for {what}: no connection after {HELLO_TIMEOUT:?}");
            }
        }
    }

    fn proc(&self, who: Who) -> Option<&Proc> {
        match who {
            Who::L0 => self.l0.as_ref(),
            Who::L1 => self.l1.as_ref(),
        }
    }

    fn proc_mut(&mut self, who: Who) -> Option<&mut Proc> {
        match who {
            Who::L0 => self.l0.as_mut(),
            Who::L1 => self.l1.as_mut(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.drop_l1();
        self.kill_l0();
    }
}

impl Collected {
    fn failed(code: i32, fork_point: ForkPoint) -> Collected {
        Collected {
            items: Vec::new(),
            errors: Vec::new(),
            skips: Vec::new(),
            maxfail: 0,
            rootdir: PathBuf::new(),
            python_files: Vec::new(),
            modules: HashMap::new(),
            third_party: Vec::new(),
            fixtures: Vec::new(),
            per_file: HashMap::new(),
            fork_point,
            exit_code: Some(code),
        }
    }

    fn parse(msg: &Value, fork_point: ForkPoint) -> Collected {
        let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let pairs = |v: &Value| -> Vec<(String, String)> {
            v.as_array().into_iter().flatten().map(|p| (s(&p[0]), s(&p[1]))).collect()
        };
        let items = msg["items"].as_array().into_iter().flatten();
        let per_file = msg["per_file"].as_object().into_iter().flatten();
        Collected {
            items: items.map(|i| CollectedItem { nodeid: s(&i["nodeid"]), path: s(&i["path"]) }).collect(),
            errors: pairs(&msg["errors"]),
            skips: strings(&msg["skips"]),
            maxfail: msg["maxfail"].as_u64().unwrap_or(0) as u32,
            rootdir: PathBuf::from(s(&msg["rootdir"])),
            python_files: strings(&msg["python_files"]),
            modules: msg["modules"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), s(v))).collect(),
            third_party: strings(&msg["third_party"]),
            fixtures: pairs(&msg["fixtures"]),
            per_file: per_file.map(|(k, v)| (k.clone(), (pairs(&v["names"]), strings(&v["reads"])))).collect(),
            fork_point,
            exit_code: None,
        }
    }
}

struct Slot {
    nodeids: Vec<String>,
    pending: HashSet<String>,
    conn: Option<u64>,
    pid: Option<i32>,
    child: Option<Child>,
    crash: Option<String>,
    /// Wait status and when its parent reported it.
    exited: Option<(i64, Instant)>,
    /// When its connection closed.
    closed: Option<Instant>,
    finished: bool,
}

impl Slot {
    fn new(batch: &Batch) -> Slot {
        Slot {
            nodeids: batch.nodeids.clone(),
            pending: batch.nodeids.iter().cloned().collect(),
            conn: None,
            pid: None,
            child: None,
            crash: None,
            exited: None,
            closed: None,
            finished: false,
        }
    }

    /// Whether a worker that will send nothing more is known well enough to report what it left.
    /// A connection closes before its parent reports the wait status, so a closed one waits a little for it.
    fn settled(&self, halted: bool, parent_gone: bool) -> bool {
        match (self.closed, self.exited) {
            (Some(_), _) if halted => true,
            (Some(_), Some(_)) => true,
            (Some(at), None) => parent_gone || at.elapsed() > EXIT_GRACE,
            (None, Some((_, at))) => self.conn.is_none() && at.elapsed() > EXIT_GRACE,
            (None, None) => self.conn.is_none() && parent_gone,
        }
    }

    fn kill(&mut self) {
        if let Some(child) = &self.child {
            kill_group(child.id() as i32);
        } else if let Some(pid) = self.pid {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    fn exit_message(&self, parent_gone: bool) -> String {
        match (&self.crash, self.exited) {
            (Some(crash), _) => crash.clone(),
            (None, Some((status, _))) => format!("worker exited before running this test (wait status {status})"),
            (None, None) if parent_gone && self.conn.is_none() => "the process workers fork from exited".into(),
            (None, None) => "worker exited before running this test".into(),
        }
    }

    /// Reports what never ran unless the run was halted. Returns whether the sink asked to stop.
    fn finish(
        &mut self,
        index: usize,
        halted: bool,
        message: &str,
        sink: &mut dyn FnMut(WorkerEvent, &Names) -> Control,
        names: &Names,
    ) -> bool {
        if self.finished {
            return false;
        }
        self.finished = true;
        let lost: Vec<String> = self.nodeids.iter().filter(|n| self.pending.contains(*n)).cloned().collect();
        if halted || lost.is_empty() {
            return false;
        }
        let message = self.crash.clone().unwrap_or_else(|| message.to_string());
        matches!(sink(WorkerEvent::Lost { worker: index, nodeids: lost, message }, names), Control::Stop)
    }

}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id() as i32;
            kill_group(pid);
            let _ = child.wait();
            groups::remove(pid);
        }
    }
}

fn status_code(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status.into_raw() as i64
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

fn kill_group(pgid: i32) {
    if pgid > 0 {
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

/// Process groups of live Python children, so a fatal signal to us takes them down too.
mod groups {
    use std::sync::Once;
    use std::sync::atomic::{AtomicI32, Ordering};

    static SLOTS: [AtomicI32; 256] = [const { AtomicI32::new(0) }; 256];

    pub fn add(pgid: i32) {
        install();
        for slot in &SLOTS {
            if slot.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return;
            }
        }
    }

    pub fn remove(pgid: i32) {
        for slot in &SLOTS {
            if slot.compare_exchange(pgid, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return;
            }
        }
    }

    fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                let mut old: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, std::ptr::null(), &mut old);
                if old.sa_sigaction != libc::SIG_DFL {
                    continue;
                }
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&mut sa.sa_mask);
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        });
    }

    extern "C" fn on_signal(sig: libc::c_int) {
        for slot in &SLOTS {
            let pgid = slot.load(Ordering::SeqCst);
            if pgid > 0 {
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
            }
        }
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
}

enum Incoming {
    Open(u64, UnixStream),
    Frame(u64, proto::Frame),
    Closed(u64),
}

/// The socket every Python process connects to, with one reader thread per connection feeding one channel.
struct Server {
    path: PathBuf,
    rx: Receiver<Incoming>,
    stop: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
}

impl Server {
    fn bind(root: &Path) -> anyhow::Result<Server> {
        let path = socket_path(root);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = {
            let stop = stop.clone();
            std::thread::spawn(move || accept(listener, tx, &stop))
        };
        Ok(Server { path, rx, stop, acceptor: Some(acceptor) })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn accept(listener: UnixListener, tx: Sender<Incoming>, stop: &AtomicBool) {
    for (conn, stream) in (0u64..).zip(listener.incoming()) {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let Ok(reader) = stream.try_clone() else { continue };
        if tx.send(Incoming::Open(conn, stream)).is_err() {
            return;
        }
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            while let Ok(Some(frame)) = proto::read_frame(&mut reader) {
                if tx.send(Incoming::Frame(conn, frame)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Incoming::Closed(conn));
        });
    }
}

/// `.forktest/run-<pid>-<n>.sock`, or under the temp dir when that is too long for `AF_UNIX` (about 104 bytes on macOS).
fn socket_path(root: &Path) -> PathBuf {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let name = format!("run-{}-{}.sock", std::process::id(), SERIAL.fetch_add(1, Ordering::SeqCst));
    let dir = root.join(".forktest");
    let in_project = dir.join(&name);
    if in_project.as_os_str().len() <= 96 && std::fs::create_dir_all(&dir).is_ok() {
        return in_project;
    }
    std::env::temp_dir().join(format!("forktest-{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::BufRead;
    use std::sync::Mutex;

    /// Tests share the shim path the orphan check looks for, so they run one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());
    const DEMO_PYTHON: &str = "/Users/katakam/repos/faster_pytest/.venv/bin/python";
    const JAX_PYTHON: &str = "/tmp/jaxprobe/.venv/bin/python";

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn interpreter(path: &str) -> Option<PathBuf> {
        let path = PathBuf::from(path);
        if path.is_file() {
            Some(path)
        } else {
            eprintln!("skipping: {} is missing", path.display());
            None
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir()
                .join(format!("forktest-u2-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir.canonicalize().unwrap())
        }

        fn example(name: &str) -> Scratch {
            let s = Scratch::new();
            copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(name), &s.0);
            s
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if [".forktest", "__pycache__", ".pytest_cache"].iter().any(|n| name == *n) {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &to.join(&name));
            } else {
                std::fs::copy(entry.path(), to.join(&name)).unwrap();
            }
        }
    }

    fn config(root: &Path, python: &Path) -> SessionConfig {
        SessionConfig {
            root: root.to_path_buf(),
            python: python.to_path_buf(),
            pytest_args: vec![root.display().to_string(), "-p".into(), "no:cacheprovider".into()],
            preimport: Vec::new(),
            policy: ForkPolicy::Auto,
            stdout_to_stderr: true,
        }
    }

    /// A session whose children write their stderr to a file, so tests can read it.
    fn start(scratch: &Scratch, config: SessionConfig) -> (Session, PathBuf) {
        let log = scratch.0.join(".forktest-stderr.log");
        let file = File::create(&log).unwrap();
        (Session::start_with(config, Some(file)).unwrap(), log)
    }

    fn batches(items: &[CollectedItem], n: usize) -> Vec<Batch> {
        items
            .chunks(items.len().div_ceil(n).max(1))
            .map(|chunk| {
                let mut files: Vec<String> = chunk.iter().map(|i| i.path.clone()).collect();
                files.dedup();
                Batch { nodeids: chunk.iter().map(|i| i.nodeid.clone()).collect(), files }
            })
            .collect()
    }

    #[derive(Default)]
    struct Report {
        results: Vec<(usize, TestResult, Vec<(String, String)>)>,
        lost: Vec<(usize, Vec<String>, String)>,
        collect_errors: Vec<String>,
    }

    impl Report {
        fn sink(&mut self) -> impl FnMut(WorkerEvent, &Names) -> Control + '_ {
            |event, names| {
                match event {
                    WorkerEvent::Result { worker, result, hits, .. } => {
                        let hits = hits
                            .iter()
                            .map(|&h| {
                                let (p, q) = &names.list[h as usize];
                                (p.to_string(), q.to_string())
                            })
                            .collect();
                        self.results.push((worker, result, hits));
                    }
                    WorkerEvent::CollectError { nodeid, .. } => self.collect_errors.push(nodeid),
                    WorkerEvent::Lost { worker, nodeids, message } => self.lost.push((worker, nodeids, message)),
                }
                Control::Continue
            }
        }

        fn counts(&self) -> BTreeMap<String, u64> {
            let mut counts = BTreeMap::new();
            for (_, r, _) in &self.results {
                let word = match r.outcome {
                    Outcome::Passed => "passed",
                    Outcome::Failed => "failed",
                    Outcome::Skipped => "skipped",
                    Outcome::Error => "error",
                    Outcome::Xfailed => "xfailed",
                    Outcome::Xpassed => "xpassed",
                };
                *counts.entry(word.to_string()).or_insert(0) += 1;
            }
            counts
        }
    }

    /// Outcome counts from plain pytest's summary line, run on the same copy.
    fn pytest_counts(python: &Path, root: &Path, extra: &[&str]) -> BTreeMap<String, u64> {
        let out = Command::new(python)
            .args(["-m", "pytest", "-q", "-p", "no:cacheprovider"])
            .args(extra)
            .current_dir(root)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let summary = stdout.lines().rev().find(|l| l.contains(" in ")).unwrap().trim_matches(|c| c == '=' || c == ' ');
        let summary = &summary[..summary.rfind(" in ").unwrap()];
        let mut counts = BTreeMap::new();
        for part in summary.split(", ") {
            let (n, word) = part.split_once(' ').unwrap();
            let word = match word {
                "errors" => "error",
                "warning" | "warnings" => continue,
                w => w,
            };
            counts.insert(word.to_string(), n.parse().unwrap());
        }
        counts
    }

    fn shim_processes() -> Vec<String> {
        let shim = python::shim_path().unwrap().display().to_string();
        let out = Command::new("ps").args(["-axww", "-o", "pid=,command="]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains(&shim)).map(str::to_string).collect()
    }

    /// Waits a little for killed processes to be reaped, then expects exactly `n` shim processes.
    fn assert_shim_processes(n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut procs = shim_processes();
        while procs.len() != n && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            procs = shim_processes();
        }
        assert_eq!(procs.len(), n, "shim processes: {procs:#?}");
    }

    fn assert_no_orphans() {
        assert_shim_processes(0);
        eprintln!("orphan check: no process runs the shim");
    }

    #[test]
    fn demo_runs_in_four_batches_from_the_collected_session_like_pytest() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let scratch = Scratch::example("demo");
        let expected = pytest_counts(&python, &scratch.0, &[]);
        {
            let t = Instant::now();
            let (mut session, _) = start(&scratch, config(&scratch.0, &python));
            let started = t.elapsed();
            let t = Instant::now();
            let collected = session.collect(None).unwrap();
            let collect = t.elapsed();
            assert_eq!(collected.items.len(), 16);
            assert_eq!(collected.fork_point, ForkPoint::Collected);
            assert_eq!(collected.rootdir, scratch.0);
            assert!(collected.third_party.iter().any(|m| m == "json"));
            assert_eq!(collected.modules.get("billing.tax").map(String::as_str), Some("billing/tax.py"));
            assert!(collected.fixtures.contains(&("conftest.py".into(), "ledger".into())), "{:?}", collected.fixtures);
            let t = Instant::now();
            let mut report = Report::default();
            let mut names = Names::default();
            let cancel = AtomicBool::new(false);
            session.run(batches(&collected.items, 4), false, &mut names, &cancel, &mut report.sink()).unwrap();
            let run = t.elapsed();
            eprintln!("demo timings: start {started:?}, collect {collect:?}, run {run:?}");
            assert!(report.lost.is_empty(), "{:?}", report.lost);
            assert_eq!(report.counts(), expected);
            let mut workers: Vec<usize> = report.results.iter().map(|r| r.0).collect();
            workers.sort();
            workers.dedup();
            assert_eq!(workers, [0, 1, 2, 3]);
            let round_vat: Vec<_> =
                report.results.iter().filter(|r| r.1.nodeid.starts_with("tests/test_tax.py::test_round_vat[")).collect();
            assert!(!round_vat.is_empty());
            for (_, result, hits) in round_vat {
                assert!(hits.contains(&("billing/tax.py".into(), "round_vat".into())), "{}: {hits:?}", result.nodeid);
            }
            assert_eq!(session.fork_point(), ForkPoint::Collected);
        }
        assert_no_orphans();
    }

    #[test]
    fn collection_restricted_to_files_returns_only_their_items() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let scratch = Scratch::example("demo");
        {
            let (mut session, _) = start(&scratch, config(&scratch.0, &python));
            let all = session.collect(None).unwrap();
            let expected: Vec<_> =
                all.items.iter().filter(|i| i.path == "tests/test_tax.py").map(|i| i.nodeid.clone()).collect();
            assert!(!expected.is_empty() && expected.len() < all.items.len());
            let only = session.collect(Some(&["tests/test_tax.py".to_string()])).unwrap();
            let got: Vec<_> = only.items.iter().map(|i| i.nodeid.clone()).collect();
            assert_eq!(got, expected);
            assert!(only.per_file.keys().all(|k| k == "tests/test_tax.py"), "{:?}", only.per_file.keys());
            let (names, _) = &only.per_file["tests/test_tax.py"];
            assert!(names.contains(&("tests/test_tax.py".into(), "<module>".into())), "{names:?}");
        }
        assert_no_orphans();
    }

    #[test]
    fn jax_arrays_at_import_move_the_fork_point_to_before_collection() {
        let _serial = serial();
        let Some(python) = interpreter(JAX_PYTHON) else { return };
        let scratch = Scratch::example("jaxdemo");
        {
            let t = Instant::now();
            let (mut session, log) = start(&scratch, config(&scratch.0, &python));
            let started = t.elapsed();
            let l0 = session.l0.as_ref().unwrap().pid;
            let t = Instant::now();
            let collected = session.collect(None).unwrap();
            let collect = t.elapsed();
            let l1_threads = threads::os_threads(session.l1.as_ref().unwrap().pid).unwrap();
            assert_eq!(collected.items.len(), 8);
            assert_eq!(collected.fork_point, ForkPoint::Imported, "L1 has {l1_threads} OS threads");
            assert!(l1_threads > 10, "L1 has {l1_threads} OS threads");

            let cancel = Arc::new(AtomicBool::new(false));
            let watchdog = {
                let cancel = cancel.clone();
                std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(60);
                    while Instant::now() < deadline && !cancel.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    cancel.store(true, Ordering::SeqCst);
                })
            };
            let mut report = Report::default();
            let mut names = Names::default();
            let mut parent_threads = Vec::new();
            let t = Instant::now();
            {
                let mut sink = report.sink();
                let mut sink = |event, names: &Names| {
                    parent_threads.push(threads::os_threads(l0).unwrap());
                    sink(event, names)
                };
                session.run(batches(&collected.items, 4), false, &mut names, &cancel, &mut sink).unwrap();
            }
            let run = t.elapsed();
            let timed_out = cancel.swap(true, Ordering::SeqCst);
            watchdog.join().unwrap();
            assert!(!timed_out, "the run did not finish within 60 s");
            eprintln!("jaxdemo timings: start {started:?}, collect {collect:?} (L1 {l1_threads} OS threads), run {run:?}");
            assert!(report.lost.is_empty(), "{:?}", report.lost);
            assert_eq!(report.counts(), BTreeMap::from([("passed".to_string(), 8)]));
            assert!(parent_threads.iter().all(|&n| n == 1), "L0 OS threads during the run: {parent_threads:?}");

            let lazy = session.collect(Some(&["tests/test_lazy.py".to_string()])).unwrap();
            assert_eq!(lazy.fork_point, ForkPoint::Collected);
            let mut report = Report::default();
            session.run(batches(&lazy.items, 2), false, &mut names, &AtomicBool::new(false), &mut report.sink()).unwrap();
            assert_eq!(report.counts(), BTreeMap::from([("passed".to_string(), 2)]));

            drop(session);
            let stderr = std::fs::read_to_string(log).unwrap();
            assert!(!stderr.contains("os.fork() was called"), "{stderr}");
            assert!(!stderr.contains("use of fork() may lead to deadlocks"), "{stderr}");
        }
        assert_no_orphans();
    }

    #[test]
    fn a_python_thread_started_by_a_conftest_moves_the_fork_point() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let scratch = Scratch::new();
        scratch.write(
            "conftest.py",
            "import threading, time\nthreading.Thread(target=time.sleep, args=(600,), daemon=True).start()\n",
        );
        scratch.write("tests/test_one.py", "def test_one():\n    assert 1\n\ndef test_two():\n    assert 2\n");
        let shim_dir = python::shim_path().unwrap().parent().unwrap().display().to_string();
        scratch.write("tests/test_path.py", &format!("import sys\n\ndef test_shim_dir_not_on_path():\n    assert {shim_dir:?} not in sys.path\n"));
        {
            let (mut session, _) = start(&scratch, config(&scratch.0, &python));
            let collected = session.collect(None).unwrap();
            assert_eq!(collected.fork_point, ForkPoint::Imported);
            let mut report = Report::default();
            let mut names = Names::default();
            session.run(batches(&collected.items, 2), false, &mut names, &AtomicBool::new(false), &mut report.sink()).unwrap();
            assert_eq!(report.counts(), BTreeMap::from([("passed".to_string(), 3)]));
        }
        assert_no_orphans();
    }

    #[test]
    fn strict_pytest_runs_every_item_in_one_process() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let scratch = Scratch::example("demo");
        let expected = pytest_counts(&python, &scratch.0, &[]);
        {
            let (mut session, _) = start(&scratch, config(&scratch.0, &python));
            let collected = session.collect(None).unwrap();
            let nodeids: Vec<String> = collected.items.iter().map(|i| i.nodeid.clone()).collect();
            let mut report = Report::default();
            let mut names = Names::default();
            session.run_in_process(nodeids.clone(), &mut names, &mut report.sink()).unwrap();
            assert_eq!(report.counts(), expected);
            let ran: Vec<_> = report.results.iter().map(|r| r.1.nodeid.clone()).collect();
            assert_eq!(ran, nodeids);
            assert!(session.l1.is_none());
            assert_shim_processes(1);
        }
        assert_no_orphans();
    }

    #[test]
    fn stop_after_the_first_failure_kills_the_other_workers() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let scratch = Scratch::new();
        scratch.write("tests/test_fail.py", "def test_fails():\n    assert 1 == 2\n");
        scratch.write(
            "tests/test_slow.py",
            "import time\n\ndef test_a():\n    time.sleep(60)\n\ndef test_b():\n    time.sleep(60)\n\ndef test_c():\n    time.sleep(60)\n",
        );
        {
            let (mut session, _) = start(&scratch, config(&scratch.0, &python));
            let collected = session.collect(None).unwrap();
            assert_eq!(collected.fork_point, ForkPoint::Collected);
            let batches: Vec<Batch> = collected
                .items
                .iter()
                .map(|i| Batch { nodeids: vec![i.nodeid.clone()], files: vec![i.path.clone()] })
                .collect();
            assert_eq!(batches.len(), 4);
            let mut results = Vec::new();
            let mut lost = 0;
            let t = Instant::now();
            session
                .run(batches, false, &mut Names::default(), &AtomicBool::new(false), &mut |event, _| match event {
                    WorkerEvent::Result { result, .. } => {
                        let stop = result.outcome.is_failure();
                        results.push(result);
                        if stop { Control::Stop } else { Control::Continue }
                    }
                    _ => {
                        lost += 1;
                        Control::Continue
                    }
                })
                .unwrap();
            assert!(t.elapsed() < Duration::from_secs(20), "the run took {:?}", t.elapsed());
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].nodeid, "tests/test_fail.py::test_fails");
            assert_eq!(results[0].message.as_deref(), Some("assert 1 == 2"));
            assert_eq!(results[0].file.as_deref(), Some("tests/test_fail.py"));
            assert_eq!(lost, 0);
            eprintln!("after stop: {:#?}", shim_processes());
            assert_shim_processes(2);
        }
        assert_no_orphans();
    }

    /// Sets `PYTHONPATH` for the Python children of one test; tests are serialized, so nothing else sees it.
    struct PythonPath;

    impl PythonPath {
        fn set(dir: &Path) -> PythonPath {
            unsafe { std::env::set_var("PYTHONPATH", dir) };
            PythonPath
        }
    }

    impl Drop for PythonPath {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("PYTHONPATH") };
        }
    }

    #[test]
    fn a_preimport_that_starts_threads_is_dropped_and_l0_restarts_without_it() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let site = Scratch::new();
        site.write("threadpkg/__init__.py", "import threading, time\nthreading.Thread(target=time.sleep, args=(600,), daemon=True).start()\n");
        let _path = PythonPath::set(&site.0);
        let scratch = Scratch::example("demo");
        {
            let mut config = config(&scratch.0, &python);
            config.preimport = vec!["json".into(), "threadpkg".into(), "threadpkg".into()];
            let (mut session, _) = start(&scratch, config);
            assert_eq!(session.dropped_preimports(), ["threadpkg"]);
            assert_eq!(threads::os_threads(session.l0.as_ref().unwrap().pid), Some(1));
            assert_eq!(session.collect(None).unwrap().fork_point, ForkPoint::Collected);
        }
        assert_no_orphans();
    }

    #[test]
    fn an_interpreter_that_starts_threads_by_itself_spawns_every_worker() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let site = Scratch::new();
        site.write("sitecustomize.py", "import threading, time\nthreading.Thread(target=time.sleep, args=(600,), daemon=True).start()\n");
        let _path = PythonPath::set(&site.0);
        let scratch = Scratch::example("demo");
        let expected = pytest_counts(&python, &scratch.0, &[]);
        {
            let (mut session, _) = start(&scratch, config(&scratch.0, &python));
            assert!(session.l0.is_none());
            assert_eq!(session.fork_point(), ForkPoint::Spawn);
            let collected = session.collect(None).unwrap();
            assert_eq!(collected.fork_point, ForkPoint::Spawn);
            assert_eq!(collected.items.len(), 16);
            let mut report = Report::default();
            let t = Instant::now();
            session.run(batches(&collected.items, 4), false, &mut Names::default(), &AtomicBool::new(false), &mut report.sink()).unwrap();
            eprintln!("demo spawn run: {:?}", t.elapsed());
            assert!(report.lost.is_empty(), "{:?}", report.lost);
            assert_eq!(report.counts(), expected);
        }
        assert_no_orphans();
    }

    const MIXED: &str = r#"import warnings
import pytest

def test_passes():
    assert True

def test_fails():
    assert 1 == 2

def test_skips():
    pytest.skip("not today")

@pytest.mark.xfail
def test_xfails():
    assert False

@pytest.mark.xfail
def test_xpasses():
    assert True

@pytest.fixture
def broken():
    raise RuntimeError("setup")

def test_errors(broken):
    pass

@pytest.mark.filterwarnings("error")
def test_warning_is_an_error():
    warnings.warn("boom", UserWarning)
"#;

    fn mixed_project() -> Scratch {
        let scratch = Scratch::new();
        scratch.write("pyproject.toml", "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n");
        scratch.write("tests/test_mixed.py", MIXED);
        scratch.write("tests/test_skipped_module.py", "import pytest\npytest.importorskip(\"no_such_module_here\")\n");
        scratch.write("tests/test_broken.py", "def test_x(:\n");
        scratch
    }

    #[test]
    fn every_outcome_matches_pytest_from_each_fork_point() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let scratch = mixed_project();
        let mut expected = pytest_counts(&python, &scratch.0, &["--continue-on-collection-errors"]);
        // pytest counts the module that skipped itself and the file that failed to collect; `Collected` reports those.
        *expected.get_mut("skipped").unwrap() -= 1;
        *expected.get_mut("error").unwrap() -= 1;
        for policy in [ForkPolicy::Auto, ForkPolicy::Force(ForkPoint::Imported), ForkPolicy::Force(ForkPoint::Spawn)] {
            let mut config = config(&scratch.0, &python);
            config.policy = policy;
            let (mut session, _) = start(&scratch, config);
            let collected = session.collect(None).unwrap();
            assert_eq!(collected.skips, ["tests/test_skipped_module.py"]);
            assert_eq!(collected.errors.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(), ["tests/test_broken.py"]);
            let mut report = Report::default();
            session.run(batches(&collected.items, 3), false, &mut Names::default(), &AtomicBool::new(false), &mut report.sink()).unwrap();
            assert!(report.lost.is_empty(), "{policy:?}: {:?}", report.lost);
            assert_eq!(report.counts(), expected, "{policy:?}");
            let failed = report.results.iter().find(|r| r.1.nodeid.ends_with("::test_fails")).unwrap();
            assert_eq!(failed.1.line, Some(8), "{policy:?}");
            assert_eq!(failed.1.trace.last().map(|f| f.func.as_str()), Some("test_fails"));
        }
        assert_no_orphans();
    }

    #[test]
    fn the_fork_from_ini_option_overrides_the_thread_check() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let scratch = Scratch::new();
        scratch.write("pyproject.toml", "[tool.pytest.ini_options]\nforktest_fork_from = \"imported\"\n");
        scratch.write("tests/test_one.py", "def test_one():\n    assert 1\n");
        {
            let (mut session, log) = start(&scratch, config(&scratch.0, &python));
            assert_eq!(session.collect(None).unwrap().fork_point, ForkPoint::Imported);
            drop(session);
            let stderr = std::fs::read_to_string(log).unwrap();
            assert!(!stderr.contains("Unknown config option"), "{stderr}");
        }
        assert_no_orphans();
    }

    #[test]
    fn a_worker_that_dies_mid_batch_loses_the_rest_of_its_batch() {
        let _serial = serial();
        let Some(python) = interpreter(DEMO_PYTHON) else { return };
        let scratch = Scratch::new();
        scratch.write(
            "tests/test_die.py",
            "import os\n\ndef test_before():\n    pass\n\ndef test_dies():\n    os._exit(3)\n\ndef test_after():\n    pass\n",
        );
        {
            let (mut session, _) = start(&scratch, config(&scratch.0, &python));
            let collected = session.collect(None).unwrap();
            let mut report = Report::default();
            session.run(batches(&collected.items, 1), false, &mut Names::default(), &AtomicBool::new(false), &mut report.sink()).unwrap();
            assert_eq!(report.counts(), BTreeMap::from([("passed".to_string(), 1)]));
            let [(worker, nodeids, message)] = &report.lost[..] else { panic!("{:?}", report.lost) };
            assert_eq!(*worker, 0);
            assert_eq!(nodeids, &["tests/test_die.py::test_dies", "tests/test_die.py::test_after"]);
            assert!(message.contains("wait status 768"), "{message}");
        }
        assert_no_orphans();
    }

    #[test]
    fn a_fatal_signal_to_the_rust_process_takes_its_python_children_down() {
        let _serial = serial();
        let Some(_) = interpreter(DEMO_PYTHON) else { return };
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGKILL] {
            signal_target_then(signal);
            eprintln!("signal {signal}: no orphans");
        }
    }

    fn signal_target_then(signal: i32) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["zygote::tests::signal_target", "--exact", "--ignored", "--nocapture"])
            .env("FORKTEST_SIGNAL_TARGET", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let ready = stdout.lines().map_while(Result::ok).any(|l| l.contains("signal target ready"));
        assert!(ready);
        assert!(shim_processes().len() >= 2);
        unsafe { libc::kill(child.id() as i32, signal) };
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert_no_orphans();
    }

    /// Run by the SIGTERM test as a child: a collected session, then waiting to be signalled.
    #[test]
    #[ignore]
    fn signal_target() {
        if std::env::var_os("FORKTEST_SIGNAL_TARGET").is_none() {
            return;
        }
        let python = PathBuf::from(DEMO_PYTHON);
        let scratch = Scratch::example("demo");
        let mut session = Session::start(config(&scratch.0, &python)).unwrap();
        session.collect(None).unwrap();
        println!("signal target ready");
        std::thread::sleep(Duration::from_secs(60));
    }
}
