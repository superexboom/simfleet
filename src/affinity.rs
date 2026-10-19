// SPDX-License-Identifier: AGPL-3.0-or-later
//! WHERE THE LANES RUN, and what they actually get there.
//!
//! INTEL HYBRID CPUS (P-cores and E-cores). Left to itself, Windows puts a
//! background, idle-priority process like this one on the E-cores and keeps it
//! there: on an i5-13400F (6 P-cores with two threads each, 4 E-cores) eight
//! lanes were measured sharing the four E-cores while the P-cores idled, each
//! lane getting half a core. So simfleet places them itself, in TIERS:
//!
//! - `auto` (the default): the P-cores' first threads, then the E-cores, then
//!   the P-cores' second threads — the order a core adds the most to the
//!   last: an E-core is worth more than a P-core's second thread;
//! - `p`: the P-cores' first threads, their second threads, then the E-cores;
//! - `e`: the E-cores, then the P-cores' first threads, then their second;
//! - `windows`: nothing — the scheduler decides, as before.
//!
//! Lane k takes the tier its rank falls in, as a CPU SET
//! (`SetThreadSelectedCpuSets`; on Linux the affinity, `sched_setaffinity`) of
//! exactly that tier's logical processors, so a
//! tier never holds more lanes than it has processors and Windows moves a lane
//! only within its tier. `auto` and `p` also turn off the power throttling
//! Windows gives a background process (EcoQoS), `e` turns it on.
//!
//! THE KIND OF EACH LOGICAL PROCESSOR is its efficiency class
//! (`GetSystemCpuSetInformation`; the highest class is the P-cores — on Linux,
//! whether /sys/devices/cpu_core lists it), and its physical core is its core
//! index (its first thread sibling). Used only on an Intel CPU ("GenuineIntel")
//! that reports two classes or more; on anything else, AMD included, nothing
//! is changed.
//!
//! The threads that are no lane (a native riven gain's search starts its own
//! each round) are kept to the tiers the lanes are in (`process_default`).
//!
//! ON EVERY MACHINE each lane records the processor its last job ran on and
//! its jobs' wall time, and the console sets the CPU time the whole process
//! was given against it (fleet.rs `lane_yield`) — so lanes crowded onto a few
//! processors, or starved by other programs, show as less than one core each,
//! and a lane waiting on a thread its engine started does not.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prefer {
    Auto,
    P,
    E,
    Windows,
}

impl Prefer {
    pub fn parse(s: &str) -> Prefer {
        match s.trim().to_ascii_lowercase().as_str() {
            "p" | "pcore" | "p-core" | "performance" => Prefer::P,
            "e" | "ecore" | "e-core" | "efficiency" => Prefer::E,
            "windows" | "os" | "system" | "none" => Prefer::Windows,
            _ => Prefer::Auto,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Prefer::Auto => "auto",
            Prefer::P => "p",
            Prefer::E => "e",
            Prefer::Windows => "windows",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Prefer::Auto => "自动分配",
            Prefer::P => "优先 P 核",
            Prefer::E => "优先 E 核",
            Prefer::Windows => "交给系统调度",
        }
    }
}

/// One logical processor.
#[derive(Clone, Debug)]
pub struct Cpu {
    pub id: u32,
    pub group: u16,
    pub index: u8,
    pub core: u8,
    pub class: u8,
}

pub struct Topology {
    pub vendor: String,
    pub cpus: Vec<Cpu>,
}

