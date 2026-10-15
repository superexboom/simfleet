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
/// No input to watch here (a server, or a Linux desktop): always away, so the
/// budget is `idle_cores`.
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
/// Linux: a battery that says it is discharging.
#[cfg(not(windows))]
pub fn on_battery() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else { return false };
    dir.flatten().any(|e| {
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
        read("type").trim() == "Battery" && read("status").trim() == "Discharging"
    })
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
/// Unix: `idle` is SCHED_IDLE where there is one, and nice 19; `below_normal`
/// nice 10. Set before any thread starts, so every thread inherits it.
#[cfg(unix)]
pub fn set_priority(kind: &str) {
    let nice = match kind {
        "normal" => return,
        "below_normal" => 10,
        _ => 19,
    };
    // SAFETY: plain calls on this process / thread.
    unsafe {
        #[cfg(target_os = "linux")]
        if nice == 19 {
            let p = libc::sched_param { sched_priority: 0 };
            libc::sched_setscheduler(0, libc::SCHED_IDLE, &p);
        }
        libc::setpriority(libc::PRIO_PROCESS, 0, nice);
    }
}
#[cfg(not(any(windows, unix)))]
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

/// The day count since 1970-01-01 of a civil date (`civil` backwards).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let doy = (153 * ((m as i64 + 9) % 12) + 2) / 5 + d as i64 - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn epoch_ms(y: i64, mo: u32, d: u32, h: i64, mi: i64, s: i64, ms: i64) -> Option<u64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..=60).contains(&s) {
        return None;
    }
    u64::try_from(((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s) * 1000) + ms).ok()
}

/// `2026-10-10T05:06:07.123Z` (the fraction optional) in ms since the epoch.
pub fn parse_iso(t: &str) -> Option<u64> {
    let b = t.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || !t.ends_with('Z') {
        return None;
    }
    let n = |a: usize, z: usize| t.get(a..z).filter(|x| x.bytes().all(|c| c.is_ascii_digit())).and_then(|x| x.parse::<i64>().ok());
    let ms = match &t[19..t.len() - 1] {
        "" => 0,
        f => {
            let f = f.strip_prefix('.').filter(|f| !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit()))?;
            format!("{:0<3}", &f[..f.len().min(3)]).parse().ok()?
        }
    };
    epoch_ms(n(0, 4)?, n(5, 7)? as u32, n(8, 10)? as u32, n(11, 13)?, n(14, 16)?, n(17, 19)?, ms)
}

/// An HTTP `Date` (`Thu, 15 Oct 2026 02:17:45 GMT`) in ms since the epoch.
pub fn parse_http_date(t: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let p: Vec<&str> = t.split_whitespace().collect();
    if p.len() != 6 || p[5] != "GMT" {
        return None;
    }
    let mo = MONTHS.iter().position(|m| *m == p[2])? as u32 + 1;
    let hms: Vec<i64> = p[4].split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [h, mi, s] = hms[..] else { return None };
    epoch_ms(p[3].parse().ok()?, mo, p[1].parse().ok()?, h, mi, s, 0)
}

/// A number in [0, 1), for the waits the protocol spreads at random.
pub fn random_unit() -> f64 {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("system randomness");
    u32::from_le_bytes(b) as f64 / (u32::MAX as f64 + 1.0)
}

/// A COMPUTER ID IN USE by this process, for as long as this lives: a file in
/// the temp folder named by the id's digest (never the id), held open with no
/// sharing on Windows and under `flock` on Unix, so another simfleet — the
/// same folder run twice, or a copy of it — sees the id is taken. An id is one
/// client (docs/CLIENTS.md "Identity"): a protocol 8 sync names everything its
/// id holds, and gives back whatever it does not name.
pub struct IdLock(#[allow(dead_code)] std::fs::File);

pub enum IdUse {
    Mine(IdLock),
    /// Another process holds it.
    Taken,
    /// No lock could be made here (the temp folder): used without one.
    Unknown,
}

pub fn lock_id(id: &str) -> IdUse {
    use sha2::Digest;
    let h: String = sha2::Sha256::digest(format!("simfleet-id:{id}").as_bytes()).iter().take(8).map(|x| format!("{x:02x}")).collect();
    let path = std::env::temp_dir().join(format!("simfleet-id-{h}.lock"));
    let mut open = std::fs::OpenOptions::new();
    open.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        match open.share_mode(0).open(&path) {
            Ok(f) => IdUse::Mine(IdLock(f)),
            // ERROR_SHARING_VIOLATION: another process has it open.
            Err(e) if e.raw_os_error() == Some(32) => IdUse::Taken,
            Err(_) => IdUse::Unknown,
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let Ok(f) = open.open(&path) else { return IdUse::Unknown };
        // SAFETY: a plain call on a descriptor this file owns.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            IdUse::Mine(IdLock(f))
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
            IdUse::Taken
        } else {
            IdUse::Unknown
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (open, path);
        IdUse::Unknown
    }
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
#[cfg(unix)]
pub fn utc_offset_min() -> i64 {
    // SAFETY: localtime_r fills the struct it is given.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64 / 60
    }
}
#[cfg(not(any(windows, unix)))]
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
    if let Ok(h) = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")) {
        return h;
    }
    #[cfg(unix)]
    {
        let mut b = [0u8; 256];
        // SAFETY: the buffer and its length.
        if unsafe { libc::gethostname(b.as_mut_ptr() as *mut libc::c_char, b.len()) } == 0 {
            let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            if n > 0 {
                return String::from_utf8_lossy(&b[..n]).to_string();
            }
        }
    }
    "pc".into()
}

