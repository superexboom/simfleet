// SPDX-License-Identifier: AGPL-3.0-or-later
//! THE BOARD'S WORK, done on this machine — the page's compute loop
//! (WFSim web/src/static/app/69-board-work.js, 08-checkpoint-api.js
//! `quickFleet`) outside the browser, one task at a time:
//!
//! - a BOARD ORDER: a build, a ruler and a mode. Its runs are fought on every
//!   lane, folded strictly in run order, then scored — the scorer's path, so
//!   the number is the scorer's to the bit — and sent back with its work.
//! - a RIVEN GAIN: a frozen optimize request run as the page's quick descent
//!   across the lanes, its best build handed back.
//!
//! Whether a result is the first measurement or the one that confirms another,
//! this side cannot tell and does not need to.

use crate::config::{Config, Paths};
use crate::engine::{Backend, Call, Code, Done, Exec, Job, NativeLib, NativeBoard, Pool, WasmCode};
use crate::net::Net;
use crate::ui::{Ui, C_BAD, C_END, C_GO, C_INFO};
use crate::jsnum;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A piece of a row is sized to about this long on one lane.
pub const PIECE_MS: f64 = 250.0;
pub const RIVEN_RENEW: Duration = Duration::from_secs(120);
pub const STATS_EVERY: Duration = Duration::from_secs(300);
/// Jobs queued on a lane at once, so a lane never waits on the dispatcher.
const LANE_DEPTH: usize = 2;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Release {
    pub engine: String,
    pub wasm: String,
    /// The protocols the server takes (`protocols_served`); empty from a
    /// release.json that does not say.
    pub protocols: Vec<u32>,
    /// The engine's official data pack (`data`, the first 12 hex of its
    /// SHA-256: `/pkg/pack.data.<data>.json`); empty from a release that names
    /// none, whose engine carries its data.
    pub data: String,
    #[serde(default)]
    pub accepted: Vec<Accepted>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Accepted { pub engine: String, pub until: String }

const ID_OK: fn(&str) -> bool = |s| !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));

/// `protocols_served` of a reply or release.json.
pub fn protocols_of(v: &Value) -> Vec<u32> {
    v["protocols_served"].as_array().into_iter().flatten().filter_map(|p| p.as_u64().map(|p| p as u32)).collect()
}

pub fn fetch_release(net: &Net) -> Result<Release> {
    let r = net.get("/release.json")?;
    parse_release(&r.body)
}
pub fn parse_release(body: &Value) -> Result<Release> {
    let s = |k: &str| body[k].as_str().unwrap_or("").to_string();
    let rel = Release { engine: s("engine"), wasm: s("wasm"), protocols: protocols_of(body), data: s("data"),
        accepted:body["accepted"].as_array().into_iter().flatten().filter_map(|v|serde_json::from_value(v.clone()).ok()).collect() };
    if !ID_OK(&rel.engine) || !ID_OK(&rel.wasm) || !(rel.data.is_empty() || ID_OK(&rel.data)) {
        bail!("release.json 无法识别：{}", body);
    }
    Ok(rel)
}

/// `variant` is "" for the baseline build, or a CPU level ("-v3").
pub fn native_lib_name(engine: &str, variant: &str) -> String {
    legacy_native_lib_name(&format!("{engine}-sfe{}", include_str!("../native-engine/revision").trim()), variant)
}

fn legacy_native_lib_name(engine: &str, variant: &str) -> String {
    if cfg!(windows) {
        format!("{engine}{variant}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{engine}{variant}.dylib")
    } else {
        format!("lib{engine}{variant}.so")
    }
}

/// THE NATIVE BUILDS THIS CPU CAN RUN, best first: `-v3` (x86-64-v3 — AVX2,
/// FMA, BMI2, … — about 10% faster on board rows, the same bits; AVX-512 adds
/// nothing measurable) and the baseline, which runs on any x86-64.
/// `SIMFLEET_CPU=baseline` keeps to the baseline, `SIMFLEET_CPU=v3` to `-v3`.
pub fn native_variants() -> Vec<&'static str> {
    let v3 = cpu_v3();
    match std::env::var("SIMFLEET_CPU").unwrap_or_default().as_str() {
        "baseline" => vec![""],
        "v3" => if v3 { vec!["-v3"] } else { vec![] },
        _ if v3 && cfg!(windows) => vec!["-v3"],
        _ if v3 => vec!["-v3", ""],
        _ if cfg!(windows) => vec![],
        _ => vec![""],
    }
}

/// How the console names the backend a `Code` computes board rows on.
pub fn backend_label(code: &Code) -> &'static str {
    match &code.native {
        Some(n) if n.official => "原生（官方）",
        Some(n) if n.variant == "-v3" && n.can_optimize() => "原生 AVX2（含紫卡）",
        Some(n) if n.variant == "-v3" => "原生 AVX2",
        Some(n) if n.can_optimize() => "原生（含紫卡）",
        Some(_) => "原生",
        None => "wasm",
    }
}

fn variant_label(variant: &str) -> &'static str {
    if variant == "-v3" { "（AVX2 版）" } else { "" }
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn cpu_v3() -> bool {
    use std::arch::is_x86_feature_detected as has;
    has!("avx2") && has!("fma") && has!("bmi1") && has!("bmi2") && has!("lzcnt") && has!("movbe") && has!("f16c")
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) fn cpu_v3() -> bool {
    false
}