/// A tier: its name and the CPU set ids in it.
type Tier = (&'static str, Vec<u32>);

impl Topology {
    pub fn read() -> Topology {
        Topology { vendor: vendor(), cpus: cpu_sets() }
    }
    fn p_class(&self) -> u8 {
        self.cpus.iter().map(|c| c.class).max().unwrap_or(0)
    }
    /// P- and E-cores told apart, on an Intel CPU — or why not.
    pub fn hybrid(&self) -> Result<(), String> {
        if self.vendor != "GenuineIntel" {
            return Err(format!("{} 不是 Intel CPU，不调整", if self.vendor.is_empty() { "这颗" } else { &self.vendor }));
        }
        let mut classes: Vec<u8> = self.cpus.iter().map(|c| c.class).collect();
        classes.sort_unstable();
        classes.dedup();
        if classes.len() < 2 {
            return Err("不是大小核架构（所有核心同一等级），不调整".into());
        }
        Ok(())
    }
    pub fn is_p(&self, c: &Cpu) -> bool {
        c.class == self.p_class()
    }
    /// Logical processors of a kind.
    pub fn count(&self, p: bool) -> usize {
        self.cpus.iter().filter(|c| self.is_p(c) == p).count()
    }
    /// Physical cores of a kind.
    pub fn cores(&self, p: bool) -> usize {
        let mut seen: Vec<(u16, u8)> = self.cpus.iter().filter(|c| self.is_p(c) == p).map(|c| (c.group, c.core)).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }
    /// A kind's logical processors split into each core's first thread and the rest.
    fn threads(&self, p: bool) -> (Vec<u32>, Vec<u32>) {
        let mut mine: Vec<&Cpu> = self.cpus.iter().filter(|c| self.is_p(c) == p).collect();
        mine.sort_by_key(|c| (c.group, c.core, c.index));
        let (mut first, mut rest) = (Vec::new(), Vec::new());
        let mut last = None;
        for c in mine {
            if last == Some((c.group, c.core)) {
                rest.push(c.id);
            } else {
                first.push(c.id);
                last = Some((c.group, c.core));
            }
        }
        (first, rest)
    }
    /// The tiers lanes fill, in order; empty for `windows`.
    pub fn tiers(&self, prefer: Prefer) -> Vec<Tier> {
        let (p1, p2) = self.threads(true);
        let (e1, e2) = self.threads(false);
        let t: Vec<Tier> = match prefer {
            Prefer::Windows => return Vec::new(),
            Prefer::Auto => vec![("P", p1), ("E", e1), ("P超线程", p2), ("E", e2)],
            Prefer::P => vec![("P", p1), ("P超线程", p2), ("E", e1), ("E", e2)],
            Prefer::E => vec![("E", e1), ("E", e2), ("P", p1), ("P超线程", p2)],
        };
        t.into_iter().filter(|(_, ids)| !ids.is_empty()).collect()
    }
    /// The tier lane `lane` is given: the one its rank falls in.
    pub fn lane_tier(tiers: &[Tier], lane: usize) -> Option<&Tier> {
        let total: usize = tiers.iter().map(|t| t.1.len()).sum();
        if total == 0 {
            return None;
        }
        let mut k = lane % total;
        for t in tiers {
            if k < t.1.len() {
                return Some(t);
            }
            k -= t.1.len();
        }
        None
    }
}

struct Plan {
    prefer: Prefer,
    tiers: Vec<Tier>,
    /// Logical processor number (group 0) → P.
    kind: Vec<Option<bool>>,
    single_group: bool,
}

static PLAN: OnceLock<Plan> = OnceLock::new();

const LANES: usize = 256;
/// Each lane's processor at its last job, plus one (0: not yet)…
static LAST: [AtomicU32; LANES] = [const { AtomicU32::new(0) }; LANES];
/// …and, over all its jobs, the CPU time it was given and their wall time (µs).
static CPU_US: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];
static WALL_US: [AtomicU64; LANES] = [const { AtomicU64::new(0) }; LANES];

