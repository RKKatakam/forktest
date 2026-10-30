//! The process layer: L0, L1 and workers, and where workers fork from. Unit U2.
//!
//! A `Session` owns a unix socket server and the Python processes of one root and one set of pytest args.
//! One-shot runs use a session once; the daemon keeps one warm.

use crate::events::TestResult;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkPoint {
    /// Workers fork from L1 and inherit its collected session.
    Collected,
    /// L1 had threads: workers fork from L0 and collect their own batch's files.
    Imported,
    /// L0 had threads: each worker is a fresh interpreter that collects its own files.
    Spawn,
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

pub struct Session {}

impl Session {
    /// Starts L0 and waits until its preimports are done and its threads are counted.
    pub fn start(config: SessionConfig) -> anyhow::Result<Session> {
        todo!("U2")
    }

    /// Forks L1 from L0 and collects, restricted to `files` when given.
    pub fn collect(&mut self, files: Option<&[String]>) -> anyhow::Result<Collected> {
        todo!("U2")
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
        todo!("U2")
    }

    /// Runs every batch in this process's L1 without forking, for `--strict-pytest`.
    pub fn run_in_process(
        &mut self,
        nodeids: Vec<String>,
        names: &mut Names,
        sink: &mut dyn FnMut(WorkerEvent, &Names) -> Control,
    ) -> anyhow::Result<()> {
        todo!("U2")
    }

    pub fn fork_point(&self) -> ForkPoint {
        todo!("U2")
    }

    /// Kills L1 (a project file it imported changed) and keeps L0.
    pub fn drop_l1(&mut self) {
        todo!("U2")
    }

    /// Ends L0, L1 and any worker.
    pub fn shutdown(self) {
        todo!("U2")
    }
}
