// SPDX-License-Identifier: AGPL-3.0-or-later
//! Stable compute placement by NUMA node and last-level cache. First touch
//! occurs on the lane, before allocating its engine or row preparation.
//! NUMA nodes and processor groups are different; every CPU retains both.
use crate::affinity::{Cpu, Prefer, Topology};
use crate::config::Config;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::ops::Range;

struct Plan {
    cpus: Vec<Cpu>,
    mode: String,
    slots: Vec<Range<usize>>,
}
static PLAN: OnceLock<Option<Plan>> = OnceLock::new();

fn plan(t: &Topology, cfg: &Config) -> Result<Option<Plan>> {
    if !["auto", "system", "numa", "cache"].contains(&cfg.placement.as_str()) {
        bail!("placement 必须是 auto、system、numa 或 cache");
    }
    if cfg.placement == "system" {
        if !cfg.numa_nodes.is_empty() || cfg.cpu_offset != 0 {
            bail!("placement=system 不能同时指定 numa_nodes 或 cpu_offset");
        }
        return Ok(None);
    }
    if cfg.slot_threads == 0 && cfg.placement == "auto" && Prefer::parse(&cfg.core_type) == Prefer::Windows && cfg.numa_nodes.is_empty() && cfg.cpu_offset == 0 {
        return Ok(None);
    }
    let all_nodes: BTreeSet<u32> = t.cpus.iter().map(|c| c.numa).collect();
    for n in &cfg.numa_nodes {
        if !all_nodes.contains(n) {
            bail!("NUMA 节点 {n} 不存在或不可用");
        }
    }
    let mut cpus: Vec<Cpu> = t.cpus.iter().filter(|c| cfg.numa_nodes.is_empty() || cfg.numa_nodes.contains(&c.numa)).cloned().collect();
    if cpus.is_empty() {
        if !cfg.numa_nodes.is_empty() || cfg.cpu_offset != 0 || cfg.placement != "auto" {
            bail!("系统没有提供可用的 CPU 拓扑，不能应用指定的 placement/numa_nodes/cpu_offset");
        }
        return Ok(None);
    }
    let nodes: BTreeSet<u32> = cpus.iter().map(|c| c.numa).collect();
    let caches: BTreeSet<_> = cpus.iter().map(|c| (c.group, c.llc)).collect();
    let groups: BTreeSet<_> = cpus.iter().map(|c| c.group).collect();
    let hybrid = t.hybrid().is_ok();
    if cfg.slot_threads == 0 && cfg.placement == "auto" && nodes.len() == 1 && caches.len() == 1 && groups.len() == 1 && !hybrid && cfg.numa_nodes.is_empty() && cfg.cpu_offset == 0 {
        return Ok(None);
    }
    let mode = if cfg.placement == "auto" {
        if nodes.len() > 1 {
            "numa"
        } else {
            "cache"
        }
    } else {
        &cfg.placement
    };
    let prefer = Prefer::parse(&cfg.core_type);
    let mut siblings: BTreeMap<(u16, u32), Vec<u32>> = BTreeMap::new();
    for c in &cpus {
        siblings.entry((c.group, c.core)).or_default().push(c.index);
    }
    for s in siblings.values_mut() {
        s.sort_unstable();
    }
    let mut by_tier_node: BTreeMap<(usize, u32), Vec<Cpu>> = BTreeMap::new();
    for c in cpus.drain(..) {
        let sibling = siblings[&(c.group, c.core)].iter().position(|&i| i == c.index).unwrap();
        let tier = if hybrid {
            match (prefer, t.is_p(&c), sibling == 0) {
                (Prefer::E, false, _) => sibling,
                (Prefer::E, true, _) => 2 + sibling,
                (Prefer::P, true, _) => sibling,
                (Prefer::P, false, _) => 2 + sibling,
                (_, true, true) => 0,
                (_, false, true) => 1,
                (_, true, false) => 2 + sibling - 1,
                (_, false, false) => 4 + sibling - 1,
            }
        } else {
            sibling
        };
        by_tier_node.entry((tier, c.numa)).or_default().push(c);
    }
    for list in by_tier_node.values_mut() {
        // Large LLC first for an asymmetric cache chip; never select SMT on
        // that CCD before the remaining physical cores on the other CCD.
        list.sort_by_key(|c| (std::cmp::Reverse(c.cache_bytes), c.group, c.llc, c.core, c.index));
    }
    let tiers: BTreeSet<usize> = by_tier_node.keys().map(|(tier, _)| *tier).collect();
    for tier in tiers {
        let lists: Vec<&Vec<Cpu>> = nodes.iter().filter_map(|n| by_tier_node.get(&(tier, *n))).collect();
        for i in 0..lists.iter().map(|l| l.len()).max().unwrap_or(0) {
            for list in &lists {
                if let Some(c) = list.get(i) {
                    cpus.push(c.clone());
                }
            }
        }
    }
    let offset = cfg.cpu_offset % cpus.len();
    cpus.rotate_left(offset);
    let (cpus, slots) = if cfg.slot_threads > 0 {
        partition_cpus(cpus, cfg.slot_threads, mode)
    } else { (cpus, Vec::new()) };
    Ok(Some(Plan { cpus, mode: mode.into(), slots }))
}