/// THE ENGINE THE SITE SERVES, ready to run: its wasm (downloaded once,
/// checked against the digest in its own name, compiled once), and a native
/// build of it if `engines/` holds one that reproduces the wasm's bits.
pub fn load_code(net: &Net, paths: &Paths, cfg: &Config, ui: &Ui, rel: &Release) -> Result<Arc<Code>> {
    std::fs::create_dir_all(&paths.cache)?;
    let name = format!("wfsim_wasm_bg.{}.wasm", rel.wasm);
    let file = paths.cache.join(&name);
    let digest_ok = |b: &[u8]| {
        let h = Sha256::digest(b);
        h.iter().map(|x| format!("{x:02x}")).collect::<String>().starts_with(&rel.wasm)
    };
    let bytes = match std::fs::read(&file) {
        Ok(b) if digest_ok(&b) => b,
        _ => {
            ui.log(format!("{C_INFO}[引擎]{C_END} 下载 {name}"));
            let b = net.download(&format!("/pkg/{name}"), |got, total| {
                ui.status(|s| {
                    s.note = if total > 0 {
                        format!("下载引擎 {:.1}/{:.1} MB", got as f64 / 1e6, total as f64 / 1e6)
                    } else {
                        format!("下载引擎 {:.1} MB", got as f64 / 1e6)
                    }
                })
            })?;
            if !digest_ok(&b) {
                bail!("下载的引擎校验失败（sha256 与 {} 不符）", rel.wasm);
            }
            std::fs::write(&file, &b)?;
            b
        }
    };
    let pack = fetch_data(net, paths, ui, rel)?;
    build_code(net, paths, cfg, ui, rel, &bytes, pack)
}

/// THE RELEASE'S DATA PACK (WFSim docs/CLIENTS.md "The official data pack"):
/// `/pkg/pack.data.<data>.json`, kept in the cache by its name and checked
/// against the digest in it, as the wasm is. None for a release naming none.
pub fn fetch_data(net: &Net, paths: &Paths, ui: &Ui, rel: &Release) -> Result<Option<Arc<Vec<u8>>>> {
    if rel.data.is_empty() {
        return Ok(None);
    }
    std::fs::create_dir_all(&paths.cache)?;
    let name = format!("pack.data.{}.json", rel.data);
    let file = paths.cache.join(&name);
    let digest_ok = |b: &[u8]| hex(&Sha256::digest(b)).starts_with(&rel.data);
    let bytes = match std::fs::read(&file) {
        Ok(b) if digest_ok(&b) => b,
        _ => {
            ui.log(format!("{C_INFO}[引擎]{C_END} 下载数据包 {name}"));
            let b = net.download(&format!("/pkg/{name}"), |got, total| {
                ui.status(|s| s.note = format!("下载数据包 {:.1}/{:.1} MB", got as f64 / 1e6, total as f64 / 1e6))
            })?;
            ui.status(|s| s.note.clear());
            if !digest_ok(&b) {
                bail!("下载的数据包校验失败（sha256 与 {} 不符）", rel.data);
            }
            std::fs::write(&file, &b)?;
            b
        }
    };
    Ok(Some(Arc::new(bytes)))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The engine from its wasm `bytes` (named by `rel`) and its data `pack`:
/// compiled (or read from the compile cache), with a native build beside it if
/// one matches.
pub fn build_code(net: &Net, paths: &Paths, cfg: &Config, ui: &Ui, rel: &Release, bytes: &[u8], pack: Option<Arc<Vec<u8>>>) -> Result<Arc<Code>> {
    std::fs::create_dir_all(&paths.cache)?;
    let cwasm = paths.cache.join(format!("{}.wasmtime49.cwasm", rel.wasm));
    if !cwasm.exists() {
        ui.status(|s| s.note = "编译引擎（只在新版本第一次运行时）…".into());
    }
    let t = Instant::now();
    let wasm = WasmCode::load(bytes, &cwasm)?.with_pack(pack.clone());
    if t.elapsed() > Duration::from_secs(1) {
        ui.log(format!("{C_INFO}[引擎]{C_END} 编译完成，用时 {:.1}s（已缓存）", t.elapsed().as_secs_f64()));
    }
    // Keep the selected old release while a candidate is being checked. A
    // restart must be able to continue it during its accepted grace window.
    let previous = crate::engine_update::State::read(paths).current;
    if let Ok(rd) = std::fs::read_dir(&paths.cache) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let keep_old=previous.as_ref().is_some_and(|r|n.contains(&r.wasm)||(!r.data.is_empty()&&n.contains(&r.data)));
            if !keep_old && !n.contains(&rel.wasm) && (rel.data.is_empty() || !n.contains(&rel.data)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let mut code = Code { wasm, native: None };
    // The best build this CPU runs that is here (or published) and passes.
    let variants = if cfg.native_engine { native_variants() } else { vec![] };
    for (variant, name) in [true, false].into_iter().flat_map(|current| {
        variants.iter().map(move |&variant| (variant, if current { native_lib_name(&rel.engine, variant) } else { legacy_native_lib_name(&rel.engine, variant) }))
    }) {
        let lib = paths.engines.join(name);
        if !lib.exists() && (cfg.native_engine_url.is_empty() || !fetch_native(net, cfg, ui, rel, &lib)) {
            continue;
        }
        match NativeLib::open(&lib) {
            Ok(mut n) if n.engine_id == rel.engine => {
                n.variant = variant;
                code = held_to_wasm(code, n, ui, format!("原生引擎 {}{}", rel.engine, variant_label(variant)))?;
                if code.native.is_some() {
                    break;
                }
            }
            Ok(n) => ui.log(format!("{C_GO}[引擎]{C_END} {} 的引擎号是 {}，不是 {}；不使用", lib.display(), n.engine_id, rel.engine)),
            Err(e) => ui.log(format!("{C_BAD}[引擎]{C_END} 原生引擎{}加载失败：{}", variant_label(variant), load_failure(&e))),
        }
    }
    // NONE OF SIMFLEET'S YET: WFSim's own library, for board rows (a riven gain
    // stays on the wasm — the library's one door is `api`).
    if code.native.is_none() && cfg.native_engine && cfg.official_engine {
        let lib = paths.engines.join(official_lib_name(&rel.engine));
        if lib.exists() || fetch_official(net, ui, rel, &lib) {
            match NativeLib::open_official(&lib, &rel.engine, pack.as_deref().map(|p| p.as_slice())) {
                Ok(n) => code = held_to_wasm(code, n, ui, format!("WFSim 官方原生引擎 {}", rel.engine))?,
                Err(e) => ui.log(format!("{C_BAD}[引擎]{C_END} WFSim 官方原生引擎加载失败：{}", load_failure(&e))),
            }
        }
    }
    ui.status(|s| s.note.clear());
    let code = Arc::new(code);
    // A RIVEN GAIN natively too, once a search gives the wasm's answer: held to
    // it on a thread of its own (a dozen seconds), riven gains waiting for the
    // verdict meanwhile (fleet.rs `riven_backend`) — so there always is one.
    if let Some(n) = code.native.as_ref().filter(|n| cfg.native_riven && n.has_optimize()) {
        n.riven_checking();
        let c = code.clone();
        let spawned = std::thread::Builder::new().name("riven-canary".into()).stack_size(8 << 20).spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| riven_canary(&c)))
                .unwrap_or_else(|_| Err(anyhow!("the riven canary panicked")));
            if let Some(n) = &c.native {
                n.riven_checked(r);
            }
        });
        if let Err(e) = spawned {
            n.riven_checked(Err(anyhow!("the riven canary did not start: {e}")));
        }
    }
    Ok(code)
}

