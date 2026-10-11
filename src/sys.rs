// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the machine is doing: how long since the person touched it, whether it
//! runs on battery, and the scheduling priority this process yields with.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// Milliseconds since the last keyboard or mouse input anywhere on the desktop.
#[cfg(windows)]
pub fn idle_ms() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    unsafe {
        let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        if GetLastInputInfo(&mut li) == 0 {
            return u64::MAX;
        }
        GetTickCount().wrapping_sub(li.dwTime) as u64
    }
}
#[cfg(not(windows))]
pub fn idle_ms() -> u64 {
    u64::MAX
}

/// On battery right now (a desktop without one never is).
#[cfg(windows)]
pub fn on_battery() -> bool {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    unsafe {
        let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
        if GetSystemPowerStatus(&mut s) == 0 {
            return false;
        }
        s.ACLineStatus == 0 && s.BatteryFlag != 128
    }
}
#[cfg(not(windows))]
pub fn on_battery() -> bool {
    false
}

/// The whole process steps aside for anything else the machine runs:
/// `idle` takes only cycles nobody wants, `below_normal` a little more.
#[cfg(windows)]
pub fn set_priority(kind: &str) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    };
    let class = match kind {
        "normal" => NORMAL_PRIORITY_CLASS,
        "below_normal" => BELOW_NORMAL_PRIORITY_CLASS,
        _ => IDLE_PRIORITY_CLASS,
    };
    unsafe {
        SetPriorityClass(GetCurrentProcess(), class);
    }
}
#[cfg(not(windows))]
pub fn set_priority(_kind: &str) {}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// (year, month, day) of a day count since 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// `2026-10-10T05:06:07.123Z`, as `Date.prototype.toISOString` writes it.
pub fn iso(ms: u64) -> String {
    let s = (ms / 1000) as i64;
    let (y, mo, d) = civil(s.div_euclid(86_400));
    let t = s.rem_euclid(86_400);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", t / 3600, t / 60 % 60, t % 60, ms % 1000)
}

/// Local time minus UTC, in minutes.
#[cfg(windows)]
pub fn utc_offset_min() -> i64 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    unsafe {
        let mut l: SYSTEMTIME = std::mem::zeroed();
        let mut u: SYSTEMTIME = std::mem::zeroed();
        GetSystemTime(&mut u);
        GetLocalTime(&mut l);
        let mins = |t: &SYSTEMTIME| t.wDay as i64 * 1440 + t.wHour as i64 * 60 + t.wMinute as i64;
        let mut d = mins(&l) - mins(&u);
        // Across a month boundary the day numbers jump; an offset is within ±14 h.
        if d > 14 * 60 {
            d -= 1440 * ((d + 720) / 1440);
        } else if d < -14 * 60 {
            d += 1440 * ((-d + 720) / 1440);
        }
        d
    }
}
#[cfg(not(windows))]
pub fn utc_offset_min() -> i64 {
    0
}

/// The UTC hour this machine's local midnight falls in: `2026-10-09T16`.
pub fn local_midnight_utc_hour() -> String {
    let off = utc_offset_min() * 60_000;
    let local = now_ms() as i64 + off;
    let midnight_local = local - local.rem_euclid(86_400_000);
    let utc = (midnight_local - off).max(0) as u64;
    iso(utc)[..13].to_string()
}

/// Local wall clock `HH:MM:SS` for the log.
pub fn clock() -> String {
    let local = now_ms() as i64 + utc_offset_min() * 60_000;
    let t = (local / 1000).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "pc".into())
}

/// CPU time so far, in 100 ns: all cores together (busy and idle), the idle
/// part of it, and this process's.
#[cfg(windows)]
pub fn cpu_times() -> Option<(u64, u64, u64)> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut idle, mut kernel, mut user) = (z, z, z);
    let (mut made, mut ended, mut pk, mut pu) = (z, z, z, z);
    // SAFETY: plain out-parameters.
    unsafe {
        if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0
            || GetProcessTimes(GetCurrentProcess(), &mut made, &mut ended, &mut pk, &mut pu) == 0
        {
            return None;
        }
    }
    // The kernel time includes the idle time.
    Some((t(kernel) + t(user), t(idle), t(pk) + t(pu)))
}

#[cfg(not(windows))]
pub fn cpu_times() -> Option<(u64, u64, u64)> {
    None
}

/// Physical memory in use and in all, and this process's working set, in bytes.
#[cfg(windows)]
pub fn memory() -> Option<(u64, u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain out-parameters, their sizes given.
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == 0 {
            return None;
        }
        let mut p: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        p.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let me = if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut p, p.cb) != 0 { p.WorkingSetSize as u64 } else { 0 };
        Some((m.ullTotalPhys - m.ullAvailPhys, m.ullTotalPhys, me))
    }
}

#[cfg(not(windows))]
pub fn memory() -> Option<(u64, u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_matches_js() {
        assert_eq!(super::iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(super::iso(1_791_633_967_123), "2026-10-10T12:06:07.123Z");
    }
}
