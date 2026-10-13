// SPDX-License-Identifier: AGPL-3.0-or-later
//! THE DISPATCHER: every core busy, within the budget, on as many tasks as that takes.
//!
//! A task does not fill a machine on its own. A riven gain's quick descent is a
//! chain of LEAD steps on one core with the round's scoring between them, so on
//! sixteen lanes one search keeps about half of them busy (measured: 55%); a
//! board row parallelises across its runs but ends in a tail, and between tasks
//! there is a round trip to the server. So:
//!
//! - BOARD ROWS COME IN PACKS (protocol 7, worker/verify.js `PACK_MS`): about a
//!   minute of a computer's work under one lease, sized by the pace the client
//!   reports. A pack's rows start as lanes come free — one row's tail is the
//!   next row's start — and their answers go back together every half minute
//!   and as the pack ends (`/api/board/answers`), the last send giving back the
//!   rows it did not reach. A riven gain still comes alone.
//! - SEVERAL AT ONCE. The server lets one computer id hold what it computes and
//!   one more asked ahead (`PACKS_HELD`), so a GROUP — one id — holds up to two
//!   packs, or a riven gain and one more. One browser is one id and players open several to compute more;
//!   simfleet opens groups the same way, each with its own id, claimed for the
//!   same account. The server never lets two of one owner's ids, or two ids on
//!   one network, confirm each other, so this adds compute and never a witness.
//! - ONE LANE POOL, OLDEST TASK FIRST. A free lane takes the next job of the
//!   oldest task that has one; what that task cannot use goes to the next
//!   (backfill), and a lane never waits on a task.
//! - A RIVEN GAIN'S LEADER LIVES ON ITS OWN LANE: the search state is that
//!   lane's between steps (its wasm instance, or natively its thread), so the
//!   lane takes no other task's work while the search lives — not even a native
//!   board piece, which the next lead step would wait behind (measured: a
//!   search 10-15% slower).
//! - THE SERVED ENGINE IS FOLLOWED, NOT WAITED FOR: an older engine is still
//!   handed board rows for a week (docs/CLIENTS.md `accepted`), so when the site
//!   names a newer one the client fetches it on a thread of its own, finishes
//!   what it holds on the old one, and computes the next work on the new.
//! - THE NETWORK IS NEVER WAITED ON: asks, renewals and answers run on their own
//!   threads. A plain ask (not `ahead`) makes the server take back what that id
//!   holds, so it is only sent by an id that holds nothing and is sending nothing.
//! - A RIVEN GAIN'S JOBS GO FIRST. Its search is a chain — a lead step, then the
//!   round's scoring — so a step kept waiting holds the whole search back, while
//!   a board piece is a quarter second and anything else can wait for one.
//! - HOW MANY GROUPS IS MEASURED, not guessed, and goes both ways: every 20
//!   seconds, from the lanes' utilisation, the board work held (in seconds of
//!   the whole budget, from what a row has cost) and whether the server still
//!   hands every group work. One more opens when lanes idle with little held and
//!   every group busy; the newest stops asking when the work held would keep
//!   the budget busy for minutes, when the server has none to spare, or when the
//!   budget shrank — at most the cap (`max_slots`, `[`/`]`), one step at a time,
//!   and not straight back after closing.
//!
//! Every computation is the page's own, step for step (work.rs), so a result is
//! bit for bit what a browser sends, however the lanes were shared.