/// Pack physical-core bundles within a locality domain. Distribute each
/// class across slots before the next class; SMT siblings travel together
/// unless an odd slot limit leaves only single-thread holes.
fn partition_cpus(cpus: Vec<Cpu>, limit: usize, mode: &str) -> (Vec<Cpu>, Vec<Range<usize>>) {
    let mut domains: Vec<((u32,u64), Vec<Vec<Cpu>>)> = Vec::new();
    for cpu in cpus {
        let domain=(cpu.numa,if mode=="numa" {0} else {(u64::from(cpu.group)<<32)|u64::from(cpu.llc)});
        let d=domains.iter().position(|(key,_)|*key==domain).unwrap_or_else(||{domains.push((domain,Vec::new()));domains.len()-1});
        let cores=&mut domains[d].1;
        if let Some(core)=cores.iter_mut().find(|v|v[0].group==cpu.group && v[0].core==cpu.core) {core.push(cpu);}
        else {cores.push(vec![cpu]);}
    }
    let (mut ordered,mut slots)=(Vec::new(),Vec::new());
    for (_, mut cores) in domains {
        for core in &mut cores {core.sort_by_key(|c|c.index);}
        let total=cores.iter().map(Vec::len).sum::<usize>();
        let n=total.div_ceil(limit.max(1));
        let capacities:Vec<_>=(0..n).map(|i|limit.min(total-i*limit)).collect();
        let mut banks:Vec<Vec<Cpu>>=vec![Vec::new();n];
        let mut singles=Vec::new();
        for core in cores {
            let class=core[0].class;
            if let Some(i)=(0..n).filter(|i|banks[*i].len()+core.len()<=capacities[*i])
                .min_by_key(|i|(banks[*i].iter().filter(|c|c.class==class).count()*4096/capacities[*i],banks[*i].len()*4096/capacities[*i],*i)) {
                banks[i].extend(core);
            } else {singles.extend(core);}
        }
        for cpu in singles {
            let i=(0..n).find(|i|banks[*i].len()<capacities[*i]).expect("partition capacity");
            banks[i].push(cpu);
        }
        for bank in banks.into_iter().filter(|b|!b.is_empty()) {
            let start=ordered.len();ordered.extend(bank);slots.push(start..ordered.len());
        }
    }
    (ordered,slots)
}

