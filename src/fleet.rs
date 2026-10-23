// SPDX-License-Identifier: AGPL-3.0-or-later
//! THE DISPATCHER: every core busy, within the budget, on as many tasks as that takes.
//!
//! A task does not fill a machine on its own. A riven gain's quick descent is a
//! chain of LEAD steps on one core with the round's scoring between them, so on
//! sixteen lanes one search keeps about half of them busy (measured: 55%); a
//! board row parallelises across its runs but ends in a tail, and between tasks
//! there is a round trip to the server. So:
//!
//! - BOARD ROWS COME IN PACKS, sized by the pace the client reports, and a
//!   pack's rows start as lanes come free — one row's tail is the next row's
//!   start. PROTOCOL 8 (sync.rs): a pack is about ten minutes of the id's work;
//!   its results are kept on disk and handed in by the id's next sync, which
//!   also renews every row the id still holds and asks for more as the work
//!   runs low; an id holding nothing reads the work signal and syncs when it
//!   shows work. PROTOCOL 7, where the server asks for it: a pack is about a
//!   minute, its answers go back every half minute and as it ends
//!   (`/api/board/answers`), each row renewed apart. A riven gain comes alone.
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
//! - THE NETWORK IS NEVER WAITED ON: syncs, asks, renewals and answers run on
//!   their own threads. A protocol 8 sync gives back every row of its id it
//!   does not name, so it names them all, and an id computing a riven gain
//!   (which no sync can name) does not sync until its answer is in; a protocol
//!   7 plain ask (not `ahead`) does the same, so only an id holding nothing
//!   and sending nothing sends one.
//! - ONE ID, ONE PROCESS (sys.rs `lock_id`): a simfleet started twice on one
//!   config takes ids the other is not using.
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
use crate::sync::{Dials, Journal, Kept, Out, Signal};
use crate::ui::{group, points, Ctl, GroupView, Ranked, TaskLine, Ui, C_BAD, C_END, C_GO, C_INFO, C_OK};
use crate::work::{board_payload, fetch_release, load_code, truthy, BoardRun, Release, RowScore, RIVEN_RENEW, STATS_EVERY};
use crate::{account, jsnum, sync, sys};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
/// A lane given nothing for this long ends its thread (`rest_lanes`).
const LANE_IDLE: Duration = Duration::from_secs(60);
/// SHORT OF MEMORY: the system's free physical memory under this share of it,
/// and never under `MEM_LOW_MIN` (`watch_memory`).
const MEM_LOW_SHARE: u64 = 10;
const MEM_LOW_MIN: u64 = 1 << 30;

// ---- one task's computation, as jobs ------------------------------------------

