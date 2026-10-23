// SPDX-License-Identifier: AGPL-3.0-or-later
//! WDDM counters for exactly this client's GPU child, sampled off dispatch/UI.
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Default, Debug, serde::Serialize)]
pub struct Sample {
    /// Busiest engine for this PID, as in Task Manager; not SM occupancy.
    pub utilization: Option<f64>,
    pub dedicated_bytes: Option<u64>,
    pub shared_bytes: Option<u64>,
}
pub struct Monitor {
    value: Arc<Mutex<(Instant, Sample)>>,
    _stop: mpsc::Sender<()>,
}
impl Monitor {
    pub fn new(pid: u32) -> Self {
        let value = Arc::new(Mutex::new((Instant::now(), Sample::default())));
        let copy = value.clone();
        let (stop, rx) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("gpu-meter".into())
            .spawn(move || {
                let mut counter = Counter::new(pid);
                loop {
                    let sample = counter
                        .as_mut()
                        .map_or_else(Sample::default, Counter::sample);
                    if let Ok(mut value) = copy.lock() {
                        *value = (Instant::now(), sample);
                    }
                    if rx.recv_timeout(Duration::from_secs(1))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                }
            });
        Self { value, _stop: stop }
    }
    pub fn read(&self) -> Sample {
        self.value
            .lock()
            .ok()
            .filter(|v| v.0.elapsed() < Duration::from_secs(4))
            .map_or_else(Sample::default, |v| v.1)
    }
}

#[cfg(not(windows))]
struct Counter;
#[cfg(not(windows))]
impl Counter {
    fn new(_: u32) -> Option<Self> {
        None
    }
    fn sample(&mut self) -> Sample {
        Sample::default()
    }
}

/// Instance PIDs need exact token matching (12 must never match 123).
fn belongs(name: &str, pid: u32) -> bool {
    name.strip_prefix("pid_")
        .and_then(|s| s.split_once('_'))
        .and_then(|(id, _)| id.parse::<u32>().ok())
        == Some(pid)
}
fn utilization(samples: impl Iterator<Item = (String, f64)>, pid: u32) -> Option<f64> {
    let mut value = 0.0f64;
    let mut seen = false;
    for (name, n) in samples {
        if belongs(&name, pid) && n.is_finite() && n >= 0.0 {
            seen = true;
            value = value.max(n);
        }
    }
    seen.then_some((value / 100.0).clamp(0.0, 1.0))
}

#[cfg(windows)]
struct Counter {
    query: windows_sys::Win32::System::Performance::PDH_HQUERY,
    engine: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
    dedicated: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
    shared: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
    pid: u32,
    primed: bool,
}
#[cfg(windows)]
impl Counter {
    fn new(pid: u32) -> Option<Self> {
        use windows_sys::Win32::System::Performance::*;
        unsafe {
            let mut result = Self {
                query: std::ptr::null_mut(),
                engine: std::ptr::null_mut(),
                dedicated: std::ptr::null_mut(),
                shared: std::ptr::null_mut(),
                pid,
                primed: false,
            };
            if PdhOpenQueryW(std::ptr::null(), 0, &mut result.query) != 0 {
                return None;
            }
            for (suffix, counter) in [
                ("GPU Engine(*)\\Utilization Percentage", &mut result.engine),
                (
                    "GPU Process Memory(*)\\Dedicated Usage",
                    &mut result.dedicated,
                ),
                ("GPU Process Memory(*)\\Shared Usage", &mut result.shared),
            ] {
                let path: Vec<u16> = format!("\\{suffix}")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                if PdhAddEnglishCounterW(result.query, path.as_ptr(), 0, counter) != 0 {
                    *counter = std::ptr::null_mut();
                }
            }
            if result.engine.is_null() && result.dedicated.is_null() {
                return None;
            }
            Some(result)
        }
    }
    fn sample(&mut self) -> Sample {
        use windows_sys::Win32::System::Performance::*;
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return Sample::default();
        }
        if !std::mem::replace(&mut self.primed, true) {
            return Sample::default();
        }
        let usage = Self::read(self.engine, PDH_FMT_DOUBLE)
            .and_then(|a| utilization(a.into_iter(), self.pid));
        let bytes = |handle| {
            Self::read(handle, PDH_FMT_LARGE).map(|a| {
                a.into_iter()
                    .filter(|(n, _)| belongs(n, self.pid))
                    .map(|(_, v)| v.max(0.0) as u64)
                    .fold(0u64, u64::saturating_add)
            })
        };
        Sample {
            utilization: usage,
            dedicated_bytes: bytes(self.dedicated),
            shared_bytes: bytes(self.shared),
        }
    }
    fn read(
        counter: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
        format: u32,
    ) -> Option<Vec<(String, f64)>> {
        use windows_sys::Win32::System::Performance::*;
        if counter.is_null() {
            return None;
        }
        for _ in 0..3 {
            let (mut bytes, mut count) = (0u32, 0u32);
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    counter,
                    format,
                    &mut bytes,
                    &mut count,
                    std::ptr::null_mut(),
                )
            };
            if status == 0 && bytes == 0 {
                return Some(Vec::new());
            }
            if status != PDH_MORE_DATA || bytes == 0 || bytes > 4 << 20 {
                return None;
            }
            // u64 provides alignment for PDH_FMT_COUNTERVALUE_ITEM_W and its union.
            let mut storage = vec![0u64; (bytes as usize).div_ceil(8)];
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    counter,
                    format,
                    &mut bytes,
                    &mut count,
                    storage.as_mut_ptr().cast(),
                )
            };
            if status == PDH_MORE_DATA {
                continue;
            }
            if status != 0
                || count as usize * std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()
                    > storage.len() * 8
            {
                return None;
            }
            let begin = storage.as_ptr() as usize;
            let end = begin + storage.len() * 8;
            let items = unsafe {
                std::slice::from_raw_parts(
                    storage.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                    count as usize,
                )
            };
            let mut output = Vec::with_capacity(items.len());
            for item in items {
                if !matches!(
                    item.FmtValue.CStatus,
                    PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
                ) {
                    continue;
                }
                let at = item.szName as usize;
                if at < begin || at >= end || at % 2 != 0 {
                    continue;
                }
                let text = unsafe { std::slice::from_raw_parts(item.szName, (end - at) / 2) };
                let n = text.iter().position(|ch| *ch == 0)?;
                let value = unsafe {
                    if format == PDH_FMT_DOUBLE {
                        item.FmtValue.Anonymous.doubleValue
                    } else {
                        item.FmtValue.Anonymous.largeValue as f64
                    }
                };
                output.push((String::from_utf16_lossy(&text[..n]), value));
            }
            return Some(output);
        }
        None
    }
}
#[cfg(windows)]
impl Drop for Counter {
    fn drop(&mut self) {
        if !self.query.is_null() {
            unsafe {
                windows_sys::Win32::System::Performance::PdhCloseQuery(self.query);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utilization_is_pid_specific_and_busiest_engine_not_sum() {
        let sample = vec![
            ("pid_12_luid_a_eng_0".into(), 75.0),
            ("pid_12_luid_a_eng_1".into(), 80.0),
            ("pid_123_luid_a_eng_0".into(), 100.0),
        ];
        assert_eq!(utilization(sample.into_iter(), 12), Some(0.8));
        assert!(!belongs("pid_123_luid_0", 12));
        assert!(!belongs("other_12_luid_0", 12));
        assert_eq!(
            utilization(vec![("pid_12_luid_0".into(), f64::NAN)].into_iter(), 12),
            None
        );
    }
}
