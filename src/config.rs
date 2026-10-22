// SPDX-License-Identifier: AGPL-3.0-or-later
//! `simfleet.toml` and where it lives.
//!
//! PORTABLE FIRST: the config and the engine cache sit beside the executable,
//! so the folder is the install — copy it to move it, delete it to uninstall.
//! A folder that cannot be written (Program Files) falls back to the user's
//! local app data.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The statement this client asks agreement to (main.rs `STATEMENT`), numbered
/// as WFSim numbers its own (69-board-work.js `COMPUTE_CONSENT_V`): a new one is
/// asked again, and what was agreed travels with every ask for work.
pub const CONSENT_V: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// The site this client computes for.
    pub server: String,
    /// THIS MACHINE'S COMPUTER ID — what a browser keeps as `wfsim-verifier`.
    /// Its points belong to whoever holds it, so keep it to yourself.
    pub verifier: String,
    /// When you agreed to compute (ISO 8601), and to which statement; both are
    /// sent with every ask for work.
    pub consent_at: String,
    #[serde(default = "consent_v_before_numbering")]
    pub consent_v: u32,
    /// Cores used while you are away from the computer.
    pub idle_cores: usize,
    /// Cores used while you are using it; 0 pauses computing then.
    pub active_cores: usize,
    /// Seconds without keyboard/mouse input before the computer counts as idle.
    pub idle_after_secs: u64,
    pub pause_on_battery: bool,
    /// "idle" (never slows anything), "below_normal" or "normal".
    pub priority: String,
    /// Intel hybrid CPUs: "auto" (P-cores, then E-cores, then the P-cores'
    /// second threads), "p" (P-cores first), "e" (E-cores first) or "windows"
    /// (the scheduler decides). Ignored on any other CPU, AMD included (affinity.rs).
    #[serde(default = "default_core_type")]
    pub core_type: String,
    /// Compute placement: auto, system, numa, cache. Does not change budget.
    #[serde(default = "default_core_type", deserialize_with = "read_placement")]
    pub placement: String,
    /// Empty means every NUMA node. Useful for independent process partitions.
    #[serde(default)]
    pub numa_nodes: Vec<u32>,
    /// Rotate the selected physical-first CPU list for disjoint process budgets.
    #[serde(default)]
    pub cpu_offset: usize,
    /// Use a native engine build from `engines/` for board orders when one
    /// matches the served engine; a validated optimizer can run riven gains too.
    pub native_engine: bool,
    /// Where published native engines are fetched from when `engines/` holds
    /// none (`<url>/engine-<id>/<id>-sfe<revision>.dll`); "" never downloads one.
    #[serde(default = "default_native_url")]
    pub native_engine_url: String,
    /// Computer ids run at once (each one holds a task and one asked ahead),
    /// as players open several browsers; 0 lets simfleet open and close them
    /// as the load asks (fleet.rs `control`).
    #[serde(default)]
    pub slots: usize,
    /// THE MOST groups at once (`[`/`]` change it while running); 0 is by the
    /// machine's size (`auto_max_slots`).
    #[serde(default)]
    pub max_slots: usize,
    /// Riven gains on the native engine too, when its build has the optimizer
    /// and it gave the wasm's answer on the riven canary.
    #[serde(default = "yes")]
    pub native_riven: bool,
    /// WFSim's own native library (signed, board rows only) while simfleet's
    /// build of the served engine is not published yet.
    #[serde(default = "yes")]
    pub official_engine: bool,
    /// Start the client again when it dies (watch.rs); `false`, or `--no-watch`,
    /// runs it alone.
    #[serde(default = "yes")]
    pub watch: bool,
    /// Soft process-memory ceiling in MiB; finish held work, save results,
    /// then start a fresh process. Zero disables this maintenance.
    #[serde(default = "default_memory_limit_mb")]
    pub memory_limit_mb: u64,
    /// Wait for simfleet's native build while the current engine remains accepted.
    /// Zero follows the served engine immediately. Server expiry always wins.
    #[serde(default = "default_native_wait_hours")]
    pub native_wait_hours: u64,
    /// The board's protocol: 8 (one sync for everything, fleet.rs) or 7 (packs
    /// asked, answered and renewed apart). The server may say 7 for a session.
    #[serde(default = "default_protocol")]
    pub protocol: u32,
    /// The ids beyond `verifier`, made as groups open. Keep them to yourself too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_verifiers: Vec<String>,
    #[serde(default)]
    pub account: Account,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Account {
    /// The `wfsim_session` cookie of a signed-in wfsim.app session.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// A config written before the field existed was agreed under statement 2.
fn consent_v_before_numbering() -> u32 {
    2
}

pub const NATIVE_URL: &str = "https://github.com/superexboom/simfleet/releases/download";
fn default_native_url() -> String {
    NATIVE_URL.into()
}
fn default_core_type() -> String {
    "auto".into()
}
fn read_placement<'de,D:serde::Deserializer<'de>>(d:D)->std::result::Result<String,D::Error> {
    let value=String::deserialize(d)?;
    Ok(if value.trim().is_empty(){"auto".into()}else{value})
}
fn yes() -> bool {
    true
}

fn default_memory_limit_mb() -> u64 {
    2048
}
fn default_native_wait_hours() -> u64 { 48 }
fn default_protocol() -> u32 {
    8
}

/// `max_slots = 0`: a quarter of the logical processors, 2 to 16. A pack is
/// at most twenty rows, which a big machine runs through in seconds, and the
/// controller opens a group only while lanes idle (fleet.rs `control`).
pub fn auto_max_slots(cores: usize) -> usize {
    (cores / 4).clamp(2, 16)
}

impl Config {
    pub fn fresh(idle_cores: usize) -> Self {
        Config {
            server: "https://wfsim.app".into(),
            verifier: crate::sys::random_hex(12),
            consent_at: String::new(),
            consent_v: CONSENT_V,
            idle_cores,
            active_cores: 1,
            idle_after_secs: 60,
            pause_on_battery: true,
            priority: "idle".into(),
            core_type: default_core_type(),
            placement: "auto".into(),
            numa_nodes: Vec::new(),
            cpu_offset: 0,
            native_engine: true,
            native_engine_url: NATIVE_URL.into(),
            slots: 0,
            max_slots: 0,
            native_riven: true,
            official_engine: true,
            watch: true,
            memory_limit_mb: default_memory_limit_mb(),
            native_wait_hours: default_native_wait_hours(),
            protocol: default_protocol(),
            extra_verifiers: Vec::new(),
            account: Account::default(),
        }
    }
    /// Agreed to the CURRENT statement; an older yes is asked again.
    pub fn consented(&self) -> bool {
        !self.consent_at.is_empty() && self.consent_v == CONSENT_V
    }
}

#[derive(Clone)]
pub struct Paths {
    pub dir: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
    pub engines: PathBuf,
}

impl Paths {
    pub fn locate() -> Paths {
        let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        let writable = |d: &Path| {
            let probe = d.join(".simfleet-write-test");
            let ok = std::fs::write(&probe, b"").is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        };
        let dir = match beside {
            Some(d) if d.join("simfleet.toml").exists() || writable(&d) => d,
            _ => std::env::var_os("LOCALAPPDATA")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("simfleet"),
        };
        Paths {
            config: dir.join("simfleet.toml"),
            cache: dir.join("cache"),
            engines: dir.join("engines"),
            dir,
        }
    }

    pub fn load(&self) -> Result<Option<Config>> {
        if !self.config.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&self.config)?;
        let c: Config = toml::from_str(&text).with_context(|| format!("读取 {} 失败", self.config.display()))?;
        Ok(Some(c))
    }

    /// THE IDS ANOTHER SIMFLEET ADDED to this config meanwhile (the same folder
    /// run twice), kept beside `c`'s own: each saves the whole file.
    pub fn merge_extras(&self, c: &mut Config) {
        if let Ok(Some(disk)) = self.load() {
            for v in disk.extra_verifiers {
                if v != c.verifier && !c.extra_verifiers.contains(&v) {
                    c.extra_verifiers.push(v);
                }
            }
        }
    }

    pub fn save(&self, c: &Config) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let body = toml::to_string_pretty(c)?;
        let text = format!(
            "# simfleet — WFSim 志愿计算客户端的配置。运行中按 S 打开设置，确认的改动会立即保存；手工编辑请先退出程序。\n\
             # native_wait_hours：新引擎缺少优化原生库时最多等待的小时数，默认 48；0 立即更新，服务器的旧引擎接受期限优先。\n\
             # core_type（只对 Intel 大小核和 Apple 芯片有效）：auto 先 P 核、再 E 核、最后 P 核超线程（Apple 芯片：性能核优先、能效核也用）；p 优先性能核；e 优先能效核（Apple 芯片：只用能效核）；windows 交给系统。\n\
             # verifier 是本机的计算者 id，account.session 是登录凭据：都请勿发给别人。\n\n{body}"
        );
        let tmp = self.config.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.config)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn older_config_gets_wait_default_and_current_options_roundtrip() {
        let mut cfg=Config::fresh(6);cfg.placement=String::new();cfg.account.name="test user".into();cfg.extra_verifiers=vec!["another".into()];
        let text=toml::to_string(&cfg).unwrap().lines().filter(|l|!l.starts_with("native_wait_hours")).collect::<Vec<_>>().join("\n");
        let decoded:Config=toml::from_str(&text).unwrap();assert_eq!(decoded.native_wait_hours,48);
        assert_eq!(decoded.placement,"auto");
        assert_eq!(decoded.extra_verifiers,cfg.extra_verifiers);assert_eq!(decoded.account.name,cfg.account.name);
        let mut settings=crate::settings::Settings::from_config(&decoded);settings.native_wait_hours=0;settings.write(&mut cfg);
        let decoded:Config=toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();assert_eq!(decoded.native_wait_hours,0);
    }
}
