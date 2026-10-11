// SPDX-License-Identifier: AGPL-3.0-or-later
//! THE DISPATCHER: every core busy, within the budget, on as many tasks as that takes.
//!
//! A task does not fill a machine on its own. A riven gain's quick descent is a
//! chain of LEAD steps on one core with the round's scoring between them, so on
//! sixteen lanes one search keeps about half of them busy (measured: 55%); a
//! board row parallelises across its runs but ends in a tail, and between tasks
//! there is a round trip to the server. So:
//!
//! - SEVERAL TASKS AT ONCE. The server lets one computer id hold a task and one
//!   asked ahead (worker/verify.js `TASKS_AHEAD`), so a GROUP — one id — runs up
//!   to two. One browser is one id and players open several to compute more;
//!   simfleet opens groups the same way, each with its own id, claimed for the
//!   same account. The server never lets two of one owner's ids, or two ids on
//!   one network, confirm each other, so this adds compute and never a witness.
//! - ONE LANE POOL, OLDEST TASK FIRST. A free lane takes the next job of the
//!   oldest task that has one; what that task cannot use goes to the next
//!   (backfill), and a lane never waits on a task.
//! - A RIVEN GAIN'S LEADER LIVES ON ITS OWN LANE: the search state is in that
//!   lane's wasm instance between steps, so the lane takes no other task's wasm
//!   work while the search lives (a native board piece touches no wasm, and may).
//! - THE NETWORK IS NEVER WAITED ON: asks, renewals and answers run on their own
//!   threads. A plain ask (not `ahead`) makes the server take back what that id
//!   holds, so it is only sent by an id that holds nothing and is sending nothing.
//! - HOW MANY GROUPS IS MEASURED, not guessed: every half minute, a machine below
//!   85% utilisation whose groups all hold work, while the server still has work,
//!   opens one more (up to a quarter of its cores); one saturated with a task
//!   starved of lanes stops refilling its newest group.
//!
//! Every computation is the page's own, step for step (work.rs), so a result is
//! bit for bit what a browser sends, however the lanes were shared.