/// Why a library would not load, with what Windows said; a library that needs
/// a DLL this machine lacks (126) is nearly always one built against the
/// Visual C++ runtime (WFSim's own library is: vcruntime140_1.dll).
fn load_failure(e: &anyhow::Error) -> String {
    let why = format!("{e:#}");
    if why.contains("os error 126") {
        format!("{why}——多半是缺少 Microsoft Visual C++ 2015-2022 运行库（x64），装上即可；simfleet 自己发布的版本不需要它")
    } else {
        why
    }
}

/// `n` beside the wasm in `code`, kept only if it gives the wasm's bits on the
/// canary rows; `what` names it in the log.
fn held_to_wasm(mut code: Code, n: NativeLib, ui: &Ui, what: String) -> Result<Code> {
    code.native = Some(n);
    let shared = Arc::new(code);
    let checked = canary(&shared);
    if checked.is_ok() && shared.native.as_ref().is_some() {
        match native_board_canary(&shared) {
            Ok(true) => {
                shared.native.as_ref().unwrap().board_checked.store(true, std::sync::atomic::Ordering::Release);
                ui.log(format!("{C_INFO}[引擎]{C_END} 原生批次接口通过顺序与乱序校验，分片直接在引擎内合并"));
            }
            Ok(false) => {}
            Err(e) => ui.log(format!("{C_GO}[引擎]{C_END} 原生批次接口未通过校验，沿用普通接口：{e:#}")),
        }
    }
    let mut code = Arc::try_unwrap(shared).map_err(|_| anyhow!("engine still shared"))?;
    match checked {
        Ok(()) => ui.log(format!("{C_INFO}[引擎]{C_END} {what} 与 wasm 逐位一致，榜单任务改用原生")),
        Err(e) => {
            code.native = None;
            ui.log(format!("{C_BAD}[引擎]{C_END} {what} 与 wasm 结果不一致，已停用：{e}"));
        }
    }
    Ok(code)
}

/// WFSIM'S OWN ENGINE LIBRARY (docs/CLIENTS.md "The official engine library"):
/// `engine/<id>.json` in its bucket is `{ sig, signed }`, `signed` the JSON
/// text `{ engine, commit, abi, files }` signed with WFSim's Ed25519 key, each
/// target's file `{ path, sha256, size }`.
const OFFICIAL_BASE: &str = "https://wfsim-1388973035.cos.ap-shanghai.myqcloud.com";
/// The key WFSim's desktop client verifies the manifest with.
const OFFICIAL_KEY: &str = "b488481177cb0a459a685c5d66f646768e7f66c405b92ecde81025581419a95e";

fn official_target() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("x86_64-windows")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-linux")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        // From the first engine WFSim ships after 2026-10-19; an engine
        // without it is computed on our build or the wasm.
        Some("aarch64-macos")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-macos")
    } else {
        None
    }
}

pub fn official_lib_name(engine: &str) -> String {
    legacy_native_lib_name(&format!("wfsim-{engine}"), "")
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len() % 2 == 0).then_some(())?;
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// `signed` under WFSim's key, or why not.
fn verify_official(signed: &str, sig: &str) -> Result<()> {
    let key: [u8; 32] = unhex(OFFICIAL_KEY).and_then(|k| k.try_into().ok()).context("key")?;
    let sig: [u8; 64] = unhex(sig).and_then(|s| s.try_into().ok()).context("签名格式不对")?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&key)?;
    key.verify_strict(signed.as_bytes(), &ed25519_dalek::Signature::from_bytes(&sig)).map_err(|_| anyhow!("签名不符"))
}

