//! OS thread counts of other processes, which see native threads `threading` cannot. Unit U2.

/// `None` when the process is gone or the count is unavailable.
pub fn os_threads(pid: i32) -> Option<u32> {
    todo!("U2: proc_pidinfo(PROC_PIDTASKINFO) on macOS, /proc/<pid>/status on Linux")
}