/// Decide for this run. A line for the log: what was done, or why nothing was.
/// `priority` is the config's: on macOS it chooses the lanes' QoS class too.
pub fn init(prefer: Prefer, priority: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        apple::init(prefer, priority)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = priority;
        init_intel(prefer)
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn init_intel(prefer: Prefer) -> Option<String> {
    let topo = Topology::read();
    if let Err(why) = topo.hybrid() {
        return (!matches!(prefer, Prefer::Auto | Prefer::Windows)).then(|| format!("大小核亲和性：{why}"));
    }
    let mut kind = vec![None; 256];
    for c in topo.cpus.iter().filter(|c| c.group == 0) {
        kind[c.index as usize] = Some(topo.is_p(c));
    }
    let tiers = topo.tiers(prefer);
    match prefer {
        Prefer::Auto | Prefer::P => power_throttling(false),
        Prefer::E => power_throttling(true),
        Prefer::Windows => {}
    }
    let order: Vec<String> = tiers.iter().map(|(n, ids)| format!("{n}×{}", ids.len())).collect();
    let line = format!(
        "Intel 大小核：P 核 {} 个（{} 线程），E 核 {} 个（{} 线程）；{}{}",
        topo.cores(true),
        topo.count(true),
        topo.cores(false),
        topo.count(false),
        prefer.name(),
        if order.is_empty() { String::new() } else { format!("，计算线程依次放在 {}", order.join(" → ")) }
    );
    let single_group = topo.cpus.iter().all(|c| c.group == 0);
    let _ = PLAN.set(Plan { prefer, tiers, kind, single_group });
    Some(line)
}

/// At a lane thread's start: its tier's CPU set (on macOS, its QoS class).
pub fn apply_lane(lane: usize) {
    #[cfg(target_os = "macos")]
    apple::apply_lane();
    let Some(p) = PLAN.get() else { return };
    if let Some((_, ids)) = Topology::lane_tier(&p.tiers, lane) {
        set_thread_cpus(ids);
    }
}

/// THE THREADS NO LANE IS: a native riven gain's search scores each round on
/// a thread WFSim's optimizer starts (optimizer/src/evaluate.rs), the lane
/// waiting for it, and on Windows a new thread takes the process's CPU set,
/// not its creator's — so those ran anywhere, on the P-cores' second threads
/// and E-cores the budget had not reached. They, and the rest of the
/// process, are kept to the processors the first `lanes` lanes are given. (On
/// Linux a new thread takes its creator's affinity: nothing to do.)
pub fn process_default(lanes: usize) {
    static SET_FOR: AtomicU64 = AtomicU64::new(0);
    let Some(p) = PLAN.get() else { return };
    if lanes == 0 || SET_FOR.swap(lanes as u64, Ordering::Relaxed) == lanes as u64 {
        return;
    }
    let mut ids: Vec<u32> = (0..lanes).filter_map(|l| Topology::lane_tier(&p.tiers, l)).flat_map(|t| t.1.iter().copied()).collect();
    ids.sort_unstable();
    ids.dedup();
    if !ids.is_empty() {
        set_process_cpus(&ids);
    }
}

/// A lane's job measured: where it ran, the CPU time it was given, its wall time.
pub fn note_job(lane: usize, cpu_us: u64, wall_us: u64) {
    if lane < LANES {
        let cpu = crate::sys::current_cpu();
        if cpu != crate::sys::NO_CPU {
            LAST[lane].store(cpu + 1, Ordering::Relaxed);
        }
        CPU_US[lane].fetch_add(cpu_us, Ordering::Relaxed);
        WALL_US[lane].fetch_add(wall_us, Ordering::Relaxed);
    }
}

/// Over all lanes so far: CPU time given and wall time worked (µs).
pub fn lane_totals() -> (u64, u64) {
    (0..LANES).fold((0, 0), |a, l| (a.0 + CPU_US[l].load(Ordering::Relaxed), a.1 + WALL_US[l].load(Ordering::Relaxed)))
}

/// The distinct logical processors the first `n` lanes last ran on.
pub fn processors_used(n: usize) -> usize {
    let mut seen: Vec<u32> = (0..n.min(LANES)).map(|l| LAST[l].load(Ordering::Relaxed)).filter(|v| *v > 0).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

/// Where each of the first `n` lanes last ran: 'P', 'E', or nothing known.
pub fn lane_kinds(n: usize) -> Vec<Option<char>> {
    let Some(p) = PLAN.get().filter(|p| p.single_group) else { return Vec::new() };
    (0..n.min(LANES))
        .map(|l| match LAST[l].load(Ordering::Relaxed) {
            0 => None,
            v => p.kind.get((v - 1) as usize).copied().flatten().map(|is_p| if is_p { 'P' } else { 'E' }),
        })
        .collect()
}

/// The setting in effect, on an Intel hybrid (or on Apple silicon).
pub fn active() -> Option<Prefer> {
    #[cfg(target_os = "macos")]
    if let Some(p) = apple::active() {
        return Some(p);
    }
    PLAN.get().map(|p| p.prefer)
}

/// The setup's question about where the lanes go, on a machine where the
/// answer matters (an Intel hybrid, Apple silicon); None elsewhere.
pub fn core_question() -> Option<String> {
    #[cfg(target_os = "macos")]
    if let Some((p, e)) = crate::macos::perf_levels() {
        return Some(format!(
            "\n检测到 Apple 芯片（性能核 {p} 个，能效核 {e} 个）。计算线程怎么放？\n  回车 = 自动（性能核优先、能效核也用，让着你正在用的程序），p = 优先性能核（更快），e = 只用能效核（最省电、最安静，也最慢），windows = 交给系统："
        ));
    }
    let topo = Topology::read();
    topo.hybrid().is_ok().then(|| {
        format!(
            "\n检测到 Intel 大小核（P 核 {} 个、{} 线程，E 核 {} 个）。计算线程怎么放？\n  回车 = 自动（先 P 核，再 E 核，最后 P 核超线程，最快），p = 优先 P 核，e = 优先 E 核（更省电、更不打扰），windows = 交给系统：",
            topo.cores(true),
            topo.count(true),
            topo.cores(false)
        )
    })
}

/// APPLE SILICON (macOS): no thread affinity exists there; a thread's QoS
/// class steers it (macos.rs). Each lane asks for the class the setting
/// names, and the threads a lane starts (a native riven gain's scoring) take
/// its class with them:
///
/// - `auto`: `utility` — the performance cores first, the efficiency cores
///   too, yielding to what the person runs (`user_initiated` with
///   `priority = "normal"`);
/// - `p`: `user_initiated`, the performance cores' first claim;
/// - `e`: `background` — the efficiency cores alone, the least power and heat;
/// - `windows`: no class asked, the system's default.
#[cfg(target_os = "macos")]
mod apple {
    use super::Prefer;
    use crate::macos::{QOS_BACKGROUND, QOS_USER_INITIATED, QOS_UTILITY};
    use std::sync::OnceLock;

    struct Qos {
        prefer: Prefer,
        class: Option<u32>,
        levels: Option<(usize, usize)>,
    }
    static QOS: OnceLock<Qos> = OnceLock::new();

    pub fn class_of(prefer: Prefer, priority: &str) -> Option<u32> {
        match prefer {
            Prefer::Windows => None,
            Prefer::E => Some(QOS_BACKGROUND),
            Prefer::P => Some(QOS_USER_INITIATED),
            Prefer::Auto => Some(if priority == "normal" { QOS_USER_INITIATED } else { QOS_UTILITY }),
        }
    }

    pub fn class_name(class: Option<u32>) -> &'static str {
        match class {
            Some(QOS_USER_INITIATED) => "user_initiated（性能核优先）",
            Some(QOS_UTILITY) => "utility（性能核优先，能效核也用，让着前台程序）",
            Some(QOS_BACKGROUND) => "background（只用能效核）",
            _ => "不指定（系统默认）",
        }
    }

    pub fn init(prefer: Prefer, priority: &str) -> Option<String> {
        let levels = crate::macos::perf_levels();
        let class = class_of(prefer, priority);
        let _ = QOS.set(Qos { prefer, class, levels });
        let chip = crate::macos::chip();
        let chip = if chip.is_empty() { "Apple 芯片".to_string() } else { chip };
        Some(match levels {
            Some((p, e)) => format!("{chip}：性能核 {p} 个，能效核 {e} 个；{}，计算线程 QoS {}", prefer.name(), class_name(class)),
            None => format!("{chip}：系统没有分出性能核和能效核；计算线程 QoS {}", class_name(class)),
        })
    }

    pub fn apply_lane() {
        if let Some(c) = QOS.get().and_then(|q| q.class) {
            crate::macos::set_thread_qos(c);
        }
    }

    /// The setting, where Apple silicon's two kinds are there and a class asked.
    pub fn active() -> Option<Prefer> {
        QOS.get().filter(|q| q.levels.is_some() && q.class.is_some()).map(|q| q.prefer)
    }

    pub fn report(prefer: Prefer, priority: &str) -> String {
        let chip = crate::macos::chip();
        let mut s = format!("芯片：{}\n逻辑处理器：{} 个\n", if chip.is_empty() { "?" } else { &chip }, crate::sys::cores());
        match crate::macos::perf_levels() {
            Some((p, e)) => s += &format!("性能核 {p} 个，能效核 {e} 个（hw.perflevel0 / hw.perflevel1）\n"),
            None => s += "系统没有分出性能核和能效核（hw.nperflevels < 2）\n",
        }
        s += &format!(
            "当前设置 core_type = {}（{}），priority = {priority}：计算线程 QoS {}\nmacOS 不能把线程绑到指定核心，只能用 QoS 引导；background 只在能效核上跑，其余优先性能核。\n",
            prefer.key(),
            prefer.name(),
            class_name(class_of(prefer, priority))
        );
        s
    }
}

// ---- the platform ------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
fn vendor() -> String {
    // CPUID leaf 0: the vendor string, on every x86-64 CPU.
    let r = std::arch::x86_64::__cpuid(0);
    let b: Vec<u8> = [r.ebx, r.edx, r.ecx].iter().flat_map(|x| x.to_le_bytes()).collect();
    String::from_utf8_lossy(&b).to_string()
}

#[cfg(not(target_arch = "x86_64"))]
fn vendor() -> String {
    String::new()
}

#[cfg(windows)]
fn cpu_sets() -> Vec<Cpu> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    let mut len = 0u32;
    // SAFETY: asked for the size first, then filled; entries walked by their own `Size`.
    unsafe {
        GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut len, std::ptr::null_mut(), 0);
        if len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let p = buf.as_mut_ptr() as *mut u8;
        if GetSystemCpuSetInformation(p as *mut SYSTEM_CPU_SET_INFORMATION, len, &mut len, std::ptr::null_mut(), 0) == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut off = 0usize;
        while off + std::mem::size_of::<SYSTEM_CPU_SET_INFORMATION>() <= len as usize {
            let e = &*(p.add(off) as *const SYSTEM_CPU_SET_INFORMATION);
            if e.Size == 0 {
                break;
            }
            // Type 0 is CpuSetInformation.
            if e.Type == 0 {
                let c = e.Anonymous.CpuSet;
                out.push(Cpu { id: c.Id, group: c.Group, index: c.LogicalProcessorIndex, core: c.CoreIndex, class: c.EfficiencyClass });
            }
            off += e.Size as usize;
        }
        out
    }
}

