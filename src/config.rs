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
    /// Use a native engine build from `engines/` for board orders when one
    /// matches the served engine (faster); riven gains always use the wasm.
    pub native_engine: bool,
    /// Where published native engines are fetched from when `engines/` holds
    /// none (`<url>/engine-<id>/<id>.dll`); "" never downloads one.
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
fn yes() -> bool {
    true
}

/// `max_slots = 0`: an eighth of the logical processors, 2 to 12. One group's
/// pack can start rows on every lane, so a few feed a big machine.
pub fn auto_max_slots(cores: usize) -> usize {
    (cores / 8).clamp(2, 12)
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
            native_engine: true,
            native_engine_url: NATIVE_URL.into(),
            slots: 0,
            max_slots: 0,
            native_riven: true,
            official_engine: true,
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

    pub fn save(&self, c: &Config) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let body = toml::to_string_pretty(c)?;
        let text = format!(
            "# simfleet — WFSim 志愿计算客户端的配置。可以直接编辑；运行中按键调整的核心数会在退出时写回。\n\
             # core_type（只对 Intel 大小核有效）：auto 先 P 核、再 E 核、最后 P 核超线程；p 优先性能核；e 优先能效核；windows 交给系统。\n\
             # verifier 是本机的计算者 id，account.session 是登录凭据：都请勿发给别人。\n\n{body}"
        );
        let tmp = self.config.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.config)?;
        Ok(())
    }
}
