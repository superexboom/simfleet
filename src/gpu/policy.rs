// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded local observations, never a weapon allowlist or server authority.
use super::{Input, Probe};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

const KEEP: usize = 512;
const GOOD_TTL: u64 = 24 * 3600 * 1000;
const BAD_TTL: u64 = 30 * 60 * 1000;
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub allowed: bool,
    pub until: u64,
    pub updated: u64,
    pub private_kib: u32,
    pub reason: String,
    pub cpu_us: f64,
    pub gpu_ms: f64,
    pub failures: u32,
}
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    binding: String,
    entries: BTreeMap<String, Entry>,
    #[serde(default)]
    next_probe: u64,
}
pub struct Policy {
    saved: Saved,
    path: PathBuf,
    pending: Option<String>,
    dirty: bool,
    save_at: u64,
    notice: Option<String>,
    layout: super::MemoryLayout,
}
pub fn family(input: &Input) -> String {
    let r = &input.request;
    // Include every non-build context field, including unknown future fields.
    // Seed/run count and the loadout change among rows of the same context.
    let mut context = BTreeMap::new();
    if let Some(object) = r.as_object() {
        for (k, v) in object {
            if ![
                "mods",
                "rivens",
                "arcane",
                "arcanes",
                "evolutions",
                "extra_stats",
                "seed",
                "runs",
            ]
            .contains(&k.as_str())
            {
                context.insert(k, v);
            }
        }
    }
    let hash =
        Sha256::digest(serde_json::to_vec(&json!([input.ruler, context])).unwrap_or_default());
    format!("{hash:x}")
}
fn valid(e: &Entry) -> bool {
    super::PRIVATE_BUDGETS.contains(&e.private_kib)
        && e.reason.len() < 256
        && e.cpu_us.is_finite()
        && e.cpu_us >= 0.0
        && e.gpu_ms.is_finite()
        && e.gpu_ms >= 0.0
}
impl Policy {
    pub fn open(path: &Path, binding: String) -> Self {
        let backup = path.with_extension("json.bak");
        let load = |p: &Path| {
            std::fs::metadata(p).ok().filter(|m|m.len() < 512 << 10)
                .and_then(|_|std::fs::read(p).ok())
                .and_then(|b|serde_json::from_slice::<Saved>(&b).ok())
                .filter(|s| compatible_binding(&s.binding,&binding) && s.entries.len()<=KEEP
                    && s.entries.iter().all(|(k,e)|k.len()==64 && valid(e)))
        };
        let primary = load(path);
        let secondary = load(&backup);
        let updated = |s: &Saved| s.entries.values().map(|e|e.updated).max().unwrap_or(0);
        let recovered = secondary.as_ref().is_some_and(|b|primary.as_ref().is_none_or(|a|updated(b)>updated(a)));
        let mut saved = (if recovered {secondary} else {primary}).unwrap_or_else(|| Saved {
                binding: binding.clone(),
                entries: BTreeMap::new(),
                next_probe: 0,
            });
        // The memory ceiling changes batch size, not result correctness. Keep
        // measurements for the same engine/device instead of discarding them.
        let notice = if recovered { Some(format!("已从备份恢复 {} 项 GPU 学习记录",saved.entries.len())) }
            else if saved.entries.is_empty() && path.exists() { Some("GPU 学习缓存不兼容或损坏，重新学习；引擎/设备变更会使旧记录失效".into()) }
            else { Some(format!("已读取 {} 项 GPU 学习记录",saved.entries.len())) };
        // Keep the persisted identity consistent after a memory-budget change.
        saved.binding = binding;
        Self {
            saved,
            path: path.to_owned(),
            pending: None,
            dirty: false,
            save_at: 0,
            notice,
            layout: super::MemoryLayout::default(),
        }
    }
    pub fn with_layout(mut self, layout:super::MemoryLayout)->Self {
        self.layout=layout;
        self.saved.entries.retain(|_,e|e.private_kib>=layout.minimum_kib());
        self
    }
    #[cfg(test)]
    pub fn classify(&self, input: &Input, now: u64) -> Option<&Entry> {
        self.classify_key(&family(input), now)
    }
    pub fn classify_key(&self, key: &str, now: u64) -> Option<&Entry> {
        self.saved
            .entries
            .get(key)
            .filter(|e| e.until > now)
    }
    #[cfg(test)]
    pub fn allowed(&self, input: &Input, now: u64) -> bool {
        self.classify(input, now).is_some_and(|e| e.allowed)
    }
    #[cfg(test)]
    pub fn budget(&self, input: &Input, now: u64) -> u32 {
        self.classify(input, now).map_or(384, |e| e.private_kib)
    }
    pub fn can_probe(&self, now: u64) -> bool {
        self.pending.is_none() && now >= self.saved.next_probe
    }
    pub fn begin_probe(&mut self, input: &Input, now: u64) {
        self.pending = Some(family(input));
        self.saved.next_probe = now + 15_000;
        // Write before launching: a killed process must not retry the same
        // possibly fatal scene forever, nor lose other completed observations.
        let previous = self.saved.entries.get(&family(input));
        self.store(family(input), Entry {
            allowed:false, until:now+BAD_TTL, updated:now,
            private_kib:previous.map_or(self.layout.minimum_kib(),|e|e.private_kib),
            reason:"probe interrupted before a result".into(),
            cpu_us:previous.map_or(0.0,|e|e.cpu_us),gpu_ms:previous.map_or(0.0,|e|e.gpu_ms),
            failures:previous.map_or(0,|e|e.failures),
        });
    }
    fn store(&mut self, key: String, entry: Entry) {
        if self.saved.entries.len() >= KEEP && !self.saved.entries.contains_key(&key) {
            if let Some(old) = self
                .saved
                .entries
                .iter()
                .min_by_key(|(_, e)| e.updated)
                .map(|(k, _)| k.clone())
            {
                self.saved.entries.remove(&old);
            }
        }
        self.saved.entries.insert(key, entry);
        self.dirty = true;
        self.save_at = 0;
        self.flush(crate::sys::now_ms());
    }
    pub fn observed(&mut self, input: &Input, probe: &Probe, now: u64) -> bool {
        self.pending = None;
        let used_kib=probe.max_pages.saturating_mul(self.layout.page_bytes as u32)/1024;
        let headroom=if self.layout.compact(){12.max(used_kib/8)}else{48};
        let budget = super::PRIVATE_BUDGETS
            .into_iter()
            .find(|k| *k >= used_kib.saturating_add(headroom) && *k >= self.layout.minimum_kib())
            .unwrap_or(1536)
            .max(
                self.saved
                    .entries
                    .get(&family(input))
                    .map_or(self.layout.minimum_kib(), |e| e.private_kib),
            );
        let full_runs = self.layout.run_limit(budget) as f64;
        // Small samples cannot measure full occupancy. Require modest latency
        // and a conservative >one-core opportunity, then revise from full rows.
        let cpu_us = probe.cpu_ms * 1000.0 / probe.samples.max(1) as f64;
        let equivalent = cpu_us * full_runs / (probe.gpu_ms.max(1.0) * 2.0 * 1000.0);
        let allowed = probe.gpu_ms <= 6000.0 && equivalent >= 1.0 && used_kib.saturating_add(headroom) <= 1536;
        let reason = if allowed {
            "probe passed"
        } else {
            "probe too costly"
        };
        // An eight-run sample does not prove a full batch has recovered. Only
        // productive full rows clear the history of slow/failed batches.
        let failures = if allowed { self.saved.entries.get(&family(input)).map_or(0,|e|e.failures) } else {
            self.saved.entries.get(&family(input)).map_or(1, |e| e.failures.saturating_add(1))
        };
        // Bound exploration overhead even across worker restarts. Learned
        // batches bypass this cooldown and can run immediately.
        let pause=(probe.gpu_ms*4.0) as u64;
        self.saved.next_probe=now+pause.clamp(if allowed {15_000}else{60_000},120_000);
        self.store(
            family(input),
            Entry {
                allowed,
                until: now + if allowed { GOOD_TTL } else { BAD_TTL * u64::from(failures.min(12)) },
                updated: now,
                private_kib: budget,
                reason: reason.into(),
                cpu_us,
                gpu_ms: probe.gpu_ms,
                failures,
            },
        );
        allowed
    }
    pub fn failed(&mut self, input: &Input, reason: &str, now: u64) {
        self.pending = None;
        let key = family(input);
        let previous = self.saved.entries.get(&key);
        let failures = previous.map_or(1, |e| e.failures.saturating_add(1));
        let budget = previous.map_or(384, |e| e.private_kib);
        if input.probe { self.saved.next_probe=now+(30_000*u64::from(failures)).clamp(60_000,120_000); }
        self.store(
            key,
            Entry {
                allowed: false,
                until: now + BAD_TTL * (failures.min(12) as u64),
                updated: now,
                private_kib: budget,
                reason: reason.chars().take(200).collect(),
                cpu_us: previous.map_or(0.0, |e| e.cpu_us),
                gpu_ms: previous.map_or(0.0, |e| e.gpu_ms),
                failures,
            },
        );
    }
    pub fn resource_exhausted(&mut self, input: &Input, now: u64) {
        self.pending = None;
        let key = family(input);
        let previous = self.saved.entries.get(&key);
        let current = input.private_kib.max(previous.map_or(0, |e| e.private_kib));
        let next = if self.layout.compact() { current + current/2 } else { current*2 };
        let budget = super::PRIVATE_BUDGETS.into_iter().find(|k| *k >= next.min(1536)).unwrap_or(1536);
        let allowed = budget > current && previous.is_some_and(|e| e.allowed);
        let failures = previous.map_or(1, |e| e.failures.saturating_add(1));
        self.store(
            key,
            Entry {
                allowed,
                until: now
                    + if allowed {
                        GOOD_TTL
                    } else {
                        BAD_TTL * failures.min(12) as u64
                    },
                updated: now,
                private_kib: budget,
                reason: if allowed {
                    "private-page budget raised; reduce batch"
                } else {
                    "single-run private-page limit reached"
                }
                .into(),
                cpu_us: previous.map_or(0.0, |e| e.cpu_us),
                gpu_ms: previous.map_or(0.0, |e| e.gpu_ms),
                failures,
            },
        );
    }
    pub fn failed_batch(&mut self, inputs: &[Input], reason: &str, now: u64) {
        let mut families = std::collections::BTreeSet::new();
        for input in inputs {
            if families.insert(family(input)) {
                self.failed(input, reason, now);
            }
        }
    }
    pub fn completed(&mut self, input: &Input, ms: u64, now: u64) {
        let key = family(input);
        let Some(mut entry) = self.saved.entries.get(&key).cloned() else {
            return;
        };
        // Every row of one batch reports the same amortized time. Once its
        // family is deferred, the remaining rows must not count as retries.
        if !entry.allowed { return; }
        // Reject a full GPU row that costs longer than one CPU core would take.
        let cpu_ms = entry.cpu_us * input.request["runs"].as_u64().unwrap_or(0) as f64 / 1000.0;
        if ms as f64 > cpu_ms * 1.25 && ms > 100 {
            entry.allowed = false;
            entry.failures=entry.failures.saturating_add(1);
            entry.until = now + BAD_TTL*u64::from(entry.failures.min(12));
            entry.reason = "full batch slower than useful CPU overlap".into();
            entry.updated = now;
            self.store(key, entry);
        } else if entry.allowed && (entry.failures>0 || now.saturating_sub(entry.updated) >= 5 * 60_000) {
            entry.failures=0;
            entry.updated = now;
            entry.until = now + GOOD_TTL;
            self.store(key,entry);
        }
    }
    pub fn summary(&self, now: u64) -> (usize, usize) {
        self.saved
            .entries
            .values()
            .filter(|e| e.until > now)
            .fold(
                (0, 0),
                |(a, b), e| if e.allowed { (a + 1, b) } else { (a, b + 1) },
            )
    }
    pub fn take_notice(&mut self) -> Option<String> { self.notice.take() }
    pub fn flush(&mut self, now: u64) {
        if !self.dirty || now < self.save_at { return; }
        self.save_at = now + 15_000;
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = serde_json::to_vec(&self.saved)?;
            // Complete and flush both copies before publishing them. Recovery
            // reads the newest compatible complete snapshot after an abrupt exit.
            for target in [self.path.with_extension("json.bak"),self.path.clone()] {
                let tmp = target.with_extension(format!("{}.tmp",std::process::id()));
                let mut file = std::fs::File::create(&tmp)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(tmp,target)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.dirty=false,
            Err(e) => self.notice=Some(format!("GPU 学习保存失败，15 秒后重试：{e}")),
        }
    }
}
fn compatible_binding(saved: &str, current: &str) -> bool {
    if saved == current { return true; }
    // adaptive-v6 ends in the configured memory limit; all earlier identity
    // components (engine, module, native library, GPU, driver, CPU) must match.
    saved.starts_with("adaptive-v6:") && current.starts_with("adaptive-v6:")
        && saved.rsplit_once(':').zip(current.rsplit_once(':')).is_some_and(|((a,x),(b,y))|
            a==b && x.parse::<u64>().is_ok() && y.parse::<u64>().is_ok())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn input() -> Input {
        serde_json::from_value(json!({"id":1,"request":{"weapon":"future_weapon","enemy":"future_enemy","mode":"new","runs":1000,"duration":180,"level":200},"ruler":"new_ruler"})).unwrap()
    }
    #[test]
    fn context_is_not_a_weapon_allowlist_and_changes_are_isolated() {
        let a = input();
        let mut b = a.clone();
        b.request["mods"] = json!(["new_mod"]);
        b.request["seed"] = json!(7);
        assert_eq!(family(&a), family(&b));
        b.request["level"] = json!(500);
        assert_ne!(family(&a), family(&b));
        b = a.clone();
        b.request["future_rule"] = json!(true);
        assert_ne!(family(&a), family(&b));
    }
    #[test]
    fn learning_is_durable_before_exit_and_recovers_a_damaged_primary() {
        let file = std::env::temp_dir().join(format!(
            "simfleet-policy-{}.json",
            crate::sys::random_hex(8)
        ));
        let binding = "adaptive-v6:engine:module:native:gpu:driver:cpu:8192";
        let mut p = Policy::open(&file, binding.into());
        let input = input();
        p.failed(&input, "timeout", 100);
        assert!(!p.classify(&input, 101).unwrap().allowed);
        assert!(p.classify(&input, 100 + BAD_TTL).is_none());
        assert!(Policy::open(&file, "engine-device-b".into())
            .classify(&input, 101)
            .is_none());
        assert!(p.observed(
            &input,
            &Probe {
                samples: 8,
                cpu_ms: 8.0,
                gpu_ms: 100.0,
                max_pages: 35
            },
            200
        ));
        p.completed(&input, 3000, 201);
        assert!(!p.allowed(&input, 202));
        // Reopen without dropping/flushing the original policy: no clean-exit hook.
        let reopened = Policy::open(&file,binding.replace("8192","16384"));
        assert_eq!(reopened.classify(&input,202).unwrap().reason,"full batch slower than useful CPU overlap");
        std::fs::write(&file,b"{partial write").unwrap();
        let mut recovered = Policy::open(&file,binding.into());
        assert!(!recovered.allowed(&input,202));
        assert!(recovered.take_notice().unwrap().contains("备份"));
        let mut other=input.clone();other.request["enemy"]=json!("another_enemy");
        recovered.begin_probe(&other,300);
        let reopened=Policy::open(&file,binding.into());
        assert!(!reopened.can_probe(301));
        assert_eq!(reopened.classify(&other,301).unwrap().reason,"probe interrupted before a result");
        assert!(reopened.classify(&input,301).is_some());
        assert!(recovered.observed(&other,&Probe{samples:8,cpu_ms:8.0,gpu_ms:100.0,max_pages:30},302));
        assert_eq!(recovered.budget(&other,303),192);
        let mut costly=other.clone();costly.probe=true;
        recovered.failed(&costly,"GPU probe timed out",400);
        let reopened=Policy::open(&file,binding.into());
        assert!(!reopened.can_probe(59_999));
        assert!(reopened.can_probe(60_400));
        let _ = std::fs::remove_file(file.with_extension("json.bak"));
        let _ = std::fs::remove_file(file);
    }
    #[test]
    fn one_failed_batch_defers_each_context_once() {
        let file = std::env::temp_dir().join(format!(
            "simfleet-batch-failure-{}.json",
            crate::sys::random_hex(8)
        ));
        let mut p = Policy::open(&file, "test".into());
        let a = input();
        let rows = vec![a.clone(); 8];
        p.failed_batch(&rows, "worker closed", 100);
        let e = p.classify(&a, 101).unwrap();
        assert_eq!(
            (e.failures, e.until, e.private_kib),
            (1, 100 + BAD_TTL, 384)
        );
        p.failed_batch(&rows, "worker closed", 200);
        let e = p.classify(&a, 201).unwrap();
        assert_eq!((e.failures, e.until), (2, 200 + 2 * BAD_TTL));
        assert!(p.observed(&a,&Probe{samples:8,cpu_ms:32.0,gpu_ms:100.0,max_pages:30},300));
        assert_eq!(p.classify(&a,301).unwrap().failures,2);
        p.failed_batch(&rows,"GPU timed out",400);
        assert_eq!(p.classify(&a,401).unwrap().until,400+3*BAD_TTL);
        assert!(p.observed(&a,&Probe{samples:8,cpu_ms:32.0,gpu_ms:100.0,max_pages:30},500));
        p.completed(&a,1000,501);
        assert_eq!(p.classify(&a,502).unwrap().failures,0);
        p.completed(&a,10_000,503);
        p.completed(&a,10_000,504);
        assert_eq!((p.classify(&a,505).unwrap().failures,p.classify(&a,505).unwrap().until),(1,503+BAD_TTL));
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn resource_retry_keeps_its_larger_budget_after_a_small_probe() {
        let file = std::env::temp_dir().join(format!(
            "simfleet-resource-floor-{}.json",
            crate::sys::random_hex(8)
        ));
        let mut p = Policy::open(&file, "test".into());
        let mut row = input();
        let sample = Probe {
            samples: 8,
            cpu_ms: 32.0,
            gpu_ms: 100.0,
            max_pages: 30,
        };
        assert!(p.observed(&row, &sample, 10));
        assert_eq!(p.budget(&row, 11), 192);
        p.resource_exhausted(&row, 20);
        assert!(p.allowed(&row, 21));
        assert_eq!(p.budget(&row, 21), 384);
        // A later sample may miss the allocation-heavy seed/build again.
        assert!(p.observed(&row, &sample, 30));
        assert_eq!(p.budget(&row, 31), 384);
        row.private_kib = 384;
        p.resource_exhausted(&row, 40);
        assert_eq!(p.budget(&row, 41), 768);
        row.private_kib = 768;
        p.resource_exhausted(&row, 50);
        assert_eq!(p.budget(&row, 51), 1536);
        row.private_kib = 1536;
        p.resource_exhausted(&row, 60);
        assert!(!p.allowed(&row, 61));
        assert_eq!(p.budget(&row, 61), 1536);
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn expensive_cpu_work_can_justify_a_slower_probe() {
        let file = std::env::temp_dir().join(format!(
            "simfleet-probe-cost-{}.json",
            crate::sys::random_hex(8)
        ));
        let mut p = Policy::open(&file, "test".into());
        let input = input();
        assert!(p.observed(
            &input,
            &Probe {
                samples: 8,
                cpu_ms: 32.0,
                gpu_ms: 4500.0,
                max_pages: 39
            },
            10
        ));
        assert_eq!(p.budget(&input, 11), 256);
        assert!(!p.observed(
            &input,
            &Probe {
                samples: 8,
                cpu_ms: 0.1,
                gpu_ms: 4500.0,
                max_pages: 39
            },
            20
        ));
        assert!(!p.observed(
            &input,
            &Probe {
                samples: 8,
                cpu_ms: 32.0,
                gpu_ms: 7000.0,
                max_pages: 39
            },
            30
        ));
        assert_eq!(p.classify(&input,31).unwrap().failures,2);
        assert_eq!(p.classify(&input,31).unwrap().until,30+BAD_TTL*2);
        let mut compact=Policy::open(&file,"compact".into()).with_layout(super::super::MemoryLayout::checked(3,1024,2).unwrap());
        assert!(compact.observed(&input,&Probe{samples:8,cpu_ms:32.0,gpu_ms:4500.0,max_pages:70},40));
        assert_eq!(compact.budget(&input,41),96);
        let mut expanded=input.clone();expanded.private_kib=96;
        compact.resource_exhausted(&expanded,42);
        assert_eq!(compact.budget(&input,43),160);
        assert!(compact.observed(&input,&Probe{samples:8,cpu_ms:32.0,gpu_ms:4500.0,max_pages:70},44));
        assert_eq!(compact.budget(&input,45),160);
        let _ = std::fs::remove_file(file);
    }
}