/// WFSim's library of `rel.engine` into `lib`, the manifest's signature
/// checked first and then the file's size and SHA-256 — either wrong, nothing
/// is kept. False when there is none (yet: it appears minutes after a release).
pub fn fetch_official(net: &Net, ui: &Ui, rel: &Release, lib: &std::path::Path) -> bool {
    let Some(target) = official_target() else { return false };
    let got = (|| -> Result<Vec<u8>> {
        let m: Value = serde_json::from_slice(&net.download(&format!("{OFFICIAL_BASE}/engine/{}.json", rel.engine), |_, _| {})?)?;
        let (sig, signed) = (m["sig"].as_str().context("清单没有签名")?, m["signed"].as_str().context("清单没有内容")?);
        verify_official(signed, sig)?;
        let s: Value = serde_json::from_str(signed)?;
        if s["engine"].as_str() != Some(rel.engine.as_str()) {
            bail!("清单的引擎号是 {}", s["engine"]);
        }
        // abi 1: `wfsim_api` and `wfsim_free`; abi 2 adds `wfsim_install_data`,
        // given the release's data pack when it is loaded (engine.rs).
        if !matches!(s["abi"].as_u64(), Some(1 | 2)) {
            bail!("不认识的接口版本 {}，请更新 simfleet", s["abi"]);
        }
        if let Some(d) = s["data"]["sha256"].as_str() {
            if !rel.data.is_empty() && !d.to_lowercase().starts_with(&rel.data) {
                bail!("清单的数据包 {} 不是 release.json 的 {}", &d[..d.len().min(12)], rel.data);
            }
        }
        let f = &s["files"][target];
        let (path, want) = (f["path"].as_str().context("清单里没有本平台")?, f["sha256"].as_str().context("sha256")?.to_lowercase());
        ui.log(format!("{C_INFO}[引擎]{C_END} 下载 WFSim 官方原生引擎 {}", rel.engine));
        let b = net.download(&format!("{OFFICIAL_BASE}/{path}"), |got, total| {
            ui.status(|s| s.note = format!("下载官方原生引擎 {:.1}/{:.1} MB", got as f64 / 1e6, total as f64 / 1e6))
        })?;
        let have: String = Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect();
        if have != want || f["size"].as_u64().is_some_and(|n| n != b.len() as u64) {
            bail!("sha256 不符");
        }
        Ok(b)
    })();
    ui.status(|s| s.note.clear());
    match got {
        Ok(b) => {
            let _ = std::fs::create_dir_all(lib.parent().unwrap_or(std::path::Path::new(".")));
            let tmp = lib.with_extension("part");
            if std::fs::write(&tmp, &b).and_then(|_| std::fs::rename(&tmp, lib)).is_err() {
                ui.log(format!("{C_BAD}[引擎]{C_END} 无法写入 {}", lib.display()));
                return false;
            }
            true
        }
        Err(e) if e.is::<crate::net::NotFound>() => false,
        Err(e) => {
            ui.log(format!("{C_GO}[引擎]{C_END} WFSim 官方原生引擎不可用：{e:#}"));
            false
        }
    }
}

/// Whether simfleet's own build of `rel.engine` is published (its `.sha256`),
/// without fetching it.
pub fn ours_published(net: &Net, cfg: &Config, rel: &Release) -> bool {
    !cfg.native_engine_url.is_empty()
        && native_variants().iter().any(|v| {
            let name = native_lib_name(&rel.engine, v);
            net.download(&format!("{}/engine-{}/{name}.sha256", cfg.native_engine_url.trim_end_matches('/'), rel.engine), |_, _| {}).is_ok()
        })
}

/// A PUBLISHED NATIVE ENGINE for this engine id, if one exists yet
/// (simfleet's `native-engine` workflow builds one for every engine wfsim.app
/// serves, card text blanked, and publishes it only after it matched the wasm
/// on real board rows). Checked against its `.sha256`; `load_code` then holds
/// it to the wasm on the canary row before using it. Silent when there is none
/// (releases before the `-v3` build have only the baseline).
pub fn fetch_native(net: &Net, cfg: &Config, ui: &Ui, rel: &Release, lib: &std::path::Path) -> bool {
    let name = lib.file_name().unwrap_or_default().to_string_lossy();
    let base = format!("{}/engine-{}/{}", cfg.native_engine_url.trim_end_matches('/'), rel.engine, name);
    let got = (|| -> Result<Vec<u8>> {
        let sum = String::from_utf8(net.download(&format!("{base}.sha256"), |_, _| {})?)?;
        let want = sum.split_whitespace().next().unwrap_or("").to_lowercase();
        ui.log(format!("{C_INFO}[引擎]{C_END} 下载原生引擎 {name}"));
        let b = net.download(&base, |got, total| {
            ui.status(|s| s.note = format!("下载原生引擎 {:.1}/{:.1} MB", got as f64 / 1e6, total as f64 / 1e6))
        })?;
        let have: String = Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect();
        if want.len() != 64 || have != want {
            bail!("sha256 不符");
        }
        Ok(b)
    })();
    ui.status(|s| s.note.clear());
    match got {
        Ok(b) => {
            let _ = std::fs::create_dir_all(lib.parent().unwrap_or(std::path::Path::new(".")));
            let tmp = lib.with_extension("part");
            if std::fs::write(&tmp, &b).and_then(|_| std::fs::rename(&tmp, lib)).is_err() {
                ui.log(format!("{C_BAD}[引擎]{C_END} 无法写入 {}", lib.display()));
                return false;
            }
            true
        }
        Err(e) if e.is::<crate::net::NotFound>() => false,
        Err(e) => {
            ui.log(format!("{C_GO}[引擎]{C_END} simfleet 的原生引擎 {name} 下载失败（{e:#}），先不用它，稍后再试"));
            false
        }
    }
}

/// A real board row (Braton Prime, demolisher, cycle), used to hold a native
/// engine to the wasm's bits and to time the engines.
pub fn sample_order() -> Value {
    json!({
        "record": {
            "weapon": "braton_prime",
            "mods": ["primed_cryo_rounds", "malignant_force", "primary_acuity", "galvanized_aptitude",
                     "galvanized_scope", "vile_acceleration", "vital_sense", "riven"],
            "evolutions": ["braton_prime_evo1_incarnon_form", "braton_prime_munitions_grit",
                           "braton_prime_voids_guidance", "braton_prime_critical_parallel"],
            "arcanes": ["primary_deadhead"],
            "exilus": "vigilante_supplies",
            "riven_pos": ["corrosive", "critical_damage", "fire_rate"],
            "riven_neg": "zoom",
            "riven_rolls": [1.1, 1.1, 1.1, 0.9]
        },
        "ruler": "demolisher",
        "mode": "cycle"
    })
}