/// The logical processor this thread runs on now.
#[cfg(windows)]
pub fn current_cpu() -> u32 {
    // SAFETY: no arguments.
    unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() }
}
#[cfg(target_os = "linux")]
pub fn current_cpu() -> u32 {
    // SAFETY: no arguments.
    unsafe { libc::sched_getcpu().max(0) as u32 }
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn current_cpu() -> u32 {
    0
}

/// CPU time this thread has been given, in µs.
#[cfg(windows)]
pub fn thread_cpu_us() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut made, mut ended, mut k, mut u) = (z, z, z, z);
    // SAFETY: plain out-parameters on this thread's pseudo-handle.
    if unsafe { GetThreadTimes(GetCurrentThread(), &mut made, &mut ended, &mut k, &mut u) } == 0 {
        return 0;
    }
    (t(k) + t(u)) / 10
}
#[cfg(unix)]
pub fn thread_cpu_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: the struct it fills.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}
#[cfg(not(any(windows, unix)))]
pub fn thread_cpu_us() -> u64 {
    0
}

/// CPU time this whole process has been given, every thread of it, in µs.
#[cfg(windows)]
pub fn process_cpu_us() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut made, mut ended, mut k, mut u) = (z, z, z, z);
    // SAFETY: plain out-parameters on this process's pseudo-handle.
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut made, &mut ended, &mut k, &mut u) } == 0 {
        return 0;
    }
    (t(k) + t(u)) / 10
}
#[cfg(unix)]
pub fn process_cpu_us() -> u64 {
    // SAFETY: the struct it fills.
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) } != 0 {
        return 0;
    }
    let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    us(r.ru_utime) + us(r.ru_stime)
}
#[cfg(not(any(windows, unix)))]
pub fn process_cpu_us() -> u64 {
    0
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

/// Linux, from /proc, in clock ticks.
#[cfg(not(windows))]
pub fn cpu_times() -> Option<(u64, u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let f: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).take(8).filter_map(|x| x.parse().ok()).collect();
    if f.len() < 5 {
        return None;
    }
    let me = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command may hold spaces; the fields after its ')' are fixed.
    let rest: Vec<&str> = me.rsplit_once(')')?.1.split_whitespace().collect();
    let (utime, stime): (u64, u64) = (rest.get(11)?.parse().ok()?, rest.get(12)?.parse().ok()?);
    Some((f.iter().sum(), f[3] + f[4], utime + stime))
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

/// Linux, from /proc.
#[cfg(not(windows))]
pub fn memory() -> Option<(u64, u64, u64)> {
    let kb = |text: &str, key: &str| -> Option<u64> {
        text.lines().find(|l| l.starts_with(key))?.split_whitespace().nth(1)?.parse::<u64>().ok().map(|k| k * 1024)
    };
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let (total, avail) = (kb(&info, "MemTotal:")?, kb(&info, "MemAvailable:")?);
    let me = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| kb(&s, "VmRSS:")).unwrap_or(0);
    Some((total - avail.min(total), total, me))
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_matches_js() {
        assert_eq!(super::iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(super::iso(1_791_633_967_123), "2026-10-10T12:06:07.123Z");
    }

    #[test]
    fn instants_read_back() {
        use super::{iso, parse_http_date, parse_iso};
        for ms in [0, 951_782_400_000, 1_791_633_967_123, 1_798_761_599_999] {
            assert_eq!(parse_iso(&iso(ms)), Some(ms));
        }
        assert_eq!(parse_iso("2026-10-15T02:17:38Z"), Some(1_792_030_658_000));
        assert_eq!(parse_iso("2026-10-15T02:17:38.8Z"), Some(1_792_030_658_800));
        assert_eq!(parse_http_date("Thu, 15 Oct 2026 02:17:38 GMT"), Some(1_792_030_658_000));
        for bad in ["", "2026-10-15", "2026-13-15T02:17:38Z", "2026-10-15T02:17:38.Z", "2026-10-15 02:17:38Z"] {
            assert_eq!(parse_iso(bad), None, "{bad}");
        }
        assert_eq!(parse_http_date("Thu, 15 Oct 2026 02:17 GMT"), None);
    }

    #[test]
    fn an_id_is_one_process_s() {
        let id = super::random_hex(12);
        let a = super::lock_id(&id);
        assert!(matches!(a, super::IdUse::Mine(_)));
        // Another open of the same file is refused (another process's would be too).
        assert!(matches!(super::lock_id(&id), super::IdUse::Taken));
        drop(a);
        assert!(matches!(super::lock_id(&id), super::IdUse::Mine(_)));
    }
}