/// Groups beyond the number of physical partitions share a partition. The
/// dispatcher rotates among them; no group can exceed its partition's width.
#[derive(Clone, Debug)]
pub struct Slots { ranges: Vec<Range<usize>>, shared: bool }
impl Slots {
    fn new(lanes: usize, limit: usize, ranges: &[Range<usize>]) -> Self {
        let shared=limit==0;
        let ranges=if shared {vec![0..lanes]} else if !ranges.is_empty() {
            ranges.iter().filter(|r|r.start<lanes).map(|r|r.start..r.end.min(lanes)).collect()
        } else {(0..lanes).step_by(limit.max(1)).map(|start|start..(start+limit).min(lanes)).collect()};
        Self {ranges,shared}
    }
    pub fn count(&self)->usize {self.ranges.len().max(1)}
    pub fn range(&self,group:usize)->Range<usize> {
        self.ranges.get(group%self.count()).cloned().unwrap_or(0..0)
    }
    pub fn allows(&self,group:usize,lane:usize)->bool {self.range(group).contains(&lane)}
    pub fn width(&self,group:usize)->usize {self.range(group).len()}
    pub fn bank(&self,lane:usize)->usize {self.ranges.iter().position(|r|r.contains(&lane)).unwrap_or(0)}
    pub fn partitioned(&self)->bool {!self.shared}
}
pub fn slots(lanes:usize,limit:usize)->Slots {
    Slots::new(lanes,limit,PLAN.get().and_then(Option::as_ref).map_or(&[],|p|p.slots.as_slice()))
}

pub fn init(cfg: &Config) -> Result<Option<String>> {
    let t = Topology::read();
    let p = plan(&t, cfg)?;
    if !cfg.numa_nodes.is_empty() {
        if let Some(p) = &p {
            let ids: Vec<u32> = p.cpus.iter().map(|c| c.id).collect();
            crate::affinity::set_process_cpus(&ids);
            #[cfg(target_os = "linux")]
            crate::affinity::set_thread_cpus(&ids);
        }
    }
    let msg = p.as_ref().map(|p| {
        format!(
            "{} 调度：{} 个可用线程，{} 个 NUMA 节点，{} 组末级缓存；{}",
            p.mode,
            p.cpus.len(),
            p.cpus.iter().map(|c| c.numa).collect::<BTreeSet<_>>().len(),
            p.cpus.iter().map(|c| (c.group, c.llc)).collect::<BTreeSet<_>>().len(),
            if cfg.slot_threads>0 {format!("每路最多 {} 线程，物理核与 SMT 成组，共 {} 个分区",cfg.slot_threads,p.slots.len())}
            else {"线程先占物理核心，再用 SMT".into()}
        )
    });
    let _ = PLAN.set(p);
    Ok(msg)
}
pub fn validate(cfg: &Config) -> Result<()> { plan(&Topology::read(),cfg).map(|_|()) }
pub fn capacity() -> Option<usize> {
    PLAN.get()?.as_ref().map(|p| p.cpus.len())
}
pub fn active() -> bool {
    PLAN.get().is_some_and(Option::is_some)
}
pub fn process_default(lanes: usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static LAST: AtomicUsize = AtomicUsize::new(0);
    let Some(p) = PLAN.get().and_then(Option::as_ref) else { return };
    let lanes = lanes.min(p.cpus.len());
    if lanes == 0 || LAST.swap(lanes, Ordering::Relaxed) == lanes {
        return;
    }
    // Older optimizer libraries can still create helper threads. Their
    // default set must honor the same partition and current lane budget.
    let ids: Vec<u32> = p.cpus.iter().take(lanes).map(|c| c.id).collect();
    crate::affinity::set_process_cpus(&ids);
}
pub fn domain(lane: usize) -> Option<(u32, u64)> {
    let p = PLAN.get()?.as_ref()?;
    let c = p.cpus.get(lane)?;
    Some((c.numa, if p.mode == "numa" { 0 } else { (u64::from(c.group) << 32) | u64::from(c.llc) }))
}
pub fn same_domain(a: usize, b: usize) -> bool {
    domain(a) == domain(b)
}
pub fn domain_lanes(lane: usize, budget: usize) -> usize {
    if !active() {
        return budget;
    }
    (0..budget).filter(|&l| same_domain(l, lane)).count().max(1)
}
pub fn apply_lane(lane: usize) -> bool {
    let Some(p) = PLAN.get().and_then(Option::as_ref) else { return false };
    let Some(c) = p.cpus.get(lane) else { return false };
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadGroupAffinity};
        let affinity = GROUP_AFFINITY { Mask: 1usize << c.index, Group: c.group, Reserved: [0; 3] };
        if SetThreadGroupAffinity(GetCurrentThread(), &affinity, std::ptr::null_mut()) == 0 {
            return false;
        }
    }
    crate::affinity::set_thread_cpus(&[c.id]);
    true
}

