//! OS thread counts of other processes, which see native threads `threading` cannot. Unit U2.

/// `None` when the process is gone or the count is unavailable.
#[cfg(target_os = "macos")]
pub fn os_threads(pid: i32) -> Option<u32> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
    (written == size).then_some(info.pti_threadnum as u32)
}

#[cfg(target_os = "linux")]
pub fn os_threads(pid: i32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|line| line.strip_prefix("Threads:")?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_the_one_thread_of_a_child() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let count = os_threads(child.id() as i32);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(count, Some(1));
    }

    #[test]
    fn a_missing_process_has_no_count() {
        assert_eq!(os_threads(i32::MAX), None);
    }
}
