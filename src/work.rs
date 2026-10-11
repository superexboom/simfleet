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
use crate::engine::{Backend, Call, Code, Exec, Job, NativeLib, Pool, WasmCode};
use crate::net::Net;
use crate::ui::{Ui, C_BAD, C_END, C_GO, C_INFO};
use crate::jsnum;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a page that can fill an order sends (worker/verify.js `PROTOCOL`):
/// 7 is board rows in packs (`/api/board/answers`).
pub const PROTOCOL: u32 = 7;
/// A piece of a row is sized to about this long on one lane.
pub const PIECE_MS: f64 = 250.0;
pub const RIVEN_RENEW: Duration = Duration::from_secs(120);
pub const STATS_EVERY: Duration = Duration::from_secs(300);
/// Jobs queued on a lane at once, so a lane never waits on the dispatcher.
const LANE_DEPTH: usize = 2;

#[derive(Clone, Debug)]
pub struct Release {
    pub engine: String,
    pub wasm: String,
}

const ID_OK: fn(&str) -> bool = |s| !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));

pub fn fetch_release(net: &Net) -> Result<Release> {
    let r = net.get("/release.json")?;
    let s = |k: &str| r.body[k].as_str().unwrap_or("").to_string();
    let rel = Release { engine: s("engine"), wasm: s("wasm") };
    if !ID_OK(&rel.engine) || !ID_OK(&rel.wasm) {
        bail!("release.json 无法识别：{}", r.body);
    }
    Ok(rel)
}

/// `variant` is "" for the baseline build, or a CPU level ("-v3").
pub fn native_lib_name(engine: &str, variant: &str) -> String {
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
        _ if v3 => vec!["-v3", ""],
        _ => vec![""],
    }
}

/// How the console names the backend a `Code` computes board rows on.
pub fn backend_label(code: &Code) -> &'static str {
    match &code.native {
        Some(n) if n.variant == "-v3" => "原生 AVX2",
        Some(_) => "原生",
        None => "wasm",
    }
}

fn variant_label(variant: &str) -> &'static str {
    if variant == "-v3" { "（AVX2 版）" } else { "" }
}

