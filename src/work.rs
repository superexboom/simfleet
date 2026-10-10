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

use crate::config::{Config, Paths, CONSENT_V};
use crate::engine::{Backend, Call, Code, Exec, Job, NativeLib, Pool, WasmCode};
use crate::net::Net;
use crate::ui::{group, points, Ctl, Ui, C_BAD, C_END, C_GO, C_INFO, C_OK};
use crate::{account, jsnum, sys};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What a page that can fill an order sends (worker/verify.js `PROTOCOL`).
pub const PROTOCOL: u32 = 6;
/// A piece of a row is sized to about this long on one lane.
const PIECE_MS: f64 = 250.0;
/// A lease runs 30 min on the server; past this the fight stops and is given back.
const LEASE_SAFE: Duration = Duration::from_secs(27 * 60);
const RIVEN_RENEW: Duration = Duration::from_secs(120);
const ASK_EVERY: Duration = Duration::from_secs(10);
const STATS_EVERY: Duration = Duration::from_secs(300);
/// Ask for the next task once this share of the current one is done.
const AHEAD_AT: f64 = 0.8;
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

pub fn native_lib_name(engine: &str) -> String {
    if cfg!(windows) {
        format!("{engine}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{engine}.dylib")
    } else {
        format!("lib{engine}.so")
    }
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
    let cwasm = paths.cache.join(format!("{}.wasmtime49.cwasm", rel.wasm));
    if !cwasm.exists() {
        ui.status(|s| s.note = "编译引擎（只在新版本第一次运行时）…".into());
    }
    let t = Instant::now();
    let wasm = WasmCode::load(&bytes, &cwasm)?;
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
    let lib = paths.engines.join(native_lib_name(&rel.engine));
    if cfg.native_engine && !lib.exists() && !cfg.native_engine_url.is_empty() {
        fetch_native(net, cfg, ui, rel, &lib);
    }
    if cfg.native_engine && lib.exists() {
        match NativeLib::open(&lib) {
            Ok(n) if n.engine_id == rel.engine => {
                code.native = Some(n);
                let shared = Arc::new(code);
                let checked = canary(&shared);
                code = Arc::try_unwrap(shared).map_err(|_| anyhow!("engine still shared"))?;
                match checked {
                    Ok(()) => ui.log(format!("{C_INFO}[引擎]{C_END} 原生引擎 {} 与 wasm 逐位一致，榜单任务改用原生", rel.engine)),
                    Err(e) => {
                        code.native = None;
                        ui.log(format!("{C_BAD}[引擎]{C_END} 原生引擎与 wasm 结果不一致，已停用：{e}"));
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
/// it to the wasm on the canary row before using it. Silent when there is none.
pub fn fetch_native(net: &Net, cfg: &Config, ui: &Ui, rel: &Release, lib: &std::path::Path) -> bool {
    let name = native_lib_name(&rel.engine);
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

fn call(exec: &mut Exec, backend: Backend, path: &'static str, body: String) -> Result<Value> {
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
    }))
}

// ---- JS truthiness, for porting `boardPayloadFromResult` exactly ----------------

fn truthy(v: &Value) -> bool {
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

// ---- the fleet ------------------------------------------------------------------

struct Held {
    lease: String,
    code: Option<String>,
}

/// THE NEXT TASKS, asked before this one ends (worker/verify.js, asking ahead),
/// each answer a lease this client holds until it uses or returns it.
#[derive(Default)]
struct Ahead {
    asks: Mutex<VecDeque<JoinHandle<Option<Value>>>>,
    asked_for: Mutex<String>,
}

pub struct Fleet {
    pub net: Net,
    pub cfg: Config,
    pub ctl: Arc<Ctl>,
    pub ui: Arc<Ui>,
    paths: Paths,
    pool: Pool,
    exec: Exec,
    rel: Release,
    held: Option<Held>,
    ahead: Arc<Ahead>,
    tasks_ahead: usize,
    riven_took: Duration,
    recent_orders: VecDeque<Value>,
    recent_rivens: VecDeque<Value>,
    stats_at: Option<Instant>,
    native_checked: Instant,
    claimed: bool,
    pub session_dropped: bool,
    banned_said: bool,
    /// `replay`: compute a saved task and print the answer, sending nothing.
    pub offline: bool,
}

impl Fleet {
    pub fn new(net: Net, cfg: Config, paths: Paths, ctl: Arc<Ctl>, ui: Arc<Ui>, rel: Release, code: Arc<Code>) -> Fleet {
        let backend = if code.native.is_some() { "native" } else { "wasm" };
        ui.status(|s| {
            s.engine = rel.engine.clone();
            s.backend = backend.into();
        });
        Fleet {
            net,
            cfg,
            ctl,
            ui,
            paths,
            pool: Pool::new(code.clone()),
            exec: Exec::new(code),
            rel,
            held: None,
            ahead: Arc::new(Ahead::default()),
            tasks_ahead: 0,
            riven_took: Duration::ZERO,
            recent_orders: VecDeque::new(),
            recent_rivens: VecDeque::new(),
            stats_at: None,
            native_checked: Instant::now(),
            claimed: false,
            session_dropped: false,
            banned_said: false,
            offline: false,
        }
    }

    fn quit(&self) -> bool {
        self.ctl.quit.load(Ordering::Relaxed)
    }

    fn idle(&self) -> bool {
        sys::idle_ms() >= self.cfg.idle_after_secs * 1000
    }

    /// Why nothing may compute now, if anything.
    fn held_why(&self) -> Option<&'static str> {
        if self.ctl.paused.load(Ordering::Relaxed) {
            Some("已暂停（按 P 继续）")
        } else if self.cfg.pause_on_battery && sys::on_battery() {
            Some("正在用电池，已暂停")
        } else if self.ctl.active_cores.load(Ordering::Relaxed) == 0 && !self.idle() {
            Some("你正在使用电脑，已暂停")
        } else {
            None
        }
    }

    fn lanes_fn(&self) -> impl Fn(bool) -> usize {
        let ctl = self.ctl.clone();
        let idle_after = self.cfg.idle_after_secs * 1000;
        move |ceiling: bool| {
            let idle = ceiling || sys::idle_ms() >= idle_after;
            if idle {
                ctl.idle_cores.load(Ordering::Relaxed).max(1)
            } else {
                ctl.active_cores.load(Ordering::Relaxed).max(1)
            }
        }
    }

    fn backend(&self) -> Backend {
        if self.pool.code().native.is_some() { Backend::Native } else { Backend::Wasm }
    }

    fn ask_body(&self, ahead: bool) -> Value {
        let mut b = json!({
            "verifier": self.cfg.verifier,
            "engine": self.rel.engine,
            "protocol": PROTOCOL,
            "consent": { "v": CONSENT_V, "at": self.cfg.consent_at },
            "lanes": (self.lanes_fn())(false),
        });
        if ahead {
            b["ahead"] = json!(true);
        }
        b
    }

    fn release_lease(net: &Net, verifier: &str, lease: &str, code: Option<&str>) {
        match code {
            Some(c) => net.post_ok(&format!("/api/appraise/{c}/renew"), &json!({ "lease": lease, "verifier": verifier, "release": true })),
            None => net.post_ok("/api/board/release", &json!({ "lease": lease, "verifier": verifier })),
        };
    }

    /// The lease this client holds, given back on every way out that is not an answer.
    fn give_back(&mut self) {
        if let Some(h) = self.held.take() {
            Self::release_lease(&self.net, &self.cfg.verifier, &h.lease, h.code.as_deref());
        }
    }

    fn give_back_ahead(&self) {
        let asks: Vec<_> = self.ahead.asks.lock().unwrap().drain(..).collect();
        for a in asks {
            if let Ok(Some(r)) = a.join() {
                let w = &r["work"];
                if let Some(lease) = w["lease"].as_str() {
                    let code = (w["kind"] == "riven_gain").then(|| w["code"].as_str().unwrap_or("")).filter(|c| !c.is_empty());
                    Self::release_lease(&self.net, &self.cfg.verifier, lease, code);
                }
            }
        }
    }

    /// Ask, in the background, for as many next tasks as the server allows.
    fn ask_ahead_fn(&self) -> impl Fn(&str) {
        let (ahead, net, body, n) = (self.ahead.clone(), self.net.clone(), self.ask_body(true), self.tasks_ahead);
        move |task: &str| {
            let mut asked = ahead.asked_for.lock().unwrap();
            if n == 0 || *asked == task {
                return;
            }
            *asked = task.to_string();
            let mut q = ahead.asks.lock().unwrap();
            while q.len() < n {
                let (net, body) = (net.clone(), body.clone());
                q.push_back(std::thread::spawn(move || net.post_ok("/api/board/work", &body)));
            }
        }
    }

    pub fn shutdown(&mut self) {
        self.give_back();
        self.give_back_ahead();
    }

    /// THE LOOP: one task after another while there is work, an ask every
    /// ten seconds while there is none.
    pub fn run(&mut self) {
        self.refresh_stats();
        while !self.quit() {
            let worked = match self.work_once() {
                Ok(w) => w,
                Err(e) => {
                    self.ui.log(format!("{C_BAD}[出错]{C_END} {e:#}"));
                    self.give_back();
                    self.ui.status(|s| s.task.clear());
                    false
                }
            };
            if self.stats_at.is_none_or(|t| t.elapsed() > STATS_EVERY) {
                self.refresh_stats();
            }
            self.maybe_native();
            if !worked {
                let t = Instant::now();
                let was_held = self.held_why().is_some();
                while t.elapsed() < ASK_EVERY && !self.quit() && self.held_why().is_some() == was_held {
                    self.ui.status(|s| {
                        s.lanes_now = 0;
                        s.idle = sys::idle_ms() >= self.cfg.idle_after_secs * 1000;
                    });
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
        self.shutdown();
    }

    fn work_once(&mut self) -> Result<bool> {
        if let Some(why) = self.held_why() {
            self.give_back_ahead();
            self.ui.status(|s| s.state = why.into());
            return Ok(false);
        }
        self.claim_once();
        // THE FIRST TASK ASKED AHEAD that holds one, when there is one; else asked now.
        let mut pre: Option<Value> = None;
        loop {
            let next = self.ahead.asks.lock().unwrap().pop_front();
            let Some(h) = next else { break };
            if let Ok(Some(r)) = h.join() {
                if r["work"].is_object() {
                    pre = Some(r);
                    break;
                }
            }
        }
        let ask = match pre {
            Some(p) => Some(p),
            None => self.net.post_ok("/api/board/work", &self.ask_body(false)),
        };
        let Some(ask) = ask else {
            self.ui.status(|s| s.state = "连不上服务器，稍后重试".into());
            return Ok(false);
        };
        if let Some(n) = ask["ahead"].as_u64() {
            self.tasks_ahead = n as usize;
        }
        if ask["stale"] == json!(true) {
            self.refresh_engine()?;
            return Ok(false);
        }
        if ask["banned"] == json!(true) {
            let until = ask["until"].as_str().unwrap_or("?").to_string();
            if !self.banned_said {
                self.banned_said = true;
                self.ui.log(format!("{C_BAD}[冷却]{C_END} 服务器复核时与本机结果不一致，暂停派活至 {until}"));
            }
            self.ui.status(|s| s.state = format!("冷却中，至 {until}"));
            return Ok(false);
        }
        self.banned_said = false;
        let w = ask["work"].clone();
        if !w.is_object() {
            self.ui.status(|s| s.state = "等待任务".into());
            return Ok(false);
        }
        self.ui.status(|s| s.state = "计算中".into());
        if w["kind"] == "riven_gain" {
            self.riven_once(&w)
        } else {
            self.board_once(&w)
        }
    }

    fn board_once(&mut self, w: &Value) -> Result<bool> {
        let lease = w["lease"].as_str().unwrap_or("").to_string();
        self.held = Some(Held { lease: lease.clone(), code: None });
        let until = Instant::now() + LEASE_SAFE;
        let (weapon, ruler, mode) = (
            w["record"]["weapon"].as_str().unwrap_or("?").to_string(),
            w["ruler"].as_str().unwrap_or("").to_string(),
            w["mode"].as_str().unwrap_or("").to_string(),
        );
        let label = format!("{weapon} · {ruler} · {mode}");
        let backend = self.backend();
        let order = call(&mut self.exec, backend, "/api/board/order", json!({ "record": w["record"], "ruler": ruler, "mode": mode }).to_string());
        let order = match order {
            Ok(o) => o,
            Err(e) => {
                self.give_back();
                self.ui.log(format!("{C_BAD}[跳过]{C_END} 榜单 {label}：{e:#}"));
                return Ok(true);
            }
        };
        let runs = order["request"]["runs"].as_u64().unwrap_or(0);
        self.ui.log(format!("{C_GO}[开始]{C_END} 榜单  {label}  {runs} 次模拟"));
        self.ui.status(|s| {
            s.task = format!("榜单 {label}");
            s.done = 0;
            s.total = runs;
        });
        let began = Instant::now();
        let lanes = self.lanes_fn();
        let (ctl, ui, ahead) = (self.ctl.clone(), self.ui.clone(), self.ask_ahead_fn());
        let pause_on_battery = self.cfg.pause_on_battery;
        let idle_after = self.cfg.idle_after_secs * 1000;
        let live = move || {
            !ctl.quit.load(Ordering::Relaxed)
                && !ctl.paused.load(Ordering::Relaxed)
                && !(pause_on_battery && sys::on_battery())
                && !(ctl.active_cores.load(Ordering::Relaxed) == 0 && sys::idle_ms() < idle_after)
                && Instant::now() < until
        };
        let lanes_ui = self.lanes_fn();
        let mut tick = |done: u32, total: u32| {
            ui.status(|s| {
                s.done = done as u64;
                s.total = total as u64;
                s.lanes_now = lanes_ui(false);
                s.idle = sys::idle_ms() >= idle_after;
            });
            if total > 0 && done as f64 >= AHEAD_AT * total as f64 {
                ahead(&lease);
            }
        };
        let s = measure_row(&mut self.pool, &mut self.exec, backend, &order["request"], &ruler, &lanes, &live, &mut tick);
        let s = match s {
            Ok(Some(s)) => s,
            Ok(None) => {
                self.give_back();
                self.ui.status(|s| s.task.clear());
                self.ui.log(format!("{C_GO}[中断]{C_END} 榜单 {label}，任务已交还"));
                return Ok(true);
            }
            Err(e) => {
                self.give_back();
                self.ui.status(|s| s.task.clear());
                self.ui.log(format!("{C_BAD}[失败]{C_END} 榜单 {label}：{e:#}"));
                return Ok(true);
            }
        };
        if self.offline {
            println!("{}", json!({ "kind": "board", "score": s.score, "metric": s.metric, "work": s.work, "compute_ms": s.compute_ms }));
            self.held = None;
            return Ok(true);
        }
        let sent = self.net.post_ok(
            "/api/board/verify",
            &json!({ "lease": lease, "verifier": self.cfg.verifier, "engine": self.rel.engine,
                     "score": s.score, "metric": s.metric, "work": s.work, "compute_ms": s.compute_ms }),
        );
        if sent.is_some() {
            self.held = None;
        } else {
            self.give_back();
        }
        self.ui.status(|st| {
            st.task.clear();
            if sent.is_some() {
                st.session_tasks += 1;
                st.session_work += s.work;
            }
        });
        if sent.is_some() {
            self.recent_orders.push_back(json!({ "identity": w["identity"], "ruler": ruler, "mode": mode }));
            while self.recent_orders.len() > 50 {
                self.recent_orders.pop_front();
            }
            self.ui.log(format!(
                "{C_OK}[完成]{C_END} 榜单  {label}  = {:.4} {} · {:.2} 分 · {:.1}s",
                s.score,
                s.metric,
                points(s.work),
                began.elapsed().as_secs_f64()
            ));
        } else {
            self.ui.log(format!("{C_BAD}[失败]{C_END} 榜单 {label}：结果没能发出"));
        }
        Ok(true)
    }

    /// Run `bodies` as optimize calls, each on its own lane among `lanes` (lane 0
    /// first), a free lane pulling the next. `None` when the task stopped.
    fn optimize_all(&mut self, bodies: Vec<String>, lanes: usize, rv: &mut RivenRun) -> Result<Option<Vec<Value>>> {
        let n = bodies.len();
        let mut queue: VecDeque<(usize, String)> = bodies.into_iter().enumerate().collect();
        let mut outs: Vec<Option<Value>> = vec![None; n];
        let mut inflight: HashMap<u64, usize> = HashMap::new();
        let mut busy = vec![false; lanes];
        let mut got = 0;
        while got < n {
            for (k, b) in busy.iter_mut().enumerate() {
                if !*b {
                    if let Some((i, body)) = queue.pop_front() {
                        let tag = self.pool.next_tag();
                        inflight.insert(tag, i);
                        *b = true;
                        self.pool.send(k, Job { tag, backend: Backend::Wasm, call: Call::Optimize(body) });
                    }
                }
            }
            if !self.riven_alive(rv) {
                return Ok(None);
            }
            let Some(d) = self.pool.recv(Duration::from_millis(250)) else { continue };
            let Some(i) = inflight.remove(&d.tag) else { continue };
            if let Some(b) = busy.get_mut(d.lane) {
                *b = false;
            }
            let v = jsnum::parse(&d.out?)?;
            rv.builds += v["scores"].as_array().map_or(0, |a| a.len() as u64);
            rv.fights += v["fights"].as_u64().unwrap_or(0);
            outs[i] = Some(v);
            got += 1;
            let idle = self.idle();
            self.ui.status(|s| {
                s.done = rv.fights;
                s.extra = format!("{} 个构筑", group(rv.builds));
                if n > 1 {
                    s.lanes_now = lanes;
                }
                s.idle = idle;
            });
        }
        Ok(Some(outs.into_iter().map(|o| o.unwrap()).collect()))
    }

    /// Still ours: the lease renewed every two minutes, nothing holding the machine.
    fn riven_alive(&mut self, rv: &mut RivenRun) -> bool {
        if !self.offline && rv.renewed.elapsed() > RIVEN_RENEW {
            rv.renewed = Instant::now();
            let r = self.net.post_ok(&format!("/api/appraise/{}/renew", rv.code), &json!({ "lease": rv.lease, "verifier": self.cfg.verifier }));
            if r.is_some_and(|r| r["held"] == json!(false)) {
                rv.lost = true;
                self.held = None;
            }
        }
        if !self.offline && !self.riven_took.is_zero() && rv.began.elapsed().as_secs_f64() > AHEAD_AT * self.riven_took.as_secs_f64() {
            (self.ask_ahead_fn())(&rv.lease);
        }
        !(rv.lost || self.quit() || self.held_why().is_some())
    }

    fn riven_once(&mut self, w: &Value) -> Result<bool> {
        let lease = w["lease"].as_str().unwrap_or("").to_string();
        let code = w["code"].as_str().unwrap_or("").to_string();
        self.held = Some(Held { lease: lease.clone(), code: Some(code.clone()) });
        let label = format!("{} · {} [{}]", w["weapon"].as_str().unwrap_or("?"), w["ruler"].as_str().unwrap_or(""), code);
        self.ui.log(format!("{C_GO}[开始]{C_END} 紫卡增益  {label}"));
        self.ui.status(|s| {
            s.task = format!("紫卡增益 {label}");
            s.done = 0;
            s.total = 0;
            s.extra.clear();
        });
        let mut rv = RivenRun { code: code.clone(), lease: lease.clone(), renewed: Instant::now(), began: Instant::now(), lost: false, builds: 0, fights: 0 };
        let request = w["request"].clone();
        let ceiling = (self.lanes_fn())(true).max(1);
        self.pool.ensure(ceiling);
        let mut scores = json!([]);
        let (mut done_work, mut done_fights) = (0u64, 0u64);
        let result = 'search: {
            for step in 0usize.. {
                let mut lead = request.clone();
                lead["quick_fleet"] = json!({ "lead": true, "fresh": step == 0, "scores": scores });
                let Some(r) = self.optimize_all(vec![lead.to_string()], 1, &mut rv)? else { break 'search None };
                let r = r.into_iter().next().unwrap();
                let pending = match r["pending"].as_array() {
                    Some(p) if truthy(&r["pending"]) => p.clone(),
                    _ => {
                        let mut out = r.clone();
                        out["work"] = json!(done_work + r["final_work"].as_u64().unwrap_or(0));
                        out["fights"] = json!(done_fights);
                        break 'search Some(out);
                    }
                };
                let k = (self.lanes_fn())(false).min(ceiling).min(pending.len()).max(1);
                let size = pending.len().div_ceil(k * 2).max(1);
                let bodies: Vec<String> = pending
                    .chunks(size)
                    .map(|slice| {
                        let mut b = request.clone();
                        b["quick_fleet"] = json!({ "score": slice });
                        b.to_string()
                    })
                    .collect();
                let Some(outs) = self.optimize_all(bodies, k, &mut rv)? else { break 'search None };
                if let Some(bad) = outs.iter().find(|o| o["ok"] == json!(false)) {
                    bail!("紫卡增益 {label}：{}", bad["error"].as_str().unwrap_or("a worker could not score its share"));
                }
                scores = Value::Array(outs.iter().flat_map(|o| o["scores"].as_array().cloned().unwrap_or_default()).collect());
                done_work += outs.iter().map(|o| o["work"].as_u64().unwrap_or(0)).sum::<u64>();
                done_fights += outs.iter().map(|o| o["fights"].as_u64().unwrap_or(0)).sum::<u64>();
            }
            None
        };
        let Some(r) = result else {
            if !rv.lost {
                self.give_back();
            }
            self.ui.status(|s| s.task.clear());
            self.ui.log(format!("{C_GO}[中断]{C_END} 紫卡增益 {label}{}", if rv.lost { "：任务已转给别的电脑" } else { "，任务已交还" }));
            return Ok(true);
        };
        self.riven_took = rv.began.elapsed();
        let best = (r["ok"] != json!(false))
            .then(|| r["results"].as_array().and_then(|a| a.iter().find(|x| x["mods"].as_array().is_some_and(|m| !m.is_empty())).cloned()))
            .flatten();
        let Some(best) = best else {
            self.give_back();
            self.ui.status(|s| s.task.clear());
            self.ui.log(format!("{C_BAD}[无结果]{C_END} 紫卡增益 {label}：{}", r["error"].as_str().unwrap_or("搜索没有找到构筑")));
            return Ok(true);
        };
        let work = r["work"].as_u64().unwrap_or(0);
        let build = board_payload(&best, &w["context"]);
        let score = best["kill_progress"].as_f64().unwrap_or(0.0);
        if self.offline {
            println!("{}", json!({ "kind": "riven_gain", "build": build, "score": score, "work": work, "fights": r["fights"], "builds": rv.builds }));
            self.held = None;
            return Ok(true);
        }
        let sent = self
            .net
            .post(&format!("/api/appraise/{code}/result"), &json!({ "build": build, "lease": lease, "verifier": self.cfg.verifier,
                "engine": self.rel.engine, "score": score, "work": work }))
            .map(|x| (200..300).contains(&x.status))
            .unwrap_or(false);
        if sent {
            self.held = None;
        } else {
            self.give_back();
        }
        self.ui.status(|s| {
            s.task.clear();
            if sent {
                s.session_tasks += 1;
                s.session_work += work;
            }
        });
        if sent {
            self.recent_rivens.push_back(json!({ "code": code, "at": sys::now_ms() }));
            while self.recent_rivens.len() > 50 {
                self.recent_rivens.pop_front();
            }
            let kp = best["kill_progress"].as_f64().or(best["kills"].as_f64()).unwrap_or(0.0);
            let dur = r["duration"].as_f64().unwrap_or(0.0);
            let shown = if dur > 0.0 { format!("{:.2} kpm", kp * 60.0 / dur) } else { format!("{kp:.2}") };
            self.ui.log(format!(
                "{C_OK}[完成]{C_END} 紫卡增益  {label}  最佳 {shown} · 试了 {} 个构筑 · {:.2} 分 · {:.0}s",
                group(rv.builds),
                points(work),
                rv.began.elapsed().as_secs_f64()
            ));
        } else {
            self.ui.log(format!("{C_BAD}[失败]{C_END} 紫卡增益 {label}：结果没能发出"));
        }
        Ok(true)
    }

    /// Compute one saved task (`work` as `/api/board/work` handed it out).
    pub fn replay(&mut self, w: &Value) -> Result<bool> {
        self.offline = true;
        if w["kind"] == "riven_gain" { self.riven_once(w) } else { self.board_once(w) }
    }

    /// The served engine moved on: fetch it, and run it from the next task.
    fn refresh_engine(&mut self) -> Result<()> {
        let rel = fetch_release(&self.net)?;
        if rel.engine == self.rel.engine {
            self.ui.status(|s| s.state = "服务器协议已更新，请更新 simfleet".into());
            self.ui.log(format!("{C_BAD}[过旧]{C_END} 服务器不再接受本版本的协议（{PROTOCOL}），请更新 simfleet"));
            std::thread::sleep(Duration::from_secs(60));
            return Ok(());
        }
        self.ui.log(format!("{C_INFO}[引擎]{C_END} 网站引擎已更新 {} → {}", self.rel.engine, rel.engine));
        let code = load_code(&self.net, &self.paths, &self.cfg, &self.ui, &rel)?;
        let backend = if code.native.is_some() { "native" } else { "wasm" };
        self.pool = Pool::new(code.clone());
        self.exec = Exec::new(code);
        self.ui.status(|s| {
            s.engine = rel.engine.clone();
            s.backend = backend.into();
        });
        self.rel = rel;
        Ok(())
    }

    /// ON THE WASM, look again every half hour for a published native build of
    /// the served engine (it appears some minutes after the engine does).
    fn maybe_native(&mut self) {
        if self.pool.code().native.is_some() || !self.cfg.native_engine || self.cfg.native_engine_url.is_empty()
            || self.native_checked.elapsed() < Duration::from_secs(1800)
        {
            return;
        }
        self.native_checked = Instant::now();
        let lib = self.paths.engines.join(native_lib_name(&self.rel.engine));
        if !lib.exists() && !fetch_native(&self.net, &self.cfg, &self.ui, &self.rel, &lib) {
            return;
        }
        if let Ok(code) = load_code(&self.net, &self.paths, &self.cfg, &self.ui, &self.rel) {
            if code.native.is_some() {
                self.pool = Pool::new(code.clone());
                self.exec = Exec::new(code);
                self.ui.status(|s| s.backend = "native".into());
            }
        }
    }

    /// A signed-in client claims its computer id once a run (BOARD.md §Contribution rule 5).
    fn claim_once(&mut self) {
        if self.claimed || self.net.session.is_none() {
            return;
        }
        match account::claim(&self.net, &self.cfg.verifier) {
            Ok(()) => self.claimed = true,
            Err(e) => {
                if e.to_string().contains("401") {
                    self.ui.log(format!("{C_BAD}[账号]{C_END} 登录已失效，贡献暂记在本机；运行 simfleet login 重新登录"));
                    self.net.session = None;
                    self.session_dropped = true;
                } else {
                    self.ui.log(format!("{C_BAD}[账号]{C_END} {e:#}"));
                }
                self.claimed = true;
            }
        }
    }

    /// The bottom line's numbers: this machine's, today's, the account's, the site's.
    fn refresh_stats(&mut self) {
        self.stats_at = Some(Instant::now());
        let p = self.net.post_ok("/api/board/points", &json!({ "verifier": self.cfg.verifier, "since": sys::local_midnight_utc_hour() }));
        let dev = self.net.session.as_ref().and_then(|_| account::devices(&self.net));
        let computing = self.net.get("/api/board/computing").ok().and_then(|r| r.body["computing"].as_u64());
        let (mut confirmed, mut waiting) = (0u64, 0u64);
        if !self.recent_orders.is_empty() {
            let orders: Vec<Value> = self.recent_orders.iter().cloned().collect();
            if let Some(m) = self.net.post_ok("/api/board/mine", &json!({ "verifier": self.cfg.verifier, "orders": orders })) {
                for o in m["orders"].as_array().into_iter().flatten() {
                    match o["state"].as_str() {
                        Some("confirmed") => confirmed += 1,
                        Some("waiting") | Some("checking") => waiting += 1,
                        _ => {}
                    }
                }
            }
        }
        if !self.recent_rivens.is_empty() {
            let tasks: Vec<Value> = self.recent_rivens.iter().cloned().collect();
            if let Some(m) = self.net.post_ok("/api/appraise/mine", &json!({ "verifier": self.cfg.verifier, "tasks": tasks })) {
                for o in m["tasks"].as_array().into_iter().flatten() {
                    match o["state"].as_str() {
                        Some("confirmed") => confirmed += 1,
                        Some("waiting") => waiting += 1,
                        _ => {}
                    }
                }
            }
        }
        let name = self.cfg.account.name.clone();
        self.ui.status(|s| {
            if let Some(p) = &p {
                s.device_points = p["points"].as_u64();
                s.today_points = p["today"]["points"].as_u64();
            }
            if let Some(d) = &dev {
                s.account = Some(if name.is_empty() { "已登录".into() } else { name });
                s.account_points = d["points"].as_u64();
                s.contributor_rank = d["contributor_rank"]["rank"].as_u64();
            } else {
                s.account = None;
                s.account_points = None;
            }
            s.computing_now = computing;
            s.confirmed = confirmed;
            s.waiting = waiting;
        });
    }
}

struct RivenRun {
    code: String,
    lease: String,
    renewed: Instant,
    began: Instant,
    lost: bool,
    builds: u64,
    fights: u64,
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
