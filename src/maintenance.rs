// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bound process-owned engine allocations, including those a DLL unload
//! cannot return. Drain before replacement; a ceiling is not a hard limit.

use std::time::{Duration, Instant};

const SUSTAIN: Duration = Duration::from_secs(60);
const DRAIN_MAX: Duration = Duration::from_secs(15 * 60);

pub struct MemoryGuard {
    limit: u64,
    over: Option<Instant>,
    draining: Option<Instant>,
}

impl MemoryGuard {
    pub fn new(mib: u64) -> Self {
        Self { limit: mib.saturating_mul(1 << 20), over: None, draining: None }
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
        self.draining.is_some_and(|start| empty || now.duration_since(start) >= DRAIN_MAX)
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