use crate::config::{Config, Paths};
use crate::engine::{Backend, Call, Code, Done, Exec, Job, Pool};
use crate::net::Net;
use crate::ui::{group, points, Ctl, GroupView, Ranked, TaskLine, Ui, C_BAD, C_END, C_GO, C_INFO, C_OK};
use crate::work::{
    board_payload, call, fetch_release, load_code, truthy, Release, RowScore, LEASE_SAFE,
    PIECE_MS, PROTOCOL, RIVEN_RENEW, STATS_EVERY,
};
use crate::{account, jsnum, sys};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A held task given back when nothing of it has run for this long (its lanes
/// went to older tasks, or the budget shrank under it).
const STARVED: Duration = Duration::from_secs(180);
/// How often the group count is reconsidered, and the bands it moves on.
const CONTROL_EVERY: Duration = Duration::from_secs(30);
const OPEN_BELOW: f64 = 0.85;
const CLOSE_ABOVE: f64 = 0.97;
/// A group the server told "nothing for you" asks again after this.
const NOTHING_WAIT: Duration = Duration::from_secs(10);

// ---- one task's computation, as jobs ------------------------------------------

/// A BOARD ROW (69-board-work.js `measureRow`): its runs as pieces of about
/// `PIECE_MS`, a shard per run, folded strictly in run order, then scored.
struct BoardRun {
    req_s: String,
    ruler: String,
    runs: u32,
    cursor: u32,
    folded: u32,
    est: Option<f64>,
    ready: BTreeMap<u32, Value>,
    inflight: HashMap<u64, (u32, u32)>,
    acc: String,
    spent: f64,
}

impl BoardRun {
    fn new(req: &Value, ruler: &str) -> BoardRun {
        BoardRun {
            req_s: req.to_string(),
            ruler: ruler.to_string(),
            runs: req["runs"].as_u64().unwrap_or(0) as u32,
            cursor: 0,
            folded: 0,
            est: None,
            ready: BTreeMap::new(),
            inflight: HashMap::new(),
            acc: "null".into(),
            spent: 0.0,
        }
    }
    fn job(&mut self, tag: u64, lanes: usize) -> Option<Call> {
        if self.cursor >= self.runs {
            return None;
        }
        let left = self.runs - self.cursor;
        let want = self.est.map(|e| (PIECE_MS / e).round().max(1.0) as u32).unwrap_or(1);
        let count = want.min(1000).min(left.div_ceil(lanes.max(1) as u32)).min(left).max(1);
        let (from, req) = (self.cursor, &self.req_s);
        self.inflight.insert(tag, (from, count));
        self.cursor += count;
        Some(Call::Api("/api/board/runs", format!(r#"{{"request":{req},"from":{from},"count":{count}}}"#)))
    }
    fn done(&mut self, d: Done, exec: &mut Exec, backend: Backend) -> Result<Option<RowScore>> {
        let Some((from, count)) = self.inflight.remove(&d.tag) else { return Ok(None) };
        let v = jsnum::parse(&d.out?)?;
        let shards = v["shards"]
            .as_array()
            .filter(|s| s.len() == count as usize)
            .with_context(|| format!("/api/board/runs: {}", v["error"].as_str().unwrap_or("wrong shard count")))?;
        self.spent += d.ms;
        let per = d.ms / count as f64;
        self.est = Some(self.est.map_or(per, |e| e * 0.7 + per * 0.3));
        for (i, s) in shards.iter().enumerate() {
            self.ready.insert(from + i as u32, s.clone());
        }
        let mut pieces = Vec::new();
        while let Some(s) = self.ready.remove(&(self.folded + pieces.len() as u32)) {
            pieces.push(s);
        }
        if !pieces.is_empty() {
            let n = pieces.len() as u32;
            let body = format!(r#"{{"request":{},"acc":{},"pieces":{}}}"#, self.req_s, self.acc, Value::Array(pieces));
            // The folds count in `compute_ms` too, as the page counts them.
            let t = Instant::now();
            self.acc = call(exec, backend, "/api/board/fold", body)?["acc"].to_string();
            self.spent += t.elapsed().as_secs_f64() * 1000.0;
            self.folded += n;
        }
        if self.folded < self.runs {
            return Ok(None);
        }
        let t = Instant::now();
        let body = format!(r#"{{"ruler":{},"request":{},"acc":{}}}"#, json!(self.ruler), self.req_s, self.acc);
        let s = call(exec, backend, "/api/board/score", body)?;
        self.spent += t.elapsed().as_secs_f64() * 1000.0;
        Ok(Some(RowScore {
            score: s["score"].as_f64().context("score")?,
            metric: s["metric"].as_str().unwrap_or("").to_string(),
            work: s["work"].as_u64().unwrap_or(0),
            compute_ms: self.spent.round() as u64,
            counts: s["counts"].clone(),
        }))
    }
}

/// A RIVEN GAIN (08-checkpoint-api.js `quickFleet`): lead steps on the leader's
/// instance, each round's builds scored in slices — two a lane — on any lane.
struct Riven {
    request: Value,
    leader: Option<usize>,
    step: usize,
    scores: Value,
    lead_tag: Option<u64>,
    slices: VecDeque<(usize, String)>,
    outs: Vec<Option<Value>>,
    inflight: HashMap<u64, usize>,
    want: usize,
    done_work: u64,
    done_fights: u64,
    builds: u64,
    fights: u64,
    /// Every call's own time, summed over the lanes.
    cpu_ms: f64,
}

impl Riven {
    fn new(request: Value) -> Riven {
        Riven {
            request,
            leader: None,
            step: 0,
            scores: json!([]),
            lead_tag: None,
            slices: VecDeque::new(),
            outs: Vec::new(),
            inflight: HashMap::new(),
            want: 0,
            done_work: 0,
            done_fights: 0,
            builds: 0,
            fights: 0,
            cpu_ms: 0.0,
        }
    }
    fn job(&mut self, lane: usize, tag: u64) -> Option<Call> {
        if self.want == 0 {
            if self.lead_tag.is_some() || self.leader != Some(lane) {
                return None;
            }
            let mut lead = self.request.clone();
            lead["quick_fleet"] = json!({ "lead": true, "fresh": self.step == 0, "scores": self.scores });
            self.lead_tag = Some(tag);
            return Some(Call::Optimize(lead.to_string()));
        }
        let (i, body) = self.slices.pop_front()?;
        self.inflight.insert(tag, i);
        Some(Call::Optimize(body))
    }
    /// `Some` with the search's result once its lead says there is nothing pending.
    fn done(&mut self, d: Done, lanes: usize) -> Result<Option<Value>> {
        self.cpu_ms += d.ms;
        if self.lead_tag == Some(d.tag) {
            self.lead_tag = None;
            let r = jsnum::parse(&d.out?)?;
            let pending = match r["pending"].as_array() {
                Some(p) if truthy(&r["pending"]) => p.clone(),
                _ => {
                    let mut out = r.clone();
                    out["work"] = json!(self.done_work + r["final_work"].as_u64().unwrap_or(0));
                    out["fights"] = json!(self.done_fights);
                    return Ok(Some(out));
                }
            };
            // THE PAGE'S SLICING: two a lane, over the lanes it may use now.
            let k = lanes.min(pending.len()).max(1);
            let size = pending.len().div_ceil(k * 2).max(1);
            self.slices = pending
                .chunks(size)
                .enumerate()
                .map(|(i, slice)| {
                    let mut b = self.request.clone();
                    b["quick_fleet"] = json!({ "score": slice });
                    (i, b.to_string())
                })
                .collect();
            self.want = self.slices.len();
            self.outs = vec![None; self.want];
            if self.want == 0 {
                self.scores = json!([]);
                self.step += 1;
            }
            return Ok(None);
        }
        let Some(i) = self.inflight.remove(&d.tag) else { return Ok(None) };
        let v = jsnum::parse(&d.out?)?;
        if v["ok"] == json!(false) {
            bail!("{}", v["error"].as_str().unwrap_or("a worker could not score its share"));
        }
        self.builds += v["scores"].as_array().map_or(0, |a| a.len() as u64);
        self.fights += v["fights"].as_u64().unwrap_or(0);
        self.outs[i] = Some(v);
        if self.outs.iter().all(Option::is_some) {
            let outs: Vec<Value> = std::mem::take(&mut self.outs).into_iter().flatten().collect();
            self.scores = Value::Array(outs.iter().flat_map(|o| o["scores"].as_array().cloned().unwrap_or_default()).collect());
            self.done_work += outs.iter().map(|o| o["work"].as_u64().unwrap_or(0)).sum::<u64>();
            self.done_fights += outs.iter().map(|o| o["fights"].as_u64().unwrap_or(0)).sum::<u64>();
            self.want = 0;
            self.step += 1;
        }
        Ok(None)
    }
}

enum Runner {
    Board(BoardRun),
    Riven(Riven),
}

/// A held task.
struct Active {
    id: u64,
    group: usize,
    verifier: String,
    lease: String,
    code: Option<String>,
    label: String,
    w: Value,
    runner: Runner,
    began: Instant,
    last_ran: Instant,
    renewed: Instant,
    renewing: Option<JoinHandle<Option<Value>>>,
    /// The lanes its jobs ran on, reported with the answer.
    lanes_used: HashSet<usize>,
}

impl Active {
    fn riven(&self) -> bool {
        matches!(self.runner, Runner::Riven(_))
    }
    fn line(&self, lanes: usize) -> TaskLine {
        let mut l = match &self.runner {
            Runner::Board(b) => TaskLine { done: b.folded as u64, total: b.runs as u64, ..Default::default() },
            Runner::Riven(r) => TaskLine { riven: true, done: r.fights, extra: format!("{} 个构筑", group(r.builds)), ..Default::default() },
        };
        l.group = self.group;
        l.label = self.label.clone();
        l.secs = self.began.elapsed().as_secs();
        l.lanes = lanes;
        l
    }
}

/// A computer id and what it is asking for.
struct Group {
    verifier: String,
    asking: Option<(bool, JoinHandle<Option<Value>>)>,
    next_ask: Instant,
    next_ahead: Instant,
    claimed: bool,
    /// Answers the server took from it this run, and their work.
    done: u64,
    work: u64,
}

/// An answer on its way to the server.
struct Sending {
    group: usize,
    handle: JoinHandle<bool>,
    log_ok: String,
    work: u64,
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
    tasks: Vec<Active>,
    groups: Vec<Group>,
    /// How many groups may take new work now.
    open: usize,
    sending: Vec<Sending>,
    tag_task: HashMap<u64, u64>,
    /// The task whose job each lane runs.
    lane_job: Vec<Option<u64>>,
    next_id: u64,
    tasks_ahead: usize,
    stale: bool,
    banned_until: Option<String>,
    recent: Vec<(String, bool, Value)>,
    stats_at: Option<Instant>,
    native_checked: Instant,
    control_at: Instant,
    control_busy: f64,
    control_cap: f64,
    meter: Meter,
    pub session_dropped: bool,
    pub offline: bool,
}

impl Fleet {
    pub fn new(net: Net, cfg: Config, paths: Paths, ctl: Arc<Ctl>, ui: Arc<Ui>, rel: Release, code: Arc<Code>) -> Fleet {
        let backend = crate::work::backend_label(&code);
        ui.status(|s| {
            s.engine = rel.engine.clone();
            s.backend = backend.into();
        });
        let mut f = Fleet {
            net,
            cfg,
            ctl,
            ui,
            paths,
            pool: Pool::new(code.clone()),
            exec: Exec::new(code),
            rel,
            tasks: Vec::new(),
            groups: Vec::new(),
            open: 1,
            sending: Vec::new(),
            tag_task: HashMap::new(),
            lane_job: Vec::new(),
            next_id: 0,
            tasks_ahead: 1,
            stale: false,
            banned_until: None,
            recent: Vec::new(),
            stats_at: None,
            native_checked: Instant::now(),
            control_at: Instant::now(),
            control_busy: 0.0,
            control_cap: 0.0,
            meter: Meter::new(),
            session_dropped: false,
            offline: false,
        };
        if f.cfg.slots > 0 {
            f.open = f.cfg.slots;
        }
        f.ensure_groups(f.open);
        f
    }

    /// Up to `n` groups, each with an id: the config's first, then its extras —
    /// made here when a new group opens, and saved.
    fn ensure_groups(&mut self, n: usize) {
        while self.groups.len() < n {
            let i = self.groups.len();
            let verifier = if i == 0 {
                self.cfg.verifier.clone()
            } else {
                while self.cfg.extra_verifiers.len() < i {
                    self.cfg.extra_verifiers.push(sys::random_hex(12));
                    let _ = self.paths.save(&self.cfg);
                }
                self.cfg.extra_verifiers[i - 1].clone()
            };
            let now = Instant::now();
            self.groups.push(Group { verifier, asking: None, next_ask: now, next_ahead: now, claimed: false, done: 0, work: 0 });
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
    /// The lanes the budget allows now.
    fn lanes(&self) -> usize {
        if self.idle() {
            self.ctl.idle_cores.load(Ordering::Relaxed).max(1)
        } else {
            self.ctl.active_cores.load(Ordering::Relaxed).max(1)
        }
    }
    fn max_groups(&self) -> usize {
        if self.cfg.slots > 0 {
            self.cfg.slots
        } else {
            self.ctl.max_cores.div_ceil(4).max(1)
        }
    }
    fn backend(&self) -> Backend {
        if self.pool.code().native.is_some() { Backend::Native } else { Backend::Wasm }
    }

    fn ask_body(&self, verifier: &str, ahead: bool) -> Value {
        // WHAT THIS ID CAN GIVE: its share of the lanes, as one browser among several says its own.
        let lanes = (self.lanes() / self.open.max(1)).max(1);
        let mut b = json!({
            "verifier": verifier,
            "engine": self.rel.engine,
            "protocol": PROTOCOL,
            "consent": { "v": self.cfg.consent_v, "at": self.cfg.consent_at },
            "lanes": lanes,
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

    /// Give a task back, in the background.
    fn drop_task(&mut self, id: u64, why: &str, give_back: bool) {
        let Some(i) = self.tasks.iter().position(|t| t.id == id) else { return };
        let t = self.tasks.remove(i);
        self.tag_task.retain(|_, v| *v != id);
        if give_back && !self.offline {
            let (net, v, lease, code) = (self.net.clone(), t.verifier.clone(), t.lease.clone(), t.code.clone());
            std::thread::spawn(move || Self::release_lease(&net, &v, &lease, code.as_deref()));
        }
        let kind = if t.riven() { "紫卡增益" } else { "榜单" };
        self.ui.glog(t.group, format!("{C_GO}[中断]{C_END} {kind} {}{why}", t.label));
    }

    fn drop_all(&mut self, why: &str) {
        let ids: Vec<u64> = self.tasks.iter().map(|t| t.id).collect();
        for id in ids {
            self.drop_task(id, why, true);
        }
        // An ask still on its way may come back holding a lease: give it back.
        for g in &mut self.groups {
            if let Some((_, h)) = g.asking.take() {
                if let Ok(Some(r)) = h.join() {
                    if let Some(lease) = r["work"]["lease"].as_str() {
                        let code = (r["work"]["kind"] == "riven_gain").then(|| r["work"]["code"].as_str().unwrap_or("").to_string());
                        Self::release_lease(&self.net, &g.verifier, lease, code.as_deref());
                    }
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        self.drop_all("，任务已交还");
        for s in self.sending.drain(..) {
            let _ = s.handle.join();
        }
    }

    // ---- taking work in -------------------------------------------------------

    /// Answers to asks that came back, and new asks where a group has room.
    fn asks(&mut self) {
        let lanes = self.lanes();
        let now = Instant::now();
        for gi in 0..self.groups.len() {
            if let Some((ahead, h)) = self.groups[gi].asking.take() {
                if !h.is_finished() {
                    self.groups[gi].asking = Some((ahead, h));
                } else {
                    let reply = h.join().ok().flatten();
                    self.take_reply(gi, ahead, reply);
                }
            }
            if self.groups[gi].asking.is_some() || self.offline || self.stale || self.banned_until.is_some() {
                continue;
            }
            let held = self.tasks.iter().filter(|t| t.group == gi).count();
            let sending = self.sending.iter().filter(|s| s.group == gi).count();
            let open = gi < self.open;
            let g = &self.groups[gi];
            // A PLAIN ASK only from an id holding nothing and sending nothing;
            // AN AHEAD ASK from one holding exactly the task it computes.
            let plain = open && held == 0 && sending == 0 && now >= g.next_ask;
            let ahead = open && held == 1 && sending == 0 && self.tasks_ahead > 0 && lanes >= 2 && now >= g.next_ahead;
            if plain || ahead {
                if !g.claimed && self.net.session.is_some() {
                    self.claim(gi);
                }
                let (net, body) = (self.net.clone(), self.ask_body(&self.groups[gi].verifier, ahead));
                self.groups[gi].asking = Some((ahead, std::thread::spawn(move || net.post_ok("/api/board/work", &body))));
            }
        }
    }

    fn take_reply(&mut self, gi: usize, ahead: bool, reply: Option<Value>) {
        let wait = Instant::now() + NOTHING_WAIT;
        let Some(r) = reply else {
            self.ui.status(|s| s.state = "连不上服务器，稍后重试".into());
            self.groups[gi].next_ask = wait;
            self.groups[gi].next_ahead = wait;
            return;
        };
        if let Some(n) = r["ahead"].as_u64() {
            self.tasks_ahead = n as usize;
        }
        if r["stale"] == json!(true) {
            self.stale = true;
            return;
        }
        if r["banned"] == json!(true) {
            let until = r["until"].as_str().unwrap_or("?").to_string();
            if self.banned_until.is_none() {
                self.ui.log(format!("{C_BAD}[冷却]{C_END} 服务器复核时与本机结果不一致，暂停派活至 {until}"));
            }
            self.banned_until = Some(until);
            return;
        }
        let w = r["work"].clone();
        if !w.is_object() {
            if ahead {
                self.groups[gi].next_ahead = wait;
            } else {
                self.groups[gi].next_ask = wait;
            }
            return;
        }
        if self.quit() || self.held_why().is_some() {
            let code = (w["kind"] == "riven_gain").then(|| w["code"].as_str().unwrap_or("").to_string());
            Self::release_lease(&self.net, &self.groups[gi].verifier, w["lease"].as_str().unwrap_or(""), code.as_deref());
            return;
        }
        let verifier = self.groups[gi].verifier.clone();
        self.start(gi, verifier, w);
    }

    /// A leased task becomes a held one: a board row asks the engine for its fight.
    fn start(&mut self, gi: usize, verifier: String, w: Value) -> bool {
        let lease = w["lease"].as_str().unwrap_or("").to_string();
        let now = Instant::now();
        self.next_id += 1;
        let id = self.next_id;
        let (runner, label, code) = if w["kind"] == "riven_gain" {
            let code = w["code"].as_str().unwrap_or("").to_string();
            let label = format!("{} · {} [{}]", w["weapon"].as_str().unwrap_or("?"), w["ruler"].as_str().unwrap_or(""), code);
            self.ui.glog(gi, format!("{C_GO}[开始]{C_END} 紫卡增益  {label}"));
            (Runner::Riven(Riven::new(w["request"].clone())), label, Some(code))
        } else {
            let (ruler, mode) = (w["ruler"].as_str().unwrap_or("").to_string(), w["mode"].as_str().unwrap_or("").to_string());
            let label = format!("{} · {ruler} · {mode}", w["record"]["weapon"].as_str().unwrap_or("?"));
            let backend = self.backend();
            match call(&mut self.exec, backend, "/api/board/order", json!({ "record": w["record"], "ruler": ruler, "mode": mode }).to_string()) {
                Ok(o) => {
                    let runs = o["request"]["runs"].as_u64().unwrap_or(0);
                    self.ui.glog(gi, format!("{C_GO}[开始]{C_END} 榜单  {label}  {runs} 次模拟"));
                    (Runner::Board(BoardRun::new(&o["request"], &ruler)), label, None)
                }
                Err(e) => {
                    if !self.offline {
                        Self::release_lease(&self.net, &verifier, &lease, None);
                    }
                    self.ui.glog(gi, format!("{C_BAD}[跳过]{C_END} 榜单 {label}：{e:#}"));
                    return false;
                }
            }
        };
        self.tasks.push(Active {
            id,
            group: gi,
            verifier,
            lease,
            code,
            label,
            w,
            runner,
            began: now,
            last_ran: now,
            renewed: now,
            renewing: None,
            lanes_used: HashSet::new(),
        });
        true
    }

    fn claim(&mut self, gi: usize) {
        self.groups[gi].claimed = true;
        match account::claim_as(&self.net, &self.groups[gi].verifier, gi) {
            Ok(()) => {}
            Err(e) if e.to_string().contains("401") => {
                self.ui.log(format!("{C_BAD}[账号]{C_END} 登录已失效，贡献暂记在本机；运行 simfleet login 重新登录"));
                self.net.session = None;
                self.session_dropped = true;
            }
            Err(e) => self.ui.log(format!("{C_BAD}[账号]{C_END} {e:#}")),
        }
    }

    // ---- the lanes --------------------------------------------------------------

    /// Fill every free lane: the oldest task with a job for it, honouring leaders.
    fn dispatch(&mut self) {
        let lanes = self.lanes();
        self.pool.ensure(lanes);
        let native = self.pool.code().native.is_some();
        let backend = self.backend();
        // Leaders: a riven gain without one takes the lowest free lane in budget.
        for i in 0..self.tasks.len() {
            let taken: Vec<usize> = self.tasks.iter().filter_map(|t| match &t.runner { Runner::Riven(r) => r.leader, _ => None }).collect();
            if let Runner::Riven(r) = &mut self.tasks[i].runner {
                if r.leader.is_none() {
                    r.leader = (0..lanes).find(|l| !taken.contains(l));
                }
            }
        }
        let leader_of: HashMap<usize, u64> = self
            .tasks
            .iter()
            .filter_map(|t| match &t.runner { Runner::Riven(r) => r.leader.map(|l| (l, t.id)), _ => None })
            .collect();
        for lane in 0..lanes {
            if self.pool.pending(lane) > 0 {
                continue;
            }
            let owner = leader_of.get(&lane).copied();
            for ti in 0..self.tasks.len() {
                let t = &mut self.tasks[ti];
                let mine = owner == Some(t.id);
                let tag = self.pool.next_tag();
                let job = match &mut t.runner {
                    // A board piece on a leader's lane only when it needs no wasm there.
                    Runner::Board(b) if owner.is_none() || native => b.job(tag, lanes).map(|c| (backend, c)),
                    Runner::Riven(r) if owner.is_none() || mine => r.job(lane, tag).map(|c| (Backend::Wasm, c)),
                    _ => None,
                };
                if let Some((backend, call)) = job {
                    t.last_ran = Instant::now();
                    t.lanes_used.insert(lane);
                    if self.lane_job.len() <= lane {
                        self.lane_job.resize(lane + 1, None);
                    }
                    self.lane_job[lane] = Some(t.id);
                    self.tag_task.insert(tag, t.id);
                    self.pool.send(lane, Job { tag, backend, call });
                    break;
                }
            }
        }
    }

    /// One finished job, into its task; a task that is done is answered.
    fn absorb(&mut self, d: Done) {
        if let Some(l) = self.lane_job.get_mut(d.lane) {
            *l = None;
        }
        let Some(id) = self.tag_task.remove(&d.tag) else { return };
        let Some(ti) = self.tasks.iter().position(|t| t.id == id) else { return };
        let lanes = self.lanes();
        let backend = self.backend();
        let t = &mut self.tasks[ti];
        t.last_ran = Instant::now();
        let outcome = match &mut t.runner {
            Runner::Board(b) => b.done(d, &mut self.exec, backend).map(|s| s.map(Finished::Board)),
            Runner::Riven(r) => r.done(d, lanes).map(|s| s.map(Finished::Riven)),
        };
        match outcome {
            Ok(None) => {}
            Ok(Some(f)) => {
                let t = self.tasks.remove(ti);
                self.tag_task.retain(|_, v| *v != id);
                self.answer(t, f);
            }
            Err(e) => {
                let (label, gi) = (self.tasks[ti].label.clone(), self.tasks[ti].group);
                self.ui.glog(gi, format!("{C_BAD}[失败]{C_END} {label}：{e:#}"));
                self.drop_task(id, "，任务已交还", true);
            }
        }
    }

    /// The answer, sent on its own thread (printed instead, offline).
    fn answer(&mut self, t: Active, f: Finished) {
        let secs = t.began.elapsed().as_secs_f64();
        let lanes = t.lanes_used.len().max(1);
        let (path, body, log_ok, work, recent) = match f {
            Finished::Board(s) => (
                "/api/board/verify".to_string(),
                {
                    // `counts` and `lanes`: what the row cost, kept beside the answer to
                    // fit what a point is worth (never credited; 69-board-work.js).
                    let mut b = json!({ "lease": t.lease, "verifier": t.verifier, "engine": self.rel.engine,
                        "score": s.score, "metric": s.metric, "work": s.work, "compute_ms": s.compute_ms, "lanes": lanes });
                    if s.counts.is_object() {
                        b["counts"] = s.counts.clone();
                    }
                    b
                },
                format!("{C_OK}[完成]{C_END} 榜单  {}  = {:.4} {} · {:.2} 分 · {secs:.1}s", t.label, s.score, s.metric, points(s.work)),
                s.work,
                Some((false, json!({ "identity": t.w["identity"], "ruler": t.w["ruler"], "mode": t.w["mode"] }))),
            ),
            Finished::Riven(r) => {
                let best = (r["ok"] != json!(false))
                    .then(|| r["results"].as_array().and_then(|a| a.iter().find(|x| x["mods"].as_array().is_some_and(|m| !m.is_empty())).cloned()))
                    .flatten();
                let Some(best) = best else {
                    if !self.offline {
                        let (net, v, lease, code) = (self.net.clone(), t.verifier.clone(), t.lease.clone(), t.code.clone());
                        std::thread::spawn(move || Self::release_lease(&net, &v, &lease, code.as_deref()));
                    }
                    self.ui.glog(t.group, format!("{C_BAD}[无结果]{C_END} 紫卡增益 {}：{}", t.label, r["error"].as_str().unwrap_or("搜索没有找到构筑")));
                    return;
                };
                let work = r["work"].as_u64().unwrap_or(0);
                let build = board_payload(&best, &t.w["context"]);
                let score = best["kill_progress"].as_f64().unwrap_or(0.0);
                let (builds, kp, cost) = match &t.runner {
                    Runner::Riven(rv) => (
                        rv.builds,
                        best["kill_progress"].as_f64().or(best["kills"].as_f64()).unwrap_or(0.0),
                        (rv.cpu_ms.round() as u64, json!({ "builds": rv.builds, "fights": rv.done_fights, "steps": rv.step })),
                    ),
                    _ => (0, 0.0, (0, json!({}))),
                };
                let dur = r["duration"].as_f64().unwrap_or(0.0);
                let shown = if dur > 0.0 { format!("{:.2} kpm", kp * 60.0 / dur) } else { format!("{kp:.2}") };
                if self.offline {
                    println!("{}", json!({ "kind": "riven_gain", "build": build, "score": score, "work": work, "fights": r["fights"], "builds": builds }));
                }
                (
                    format!("/api/appraise/{}/result", t.code.clone().unwrap_or_default()),
                    // …and what the search cost, as the page sends it (never credited).
                    json!({ "build": build, "lease": t.lease, "verifier": t.verifier, "engine": self.rel.engine, "score": score, "work": work,
                            "compute_ms": cost.0, "lanes": lanes, "counts": cost.1 }),
                    format!("{C_OK}[完成]{C_END} 紫卡增益  {}  最佳 {shown} · 试了 {} 个构筑 · {:.2} 分 · {secs:.0}s", t.label, group(builds), points(work)),
                    work,
                    Some((true, json!({ "code": t.code, "at": sys::now_ms() }))),
                )
            }
        };
        if self.offline {
            if !t.riven() {
                println!("{}", json!({ "kind": "board", "score": body["score"], "metric": body["metric"], "work": body["work"], "compute_ms": body["compute_ms"] }));
            }
            self.ui.glog(t.group, log_ok);
            return;
        }
        if let Some((kind, v)) = recent {
            self.recent.push((t.verifier.clone(), kind, v));
            if self.recent.len() > 100 {
                self.recent.remove(0);
            }
        }
        let net = self.net.clone();
        let handle = std::thread::spawn(move || net.post(&path, &body).map(|r| (200..300).contains(&r.status)).unwrap_or(false));
        self.sending.push(Sending { group: t.group, handle, log_ok, work });
    }

    /// Answers that arrived, and the riven gains' leases kept alive.
    fn housekeeping(&mut self) {
        let mut i = 0;
        while i < self.sending.len() {
            if self.sending[i].handle.is_finished() {
                let s = self.sending.remove(i);
                let ok = s.handle.join().unwrap_or(false);
                if ok {
                    self.ui.glog(s.group, s.log_ok);
                    if let Some(g) = self.groups.get_mut(s.group) {
                        g.done += 1;
                        g.work += s.work;
                    }
                    let now = Instant::now();
                    self.ui.status(|st| {
                        st.session_tasks += 1;
                        st.session_work += s.work;
                        st.answered.push_back((now, s.work));
                        while st.answered.front().is_some_and(|a| now.duration_since(a.0) > Duration::from_secs(900)) {
                            st.answered.pop_front();
                        }
                    });
                } else {
                    self.ui.glog(s.group, format!("{C_BAD}[失败]{C_END} 结果没能发出"));
                }
            } else {
                i += 1;
            }
        }
        let now = Instant::now();
        let mut lost = Vec::new();
        let mut late = Vec::new();
        for t in &mut self.tasks {
            if let Some(h) = t.renewing.take() {
                if h.is_finished() {
                    if h.join().ok().flatten().is_some_and(|r| r["held"] == json!(false)) {
                        lost.push(t.id);
                    }
                } else {
                    t.renewing = Some(h);
                }
            }
            if t.riven() && !self.offline && t.renewing.is_none() && now.duration_since(t.renewed) > RIVEN_RENEW {
                t.renewed = now;
                let (net, path, body) = (
                    self.net.clone(),
                    format!("/api/appraise/{}/renew", t.code.clone().unwrap_or_default()),
                    json!({ "lease": t.lease, "verifier": t.verifier }),
                );
                t.renewing = Some(std::thread::spawn(move || net.post_ok(&path, &body)));
            }
            let quiet = now.duration_since(t.last_ran) > STARVED && !self.tag_task.values().any(|v| *v == t.id);
            if (!t.riven() && now.duration_since(t.began) > LEASE_SAFE) || quiet {
                late.push(t.id);
            }
        }
        for id in lost {
            self.drop_task(id, "：任务已转给别的电脑", false);
        }
        for id in late {
            self.drop_task(id, "：太久没轮到算力，任务已交还", true);
        }
    }

    /// THE GROUP COUNT, from what the meter saw over the last half minute.
    fn control(&mut self) {
        if self.cfg.slots > 0 || self.control_at.elapsed() < CONTROL_EVERY {
            return;
        }
        let (busy, cap) = (self.meter.busy_total - self.control_busy, self.meter.cap_total - self.control_cap);
        self.control_at = Instant::now();
        self.control_busy = self.meter.busy_total;
        self.control_cap = self.meter.cap_total;
        if cap < 1000.0 {
            return;
        }
        let util = busy / cap;
        let full = (0..self.open).all(|g| self.tasks.iter().any(|t| t.group == g));
        let now = Instant::now();
        let work_left = (0..self.open).all(|g| self.groups[g].next_ask <= now);
        if util < OPEN_BELOW && full && work_left && self.open < self.max_groups() {
            self.open += 1;
            self.ensure_groups(self.open);
            self.ui.log(format!("{C_INFO}[调度]{C_END} 核心利用率 {:.0}%，加开第 {} 路", util * 100.0, self.open));
        } else if util > CLOSE_ABOVE && self.open > 1 {
            let starving = self.tasks.iter().any(|t| now.duration_since(t.last_ran) > Duration::from_secs(20));
            if starving {
                self.open -= 1;
                self.ui.log(format!("{C_INFO}[调度]{C_END} 核心已满，第 {} 路做完手头任务后不再接活", self.open + 1));
            }
        }
    }

    fn show(&mut self) {
        let (lanes, idle, open) = (self.lanes(), self.idle(), self.open);
        let running = |id: u64| self.lane_job.iter().filter(|l| **l == Some(id)).count();
        let lines: Vec<TaskLine> = self.tasks.iter().map(|t| t.line(running(t.id))).collect();
        let group_of: HashMap<u64, usize> = self.tasks.iter().map(|t| (t.id, t.group)).collect();
        let lane_map: Vec<Option<usize>> =
            (0..lanes).map(|l| self.lane_job.get(l).copied().flatten().and_then(|id| group_of.get(&id).copied())).collect();
        let now = Instant::now();
        let views: Vec<GroupView> = self
            .groups
            .iter()
            .enumerate()
            .map(|(gi, g)| {
                let held = self.tasks.iter().filter(|t| t.group == gi).count();
                let state = if held > 1 {
                    "计算中（含预取）"
                } else if held == 1 {
                    "计算中"
                } else if self.sending.iter().any(|s| s.group == gi) {
                    "提交结果"
                } else if g.asking.is_some() {
                    "取任务"
                } else if gi >= open {
                    "暂不接活"
                } else if g.next_ask > now {
                    "暂无任务"
                } else {
                    "等待"
                };
                GroupView { id: g.verifier.get(..8).unwrap_or(&g.verifier).to_string(), state: state.into(), open: gi < open, tasks: g.done, work: g.work }
            })
            .collect();
        let state = if let Some(u) = &self.banned_until {
            format!("冷却中，至 {u}")
        } else if self.tasks.is_empty() {
            "等待任务".to_string()
        } else {
            "计算中".to_string()
        };
        self.ui.status(|s| {
            s.state = state;
            s.lanes_now = lanes;
            s.idle = idle;
            s.tasks = lines;
            s.groups = open;
            s.group_views = views;
            s.lane_map = lane_map;
        });
    }

    fn measure(&mut self) {
        let lanes = if self.held_why().is_some() { 0 } else { self.lanes() };
        let (window, total) = self.meter.read(self.pool.busy_ms, lanes, self.tasks.len());
        self.ui.status(|s| {
            s.utilisation = window;
            if total.is_some() {
                s.utilisation_total = total;
            }
        });
    }

    /// THE LOOP.
    pub fn run(&mut self) {
        self.refresh_stats();
        let mut shown = Instant::now();
        while !self.quit() {
            if let Some(why) = self.held_why() {
                if !self.tasks.is_empty() {
                    self.drop_all("，任务已交还");
                }
                self.ui.status(|s| {
                    s.state = why.into();
                    s.tasks.clear();
                    s.lanes_now = 0;
                });
                self.measure();
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            if self.stale {
                self.drop_all("，网站引擎已更新");
                if let Err(e) = self.refresh_engine() {
                    self.ui.log(format!("{C_BAD}[出错]{C_END} {e:#}"));
                    std::thread::sleep(Duration::from_secs(10));
                }
                continue;
            }
            self.asks();
            self.dispatch();
            if let Some(d) = self.pool.recv(Duration::from_millis(50)) {
                self.absorb(d);
                while let Some(d) = self.pool.recv(Duration::ZERO) {
                    self.absorb(d);
                }
                self.dispatch();
            }
            self.housekeeping();
            if shown.elapsed() > Duration::from_millis(250) {
                shown = Instant::now();
                self.show();
                self.measure();
                self.control();
            }
            if self.tasks.is_empty() && self.sending.is_empty() {
                if self.stats_at.is_none_or(|t| t.elapsed() > STATS_EVERY) {
                    self.refresh_stats();
                }
                self.maybe_native();
            } else if self.stats_at.is_none_or(|t| t.elapsed() > STATS_EVERY * 2) {
                self.refresh_stats();
            }
        }
        self.shutdown();
    }

    /// Compute one saved task (`work` as `/api/board/work` handed it out), offline.
    pub fn replay(&mut self, w: &Value) -> Result<()> {
        self.offline = true;
        let v = self.groups[0].verifier.clone();
        if !self.start(0, v, w.clone()) {
            bail!("task did not start");
        }
        while !self.tasks.is_empty() {
            self.dispatch();
            if let Some(d) = self.pool.recv(Duration::from_millis(100)) {
                self.absorb(d);
            }
        }
        Ok(())
    }

    /// What the meter saw, for the closing line.
    pub fn utilisation_summary(&self) -> Option<String> {
        let m = &self.meter;
        if m.cap_total < 1000.0 {
            return None;
        }
        let share = |x: f64| 100.0 * x / m.wall_total.max(1e-9);
        Some(format!(
            "核心利用率 {:.0}%（引擎计算时间 ÷ 预算允许的核心时间）；没有任务的时间 {:.0}%，一个任务 {:.0}%，多个任务并行 {:.0}%；最多开到 {} 路",
            100.0 * m.busy_total / m.cap_total,
            share(m.wall_by_tasks[0]),
            share(m.wall_by_tasks[1]),
            share(m.wall_by_tasks[2]),
            self.groups.len()
        ))
    }

    /// The served engine moved on: fetch it, and run it from the next task.
    fn refresh_engine(&mut self) -> Result<()> {
        let rel = fetch_release(&self.net)?;
        self.stale = false;
        if rel.engine == self.rel.engine {
            self.ui.status(|s| s.state = "服务器协议已更新，请更新 simfleet".into());
            self.ui.log(format!("{C_BAD}[过旧]{C_END} 服务器不再接受本版本的协议（{PROTOCOL}），请更新 simfleet"));
            std::thread::sleep(Duration::from_secs(60));
            return Ok(());
        }
        self.ui.log(format!("{C_INFO}[引擎]{C_END} 网站引擎已更新 {} → {}", self.rel.engine, rel.engine));
        let code = load_code(&self.net, &self.paths, &self.cfg, &self.ui, &rel)?;
        self.swap_code(code);
        self.ui.status(|s| s.engine = rel.engine.clone());
        self.rel = rel;
        Ok(())
    }

    fn swap_code(&mut self, code: Arc<Code>) {
        let backend = crate::work::backend_label(&code);
        self.pool = Pool::new(code.clone());
        self.meter.rebase();
        self.tag_task.clear();
        self.exec = Exec::new(code);
        self.ui.status(|s| s.backend = backend.into());
    }

    /// ON THE WASM, look again every half hour for a published native build of
    /// the served engine (it appears some minutes after the engine does). Only
    /// between tasks, since the lanes are rebuilt.
    fn maybe_native(&mut self) {
        if self.pool.code().native.is_some() || !self.cfg.native_engine || self.cfg.native_engine_url.is_empty()
            || self.native_checked.elapsed() < Duration::from_secs(1800)
        {
            return;
        }
        self.native_checked = Instant::now();
        // `load_code` fetches whichever native build is published by now.
        if let Ok(code) = load_code(&self.net, &self.paths, &self.cfg, &self.ui, &self.rel) {
            if code.native.is_some() {
                self.swap_code(code);
            }
        }
    }

    /// The bottom line's numbers, summed over this machine's ids.
    fn refresh_stats(&mut self) {
        self.stats_at = Some(Instant::now());
        let since = sys::local_midnight_utc_hour();
        let (mut device, mut today, mut any) = (0u64, 0u64, false);
        for g in &self.groups {
            if let Some(p) = self.net.post_ok("/api/board/points", &json!({ "verifier": g.verifier, "since": since })) {
                any = true;
                device += p["points"].as_u64().unwrap_or(0);
                today += p["today"]["points"].as_u64().unwrap_or(0);
            }
        }
        let dev = self.net.session.as_ref().and_then(|_| account::devices(&self.net));
        // THE SITE'S SIDE - who is computing, what is owed, the week's ranking - on
        // its own thread; the ranking is the page's own (`/api/contributors`).
        let (net, ui) = (self.net.clone(), self.ui.clone());
        std::thread::spawn(move || {
            let demand = net.get("/api/board/demand").ok().map(|r| r.body).filter(|b| b["ok"] == json!(true));
            let top = net.get("/api/contributors?period=week&limit=40").ok().map(|r| r.body).filter(|b| b["ok"] == json!(true));
            ui.status(|s| {
                if let Some(d) = &demand {
                    s.computing_now = d["computing"].as_u64();
                    s.owed = Some(["new_builds", "rescores", "sweeps", "refreshes"].iter().map(|k| d["owed"][k].as_u64().unwrap_or(0)).sum());
                    s.per_hour = Some((d["per_hour"]["volunteers"].as_u64().unwrap_or(0), d["per_hour"]["official"].as_u64().unwrap_or(0)));
                }
                if let Some(t) = &top {
                    s.ranking_total = t["total"].as_u64();
                    s.ranking = t["contributors"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .map(|(i, r)| Ranked {
                            rank: i as u64 + 1,
                            name: r["name"].as_str().unwrap_or("匿名").chars().filter(|c| !c.is_control()).take(24).collect(),
                            points: r["week"].as_u64().unwrap_or(0),
                            you: r["you"] == json!(true),
                        })
                        .collect();
                }
            });
        });
        let (mut confirmed, mut waiting) = (0u64, 0u64);
        for g in &self.groups {
            let mine = |riven: bool| -> Vec<Value> {
                self.recent.iter().filter(|(v, k, _)| *v == g.verifier && *k == riven).map(|(_, _, x)| x.clone()).collect()
            };
            let orders = mine(false);
            if !orders.is_empty() {
                if let Some(m) = self.net.post_ok("/api/board/mine", &json!({ "verifier": g.verifier, "orders": orders })) {
                    for o in m["orders"].as_array().into_iter().flatten() {
                        match o["state"].as_str() {
                            Some("confirmed") => confirmed += 1,
                            Some("waiting") | Some("checking") => waiting += 1,
                            _ => {}
                        }
                    }
                }
            }
            let tasks = mine(true);
            if !tasks.is_empty() {
                if let Some(m) = self.net.post_ok("/api/appraise/mine", &json!({ "verifier": g.verifier, "tasks": tasks })) {
                    for o in m["tasks"].as_array().into_iter().flatten() {
                        match o["state"].as_str() {
                            Some("confirmed") => confirmed += 1,
                            Some("waiting") => waiting += 1,
                            _ => {}
                        }
                    }
                }
            }
        }
        let name = self.cfg.account.name.clone();
        self.ui.status(|s| {
            if any {
                s.device_points = Some(device);
                s.today_points = Some(today);
            }
            if let Some(d) = &dev {
                s.account = Some(if name.is_empty() { "已登录".into() } else { name });
                s.account_points = d["points"].as_u64();
                s.contributor_rank = d["contributor_rank"]["rank"].as_u64();
            } else {
                s.account = None;
                s.account_points = None;
            }
            s.confirmed = confirmed;
            s.waiting = waiting;
        });
    }
}

enum Finished {
    Board(RowScore),
    Riven(Value),
}

/// Busy lane time over allowed lane time — a five-minute window and the session —
/// and the wall time spent with none, one or several tasks held.
struct Meter {
    at: Instant,
    busy_seen: f64,
    window: VecDeque<(Instant, f64, f64)>,
    busy_total: f64,
    cap_total: f64,
    wall_total: f64,
    wall_by_tasks: [f64; 3],
}

impl Meter {
    fn new() -> Meter {
        Meter { at: Instant::now(), busy_seen: 0.0, window: VecDeque::new(), busy_total: 0.0, cap_total: 0.0, wall_total: 0.0, wall_by_tasks: [0.0; 3] }
    }
    /// A new pool counts from zero.
    fn rebase(&mut self) {
        self.busy_seen = 0.0;
    }
    fn read(&mut self, busy: f64, lanes: usize, tasks: usize) -> (Option<f64>, Option<f64>) {
        let now = Instant::now();
        let dt = now.duration_since(self.at).as_secs_f64() * 1000.0;
        let cap = dt * lanes as f64;
        let b = (busy - self.busy_seen).max(0.0);
        self.at = now;
        self.busy_seen = busy;
        self.busy_total += b;
        self.cap_total += cap;
        if lanes > 0 {
            self.wall_total += dt;
            self.wall_by_tasks[tasks.min(2)] += dt;
        }
        self.window.push_back((now, b, cap));
        while self.window.front().is_some_and(|w| now.duration_since(w.0) > Duration::from_secs(300)) {
            self.window.pop_front();
        }
        let (wb, wc) = self.window.iter().fold((0.0, 0.0), |a, w| (a.0 + w.1, a.1 + w.2));
        let pct = |b: f64, c: f64| (c > 1000.0).then(|| (b / c).min(1.0));
        (pct(wb, wc), pct(self.busy_total, self.cap_total))
    }
}