/// LINUX: the kernel lists a hybrid's kinds in /sys/devices/cpu_core/cpus (P)
/// and /sys/devices/cpu_atom/cpus (E); a core is its threads' first sibling.
/// The "id" is the logical processor itself, as sched_setaffinity takes it.
#[cfg(target_os = "linux")]
fn cpu_sets() -> Vec<Cpu> {
    let read = |p: &str| std::fs::read_to_string(p).ok();
    let list = |text: &str| -> Vec<u32> {
        let mut out = Vec::new();
        for part in text.trim().split(',').filter(|p| !p.is_empty()) {
            let mut ends = part.split('-').filter_map(|x| x.trim().parse::<u32>().ok());
            if let Some(a) = ends.next() {
                out.extend(a..=ends.next().unwrap_or(a));
            }
        }
        out
    };
    let p_cores = read("/sys/devices/cpu_core/cpus").map(|t| list(&t)).unwrap_or_default();
    let Some(online) = read("/sys/devices/system/cpu/online").map(|t| list(&t)) else { return Vec::new() };
    online
        .into_iter()
        .filter(|&n| n < 256)
        .map(|n| {
            let sib = read(&format!("/sys/devices/system/cpu/cpu{n}/topology/thread_siblings_list"))
                .and_then(|t| list(&t).into_iter().min())
                .unwrap_or(n);
            Cpu { id: n, group: 0, index: n as u8, core: sib.min(255) as u8, class: u8::from(p_cores.contains(&n)) }
        })
        .collect()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn cpu_sets() -> Vec<Cpu> {
    Vec::new()
}

#[cfg(windows)]
fn set_thread_cpus(ids: &[u32]) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
    if !ids.is_empty() {
        // SAFETY: ids from GetSystemCpuSetInformation; the pseudo-handle is this thread.
        unsafe {
            SetThreadSelectedCpuSets(GetCurrentThread(), ids.as_ptr(), ids.len() as u32);
        }
    }
}