pub fn report(cfg: &Config) -> Result<String> {
    let t = Topology::read();
    let mut by: BTreeMap<(u32, u16, u32), (usize, u64)> = BTreeMap::new();
    for c in &t.cpus {
        let v = by.entry((c.numa, c.group, c.llc)).or_default();
        v.0 += 1;
        v.1 = v.1.max(c.cache_bytes);
    }
    let mut s = String::from("NUMA / 缓存拓扑：\n");
    for ((node, group, llc), (n, bytes)) in by {
        s += &format!("  节点 {node}，处理器组 {group}，缓存 {llc}：{n} 线程，{} MiB\n", bytes / (1 << 20));
    }
    match plan(&t, cfg)? {
        None => s += "本机采用系统调度。\n",
        Some(p) => {
            s += &format!("placement={}，cpu_offset={}，最多 {} 线程\n", p.mode, cfg.cpu_offset, p.cpus.len());
            s += "计算线程顺序：";
            for c in p.cpus.iter().take(256) {
                s += &format!(" N{}:G{}:{}", c.numa, c.group, c.index);
            }
            s.push('\n');
            for (i,range) in p.slots.iter().enumerate() {
                let members=&p.cpus[range.clone()];
                let cores=members.iter().map(|c|(c.group,c.core)).collect::<BTreeSet<_>>().len();
                s+=&format!("  分区 {}：{} 线程 / {} 物理核，CPU {}\n",i+1,members.len(),cores,
                    members.iter().map(|c|format!("G{}:{}",c.group,c.index)).collect::<Vec<_>>().join(","));
            }
        }
    }
    Ok(s)
}

#[cfg(windows)]
pub fn cache_sizes(cpus: &mut [Cpu]) {
    use windows_sys::Win32::System::SystemInformation::*;
    unsafe {
        let mut len = 0;
        GetLogicalProcessorInformationEx(RelationCache, std::ptr::null_mut(), &mut len);
        if len == 0 {
            return;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ptr = buf.as_mut_ptr().cast::<u8>();
        if GetLogicalProcessorInformationEx(RelationCache, ptr.cast(), &mut len) == 0 {
            return;
        }
        let mut off = 0;
        let minimum = std::mem::offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous) + std::mem::size_of::<CACHE_RELATIONSHIP>();
        while off + minimum <= len as usize {
            let size = ptr.add(off + 4).cast::<u32>().read_unaligned() as usize;
            if size < minimum || off + size > len as usize {
                break;
            }
            let cache_offset = std::mem::offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
            let cache = ptr.add(off + cache_offset).cast::<CACHE_RELATIONSHIP>().read_unaligned();
            if cache.Level == 3 {
                let offset = cache_offset + std::mem::offset_of!(CACHE_RELATIONSHIP, Anonymous);
                let masks = ptr.add(off + offset).cast::<GROUP_AFFINITY>();
                let n = usize::from(cache.GroupCount.max(1)).min(size.saturating_sub(offset) / std::mem::size_of::<GROUP_AFFINITY>());
                for mask in std::slice::from_raw_parts(masks, n) {
                    for c in cpus.iter_mut().filter(|c| c.group == mask.Group && mask.Mask & (1usize << c.index) != 0) {
                        c.cache_bytes = cache.CacheSize as u64;
                    }
                }
            }
            off += size;
        }
    }
}

