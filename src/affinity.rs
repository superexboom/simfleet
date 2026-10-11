// SPDX-License-Identifier: AGPL-3.0-or-later
//! INTEL HYBRID CPUS — P-cores and E-cores — and which kind the lanes run on.
//!
//! Left alone (`auto`), Windows decides, and it steers a background,
//! idle-priority process like this one towards the E-cores. `p` fills the
//! P-cores' logical processors first and spills over to the E-cores, `e` the
//! reverse. Each lane thread is given its kind's CPU SET
//! (`SetThreadSelectedCpuSets`): the scheduler keeps the thread inside it and
//! moves it freely within it, so the choice holds without pinning a lane to
//! one processor. `p` also asks Windows not to slow the process down as a
//! background one (power throttling / EcoQoS off), `e` asks it to.
//!
//! THE KIND OF EACH LOGICAL PROCESSOR is its efficiency class
//! (`GetSystemCpuSetInformation`; the highest class is the P-cores). It is
//! used only on an Intel CPU ("GenuineIntel") that reports two classes or
//! more; on anything else, AMD included, nothing is changed.
//!
//! Where each lane ACTUALLY ran is recorded after every job, so the console
//! shows whether Windows kept to it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prefer {
    Auto,
    P,
    E,
}

impl Prefer {
    pub fn parse(s: &str) -> Prefer {
        match s.trim().to_ascii_lowercase().as_str() {
            "p" | "pcore" | "p-core" | "performance" => Prefer::P,
            "e" | "ecore" | "e-core" | "efficiency" => Prefer::E,
            _ => Prefer::Auto,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Prefer::Auto => "交给 Windows",
            Prefer::P => "优先 P 核",
            Prefer::E => "优先 E 核",
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
    pub fn count(&self, p: bool) -> usize {
        self.cpus.iter().filter(|c| self.is_p(c) == p).count()
    }
    fn ids(&self, p: bool) -> Vec<u32> {
        self.cpus.iter().filter(|c| self.is_p(c) == p).map(|c| c.id).collect()
    }
    /// The kind each lane is given: lanes up to the preferred kind's logical
    /// processors take it, the rest the other.
    pub fn lane_is_p(&self, prefer: Prefer, lane: usize) -> Option<bool> {
        let first = match prefer {
            Prefer::Auto => return None,
            Prefer::P => true,
            Prefer::E => false,
        };
        Some(if lane < self.count(first) { first } else { !first })
    }
}

struct Plan {
    topo: Topology,
    prefer: Prefer,
    p_ids: Vec<u32>,
    e_ids: Vec<u32>,
    /// Logical processor number (group 0) → P.
    kind: HashMap<u8, bool>,
    single_group: bool,
}

static PLAN: OnceLock<Plan> = OnceLock::new();

const LANES: usize = 256;
/// Each lane's processor at its last job, plus one (0: not yet).
static LAST: [AtomicU32; LANES] = [const { AtomicU32::new(0) }; LANES];

/// Decide for this run. A line for the log: what was done, or why nothing was.
pub fn init(prefer: Prefer) -> Option<String> {
    let topo = Topology::read();
    if let Err(why) = topo.hybrid() {
        return (prefer != Prefer::Auto).then(|| format!("大小核亲和性：{why}"));
    }
    let (np, ne) = (topo.count(true), topo.count(false));
    let kind = topo.cpus.iter().filter(|c| c.group == 0).map(|c| (c.index, topo.is_p(c))).collect();
    let single_group = topo.cpus.iter().all(|c| c.group == 0);
    let plan = Plan { p_ids: topo.ids(true), e_ids: topo.ids(false), topo, prefer, kind, single_group };
    match prefer {
        Prefer::P => power_throttling(false),
        Prefer::E => power_throttling(true),
        Prefer::Auto => {}
    }
    let _ = PLAN.set(plan);
    Some(format!("Intel 大小核：P 核 {np} 个线程，E 核 {ne} 个；计算线程{}", prefer.name()))
}

/// At a lane thread's start: its kind's CPU set.
pub fn apply_lane(lane: usize) {
    let Some(p) = PLAN.get() else { return };
    let Some(is_p) = p.topo.lane_is_p(p.prefer, lane) else { return };
    set_thread_cpus(if is_p { &p.p_ids } else { &p.e_ids });
}

/// After a lane's job: where it ran.
pub fn note_cpu(lane: usize) {
    if lane < LANES && PLAN.get().is_some() {
        LAST[lane].store(current_cpu() + 1, Ordering::Relaxed);
    }
}

/// Where each of the first `n` lanes last ran: 'P', 'E', or nothing known.
pub fn lane_kinds(n: usize) -> Vec<Option<char>> {
    let Some(p) = PLAN.get().filter(|p| p.single_group) else { return Vec::new() };
    (0..n.min(LANES))
        .map(|l| match LAST[l].load(Ordering::Relaxed) {
            0 => None,
            v => p.kind.get(&((v - 1) as u8)).map(|&is_p| if is_p { 'P' } else { 'E' }),
        })
        .collect()
}

/// The setting in effect, for the console: "优先 P 核", or nothing when not hybrid.
pub fn active() -> Option<Prefer> {
    PLAN.get().map(|p| p.prefer)
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
fn set_thread_cpus(_ids: &[u32]) {}

#[cfg(windows)]
fn current_cpu() -> u32 {
    // SAFETY: no arguments.
    unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() }
}

#[cfg(not(windows))]
fn current_cpu() -> u32 {
    0
}

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

/// `simfleet cpu`: what was detected and what each lane would be given.
pub fn report(prefer: Prefer, lanes: usize) -> String {
    let t = Topology::read();
    let mut s = format!("CPU 厂商：{}\n逻辑处理器：{} 个\n", if t.vendor.is_empty() { "?" } else { &t.vendor }, t.cpus.len());
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
            s += &format!("判定：Intel 大小核，P 核（等级最高的）{} 个线程，E 核 {} 个。\n", t.count(true), t.count(false));
            s += &format!("当前设置 core_type = {}：", prefer.name());
            if prefer == Prefer::Auto {
                s += "不绑定，Windows 自行调度（空闲优先级的后台进程通常会被放到 E 核）。\n";
            } else {
                let kinds: Vec<&str> = (0..lanes).map(|l| if t.lane_is_p(prefer, l) == Some(true) { "P" } else { "E" }).collect();
                s += &format!("前 {lanes} 个计算线程依次放在 {}\n", kinds.join(" "));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topo(classes: &[u8]) -> Topology {
        Topology {
            vendor: "GenuineIntel".into(),
            cpus: classes.iter().enumerate().map(|(i, &class)| Cpu { id: 256 + i as u32, group: 0, index: i as u8, core: i as u8, class }).collect(),
        }
    }

    #[test]
    fn lanes_fill_the_preferred_kind_first() {
        // 4 P threads, 2 E.
        let t = topo(&[1, 1, 1, 1, 0, 0]);
        assert!(t.hybrid().is_ok());
        let p: Vec<_> = (0..6).map(|l| t.lane_is_p(Prefer::P, l).unwrap()).collect();
        assert_eq!(p, [true, true, true, true, false, false]);
        let e: Vec<_> = (0..6).map(|l| t.lane_is_p(Prefer::E, l).unwrap()).collect();
        assert_eq!(e, [false, false, true, true, true, true]);
        assert_eq!(t.lane_is_p(Prefer::Auto, 0), None);
    }

    #[test]
    fn only_intel_hybrids_count() {
        assert!(topo(&[1, 1, 1, 1]).hybrid().is_err());
        let mut amd = topo(&[1, 1, 0, 0]);
        amd.vendor = "AuthenticAMD".into();
        assert!(amd.hybrid().is_err());
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
                seen.insert(current_cpu());
            }
            seen
        })
        .join()
        .unwrap();
        println!("allowed {allowed:?}, ran on {seen:?}");
        assert!(seen.iter().all(|c| allowed.contains(c)));
    }

    #[test]
    fn the_setting_reads_loosely() {
        assert_eq!(Prefer::parse(" P "), Prefer::P);
        assert_eq!(Prefer::parse("efficiency"), Prefer::E);
        assert_eq!(Prefer::parse(""), Prefer::Auto);
    }
}