#[cfg(target_os = "linux")]
fn set_thread_cpus(ids: &[u32]) {
    // SAFETY: a zeroed cpu_set_t, filled with the ids, for this thread (0).
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &i in ids {
            libc::CPU_SET(i as usize, &mut set);
        }
        if !ids.is_empty() {
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn set_thread_cpus(_ids: &[u32]) {}

#[cfg(windows)]
fn set_process_cpus(ids: &[u32]) {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessDefaultCpuSets};
    // SAFETY: ids from GetSystemCpuSetInformation; the pseudo-handle is this process.
    unsafe {
        SetProcessDefaultCpuSets(GetCurrentProcess(), ids.as_ptr(), ids.len() as u32);
    }
}

#[cfg(not(windows))]
fn set_process_cpus(_ids: &[u32]) {}

/// EcoQoS for the whole process: on (`slow`) or off.
#[cfg(windows)]
fn power_throttling(slow: bool) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    };
    let s = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: if slow { PROCESS_POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
    };
    // SAFETY: a plain struct of the size given.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &s as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn power_throttling(_slow: bool) {}

/// `simfleet cpu`: what was detected and where each lane would go.
pub fn report(prefer: Prefer, lanes: usize, priority: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        let _ = lanes;
        apple::report(prefer, priority)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = priority;
        report_intel(prefer, lanes)
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn report_intel(prefer: Prefer, lanes: usize) -> String {
    let t = Topology::read();
    let mut s = format!(
        "CPU 厂商：{}\n逻辑处理器：{} 个\n",
        if t.vendor.is_empty() { "?" } else { &t.vendor },
        if t.cpus.is_empty() { crate::sys::cores() } else { t.cpus.len() }
    );
    let mut by_class: std::collections::BTreeMap<u8, Vec<String>> = Default::default();
    for c in &t.cpus {
        by_class.entry(c.class).or_default().push(format!("{}(核{})", c.index, c.core));
    }
    for (class, list) in by_class.iter().rev() {
        s += &format!("  效率等级 {class}：{} 个 — {}\n", list.len(), list.join(" "));
    }
    match t.hybrid() {
        Err(why) => s += &format!("{why}。core_type 设置不会生效。\n"),
        Ok(()) => {
            s += &format!(
                "判定：Intel 大小核，P 核 {} 个（{} 线程），E 核 {} 个（{} 线程）。\n",
                t.cores(true),
                t.count(true),
                t.cores(false),
                t.count(false)
            );
            s += &format!("当前设置 core_type = {}（{}）：", prefer.key(), prefer.name());
            let tiers = t.tiers(prefer);
            if tiers.is_empty() {
                s += "不绑定，由系统自行调度（Windows 上空闲优先级的后台进程通常会被挤在 E 核上）。\n";
            } else {
                let order: Vec<String> = tiers.iter().map(|(n, ids)| format!("{n}×{}", ids.len())).collect();
                s += &format!("计算线程依次放在 {}\n", order.join(" → "));
                let kinds: Vec<&str> = (0..lanes).map(|l| Topology::lane_tier(&tiers, l).map_or("?", |t| t.0)).collect();
                s += &format!("预算 {lanes} 核时：{}\n", kinds.join(" "));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (class, core) per logical processor.
    fn topo(cpus: &[(u8, u8)]) -> Topology {
        Topology {
            vendor: "GenuineIntel".into(),
            cpus: cpus.iter().enumerate().map(|(i, &(class, core))| Cpu { id: 256 + i as u32, group: 0, index: i as u8, core, class }).collect(),
        }
    }

    /// An i5-13400F: 6 P-cores with two threads each, 4 E-cores.
    fn i5_13400f() -> Topology {
        let mut v: Vec<(u8, u8)> = (0..12).map(|i| (1, i / 2)).collect();
        v.extend((0..4).map(|i| (0, 6 + i)));
        topo(&v)
    }

    fn kinds(t: &Topology, prefer: Prefer, n: usize) -> Vec<&'static str> {
        let tiers = t.tiers(prefer);
        (0..n).map(|l| Topology::lane_tier(&tiers, l).unwrap().0).collect()
    }

    #[test]
    fn auto_fills_p_cores_then_e_cores_then_second_threads() {
        let t = i5_13400f();
        assert!(t.hybrid().is_ok());
        assert_eq!((t.cores(true), t.count(true), t.cores(false), t.count(false)), (6, 12, 4, 4));
        let k = kinds(&t, Prefer::Auto, 16);
        assert_eq!(k[..6], ["P"; 6]);
        assert_eq!(k[6..10], ["E"; 4]);
        assert_eq!(k[10..], ["P超线程"; 6]);
        // Each core's first thread is in the P tier, its second in the other.
        let tiers = t.tiers(Prefer::Auto);
        assert_eq!(tiers[0].1, vec![256, 258, 260, 262, 264, 266]);
    }

    #[test]
    fn p_and_e_put_their_kind_first() {
        let t = i5_13400f();
        let p = kinds(&t, Prefer::P, 16);
        assert_eq!(p[..12].iter().filter(|k| k.starts_with('P')).count(), 12);
        assert_eq!(p[12..], ["E"; 4]);
        let e = kinds(&t, Prefer::E, 6);
        assert_eq!(e, ["E", "E", "E", "E", "P", "P"]);
        assert!(t.tiers(Prefer::Windows).is_empty());
    }

    #[test]
    fn only_intel_hybrids_count() {
        assert!(topo(&[(1, 0), (1, 0), (1, 1), (1, 1)]).hybrid().is_err());
        let mut amd = topo(&[(1, 0), (1, 1), (0, 2), (0, 3)]);
        amd.vendor = "AuthenticAMD".into();
        assert!(amd.hybrid().is_err());
    }

    #[test]
    fn the_setting_reads_loosely() {
        assert_eq!(Prefer::parse(" P "), Prefer::P);
        assert_eq!(Prefer::parse("efficiency"), Prefer::E);
        assert_eq!(Prefer::parse("Windows"), Prefer::Windows);
        assert_eq!(Prefer::parse(""), Prefer::Auto);
    }

    /// Windows keeps a thread inside the CPU set it was given (any CPU):
    /// `cargo test cpu_sets_hold -- --ignored --nocapture`.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn cpu_sets_hold() {
        let t = Topology::read();
        let want: Vec<&Cpu> = t.cpus.iter().filter(|c| c.group == 0).rev().take(2).collect();
        let ids: Vec<u32> = want.iter().map(|c| c.id).collect();
        let allowed: Vec<u32> = want.iter().map(|c| c.index as u32).collect();
        let seen = std::thread::spawn(move || {
            set_thread_cpus(&ids);
            let mut seen = std::collections::BTreeSet::new();
            let t0 = std::time::Instant::now();
            while t0.elapsed() < std::time::Duration::from_millis(1500) {
                seen.insert(crate::sys::current_cpu());
            }
            seen
        })
        .join()
        .unwrap();
        println!("allowed {allowed:?}, ran on {seen:?}");
        assert!(seen.iter().all(|c| allowed.contains(c)));
    }

    /// A thread a lane starts takes the PROCESS's CPU set, not the lane's —
    /// so the process default is what holds a native riven gain's helpers:
    /// `cargo test process_cpus_hold -- --ignored --nocapture`.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn process_cpus_hold() {
        let t = Topology::read();
        let mine: Vec<&Cpu> = t.cpus.iter().filter(|c| c.group == 0).collect();
        assert!(mine.len() >= 4, "needs four logical processors");
        let (lane, rest) = (&mine[mine.len() - 2..], &mine[..2]);
        let ids = |c: &[&Cpu]| -> Vec<u32> { c.iter().map(|c| c.id).collect() };
        let (lane_ids, rest_ids) = (ids(lane), ids(rest));
        let allowed: Vec<u32> = rest.iter().map(|c| c.index as u32).collect();
        set_process_cpus(&rest_ids);
        let seen = std::thread::spawn(move || {
            set_thread_cpus(&lane_ids);
            // …and the thread it starts.
            std::thread::spawn(|| {
                let mut seen = std::collections::BTreeSet::new();
                let t0 = std::time::Instant::now();
                while t0.elapsed() < std::time::Duration::from_millis(1500) {
                    seen.insert(crate::sys::current_cpu());
                }
                seen
            })
            .join()
            .unwrap()
        })
        .join()
        .unwrap();
        // SAFETY: no CPU sets clears the process default.
        unsafe {
            windows_sys::Win32::System::Threading::SetProcessDefaultCpuSets(windows_sys::Win32::System::Threading::GetCurrentProcess(), std::ptr::null(), 0);
        }
        println!("process default {allowed:?}, the lane's helper ran on {seen:?}");
        assert!(seen.iter().all(|c| allowed.contains(c)));
    }
}
