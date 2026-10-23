// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bound process-owned engine allocations, including those a DLL unload
//! cannot return. Drain before replacement; a ceiling is not a hard limit.

use std::time::{Duration, Instant};

/// Windows Job commit ceiling includes this worker and all GPU descendants.
/// Lifetime spans the run; changing it uses the normal drain/restart path.
#[cfg(windows)]
pub struct ComputeBudget(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl ComputeBudget {
    pub fn new(mib: u64) -> anyhow::Result<Self> {
        use windows_sys::Win32::{System::JobObjects::*, System::Threading::GetCurrentProcess};
        anyhow::ensure!(
            (4096..=65536).contains(&mib),
            "CPU+GPU 内存上限应在 4096..=65536 MiB"
        );
        unsafe {
            let name: Vec<u16> = format!("Local\\simfleet-compute-{}", std::process::id())
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let job = Self(CreateJobObjectW(std::ptr::null(), name.as_ptr()));
            anyhow::ensure!(
                !job.0.is_null(),
                "无法创建计算内存限制：{}",
                std::io::Error::last_os_error()
            );
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY;
            // Leave 64 MiB for the separate, small restart supervisor.
            info.JobMemoryLimit = ((mib - 64) as usize) * 1024 * 1024;
            anyhow::ensure!(
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32
                ) != 0,
                "设置计算内存上限失败：{}",
                std::io::Error::last_os_error()
            );
            anyhow::ensure!(
                AssignProcessToJobObject(job.0, GetCurrentProcess()) != 0,
                "应用计算内存上限失败：{}",
                std::io::Error::last_os_error()
            );
            Ok(job)
        }
    }
}
#[cfg(windows)]
impl Drop for ComputeBudget {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(not(windows))]
pub struct ComputeBudget;
#[cfg(not(windows))]
impl ComputeBudget {
    pub fn new(_mib: u64) -> anyhow::Result<Self> {
        Ok(Self)
    }
}

const SUSTAIN: Duration = Duration::from_secs(60);
const DRAIN_MAX: Duration = Duration::from_secs(15 * 60);

pub struct MemoryGuard {
    limit: u64,
    over: Option<Instant>,
    draining: Option<Instant>,
}

impl MemoryGuard {
    pub fn new(mib: u64) -> Self {
        Self {
            limit: mib.saturating_mul(1 << 20),
            over: None,
            draining: None,
        }
    }

    /// True only when entering the drain. A short peak does not restart a
    /// client; twice the ceiling is urgent enough not to wait a minute.
    pub fn observe(&mut self, bytes: Option<u64>, now: Instant) -> bool {
        if self.limit == 0 || self.draining.is_some() {
            return false;
        }
        let Some(bytes) = bytes else { return false };
        if bytes < self.limit {
            self.over = None;
            return false;
        }
        let since = *self.over.get_or_insert(now);
        if bytes >= self.limit.saturating_mul(2) || now.duration_since(since) >= SUSTAIN {
            self.draining = Some(now);
            return true;
        }
        false
    }

    pub fn draining(&self) -> bool {
        self.draining.is_some()
    }

    /// If a task cannot finish, ordinary shutdown returns its lease and keeps
    /// computed results. It must not prevent reclamation indefinitely.
    pub fn ready(&self, empty: bool, now: Instant) -> bool {
        self.draining
            .is_some_and(|start| empty || now.duration_since(start) >= DRAIN_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_peak_recovers_but_a_sustained_peak_drains() {
        let mut guard = MemoryGuard::new(100);
        let t = Instant::now();
        assert!(!guard.observe(Some(101 << 20), t));
        assert!(!guard.observe(Some(99 << 20), t + Duration::from_secs(59)));
        assert!(!guard.observe(Some(101 << 20), t + Duration::from_secs(60)));
        assert!(!guard.observe(None, t + Duration::from_secs(100)));
        assert!(guard.observe(Some(101 << 20), t + Duration::from_secs(120)));
        assert!(!guard.ready(false, t + Duration::from_secs(121)));
        assert!(guard.ready(true, t + Duration::from_secs(121)));
        assert!(!guard.observe(Some(300 << 20), t + Duration::from_secs(122)));
        assert!(guard.draining());
    }

    #[test]
    fn urgent_pressure_and_a_stuck_task_have_a_bounded_wait() {
        let mut guard = MemoryGuard::new(100);
        let t = Instant::now();
        assert!(guard.observe(Some(200 << 20), t));
        assert!(!guard.ready(false, t + DRAIN_MAX - Duration::from_secs(1)));
        assert!(guard.ready(false, t + DRAIN_MAX));
        let mut disabled = MemoryGuard::new(0);
        assert!(!disabled.observe(Some(u64::MAX), t));
        assert!(!disabled.ready(true, t + DRAIN_MAX));
    }
}