#[cfg(target_arch = "x86_64")]
fn cpu_v3() -> bool {
    use std::arch::is_x86_feature_detected as has;
    has!("avx2") && has!("fma") && has!("bmi1") && has!("bmi2") && has!("lzcnt") && has!("movbe") && has!("f16c")
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_v3() -> bool {
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
    build_code(net, paths, cfg, ui, rel, &bytes)
}

/// The engine from its wasm `bytes` (named by `rel`): compiled (or read from
/// the compile cache), with a native build beside it if one matches.
pub fn build_code(net: &Net, paths: &Paths, cfg: &Config, ui: &Ui, rel: &Release, bytes: &[u8]) -> Result<Arc<Code>> {
    std::fs::create_dir_all(&paths.cache)?;
    let cwasm = paths.cache.join(format!("{}.wasmtime49.cwasm", rel.wasm));
    if !cwasm.exists() {
        ui.status(|s| s.note = "编译引擎（只在新版本第一次运行时）…".into());
    }
    let t = Instant::now();
    let wasm = WasmCode::load(bytes, &cwasm)?;
    if t.elapsed() > Duration::from_secs(1) {
        ui.log(format!("{C_INFO}[引擎]{C_END} 编译完成，用时 {:.1}s（已缓存）", t.elapsed().as_secs_f64()));
    }
    // Only the current release is kept.
    if let Ok(rd) = std::fs::read_dir(&paths.cache) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.contains(&rel.wasm) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let mut code = Code { wasm, native: None };
    // The best build this CPU runs that is here (or published) and passes.
    for variant in if cfg.native_engine { native_variants() } else { vec![] } {
        let lib = paths.engines.join(native_lib_name(&rel.engine, variant));
        if !lib.exists() && (cfg.native_engine_url.is_empty() || !fetch_native(net, cfg, ui, rel, variant, &lib)) {
            continue;
        }
        match NativeLib::open(&lib) {
            Ok(mut n) if n.engine_id == rel.engine => {
                n.variant = variant;
                code.native = Some(n);
                let shared = Arc::new(code);
                let checked = canary(&shared);
                code = Arc::try_unwrap(shared).map_err(|_| anyhow!("engine still shared"))?;
                match checked {
                    Ok(()) => {
                        ui.log(format!(
                            "{C_INFO}[引擎]{C_END} 原生引擎 {}{} 与 wasm 逐位一致，榜单任务改用原生",
                            rel.engine,
                            variant_label(variant)
                        ));
                        break;
                    }
                    Err(e) => {
                        code.native = None;
                        ui.log(format!("{C_BAD}[引擎]{C_END} 原生引擎{}与 wasm 结果不一致，已停用：{e}", variant_label(variant)));
                    }
                }
            }
            Ok(n) => ui.log(format!("{C_GO}[引擎]{C_END} {} 的引擎号是 {}，不是 {}；不使用", lib.display(), n.engine_id, rel.engine)),
            Err(e) => ui.log(format!("{C_BAD}[引擎]{C_END} 原生引擎加载失败：{e}")),
        }
    }
    ui.status(|s| s.note.clear());
    Ok(Arc::new(code))
}

/// A PUBLISHED NATIVE ENGINE for this engine id, if one exists yet
/// (simfleet's `native-engine` workflow builds one for every engine wfsim.app
/// serves, card text blanked, and publishes it only after it matched the wasm
/// on real board rows). Checked against its `.sha256`; `load_code` then holds
/// it to the wasm on the canary row before using it. Silent when there is none
/// (releases before the `-v3` build have only the baseline).
pub fn fetch_native(net: &Net, cfg: &Config, ui: &Ui, rel: &Release, variant: &str, lib: &std::path::Path) -> bool {
    let name = native_lib_name(&rel.engine, variant);
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
            ui.log(format!("{C_GO}[引擎]{C_END} 原生引擎下载失败，继续用 wasm：{e:#}"));
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

pub struct RowScore {
    pub score: f64,
    pub metric: String,
    pub work: u64,
    pub compute_ms: u64,
    /// The counts the work was weighed from (`/api/board/score`), sent beside
    /// the answer; null from an engine that does not say them.
    pub counts: Value,
}

/// ONE ROW, MEASURED (69-board-work.js `measureRow`): its runs fought on the
/// lanes as pieces sized to about `PIECE_MS`, a shard per run, folded in run
/// order on `exec`, then scored. `lanes(true)` is the ceiling, `lanes(false)`
/// what may work now; `None` when `live()` went false or the engine refused.
#[allow(clippy::too_many_arguments)]
pub fn measure_row(
    pool: &mut Pool,
    exec: &mut Exec,
    backend: Backend,
    req: &Value,
    ruler: &str,
    lanes: &dyn Fn(bool) -> usize,
    live: &dyn Fn() -> bool,
    tick: &mut dyn FnMut(u32, u32),
) -> Result<Option<RowScore>> {
    let runs = req["runs"].as_u64().unwrap_or(0) as u32;
    let req_s = req.to_string();
    let ceiling = lanes(true).max(1);
    pool.ensure(ceiling);
    let (mut cursor, mut folded) = (0u32, 0u32);
    let mut est: Option<f64> = None;
    let mut spent = 0.0f64;
    let mut ready: BTreeMap<u32, Value> = BTreeMap::new();
    let mut inflight: HashMap<u64, (u32, u32)> = HashMap::new();
    let mut acc = "null".to_string();
    loop {
        if !live() {
            return Ok(None);
        }
        let now = lanes(false).min(ceiling);
        for k in 0..now {
            while pool.pending(k) < LANE_DEPTH && cursor < runs {
                let left = runs - cursor;
                let want = est.map(|e| (PIECE_MS / e).round().max(1.0) as u32).unwrap_or(1);
                let count = want.min(1000).min(left.div_ceil(ceiling as u32)).min(left).max(1);
                let tag = pool.next_tag();
                inflight.insert(tag, (cursor, count));
                pool.send(k, Job {
                    tag,
                    backend,
                    call: Call::Api("/api/board/runs", format!(r#"{{"request":{req_s},"from":{cursor},"count":{count}}}"#)),
                });
                cursor += count;
            }
        }
        if inflight.is_empty() {
            if cursor >= runs {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let Some(d) = pool.recv(Duration::from_millis(200)) else { continue };
        let Some((from, count)) = inflight.remove(&d.tag) else { continue };
        let v = jsnum::parse(&d.out?)?;
        let shards = v["shards"].as_array().filter(|s| s.len() == count as usize).ok_or_else(|| {
            anyhow!("/api/board/runs: {}", v["error"].as_str().unwrap_or("wrong shard count"))
        })?;
        spent += d.ms;
        let per = d.ms / count as f64;
        est = Some(est.map_or(per, |e| e * 0.7 + per * 0.3));
        for (i, s) in shards.iter().enumerate() {
            ready.insert(from + i as u32, s.clone());
        }
        // FOLD WHAT IS CONTIGUOUS, in run order.
        let mut pieces = Vec::new();
        while let Some(s) = ready.remove(&(folded + pieces.len() as u32)) {
            pieces.push(s);
        }
        if !pieces.is_empty() {
            let n = pieces.len() as u32;
            let body = format!(r#"{{"request":{req_s},"acc":{acc},"pieces":{}}}"#, Value::Array(pieces));
            let f = call(exec, backend, "/api/board/fold", body)?;
            acc = f["acc"].to_string();
            folded += n;
            tick(folded, runs);
        }
    }
    if folded != runs {
        return Ok(None);
    }
    let t = Instant::now();
    let s = call(exec, backend, "/api/board/score", format!(r#"{{"ruler":{},"request":{req_s},"acc":{acc}}}"#, json!(ruler)))?;
    spent += t.elapsed().as_secs_f64() * 1000.0;
    let score = s["score"].as_f64().context("score")?;
    Ok(Some(RowScore {
        score,
        metric: s["metric"].as_str().unwrap_or("").to_string(),
        work: s["work"].as_u64().unwrap_or(0),
        compute_ms: spent.round() as u64,
        counts: s["counts"].clone(),
    }))
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
    use super::*;
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