use crate::config::{Config, Paths};
use crate::engine::{Backend, Call, Code, Done, Exec, Job, Pool};
use crate::net::Net;
use crate::ui::{group, points, Ctl, GroupView, Ranked, TaskLine, Ui, C_BAD, C_END, C_GO, C_INFO, C_OK};
use crate::work::{
    board_payload, call, fetch_release, load_code, truthy, BoardRun, Release, RowScore, PROTOCOL, RIVEN_RENEW, STATS_EVERY,
};
use crate::{account, jsnum, sys};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A held task given back when nothing of it has run for this long (its lanes
/// went to older tasks, or the budget shrank under it).
const STARVED: Duration = Duration::from_secs(180);
/// How often the group count is reconsidered, and what moves it: below this
/// utilisation with less than `HELD_LOW` seconds of board work held, one more
/// opens; more than `HELD_HIGH` held (or `HELD_FULL` on lanes above
/// `FULL_ABOVE`) closes the newest. A close keeps the count from opening again
/// for `REOPEN_AFTER`.
///
/// IN PACKS: a pack is about a minute of the group's work (worker/verify.js
/// `PACK_MS`, at most `PACK_MAX` rows) and a group holds one and one asked
/// ahead, so one group alone holds up to two minutes — and on a big machine a
/// pack of twenty quick rows lasts seconds. So a group opens while lanes idle
/// more than one part in twenty with less than a pack in hand, and closes only
/// when what is held would keep every lane busy for minutes.
const CONTROL_EVERY: Duration = Duration::from_secs(20);
const OPEN_BELOW: f64 = 0.95;
const FULL_ABOVE: f64 = 0.98;
const HELD_LOW: f64 = 60.0;
const HELD_FULL: f64 = 180.0;
const HELD_HIGH: f64 = 300.0;
const REOPEN_AFTER: Duration = Duration::from_secs(60);
/// A group the server told "nothing" this recently says the server has none to spare.
const NOTHING_RECENT: Duration = Duration::from_secs(60);
/// A group the server told "nothing for you" asks again after this.
const NOTHING_WAIT: Duration = Duration::from_secs(10);
/// …and after this when it says it is busy (worker/verify.js `COMPUTE_CAP`:
/// too many computers asking, and this id has answered nothing lately).
const BUSY_WAIT: Duration = Duration::from_secs(60);
/// How often `/release.json` is read for the engine the site serves: its
/// library is published before the site serves it (docs/CLIENTS.md).
const RELEASE_EVERY: Duration = Duration::from_secs(60);
/// A pack's answers so far go back this often (69-board-work.js `ANSWER_EVERY_MS`)…
const ANSWER_EVERY: Duration = Duration::from_secs(30);
/// …the next pack is asked once what is left of this one would take under this
/// at the group's pace (`REFILL_MS`)…
const REFILL_MS: f64 = 15_000.0;
/// A ROW'S LEASE IS A HEARTBEAT (worker/tasks.js `renewRow`): renewed once it
/// is this close to its end, before the row starts and while it runs, and
/// tried again this much later when a renewal got no answer.
const RENEW_BEFORE: Duration = Duration::from_secs(120);
const RENEW_RETRY: Duration = Duration::from_secs(30);

// ---- one task's computation, as jobs ------------------------------------------

/// A RIVEN GAIN (08-checkpoint-api.js `quickFleet`): lead steps on the leader's
/// instance, each round's builds scored in slices — two a lane — on any lane.
struct Riven {
    /// Where its search runs, fixed when it starts: its lead's state lives
    /// there (a wasm instance, or a native lane's thread).
    backend: Backend,
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
    fn new(request: Value, backend: Backend) -> Riven {
        Riven {
            backend,
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
    /// A board row's pack, and its question there.
    pack: Option<u64>,
    question: String,
    /// The row's lease end as last renewed, and when a failed renewal is tried again.
    row_until: Instant,
    renew_retry: Instant,
}

/// A PACK of board rows under one lease: its rows not started yet, and its
/// answers not sent yet.
struct Pack {
    id: u64,
    group: usize,
    verifier: String,
    lease: String,
    rows: VecDeque<Value>,
    total: usize,
    running: usize,
    answered: usize,
    unsent: Vec<Value>,
    unsent_work: u64,
    sent_at: Instant,
    /// When the lease handed out with the pack ends; each row renews its own.
    until: Instant,
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
    /// An ask on its way: ahead or not, and when it was sent.
    asking: Option<(bool, Instant, JoinHandle<Option<Value>>)>,
    next_ask: Instant,
    next_ahead: Instant,
    claimed: bool,
    /// Answers the server took from it this run, and their work.
    done: u64,
    work: u64,
    /// A row's wall time in this group, smoothed (the `pace_ms` a pack is sized
    /// by): rows finishing side by side count the time between them.
    pace_ms: f64,
    last_row: Option<Instant>,
    /// When the server last had nothing for it.
    nothing_at: Option<Instant>,
}

/// An answer on its way to the server.
struct Sending {
    group: usize,
    handle: JoinHandle<bool>,
    log_ok: String,
    work: u64,
    rows: u64,
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
    packs: Vec<Pack>,
    groups: Vec<Group>,
    /// How many groups may take new work now.
    open: usize,
    sending: Vec<Sending>,
    tag_task: HashMap<u64, u64>,
    /// The task whose job each lane runs.
    lane_job: Vec<Option<u64>>,
    next_id: u64,
    stale: bool,
    banned_until: Option<String>,
    /// The server's word when it hands this client no work at all (`unrecognised_client`).
    refused: Option<String>,
    recent: Vec<(String, bool, Value)>,
    stats_at: Option<Instant>,
    native_checked: Instant,
    /// A native build being fetched and checked, off the dispatcher: for the engine named.
    native_load: Option<(String, JoinHandle<Option<Arc<Code>>>)>,
    /// The bottom line's numbers being asked for (`refresh_stats`).
    stats_job: Option<JoinHandle<()>>,
    /// `/release.json` being read, and when it last was (`follow_engine`).
    release_read: Option<JoinHandle<Option<Value>>>,
    release_at: Instant,
    /// When "the site is busy" was last said.
    busy_said: Option<Instant>,
    /// THE ENGINE THE SITE SERVES NOW, being made ready off the dispatcher…
    engine_load: Option<(String, JoinHandle<Option<(Release, Arc<Code>)>>)>,
    /// …ready: no more work is asked, and once what is held is done, it runs.
    switch_to: Option<(Release, Arc<Code>)>,
    /// When a failed preparation may be tried again.
    engine_retry: Instant,
    control_at: Instant,
    control_busy: f64,
    control_cap: f64,
    /// When the group count last went down.
    closed_at: Option<Instant>,
    /// The riven canary's verdict as last seen (engine.rs `riven_state`).
    riven_seen: u8,
    /// A board row's cost in lane milliseconds, smoothed: what the work held is measured in.
    row_cost_ms: f64,
    meter: Meter,
    /// The lanes' CPU and wall time when last read, and when it was read.
    yield_seen: (Instant, u64, u64),
    /// Since when the lanes have been given too little, and when that was last said.
    starved_since: Option<Instant>,
    starved_said: Option<Instant>,
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
            packs: Vec::new(),
            groups: Vec::new(),
            open: 1,
            sending: Vec::new(),
            tag_task: HashMap::new(),
            lane_job: Vec::new(),
            next_id: 0,
            stale: false,
            banned_until: None,
            refused: None,
            recent: Vec::new(),
            stats_at: None,
            native_checked: Instant::now(),
            native_load: None,
            stats_job: None,
            release_read: None,
            release_at: Instant::now(),
            busy_said: None,
            engine_load: None,
            switch_to: None,
            engine_retry: Instant::now(),
            control_at: Instant::now(),
            control_busy: 0.0,
            control_cap: 0.0,
            closed_at: None,
            row_cost_ms: 0.0,
            riven_seen: 0,
            meter: Meter::new(),
            yield_seen: (Instant::now(), 0, 0),
            starved_since: None,
            starved_said: None,
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
            self.groups.push(Group {
                verifier,
                asking: None,
                next_ask: now,
                next_ahead: now,
                claimed: false,
                done: 0,
                work: 0,
                pace_ms: 0.0,
                last_row: None,
                nothing_at: None,
            });
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
    /// THE CAP on groups: `max_slots` (0: by the machine's size), and no more
    /// than half the lanes the budget allows now — one group's pack starts
    /// rows on every lane.
    fn max_groups(&self) -> usize {
        let cap = match self.ctl.max_slots.load(Ordering::Relaxed) {
            0 => crate::config::auto_max_slots(self.ctl.max_cores),
            n => n,
        };
        (self.lanes() / 2).max(1).min(cap)
    }
    fn backend(&self) -> Backend {
        if self.pool.code().native.is_some() { Backend::Native } else { Backend::Wasm }
    }
    /// A RIVEN GAIN natively when the build has the optimizer and gave the
    /// wasm's answer on the riven canary (work.rs), unless `native_riven` is off.
    fn riven_backend(&self) -> Backend {
        if self.cfg.native_riven && self.pool.code().native.as_ref().is_some_and(|n| n.can_optimize()) {
            Backend::Native
        } else {
            Backend::Wasm
        }
    }

    fn ask_body(&self, gi: usize, ahead: bool) -> Value {
        // WHAT THIS ID CAN GIVE: its share of the lanes, as one browser among several says its own.
        let lanes = (self.lanes() / self.open.max(1)).max(1);
        let g = &self.groups[gi];
        let mut b = json!({
            "verifier": g.verifier,
            "engine": self.rel.engine,
            "protocol": PROTOCOL,
            "consent": { "v": self.cfg.consent_v, "at": self.cfg.consent_at },
            "lanes": lanes,
        });
        if ahead {
            b["ahead"] = json!(true);
        }
        if g.pace_ms > 0.0 {
            b["pace_ms"] = json!((g.pace_ms.round() as u64).clamp(100, 30 * 60_000));
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
        let kind = if t.riven() { "紫卡增益" } else { "榜单" };
        self.ui.glog(t.group, format!("{C_GO}[中断]{C_END} {kind} {}{why}", t.label));
        // A PACK'S ROW is given back with the pack, by leaving it unanswered.
        if let Some(pid) = t.pack {
            if let Some(p) = self.packs.iter_mut().find(|p| p.id == pid) {
                p.running = p.running.saturating_sub(1);
            }
            self.maybe_end_pack(pid);
            return;
        }
        if give_back && !self.offline {
            let (net, v, lease, code) = (self.net.clone(), t.verifier.clone(), t.lease.clone(), t.code.clone());
            std::thread::spawn(move || Self::release_lease(&net, &v, &lease, code.as_deref()));
        }
    }

    /// A pack with nothing left to start or finish: its last answers, `done`.
    fn maybe_end_pack(&mut self, pid: u64) {
        if self.packs.iter().any(|p| p.id == pid && p.rows.is_empty() && p.running == 0) {
            self.send_pack(pid, true);
        }
    }

    /// A pack's answers so far, sent on their own thread; `done` ends the pack,
    /// giving back what it did not reach.
    fn send_pack(&mut self, pid: u64, done: bool) {
        let Some(pi) = self.packs.iter().position(|p| p.id == pid) else { return };
        let p = &mut self.packs[pi];
        if p.unsent.is_empty() && !done {
            return;
        }
        let answers = std::mem::take(&mut p.unsent);
        let (rows, work) = (answers.len() as u64, std::mem::take(&mut p.unsent_work));
        p.sent_at = Instant::now();
        let body = json!({ "lease": p.lease, "verifier": p.verifier, "engine": self.rel.engine, "done": done, "answers": answers });
        let gi = p.group;
        let log_ok = if rows > 0 {
            format!("{C_OK}[提交]{C_END} {rows} 行答案 · {:.2} 分{}", points(work), if done { "（本包结束）" } else { "" })
        } else {
            String::new()
        };
        if done {
            let p = self.packs.remove(pi);
            if p.answered < p.total {
                self.ui.glog(gi, format!("{C_GO}[交还]{C_END} 本包还有 {} 行没算，已交还", p.total - p.answered));
            }
        }
        let net = self.net.clone();
        let handle = std::thread::spawn(move || send_answers(&net, &body));
        self.sending.push(Sending { group: gi, handle, log_ok, work, rows });
    }

    fn drop_all(&mut self, why: &str) {
        for p in &mut self.packs {
            p.rows.clear();
        }
        let ids: Vec<u64> = self.tasks.iter().map(|t| t.id).collect();
        for id in ids {
            self.drop_task(id, why, true);
        }
        let packs: Vec<u64> = self.packs.iter().map(|p| p.id).collect();
        for pid in packs {
            self.send_pack(pid, true);
        }
        // An ask still on its way may come back holding a lease: give it back.
        for g in &mut self.groups {
            if let Some((_, _, h)) = g.asking.take() {
                if let Ok(Some(r)) = h.join() {
                    if let Some(lease) = r["pack"]["lease"].as_str() {
                        Self::release_lease(&self.net, &g.verifier, lease, None);
                    }
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
        // The last answers count in the closing line too — sent again for up to
        // three quarters of a minute when the site does not answer.
        if !self.sending.is_empty() {
            self.ui.status(|st| st.state = "正在交最后的答案（连不上服务器时会重试，最多约 45 秒）".into());
        }
        for s in self.sending.drain(..) {
            if s.handle.join().unwrap_or(false) {
                self.ui.status(|st| {
                    st.session_tasks += s.rows;
                    st.session_work += s.work;
                });
            }
        }
    }

    // ---- taking work in -------------------------------------------------------

    /// Answers to asks that came back, and new asks where a group has room.
    fn asks(&mut self) {
        let lanes = self.lanes();
        let now = Instant::now();
        for gi in 0..self.groups.len() {
            if let Some((ahead, at, h)) = self.groups[gi].asking.take() {
                if !h.is_finished() {
                    self.groups[gi].asking = Some((ahead, at, h));
                } else {
                    let reply = h.join().ok().flatten();
                    self.take_reply(gi, ahead, at, reply);
                }
            }
            if self.groups[gi].asking.is_some() || self.offline || self.stale || self.switch_to.is_some() || self.banned_until.is_some() || self.refused.is_some() {
                continue;
            }
            // WHAT THE GROUP HOLDS: its packs and the riven gains it computes.
            let packs: Vec<&Pack> = self.packs.iter().filter(|p| p.group == gi).collect();
            let units = packs.len() + self.tasks.iter().filter(|t| t.group == gi && t.pack.is_none()).count();
            let sending = self.sending.iter().filter(|s| s.group == gi).count();
            let open = gi < self.open;
            let g = &self.groups[gi];
            // A PLAIN ASK makes the server take back all the id holds, so only an
            // id holding nothing and sending nothing sends one; AN AHEAD ASK comes
            // from one holding one thing — a pack nearly through, or a search.
            let refill = match packs.first() {
                Some(p) => p.rows.len() as f64 * g.pace_ms <= REFILL_MS,
                None => true,
            };
            let plain = open && units == 0 && sending == 0 && now >= g.next_ask;
            let ahead = open && units == 1 && refill && lanes >= 2 && now >= g.next_ahead;
            if plain || ahead {
                if !g.claimed && self.net.session.is_some() {
                    self.claim(gi);
                }
                let (net, body) = (self.net.clone(), self.ask_body(gi, ahead));
                // A refusal's body is the message, so it is kept too.
                let ask = move || {
                    net.post("/api/board/work", &body)
                        .ok()
                        .filter(|r| (200..300).contains(&r.status) || r.body["error"].is_string())
                        .map(|r| r.body)
                };
                self.groups[gi].asking = Some((ahead, Instant::now(), std::thread::spawn(ask)));
            }
        }
    }

    fn take_reply(&mut self, gi: usize, ahead: bool, asked_at: Instant, reply: Option<Value>) {
        let wait = Instant::now() + NOTHING_WAIT;
        let Some(r) = reply else {
            self.ui.status(|s| s.state = "连不上服务器，稍后重试".into());
            self.groups[gi].next_ask = wait;
            self.groups[gi].next_ahead = wait;
            return;
        };
        // NO WORK FOR THIS CLIENT AT ALL, and the site says whom to ask.
        if r["error"] == json!("unrecognised_client") {
            let msg = r["message"].as_str().unwrap_or("服务器不再给这个客户端派发任务").to_string();
            if self.refused.is_none() {
                self.ui.log(format!("{C_BAD}[服务器]{C_END} {msg}"));
            }
            self.refused = Some(msg);
            return;
        }
        if r["error"].is_string() {
            self.groups[gi].next_ask = wait;
            self.groups[gi].next_ahead = wait;
            return;
        }
        if r["stale"] == json!(true) {
            self.stale = true;
            return;
        }
        // AN OLDER ENGINE STILL ACCEPTED: the site serves `engine` now, and takes
        // this one's answers on the rows it hands it until `accepted_until`.
        if let Some(e) = r["engine"].as_str().filter(|e| *e != self.rel.engine) {
            self.served_moved(e.to_string(), r["accepted_until"].as_str().unwrap_or("?").to_string());
        }
        if r["banned"] == json!(true) {
            let until = r["until"].as_str().unwrap_or("?").to_string();
            if self.banned_until.is_none() {
                self.ui.log(format!("{C_BAD}[冷却]{C_END} 服务器复核时与本机结果不一致，暂停派活至 {until}"));
            }
            self.banned_until = Some(until);
            return;
        }
        // A PACK of board rows.
        let pack = &r["pack"];
        if let (Some(rows), Some(lease)) = (pack["tasks"].as_array().filter(|t| !t.is_empty()), pack["lease"].as_str()) {
            let verifier = self.groups[gi].verifier.clone();
            if self.quit() || self.held_why().is_some() {
                Self::release_lease(&self.net, &verifier, lease, None);
                return;
            }
            let lease_ms = pack["lease_ms"].as_u64().unwrap_or(30 * 60_000);
            let until = asked_at + Duration::from_millis(lease_ms);
            self.next_id += 1;
            self.ui.glog(gi, format!("{C_INFO}[领取]{C_END} 一包 {} 行榜单，租约 {:.0} 分钟{}", rows.len(), lease_ms as f64 / 60_000.0, if ahead { "（预取）" } else { "" }));
            self.packs.push(Pack {
                id: self.next_id,
                group: gi,
                verifier,
                lease: lease.to_string(),
                rows: rows.iter().cloned().collect(),
                total: rows.len(),
                running: 0,
                answered: 0,
                unsent: Vec::new(),
                unsent_work: 0,
                sent_at: Instant::now(),
                until,
            });
            return;
        }
        let w = r["work"].clone();
        if !w.is_object() {
            // THE SITE IS BUSY: asked again a minute later, not ten seconds.
            let wait = if r["busy"] == json!(true) {
                if self.busy_said.is_none_or(|t| t.elapsed() > Duration::from_secs(600)) {
                    self.busy_said = Some(Instant::now());
                    self.ui.log(format!("{C_GO}[服务器]{C_END} 服务器繁忙，暂不给新的计算机派活，1 分钟后再领"));
                }
                Instant::now() + BUSY_WAIT
            } else {
                wait
            };
            if ahead {
                self.groups[gi].next_ahead = wait;
            } else {
                self.groups[gi].next_ask = wait;
                self.groups[gi].nothing_at = Some(Instant::now());
            }
            return;
        }
        if self.quit() || self.held_why().is_some() {
            let code = (w["kind"] == "riven_gain").then(|| w["code"].as_str().unwrap_or("").to_string());
            Self::release_lease(&self.net, &self.groups[gi].verifier, w["lease"].as_str().unwrap_or(""), code.as_deref());
            return;
        }
        let verifier = self.groups[gi].verifier.clone();
        self.start(gi, verifier, w, None);
    }

    /// THE NEXT ROW of the oldest pack with one, started — `false` when no pack has one.
    fn start_row(&mut self) -> bool {
        let Some(pi) = self.packs.iter().position(|p| !p.rows.is_empty()) else { return false };
        let p = &mut self.packs[pi];
        let Some(mut w) = p.rows.pop_front() else { return false };
        p.running += 1;
        w["lease"] = json!(p.lease);
        let (gi, verifier, pid) = (p.group, p.verifier.clone(), p.id);
        if !self.start(gi, verifier, w, Some(pid)) {
            if let Some(p) = self.packs.iter_mut().find(|p| p.id == pid) {
                p.running -= 1;
            }
            self.maybe_end_pack(pid);
        }
        true
    }

    /// A leased task becomes a held one: a board row asks the engine for its fight.
    fn start(&mut self, gi: usize, verifier: String, w: Value, pack: Option<u64>) -> bool {
        let lease = w["lease"].as_str().unwrap_or("").to_string();
        let now = Instant::now();
        self.next_id += 1;
        let id = self.next_id;
        let (runner, label, code) = if w["kind"] == "riven_gain" {
            let code = w["code"].as_str().unwrap_or("").to_string();
            let label = format!("{} · {} [{}]", w["weapon"].as_str().unwrap_or("?"), w["ruler"].as_str().unwrap_or(""), code);
            self.ui.glog(gi, format!("{C_GO}[开始]{C_END} 紫卡增益  {label}"));
            (Runner::Riven(Riven::new(w["request"].clone(), self.riven_backend())), label, Some(code))
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
                    // A pack's row is left unanswered, and goes back with the pack.
                    if !self.offline && pack.is_none() {
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
            runner,
            began: now,
            last_ran: now,
            renewed: now,
            renewing: None,
            lanes_used: HashSet::new(),
            row_until: pack.and_then(|pid| self.packs.iter().find(|p| p.id == pid)).map_or(now + Duration::from_secs(1800), |p| p.until),
            renew_retry: now,
            question: w["question"].as_str().unwrap_or("").to_string(),
            pack,
            w,
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

    /// Fill every free lane: the oldest task with a job for it, honouring
    /// leaders; lanes still free start a pack's next row.
    fn dispatch(&mut self) {
        for _ in 0..16 {
            if self.fill_lanes() == 0 || !self.start_row() {
                break;
            }
        }
    }

    /// …one pass; the lanes it left free that a board row could use.
    fn fill_lanes(&mut self) -> usize {
        let lanes = self.lanes();
        self.pool.ensure(lanes);
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
        // A RIVEN GAIN'S JOBS FIRST (see the module's note), then the rest by age.
        let order: Vec<usize> =
            (0..self.tasks.len()).filter(|&i| self.tasks[i].riven()).chain((0..self.tasks.len()).filter(|&i| !self.tasks[i].riven())).collect();
        let mut free = 0;
        for lane in 0..lanes {
            if self.pool.pending(lane) > 0 {
                continue;
            }
            let owner = leader_of.get(&lane).copied();
            for &ti in &order {
                let t = &mut self.tasks[ti];
                let mine = owner == Some(t.id);
                let tag = self.pool.next_tag();
                let job = match &mut t.runner {
                    // Never on a leader's lane: its next lead step must not wait.
                    Runner::Board(b) if owner.is_none() => b.job(tag, lanes).map(|c| (backend, c)),
                    Runner::Riven(r) if owner.is_none() || mine => r.job(lane, tag).map(|c| (r.backend, c)),
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
            if self.pool.pending(lane) == 0 && owner.is_none() {
                free += 1;
            }
        }
        free
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
        if let (Some(pid), Finished::Board(s)) = (t.pack, &f) {
            return self.answer_row(&t, pid, s, lanes, secs);
        }
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
        self.sending.push(Sending { group: t.group, handle, log_ok, work, rows: 1 });
    }

    /// A PACK'S ROW, measured: its answer joins the pack's, sent every half minute and as the pack ends.
    fn answer_row(&mut self, t: &Active, pid: u64, s: &RowScore, lanes: usize, secs: f64) {
        let now = Instant::now();
        let g = &mut self.groups[t.group];
        let wall = secs * 1000.0;
        let ms = g.last_row.map_or(wall, |l| now.duration_since(l).as_secs_f64() * 1000.0).min(wall);
        g.pace_ms = if g.pace_ms > 0.0 { g.pace_ms * 0.7 + ms * 0.3 } else { ms };
        g.last_row = Some(now);
        // `counts` and `lanes`: what the row cost, kept beside the answer to
        // fit what a point is worth (never credited; 69-board-work.js).
        let cost = s.compute_ms as f64;
        self.row_cost_ms = if self.row_cost_ms > 0.0 { self.row_cost_ms * 0.8 + cost * 0.2 } else { cost };
        let mut a = json!({ "question": t.question, "score": s.score, "metric": s.metric, "work": s.work,
                            "compute_ms": s.compute_ms, "lanes": lanes });
        if s.counts.is_object() {
            a["counts"] = s.counts.clone();
        }
        self.ui.glog(t.group, format!("{C_OK}[完成]{C_END} 榜单  {}  = {:.4} {} · {:.2} 分 · {secs:.1}s", t.label, s.score, s.metric, points(s.work)));
        self.recent.push((t.verifier.clone(), false, json!({ "identity": t.w["identity"], "ruler": t.w["ruler"], "mode": t.w["mode"] })));
        if self.recent.len() > 100 {
            self.recent.remove(0);
        }
        let Some(p) = self.packs.iter_mut().find(|p| p.id == pid) else { return };
        p.running = p.running.saturating_sub(1);
        p.answered += 1;
        p.unsent.push(a);
        p.unsent_work += s.work;
        if p.rows.is_empty() && p.running == 0 {
            self.send_pack(pid, true);
        } else if p.sent_at.elapsed() >= ANSWER_EVERY {
            self.send_pack(pid, false);
        }
    }

    /// Answers that arrived, and the riven gains' leases kept alive.
    fn housekeeping(&mut self) {
        let mut i = 0;
        while i < self.sending.len() {
            if self.sending[i].handle.is_finished() {
                let s = self.sending.remove(i);
                let ok = s.handle.join().unwrap_or(false);
                if ok {
                    if !s.log_ok.is_empty() {
                        self.ui.glog(s.group, s.log_ok);
                    }
                    if let Some(g) = self.groups.get_mut(s.group) {
                        g.done += s.rows;
                        g.work += s.work;
                    }
                    let now = Instant::now();
                    self.ui.status(|st| {
                        st.session_tasks += s.rows;
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
        let mut taken = Vec::new();
        for t in &mut self.tasks {
            if let Some(h) = t.renewing.take() {
                if !h.is_finished() {
                    t.renewing = Some(h);
                } else {
                    let r = h.join().ok().flatten();
                    if r.as_ref().is_some_and(|r| r["held"] == json!(false)) {
                        if t.pack.is_some() { taken.push(t.id) } else { lost.push(t.id) }
                    } else if t.pack.is_some() {
                        // A ROW'S HEARTBEAT: the lease runs on from when it was sent.
                        match r.as_ref().and_then(|r| r["renew_ms"].as_u64()) {
                            Some(ms) => t.row_until = t.renewed + Duration::from_millis(ms),
                            None => t.renew_retry = now + RENEW_RETRY,
                        }
                    }
                }
            }
            if t.pack.is_some() && !self.offline && t.renewing.is_none() && now + RENEW_BEFORE >= t.row_until && now >= t.renew_retry {
                t.renewed = now;
                let (net, body) = (self.net.clone(), json!({ "lease": t.lease, "verifier": t.verifier, "question": t.question }));
                t.renewing = Some(std::thread::spawn(move || net.post_ok("/api/board/renew", &body)));
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
            if quiet {
                late.push(t.id);
            }
        }
        // PACKS: their answers every half minute. A pack ends when its rows do;
        // a row another computer took meanwhile is passed by.
        let sends: Vec<u64> = self.packs.iter().filter(|p| !p.unsent.is_empty() && p.sent_at.elapsed() >= ANSWER_EVERY).map(|p| p.id).collect();
        for pid in sends {
            self.send_pack(pid, false);
        }
        for id in taken {
            self.drop_task(id, "：这一行已由别的电脑接手，跳过", false);
        }
        for id in lost {
            self.drop_task(id, "：任务已转给别的电脑", false);
        }
        for id in late {
            self.drop_task(id, "：太久没轮到算力，任务已交还", true);
        }
    }

    /// THE GROUP COUNT, from what the meter saw over the last half minute.
    /// The riven canary's verdict, said once it lands.
    fn riven_verdict(&mut self) {
        let Some(n) = self.pool.code().native.as_ref() else { return };
        let st = n.riven_state();
        if st == self.riven_seen {
            return;
        }
        self.riven_seen = st;
        match st {
            2 => self.ui.log(format!("{C_INFO}[引擎]{C_END} 紫卡搜索与 wasm 逐位一致，之后开始的紫卡增益改用原生")),
            3 => self.ui.log(format!("{C_GO}[引擎]{C_END} 紫卡搜索与 wasm 不一致，紫卡增益仍用 wasm：{}", n.riven_why())),
            _ => return,
        }
        let label = crate::work::backend_label(self.pool.code());
        self.ui.status(|s| s.backend = label.into());
    }

    /// THE BOARD WORK HELD, in seconds of the whole budget: the rows not
    /// started and what is left of those running, at what a row has cost.
    /// `None` until a row has been measured.
    fn held_secs(&self) -> Option<f64> {
        if self.row_cost_ms <= 0.0 {
            return None;
        }
        let waiting: f64 = self.packs.iter().map(|p| p.rows.len() as f64).sum();
        let running: f64 = self
            .tasks
            .iter()
            .filter_map(|t| match &t.runner {
                Runner::Board(b) if b.runs > 0 => Some(1.0 - b.folded as f64 / b.runs as f64),
                _ => None,
            })
            .sum();
        Some((waiting + running) * self.row_cost_ms / self.lanes().max(1) as f64 / 1000.0)
    }

    /// THE GROUP COUNT, both ways (see the module's note). A fixed `slots` is
    /// kept, under the cap.
    fn control(&mut self) {
        let cap = self.max_groups();
        let now = Instant::now();
        if self.cfg.slots > 0 {
            let want = self.cfg.slots.min(cap);
            if want != self.open {
                self.open = want;
                self.ensure_groups(want);
            }
            return;
        }
        // THE CAP comes first, and at once: a key press, a smaller budget.
        if self.open > cap {
            self.ui.log(format!("{C_INFO}[调度]{C_END} 并发上限 {cap}，第 {}～{} 路做完手头的活后不再接", cap + 1, self.open));
            self.open = cap;
            self.closed_at = Some(now);
            return;
        }
        if self.control_at.elapsed() < CONTROL_EVERY {
            return;
        }
        let (busy, lane_ms) = (self.meter.busy_total - self.control_busy, self.meter.cap_total - self.control_cap);
        self.control_at = now;
        self.control_busy = self.meter.busy_total;
        self.control_cap = self.meter.cap_total;
        if lane_ms < 1000.0 {
            return;
        }
        let util = busy / lane_ms;
        let held = self.held_secs();
        let h = held.unwrap_or(0.0);
        let busy_groups = (0..self.open).all(|g| self.tasks.iter().any(|t| t.group == g) || self.packs.iter().any(|p| p.group == g));
        let told_nothing = (0..self.open).any(|g| self.groups[g].nothing_at.is_some_and(|t| now.duration_since(t) < NOTHING_RECENT));
        let reopen_ok = self.closed_at.is_none_or(|t| now.duration_since(t) >= REOPEN_AFTER);
        let close = |why: String| (self.open > 1).then_some(why);
        let closing = if told_nothing && util < OPEN_BELOW {
            close("服务器暂时没有更多任务可分".into())
        } else if h > HELD_HIGH {
            close(format!("手上的榜单够全部核心算 {h:.0} 秒"))
        } else if util > FULL_ABOVE && h > HELD_FULL {
            close(format!("核心已满，手上还有 {h:.0} 秒的活"))
        } else {
            None
        };
        if let Some(why) = closing {
            self.open -= 1;
            self.closed_at = Some(now);
            self.ui.log(format!("{C_INFO}[调度]{C_END} {why}，第 {} 路做完手头的活后不再接（现 {} 路）", self.open + 1, self.open));
        } else if util < OPEN_BELOW && h < HELD_LOW && busy_groups && !told_nothing && reopen_ok && self.open < cap {
            self.open += 1;
            self.ensure_groups(self.open);
            let held = held.map_or(String::new(), |h| format!("，手上只剩 {h:.0} 秒的活"));
            self.ui.log(format!("{C_INFO}[调度]{C_END} 核心利用率 {:.0}%{held}，加开第 {} 路", util * 100.0, self.open));
        }
    }

    fn show(&mut self) {
        let (lanes, idle, open) = (self.lanes(), self.idle(), self.open);
        let (max_groups, held_secs) = (if self.cfg.slots > 0 { self.cfg.slots.min(self.max_groups()) } else { self.max_groups() }, self.held_secs());
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
                let packs: Vec<&Pack> = self.packs.iter().filter(|p| p.group == gi).collect();
                let riven = self.tasks.iter().any(|t| t.group == gi && t.riven());
                let more = if packs.len() + usize::from(riven) > 1 { "，已预取" } else { "" };
                let state = if let Some(p) = packs.first() {
                    format!("一包 {}/{} 行{more}", p.answered, p.total)
                } else if riven {
                    format!("紫卡搜索{more}")
                } else if self.sending.iter().any(|s| s.group == gi) {
                    "提交结果".into()
                } else if g.asking.is_some() {
                    "取任务".into()
                } else if gi >= open {
                    "暂不接活".into()
                } else if g.next_ask > now {
                    "暂无任务".into()
                } else {
                    "等待".into()
                };
                GroupView { id: g.verifier.get(..8).unwrap_or(&g.verifier).to_string(), state, open: gi < open, tasks: g.done, work: g.work }
            })
            .collect();
        let state = if self.refused.is_some() {
            "服务器不派活（见日志）".to_string()
        } else if let Some(u) = &self.banned_until {
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
            s.max_groups = max_groups;
            s.held_secs = held_secs;
            s.lane_kind = crate::affinity::lane_kinds(lanes);
            s.core_pref = crate::affinity::active().map(|p| p.name());
            s.idle_cores = self.ctl.idle_cores.load(Ordering::Relaxed);
            s.active_cores = self.ctl.active_cores.load(Ordering::Relaxed);
        });
    }

    /// WHAT THE LANES ARE GIVEN, every few seconds: the CPU time their jobs got
    /// against their wall time. Lanes that get far less than a core each for a
    /// minute are said so, once in ten minutes.
    fn lane_yield(&mut self) {
        let (at, cpu0, wall0) = self.yield_seen;
        if at.elapsed() < Duration::from_secs(5) {
            return;
        }
        let (cpu, wall) = crate::affinity::lane_totals();
        self.yield_seen = (Instant::now(), cpu, wall);
        let (dc, dw) = (cpu.saturating_sub(cpu0) as f64, wall.saturating_sub(wall0) as f64);
        let lanes = self.lanes();
        if dw < 1e6 {
            self.ui.status(|s| {
                s.lane_yield = None;
                s.effective_cores = None;
            });
            return;
        }
        let share = (dc / dw).min(1.0);
        let processors = crate::affinity::processors_used(lanes);
        // AS CORES: the share each lane gets, over the lanes working now. A job
        // is counted when it ends, so a window's own CPU over its wall time
        // would read a long riven step as more cores than there are.
        let working = self.lane_job.iter().take(lanes).filter(|j| j.is_some()).count();
        self.ui.status(|s| {
            s.lane_yield = Some(share);
            s.effective_cores = Some(share * working as f64);
            s.processors = processors;
        });
        let now = Instant::now();
        if share >= 0.6 || lanes < 2 {
            self.starved_since = None;
            return;
        }
        let since = *self.starved_since.get_or_insert(now);
        if now.duration_since(since) < Duration::from_secs(60) || self.starved_said.is_some_and(|t| now.duration_since(t) < Duration::from_secs(600)) {
            return;
        }
        self.starved_said = Some(now);
        let why = if crate::affinity::active() == Some(crate::affinity::Prefer::Windows) {
            "；Intel 大小核上 Windows 会把后台程序挤在 E 核，把 core_type 改成 auto 试试"
        } else {
            "，多半是被别的程序占用了（simfleet 只用别人不用的 CPU）"
        };
        self.ui.log(format!(
            "{C_GO}[CPU]{C_END} {lanes} 条计算线程每条只拿到 {:.0}% 的算力（分布在 {processors} 个逻辑处理器上）{why}",
            share * 100.0
        ));
    }

    fn measure(&mut self) {
        self.lane_yield();
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
                self.riven_verdict();
                self.follow_engine();
                self.maybe_native();
            }
            if self.tasks.is_empty() && self.sending.is_empty() {
                if self.stats_at.is_none_or(|t| t.elapsed() > STATS_EVERY) {
                    self.refresh_stats();
                }
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
        if !self.start(0, v, w.clone(), None) {
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
        if let Some((rel, code)) = self.switch_to.take() {
            self.stale = false;
            self.take_engine(rel, code);
            return Ok(());
        }
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

    /// The site names `engine`, not ours: make it ready on a thread of its
    /// own — the wasm, and a native build if one is published (simfleet's, or
    /// WFSim's own) — while the lanes go on with the old one.
    fn served_moved(&mut self, engine: String, until: String) {
        let busy = self.engine_load.as_ref().is_some_and(|(e, _)| *e == engine) || self.switch_to.as_ref().is_some_and(|(r, _)| r.engine == engine);
        if busy || Instant::now() < self.engine_retry {
            return;
        }
        let kept = if until.is_empty() { "本机的引擎不再被接受".to_string() } else { format!("本机的引擎在 {until} 前仍被接受") };
        self.ui.log(format!("{C_INFO}[引擎]{C_END} 网站引擎已更新 {} → {engine}；{kept}，新引擎在后台准备，准备好后接着用", self.rel.engine));
        let (net, paths, cfg, ui) = (self.net.clone(), self.paths.clone(), self.cfg.clone(), self.ui.clone());
        let h = std::thread::spawn(move || {
            let rel = fetch_release(&net).ok()?;
            let code = load_code(&net, &paths, &cfg, &ui, &rel).ok()?;
            Some((rel, code))
        });
        self.engine_load = Some((engine, h));
    }

    /// A prepared engine is taken up once nothing is held on the old one: no
    /// more is asked meanwhile, and what is held is finished (and answered) first.
    fn follow_engine(&mut self) {
        // THE SITE'S OWN WORD, read every minute: what it serves, and until when
        // it still takes ours (`accepted`).
        if self.release_read.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(Some(r)) = self.release_read.take().and_then(|h| h.join().ok()) {
                if let Some(e) = r["engine"].as_str().filter(|e| *e != self.rel.engine && e.len() == 12) {
                    let until = r["accepted"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|a| a["engine"].as_str() == Some(self.rel.engine.as_str()))
                        .and_then(|a| a["until"].as_str())
                        .unwrap_or("")
                        .to_string();
                    self.served_moved(e.to_string(), until);
                }
            }
        }
        if self.release_read.is_none() && !self.offline && self.release_at.elapsed() >= RELEASE_EVERY {
            self.release_at = Instant::now();
            let net = self.net.clone();
            self.release_read = Some(std::thread::spawn(move || net.get("/release.json").ok().filter(|r| r.status == 200).map(|r| r.body)));
        }
        if self.engine_load.as_ref().is_some_and(|(_, h)| h.is_finished()) {
            let (engine, h) = self.engine_load.take().expect("checked above");
            match h.join().ok().flatten() {
                Some((rel, code)) if rel.engine != self.rel.engine => {
                    self.ui.log(format!(
                        "{C_INFO}[引擎]{C_END} 新引擎 {}（{}）已准备好：手上的活做完就换上",
                        rel.engine,
                        crate::work::backend_label(&code)
                    ));
                    self.switch_to = Some((rel, code));
                }
                // The site's page still names ours (a cache): asked again in a minute.
                Some(_) => self.engine_retry = Instant::now() + Duration::from_secs(60),
                None => {
                    self.engine_retry = Instant::now() + Duration::from_secs(600);
                    self.ui.log(format!("{C_GO}[引擎]{C_END} 新引擎 {engine} 没能准备好，10 分钟后再试；旧引擎照常计算"));
                }
            }
        }
        let idle = self.tasks.is_empty() && self.packs.is_empty() && self.sending.is_empty() && self.groups.iter().all(|g| g.asking.is_none());
        if idle {
            if let Some((rel, code)) = self.switch_to.take() {
                self.take_engine(rel, code);
            }
        }
    }

    fn take_engine(&mut self, rel: Release, code: Arc<Code>) {
        self.ui.log(format!("{C_INFO}[引擎]{C_END} 已换上网站引擎 {} → {}", self.rel.engine, rel.engine));
        self.swap_code(code);
        self.ui.status(|s| s.engine = rel.engine.clone());
        self.rel = rel;
    }

    fn swap_code(&mut self, code: Arc<Code>) {
        self.riven_seen = 0;
        let backend = crate::work::backend_label(&code);
        self.pool = Pool::new(code.clone());
        self.meter.rebase();
        self.tag_task.clear();
        self.exec = Exec::new(code);
        self.ui.status(|s| s.backend = backend.into());
    }

    /// ON THE WASM, look again every ten minutes for a published native build of
    /// the served engine (it appears some minutes or hours after the engine
    /// does). Fetched and checked on a thread of its own, and taken up while the
    /// lanes work: the wasm is the same, so nothing running is disturbed — a
    /// riven gain begun on the wasm ends there, and what starts next runs natively.
    fn maybe_native(&mut self) {
        // What the lanes have now: nothing native, or WFSim's own library (which
        // simfleet's build, with riven gains and AVX2, replaces once published).
        let have = self.pool.code().native.as_ref().map(|n| n.official);
        if self.native_load.as_ref().is_some_and(|(_, h)| h.is_finished()) {
            let (engine, h) = self.native_load.take().expect("checked above");
            if let Ok(Some(code)) = h.join() {
                let better = match (have, code.native.as_ref().map(|n| n.official)) {
                    (None, Some(_)) | (Some(true), Some(false)) => true,
                    _ => false,
                };
                if engine == self.rel.engine && better && self.switch_to.is_none() {
                    self.riven_seen = 0;
                    self.pool.adopt(code.clone());
                    self.exec.adopt(code.clone());
                    let label = crate::work::backend_label(&code);
                    self.ui.status(|s| s.backend = label.into());
                }
            }
        }
        if self.native_load.is_some() || self.engine_load.is_some() || self.switch_to.is_some() || have == Some(false) || !self.cfg.native_engine
            || self.native_checked.elapsed() < Duration::from_secs(600)
        {
            return;
        }
        self.native_checked = Instant::now();
        // `load_code` fetches whichever native build is published by now — with
        // WFSim's library in use, only once simfleet's own is.
        let (net, paths, cfg, ui, rel) = (self.net.clone(), self.paths.clone(), self.cfg.clone(), self.ui.clone(), self.rel.clone());
        let engine = rel.engine.clone();
        self.native_load = Some((
            engine,
            std::thread::spawn(move || {
                if have == Some(true) && !crate::work::ours_published(&net, &cfg, &rel) {
                    return None;
                }
                load_code(&net, &paths, &cfg, &ui, &rel).ok()
            }),
        ));
    }

    /// The bottom line's numbers, summed over this machine's ids — asked on a
    /// thread of their own, so a slow site never holds the lanes up. A failed
    /// ask keeps what was shown; only a session the site refuses is signed out.
    fn refresh_stats(&mut self) {
        if self.stats_job.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        self.stats_at = Some(Instant::now());
        let since = sys::local_midnight_utc_hour();
        let groups: Vec<(String, Vec<Value>, Vec<Value>)> = self
            .groups
            .iter()
            .map(|g| {
                let mine = |riven: bool| -> Vec<Value> {
                    self.recent.iter().filter(|(v, k, _)| *v == g.verifier && *k == riven).map(|(_, _, x)| x.clone()).collect()
                };
                (g.verifier.clone(), mine(false), mine(true))
            })
            .collect();
        let (net, ui, name) = (self.net.clone(), self.ui.clone(), self.cfg.account.name.clone());
        self.stats_job = Some(std::thread::spawn(move || {
            let (mut device, mut today, mut any) = (0u64, 0u64, false);
            for (v, _, _) in &groups {
                if let Some(p) = net.post_ok("/api/board/points", &json!({ "verifier": v, "since": since })) {
                    any = true;
                    device += p["points"].as_u64().unwrap_or(0);
                    today += p["today"]["points"].as_u64().unwrap_or(0);
                }
            }
            let dev = net.session.as_ref().map(|_| account::devices(&net));
            // THE SITE'S SIDE - who is computing, what is owed, the week's ranking;
            // the ranking is the page's own (`/api/contributors`).
            let demand = net.get("/api/board/demand").ok().map(|r| r.body).filter(|b| b["ok"] == json!(true));
            let top = net.get("/api/contributors?period=week&limit=40").ok().map(|r| r.body).filter(|b| b["ok"] == json!(true));
            let (mut confirmed, mut waiting, mut told) = (0u64, 0u64, false);
            for (v, orders, tasks) in &groups {
                if !orders.is_empty() {
                    if let Some(m) = net.post_ok("/api/board/mine", &json!({ "verifier": v, "orders": orders })) {
                        told = true;
                        for o in m["orders"].as_array().into_iter().flatten() {
                            match o["state"].as_str() {
                                Some("confirmed") => confirmed += 1,
                                Some("waiting") | Some("checking") => waiting += 1,
                                _ => {}
                            }
                        }
                    }
                }
                if !tasks.is_empty() {
                    if let Some(m) = net.post_ok("/api/appraise/mine", &json!({ "verifier": v, "tasks": tasks })) {
                        told = true;
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
            ui.status(|s| {
                if any {
                    s.device_points = Some(device);
                    s.today_points = Some(today);
                }
                match &dev {
                    Some(Ok(d)) => {
                        s.account = Some(if name.is_empty() { "已登录".into() } else { name });
                        s.account_points = d["points"].as_u64();
                        s.contributor_rank = d["contributor_rank"]["rank"].as_u64();
                    }
                    // The site refused the session: signed out.
                    Some(Err(e)) if format!("{e}").contains("HTTP 401") => {
                        s.account = None;
                        s.account_points = None;
                    }
                    // Anything else (a slow or failing site) keeps what was shown.
                    Some(Err(_)) => {}
                    None => {
                        s.account = None;
                        s.account_points = None;
                    }
                }
                if let Some(d) = &demand {
                    s.computing_now = d["computing"].as_u64();
                    s.relayed = net.relayed();
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
                if told || groups.iter().all(|(_, o, t)| o.is_empty() && t.is_empty()) {
                    s.confirmed = confirmed;
                    s.waiting = waiting;
                }
            });
        }));
    }
}

/// A SEND THAT GOT NO REPLY IS SENT AGAIN (69-board-work.js `sendAgain`), a
/// computed pack being the one thing a dropped connection must not cost: the
/// server takes a device's answer to a question once, so a reply lost after the
/// answers landed does no harm.
const SEND_AGAIN: [Duration; 3] = [Duration::from_secs(3), Duration::from_secs(10), Duration::from_secs(30)];

fn send_answers(net: &Net, body: &Value) -> bool {
    for i in 0.. {
        if net.post_ok("/api/board/answers", body).is_some_and(|r| r["ok"] == json!(true)) {
            return true;
        }
        let Some(wait) = SEND_AGAIN.get(i) else { return false };
        std::thread::sleep(*wait);
    }
    false
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