/// A RIVEN GAIN (08-checkpoint-api.js `quickFleet`): lead steps on the leader's
/// instance, each round's builds scored in slices — two a lane — on any lane.
struct Riven {
    /// Where its search runs, fixed when it is given its leader: its lead's
    /// state lives there (a wasm instance, or a native lane's thread).
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
    fn new(request: Value) -> Riven {
        Riven {
            backend: Backend::Wasm,
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
            Runner::Board(b) => TaskLine { done: b.folded as u64, total: b.runs as u64, extra: if b.gpu_pending {"NVIDIA GPU".into()}else{String::new()}, ..Default::default() },
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
    claiming: Option<(Instant,JoinHandle<Result<()>>)>,
    /// Answers the server took from it this run, and their work.
    done: u64,
    work: u64,
    /// A row's wall time in this group, smoothed (the `pace_ms` a pack is sized
    /// by): rows finishing side by side count the time between them.
    pace_ms: f64,
    last_row: Option<Instant>,
    /// When the server last had nothing for it.
    nothing_at: Option<Instant>,
    // PROTOCOL 8 (sync.rs):
    /// The results it keeps and those taken but unsettled, on disk.
    journal: Journal,
    journal_dirty: bool,
    /// When it may next be handed work (`next_ms`), and when a failed sync may go again.
    not_before: Instant,
    retry_at: Instant,
    failures: u32,
    last_sync: Option<Instant>,
    /// Holding nothing: when it syncs on the signal (its jitter), and when without one.
    go_at: Option<Instant>,
    fallback_at: Instant,
    syncing: Option<Syncing>,
    /// Rows its engine refused, said in the next sync (`refused`).
    refused_rows: Vec<Value>,
    /// What the server last said it earned (`points`), and when.
    points: Option<(Instant, Value)>,
    /// To sync soon, giving back what it no longer computes.
    give_back: bool,
    last_received: Option<Instant>,
    wait_reason: String,
}

/// A protocol 8 sync on its way…
struct Syncing {
    handle: JoinHandle<Out>,
    sent: Sent,
}

/// …and what it carried: the results (by question), the refused rows, the
/// packs it named, and the work it asked for.
struct Sent {
    at: Instant,
    engine: String,
    want: u64,
    results: Vec<String>,
    refused: Vec<String>,
    leases: Vec<String>,
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
    /// THE BOARD'S PROTOCOL this run: 8, or 7 once the server or the config says so.
    proto: u32,
    dials: Dials,
    /// The work signal as last read (`None` inside: not to be believed) and
    /// when; a read on its way, and when the next may go.
    signal: Option<(Instant, Option<Signal>)>,
    signal_job: Option<JoinHandle<Option<Value>>>,
    signal_next: Instant,
    /// Board results of this run another computer confirmed (`states`).
    board_confirmed: u64,
    journals_at: Instant,
    journal_failed: bool,
    /// The ids this process holds (sys.rs `lock_id`), and those another holds.
    id_locks: Vec<sys::IdLock>,
    ids_taken: HashSet<String>,
    /// A newer protocol than this client's has been said.
    newer_said: bool,
    banned_until: Option<String>,
    /// …when the cool-down ends, in ms since the epoch.
    banned_end: Option<u64>,
    /// The server's word when it hands this client no work at all (`unrecognised_client`).
    refused: Option<String>,
    recent: Vec<(String, bool, Value)>,
    stats_at: Option<Instant>,
    native_checked: Instant,
    /// A native build being fetched and checked, off the dispatcher: for the engine named.
    native_load: Option<(String, JoinHandle<Option<Arc<Code>>>)>,
    /// The bottom line's numbers being asked for (`refresh_stats`).
    stats_job: Option<JoinHandle<()>>,
    /// The ranking as it was seen, a refresh at a time (`paces`).
    rank_seen: Arc<std::sync::Mutex<VecDeque<(Instant, HashMap<String, u64>)>>>,
    /// `/release.json` being read, and when it last was (`follow_engine`).
    release_read: Option<JoinHandle<Option<Value>>>,
    release_at: Instant,
    /// When "the site is busy" was last said.
    busy_said: Option<Instant>,
    /// THE ENGINE THE SITE SERVES NOW, being made ready off the dispatcher…
    engine_load: Option<(String, JoinHandle<Result<crate::engine_update::Prepared>>)>,
    /// …ready: no more work is asked, and once what is held is done, it runs.
    switch_to: Option<(Release, Arc<Code>)>,
    /// When a failed preparation may be tried again.
    engine_retry: Instant,
    control_at: Instant,
    control_busy: f64,
    control_cap: f64,
    /// When the group count last went down.
    closed_at: Option<Instant>,
    /// THE CAP while the system is short of memory, when it last came down,
    /// and when free memory was last under twice the line (`watch_memory`).
    mem_cap: Option<usize>,
    mem_at: Option<Instant>,
    mem_tight_at: Option<Instant>,
    /// The riven canary's verdict as last seen (engine.rs `riven_state`).
    riven_seen: u8,
    /// A board row's cost in lane milliseconds, smoothed: what the work held is measured in.
    row_cost_ms: f64,
    meter: Meter,
    /// The process's CPU time and the lanes' working time when last read (µs), and when.
    yield_seen: (Instant, u64, u64),
    /// Since when the lanes have been given too little, and when that was last said.
    starved_since: Option<Instant>,
    starved_said: Option<Instant>,
    pub session_dropped: bool,
    pub offline: bool,
    /// Offline replay can lower reported availability without filling host RAM.
    replay_memory_available: Option<u64>,
    memory_guard: crate::maintenance::MemoryGuard,
    memory_checked: Instant,
    /// The drain finished and main may replace this process after saving config.
    pub recycle: bool,
    settings_dirty: bool,
    settings_save_at: Instant,
    settings_restart: Option<Instant>,
    settings_original: crate::settings::Settings,
    accepted_until: Option<u64>,
    waiting_said: Option<String>,
    gpu: Option<crate::gpu::Manager>,
    gpu_state: String,
    gpu_restart:Option<Instant>,
    gpu_restarts:u32,
    gpu_recovery:crate::gpu::Recovery,
    trace:crate::dispatch_trace::Trace,
}

impl Fleet {
    pub fn new(net: Net, cfg: Config, paths: Paths, ctl: Arc<Ctl>, ui: Arc<Ui>, rel: Release, code: Arc<Code>) -> Fleet {
        let backend = crate::work::backend_label(&code);
        ui.status(|s| {
            s.engine = rel.engine.clone();
            s.backend = backend.into();
        });
        let proto = if cfg.protocol == 7 { 7 } else { sync::PROTOCOL };
        let memory_guard = crate::maintenance::MemoryGuard::new(cfg.memory_limit_mb);
        let settings_original=crate::settings::Settings::from_config(&cfg);
        let ids: Vec<String> = std::iter::once(cfg.verifier.clone()).chain(cfg.extra_verifiers.iter().cloned()).collect();
        let mut f = Fleet {
            trace:crate::dispatch_trace::Trace::new(&paths.dir),
            gpu: None,
            gpu_state: String::new(),
            gpu_restart:None,
            gpu_restarts:0,
            gpu_recovery:crate::gpu::Recovery::default(),
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
            proto,
            dials: Dials::default(),
            signal: None,
            signal_job: None,
            signal_next: Instant::now(),
            board_confirmed: 0,
            journals_at: Instant::now(),
            journal_failed: false,
            id_locks: Vec::new(),
            ids_taken: HashSet::new(),
            newer_said: false,
            banned_until: None,
            banned_end: None,
            refused: None,
            recent: Vec::new(),
            stats_at: None,
            native_checked: Instant::now(),
            native_load: None,
            stats_job: None,
            rank_seen: Arc::default(),
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
            mem_cap: None,
            mem_at: None,
            mem_tight_at: None,
            row_cost_ms: 0.0,
            riven_seen: 0,
            memory_guard,
            settings_dirty:false,
            settings_save_at:Instant::now()-Duration::from_secs(2),
            settings_restart:None,
            settings_original,
            accepted_until:None,
            waiting_said:None,
            memory_checked: Instant::now(),
            recycle: false,
            meter: Meter::new(),
            yield_seen: (Instant::now(), sys::process_cpu_us(), crate::affinity::lane_totals().1),
            starved_since: None,
            starved_said: None,
            session_dropped: false,
            offline: false,
            replay_memory_available: None,
        };
        if f.cfg.slots > 0 {
            f.open = f.cfg.slots;
        }
        if f.proto == 7 {
            f.ui.log(format!("{C_INFO}[协议]{C_END} 按配置使用协议 7"));
        }
        f.ensure_groups(f.open);
        // IDS WITH RESULTS LEFT by a run that ended before handing them in get
        // their groups too, which hand them in (one past `open` takes no work).
        if let Some(last) = ids.iter().rposition(|v| !Journal::load(&f.paths.dir, v).results.is_empty()) {
            f.ensure_groups(last + 1);
        }
        f
    }

    /// Up to `n` groups, each with an id of this config another simfleet does
    /// not hold (`free_id`), and what that id kept (sync.rs `Journal`).
    fn ensure_groups(&mut self, n: usize) {
        while self.groups.len() < n {
            let verifier = self.free_id();
            let journal = Journal::load(&self.paths.dir, &verifier);
            if !journal.results.is_empty() {
                self.ui.glog(
                    self.groups.len(),
                    format!("{C_INFO}[结果]{C_END} 上次运行有 {} 行结果没交上，这次补交", journal.results.len()),
                );
            }
            let now = Instant::now();
            self.groups.push(Group {
                verifier,
                asking: None,
                next_ask: now,
                next_ahead: now,
                claimed: false,
                claiming: None,
                done: 0,
                work: 0,
                pace_ms: journal.pace_ms.max(0.0),
                last_row: None,
                nothing_at: None,
                journal,
                journal_dirty: false,
                not_before: now,
                retry_at: now,
                failures: 0,
                last_sync: None,
                go_at: None,
                fallback_at: now,
                syncing: None,
                refused_rows: Vec::new(),
                points: None,
                give_back: false,
                last_received: None,
                wait_reason: String::new(),
            });
        }
    }

    /// THE NEXT ID of this config that no other simfleet holds now: the
    /// config's own, then its extras, one more made (and saved) when they are
    /// all in use. Another process holding one is the same config run twice;
    /// two of them on one id would each give back what the other holds.
    fn free_id(&mut self) -> String {
        let mut i = 0;
        loop {
            if i > 0 && self.cfg.extra_verifiers.len() < i {
                self.paths.merge_extras(&mut self.cfg);
                if self.cfg.extra_verifiers.len() < i {
                    self.cfg.extra_verifiers.push(sys::random_hex(12));
                    let _ = self.paths.save(&self.cfg);
                }
            }
            let id = if i == 0 { self.cfg.verifier.clone() } else { self.cfg.extra_verifiers[i - 1].clone() };
            i += 1;
            if self.groups.iter().any(|g| g.verifier == id) || self.ids_taken.contains(&id) {
                continue;
            }
            match sys::lock_id(&id) {
                sys::IdUse::Mine(l) => {
                    self.id_locks.push(l);
                    return id;
                }
                sys::IdUse::Unknown => return id,
                sys::IdUse::Taken => {
                    self.ui.log(format!(
                        "{C_GO}[多开]{C_END} 计算者 id {}… 正被另一个 simfleet 使用（同一份配置开了两次），本进程改用别的 id",
                        id.get(..6).unwrap_or(&id)
                    ));
                    self.ids_taken.insert(id);
                }
            }
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
        (if self.idle() {
            self.ctl.idle_cores.load(Ordering::Relaxed).max(1)
        } else {
            self.ctl.active_cores.load(Ordering::Relaxed).max(1)
        }).min(self.ctl.max_cores)
    }
    /// THE CAP on groups: `max_slots` (0: by the machine's size), and no more
    /// than half the lanes the budget allows now — one group's pack starts
    /// rows on every lane — nor than memory allows (`watch_memory`).
    fn max_groups(&self) -> usize {
        let cap = match self.ctl.max_slots.load(Ordering::Relaxed) {
            0 => crate::config::auto_max_slots(self.ctl.max_cores),
            n => n,
        };
        (self.lanes() / 2).max(1).min(cap).min(self.mem_cap.unwrap_or(usize::MAX))
    }
    fn backend(&self) -> Backend {
        if self.pool.code().native.is_some() { Backend::Native } else { Backend::Wasm }
    }
    /// A RIVEN GAIN natively when the build has the optimizer and gave the
    /// wasm's answer on the riven canary (work.rs), unless `native_riven` is off.
    /// None while that canary runs (a dozen seconds): a search begun on the
    /// wasm ends there, with an instance on every lane it scores on.
    fn riven_backend(&self) -> Option<Backend> {
        match self.pool.code().native.as_ref() {
            Some(n) if self.cfg.native_riven && n.has_optimize() => match n.riven_state() {
                1 => None,
                2 => Some(Backend::Native),
                _ => Some(Backend::Wasm),
            },
            _ => Some(Backend::Wasm),
        }
    }

    /// WHAT THIS ID CAN GIVE: its share of the lanes, as one browser among several says its own.
    fn id_lanes(&self) -> usize {
        (self.lanes() / self.open.max(1)).max(1)
    }

    /// A protocol 7 ask (`/api/board/work`).
    fn ask_body(&self, gi: usize, ahead: bool) -> Value {
        let lanes = self.id_lanes();
        let g = &self.groups[gi];
        let mut b = json!({
            "verifier": g.verifier,
            "engine": self.rel.engine,
            "protocol": 7,
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
        self.let_go(&t);
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

    /// A pack with nothing left to start or finish: protocol 7's last answers,
    /// `done`. A protocol 8 pack's results are kept already, and a row it did
    /// not reach goes back with the id's next sync, which no longer names it —
    /// sent soon for it.
    fn maybe_end_pack(&mut self, pid: u64) {
        let Some(pi) = self.packs.iter().position(|p| p.id == pid && p.rows.is_empty() && p.running == 0) else { return };
        if self.proto == 7 {
            return self.send_pack(pid, true);
        }
        let p = self.packs.remove(pi);
        self.trace.event(json!({"event":"pack_finished","group":p.group+1,"rows":p.total,"answered":p.answered,"next_ms":self.groups[p.group].not_before.saturating_duration_since(Instant::now()).as_millis(),"pace_ms":self.groups[p.group].pace_ms}));
        if p.answered < p.total {
            self.groups[p.group].give_back = true;
            self.ui.glog(p.group, format!("{C_GO}[交还]{C_END} 本包还有 {} 行没算，随下次同步交还", p.total - p.answered));
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
            // Protocol 8: what is no longer computed goes back with a sync soon.
            self.groups[p.group].give_back = true;
        }
        let ids: Vec<u64> = self.tasks.iter().map(|t| t.id).collect();
        for id in ids {
            self.drop_task(id, why, true);
        }
        let packs: Vec<u64> = self.packs.iter().map(|p| p.id).collect();
        for pid in packs {
            if self.proto == 7 {
                self.send_pack(pid, true);
            } else {
                self.maybe_end_pack(pid);
            }
        }
        for g in &mut self.groups {
            g.give_back |= !g.journal.results.is_empty();
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
        self.stop_gpu();
        self.drop_all("，任务已交还");
        // The last answers count in the closing line too — sent again for up to
        // three quarters of a minute when the site does not answer.
        if !self.sending.is_empty() || (self.proto == 8 && !self.offline) {
            self.ui.status(|st| st.state = "正在交最后的答案（连不上服务器时会重试，最多约 45 秒）".into());
        }
        self.join_sending();
        // …and then each id's last sync: a riven gain's answer is in by now,
        // which a sync naming nothing would have given back.
        if self.proto == 8 && !self.offline {
            self.last_syncs();
            // (A server that said protocol 7 meanwhile was sent the results as answers.)
            self.join_sending();
        }
        self.save_journals();
        let (rows, _) = self.kept();
        self.ui.status(|st| st.kept_rows = rows);
    }

    /// Every answer on its way, waited for and counted.
    fn join_sending(&mut self) {
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
            if self.groups[gi].asking.is_some() || self.offline || self.stale || self.switch_to.is_some() || self.memory_guard.draining() || self.settings_restart.is_some() || self.banned_until.is_some() || self.refused.is_some() {
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
                if !self.claim(gi) {continue;}
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
            self.served_moved(e.to_string(), r["accepted_until"].as_str().map(String::from));
        }
        if r["banned"] == json!(true) {
            return self.cool_down(&r);
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

    /// A COOL-DOWN (`banned: true, until`): no work is asked for until it ends —
    /// the next ask after it lifts it (docs/CLIENTS.md, "a penalty is a
    /// cool-down"). An end the reply does not say is taken as an hour.
    fn cool_down(&mut self, r: &Value) {
        let until = r["until"].as_str().unwrap_or("?").to_string();
        if self.banned_until.is_none() {
            self.ui.log(format!("{C_BAD}[冷却]{C_END} 服务器复核时与本机结果不一致，暂停派活至 {until}"));
        }
        self.banned_end = Some(sys::parse_iso(&until).unwrap_or(sys::now_ms() + 3_600_000));
        self.banned_until = Some(until);
    }

    /// …and its end.
    fn cool_down_over(&mut self) {
        if self.banned_end.is_some_and(|t| sys::now_ms() >= t) {
            self.banned_end = None;
            self.banned_until = None;
            self.ui.log(format!("{C_INFO}[冷却]{C_END} 冷却结束，重新领活"));
        }
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
            (Runner::Riven(Riven::new(w["request"].clone())), label, Some(code))
        } else {
            let (ruler, mode) = (w["ruler"].as_str().unwrap_or("").to_string(), w["mode"].as_str().unwrap_or("").to_string());
            let label = format!("{} · {ruler} · {mode}", w["record"]["weapon"].as_str().unwrap_or("?"));
            let backend = self.backend();
            let body = json!({ "record": w["record"], "ruler": ruler, "mode": mode }).to_string();
            match self.exec.api(backend, "/api/board/order", body).and_then(|t| jsnum::parse(&t)) {
                Ok(o) if o["ok"] != json!(false) => {
                    let runs = o["request"]["runs"].as_u64().unwrap_or(0);
                    self.ui.glog(gi, format!("{C_GO}[开始]{C_END} 榜单  {label}  {runs} 次模拟"));
                    let mut row = BoardRun::new(&o["request"], &ruler);
                    if backend == Backend::Native { row.enable_native(self.pool.code().clone(), &o["request"]); }
                    (Runner::Board(row), label, None)
                }
                // A ROW THIS ENGINE REFUSES (protocol 8): said so in the next
                // sync, given back, and never handed here again on this engine.
                Ok(o) if pack.is_some() && self.proto == 8 => {
                    self.groups[gi].refused_rows.push(json!({ "lease": lease, "question": w["question"], "step": "order" }));
                    self.ui.glog(gi, format!("{C_BAD}[跳过]{C_END} 榜单 {label}：引擎不接受这一行（{}），随下次同步报告", o["error"].as_str().unwrap_or("?")));
                    return false;
                }
                Ok(o) => {
                    if !self.offline && pack.is_none() {
                        Self::release_lease(&self.net, &verifier, &lease, None);
                    }
                    self.ui.glog(gi, format!("{C_BAD}[跳过]{C_END} 榜单 {label}：/api/board/order: {}", o["error"].as_str().unwrap_or("engine refused")));
                    return false;
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

    fn claim(&mut self, gi: usize)->bool {
        if self.groups[gi].claimed || self.net.session.is_none(){return true;}
        if self.groups[gi].claiming.is_none() {
            let (net,id)=(self.net.clone(),self.groups[gi].verifier.clone());
            self.groups[gi].claiming=Some((Instant::now(),std::thread::spawn(move||account::claim_as(&net,&id,gi))));
            self.trace.event(json!({"event":"claim_start","group":gi+1}));
            return false;
        }
        if !self.groups[gi].claiming.as_ref().is_some_and(|(_,h)|h.is_finished()){return false;}
        let (at,job)=self.groups[gi].claiming.take().expect("finished claim");
        self.groups[gi].claimed = true;
        let result=job.join().unwrap_or_else(|_|Err(anyhow::anyhow!("设备绑定线程异常")));
        self.trace.event(json!({"event":"claim_done","group":gi+1,"elapsed_ms":at.elapsed().as_millis(),"ok":result.is_ok()}));
        match result {
            Ok(()) => {}
            Err(e) if e.to_string().contains("401") => {
                self.ui.log(format!("{C_BAD}[账号]{C_END} 登录已失效，贡献暂记在本机；运行 simfleet login 重新登录"));
                self.net.session = None;
                self.session_dropped = true;
            }
            Err(e) => self.ui.log(format!("{C_BAD}[账号]{C_END} {e:#}")),
        }
        true
    }

    // ---- protocol 8 ---------------------------------------------------------------

    /// EVERY ROW AN ID HOLDS UNANSWERED, by lease — those not started and those
    /// running — named in full in each of its syncs.
    fn held_rows(&self, gi: usize) -> Vec<(String, Vec<String>)> {
        self.packs
            .iter()
            .filter(|p| p.group == gi)
            .filter_map(|p| {
                let mut rows: Vec<String> = p.rows.iter().filter_map(|w| w["question"].as_str().map(String::from)).collect();
                rows.extend(self.tasks.iter().filter(|t| t.pack == Some(p.id)).map(|t| t.question.clone()));
                (!rows.is_empty()).then(|| (p.lease.clone(), rows))
            })
            .collect()
    }

    fn holds_nothing(&self, gi: usize) -> bool {
        !self.packs.iter().any(|p| p.group == gi) && !self.tasks.iter().any(|t| t.group == gi)
    }

    /// The results every id keeps, not taken yet: rows and work.
    fn kept(&self) -> (u64, u64) {
        self.groups.iter().fold((0, 0), |(r, w), g| (r + g.journal.results.len() as u64, w + g.journal.kept_work()))
    }

    /// The journals changed since they were last written, written.
    fn save_journals(&mut self) {
        self.journals_at = Instant::now();
        for g in &mut self.groups {
            if std::mem::take(&mut g.journal_dirty) {
                if let Err(e) = g.journal.save(&self.paths.dir) {
                    g.journal_dirty = true;
                    if !self.journal_failed {
                        self.ui.log(format!("{C_BAD}[结果]{C_END} 无法把结果写进 {}：{e}", self.paths.dir.join("kept").display()));
                    }
                    self.journal_failed = true;
                }
            }
        }
        self.journal_failed = self.groups.iter().any(|g| g.journal_dirty);
    }

    /// A PROTOCOL NEWER than this client's: it keeps working, and says so once.
    fn note_protocol(&mut self, r: &Value) {
        let newest = r["protocol"].as_u64().unwrap_or(0);
        if newest > sync::PROTOCOL as u64 && !self.newer_said {
            self.newer_said = true;
            self.ui.log(format!("{C_GO}[协议]{C_END} 服务器已有协议 {newest}（simfleet 用 {}），照常计算；新版 simfleet 会跟上", self.proto));
        }
    }

    /// THE WORK SIGNAL, read every `poll_ms` (give or take a quarter) on a
    /// thread of its own while an id holds nothing and may be handed work. An
    /// engine it names other than ours is made ready as release.json's would be.
    fn read_signal(&mut self) {
        if self.signal_job.as_ref().is_some_and(|h| h.is_finished()) {
            let v = self.signal_job.take().and_then(|h| h.join().ok()).flatten();
            let s = v.and_then(|v| sync::signal(&v, self.net.server_now_ms(), &mut self.dials));
            if let Some(e) = s.as_ref().map(|s| s.engine.clone()).filter(|e| e.len() == 12 && *e != self.rel.engine) {
                self.served_moved(e, None);
            }
            self.signal = Some((Instant::now(), s));
        }
        let now = Instant::now();
        let wanted = (0..self.groups.len().min(self.open)).any(|gi| {
            let g = &self.groups[gi];
            // CDN signals have their own poll interval. Keep one fresh while
            // the grant gate runs, so expiry does not start another HTTP wait.
            g.syncing.is_none() && now >= g.retry_at && self.holds_nothing(gi)
        });
        if wanted && self.signal_job.is_none() && now >= self.signal_next {
            self.signal_next = now + sync::jittered(self.dials.poll_ms);
            let net = self.net.clone();
            self.signal_job = Some(std::thread::spawn(move || net.get_timeout(&net.public_file(sync::SIGNAL_PATH),Duration::from_secs(5)).ok().filter(|r| r.status == 200).map(|r| r.body)));
        }
    }

    /// WHETHER AN ID HOLDING NOTHING SYNCS FOR WORK now: `None` until a signal
    /// has been read lately; with work showing, once its jitter has passed;
    /// with no signal to believe, every `FALLBACK_MS` or so.
    fn idle_go(&mut self, gi: usize, now: Instant) -> Option<bool> {
        let read = self.signal.as_ref().filter(|(t, _)| now.duration_since(*t) < Duration::from_millis(3 * self.dials.poll_ms))?;
        let g = &mut self.groups[gi];
        Some(match &read.1 {
            None => g.last_sync.is_none() || now >= g.fallback_at,
            Some(s) if s.urgent > 0 || s.open > 0 => {
                let wait = if s.urgent > 0 { self.dials.urgent_jitter_ms } else { self.dials.jitter_ms };
                now >= *g.go_at.get_or_insert_with(|| now + Duration::from_millis((sys::random_unit() * wait as f64) as u64))
            }
            Some(_) => {
                g.go_at = None;
                false
            }
        })
    }

    /// PROTOCOL 8 (see the module's note): the syncs that came back, and each
    /// id's next where one is due — for work as its work runs low (`sync_low_ms`
    /// left) or, holding nothing, when the signal shows some, never before its
    /// `next_ms`; to hand in results that have waited `flush_ms`; to renew rows
    /// whose lease nears its end; to give back what it no longer computes; and
    /// every `report_ms` to say it is on.
    fn syncs(&mut self) {
        // Account binding runs independently of the signal and never blocks
        // dispatch/renewals for every other group while HTTP is outstanding.
        if !self.offline && !self.quit() {for gi in 0..self.groups.len().min(self.open) {self.claim(gi);}}
        for gi in 0..self.groups.len() {
            if self.groups[gi].syncing.as_ref().is_some_and(|s| s.handle.is_finished()) {
                let s = self.groups[gi].syncing.take().expect("checked above");
                let out = s.handle.join().unwrap_or_else(|_| Out::Failed("panicked".into()));
                self.take_sync(gi, s.sent, out);
                if self.proto != 8 {
                    return;
                }
            }
        }
        if self.offline || self.refused.is_some() || self.stale {
            return;
        }
        let held_back = self.quit() || self.held_why().is_some() || self.banned_until.is_some();
        if !held_back {
            self.read_signal();
        }
        let now = Instant::now();
        for gi in 0..self.groups.len() {
            if self.groups[gi].syncing.is_some() || now < self.groups[gi].retry_at {
                continue;
            }
            // A RIVEN GAIN IS NO ROW A SYNC CAN NAME, so a sync would give it
            // back: its id syncs again once its answer is in.
            if self.tasks.iter().any(|t| t.group == gi && t.riven()) || self.sending.iter().any(|s| s.group == gi) {
                continue;
            }
            let held: usize = self.held_rows(gi).iter().map(|(_, r)| r.len()).sum();
            let packs = self.packs.iter().filter(|p| p.group == gi).count() as u64;
            let idle = packs == 0 && held == 0;
            // AN OLDER ENGINE'S RESULTS go in first, under that engine, from an
            // id holding nothing (such a sync names no row): after the engine
            // changed, or from a run that ended before handing them in.
            if let Some(e) = self.groups[gi].journal.other_engine(&self.rel.engine) {
                if idle {
                    self.start_sync(gi, Some(e), 0, false);
                }
                continue;
            }
            let open = gi < self.open && !held_back && self.switch_to.is_none() && !self.memory_guard.draining() && self.settings_restart.is_none();
            let go = match (idle && open && now >= self.groups[gi].not_before).then(|| self.idle_go(gi, now)) {
                Some(None) => false,
                // Work waits for the signal/jitter; due result flushes and
                // renewals must remain independent of signal availability.
                Some(Some(false)) if self.groups[gi].go_at.is_some() => false,
                Some(Some(go)) => go,
                None => false,
            };
            let g = &self.groups[gi];
            let left = held as f64 * if g.pace_ms > 0.0 { g.pace_ms } else { 60_000.0 };
            let work = open && now >= g.not_before && packs < self.dials.packs_held && if idle { go } else { left < self.dials.sync_low_ms as f64 };
            // WITH NO PACE YET the server hands `pack_first` rows whatever is
            // asked, and says to come back once what was asked is nearly done:
            // asked for ten minutes, that is eight minutes with three rows.
            // So it is asked for about what it gives — back in a minute, pace known.
            let want = if g.pace_ms > 0.0 {
                (self.dials.pack8_ms as f64 - left).max(0.0) as u64
            } else {
                (self.dials.sync_low_ms + self.dials.sync_floor_ms).min(self.dials.pack8_ms)
            };
            let flush = g.journal.oldest(&self.rel.engine).is_some_and(|at| sys::now_ms().saturating_sub(at) >= self.dials.flush_ms)
                || (g.last_sync.is_none() && !g.journal.results.is_empty());
            let renew = held > 0 && self.packs.iter().any(|p| p.group == gi && now + sync::RENEW_BEFORE >= p.until);
            let report = idle
                && open
                && self.signal.is_some()
                && self.groups[gi].go_at.is_none()
                && self.dials.report_ms > 0
                && g.last_sync.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(self.dials.report_ms));
            if work {
                self.start_sync(gi, None, want, false);
            } else if flush || renew || report || g.give_back {
                self.start_sync(gi, None, 0, false);
            }
        }
    }

    /// ONE SYNC of id `gi`, on its own thread. `engine`: another engine whose
    /// results it hands in (such a sync names no row); `want`: the work asked
    /// for, 0 to renew and hand in only; `last`: the way out, sent again for up
    /// to three quarters of a minute when it gets no reply.
    fn start_sync(&mut self, gi: usize, engine: Option<String>, want: u64, last: bool) {
        if !last && !self.claim(gi){return;}
        let engine = engine.unwrap_or_else(|| self.rel.engine.clone());
        let current = engine == self.rel.engine;
        let held = if current { self.held_rows(gi) } else { Vec::new() };
        let lanes = self.id_lanes();
        let consent = json!({ "v": self.cfg.consent_v, "at": self.cfg.consent_at });
        let (since, lease_max) = (sys::local_midnight_utc_hour(), self.dials.sync_lease_ms);
        let g = &mut self.groups[gi];
        let old = g.journal.prune(sys::now_ms());
        if old > 0 {
            g.journal_dirty = true;
            self.ui.glog(gi, format!("{C_GO}[作废]{C_END} {old} 行结果存了超过一天仍没交上，已放弃"));
        }
        let refused: Vec<Value> = if current { g.refused_rows.iter().take(sync::REFUSED_MAX).cloned().collect() } else { Vec::new() };
        let pace = (g.pace_ms > 0.0).then(|| (g.pace_ms.round() as u64).clamp(100, lease_max));
        let (body, results) = {
            let send = g.journal.to_send(&engine);
            let body = sync::body(&g.verifier, &engine, consent, lanes, pace, want, &held, &send, &refused, g.journal.ask_states(), &since);
            (body, send.iter().map(|k| k.question.clone()).collect::<Vec<_>>())
        };
        self.trace.event(json!({"event":"sync_start","group":gi+1,"want_ms":want,"pace_ms":pace,"held":held.iter().map(|(_,r)|r.len()).sum::<usize>(),"results":results.len(),"relayed":self.net.relayed()}));
        let sent = Sent {
            at: Instant::now(),
            engine,
            want,
            results,
            refused: refused.iter().filter_map(|r| r["question"].as_str().map(String::from)).collect(),
            leases: held.into_iter().map(|(lease, _)| lease).collect(),
        };
        g.go_at = None;
        g.give_back = false;
        g.fallback_at = Instant::now() + sync::jittered(sync::FALLBACK_MS);
        let net = self.net.clone();
        let handle = std::thread::spawn(move || {
            let mut out = sync::send(&net, &body);
            for wait in SEND_AGAIN.iter().filter(|_| last) {
                if !matches!(out, Out::Failed(_)) {
                    break;
                }
                std::thread::sleep(*wait);
                out = sync::send(&net, &body);
            }
            out
        });
        g.syncing = Some(Syncing { handle, sent });
    }

    /// A SYNC'S REPLY (docs/CLIENTS.md, the reply's table): results taken
    /// leave the journal for the pending list and those dropped go; rows no
    /// longer the id's are passed by or stopped; each pack it named runs on; a
    /// new pack waits for lanes; `next_ms` is when the id may next be handed work.
    fn take_sync(&mut self, gi: usize, s: Sent, out: Out) {
        let now = Instant::now();
        let r = match out {
            Out::Reply(r) => r,
            Out::Refused(msg) => {
                if self.refused.is_none() {
                    self.ui.log(format!("{C_BAD}[服务器]{C_END} {msg}"));
                }
                self.refused = Some(msg);
                return;
            }
            Out::Failed(why) => {
                self.trace.event(json!({"event":"sync_failed","group":gi+1,"elapsed_ms":s.at.elapsed().as_millis()}));
                let g = &mut self.groups[gi];
                g.failures += 1;
                g.retry_at = now + sync::backoff(g.failures);
                // A body the server cannot read is this client's to fix: said, not only tried again.
                if why.starts_with("HTTP 400") || g.failures == 3 {
                    self.ui.glog(gi, format!("{C_BAD}[同步]{C_END} 同步失败（{why}），稍后重试；算完的结果存在本地，不会丢"));
                }
                self.ui.status(|st| st.state = "连不上服务器，稍后重试".into());
                return;
            }
        };
        let incoming=r["pack"]["tasks"].as_array().map_or(0,Vec::len);
        self.trace.event(json!({"event":"sync_reply","group":gi+1,"elapsed_ms":s.at.elapsed().as_millis(),"want_ms":s.want,"rows":incoming,"next_ms":r["next_ms"],"dials":r["dials"],"early":r["early"],"busy":r["busy"],"paused":r["paused"],"stale":r["stale"],"riven":r["work"]["kind"]=="riven_gain"}));
        if r["fallback"] == json!(7) {
            return self.fall_back("服务器暂时关闭了协议 8");
        }
        if r["dials"].is_object() {
            self.dials.take(&r["dials"]);
        }
        self.note_protocol(&r);
        let lease_ms = self.dials.sync_lease_ms;
        let g = &mut self.groups[gi];
        g.failures = 0;
        g.last_sync = Some(now);
        g.wait_reason=if r["early"]==true{"服务器领取间隔"}else if r["busy"]==true||r["paused"]==true{"服务器暂缓派发"}else if incoming==0 && r["work"]["kind"]!="riven_gain" && s.want>0{"服务器未分配"}else{"领取间隔"}.into();
        if let Some(ms) = r["next_ms"].as_u64() {
            g.not_before = now + Duration::from_millis(ms);
        }
        if r["points"].is_object() {
            g.points = Some((now, r["points"].clone()));
        }
        g.refused_rows.retain(|x| !s.refused.iter().any(|q| x["question"].as_str() == Some(q.as_str())));
        let (rows, work, dropped) = g.journal.settle(&s.results, &r, sys::now_ms());
        let confirmed = g.journal.states(&r);
        g.journal_dirty = true;
        g.done += rows;
        g.work += work;
        self.board_confirmed += confirmed;
        if rows > 0 {
            self.ui.glog(gi, format!("{C_OK}[提交]{C_END} {rows} 行答案 · {:.2} 分", points(work)));
            self.ui.status(|st| {
                st.session_tasks += rows;
                st.session_work += work;
            });
        }
        if dropped > 0 {
            self.ui.glog(gi, format!("{C_GO}[作废]{C_END} {dropped} 行结果没被收下（那几行已转给别的电脑，或这个引擎已不再接受）"));
        }
        // RENEWED: each pack it named runs on from when the sync went.
        for p in self.packs.iter_mut().filter(|p| p.group == gi && s.leases.contains(&p.lease)) {
            p.until = s.at + Duration::from_millis(lease_ms);
        }
        // NO LONGER THIS ID'S: a row not started is passed by, one running stopped.
        let lost = sync::lost(&r);
        if !lost.is_empty() {
            let gone = |w: &Value| w["question"].as_str().is_some_and(|q| lost.iter().any(|l| l == q));
            for p in self.packs.iter_mut().filter(|p| p.group == gi) {
                let before = p.rows.len();
                p.rows.retain(|w| !gone(w));
                p.total -= before - p.rows.len();
            }
            let stop: Vec<u64> = self.tasks.iter().filter(|t| t.group == gi && lost.contains(&t.question)).map(|t| t.id).collect();
            for id in stop {
                self.drop_task(id, "：这一行已由别的电脑接手，跳过", false);
            }
            let pids: Vec<u64> = self.packs.iter().filter(|p| p.group == gi).map(|p| p.id).collect();
            for pid in pids {
                self.maybe_end_pack(pid);
            }
        }
        if r["stale"] == json!(true) {
            if s.engine == self.rel.engine {
                self.stale = true;
            } else {
                // An older engine's results the server no longer takes.
                let g = &mut self.groups[gi];
                let before = g.journal.results.len();
                g.journal.results.retain(|k| k.engine != s.engine);
                g.journal_dirty = true;
                self.ui.glog(gi, format!("{C_GO}[作废]{C_END} 引擎 {} 已不再被接受，它算的 {} 行结果交不上了", s.engine, before - g.journal.results.len()));
            }
            return;
        }
        if r["banned"] == json!(true) {
            return self.cool_down(&r);
        }
        if r["busy"] == json!(true) && self.busy_said.is_none_or(|t| t.elapsed() > Duration::from_secs(600)) {
            self.busy_said = Some(Instant::now());
            self.ui.log(format!("{C_GO}[服务器]{C_END} 服务器繁忙，暂不派新活；算完的结果先存在本地"));
        }
        // AN OLDER ENGINE STILL ACCEPTED: the site serves `engine` now.
        if let Some(e) = r["engine"].as_str().filter(|e| *e != self.rel.engine) {
            self.served_moved(e.to_string(), r["accepted_until"].as_str().map(String::from));
        }
        let pack = &r["pack"];
        if let (Some(rows), Some(lease)) = (pack["tasks"].as_array().filter(|t| !t.is_empty()), pack["lease"].as_str()) {
            self.groups[gi].last_received=Some(now);self.groups[gi].nothing_at=None;
            if self.quit() || self.held_why().is_some() || s.engine != self.rel.engine {
                // Not to be computed: the next sync names none of it, so it goes back.
                self.groups[gi].give_back = true;
                return;
            }
            let lease_ms = pack["lease_ms"].as_u64().unwrap_or(self.dials.pack_lease_ms);
            let verifier = self.groups[gi].verifier.clone();
            self.next_id += 1;
            self.ui.glog(gi, format!("{C_INFO}[领取]{C_END} 一包 {} 行榜单，租约 {:.0} 分钟", rows.len(), lease_ms as f64 / 60_000.0));
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
                sent_at: now,
                until: s.at + Duration::from_millis(lease_ms),
            });
            return;
        }
        let w = r["work"].clone();
        if w["kind"] == json!("riven_gain") {
            let verifier = self.groups[gi].verifier.clone();
            if self.quit() || self.held_why().is_some() {
                Self::release_lease(&self.net, &verifier, w["lease"].as_str().unwrap_or(""), w["code"].as_str());
            } else {
                self.start(gi, verifier, w, None);
            }
            return;
        }
        if s.want > 0 && r["early"] != json!(true) {
            self.groups[gi].nothing_at = Some(now);
        }
    }

    /// PROTOCOL 7 FOR THE REST OF THE RUN (`fallback: 7`, or a server that no
    /// longer takes 8): each id's kept results go in as protocol 7 answers, by
    /// lease and twenty at a time (`/api/board/answers` takes `pack_max` and
    /// 32 KB at most), and the rows held go on under protocol 7's renewals.
    fn fall_back(&mut self, why: &str) {
        if self.proto == 7 {
            return;
        }
        self.proto = 7;
        self.ui.log(format!("{C_GO}[协议]{C_END} {why}，本次运行改用协议 7"));
        let now = Instant::now();
        for gi in 0..self.groups.len() {
            let g = &mut self.groups[gi];
            // A sync on its way is not waited for: what it took is sent again below, and taken once.
            g.syncing = None;
            let mut by: BTreeMap<(String, String), Vec<Kept>> = BTreeMap::new();
            for k in g.journal.results.drain(..) {
                let lease = k.result["lease"].as_str().unwrap_or("").to_string();
                by.entry((k.engine.clone(), lease)).or_default().push(k);
            }
            g.journal_dirty = true;
            let verifier = g.verifier.clone();
            for ((engine, lease), ks) in by {
                for part in ks.chunks(20) {
                    let (rows, work) = (part.len() as u64, part.iter().map(|k| k.work).sum::<u64>());
                    let answers: Vec<Value> = part
                        .iter()
                        .map(|k| {
                            let mut a = k.result.clone();
                            if let Some(o) = a.as_object_mut() {
                                o.remove("lease");
                            }
                            a
                        })
                        .collect();
                    let body = json!({ "lease": lease, "verifier": verifier, "engine": engine, "done": false, "answers": answers });
                    let net = self.net.clone();
                    let handle = std::thread::spawn(move || send_answers(&net, &body));
                    let log_ok = format!("{C_OK}[提交]{C_END} {rows} 行答案 · {:.2} 分", points(work));
                    self.sending.push(Sending { group: gi, handle, log_ok, work, rows });
                }
            }
        }
        for t in &mut self.tasks {
            if t.pack.is_some() {
                t.row_until = now;
                t.renew_retry = now;
            }
        }
        for p in &mut self.packs {
            p.sent_at = now;
        }
    }

    /// THE WAY OUT, protocol 8 (docs/CLIENTS.md, "every way out gives back what
    /// is held"): each id's sync on its way is heard; then each id that synced
    /// or keeps results syncs once more, naming nothing — every lease goes
    /// back — with what it keeps, sent again for up to three quarters of a
    /// minute when the site does not answer. What is still not taken stays on
    /// disk for the next run.
    fn last_syncs(&mut self) {
        for gi in 0..self.groups.len() {
            if let Some(s) = self.groups[gi].syncing.take() {
                let out = s.handle.join().unwrap_or_else(|_| Out::Failed("panicked".into()));
                self.take_sync(gi, s.sent, out);
                if self.proto != 8 {
                    return;
                }
            }
        }
        let going: Vec<usize> = (0..self.groups.len())
            .filter(|&gi| {
                let g = &self.groups[gi];
                g.last_sync.is_some() || g.failures > 0 || g.give_back || !g.journal.to_send(&self.rel.engine).is_empty()
            })
            .collect();
        for &gi in &going {
            self.start_sync(gi, None, 0, true);
        }
        for gi in going {
            if let Some(s) = self.groups[gi].syncing.take() {
                let out = s.handle.join().unwrap_or_else(|_| Out::Failed("panicked".into()));
                self.take_sync(gi, s.sent, out);
            }
        }
    }

    // ---- the lanes --------------------------------------------------------------

    /// Fill every free lane: the oldest task with a job for it, honouring
    /// leaders; lanes still free start a pack's next row.
    fn dispatch(&mut self) {
        // Keep ready CPU work moving before GPU polling/preparation/cleanup.
        self.fill_lanes(false);
        self.dispatch_gpu();
        for _ in 0..16 {
            if self.fill_lanes(false) == 0 || !self.start_row() {
                break;
            }
        }
        // Local work first, then steal across nodes/cache domains when they
        // would otherwise idle. A single task can still use the full budget.
        if crate::placement::active() { self.fill_lanes(true); }
    }

    fn release_gpu_rows(&mut self) {
        for t in &mut self.tasks {if let Runner::Board(row)=&mut t.runner {row.gpu_pending=false;}}
    }
    fn stop_gpu(&mut self) {
        self.gpu=None;self.gpu_state.clear();self.release_gpu_rows();
        self.gpu_restart=None;self.gpu_restarts=0;
        self.gpu_recovery=crate::gpu::Recovery::default();
        let label=crate::work::backend_label(self.pool.code());self.ui.status(|s|s.backend=label.into());
        self.ui.status(|s|s.gpu=crate::gpu::View{enabled:self.cfg.gpu_enabled,state:"已暂停".into(),..Default::default()});
    }
    /// One bounded batch owns its rows until it returns. Failed, cancelled or
    /// expired rows never bypass the ordinary Fleet ownership and journal path.
    fn dispatch_gpu(&mut self) {
        let state=format!("{}:{}",self.cfg.gpu_enabled,self.rel.engine);
        if state!=self.gpu_state {
            self.stop_gpu();self.gpu_state=state;
            self.ui.status(|s|s.gpu=crate::gpu::View{enabled:self.cfg.gpu_enabled,state:if self.cfg.gpu_enabled{"无匹配引擎".into()}else{"未启用".into()},..Default::default()});
            if self.cfg.gpu_enabled {
                let dir=crate::gpu::directory(&self.paths.engines,&self.rel.engine);
                if dir.join("engine.gpu.json").is_file() {
                    match crate::gpu::Manager::new(&dir,&self.rel.engine,self.cfg.compute_memory_limit_mb) {
                        Ok(g)=>{self.gpu=Some(g);self.ui.log(format!("{C_INFO}[GPU]{C_END} 正在加载并校验 {}",dir.file_name().unwrap_or_default().to_string_lossy()));}
                        Err(e)=>{self.ui.status(|s|s.gpu.state="启动失败，回退 CPU".into());self.ui.log(format!("{C_GO}[GPU]{C_END} 启动失败，继续 CPU：{e:#}"));}
                    }
                } else {self.ui.log(format!("{C_GO}[GPU]{C_END} {} 没有匹配的 GPU 包，继续 CPU",self.rel.engine));}
            }
        }
        if self.gpu_restart.is_some_and(|at|Instant::now()>=at) && self.gpu.as_ref().is_none_or(|g|g.cleanup_finished()) {
            self.gpu_restart=None;self.gpu_restarts=self.gpu_restarts.saturating_add(1);
            let dir=crate::gpu::directory(&self.paths.engines,&self.rel.engine);
            if let Some(old)=self.gpu.take(){
                match old.restart(&dir,&self.rel.engine){
                    Ok(g)=>{self.gpu=Some(g);self.ui.log(format!("{C_INFO}[GPU]{C_END} 重建计算进程，第 {} 次恢复；故障批次继续由 CPU 完成",self.gpu_restarts));}
                    Err(e)=>{self.gpu=Some(old);let delay=self.gpu_recovery.failure(false);self.gpu_restart=Some(Instant::now()+delay);self.trace.event(json!({"event":"gpu_restart_failed","error":format!("{e:#}"),"retry_secs":delay.as_secs()}));self.ui.log(format!("{C_GO}[GPU]{C_END} 恢复失败，{} 秒后再试：{e:#}",delay.as_secs()));}
                }
            }
        }
        if let Some(event)=self.gpu.as_mut().and_then(|g|g.poll()) {
            match event {
                crate::gpu::Event::Progress=>{},
                crate::gpu::Event::Ready(name)=>{
                    self.trace.event(json!({"event":"gpu_ready","engine":self.rel.engine,"restarts":self.gpu_restarts}));
                    self.ui.log(format!("{C_OK}[GPU]{C_END} {name} 就绪；自动探测榜单场景并按收益分流"));
                    let label=crate::work::backend_label(self.pool.code());self.ui.status(|s|s.backend=format!("{label} + NVIDIA GPU 预览"));
                }
                crate::gpu::Event::Failed(e)=>{
                    let retry=self.gpu.as_ref().is_some_and(|g|g.retryable);
                    let probe_timeout=self.gpu.as_ref().is_some_and(|g|g.probe_timeout);
                    let allocation=self.gpu.as_ref().is_some_and(|g|g.allocation_failure);
                    let delay=if retry {self.gpu_recovery.failure(probe_timeout).as_secs()}else{0};
                    if retry {self.gpu_restart=Some(Instant::now()+Duration::from_secs(delay));}
                    self.trace.event(json!({"event":"gpu_failure","engine":self.rel.engine,"error":e,"retryable":retry,"probe_timeout":probe_timeout,"allocation_failure":allocation,"batch_limit":self.gpu.as_ref().map(|g|g.batch_limit()),"retry_secs":delay,"restarts":self.gpu_restarts}));
                    self.release_gpu_rows();self.ui.log(if probe_timeout {format!("{C_INFO}[GPU探测]{C_END} 场景超时暂缓，任务回到 CPU；{delay} 秒后重建 GPU，其他已学会场景优先")}else{format!("{C_GO}[GPU]{C_END} 任务回到 CPU：{e}{}",if retry{format!("；{delay} 秒后恢复 GPU")}else{"；本次停用 GPU".into()})});
                    if allocation {self.ui.log(format!("{C_INFO}[GPU]{C_END} 驱动分配失败；后续批量上限 {} 局，保留已学会场景，CPU 继续计算",self.gpu.as_ref().map_or(0,|g|g.batch_limit())));}
                    let label=crate::work::backend_label(self.pool.code());self.ui.status(|s|s.backend=label.into());
                }
                crate::gpu::Event::Finished(rows)=>{
                    let fallback=rows.iter().filter(|r|r.fallback.is_some()).count();
                    if rows.len()>fallback {self.gpu_recovery.success(Instant::now());}
                    self.trace.event(json!({"event":"gpu_batch_finished","rows":rows.len(),"fallback":fallback,"private_kib":self.gpu.as_ref().map(|g|g.budget())}));
                    if fallback>0 {
                        let pages=rows.iter().filter(|r|r.fallback==Some(120)).count();
                        let output=rows.iter().filter(|r|r.fallback==Some(2)).count();
                        if pages>0 {self.ui.log(format!("{C_GO}[GPU]{C_END} {pages} 行单局私有页不足（120），交由 CPU；后续提高单局配额并缩批，达到上限则暂缓"));}
                        if output>0 {self.ui.log(format!("{C_GO}[GPU]{C_END} {output} 行输出/地址范围超限（2），交由 CPU；与整机空闲内存无关"));}
                    }
                    for row in rows {
                        if let Some(probe)=&row.probe {
                            let (allowed,deferred)=self.gpu.as_ref().map_or((0,0),|g|g.learning_summary());
                            self.trace.event(json!({"event":"gpu_probe","samples":probe.samples,"gpu_ms":probe.gpu_ms,"cpu_ms":probe.cpu_ms,"max_pages":probe.max_pages,"allowed_contexts":allowed,"deferred_contexts":deferred}));
                            self.ui.log(format!("{C_INFO}[GPU探测]{C_END} {} 局校验一致，GPU {:.2}s；已学习 {allowed} 类可用 / {deferred} 类暂缓",probe.samples,probe.gpu_ms/1000.0));
                            continue;
                        }
                        let Some(ti)=self.tasks.iter().position(|t|t.id==row.id) else {continue};
                        if !matches!(&self.tasks[ti].runner,Runner::Board(b) if b.gpu_pending){continue;}
                        if row.fallback.is_some(){if let Runner::Board(b)=&mut self.tasks[ti].runner {b.gpu_pending=false;}continue;}
                        let t=self.tasks.remove(ti);
                        let s=RowScore{score:row.score["score"].as_f64().unwrap_or(0.0),metric:row.score["metric"].as_str().unwrap_or("").into(),
                            work:row.score["work"].as_u64().unwrap_or(0),compute_ms:row.compute_ms,counts:row.score["counts"].clone()};
                        self.ui.glog(t.group,format!("{C_OK}[GPU]{C_END} 完成 {}",t.label));self.answer(t,Finished::Board(s));
                    }
                }
            }
        }
        if let Some(g)=&self.gpu {
            let mut view=g.view();let idle=g.available();
            if let Some(at)=self.gpu_restart{view.state=if !g.cleanup_finished(){"回收 GPU 资源，CPU 继续".into()}else{format!("恢复等待 {}s",at.saturating_duration_since(Instant::now()).as_secs()+1)};}
            self.ui.status(|s|{if idle && s.gpu.state.starts_with("待机："){view.state=s.gpu.state.clone();}s.gpu=view;});
        }
        if self.gpu_restart.is_some(){return;}
        if self.lanes()==0 || !self.gpu.as_ref().is_some_and(|g|g.available()) {return;}
        // Physical availability limits additional batch storage. The configured
        // CPU+GPU Job ceiling separately limits total process commit.
        let memory=sys::memory().map(|(used,total,_)| {
            let available=total.saturating_sub(used);
            (self.replay_memory_available.map_or(available,|cap|available.min(cap)),total)
        });
        // Inspect only a bounded window of already leased work, leaving CPU
        // lanes free to take the rest. Never ask for artificial/duplicate rows.
        for _ in 0..crate::gpu::MAX_ROWS {
            if self.tasks.len()>=crate::gpu::MAX_ROWS+self.lanes()*2 || !self.start_row(){break;}
        }
        let manager=self.gpu.as_ref().expect("available");
        let cpu_bytes=sys::process_memory().unwrap_or(0);
        let system_limit=|budget| memory.map_or(crate::gpu::MAX_RUNS,|(available,total)|crate::gpu::system_run_limit(available,total,budget));
        let run_limit=|budget| manager.max_runs_for(budget)
            .min(crate::gpu::memory_run_limit(self.cfg.compute_memory_limit_mb,cpu_bytes,budget))
            .min(system_limit(budget));
        let blocked=|budget| {
            if system_limit(budget)==0 {
                memory.map_or("待机：系统可用内存不足".into(),|(available,total)|format!("待机：可用 {:.1}G，预留 {:.1}G",available as f64/(1u64<<30) as f64,crate::gpu::system_memory_reserve(total) as f64/(1u64<<30) as f64))
            } else if manager.max_runs_for(budget)==0 {"待机：显存余量不足".into()}
            else {"待机：合计内存预算不足".into()}
        };
        if run_limit(manager.budget())==0 {self.ui.status(|s|s.gpu.state=blocked(manager.budget()));return;}
        let candidates:Vec<_>=self.tasks.iter().filter_map(|t|match &t.runner {Runner::Board(b)=>b.gpu_input(t.id),_=>None}).collect();
        let mut rows=manager.select(&candidates,&run_limit);
        let probing=rows.first().is_some_and(|r|r.probe);
        // Keep enough work on the CPU to overlap the GPU batch. The last small
        // tail is faster on the native lanes than waiting for a GPU batch.
        let held=self.tasks.len()+self.packs.iter().map(|p|p.rows.len()).sum::<usize>();
        if !probing {rows.truncate(held.saturating_sub((self.lanes()*2).clamp(4,32)));}
        let budget=rows.iter().map(|r|r.private_kib).max().unwrap_or(192);
        let allowed=run_limit(budget);
        if !probing && allowed==0 {self.ui.status(|s|s.gpu.state=blocked(budget));return;}
        if !probing {let mut total=0;rows.retain(|r|{total+=r.request["runs"].as_u64().unwrap_or(0) as usize;total<=allowed});}
        let required=if probing{1}else{4000.min(allowed) as u64};
        let selected=rows.iter().map(|r|r.request["runs"].as_u64().unwrap_or(0)).sum::<u64>();
        if rows.is_empty() || selected<required {self.ui.status(|s|s.gpu.state=if held==0{"待机：等待新任务".into()}else if held<=32{"待机：CPU收尾".into()}else if selected==0{"待机：暂缓场景或探测间隔".into()}else{format!("待机：合适任务 {selected}/{required}次")});return;}
        match self.gpu.as_mut().expect("available").submit(&rows) {
            Ok(())=>{
                self.trace.event(json!({"event":"gpu_dispatch","probe":probing,"rows":rows.len(),"runs":selected,"private_kib":budget,"allowed_runs":allowed,"compute_limit_mib":self.cfg.compute_memory_limit_mb,"system_available_bytes":memory.map(|m|m.0),"system_reserve_bytes":memory.map(|m|crate::gpu::system_memory_reserve(m.1))}));
                if !probing {for t in &mut self.tasks {if rows.iter().any(|r|r.id==t.id) {if let Runner::Board(b)=&mut t.runner {b.gpu_pending=true;t.last_ran=Instant::now();}}}}
                if probing {self.ui.log(format!("{C_INFO}[GPU]{C_END} 探测 {} / {}，CPU 同时正常计算",rows[0].request["weapon"].as_str().unwrap_or("?"),rows[0].ruler));}
                else{self.ui.log(format!("{C_INFO}[GPU]{C_END} 已分配 {} 条独立榜单任务",rows.len()));}
            }
            Err(e)=>{if let Some(g)=&mut self.gpu{g.send_failed(&e);}let delay=self.gpu_recovery.failure(false);self.gpu_restart=Some(Instant::now()+delay);self.release_gpu_rows();self.trace.event(json!({"event":"gpu_send_failed","error":format!("{e:#}"),"retry_secs":delay.as_secs()}));self.ui.status(|s|s.gpu.state="发送失败，待恢复".into());self.ui.log(format!("{C_GO}[GPU]{C_END} 发送失败，任务回 CPU，{} 秒后恢复：{e:#}",delay.as_secs()));}
        }
    }

    /// …one pass; the lanes it left free that a board row could use.
    fn fill_lanes(&mut self, remote: bool) -> usize {
        let lanes = self.lanes();
        self.pool.ensure(lanes);
        crate::affinity::process_default(lanes);
        let backend = self.backend();
        // Leaders: a riven gain without one takes the lowest free lane in
        // budget, and its backend — once the riven canary has its verdict.
        let riven_backend = self.riven_backend();
        for i in 0..self.tasks.len() {
            let taken: Vec<usize> = self.tasks.iter().filter_map(|t| match &t.runner { Runner::Riven(r) => r.leader, _ => None }).collect();
            if let (Runner::Riven(r), Some(b)) = (&mut self.tasks[i].runner, riven_backend) {
                if r.leader.is_none() {
                    r.leader = (0..lanes).find(|l| !taken.contains(l));
                    r.backend = b;
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
                    Runner::Board(b) if owner.is_none() => b.job_on(tag, lanes, lane, remote).map(|c| (backend, c)),
                    Runner::Riven(r) if (owner.is_none() || mine)
                        && (remote || r.leader.is_none_or(|l| crate::placement::same_domain(l,lane))) => r.job(lane, tag).map(|c| (r.backend, c)),
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
        let t = &mut self.tasks[ti];
        t.last_ran = Instant::now();
        let outcome = match &mut t.runner {
            Runner::Board(b) => b.done(d).map(|s| s.map(Finished::Board)),
            Runner::Riven(r) => r.done(d, lanes).map(|s| s.map(Finished::Riven)),
        };
        match outcome {
            Ok(None) => {}
            Ok(Some(f)) => {
                let t = self.tasks.remove(ti);
                self.tag_task.retain(|_, v| *v != id);
                self.let_go(&t);
                self.answer(t, f);
            }
            Err(e) => {
                let (label, gi) = (self.tasks[ti].label.clone(), self.tasks[ti].group);
                self.ui.glog(gi, format!("{C_BAD}[失败]{C_END} {label}：{e:#}"));
                self.drop_task(id, "，任务已交还", true);
            }
        }
    }

    /// SHORT OF MEMORY — the system's free physical memory under a tenth of it,
    /// 1 GiB at least — the cap on groups comes down by one, every
    /// `CONTROL_EVERY` while that lasts: what a group holds costs memory, a
    /// riven gain's search some hundreds of MB. With twice that free for
    /// `REOPEN_AFTER`, the cap goes.
    fn watch_memory(&mut self) {
        let Some((used, total, _)) = sys::memory() else { return };
        let (free, low) = (total.saturating_sub(used), (total / MEM_LOW_SHARE).max(MEM_LOW_MIN));
        let gb = free as f64 / (1u64 << 30) as f64;
        let now = Instant::now();
        if free < 2 * low {
            self.mem_tight_at = Some(now);
        }
        if free < low && self.open > 1 && self.mem_at.is_none_or(|t| now.duration_since(t) >= CONTROL_EVERY) {
            self.ui.log(format!("{C_GO}[内存]{C_END} 系统可用内存只剩 {gb:.1} GB，第 {} 路做完手头的活后不再接", self.open));
            self.open -= 1;
            self.mem_cap = Some(self.open);
            self.mem_at = Some(now);
            self.closed_at = Some(now);
        } else if self.mem_cap.is_some() && self.mem_tight_at.is_some_and(|t| now.duration_since(t) >= REOPEN_AFTER) {
            self.mem_cap = None;
            self.ui.log(format!("{C_INFO}[内存]{C_END} 系统可用内存回到 {gb:.1} GB，并发不再因内存受限"));
        }
    }

    /// A RIVEN GAIN ENDED, answered or not: its leader's thread is let go
    /// (engine.rs `Pool`), and its search with it — some hundreds of MB, which
    /// the thread kept until it next led a search (the wasm's instance too).
    fn let_go(&mut self, t: &Active) {
        if let Runner::Riven(Riven { leader: Some(l), .. }) = &t.runner {
            self.pool.retire(*l);
        }
    }

    /// A LANE GIVEN NOTHING FOR A MINUTE ends its thread (engine.rs `Pool`), as
    /// the page lets a kept worker go (08-checkpoint-api.js `fleetKeep`): the
    /// budget shrank, the client is paused, or there is little work. Not a
    /// riven gain's leader, whose search waits there for the rest of its round.
    fn rest_lanes(&mut self) {
        let leaders: Vec<usize> = self.tasks.iter().filter_map(|t| match &t.runner { Runner::Riven(r) => r.leader, _ => None }).collect();
        self.pool.retire_idle(LANE_IDLE, &|l| leaders.contains(&l));
    }

    /// The answer, sent on its own thread (printed instead, offline).
    fn answer(&mut self, t: Active, f: Finished) {
        let secs = t.began.elapsed().as_secs_f64();
        let lanes = t.lanes_used.len().max(1);
        if let (Some(pid), Finished::Board(s)) = (t.pack, &f) {
            return self.answer_row(&t, pid, s, lanes, secs);
        }
        if let Finished::Board(s) = &f {
            self.computed(s.work, matches!(&t.runner, Runner::Board(b) if b.gpu_pending));
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
                self.computed(work, false);
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

    /// WHAT WAS COMPUTED, as it is: the speed shown counts work done here, not
    /// the moment the server took it (a protocol 8 result waits for a sync).
    fn computed(&self, work: u64, gpu: bool) {
        let now = Instant::now();
        self.ui.status(|st| {
            st.record_computed(now, work, gpu);
        });
    }

    /// A PACK'S ROW, measured: protocol 8 keeps its result for the id's next
    /// sync; protocol 7 joins it to the pack's answers, sent every half minute
    /// and as the pack ends.
    fn answer_row(&mut self, t: &Active, pid: u64, s: &RowScore, lanes: usize, secs: f64) {
        let now = Instant::now();
        let g = &mut self.groups[t.group];
        let wall = secs * 1000.0;
        let ms = g.last_row.map_or(wall, |l| now.duration_since(l).as_secs_f64() * 1000.0).min(wall);
        g.pace_ms = if g.pace_ms > 0.0 { g.pace_ms * 0.7 + ms * 0.3 } else { ms };
        g.journal.pace_ms = g.pace_ms;
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
        self.computed(s.work, matches!(&t.runner, Runner::Board(b) if b.gpu_pending));
        let Some(p) = self.packs.iter_mut().find(|p| p.id == pid) else { return };
        p.running = p.running.saturating_sub(1);
        p.answered += 1;
        if self.proto == 8 {
            // KEPT BEFORE ANYTHING ELSE, on disk (`save_journals`): the id's next
            // sync hands it in, and a reply that never comes costs nothing.
            a["lease"] = json!(p.lease);
            let k = Kept { question: t.question.clone(), engine: self.rel.engine.clone(), at: sys::now_ms(), work: s.work, result: a };
            let g = &mut self.groups[t.group];
            g.journal.keep(k);
            g.journal_dirty = true;
            return self.maybe_end_pack(pid);
        }
        self.recent.push((t.verifier.clone(), false, json!({ "identity": t.w["identity"], "ruler": t.w["ruler"], "mode": t.w["mode"] })));
        if self.recent.len() > 100 {
            self.recent.remove(0);
        }
        let Some(p) = self.packs.iter_mut().find(|p| p.id == pid) else { return };
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
                    self.ui.status(|st| {
                        st.session_tasks += s.rows;
                        st.session_work += s.work;
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
            // Protocol 7's heartbeat, row by row; protocol 8's syncs renew every row an id holds.
            if t.pack.is_some() && self.proto == 7 && !self.offline && t.renewing.is_none() && now + RENEW_BEFORE >= t.row_until && now >= t.renew_retry {
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
        let gpu=self.gpu.as_ref().is_some_and(|g|g.active());
        self.ui.status(|s| s.backend = if gpu {format!("{label} + NVIDIA GPU 预览")}else{label.into()});
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
        self.watch_memory();
        if self.memory_guard.draining() || self.settings_restart.is_some() {
            return;
        }
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
        // THE CAP comes first, and at once: a key press, a smaller budget, memory.
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
        // WHAT IS HELD is weighed against what a pack is: protocol 7's a minute;
        // protocol 8's ten (`pack8_ms`), the next asked once two are left
        // (`sync_low_ms`) — so the board work held over every id runs from about
        // `low` to `full`, however many ids share the lanes.
        let (held_low, held_full, held_high) = if self.proto == 8 {
            let (pack, low) = (self.dials.pack8_ms as f64 / 1000.0, self.dials.sync_low_ms as f64 / 1000.0);
            (low, pack + low, 1.5 * (pack + low))
        } else {
            (HELD_LOW, HELD_FULL, HELD_HIGH)
        };
        let signal_work=self.signal.as_ref().is_some_and(|(at,s)|now.duration_since(*at)<Duration::from_millis(3*self.dials.poll_ms)
            && s.as_ref().is_some_and(|s|s.open>0 || s.urgent>0));
        let busy_groups = (0..self.open).all(|gi| {
            let g=&self.groups[gi];
            let held=self.tasks.iter().any(|t|t.group==gi)||self.packs.iter().any(|p|p.group==gi);
            capacity_limited(held,g.last_received.is_some(),now<g.not_before,signal_work,
                g.nothing_at.is_some_and(|at|now.duration_since(at)<NOTHING_RECENT),g.failures>0)
        });
        let told_nothing = (0..self.open).any(|g| self.groups[g].nothing_at.is_some_and(|t| now.duration_since(t) < NOTHING_RECENT));
        let reopen_ok = self.closed_at.is_none_or(|t| now.duration_since(t) >= REOPEN_AFTER);
        let close = |why: String| (self.open > 1).then_some(why);
        let closing = if told_nothing && util < OPEN_BELOW {
            close("服务器暂时没有更多任务可分".into())
        } else if h > held_high {
            close(format!("手上的榜单够全部核心算 {h:.0} 秒"))
        } else if util > FULL_ABOVE && h > held_full {
            close(format!("核心已满，手上还有 {h:.0} 秒的活"))
        } else {
            None
        };
        if let Some(why) = closing {
            self.open -= 1;
            self.closed_at = Some(now);
            self.ui.log(format!("{C_INFO}[调度]{C_END} {why}，第 {} 路做完手头的活后不再接（现 {} 路）", self.open + 1, self.open));
        } else if util < OPEN_BELOW && h < held_low && busy_groups && !told_nothing && reopen_ok && self.open < cap {
            self.open += 1;
            self.ensure_groups(self.open);
            let held = held.map_or(String::new(), |h| format!("，手上只剩 {h:.0} 秒的活"));
            self.ui.log(format!("{C_INFO}[调度]{C_END} 核心利用率 {:.0}%{held}，加开第 {} 路", util * 100.0, self.open));
        }
    }

    fn wait_text(&self,gi:usize,now:Instant)->String {
        let g=&self.groups[gi];
        let left=|at:Instant|at.saturating_duration_since(now).as_secs()+1;
        if let Some((at,_))=&g.claiming {return format!("绑定设备 {}s",at.elapsed().as_secs());}
        if let Some(s)=&g.syncing {return format!("{} {}s",if s.sent.want>0{"请求任务"}else{"同步结果"},s.sent.at.elapsed().as_secs());}
        if self.switch_to.is_some(){return "等待切换引擎".into();}
        if self.memory_guard.draining(){return "排空并回收内存".into();}
        if self.settings_restart.is_some(){return "排空并应用设置".into();}
        if self.proto==8 {
            if now<g.retry_at{return format!("网络重试 {}s",left(g.retry_at));}
            if now<g.not_before{return format!("{} {}s",g.wait_reason,left(g.not_before));}
            if let Some(at)=g.go_at.filter(|at|*at>now){return format!("领取错峰 {}s",left(at));}
            if self.signal_job.is_some() && self.signal.is_none(){return "读取任务信号".into();}
            if let Some((_,signal))=&self.signal {
                if signal.is_none() && now<g.fallback_at{return format!("信号不可用，直询 {}s",left(g.fallback_at));}
                if signal.as_ref().is_some_and(|s|s.open==0 && s.urgent==0){return "信号暂无可派任务".into();}
            }
        }
        "等待任务响应".into()
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
                } else if gi >= open {
                    "暂不接活".into()
                } else {
                    self.wait_text(gi,now)
                };
                GroupView { id: g.verifier.get(..8).unwrap_or(&g.verifier).to_string(), state, open: gi < open, tasks: g.done, work: g.work }
            })
            .collect();
        let state = if self.refused.is_some() {
            "服务器不派活（见日志）".to_string()
        } else if let Some(u) = &self.banned_until {
            format!("冷却中，至 {u}")
        } else if self.tasks.is_empty() {
            (0..self.open).min_by_key(|i|self.groups[*i].not_before).map_or_else(||"等待任务".into(),|i|self.wait_text(i,now))
        } else {
            "计算中".to_string()
        };
        let (kept_rows, kept_work) = self.kept();
        // BOARD RESULTS' STANDING, protocol 8: those taken and not settled yet
        // wait; protocol 7's is asked of `/api/board/mine` (`refresh_stats`).
        let board = (self.proto == 8).then(|| (self.board_confirmed, self.groups.iter().map(|g| g.journal.pending.len() as u64).sum::<u64>()));
        self.ui.status(|s| {
            s.state = state;
            s.kept_rows = kept_rows;
            s.kept_work = kept_work;
            if let Some(b) = board {
                s.board_states = b;
            }
            s.confirmed = s.board_states.0 + s.riven_states.0;
            s.waiting = s.board_states.1 + s.riven_states.1;
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
            s.compute_memory_limit_mb=self.cfg.compute_memory_limit_mb;
        });
    }

    /// WHAT THE LANES ARE GIVEN, every few seconds: the CPU time the whole
    /// process got against the lanes' working time. The process's, not each
    /// lane thread's own: a native riven gain's search scores on threads
    /// WFSim's optimizer starts, its lane waiting meanwhile, and counted by the
    /// lanes' threads alone those lanes read as getting next to nothing. Lanes
    /// that get far less than a core each for a minute are said so, once in ten
    /// minutes.
    fn lane_yield(&mut self) {
        let (at, cpu0, wall0) = self.yield_seen;
        if at.elapsed() < Duration::from_secs(5) {
            return;
        }
        let (cpu, (_, wall)) = (sys::process_cpu_us(), crate::affinity::lane_totals());
        let now = Instant::now();
        let span = now.duration_since(at).as_micros() as f64;
        self.yield_seen = (now, cpu, wall);
        let (dc, dw) = (cpu.saturating_sub(cpu0) as f64, wall.saturating_sub(wall0) as f64);
        let lanes = self.lanes();
        if dw < 1e6 {
            self.ui.status(|s| {
                s.lane_yield = None;
                s.effective_cores = None;
            });
            return;
        }
        // A job's wall time is counted when it ends, so a long riven step
        // landing in a window reads as more working time than the window held.
        let share = (dc / dw).min(1.0);
        let processors = crate::affinity::processors_used(lanes);
        // AS CORES: the process's CPU time over the window's.
        let cores = dc / span.max(1.0);
        self.ui.status(|s| {
            s.lane_yield = Some(share);
            s.effective_cores = Some(cores);
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
        let on = if processors == 0 { String::new() } else { format!("（分布在 {processors} 个逻辑处理器上）") };
        self.ui.log(format!("{C_GO}[CPU]{C_END} {lanes} 条计算线程每条只拿到 {:.0}% 的算力{on}{why}", share * 100.0));
    }

    fn measure(&mut self) {
        self.lane_yield();
        let lanes = if self.held_why().is_some() { 0 } else { self.lanes() };
        // macOS: the Mac stays awake while the lanes may compute (sys.rs).
        sys::keep_awake(lanes > 0);
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
            self.apply_settings();
            if self.maintain_memory() || self.settings_restart_ready() {
                self.shutdown();
                while !self.quit() && self.groups.iter().any(|g| g.journal_dirty) {
                    // Never replace the process while its only copy of an
                    // answer is in RAM. Retry persistence without taking work.
                    std::thread::sleep(Duration::from_secs(1));
                    self.save_journals();
                }
                if !self.quit() && self.ctl.paused.load(Ordering::Relaxed) {
                    continue;
                }
                self.recycle = !self.quit();
                return;
            }
            self.cool_down_over();
            if let Some(why) = self.held_why() {
                self.stop_gpu();
                if !self.tasks.is_empty() || !self.packs.is_empty() {
                    self.drop_all("，任务已交还");
                }
                // Held, the ids still give back what they held and hand in what they keep.
                if self.proto == 8 {
                    self.syncs();
                }
                self.housekeeping();
                self.save_journals();
                self.ui.status(|s| {
                    s.state = why.into();
                    s.tasks.clear();
                    s.lanes_now = 0;
                });
                self.measure();
                self.rest_lanes();
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
            if self.proto == 8 {
                self.syncs();
            } else {
                self.asks();
            }
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
                self.rest_lanes();
            }
            if self.journals_at.elapsed() >= Duration::from_secs(1) {
                self.save_journals();
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

    fn apply_settings(&mut self) {
        let desired=self.ctl.desired_settings();let old=crate::settings::Settings::from_config(&self.cfg);
        if desired!=old {
            if desired.memory_limit_mb!=old.memory_limit_mb {self.memory_guard=crate::maintenance::MemoryGuard::new(desired.memory_limit_mb);}
            if desired.priority!=old.priority {sys::set_priority(&desired.priority);}
            if desired.native_wait_hours!=old.native_wait_hours {self.engine_retry=Instant::now();self.release_at=Instant::now()-RELEASE_EVERY;}
            desired.write(&mut self.cfg);
            self.settings_dirty=true;self.settings_save_at=Instant::now()-Duration::from_secs(2);
        }
        if self.settings_dirty && self.settings_save_at.elapsed()>=Duration::from_secs(1) {
            self.settings_save_at=Instant::now();self.paths.merge_extras(&mut self.cfg);
            match self.paths.save(&self.cfg) {
                Ok(())=>{
                    self.settings_dirty=false;
                    let topology=desired.topology_changed(&self.settings_original);
                    self.settings_restart=if topology {Some(self.settings_restart.unwrap_or_else(Instant::now))}else{None};
                    let mut panel=self.ctl.settings.lock().unwrap();
                    panel.message=if topology {"已保存；正在排空任务，随后自动重启应用调度设置。".into()}else{"已保存到 simfleet.toml，设置已生效。".into()};
                    panel.topology=crate::placement::report(&self.cfg).unwrap_or_else(|e|e.to_string());
                }
                Err(e)=>{self.ctl.settings.lock().unwrap().message=format!("保存失败，1 秒后重试：{e}");}
            }
        }
    }
    fn settings_restart_ready(&self)->bool {
        !self.settings_dirty && !self.ctl.paused.load(Ordering::Relaxed) && self.settings_restart.is_some_and(|at|
            at.elapsed()>=Duration::from_secs(15*60) || (self.tasks.is_empty() && self.packs.is_empty() && self.sending.is_empty()
                && self.groups.iter().all(|g|g.asking.is_none()&&g.syncing.is_none())))
    }

    /// Stop asking first, finish what is held, then let the OS reclaim the
    /// native libraries' global allocations as well as their thread heaps.
    fn maintain_memory(&mut self) -> bool {
        // A manual pause must survive until the user resumes it. A new
        // process starts unpaused, so defer maintenance while P is active.
        if self.ctl.paused.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if self.memory_checked.elapsed() >= Duration::from_secs(5) {
            self.memory_checked = now;
            let bytes = sys::process_memory();
            if self.memory_guard.observe(bytes, now) {
                self.ui.log(format!(
                    "{C_GO}[内存]{C_END} 本进程 {:.0} MiB，超过回收线 {} MiB；停止领新活，做完手上的任务后自动换新进程（最多等待 15 分钟）",
                    bytes.unwrap_or(0) as f64 / 1048576.0, self.cfg.memory_limit_mb
                ));
            }
        }
        let empty = self.tasks.is_empty() && self.packs.is_empty() && self.sending.is_empty()
            && self.groups.iter().all(|g| g.asking.is_none() && g.syncing.is_none());
        self.memory_guard.ready(empty, now)
    }

    /// Compute saved tasks (`work` as `/api/board/work` handed them out), all
    /// at once, offline.
    pub fn replay(&mut self, tasks: &[Value], available_memory: Option<u64>) -> Result<()> {
        self.offline = true;
        self.replay_memory_available = available_memory;
        if self.cfg.gpu_enabled {
            self.dispatch_gpu();
            let until=Instant::now()+Duration::from_secs(185);
            while self.gpu.is_some() && !self.gpu.as_ref().is_some_and(|g|g.available()) && Instant::now()<until {
                self.dispatch_gpu();std::thread::sleep(Duration::from_millis(50));
                if self.gpu.as_ref().is_some_and(|g|g.disabled()){break;}
            }
        }
        let v = self.groups[0].verifier.clone();
        for w in tasks {
            if !self.start(0, v.clone(), w.clone(), None) {
                bail!("task did not start");
            }
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
        if rel.engine == self.rel.engine {
            // NOT THE ENGINE BUT THE PROTOCOL: protocol 7 while the server takes it.
            if self.proto == 8 && !rel.protocols.is_empty() && !rel.protocols.contains(&8) && rel.protocols.contains(&7) {
                self.stale=false;
                self.fall_back("服务器不再接受协议 8");
                return Ok(());
            }
            if rel.protocols.contains(&self.proto) {
                // The site's release.json is a cache behind the server's word.
                self.ui.log(format!("{C_GO}[引擎]{C_END} 服务器说本机引擎已过期，但 release.json 还没更新；1 分钟后再看"));
            } else {
                self.ui.status(|s| s.state = "服务器协议已更新，请更新 simfleet".into());
                self.ui.log(format!("{C_BAD}[过旧]{C_END} 服务器不再接受本版本的协议（{}），请更新 simfleet", self.proto));
            }
            std::thread::sleep(Duration::from_secs(60));
            return Ok(());
        }
        self.ui.log(format!("{C_INFO}[引擎]{C_END} 网站引擎已更新 {} → {}", self.rel.engine, rel.engine));
        let code = load_code(&self.net, &self.paths, &self.cfg, &self.ui, &rel)?;
        self.stale=false;
        self.take_engine(rel,code);
        Ok(())
    }

    /// The site names `engine`, not ours: make it ready on a thread of its
    /// own — the wasm, and a native build if one is published (simfleet's, or
    /// WFSim's own) — while the lanes go on with the old one.
    /// `until`: when the site stops taking ours ("" already; `None` not said).
    fn served_moved(&mut self, engine: String, until: Option<String>) {
        if self.memory_guard.draining() || self.settings_restart.is_some() {
            return;
        }
        if let Some(until)=until.as_deref() {self.accepted_until=Some(sys::parse_iso(until).unwrap_or(0));}
        let busy = self.engine_load.as_ref().is_some_and(|(e, _)| *e == engine) || self.switch_to.as_ref().is_some_and(|(r, _)| r.engine == engine);
        if busy || Instant::now() < self.engine_retry {
            return;
        }
        if self.waiting_said.as_deref()!=Some(&engine) {
            self.ui.log(format!("{C_INFO}[引擎]{C_END} 网站引擎已更新 {} → {engine}；后台检查原生构建与旧引擎接受期限", self.rel.engine));
            self.waiting_said=Some(engine.clone());
        }
        let (net, paths, cfg, ui) = (self.net.clone(), self.paths.clone(), self.cfg.clone(), self.ui.clone());
        let current=self.rel.clone();
        let old_optimized=self.pool.code().native.as_ref().is_some_and(|n|!n.official);
        let force=self.stale || self.accepted_until.is_some_and(|t|t<=sys::now_ms());
        let h = std::thread::spawn(move || {
            crate::engine_update::prepare(&net,&paths,&cfg,&ui,&current,old_optimized,force)
        });
        self.engine_load = Some((engine, h));
    }

    /// A prepared engine is taken up once nothing is held on the old one: no
    /// more is asked meanwhile, and what is held is finished (and answered) first.
    fn follow_engine(&mut self) {
        if self.accepted_until.is_some_and(|until|sys::now_ms()>=until) {self.stale=true;}
        // THE SITE'S OWN WORD, read every minute: what it serves, and until when
        // it still takes ours (`accepted`).
        if self.release_read.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(Some(r)) = self.release_read.take().and_then(|h| h.join().ok()) {
                self.note_protocol(&r);
                if let Some(e) = r["engine"].as_str().filter(|e| *e != self.rel.engine && e.len() == 12) {
                    let until = r["accepted"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|a| a["engine"].as_str() == Some(self.rel.engine.as_str()))
                        .and_then(|a| a["until"].as_str())
                        .unwrap_or("")
                        .to_string();
                    self.served_moved(e.to_string(), Some(until));
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
            match h.join().ok().and_then(|r|r.ok()) {
                Some(crate::engine_update::Prepared {release:rel,code:Some(code),..}) if rel.engine != self.rel.engine => {
                    self.ui.log(format!(
                        "{C_INFO}[引擎]{C_END} 新引擎 {}（{}）已准备好：手上的活做完就换上",
                        rel.engine,
                        crate::work::backend_label(&code)
                    ));
                    self.switch_to = Some((rel, code));
                }
                Some(crate::engine_update::Prepared {release,code:None,waiting_since_ms})=>{
                    self.engine_retry=Instant::now()+Duration::from_secs(60);
                    let elapsed=sys::now_ms().saturating_sub(waiting_since_ms.unwrap_or_else(sys::now_ms));
                    let remaining=self.cfg.native_wait_hours.saturating_mul(3_600_000).saturating_sub(elapsed);
                    self.ui.status(|s|s.note=format!("等待 {} 原生库，剩余最多 {:.1} 小时",release.engine,remaining as f64/3_600_000.0));
                }
                // The site's page still names ours (a cache): asked again in a minute.
                Some(_) => self.engine_retry = Instant::now() + Duration::from_secs(60),
                None => {
                    self.engine_retry = Instant::now() + Duration::from_secs(600);
                    self.ui.log(format!("{C_GO}[引擎]{C_END} 新引擎 {engine} 没能准备好，10 分钟后再试；旧引擎照常计算"));
                }
            }
        }
        let idle = self.tasks.is_empty()
            && self.packs.is_empty()
            && self.sending.is_empty()
            && self.groups.iter().all(|g| g.asking.is_none() && g.syncing.is_none());
        if idle {
            if let Some((rel, code)) = self.switch_to.take() {
                self.take_engine(rel, code);
            }
        }
    }

    fn take_engine(&mut self, rel: Release, code: Arc<Code>) {
        self.ui.log(format!("{C_INFO}[引擎]{C_END} 已换上网站引擎 {} → {}", self.rel.engine, rel.engine));
        self.swap_code(code);
        self.ui.status(|s| {s.engine = rel.engine.clone();s.note.clear();});
        self.rel = rel;
        self.accepted_until=None;self.waiting_said=None;
        if let Err(e)=crate::engine_update::remember(&self.paths,&self.rel) {self.ui.log(format!("{C_GO}[引擎]{C_END} 保存引擎选择失败：{e}"));}
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
        if self.memory_guard.draining() || self.native_load.is_some() || self.engine_load.is_some() || self.switch_to.is_some() || have == Some(false) || !self.cfg.native_engine
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
        // A PROTOCOL 8 ID'S POINTS come with its syncs (`since` → `points`); only
        // an id that has not synced lately is asked.
        let fresh = |g: &Group| g.points.as_ref().filter(|(t, _)| t.elapsed() < STATS_EVERY * 2).map(|(_, p)| p.clone());
        let groups: Vec<(String, Vec<Value>, Vec<Value>, Option<Value>)> = self
            .groups
            .iter()
            .map(|g| {
                let mine = |riven: bool| -> Vec<Value> {
                    self.recent.iter().filter(|(v, k, _)| *v == g.verifier && *k == riven).map(|(_, _, x)| x.clone()).collect()
                };
                (g.verifier.clone(), mine(false), mine(true), fresh(g))
            })
            .collect();
        let (net, ui, name, seen) = (self.net.clone(), self.ui.clone(), self.cfg.account.name.clone(), self.rank_seen.clone());
        let p7 = self.proto == 7;
        self.stats_job = Some(std::thread::spawn(move || {
            let (mut device, mut today, mut any) = (0u64, 0u64, false);
            for (v, _, _, have) in &groups {
                if let Some(p) = have.clone().or_else(|| net.post_ok("/api/board/points", &json!({ "verifier": v, "since": since }))) {
                    any = true;
                    device += p["points"].as_u64().unwrap_or(0);
                    today += p["today"]["points"].as_u64().unwrap_or(0);
                }
            }
            let dev = net.session.as_ref().map(|_| account::devices(&net));
            // THE SITE'S SIDE - who is computing, what is owed, the week's ranking -
            // read as the page reads it: files on the CDN, the worker's endpoint
            // where a file is not reached (docs/BOARD.md §"The public stats").
            let demand = net
                .get(&net.public_file(DEMAND_FILE))
                .ok()
                .map(|r| r.body)
                .filter(|b| b["ok"] == json!(true) && b["v"] == json!(1))
                .or_else(|| net.get("/api/board/demand").ok().map(|r| r.body))
                .filter(|b| b["ok"] == json!(true));
            // Signed in, the worker's ranking marks this account's row.
            let file = net.session.is_none().then(|| ranking_file(&net)).flatten();
            let top = file
                .or_else(|| net.get("/api/contributors?period=week&limit=40").ok().map(|r| r.body))
                .filter(|b| b["ok"] == json!(true));
            let rows: Vec<Value> = top.as_ref().and_then(|t| t["contributors"].as_array().cloned()).unwrap_or_default();
            let rates = paces(&seen, &rows, Instant::now());
            // Protocol 7's board rows' standing (protocol 8's comes with its syncs), and the riven gains'.
            let (mut board, mut riven, mut board_told, mut riven_told) = ((0u64, 0u64), (0u64, 0u64), false, false);
            for (v, orders, tasks, _) in &groups {
                if !orders.is_empty() {
                    if let Some(m) = net.post_ok("/api/board/mine", &json!({ "verifier": v, "orders": orders })) {
                        board_told = true;
                        for o in m["orders"].as_array().into_iter().flatten() {
                            match o["state"].as_str() {
                                Some("confirmed") => board.0 += 1,
                                Some("waiting") | Some("checking") => board.1 += 1,
                                _ => {}
                            }
                        }
                    }
                }
                if !tasks.is_empty() {
                    if let Some(m) = net.post_ok("/api/appraise/mine", &json!({ "verifier": v, "tasks": tasks })) {
                        riven_told = true;
                        for o in m["tasks"].as_array().into_iter().flatten() {
                            match o["state"].as_str() {
                                Some("confirmed") => riven.0 += 1,
                                Some("waiting") => riven.1 += 1,
                                _ => {}
                            }
                        }
                    }
                }
            }
            let no_board = groups.iter().all(|(_, o, _, _)| o.is_empty());
            let no_riven = groups.iter().all(|(_, _, t, _)| t.is_empty());
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
                    s.ranking = rows
                        .iter()
                        .zip(&rates)
                        .enumerate()
                        .map(|(i, (r, rate))| Ranked {
                            rank: i as u64 + 1,
                            name: r["name"].as_str().unwrap_or("匿名").chars().filter(|c| !c.is_control()).take(24).collect(),
                            points: r["week"].as_u64().unwrap_or(0),
                            rate: *rate,
                            you: r["you"] == json!(true),
                        })
                        .collect();
                }
                if riven_told || no_riven {
                    s.riven_states = riven;
                }
                // (Protocol 8's is set in `show`, from the syncs.)
                if p7 && (board_told || no_board) {
                    s.board_states = board;
                }
            });
        }));
    }
}

/// THE PUBLIC FILES the page reads before the worker (docs/BOARD.md §"The
/// public stats", §"Demand"): what is owed and who is computing, and the
/// rankings, each period's, written by the worker's cron.
const DEMAND_FILE: &str = "/demand.json";
const RANKING_FILE: &str = "/stats/contributors.json";
/// …the ranking passed over once older than this by the server's clock, or of another day (its keys are that day's).
const RANKING_STALE_MS: u64 = 30 * 60_000;

/// The week's ranking from the file, in the shape `/api/contributors` answers.
/// Empty because it outran the per-id grant interval is still evidence that
/// existing groups cannot occupy the available cores. An empty server reply,
/// absent signal or network error is not permission to open more groups.
fn capacity_limited(held:bool,received:bool,cooldown:bool,signal:bool,empty_reply:bool,network_error:bool)->bool {
    held || (received && cooldown && signal && !empty_reply && !network_error)
}

fn ranking_file(net: &Net) -> Option<Value> {
    let f = net.get(&net.public_file(RANKING_FILE)).ok().filter(|r| r.status == 200)?.body;
    let now = net.server_now_ms();
    let at = f["at"].as_str().and_then(sys::parse_iso)?;
    let fresh = f["v"] == json!(1) && now.saturating_sub(at) <= RANKING_STALE_MS && f["day"].as_str() == Some(&sys::iso(now)[..10]);
    let week = f["rankings"]["week"].as_array().filter(|_| fresh)?;
    Some(json!({ "ok": true, "total": week.len(), "contributors": week.iter().take(40).collect::<Vec<_>>() }))
}

/// THE RANKING'S PACE: no time is given beside a contributor's points, so it
/// is read off the ranking itself — each one's all-time `points` (the week's
/// sum drops as days pass out of it) against the oldest sighting of the last
/// `PACE_WINDOW`, as points a minute. Under `PACE_LEAST` apart says nothing
/// (the site's ranking is worked out a minute at a time and may answer the last
/// one for five): the first rates come with the second refresh or so.
const PACE_WINDOW: Duration = Duration::from_secs(1800);
const PACE_LEAST: Duration = Duration::from_secs(240);

fn paces(seen: &std::sync::Mutex<VecDeque<(Instant, HashMap<String, u64>)>>, rows: &[Value], now: Instant) -> Vec<Option<f64>> {
    let here: HashMap<String, u64> = rows.iter().filter_map(|r| Some((r["key"].as_str()?.to_string(), r["points"].as_u64()?))).collect();
    let Ok(mut seen) = seen.lock() else { return vec![None; rows.len()] };
    while seen.len() > 1 && seen.front().is_some_and(|(t, _)| now.duration_since(*t) > PACE_WINDOW) {
        seen.pop_front();
    }
    let base = seen.front().filter(|(t, _)| now.duration_since(*t) >= PACE_LEAST).cloned();
    if !here.is_empty() {
        seen.push_back((now, here));
    }
    rows.iter()
        .map(|r| {
            let (t, then) = base.as_ref()?;
            let (was, is) = (*then.get(r["key"].as_str()?)?, r["points"].as_u64()?);
            (is >= was).then(|| (is - was) as f64 * 60.0 / now.duration_since(*t).as_secs_f64())
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fast_pack_drained_before_control_can_fill_unused_group_budget() {
        assert!(capacity_limited(false,true,true,true,false,false));
        assert!(capacity_limited(true,true,false,true,false,false));
        assert!(!capacity_limited(false,false,true,true,false,false));
        assert!(!capacity_limited(false,true,false,true,false,false));
        assert!(!capacity_limited(false,true,true,false,false,false));
        assert!(!capacity_limited(false,true,true,true,true,false));
        assert!(!capacity_limited(false,true,true,true,false,true));
    }

    #[test]
    fn ranking_pace() {
        let seen = std::sync::Mutex::new(VecDeque::new());
        let t0 = Instant::now();
        let at = |a: u64, b: u64| vec![json!({"key": "a", "points": a}), json!({"key": "b", "points": b})];
        // First sight, then too soon: nothing yet.
        assert_eq!(paces(&seen, &at(1000, 50), t0), vec![None, None]);
        assert_eq!(paces(&seen, &at(1100, 50), t0 + Duration::from_secs(60)), vec![None, None]);
        // Five minutes on: against the first sighting, points a minute.
        let r = paces(&seen, &at(1500, 50), t0 + Duration::from_secs(300));
        assert_eq!(r, vec![Some(100.0), Some(0.0)]);
        // Someone new to the ranking has no rate; one whose points fell, none either.
        let r = paces(&seen, &[json!({"key": "a", "points": 900}), json!({"key": "c", "points": 7})], t0 + Duration::from_secs(600));
        assert_eq!(r, vec![None, None]);
        // Past the window the oldest sightings go, but one is always kept.
        let r = paces(&seen, &at(4000, 50), t0 + Duration::from_secs(4000));
        assert!(r[0].is_some());
    }
}