#[cfg(target_os = "linux")]
pub fn linux_cpu_allowed(n: u32) -> bool {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) != 0 || libc::CPU_ISSET(n as usize, &set)
    }
}
#[cfg(target_os = "linux")]
pub fn linux_locality(n: u32) -> (u32, u32, u64) {
    let base = format!("/sys/devices/system/cpu/cpu{n}");
    let node = std::fs::read_dir(&base)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .find_map(|e| e.file_name().to_str()?.strip_prefix("node")?.parse::<u32>().ok())
        .unwrap_or(0);
    for i in 0..8 {
        let dir = format!("{base}/cache/index{i}");
        if std::fs::read_to_string(format!("{dir}/level")).ok().is_none_or(|s| s.trim() != "3") {
            continue;
        }
        let llc = std::fs::read_to_string(format!("{dir}/shared_cpu_list")).ok().and_then(|s| s.split([',', '-']).next()?.trim().parse().ok()).unwrap_or(n);
        let bytes = std::fs::read_to_string(format!("{dir}/size"))
            .ok()
            .and_then(|s| {
                let s = s.trim();
                let factor = if s.ends_with('M') { 1 << 20 } else { 1 << 10 };
                s.trim_end_matches(['K', 'M']).parse::<u64>().ok().map(|n| n * factor)
            })
            .unwrap_or(0);
        return (node, llc, bytes);
    }
    (node, node, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chip(nodes: u32, cores: u32) -> Topology {
        let cpus = (0..nodes)
            .flat_map(|n| {
                (0..cores).flat_map(move |core| {
                    (0..2).map(move |s| Cpu {
                        id: n * cores * 2 + core * 2 + s,
                        group: n as u16,
                        index: core * 2 + s,
                        core,
                        numa: n,
                        llc: 0,
                        cache_bytes: 32 << 20,
                        class: 0,
                    })
                })
            })
            .collect();
        Topology { vendor: "GenuineIntel".into(), cpus }
    }
    #[test]
    fn numa_and_groups_are_spread_before_smt() {
        let t = chip(2, 32);
        let cfg = Config::fresh(128);
        let p = plan(&t, &cfg).unwrap().unwrap();
        assert_eq!(p.cpus.len(), 128);
        assert!(p.cpus[..64].iter().all(|c| c.index % 2 == 0));
        assert!(p.cpus[64..].iter().all(|c| c.index % 2 == 1));
        assert_eq!(p.cpus[..4].iter().map(|c| c.numa).collect::<Vec<_>>(), [0, 1, 0, 1]);
        assert_eq!(p.cpus.iter().map(|c| c.id).collect::<BTreeSet<_>>().len(), 128);
        let mut cfg=cfg;cfg.slot_threads=10;
        let p=plan(&t,&cfg).unwrap().unwrap();
        assert_eq!(p.slots.len(),14);
        assert!(p.slots.iter().all(|r| r.len()<=10 && p.cpus[r.clone()].iter().all(|c|c.numa==p.cpus[r.start].numa)));
        let slots=Slots::new(128,10,&p.slots);
        assert_eq!(slots.range(0),slots.range(14));
        assert_eq!(slots.width(0),10);
        assert_eq!(slots.width(6),4);
        let reduced=Slots::new(11,10,&p.slots);
        assert_eq!(reduced.width(1),1);
        assert!(!reduced.allows(0,10));
        assert!(reduced.allows(1,10));
    }
    #[test]
    fn cache_chip_fills_large_cache_physical_cores_then_other_ccd() {
        let mut t = chip(1, 16);
        t.vendor = "AuthenticAMD".into();
        for c in &mut t.cpus {
            c.llc = c.core / 8;
            c.cache_bytes = if c.llc == 0 { 96 << 20 } else { 32 << 20 };
        }
        let p = plan(&t, &Config::fresh(32)).unwrap().unwrap();
        assert!(p.cpus[..8].iter().all(|c| c.llc == 0 && c.index % 2 == 0));
        assert!(p.cpus[8..16].iter().all(|c| c.llc == 1 && c.index % 2 == 0));
        assert!(p.cpus[16..].iter().all(|c| c.index % 2 == 1));
        let mut cfg=Config::fresh(32);cfg.slot_threads=8;
        let p=plan(&t,&cfg).unwrap().unwrap();
        assert_eq!(p.slots.len(),4);
        for r in &p.slots {
            let cpus=&p.cpus[r.clone()];
            assert_eq!(cpus.len(),8);
            assert_eq!(cpus.iter().map(|c|(c.group,c.core)).collect::<BTreeSet<_>>().len(),4);
            assert!(cpus.iter().all(|c|c.llc==cpus[0].llc));
        }
        assert_eq!(p.cpus.iter().map(|c|c.id).collect::<BTreeSet<_>>().len(),32);
    }
    #[test]
    fn hybrid_odd_slots_mix_complete_p_cores_and_independent_e_cores() {
        let mut t=chip(1,6);
        for c in &mut t.cpus {c.class=1;}
        for i in 0..4 {t.cpus.push(Cpu{id:12+i,index:12+i,core:6+i,cache_bytes:32<<20,..Default::default()});}
        let mut cfg=Config::fresh(16);cfg.slot_threads=5;
        let p=plan(&t,&cfg).unwrap().unwrap();
        assert_eq!(p.slots.iter().map(Range::len).collect::<Vec<_>>(),vec![5,5,5,1]);
        for r in p.slots.iter().take(3) {
            let cpus=&p.cpus[r.clone()];
            assert_eq!(cpus.iter().filter(|c|c.class==1).count(),4);
            assert_eq!(cpus.iter().map(|c|c.core).collect::<BTreeSet<_>>().len(),3);
        }
        assert_eq!(p.cpus.iter().map(|c|c.id).collect::<BTreeSet<_>>().len(),16);
        // No E cores: odd caps must split some pairs, while every full slot
        // still owns two complete physical cores and just one shared thread.
        let mut homogeneous=t;homogeneous.cpus.retain(|c|c.class==1);
        let p=plan(&homogeneous,&cfg).unwrap().unwrap();
        for r in p.slots.iter().filter(|r|r.len()==5) {
            let cpus=&p.cpus[r.clone()];
            assert_eq!(cpus.iter().map(|c|c.core).collect::<BTreeSet<_>>().len(),3);
        }
    }
    #[test]
    fn explicit_partition_and_offset_do_not_wrap_budget() {
        let t = chip(2, 16);
        let mut cfg = Config::fresh(32);
        cfg.numa_nodes = vec![1];
        cfg.cpu_offset = 10;
        let p = plan(&t, &cfg).unwrap().unwrap();
        assert_eq!(p.cpus.len(), 32);
        assert!(p.cpus.iter().all(|c| c.numa == 1));
        assert_eq!(p.cpus[0].index, 20);
        cfg.numa_nodes = vec![2];
        assert!(plan(&t, &cfg).is_err());
        cfg.numa_nodes.clear();
        cfg.placement = "system".into();
        cfg.cpu_offset = 0;
        assert!(plan(&t, &cfg).unwrap().is_none());
    }
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn windows_reports_cache_capacity() {
        let t = Topology::read();
        assert!(!t.cpus.is_empty());
        assert!(t.cpus.iter().any(|c| c.cache_bytes > 0), "no L3 cache capacity");
    }
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn selected_lane_really_runs_on_its_processor() {
        let mut cfg = Config::fresh(1);
        cfg.placement = "cache".into();
        cfg.slot_threads = 3;
        init(&cfg).unwrap();
        let p = PLAN.get().unwrap().as_ref().unwrap();
        for (lane, cpu) in p.cpus.iter().enumerate().take(16) {
            let expected = u32::from(cpu.group) * 64 + cpu.index;
            std::thread::spawn(move || {
                assert!(apply_lane(lane));
                for _ in 0..1000 {
                    assert_eq!(crate::sys::current_cpu(), expected);
                    std::hint::spin_loop();
                }
            })
            .join()
            .unwrap();
        }
    }
}
