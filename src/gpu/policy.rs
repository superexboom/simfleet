// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded local observations, never a weapon allowlist or server authority.
use super::{Input, Probe};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
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
}
pub struct Policy {
    saved: Saved,
    path: PathBuf,
    pending: Option<String>,
    next_probe: u64,
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
    [192, 384, 768, 1536].contains(&e.private_kib)
        && e.reason.len() < 256
        && e.cpu_us.is_finite()
        && e.cpu_us >= 0.0
        && e.gpu_ms.is_finite()
        && e.gpu_ms >= 0.0
}
impl Policy {
    pub fn open(path: &Path, binding: String) -> Self {
        let saved = std::fs::metadata(path)
            .ok()
            .filter(|m| m.len() < 512 << 10)
            .and_then(|_| std::fs::read(path).ok())
            .and_then(|b| serde_json::from_slice::<Saved>(&b).ok())
            .filter(|s| {
                s.binding == binding
                    && s.entries.len() <= KEEP
                    && s.entries.iter().all(|(k, e)| k.len() == 64 && valid(e))
            })
            .unwrap_or_else(|| Saved {
                binding,
                entries: BTreeMap::new(),
            });
        Self {
            saved,
            path: path.to_owned(),
            pending: None,
            next_probe: 0,
        }
    }
    pub fn classify(&self, input: &Input, now: u64) -> Option<&Entry> {
        self.saved
            .entries
            .get(&family(input))
            .filter(|e| e.until > now)
    }
    pub fn allowed(&self, input: &Input, now: u64) -> bool {
        self.classify(input, now).is_some_and(|e| e.allowed)
    }
    pub fn budget(&self, input: &Input, now: u64) -> u32 {
        self.classify(input, now).map_or(384, |e| e.private_kib)
    }
    pub fn can_probe(&self, now: u64) -> bool {
        self.pending.is_none() && now >= self.next_probe
    }
    pub fn begin_probe(&mut self, input: &Input, now: u64) {
        self.pending = Some(family(input));
        self.next_probe = now + 15_000;
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
        self.save();
    }
    pub fn observed(&mut self, input: &Input, probe: &Probe, now: u64) -> bool {
        self.pending = None;
        let budget = [192, 384, 768, 1536]
            .into_iter()
            .find(|k| *k >= probe.max_pages.saturating_add(12) * 4)
            .unwrap_or(1536)
            .max(
                self.saved
                    .entries
                    .get(&family(input))
                    .map_or(192, |e| e.private_kib),
            );
        let full_runs = (16000 * 192 / budget).min(16000) as f64;
        // Small samples cannot measure full occupancy. Require modest latency
        // and a conservative >one-core opportunity, then revise from full rows.
        let cpu_us = probe.cpu_ms * 1000.0 / probe.samples.max(1) as f64;
        let equivalent = cpu_us * full_runs / (probe.gpu_ms.max(1.0) * 2.0 * 1000.0);
        let allowed = probe.gpu_ms <= 6000.0 && equivalent >= 1.0 && probe.max_pages <= 372;
        let reason = if allowed {
            "probe passed"
        } else {
            "probe too costly"
        };
        self.store(
            family(input),
            Entry {
                allowed,
                until: now + if allowed { GOOD_TTL } else { BAD_TTL },
                updated: now,
                private_kib: budget,
                reason: reason.into(),
                cpu_us,
                gpu_ms: probe.gpu_ms,
                failures: 0,
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
        let budget = (current * 2).min(1536);
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
        // Reject a full GPU row that costs longer than one CPU core would take.
        let cpu_ms = entry.cpu_us * input.request["runs"].as_u64().unwrap_or(0) as f64 / 1000.0;
        if ms as f64 > cpu_ms * 1.25 && ms > 100 {
            entry.allowed = false;
            entry.until = now + BAD_TTL;
            entry.reason = "full batch slower than useful CPU overlap".into();
            entry.updated = now;
            self.store(key, entry);
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
    fn save(&self) {
        let _ = (|| -> std::io::Result<()> {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = self.path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(&self.saved)?)?;
            std::fs::rename(tmp, &self.path)
        })();
    }
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
    fn negatives_expire_and_binding_changes_clear_measurements() {
        let file = std::env::temp_dir().join(format!(
            "simfleet-policy-{}.json",
            crate::sys::random_hex(8)
        ));
        let mut p = Policy::open(&file, "engine-device-a".into());
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
        let _ = std::fs::remove_file(file);
    }
}