pub fn call(exec: &mut Exec, backend: Backend, path: &'static str, body: String) -> Result<Value> {
    let text = exec.api(backend, path, body)?;
    let v = jsnum::parse(&text)?;
    if v["ok"] == json!(false) {
        bail!("{path}: {}", v["error"].as_str().unwrap_or("engine refused"));
    }
    Ok(v)
}

/// THE RIVEN CANARY: a small quick search, led and scored as the fleet does
/// it (`quick_fleet` lead and score steps, the scoring in two slices), run
/// on the wasm and on the native optimizer, must end in the same answer.
fn riven_canary(code: &Arc<Code>) -> Result<()> {
    let start = |slots: Value, fixed: Value| json!({ "slots": slots, "evolutions": [], "arcane": [], "fixed": fixed });
    let three = json!([{ "kind": "mods", "idx": 0 }, { "kind": "mods", "idx": 1 }, { "kind": "mods", "idx": 2 }]);
    // Three cards pinned, so the descent is short; ten-run screening, so the
    // rough items travel the fleet too.
    let req = json!({
        "weapon": "braton_prime", "enemy": "thrax_centurion", "level": 100,
        "duration": 2.0, "runs": 2, "finalists": 1, "candidate_runs": 10, "strategy": "quick",
        "starts": [start(json!(["hellfire", "infected_clip", "stormbringer"]), three)],
    });
    let search = |native: bool| -> Result<Value> {
        let mut exec = Exec::new(code.clone());
        let mut step = |body: &Value| -> Result<Value> {
            let s = body.to_string();
            let out = match (native, &code.native) {
                (true, Some(n)) => n.optimize(&s)?,
                (true, None) => bail!("no native engine"),
                (false, _) => exec.run(Backend::Wasm, &Call::Optimize(s))?,
            };
            jsnum::parse(&out)
        };
        let (mut scores, mut work, mut fights) = (json!([]), 0u64, 0u64);
        for n in 0..500 {
            let mut lead = req.clone();
            lead["quick_fleet"] = json!({ "lead": true, "fresh": n == 0, "scores": scores });
            let r = step(&lead)?;
            let pending = match r["pending"].as_array() {
                Some(p) if truthy(&r["pending"]) => p.clone(),
                _ => return Ok(json!({ "results": r["results"], "final_work": r["final_work"], "work": work, "fights": fights })),
            };
            let mut got = Vec::new();
            for part in pending.chunks(pending.len().div_ceil(2).max(1)) {
                let mut b = req.clone();
                b["quick_fleet"] = json!({ "score": part });
                let o = step(&b)?;
                work += o["work"].as_u64().unwrap_or(0);
                fights += o["fights"].as_u64().unwrap_or(0);
                got.extend(o["scores"].as_array().cloned().unwrap_or_default());
            }
            scores = Value::Array(got);
        }
        bail!("the search never settled")
    };
    let a=crate::reference_cache::compute(&json!({"riven_canary":req}),||search(false))?;
    let mut b=search(true)?;
    if a["results"].as_array().is_none_or(|r| r.is_empty()) {
        bail!("the wasm found nothing");
    }
    // The native engine asks for one worker (native-engine/ `sfe_optimize`),
    // which a row's replay request carries; nothing else may differ.
    strip_threads(&mut b);
    if a != b {
        bail!("{} ≠ {}", a.to_string().chars().take(160).collect::<String>(), b.to_string().chars().take(160).collect::<String>());
    }
    Ok(())
}

fn strip_threads(v: &mut Value) {
    match v {
        Value::Object(o) => {
            if o.get("threads") == Some(&json!(1)) {
                o.remove("threads");
            }
            o.values_mut().for_each(strip_threads);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_threads),
        _ => {}
    }
}

