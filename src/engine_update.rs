// SPDX-License-Identifier: AGPL-3.0-or-later
//! Persist the selected release and one waiting window for a lagging engine.
use crate::{
    config::{Config, Paths},
    engine::Code,
    net::Net,
    ui::Ui,
    work::{self, Release},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const EXPIRY_MARGIN_MS: u64 = 5 * 60 * 1000;
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct State {
    pub current: Option<Release>,
    pub waiting_since_ms: Option<u64>,
}
impl State {
    pub fn read(paths: &Paths) -> Self {
        let mut state: Self = std::fs::read(paths.dir.join("engine-state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let id = |s: &str| s.len() == 12 && s.bytes().all(|c| c.is_ascii_hexdigit());
        if state
            .current
            .as_ref()
            .is_some_and(|r| !id(&r.engine) || !id(&r.wasm) || (!r.data.is_empty() && !id(&r.data)))
        {
            state = Self::default();
        }
        state
    }
    pub fn save(&self, paths: &Paths) -> Result<()> {
        std::fs::create_dir_all(&paths.dir)?;
        let temp = paths.dir.join("engine-state.json.tmp");
        std::fs::write(&temp, serde_json::to_vec(self)?)?;
        std::fs::rename(temp, paths.dir.join("engine-state.json"))?;
        Ok(())
    }
    pub fn observe(&mut self, current: &Release, latest: &Release, now: u64) {
        if self
            .current
            .as_ref()
            .is_none_or(|r| r.engine != current.engine)
        {
            self.waiting_since_ms = None;
        }
        self.current = Some(current.clone());
        if current.engine == latest.engine {
            self.waiting_since_ms = None;
        } else if self.waiting_since_ms.is_none_or(|t| t > now) {
            self.waiting_since_ms = Some(now);
        }
    }
    pub fn can_wait(&self, current: &Release, latest: &Release, hours: u64, now: u64) -> bool {
        if hours == 0 || current.engine == latest.engine {
            return false;
        }
        if !latest.protocols.is_empty()
            && !latest
                .protocols
                .iter()
                .any(|p| current.protocols.contains(p))
        {
            return false;
        }
        let Some(since) = self.waiting_since_ms else {
            return false;
        };
        if now.saturating_sub(since) >= hours.saturating_mul(3_600_000) {
            return false;
        }
        latest.accepted.iter().any(|r| {
            r.engine == current.engine
                && crate::sys::parse_iso(&r.until)
                    .is_some_and(|until| until > now.saturating_add(EXPIRY_MARGIN_MS))
        })
    }
}
pub fn remember(paths: &Paths, rel: &Release) -> Result<()> {
    State {
        current: Some(rel.clone()),
        waiting_since_ms: None,
    }
    .save(paths)
}
fn local_native(paths: &Paths, rel: &Release) -> bool {
    work::native_variants().iter().any(|v| {
        paths
            .engines
            .join(work::native_lib_name(&rel.engine, v))
            .exists()
    })
}
fn optimized(code: &Arc<Code>, cfg: &Config) -> bool {
    code.native.as_ref().is_some_and(|n| {
        !n.official && (!cfg.native_riven || optimizer_pending_or_ready(n.has_optimize(), n.riven_state()))
    })
}

// Board validation has already passed. Search validation stays asynchronous;
// the dispatcher separately gates native riven tasks on a successful verdict.
fn optimizer_pending_or_ready(has_optimize: bool, state: u8) -> bool {
    has_optimize && matches!(state, 1 | 2)
}
pub struct Prepared {
    pub release: Release,
    pub code: Option<Arc<Code>>,
    pub waiting_since_ms: Option<u64>,
}

pub fn prepare(
    net: &Net,
    paths: &Paths,
    cfg: &Config,
    ui: &Ui,
    current: &Release,
    old_optimized: bool,
    force: bool,
) -> Result<Prepared> {
    let latest = work::fetch_release(net)?;
    let now = crate::sys::now_ms();
    let mut state = State::read(paths);
    state.observe(current, &latest, now);
    state.save(paths)?;
    let wait = !force
        && cfg.native_engine
        && old_optimized
        && state.can_wait(current, &latest, cfg.native_wait_hours, now);
    if wait && !local_native(paths, &latest) && !work::ours_published(net, cfg, &latest) {
        return Ok(Prepared {
            release: latest,
            code: None,
            waiting_since_ms: state.waiting_since_ms,
        });
    }
    let code = work::load_code(net, paths, cfg, ui, &latest)?;
    if wait && !optimized(&code, cfg) {
        return Ok(Prepared {
            release: latest,
            code: None,
            waiting_since_ms: state.waiting_since_ms,
        });
    }
    Ok(Prepared {
        release: latest,
        code: Some(code),
        waiting_since_ms: state.waiting_since_ms,
    })
}

pub fn startup(
    net: &Net,
    paths: &Paths,
    cfg: &Config,
    ui: &Ui,
    latest: Release,
) -> Result<(Release, Arc<Code>)> {
    let mut state = State::read(paths);
    if let Some(old) = state
        .current
        .clone()
        .filter(|r| r.engine != latest.engine && cfg.native_engine && !local_native(paths, &latest) && local_native(paths, r))
    {
        state.observe(&old, &latest, crate::sys::now_ms());
        state.save(paths)?;
        if state.can_wait(&old, &latest, cfg.native_wait_hours, crate::sys::now_ms()) {
            if let Ok(code) = work::load_code(net, paths, cfg, ui, &old) {
                if optimized(&code, cfg) {
                    ui.log(format!("[引擎] 继续使用已验证原生引擎 {}；新版本 {} 的原生构建会在后台检查，最多等待 {} 小时",old.engine,latest.engine,cfg.native_wait_hours));
                    return Ok((old, code));
                }
            }
        }
    }
    let code = work::load_code(net, paths, cfg, ui, &latest)?;
    remember(paths, &latest)?;
    Ok((latest, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rel(id: &str) -> Release {
        Release {
            engine: id.into(),
            wasm: "abcd".into(),
            data: String::new(),
            protocols: vec![8],
            accepted: vec![],
        }
    }
    #[test]
    fn pending_riven_validation_does_not_block_board_startup() {
        assert!(optimizer_pending_or_ready(true, 1));
        assert!(optimizer_pending_or_ready(true, 2));
        assert!(!optimizer_pending_or_ready(true, 3));
        assert!(!optimizer_pending_or_ready(false, 1));
    }

    #[test]
    fn deadline_survives_new_upstream_versions_and_serialization() {
        let now = crate::sys::parse_iso("2026-10-22T00:00:00Z").unwrap();
        let old = rel("old");
        let mut latest = rel("new");
        latest.accepted.push(work::Accepted {
            engine: "old".into(),
            until: crate::sys::iso(now + 72 * 3_600_000),
        });
        let mut s = State::default();
        s.observe(&old, &latest, now);
        assert!(s.can_wait(&old, &latest, 48, now));
        latest.engine = "newer".into();
        s.observe(&old, &latest, now + 24 * 3_600_000);
        s = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(s.can_wait(&old, &latest, 48, now + 47 * 3_600_000));
        assert!(!s.can_wait(&old, &latest, 48, now + 48 * 3_600_000));
        assert!(!s.can_wait(&old, &latest, 0, now));
        latest.accepted.clear();
        assert!(!s.can_wait(&old, &latest, 48, now));
    }
    #[test]
    fn grace_expiry_and_invalid_dates_override_wait() {
        let now = crate::sys::now_ms();
        let old = rel("old");
        let mut latest = rel("new");
        let mut s = State::default();
        s.observe(&old, &latest, now);
        for until in [
            "invalid".into(),
            crate::sys::iso(now - 1),
            crate::sys::iso(now + EXPIRY_MARGIN_MS),
        ] {
            latest.accepted = vec![work::Accepted {
                engine: "old".into(),
                until,
            }];
            assert!(!s.can_wait(&old, &latest, 48, now));
        }
        s.observe(&latest, &latest, now);
        assert!(s.waiting_since_ms.is_none());
    }
}