/// The native engine must give the wasm's answer on every step of a row.
fn canary(code: &Arc<Code>) -> Result<()> {
    let mut exec = Exec::new(code.clone());
    let o = sample_order();
    let mut both = |path: &'static str, body: String| -> Result<Value> {
        let a = call(&mut exec, Backend::Wasm, path, body.clone())?;
        let b = call(&mut exec, Backend::Native, path, body)?;
        if a != b {
            bail!("{path} 不一致");
        }
        Ok(a)
    };
    let order = both("/api/board/order", o.to_string())?;
    let req = order["request"].to_string();
    let runs = both("/api/board/runs", format!(r#"{{"request":{req},"from":0,"count":6}}"#))?;
    let fold = both("/api/board/fold", format!(r#"{{"request":{req},"acc":null,"pieces":{}}}"#, runs["shards"]))?;
    both("/api/board/score", format!(r#"{{"ruler":"demolisher","request":{req},"acc":{}}}"#, fold["acc"]))?;
    Ok(())
}

fn native_board_canary(code: &Arc<Code>) -> Result<bool> {
    let mut exec = Exec::new(code.clone());
    let order = call(&mut exec, Backend::Wasm, "/api/board/order", sample_order().to_string())?;
    let mut req = order["request"].clone();
    req["runs"] = json!(14);
    let Some(probe) = NativeBoard::new(code.clone(), &req, "demolisher", true) else { return Ok(false) };
    drop(probe);
    let pieces = call(&mut exec, Backend::Wasm, "/api/board/runs", json!({"request":req,"from":0,"count":14}).to_string())?;
    let fold = call(&mut exec, Backend::Wasm, "/api/board/fold", json!({"request":req,"pieces":pieces["shards"],"acc":null}).to_string())?;
    let expected = call(&mut exec, Backend::Wasm, "/api/board/score", json!({"request":req,"acc":fold["acc"],"ruler":"demolisher"}).to_string())?;
    for ranges in [vec![(0,14)], vec![(7,7),(0,7)], (0..14).rev().map(|i| (i,1)).collect()] {
        let row = NativeBoard::new(code.clone(), &req, "demolisher", true).context("native context")?;
        for (from,count) in ranges { exec.run(Backend::Native, &Call::NativeRuns(row.clone(),from,count))?; }
        let actual = jsnum::parse(&exec.run(Backend::Native, &Call::NativeScore(row))?)?;
        if actual != expected { bail!("native batch answer differs"); }
    }
    Ok(true)
}

pub struct RowScore {
    pub score: f64,
    pub metric: String,
    pub work: u64,
    pub compute_ms: u64,
    /// The counts the work was weighed from (`/api/board/score`), sent beside
    /// the answer; null from an engine that does not say them.
    pub counts: Value,
}

/// A BOARD ROW (69-board-work.js `measureRow`): its runs fought as pieces of
/// about `PIECE_MS`, a shard per run, folded strictly in run order, then scored.
///
/// THE LANES READ AND FOLD, the dispatcher only joins text: a piece comes back
/// as its shards' JSON, already read as the page reads it (`Call::Runs`), and
/// each fold is a job of its own (`Call::Fold`), one at a time for a row. Read
/// on the dispatcher, a run's shard (some 4 KB of JSON) cost it about 0.15 ms
/// to parse, write out and fold — a sixth of one core on six lanes, so with
/// sixty the lanes would wait on it. The fold's body is the same text, byte for
/// byte, as when it was read there.
pub struct BoardRun {
    pub gpu_pending: bool,
    native: Option<Arc<NativeBoard>>,
    home: Option<usize>,
    req_s: String,
    ruler: String,
    pub runs: u32,
    cursor: u32,
    pub folded: u32,
    est: Option<f64>,
    /// Pieces back and not folded yet, by their first run: how many runs, and
    /// their shards as JSON text without the array's brackets.
    ready: BTreeMap<u32, (u32, String)>,
    inflight: HashMap<u64, (u32, u32)>,
    /// The fold on a lane now, and the run it brings the row to.
    folding: Option<(u64, u32)>,
    scoring: Option<u64>,
    acc: String,
    spent: f64,
}

impl BoardRun {
    pub fn new(req: &Value, ruler: &str) -> BoardRun {
        BoardRun {
            gpu_pending: false,
            native: None,
            home: None,
            req_s: req.to_string(),
            ruler: ruler.to_string(),
            runs: req["runs"].as_u64().unwrap_or(0) as u32,
            cursor: 0,
            folded: 0,
            est: None,
            ready: BTreeMap::new(),
            inflight: HashMap::new(),
            folding: None,
            scoring: None,
            acc: "null".into(),
            spent: 0.0,
        }
    }
    pub fn enable_native(&mut self, code: Arc<Code>, req: &Value) {
        self.native = NativeBoard::new(code, req, &self.ruler, false);
    }
    pub fn gpu_input(&self, id:u64)->Option<crate::gpu::Input> {
        if self.cursor!=0 || self.gpu_pending {return None;}
        let request:Value=serde_json::from_str(&self.req_s).ok()?;
        crate::gpu::eligible(&request,&self.ruler).then(||crate::gpu::Input{id,request,ruler:self.ruler.clone(),private_kib:192,probe:false})
    }
    pub fn job_on(&mut self, tag: u64, lanes: usize, lane: usize, remote: bool) -> Option<Call> {
        if !remote && self.home.is_some_and(|h| !crate::placement::same_domain(h,lane)) { return None; }
        let share = if remote { lanes } else { crate::placement::domain_lanes(lane,lanes) };
        let call = self.job(tag,share)?;
        self.home.get_or_insert(lane);
        Some(call)
    }
    /// The next job: a fold of what is contiguous first (it moves the row on),
    /// else the next piece. `lanes`: how many share the row.
    pub fn job(&mut self, tag: u64, lanes: usize) -> Option<Call> {
        if self.gpu_pending {return None;}
        if self.runs > 0 && self.folded == self.runs && self.inflight.is_empty() && self.scoring.is_none() {
            self.scoring = Some(tag);
            if let Some(row) = &self.native { return Some(Call::NativeScore(row.clone())); }
            let body = format!(r#"{{"ruler":{},"request":{},"acc":{}}}"#, json!(self.ruler), self.req_s, self.acc);
            return Some(Call::Api("/api/board/score", body));
        }
        if self.folding.is_none() {
            let mut upto = self.folded;
            let mut parts: Vec<String> = Vec::new();
            while let Some((n, text)) = self.ready.remove(&upto) {
                parts.push(text);
                upto += n;
            }
            if !parts.is_empty() {
                self.folding = Some((tag, upto));
                return Some(Call::Fold(format!(r#"{{"request":{},"acc":{},"pieces":[{}]}}"#, self.req_s, self.acc, parts.join(","))));
            }
        }
        if self.cursor >= self.runs {
            return None;
        }
        let left = self.runs - self.cursor;
        let want = self.est.map(|e| (PIECE_MS / e).round().max(1.0) as u32).unwrap_or(1);
        // Native calls carry only an integer, but a one-run tail still pays
        // dispatch and synchronization. Bound that floor by measured cost so
        // a slow run is never forced into a long tail.
        let floor = if self.native.is_some() {
            self.est.map(|e| (20.0/e).floor().clamp(1.0,8.0) as u32).unwrap_or(1)
        } else { 1 };
        let count = want.min(1000).min(left.div_ceil(lanes.max(1) as u32)).max(floor).min(left).max(1);
        let (from, req) = (self.cursor, &self.req_s);
        self.inflight.insert(tag, (from, count));
        self.cursor += count;
        if let Some(row) = &self.native { return Some(Call::NativeRuns(row.clone(), from, count)); }
        Some(Call::Runs(format!(r#"{{"request":{req},"from":{from},"count":{count}}}"#), count))
    }
    /// A job back; even the final score runs on a lane. Scoring builds the
    /// simulator's report and may replay a fight, so it cannot block dispatch.
    pub fn done(&mut self, d: Done) -> Result<Option<RowScore>> {
        if self.scoring == Some(d.tag) {
            let s = jsnum::parse(&d.out?)?;
            if s["ok"] == json!(false) {
                bail!("/api/board/score: {}", s["error"].as_str().unwrap_or("engine refused"));
            }
            self.spent += d.ms;
            return Ok(Some(RowScore {
                score: s["score"].as_f64().context("score")?,
                metric: s["metric"].as_str().unwrap_or("").to_string(),
                work: s["work"].as_u64().unwrap_or(0),
                compute_ms: self.spent.round() as u64,
                counts: s["counts"].clone(),
            }));
        }
        if let Some((_, upto)) = self.folding.filter(|(t, _)| *t == d.tag) {
            self.folding = None;
            self.acc = d.out?;
            // The folds count in `compute_ms` too, as the page counts them.
            self.spent += d.ms;
            self.folded = upto;
            return Ok(None);
        }
        let Some((from, count)) = self.inflight.remove(&d.tag) else { return Ok(None) };
        let text = d.out?;
        self.spent += d.ms;
        let per = d.ms / count as f64;
        self.est = Some(self.est.map_or(per, |e| e * 0.7 + per * 0.3));
        if self.native.is_some() {
            self.folded = self.folded.max(text.parse::<u32>().context("native progress")?);
            return Ok(None);
        }
        let inner = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).context("/api/board/runs: shards")?;
        self.ready.insert(from, (count, inner.to_string()));
        Ok(None)
    }
}

/// A `/api/board/runs` answer as the page reads it: its `count` shards, as the
/// JSON text the page would write them back as.
pub fn shards_text(out: &str, count: u32) -> Result<String> {
    let v = jsnum::parse(out)?;
    match v["shards"].as_array() {
        Some(s) if s.len() == count as usize => Ok(v["shards"].to_string()),
        _ => bail!("/api/board/runs: {}", v["error"].as_str().unwrap_or("wrong shard count")),
    }
}

/// A `/api/board/fold` answer as the page reads it: its `acc`, as JSON text.
pub fn acc_text(out: &str) -> Result<String> {
    let v = jsnum::parse(out)?;
    if v["ok"] == json!(false) {
        bail!("/api/board/fold: {}", v["error"].as_str().unwrap_or("engine refused"));
    }
    Ok(v["acc"].to_string())
}

/// ONE ROW, MEASURED on the lanes (`BoardRun`, as the fleet measures one):
/// `lanes(true)` is the ceiling, `lanes(false)` what may work now; `None` when
/// `live()` went false or the engine refused.
#[allow(clippy::too_many_arguments)]
pub fn measure_row(
    pool: &mut Pool,
    _exec: &mut Exec,
    backend: Backend,
    req: &Value,
    ruler: &str,
    lanes: &dyn Fn(bool) -> usize,
    live: &dyn Fn() -> bool,
    tick: &mut dyn FnMut(u32, u32),
) -> Result<Option<RowScore>> {
    let mut row = BoardRun::new(req, ruler);
    if backend == Backend::Native { row.enable_native(pool.code().clone(), req); }
    if row.runs == 0 {
        bail!("a row of no runs");
    }
    let ceiling = lanes(true).max(1);
    pool.ensure(ceiling);
    let mut out = 0usize;
    loop {
        if !live() {
            return Ok(None);
        }
        let now = lanes(false).min(ceiling);
        for k in 0..now {
            while pool.pending(k) < LANE_DEPTH {
                let tag = pool.next_tag();
                let Some(call) = row.job(tag, ceiling) else { break };
                pool.send(k, Job { tag, backend, call });
                out += 1;
            }
        }
        if out == 0 {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let Some(d) = pool.recv(Duration::from_millis(200)) else { continue };
        out -= 1;
        if let Some(s) = row.done(d)? {
            return Ok(Some(s));
        }
        tick(row.folded, row.runs);
    }
}

// ---- JS truthiness, for porting `boardPayloadFromResult` exactly ----------------

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// `a || b` where `b` may be absent (undefined): `None` is a key JSON.stringify drops.
fn or_ctx(res: &Value, key: &str, ctx: &Value) -> Option<Value> {
    if truthy(&res[key]) {
        Some(res[key].clone())
    } else {
        ctx.get(key).cloned()
    }
}

/// THE BUILD A RESULT IS (68-board-submit.js `boardPayloadFromResult`), with
/// the frozen page's context: the eight main slots (the exilus dropped, a riven
/// as the `riven` slot, the stance appended), the riven's shape, the rest.
pub fn board_payload(res: &Value, ctx: &Value) -> Value {
    let exilus = (truthy(&res["exilus"]) && res["exilus"] != "none").then(|| res["exilus"].clone());
    let arcs: Vec<Value> = match &res["arcane"] {
        Value::Array(a) => a.clone(),
        v if truthy(v) => vec![v.clone()],
        _ => vec![],
    }
    .into_iter()
    .filter(|a| truthy(a) && a != "none")
    .collect();
    let mods_in: Vec<Value> = res["mods"].as_array().cloned().unwrap_or_default();
    let is_riven = |m: &Value| m.as_str().is_some_and(|s| s.starts_with("riven:"));
    let riven_id = mods_in.iter().find(|m| is_riven(m)).and_then(|m| m.as_str()).unwrap_or("-").to_string();
    let mut mods: Vec<Value> = mods_in
        .iter()
        .filter(|m| exilus.as_ref().is_none_or(|x| *m != x))
        .map(|m| if is_riven(m) { json!("riven") } else { m.clone() })
        .collect();
    if truthy(&ctx["stance"]) && !mods.contains(&ctx["stance"]) {
        mods.push(ctx["stance"].clone());
    }
    let mut o = Map::new();
    if truthy(&ctx["benchmark"]) {
        o.insert("benchmark".into(), ctx["benchmark"].clone());
    }
    if let Some(w) = ctx.get("weapon") {
        o.insert("weapon".into(), w.clone());
    }
    if let Some(m) = or_ctx(res, "mode", ctx) {
        o.insert("mode".into(), m);
    }
    o.insert("mods".into(), Value::Array(mods));
    let empty = json!({ "riven_pos": [], "riven_neg": "" });
    let shape = if truthy(&ctx["rivens"]) {
        let r = &ctx["rivens"][riven_id.as_str()];
        if truthy(r) { r.clone() } else { empty }
    } else {
        empty
    };
    if let Value::Object(m) = shape {
        for (k, v) in m {
            o.insert(k, v);
        }
    }
    o.insert("evolutions".into(), Value::Array(res["evolutions"].as_array().cloned().unwrap_or_default()));
    o.insert("arcanes".into(), Value::Array(arcs));
    if let Some(v) = or_ctx(res, "valence", ctx) {
        o.insert("valence".into(), v);
    }
    if let Some(x) = exilus {
        o.insert("exilus".into(), x);
    }
    for k in ["grip", "loader", "wielder"] {
        if let Some(v) = ctx.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    #[test]
    fn gpu_ownership_blocks_cpu_and_failure_resumes_at_run_zero() {
        let req=serde_json::json!({"weapon":"fulmin_prime","enemy":"corrupted_heavy_gunner","mode":"semi_auto","duration":180,"runs":1000});
        let mut row=super::BoardRun::new(&req,"heavy_gunner");
        assert!(row.gpu_input(7).is_some());
        row.gpu_pending=true;
        assert!(row.job(1,16).is_none());
        assert!(row.gpu_input(7).is_none());
        row.gpu_pending=false;
        match row.job(2,16).unwrap() {
            crate::engine::Call::Runs(body,1)=>assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["from"],0),
            _=>panic!("expected first CPU run after GPU fallback"),
        }
        assert!(row.gpu_input(7).is_none());
    }
    use super::*;

    #[test]
    fn local_revision_does_not_rename_official_or_legacy_libraries() {
        let revision = include_str!("../native-engine/revision").trim();
        assert!(!revision.is_empty() && revision.bytes().all(|c| c.is_ascii_digit()));
        for variant in ["", "-v3"] {
            let old = legacy_native_lib_name("abcdef", variant);
            let new = native_lib_name("abcdef", variant);
            assert_eq!(new, old.replace("abcdef", &format!("abcdef-sfe{revision}")));
            assert_ne!(new, old);
        }
        assert_eq!(official_lib_name("abcdef"), legacy_native_lib_name("wfsim-abcdef", ""));
    }

    #[test]
    fn out_of_order_runs_fold_in_order_and_score_on_a_lane() {
        let mut row = BoardRun::new(&json!({ "runs": 2 }), "demolisher");
        assert!(matches!(row.job(1, 2), Some(Call::Runs(_, 1))));
        assert!(matches!(row.job(2, 2), Some(Call::Runs(_, 1))));
        let done = |tag, text: &str| Done { tag, lane: 0, ms: 1.0, out: Ok(text.into()) };
        assert!(row.done(done(2, "[2]")).unwrap().is_none());
        assert!(row.job(3, 2).is_none());
        assert!(row.done(done(1, "[1]")).unwrap().is_none());
        let Some(Call::Fold(body)) = row.job(4, 2) else { panic!("fold") };
        assert_eq!(jsnum::parse(&body).unwrap()["pieces"], json!([1, 2]));
        assert!(row.job(5, 2).is_none());
        assert!(row.done(done(4, r#"{"sum":3}"#)).unwrap().is_none());
        let Some(Call::Api("/api/board/score", body)) = row.job(6, 2) else { panic!("score") };
        assert_eq!(jsnum::parse(&body).unwrap()["acc"], json!({ "sum": 3 }));
        assert!(row.job(7, 2).is_none());
        let score = row.done(done(6, r#"{"ok":true,"score":0.125,"metric":"kpm","work":17,"counts":{"gas":8}}"#)).unwrap().unwrap();
        assert_eq!(score.score.to_bits(), 0.125f64.to_bits());
        assert_eq!(score.work, 17);
        assert_eq!(score.compute_ms, 4);
        assert_eq!(score.counts["gas"], json!(8));
    }

    #[test]
    fn failed_final_score_is_not_an_answer() {
        let mut row = BoardRun::new(&json!({ "runs": 1 }), "demolisher");
        row.folded = 1;
        assert!(matches!(row.job(1, 1), Some(Call::Api("/api/board/score", _))));
        let d = Done { tag: 1, lane: 0, ms: 1.0, out: Ok(r#"{"ok":false,"error":"bad acc"}"#.into()) };
        assert!(matches!(row.done(d), Err(e) if e.to_string().contains("bad acc")));
    }
    #[test]
    fn payload_like_the_page() {
        let res = json!({ "mods": ["serration", "riven:abc", "vigilante_armaments"], "exilus": "vigilante_armaments",
            "arcane": ["primary_merciless", "none"], "evolutions": ["e1"], "mode": "", "kill_progress": 3.5 });
        let ctx = json!({ "benchmark": "standard_single_target", "weapon": "braton", "mode": "base", "valence": "",
            "rivens": { "riven:abc": { "riven_pos": ["critical_chance"], "riven_neg": "zoom" } } });
        let b = board_payload(&res, &ctx);
        assert_eq!(b.to_string(), r#"{"benchmark":"standard_single_target","weapon":"braton","mode":"base","mods":["serration","riven"],"riven_pos":["critical_chance"],"riven_neg":"zoom","evolutions":["e1"],"arcanes":["primary_merciless"],"valence":"","exilus":"vigilante_armaments"}"#);
    }
}
