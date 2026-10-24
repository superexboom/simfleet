// SPDX-License-Identifier: AGPL-3.0-or-later
//! THE CONSOLE: a dashboard redrawn in place — each group of this machine with
//! the tasks it holds and what it said, what the machine has given, its load,
//! and the week's ranking — on the terminal's alternate screen, so the window
//! comes back as it was when simfleet ends, with the log's last lines in it.
//!
//! It is drawn on a grid of cells and sent row by row, only the rows that
//! changed. NO ROW IS WRITTEN TO ITS LAST COLUMN and autowrap is off while it
//! runs: the Windows console in a CJK locale draws `·` or `●` two columns
//! wide, and a status row one column too long scrolled the whole screen there.
//! How this console draws such characters is measured when it starts (one `·`,
//! and where the cursor went), and every width counted that way.
//!
//! Without a terminal (output piped to a file) it is a plain log with a status
//! line every minute.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

/// What the keys change, read by the work loop.
pub struct Ctl {
    pub paused: AtomicBool,
    pub quit: AtomicBool,
    pub idle_cores: AtomicUsize,
    pub active_cores: AtomicUsize,
    pub max_cores: usize,
    /// The cap on groups; 0 is by the machine's size.
    pub max_slots: AtomicUsize,
    pub settings: Mutex<crate::settings::Channel>,
}

impl Ctl {
    pub fn new(cfg: &crate::config::Config, max: usize) -> Self {
        Self {paused:AtomicBool::new(false),quit:AtomicBool::new(false),idle_cores:AtomicUsize::new(cfg.idle_cores.clamp(1,max)),
            active_cores:AtomicUsize::new(cfg.active_cores.min(max)),max_cores:max,max_slots:AtomicUsize::new(cfg.max_slots),
            settings:Mutex::new(crate::settings::Channel {desired:crate::settings::Settings::from_config(cfg),message:String::new(),
                topology:crate::placement::report(cfg).unwrap_or_else(|e|e.to_string())})}
    }
    pub fn desired_settings(&self) -> crate::settings::Settings {
        let mut s=self.settings.lock().unwrap().desired.clone();
        s.idle_cores=self.idle_cores.load(Ordering::Relaxed);s.active_cores=self.active_cores.load(Ordering::Relaxed);s.max_slots=self.max_slots.load(Ordering::Relaxed);s
    }
}

/// One held task.
#[derive(Default, Clone)]
pub struct TaskLine {
    pub group: usize,
    pub riven: bool,
    pub label: String,
    pub done: u64,
    /// 0 for a task whose length is not known ahead (a search).
    pub total: u64,
    pub extra: String,
    pub secs: u64,
    /// Lanes running its jobs right now.
    pub lanes: usize,
}

/// One group (computer id).
#[derive(Default, Clone)]
pub struct GroupView {
    pub id: String,
    pub state: String,
    /// Still taking new work.
    pub open: bool,
    pub tasks: u64,
    pub work: u64,
}

/// A row of the week's ranking (`/api/contributors`).
#[derive(Default, Clone)]
pub struct Ranked {
    pub rank: u64,
    pub name: String,
    pub points: u64,
    /// Points a minute lately (fleet.rs `paces`), once measured.
    pub rate: Option<f64>,
    pub you: bool,
}

#[derive(Default, Clone)]
pub struct Status {
    /// "计算中", "等待任务", "已暂停：…"
    pub state: String,
    pub lanes_now: usize,
    pub idle: bool,
    pub engine: String,
    pub backend: String,
    pub gpu:crate::gpu::View,
    pub compute_memory_limit_mb:u64,
    pub session_tasks: u64,
    pub session_work: u64,
    /// Work computed this run, taken or not (the speed is read off it).
    pub computed_work: u64,
    pub computed_gpu_work: u64,
    /// Results computed and kept, not taken by the server yet (protocol 8).
    pub kept_rows: u64,
    pub kept_work: u64,
    /// Results confirmed and waiting: the shown sums, and the board's and
    /// the riven gains' apart.
    pub confirmed: u64,
    pub waiting: u64,
    pub board_states: (u64, u64),
    pub riven_states: (u64, u64),
    pub device_points: Option<u64>,
    pub today_points: Option<u64>,
    pub account: Option<String>,
    pub account_points: Option<u64>,
    pub contributor_rank: Option<u64>,
    pub computing_now: Option<u64>,
    /// The board's work by the Hong Kong relay (net.rs `road`), not direct.
    pub relayed: bool,
    /// Board rows the site owes (`/api/board/demand`), and the rows done an
    /// hour by volunteers and by the official machines.
    pub owed: Option<u64>,
    pub per_hour: Option<(u64, u64)>,
    pub ranking: Vec<Ranked>,
    pub ranking_total: Option<u64>,
    /// Busy lane time over the budget's lane time, last five minutes / session.
    pub utilisation: Option<f64>,
    pub utilisation_total: Option<f64>,
    pub tasks: Vec<TaskLine>,
    /// Groups taking work now, the cap on them, and the board work held (s).
    pub groups: usize,
    pub max_groups: usize,
    pub held_secs: Option<f64>,
    pub group_views: Vec<GroupView>,
    /// Each lane the budget allows now: the group whose job it runs, if any.
    pub lane_map: Vec<Option<usize>>,
    /// …and the kind of core it last ran on ('P' / 'E'), on an Intel hybrid.
    pub lane_kind: Vec<Option<char>>,
    /// The hybrid setting in effect ("优先 P 核"), on an Intel hybrid.
    pub core_pref: Option<&'static str>,
    /// WHAT THE LANES ARE GIVEN, the last few seconds: CPU time over their jobs'
    /// wall time (1.0 = a whole core each), that as cores, and the logical
    /// processors they ran on.
    pub lane_yield: Option<f64>,
    pub effective_cores: Option<f64>,
    pub processors: usize,
    /// The budget: cores while away, cores while in use (0 pauses then).
    pub idle_cores: usize,
    pub active_cores: usize,
    /// Total, CPU and GPU have independent active-time clocks.
    efficiency: [WorkRate; 3],
    pub note: String,
}

struct LogLine {
    group: Option<usize>,
    text: String,
}

struct Inner {
    lines: VecDeque<LogLine>,
    /// Lines ever dropped from the front, and the count printed on the main screen.
    first: u64,
    printed: u64,
    /// The dashboard is up (the alternate screen).
    live: bool,
    status: Status,
}

pub struct Ui {
    inner: Mutex<Inner>,
    tty: bool,
    started: Instant,
}

pub const C_DIM: &str = "\x1b[2m";
pub const C_OK: &str = "\x1b[32m";
pub const C_GO: &str = "\x1b[33m";
pub const C_BAD: &str = "\x1b[31m";
pub const C_INFO: &str = "\x1b[36m";
pub const C_END: &str = "\x1b[0m";

impl Status {
    pub fn activity(&mut self, now: Instant, cpu: bool, gpu: bool) {
        for (meter, running) in self.efficiency.iter_mut().zip([cpu || gpu, cpu, gpu]) {
            meter.tick(now, running);
        }
    }
    pub fn record_computed(&mut self, now: Instant, work: u64, gpu: bool) {
        let gpu_work = if gpu { work } else { 0 };
        self.computed_work += work;
        self.computed_gpu_work += gpu_work;
        self.efficiency[0].record(now, work);
        self.efficiency[if gpu { 2 } else { 1 }].record(now, work);
    }
}

/// One bucket per active second bounds history even at very high row rates.
/// Pausing freezes the clock and its ten-minute window, including GPU-only idle.
#[derive(Default, Clone)]
struct WorkRate {
    at: Option<Instant>,
    elapsed: Duration,
    running: bool,
    work: u64,
    recent: VecDeque<(u64, u64)>,
}
impl WorkRate {
    fn elapsed_at(&self, now: Instant) -> Duration {
        self.elapsed + if self.running { self.at.map_or(Duration::ZERO, |at| now.saturating_duration_since(at)) } else { Duration::ZERO }
    }
    fn tick(&mut self, now: Instant, running: bool) {
        self.elapsed = self.elapsed_at(now);
        self.at = Some(now);
        self.running = running;
        let cutoff = self.elapsed.as_secs().saturating_sub(600);
        while self.recent.front().is_some_and(|(second, _)| *second < cutoff) { self.recent.pop_front(); }
    }
    fn record(&mut self, now: Instant, work: u64) {
        self.tick(now, self.running);
        self.work += work;
        let second = self.elapsed.as_secs();
        if let Some((_, value)) = self.recent.back_mut().filter(|(at, _)| *at == second) {
            *value += work;
        } else if work > 0 { self.recent.push_back((second, work)); }
    }
    fn rates(&self, now: Instant) -> (f64, f64) {
        let seconds = self.elapsed_at(now).as_secs_f64().max(1.0);
        let work: u64 = self.recent.iter().filter(|(at, _)| *at as f64 + 1.0 > seconds - 600.0).map(|(_, work)| work).sum();
        (points(work) * 60.0 / seconds.min(600.0), points(self.work) * 60.0 / seconds)
    }
    fn state(&self) -> &'static str { if self.running { "运行" } else { "计时暂停" } }
}

/// Log lines kept for the panels and for paging back.
const KEEP: usize = 3000;
/// …and printed on the main screen when the dashboard closes.
const TAIL: usize = 40;

pub fn points(work: u64) -> f64 {
    work as f64 / 1e9
}

pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A rate of points a minute: "1,234", "56", "7.8".
fn per_min(r: f64) -> String {
    if r >= 100.0 {
        group(r.round() as u64)
    } else if r >= 10.0 {
        format!("{r:.0}")
    } else {
        format!("{r:.1}")
    }
}

/// 23468.6 → "23,468.6"
fn pts(p: f64) -> String {
    let whole = p.max(0.0).floor();
    format!("{}.{}", group(whole as u64), ((p - whole) * 10.0).floor().clamp(0.0, 9.0) as u64)
}

fn strip(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// 75 → "1m15s"
fn dur(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs / 60 % 60),
    }
}

fn hms(secs: u64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Total uses the union of CPU/GPU busy time; device rates use their own clocks.
/// Independent device rates must not be added to claim wall-clock throughput.
fn rates(s: &Status, now: Instant) -> (f64, f64, f64) {
    let (recent, average) = s.efficiency[0].rates(now);
    (recent, average, s.efficiency[2].rates(now).0)
}

impl Ui {
    pub fn new() -> Arc<Ui> {
        Arc::new(Ui {
            inner: Mutex::new(Inner { lines: VecDeque::new(), first: 0, printed: 0, live: false, status: Status::default() }),
            tty: std::io::stdout().is_terminal(),
            started: Instant::now(),
        })
    }
    pub fn log(&self, line: impl Into<String>) {
        self.push(None, line.into());
    }
    /// A line about one group, shown in that group's box.
    pub fn glog(&self, group: usize, line: impl Into<String>) {
        self.push(Some(group), line.into());
    }
    fn push(&self, group: Option<usize>, line: String) {
        let text = format!("{C_DIM}{}{C_END} {line}", crate::sys::clock());
        let mut g = self.inner.lock().unwrap();
        g.lines.push_back(LogLine { group, text: text.clone() });
        if g.lines.len() > KEEP {
            g.lines.pop_front();
            g.first += 1;
        }
        if !g.live {
            g.printed = g.first + g.lines.len() as u64;
            drop(g);
            if self.tty {
                println!("{text}{C_END}");
            } else {
                println!("{}", strip(&text));
            }
        }
    }
    pub fn status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.inner.lock().unwrap().status);
    }
    /// Lines logged while nothing showed them, printed plainly.
    pub fn flush_plain(&self) {
        let mut g = self.inner.lock().unwrap();
        if g.live {
            return;
        }
        let end = g.first + g.lines.len() as u64;
        let from = g.printed.max(g.first);
        for l in g.lines.range((from - g.first) as usize..) {
            println!("{}", strip(&l.text));
        }
        g.printed = end;
    }
    pub fn snapshot(&self) -> Status {
        self.inner.lock().unwrap().status.clone()
    }

    /// The minute line of a run without a terminal.
    fn plain_status(&self, s: &Status, ctl: &Ctl) -> String {
        let (rate, _, _) = rates(s, Instant::now());
        let mut a = format!(
            "{} | {}/{} 核 | 本次 {} 任务 {} 分 | {:.1} 分/分钟",
            s.state,
            s.lanes_now,
            ctl.max_cores,
            s.session_tasks,
            pts(points(s.session_work)),
            rate
        );
        if let Some(t) = s.today_points {
            a += &format!(" | 今日 {} 分", group(t));
        }
        if let Some(u) = s.utilisation {
            a += &format!(" | 利用率 {:.0}%", u * 100.0);
        }
        if let Some(eff) = s.effective_cores {
            a += &format!(" | 实得 {eff:.1} 核");
        }
        if s.kept_rows > 0 {
            a += &format!(" | 待交 {} 行 {} 分", s.kept_rows, pts(points(s.kept_work)));
        }
        if s.gpu.enabled {a+=&format!(" | GPU {} · 本程序 {}",gpu_activity(&s.gpu,self.started.elapsed()),gpu_percent(s.gpu.sample.utilization));}
        for t in &s.tasks {
            if t.extra=="NVIDIA GPU" {a+=&format!(" | {} GPU计算中",t.label);}
            else if t.total > 0 {
                a += &format!(" | {} {:.0}%", t.label, 100.0 * t.done as f64 / t.total as f64);
            } else {
                a += &format!(" | {} {} 场", t.label, group(t.done));
            }
        }
        a
    }

    /// The render-and-keys thread. Returns when `ctl.quit` is set and the screen is restored.
    pub fn run(self: Arc<Ui>, ctl: Arc<Ctl>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            if self.tty {
                self.paint(&ctl);
                return;
            }
            // Piped: a line on stdin is a key — `p` pauses, `q` quits.
            let (c2, u2) = (ctl.clone(), self.clone());
            std::thread::spawn(move || {
                let mut line = String::new();
                let mut view=View::default();
                while std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                    match line.trim() {
                        "q" | "Q" => c2.quit.store(true, Ordering::Relaxed),
                        "p" | "P" => {
                            let p = !c2.paused.load(Ordering::Relaxed);
                            c2.paused.store(p, Ordering::Relaxed);
                            u2.log(if p { "[暂停]" } else { "[继续]" });
                        }
                        "up"=>u2.key(KeyCode::Up,KeyModifiers::NONE,&c2,&mut view,24),
                        "down"=>u2.key(KeyCode::Down,KeyModifiers::NONE,&c2,&mut view,24),
                        "left"=>u2.key(KeyCode::Left,KeyModifiers::NONE,&c2,&mut view,24),
                        "right"=>u2.key(KeyCode::Right,KeyModifiers::NONE,&c2,&mut view,24),
                        "enter"=>u2.key(KeyCode::Enter,KeyModifiers::NONE,&c2,&mut view,24),
                        "esc"=>u2.key(KeyCode::Esc,KeyModifiers::NONE,&c2,&mut view,24),
                        key if key.chars().count()==1 => u2.key(KeyCode::Char(key.chars().next().unwrap()),KeyModifiers::NONE,&c2,&mut view,24),
                        _ => {}
                    }
                    line.clear();
                }
            });
            let mut last = Instant::now();
            while !ctl.quit.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                if last.elapsed() > Duration::from_secs(60) {
                    last = Instant::now();
                    let s = self.snapshot();
                    println!("{}", self.plain_status(&s, &ctl));
                }
            }
        })
    }

    fn paint(&self, ctl: &Ctl) {
        let _ = crossterm::terminal::enable_raw_mode();
        let mut out = std::io::stdout();
        let wide = probe_wide(&mut out);
        self.inner.lock().unwrap().live = true;
        let _ = write!(out, "\x1b[?1049h\x1b[?7l\x1b[?25l\x1b[2J");
        let mut prev: Vec<String> = Vec::new();
        let mut size = (0u16, 0u16);
        let mut view = View::default();
        let mut meter = SysMeter::default();
        let mut full_at = Instant::now();
        loop {
            let (w, h) = crossterm::terminal::size().unwrap_or((100, 30));
            // A WHOLE FRAME every ten seconds as well: whatever a console did to
            // the screen behind our back (a resize mid-frame) is painted over.
            if full_at.elapsed() > Duration::from_secs(10) {
                full_at = Instant::now();
                prev.clear();
            }
            if (w, h) != size {
                size = (w, h);
                prev.clear();
                let _ = write!(out, "\x1b[0m\x1b[2J");
            }
            meter.tick();
            // The last column stays empty (see the module's note).
            let mut c = Canvas::new((w as usize).saturating_sub(1), h as usize, wide);
            {
                let g = self.inner.lock().unwrap();
                let f = Frame {
                    s: &g.status,
                    lines: &g.lines,
                    ctl,
                    m: meter.view,
                    up: self.started.elapsed(),
                    multi: g.status.group_views.len() > 1,
                };
                draw(&mut c, &f, &mut view);
            }
            let mut buf = String::new();
            prev.resize(c.h, String::new());
            for y in 0..c.h {
                let row = c.row(y);
                if row != prev[y] {
                    buf += &format!("\x1b[{};1H{row}", y + 1);
                    prev[y] = row;
                }
            }
            if !buf.is_empty() {
                let _ = out.write_all(buf.as_bytes());
                let _ = out.flush();
            }
            if ctl.quit.load(Ordering::Relaxed) {
                break;
            }
            if event::poll(Duration::from_millis(250)).unwrap_or(false) {
                while let Ok(true) = event::poll(Duration::ZERO) {
                    match event::read() {
                        Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => self.key(k.code, k.modifiers, ctl, &mut view, h as usize),
                        _ => {}
                    }
                }
            }
        }
        let _ = write!(out, "\x1b[0m\x1b[2J\x1b[?25h\x1b[?7h\x1b[?1049l");
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
        // THE LOG'S TAIL, on the screen the user returns to.
        let mut g = self.inner.lock().unwrap();
        let end = g.first + g.lines.len() as u64;
        let from = g.printed.max(end.saturating_sub(TAIL as u64)).max(g.first);
        for l in g.lines.range((from - g.first) as usize..) {
            println!("{}{C_END}", l.text);
        }
        g.printed = end;
        g.live = false;
    }

    fn key(&self, code: KeyCode, mods: KeyModifiers, ctl: &Ctl, v: &mut View, h: usize) {
        if (v.settings_open || v.sidebar_open) && !(code==KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL)) {
            self.settings_key(code,ctl,v);
            return;
        }
        let page = h.saturating_sub(4).max(1);
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => ctl.quit.store(true, Ordering::Relaxed),
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => ctl.quit.store(true, Ordering::Relaxed),
            KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Char(' ') => {
                let p = !ctl.paused.load(Ordering::Relaxed);
                ctl.paused.store(p, Ordering::Relaxed);
                self.log(if p { format!("{C_GO}[暂停]{C_END} 手头的任务交还给服务器，按 P 继续") } else { format!("{C_OK}[继续]{C_END}") });
            }
            // + / - THE BUDGET WHILE AWAY; WITH SHIFT, WHILE IN USE (0 pauses then).
            KeyCode::Char(ch @ ('+' | '=' | '-' | '_')) => {
                let up = matches!(ch, '+' | '=');
                if mods.contains(KeyModifiers::SHIFT) {
                    let now = ctl.active_cores.load(Ordering::Relaxed);
                    let n = if up { (now + 1).min(ctl.max_cores) } else { now.saturating_sub(1) };
                    ctl.active_cores.store(n, Ordering::Relaxed);
                    self.log(if n == 0 { "使用电脑时暂停计算".to_string() } else { format!("使用电脑时用 {n} 个核心") });
                } else {
                    let now = ctl.idle_cores.load(Ordering::Relaxed);
                    let n = if up { (now + 1).min(ctl.max_cores) } else { now.saturating_sub(1).max(1) };
                    ctl.idle_cores.store(n, Ordering::Relaxed);
                    self.log(format!("离开电脑时用 {n} 个核心"));
                }
            }
            // [ / ] THE CAP ON GROUPS, from what it is now (by the machine's size, at 0).
            KeyCode::Char(ch @ ('[' | ']')) => {
                let now = match ctl.max_slots.load(Ordering::Relaxed) {
                    0 => crate::config::auto_max_slots(ctl.max_cores),
                    n => n,
                };
                let n = if ch == ']' { (now + 1).min(64) } else { now.saturating_sub(1).max(1) };
                ctl.max_slots.store(n, Ordering::Relaxed);
                self.log(format!("并发上限 {n} 路"));
            }
            KeyCode::Char('l') | KeyCode::Char('L') | KeyCode::Tab => {
                v.full_log = !v.full_log;
                v.scroll = 0;
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {v.settings_open=true;v.settings_row=0;v.edit=None;}
            KeyCode::Char('d') | KeyCode::Char('D') => {v.sidebar_open=true;v.sidebar_row=0;}
            KeyCode::PageUp | KeyCode::Up => v.scroll += if code == KeyCode::Up { 1 } else { page },
            KeyCode::PageDown | KeyCode::Down => v.scroll = v.scroll.saturating_sub(if code == KeyCode::Down { 1 } else { page }),
            KeyCode::End => v.scroll = 0,
            KeyCode::Home => v.scroll = usize::MAX,
            _ => {}
        }
    }

    fn settings_key(&self, code: KeyCode, ctl: &Ctl, v: &mut View) {
        if v.sidebar_open {
            let count = crate::settings::SIDEBAR_ITEMS.len();
            match code {
                KeyCode::Esc | KeyCode::Char('d') | KeyCode::Char('D') => v.sidebar_open = false,
                KeyCode::Up => v.sidebar_row = v.sidebar_row.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => v.sidebar_row = (v.sidebar_row + 1) % count,
                KeyCode::Home => v.sidebar_row = 0,
                KeyCode::End => v.sidebar_row = count - 1,
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right | KeyCode::Char('r') | KeyCode::Char('R') => {
                    let mut state = ctl.settings.lock().unwrap();
                    if matches!(code, KeyCode::Char('r') | KeyCode::Char('R')) { state.desired.hidden_sidebar.clear(); }
                    else { state.desired.toggle_sidebar(v.sidebar_row); }
                    state.message = "正在保存显示设置…".into();
                }
                _ => {}
            }
            return;
        }
        if let Some(input)=v.edit.as_mut() {
            match code {
                KeyCode::Esc=>v.edit=None,
                KeyCode::Backspace=>{input.pop();},
                KeyCode::Char(c)=>input.push(c),
                KeyCode::Enter=>{
                    let value=input.clone();
                    let mut desired=ctl.desired_settings();
                    let answer=desired.apply_value(v.settings_row,&value,crate::sys::cores()).and_then(|_| {
                        let mut probe=crate::config::Config::fresh(desired.idle_cores);desired.write(&mut probe);
                        crate::placement::validate(&probe)
                    });
                    match answer {
                        Ok(())=>{
                            ctl.idle_cores.store(desired.idle_cores,Ordering::Relaxed);ctl.active_cores.store(desired.active_cores,Ordering::Relaxed);
                            ctl.max_slots.store(desired.max_slots,Ordering::Relaxed);
                            let mut state=ctl.settings.lock().unwrap();state.desired=desired;state.message="正在保存设置…".into();v.edit=None;
                        }
                        Err(e)=>ctl.settings.lock().unwrap().message=e.to_string(),
                    }
                }
                _=>{}
            }
            return;
        }
        let count=ctl.desired_settings().rows().len();
        match code {
            KeyCode::Esc|KeyCode::Char('s')|KeyCode::Char('S')=>v.settings_open=false,
            KeyCode::Up=>v.settings_row=v.settings_row.saturating_sub(1),
            KeyCode::Down|KeyCode::Tab=>v.settings_row=(v.settings_row+1)%count,
            KeyCode::Home=>v.settings_row=0,
            KeyCode::End=>v.settings_row=count-1,
            KeyCode::Enter if v.settings_row == 14 => { v.sidebar_open=true; v.sidebar_row=0; },
            KeyCode::Enter=>v.edit=Some(ctl.desired_settings().rows()[v.settings_row].1.clone()),
            KeyCode::Left|KeyCode::Right=>{
                let choices=crate::settings::Settings::choices(v.settings_row);
                if !choices.is_empty() {
                    let old=ctl.desired_settings().rows()[v.settings_row].1.clone();
                    let i=choices.iter().position(|x|*x==old).unwrap_or(0);
                    let next=if code==KeyCode::Right {(i+1)%choices.len()}else{(i+choices.len()-1)%choices.len()};
                    v.edit=Some(choices[next].to_string());
                    self.settings_key(KeyCode::Enter,ctl,v);
                }
            }
            _=>{}
        }
    }
}

/// Whether this console draws East Asian ambiguous-width characters two
/// columns wide: one `·` written at the line's start, and where the cursor
/// ended. (conhost in a CJK locale draws `·` and `●` two wide yet `─` one, so
/// there every ambiguous one is counted two: never fewer columns than drawn.)
/// `SIMFLEET_WIDE=1` / `0` says it instead.
fn probe_wide(out: &mut std::io::Stdout) -> bool {
    if let Ok(v) = std::env::var("SIMFLEET_WIDE") {
        return v.trim() == "1";
    }
    let _ = write!(out, "\r·");
    let _ = out.flush();
    let wide = matches!(crossterm::cursor::position(), Ok((2, _)));
    let _ = write!(out, "\r  \r");
    let _ = out.flush();
    wide
}

// ---- the machine's load ---------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct SysView {
    cpu: Option<f64>,
    me: Option<f64>,
    /// In use, in all, this process.
    mem: Option<(u64, u64, u64)>,
}

#[derive(Default)]
struct SysMeter {
    last: Option<(u64, u64, u64)>,
    at: Option<Instant>,
    view: SysView,
}

impl SysMeter {
    /// A fresh reading once a second.
    fn tick(&mut self) {
        if self.at.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.at = Some(Instant::now());
        let now = crate::sys::cpu_times();
        if let (Some(a), Some(b)) = (self.last, now) {
            let all = b.0.saturating_sub(a.0) as f64;
            if all > 0.0 {
                self.view.cpu = Some((1.0 - b.1.saturating_sub(a.1) as f64 / all).clamp(0.0, 1.0));
                self.view.me = Some((b.2.saturating_sub(a.2) as f64 / all).clamp(0.0, 1.0));
            }
        }
        self.last = now;
        self.view.mem = crate::sys::memory();
    }
}

// ---- the grid ---------------------------------------------------------------------

/// A cell's colours, xterm's 256: `None` is the console's own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sty {
    fg: Option<u8>,
    bg: Option<u8>,
    bold: bool,
}

const PLAIN: Sty = Sty { fg: None, bg: None, bold: false };
const DIM: Sty = Sty { fg: Some(244), ..PLAIN };
const VAL: Sty = Sty { fg: Some(255), bold: true, ..PLAIN };
const HDR: Sty = Sty { fg: Some(252), bg: Some(237), bold: true };
const BAR: Sty = Sty { fg: Some(250), bg: Some(235), bold: false };
const KEY: Sty = Sty { fg: Some(255), bg: Some(239), bold: true };

const fn on(bg: u8) -> Sty {
    Sty { fg: Some(255), bg: Some(bg), bold: true }
}
const fn fg(n: u8) -> Sty {
    Sty { fg: Some(n), ..PLAIN }
}

/// Each group's colour.
fn gcolor(g: usize) -> u8 {
    [31, 64, 130, 97, 25, 125, 66, 136][g % 8]
}

/// The tail of a wide character.
const CONT: char = '\0';

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    st: Sty,
}

struct Canvas {
    w: usize,
    h: usize,
    wide: bool,
    cells: Vec<Cell>,
}

impl Canvas {
    fn new(w: usize, h: usize, wide: bool) -> Canvas {
        Canvas { w, h, wide, cells: vec![Cell { ch: ' ', st: PLAIN }; w * h] }
    }
    fn cw(&self, c: char) -> usize {
        if c.is_control() {
            return 0;
        }
        (if self.wide { c.width_cjk() } else { c.width() }).unwrap_or(0)
    }
    fn tw(&self, s: &str) -> usize {
        let mut n = 0;
        let mut esc = false;
        for c in s.chars() {
            if esc {
                esc = !c.is_ascii_alphabetic();
            } else if c == '\x1b' {
                esc = true;
            } else {
                n += self.cw(c);
            }
        }
        n
    }
    fn set(&mut self, x: usize, y: usize, ch: char, st: Sty, n: usize) {
        if y >= self.h || x + n > self.w {
            return;
        }
        let i = y * self.w + x;
        // A wide character this write cuts in two becomes a space.
        if self.cells[i].ch == CONT && x > 0 {
            self.cells[i - 1].ch = ' ';
        }
        if x + n < self.w && self.cells[i + n].ch == CONT {
            self.cells[i + n].ch = ' ';
        }
        self.cells[i] = Cell { ch, st };
        if n == 2 {
            self.cells[i + 1] = Cell { ch: CONT, st };
        }
    }
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, st: Sty) {
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                self.set(xx, yy, ' ', st, 1);
            }
        }
    }
    /// `s` from column `x`, at most `max` columns of it; the columns used.
    fn text(&mut self, x: usize, y: usize, max: usize, s: &str, st: Sty) -> usize {
        let mut used = 0;
        for c in s.chars() {
            let n = self.cw(c);
            if n == 0 {
                continue;
            }
            if used + n > max {
                break;
            }
            self.set(x + used, y, c, st, n);
            used += n;
        }
        used
    }
    /// …with the log's colour codes read into styles over `base`.
    fn ansi(&mut self, x: usize, y: usize, max: usize, s: &str, base: Sty) -> usize {
        let (mut st, mut used) = (base, 0);
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                if it.peek() == Some(&'[') {
                    it.next();
                    let mut code = String::new();
                    for d in it.by_ref() {
                        if d.is_ascii_alphabetic() {
                            if d == 'm' {
                                st = sgr(&code, st, base);
                            }
                            break;
                        }
                        code.push(d);
                    }
                }
                continue;
            }
            let n = self.cw(c);
            if n == 0 {
                continue;
            }
            if used + n > max {
                break;
            }
            self.set(x + used, y, c, st, n);
            used += n;
        }
        used
    }
    fn row(&self, y: usize) -> String {
        let mut out = String::new();
        let mut cur = None;
        for c in &self.cells[y * self.w..(y + 1) * self.w] {
            if c.ch == CONT {
                continue;
            }
            if cur != Some(c.st) {
                out += "\x1b[0";
                if c.st.bold {
                    out += ";1";
                }
                if let Some(f) = c.st.fg {
                    out += &format!(";38;5;{f}");
                }
                if let Some(b) = c.st.bg {
                    out += &format!(";48;5;{b}");
                }
                out.push('m');
                cur = Some(c.st);
            }
            out.push(c.ch);
        }
        out + "\x1b[0m"
    }
}

/// The log's SGR codes as cell styles. Faint is drawn grey: the Windows 10
/// console ignores it.
fn sgr(code: &str, mut st: Sty, base: Sty) -> Sty {
    for p in code.split(';') {
        match p {
            "" | "0" => st = base,
            "1" => st.bold = true,
            "2" => st.fg = Some(244),
            "31" => st.fg = Some(203),
            "32" => st.fg = Some(114),
            "33" => st.fg = Some(179),
            "34" => st.fg = Some(75),
            "35" => st.fg = Some(176),
            "36" => st.fg = Some(80),
            "37" => st.fg = Some(252),
            "39" => st.fg = base.fg,
            _ => {}
        }
    }
    st
}

// ---- the layout -------------------------------------------------------------------

#[derive(Default)]
struct View {
    /// The whole log instead of the dashboard (L).
    full_log: bool,
    /// Lines paged back from the newest.
    scroll: usize,
    settings_open: bool,
    settings_row: usize,
    sidebar_open: bool,
    sidebar_row: usize,
    edit: Option<String>,
}

struct Frame<'a> {
    s: &'a Status,
    lines: &'a VecDeque<LogLine>,
    ctl: &'a Ctl,
    m: SysView,
    up: Duration,
    multi: bool,
}

#[derive(Clone, Copy)]
enum Pick {
    All,
    Group(usize),
    Untagged,
}

fn draw(c: &mut Canvas, f: &Frame, v: &mut View) {
    let (w, h) = (c.w, c.h);
    if w < 20 || h < 3 {
        return;
    }
    title(c, f);
    footer(c, f, v);
    if v.sidebar_open {draw_sidebar_settings(c,f,v);return;}
    if v.settings_open {draw_settings(c,f,v);return;}
    let y = if h >= 5 { 2 } else { 1 };
    let bh = h - y - 1;
    if v.full_log {
        header(c, 0, y, w, " 全部日志 ", "PgUp/PgDn 翻页  End 最新  L 回到面板", HDR);
        logs(c, 0, y + 1, w, bh - 1, f, Pick::All, &mut v.scroll);
        return;
    }
    if w >= 96 && bh >= 14 {
        let settings = f.ctl.desired_settings();
        if ["contribution", "resources", "site", "ranking"].iter().all(|key| !settings.sidebar_visible(key)) {
            left(c,0,y,w,bh,f,v);
            return;
        }
        let sw = (w * 3 / 10).clamp(36, 54);
        let lw = w - sw - 1;
        side(c, lw + 1, y, sw, bh, f);
        left(c, 0, y, lw, bh, f, v);
    } else {
        compact(c, 0, y, w, bh, f, v);
    }
}

fn draw_settings(c: &mut Canvas, f: &Frame, v: &View) {
    let settings=f.ctl.desired_settings();
    let state=f.ctl.settings.lock().unwrap();
    let rows=settings.rows();let w=c.w;let h=c.h;
    header(c,0,2,w," 设置 ","确认后立即写入 TOML",HDR);
    if h<9 {c.text(1,3,w.saturating_sub(2),"窗口太小，请增大窗口后编辑设置。",DIM);return;}
    let visible=(h.saturating_sub(10)).min(rows.len()).max(1);
    let start=v.settings_row.saturating_sub(visible-1);
    for (i,(name,value,_)) in rows.iter().enumerate().skip(start).take(visible) {
        let y=4+i-start;let selected=i==v.settings_row;
        let style=if selected {HDR}else{PLAIN};
        if selected {c.fill(0,y,w,1,style);}
        let value=if selected {v.edit.as_ref().map(|x|format!("{x}_")).unwrap_or_else(||value.clone())}else{value.clone()};
        c.text(1,y,w.saturating_sub(2),&format!("{} {}: {}",if selected{">"}else{" "},name,if value.is_empty(){"全部"}else{&value}),style);
    }
    let y=4+visible;
    c.text(1,y,w.saturating_sub(2),rows[v.settings_row].2,DIM);
    c.text(1,y+1,w.saturating_sub(2),"调度/NUMA/合计内存上限改动：保存后排空手头任务，自动换进程生效。",DIM);
    c.text(1,y+2,w.saturating_sub(2),&state.message,Sty {fg:Some(80),..PLAIN});
    for (i,line) in state.topology.lines().skip(1).enumerate() {
        if y+3+i>=h-1 {break;}
        c.text(1,y+3+i,w.saturating_sub(2),line,DIM);
    }
}

fn draw_sidebar_settings(c: &mut Canvas, f: &Frame, v: &View) {
    let settings = f.ctl.desired_settings();
    let items = crate::settings::SIDEBAR_ITEMS;
    header(c,0,2,c.w," 侧栏显示 ","[x] 显示   [ ] 隐藏",HDR);
    let visible = c.h.saturating_sub(7).max(1);
    let start = v.sidebar_row.saturating_sub(visible - 1);
    for (i,(key,label)) in items.iter().enumerate().skip(start).take(visible) {
        let y = 3 + i - start;
        let style = if i == v.sidebar_row { HDR } else { PLAIN };
        if i == v.sidebar_row { c.fill(0,y,c.w,1,style); }
        c.text(1,y,c.w.saturating_sub(2),&format!("{} [{}] {}",if i==v.sidebar_row {">"} else {" "},if settings.sidebar_visible(key) {"x"} else {" "},label),style);
    }
    let y = 3 + visible;
    c.text(1,y,c.w.saturating_sub(2),if v.sidebar_row==2 {"CPU/GPU 按各自工作时间计速，无任务时暂停；两项不能直接相加。"} else {"隐藏整组会保留该组子项选择；全部整组隐藏后，任务区自动扩宽。"},DIM);
    c.text(1,y+1,c.w.saturating_sub(2),&f.ctl.settings.lock().unwrap().message,fg(80));
}

fn state_bg(state: &str) -> u8 {
    if state.starts_with("计算") {
        28
    } else if ["冷却", "连不上", "过旧", "协议", "出错"].iter().any(|k| state.contains(k)) {
        124
    } else {
        130
    }
}

fn title(c: &mut Canvas, f: &Frame) {
    let (s, w) = (f.s, c.w);
    c.fill(0, 0, w, if c.h >= 5 {2} else {1}, BAR);
    let clock = format!(" {} ", crate::sys::clock());
    let right = w.saturating_sub(c.tw(&clock));
    let mut x = c.text(0, 0, right, &format!(" simfleet {} ", env!("CARGO_PKG_VERSION")), on(24));
    let state = if s.state.is_empty() { "启动中" } else { &s.state };
    x += c.text(x, 0, right.saturating_sub(x), &format!(" {state} "), on(state_bg(state)));
    c.text(x,0,right.saturating_sub(x),&format!("  已运行 {}",hms(f.up.as_secs())),BAR);
    c.text(right, 0, w - right, &clock, BAR);
    if c.h < 5 { return; }
    // Keep backend and device state on their own row; sidebar owns core counts.
    let backend = s.backend.strip_suffix(" + NVIDIA GPU 预览").unwrap_or(&s.backend);
    let engine = if s.engine.is_empty() { "引擎加载中".into() } else {
        format!("引擎 {} {}",s.engine.get(..8).unwrap_or(&s.engine),backend)
    };
    let badge = if s.gpu.enabled { format!("GPU {}{}",s.gpu.state,if s.gpu.rows>0 {format!(" {}行",s.gpu.rows)} else {String::new()}) } else {String::new()};
    let engine_width = c.tw(&engine).min(if badge.is_empty(){w.saturating_sub(2)}else{w / 2});
    c.text(1,1,engine_width,&engine,DIM);
    if !badge.is_empty() { c.text(engine_width+3,1,w.saturating_sub(engine_width+4),&badge,fg(80)); }
}

fn footer(c: &mut Canvas, f: &Frame, v: &View) {
    let (y, w) = (c.h - 1, c.w);
    c.fill(0, y, w, 1, BAR);
    if v.sidebar_open {
        c.text(0,y,w," ↑↓ 选择  Space/Enter 显示或隐藏  R 全部显示  Esc 返回",BAR);
        return;
    }
    if v.settings_open {
        c.text(0,y,w,if v.edit.is_some(){" Enter 保存  Esc 取消编辑  Ctrl+C 退出程序"}else{" ↑↓ 选择  Enter 编辑  ←→ 切换选项  Esc 返回面板"},BAR);
        return;
    }
    let paused = f.ctl.paused.load(Ordering::Relaxed);
    let keys = [
        ("P", if paused { "继续" } else { "暂停" }),
        ("S", "设置"),
        ("D", "显示"),
        ("+/-", "空闲核心"),
        ("Shift +/-", "使用时核心"),
        ("[ ]", "并发上限"),
        ("L", if v.full_log { "面板" } else { "日志" }),
        ("PgUp/PgDn", "翻页"),
        ("Q", "退出"),
    ];
    let mut x = 0;
    for (k, d) in keys {
        x += c.text(x, y, w - x, &format!(" {k} "), KEY);
        x += c.text(x, y, w - x, &format!(" {d}  "), BAR);
    }
    if !f.s.note.is_empty() {
        let n = c.tw(&f.s.note) + 1;
        if x + n < w {
            c.text(w - n, y, n, &f.s.note, Sty { fg: Some(80), ..BAR });
        }
    }
}

/// A panel's title row: `left`, and `right` against its right edge.
fn header(c: &mut Canvas, x: usize, y: usize, w: usize, left: &str, right: &str, st: Sty) {
    if y >= c.h || w == 0 {
        return;
    }
    c.fill(x, y, w, 1, st);
    let used = c.text(x, y, w, left, st);
    let rw = c.tw(right);
    if !right.is_empty() && used + rw + 2 <= w {
        c.text(x + w - rw - 1, y, rw, right, Sty { bold: false, ..st });
    }
}

fn bar(c: &mut Canvas, x: usize, y: usize, w: usize, frac: f64, color: u8) {
    let fill = (frac.clamp(0.0, 1.0) * w as f64).round() as usize;
    c.fill(x, y, fill, 1, on(color));
    c.fill(x + fill, y, w - fill.min(w), 1, Sty { bg: Some(236), ..PLAIN });
}

/// A colour by how full: green, amber, red.
fn load_color(frac: f64) -> u8 {
    if frac < 0.6 {
        71
    } else if frac < 0.85 {
        172
    } else {
        167
    }
}

/// "label  [=====    ] text" across `w`.
fn bar_row(c: &mut Canvas, x: usize, y: usize, w: usize, label: &str, frac: Option<f64>, text: &str, color: u8) {
    c.text(x + 1, y, 7, label, DIM);
    let tw = c.tw(text);
    let bw = w.saturating_sub(8 + tw + 2);
    if bw >= 3 {
        bar(c, x + 8, y, bw, frac.unwrap_or(0.0), color);
    }
    c.text(x + 8 + bw + 1, y, tw, text, VAL);
}

fn kv(c: &mut Canvas, x: usize, y: usize, w: usize, label: &str, value: &str, rest: &str) {
    c.text(x + 1, y, 7, label, DIM);
    let used = c.text(x + 8, y, w.saturating_sub(9), value, VAL);
    c.text(x + 8 + used, y, w.saturating_sub(9 + used), rest, DIM);
}

fn task_row(c: &mut Canvas, x: usize, y: usize, w: usize, t: &TaskLine, chip: bool) {
    if w < 12 {
        return;
    }
    let color = gcolor(t.group);
    let mut cx = x + 1;
    if chip {
        cx += c.text(cx, y, 3, &format!(" {} ", t.group + 1), on(color)) + 1;
    }
    let (kind, kst) = if t.riven { ("紫卡", fg(213)) } else { ("榜单", fg(117)) };
    cx += c.text(cx, y, 4, kind, kst) + 1;
    let tail = if t.extra=="NVIDIA GPU" {format!("  {} GPU",dur(t.secs))}
        else if t.lanes == 0 { format!("  {} 等待", dur(t.secs)) } else { format!("  {} {}核", dur(t.secs), t.lanes) };
    let end = x + w - 1;
    let room = end.saturating_sub(cx);
    let label_w;
    if t.total > 0 {
        let frac = t.done as f64 / t.total as f64;
        let pct = if t.extra=="NVIDIA GPU" {" GPU".into()}else{format!(" {:>3.0}%", frac * 100.0)};
        let bw = (w / 5).clamp(6, 24);
        let rw = bw + c.tw(&pct) + c.tw(&tail);
        if rw + 10 <= room {
            let rx = end - rw;
            bar(c, rx, y, bw, frac, color);
            c.text(rx + bw, y, rw, &pct, VAL);
            c.text(rx + bw + c.tw(&pct), y, rw, &tail, DIM);
            label_w = rx - cx - 1;
        } else {
            let pw = c.tw(&pct);
            c.text(end.saturating_sub(pw), y, pw, &pct, VAL);
            label_w = room.saturating_sub(pw + 1);
        }
    } else {
        let info = format!("已打 {} 场 · {}", group(t.done), if t.extra.is_empty() { "0 个构筑" } else { &t.extra });
        let rw = c.tw(&info) + c.tw(&tail);
        if rw + 10 <= room {
            let rx = end - rw;
            let u = c.text(rx, y, rw, &info, VAL);
            c.text(rx + u, y, rw - u, &tail, DIM);
            label_w = rx - cx - 1;
        } else {
            label_w = room;
        }
    }
    c.text(cx, y, label_w, &t.label, PLAIN);
}

fn logs(c: &mut Canvas, x: usize, y: usize, w: usize, h: usize, f: &Frame, pick: Pick, scroll: &mut usize) {
    if h == 0 || w < 4 {
        return;
    }
    let rows: Vec<&LogLine> = f
        .lines
        .iter()
        .filter(|l| match pick {
            Pick::All => true,
            Pick::Group(g) => l.group == Some(g),
            Pick::Untagged => l.group.is_none(),
        })
        .collect();
    *scroll = (*scroll).min(rows.len().saturating_sub(h));
    let end = rows.len() - *scroll;
    let chips = f.multi && matches!(pick, Pick::All);
    for (i, l) in rows[end.saturating_sub(h)..end].iter().enumerate() {
        let mut cx = x + 1;
        if chips {
            if let Some(g) = l.group {
                c.text(cx, y + i, 1, &(g + 1).to_string(), on(gcolor(g)));
            }
            cx += 2;
        }
        c.ansi(cx, y + i, (x + w).saturating_sub(cx + 1), &l.text, PLAIN);
    }
}

fn group_header(c: &mut Canvas, x: usize, y: usize, w: usize, gi: usize, f: &Frame) {
    let g = f.s.group_views.get(gi).cloned().unwrap_or_default();
    let st = if g.open || g.tasks > 0 || f.s.tasks.iter().any(|t| t.group == gi) { on(gcolor(gi)) } else { on(239) };
    let left = if f.multi {
        format!(" 第 {} 路  {}  {} ", gi + 1, g.id, g.state)
    } else {
        format!(" 本机  {}  {} ", g.id, g.state)
    };
    let right = format!("{} 任务  {} 分", g.tasks, pts(points(g.work)));
    header(c, x, y, w, &left, &right, st);
}

fn left(c: &mut Canvas, x: usize, y: usize, w: usize, h: usize, f: &Frame, v: &mut View) {
    let n = f.s.group_views.len();
    if !f.multi {
        group_header(c, x, y, w, 0, f);
        let tasks = &f.s.tasks;
        let rows = tasks.len().max(1).min(h.saturating_sub(4));
        if tasks.is_empty() {
            c.text(x + 2, y + 1, w - 2, "等待任务……", DIM);
        }
        for (i, t) in tasks.iter().take(rows).enumerate() {
            task_row(c, x, y + 1 + i, w, t, false);
        }
        let ly = y + 1 + rows;
        let hint = if v.scroll > 0 { format!("往前翻了 {} 行  End 回到最新", v.scroll) } else { String::new() };
        header(c, x, ly, w, " 日志 ", &hint, HDR);
        logs(c, x, ly + 1, w, (y + h).saturating_sub(ly + 1), f, Pick::All, &mut v.scroll);
        return;
    }
    let sys_h = if h >= 24 { 5 } else if h >= 16 { 3 } else { 0 };
    let room = h - sys_h;
    let (per, extra) = (room / n, room % n);
    let mut yy = y;
    for gi in 0..n {
        let hh = per + usize::from(gi < extra);
        if hh == 0 {
            continue;
        }
        group_header(c, x, yy, w, gi, f);
        let mine: Vec<&TaskLine> = f.s.tasks.iter().filter(|t| t.group == gi).collect();
        let tr = mine.len().min(hh - 1);
        for (i, t) in mine.iter().take(tr).enumerate() {
            task_row(c, x, yy + 1 + i, w, t, false);
        }
        logs(c, x, yy + 1 + tr, w, hh - 1 - tr, f, Pick::Group(gi), &mut 0);
        yy += hh;
    }
    if sys_h > 0 {
        header(c, x, yy, w, " 系统消息 ", "", HDR);
        logs(c, x, yy + 1, w, sys_h - 1, f, Pick::Untagged, &mut 0);
    }
}

fn gpu_percent(value:Option<f64>)->String {value.map_or("--".into(),|v|format!("{:.0}%",v*100.0))}
fn gpu_activity(g:&crate::gpu::View,up:Duration)->String {
    if g.rows>0 {
        let pulse=["|","/","-","\\"][(up.as_millis()/250%4) as usize];
        format!("{pulse} {} {}行 / {}次 · {}s",g.state,g.rows,group(g.runs),g.seconds)
    }else{format!("{} · 完成{} / 回退{}",g.state,group(g.finished),group(g.fallback))}
}

fn side(c: &mut Canvas, x: usize, y: usize, w: usize, h: usize, f: &Frame) {
    let s = f.s;
    let settings = f.ctl.desired_settings();
    let visible = |key| settings.sidebar_visible(key);
    let end = y + h;
    let mut yy = y;
    let (rate, avg, gpu_rate) = rates(s, Instant::now());

    if visible("contribution") {
    header(c, x, yy, w, " 贡献 ", "近10活跃分", HDR);
    yy += 1;
    let mut rows: Vec<(&str, String, String)> = vec![
        ("本次", format!("{} 分", pts(points(s.session_work))), format!("  {} 个任务", group(s.session_tasks))),
        ("速度", format!("{rate:.1} 分/分钟"), format!("  均 {avg:.1} · {}",s.efficiency[0].state())),
        ("CPU速", format!("{:.1} 分/分钟",s.efficiency[1].rates(Instant::now()).0), format!("  {}",s.efficiency[1].state())),
    ];
    if s.gpu.enabled || s.computed_gpu_work > 0 {
        rows.push(("GPU速", format!("{gpu_rate:.1} 分/分钟"), format!("  {} · 已算 {} 分",s.efficiency[2].state(),pts(points(s.computed_gpu_work)))));
    }
    if s.kept_rows > 0 {
        rows.push(("待交", format!("{} 分", pts(points(s.kept_work))), format!("  {} 行，随下次同步提交", group(s.kept_rows))));
    }
    if let Some(t) = s.today_points {
        rows.push(("今日", format!("{} 分", group(t)), String::new()));
    }
    match (&s.account, s.account_points) {
        (Some(name), Some(p)) => rows.push((
            "账号",
            format!("{} 分", group(p)),
            format!("  {}{}", name, s.contributor_rank.map(|r| format!(" 段位 {r}")).unwrap_or_default()),
        )),
        _ => {
            if let Some(p) = s.device_points {
                rows.push(("本机", format!("{} 分", group(p)), "  未登录".into()));
            }
        }
    }
    if s.confirmed + s.waiting > 0 {
        rows.push(("确认", format!("{}", s.confirmed), format!(" 已确认  {} 待确认", s.waiting)));
    }
    for (l, val, rest) in rows {
        let key = match l { "本次"=>"session", "速度"|"CPU速"|"GPU速"=>"speed", "待交"=>"pending", "今日"=>"today", "账号"|"本机"=>"account", _=>"confirmation" };
        if !visible(key) { continue; }
        if yy < end {
            kv(c, x, yy, w, l, &val, &rest);
            yy += 1;
        }
    }
    }

    if visible("resources") && yy + 3 < end {
        let device=s.gpu.device.strip_prefix("NVIDIA GeForce ").unwrap_or(&s.gpu.device);
        header(c, x, yy, w, " 资源 ", if s.gpu.enabled {device}else{""}, HDR);
        yy += 1;
        let pct = |v: Option<f64>| v.map_or("--".into(), |v| format!("{:>3.0}%", v * 100.0));
        let m = f.m;
        if visible("cpu") { bar_row(c, x, yy, w, "CPU", m.cpu, &pct(m.cpu), load_color(m.cpu.unwrap_or(0.0))); yy += 1; }
        if visible("process_cpu") && yy < end {
            bar_row(c, x, yy, w, "本程序", m.me, &pct(m.me), 31);
            yy += 1;
        }
        if s.gpu.enabled && visible("gpu") {
            if yy<end {kv(c,x,yy,w,"GPU状态",&gpu_activity(&s.gpu,f.up),"");yy+=1;}
            if yy<end && !s.gpu.reason.is_empty(){kv(c,x,yy,w,"上次错误",&s.gpu.reason,"");yy+=1;}
            if yy<end {bar_row(c,x,yy,w,"程序GPU",s.gpu.sample.utilization,&gpu_percent(s.gpu.sample.utilization),75);yy+=1;}
        }
        if s.gpu.enabled && visible("vram") {
            if yy<end {
                let dedicated=s.gpu.sample.dedicated_bytes.map_or("--".into(),|n|format!("{:.2} GiB",n as f64/(1u64<<30) as f64));
                let shared=s.gpu.sample.shared_bytes.map_or(String::new(),|n|format!(" · 共享 {:.0} MiB",n as f64/(1u64<<20) as f64));
                kv(c,x,yy,w,"显存",&dedicated,&shared);yy+=1;
            }
        }
        if visible("memory_limit") && yy<end && cfg!(windows) && s.compute_memory_limit_mb>0 {kv(c,x,yy,w,"总限额",&format!("{:.1} GiB",s.compute_memory_limit_mb as f64/1024.0)," CPU+GPU 提交");yy+=1;}
        if let (Some((used, all, me)), true) = (m.mem, visible("memory") && yy < end) {
            let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
            let frac = used as f64 / all.max(1) as f64;
            bar_row(c, x, yy, w, "内存", Some(frac), &format!("{:.1}/{:.0}G", gb(used), gb(all)), load_color(frac));
            yy += 1;
            if yy < end {
                kv(c, x, yy, w, "占用", &format!("{:.0} MB", me as f64 / (1u64 << 20) as f64), "  本程序");
                yy += 1;
            }
        }
        if visible("utilisation") && yy < end {
            bar_row(c, x, yy, w, "利用率", s.utilisation, &pct(s.utilisation), 31);
            yy += 1;
        }
        if visible("budget") && yy < end {
            let busy = if s.active_cores == 0 { "暂停".to_string() } else { format!("{} 核", s.active_cores) };
            kv(c, x, yy, w, "预算", &format!("离开 {} 核", s.idle_cores), &format!("  使用时 {busy}"));
            yy += 1;
        }
        if visible("concurrency") && yy < end && s.max_groups > 0 {
            let held = s.held_secs.map_or(String::new(), |h| format!(" · 积压 {h:.0}s"));
            kv(c, x, yy, w, "并发", &format!("{} 路", s.groups), &format!("  上限 {}{held}", s.max_groups));
            yy += 1;
        }
        // WHAT THE LANES REALLY GET: lanes crowded onto a few processors, or
        // starved by other programs, are less than a core each.
        if let (Some(eff), Some(share), true) = (s.effective_cores, s.lane_yield, visible("effective_cores") && yy < end) {
            c.text(x + 1, yy, 7, "实得", DIM);
            let st = Sty { fg: Some(if share < 0.6 { 203 } else if share < 0.85 { 179 } else { 255 }), bold: true, ..PLAIN };
            let u = c.text(x + 8, yy, w.saturating_sub(9), &format!("{eff:.1} 核"), st);
            // No processor count where the system tells none (macOS).
            let rest = if s.processors == 0 {
                format!("  {:.0}%/线程", share * 100.0)
            } else {
                format!("  {:.0}%/线程 · {} 处理器", share * 100.0, s.processors)
            };
            c.text(x + 8 + u, yy, w.saturating_sub(9 + u), &rest, DIM);
            yy += 1;
        }
        if let (Some(pref), true) = (s.core_pref, visible("effective_cores") && yy < end) {
            let (p, e) = s.lane_kind.iter().flatten().fold((0, 0), |a, k| if *k == 'P' { (a.0 + 1, a.1) } else { (a.0, a.1 + 1) });
            // Where each lane ran is known on an Intel hybrid, not on Apple silicon.
            let tail = if s.lane_kind.is_empty() { String::new() } else { format!("  在 P {p} / E {e}") };
            kv(c, x, yy, w, "大小核", pref, &tail);
            yy += 1;
        }
        // THE LANES: each core of the machine, coloured by the group whose job it runs.
        let per = (w.saturating_sub(9) / 3).max(1);
        let max = f.ctl.max_cores;
        let rows = max.div_ceil(per);
        if visible("core_map") && yy + rows <= end && rows <= 4 {
            c.text(x + 1, yy, 7, "核心", DIM);
            for i in 0..max {
                let st = match s.lane_map.get(i) {
                    Some(Some(g)) => on(gcolor(*g)),
                    Some(None) => Sty { bg: Some(240), ..PLAIN },
                    None => Sty { bg: Some(234), ..PLAIN },
                };
                let (cx, cy) = (x + 8 + (i % per) * 3, yy + i / per);
                c.fill(cx, cy, 2, 1, st);
                // Where the lane last ran, on an Intel hybrid.
                if let Some(Some(k)) = s.lane_kind.get(i) {
                    c.text(cx, cy, 1, &k.to_string(), st);
                }
            }
            yy += rows;
        }
    }

    if visible("site") && yy + 3 < end && (s.computing_now.is_some() || s.owed.is_some()) {
        header(c, x, yy, w, " 全站 ", "", HDR);
        yy += 1;
        if let Some(n) = s.computing_now {
            let ph = s.per_hour.map(|(v, o)| format!("  每小时 志愿 {} / 官方 {}", group(v), group(o))).unwrap_or_default();
            kv(c, x, yy, w, "在线", &n.to_string(), &ph);
            yy += 1;
        }
        if let (Some(o), true) = (s.owed, yy < end) {
            kv(c, x, yy, w, "待算", &format!("{} 行", group(o)), "  榜单");
            yy += 1;
        }
        if yy < end {
            kv(c, x, yy, w, "线路", if s.relayed { "香港中转" } else { "直连" }, "  领活与交答案，每 10 分钟测一次");
            yy += 1;
        }
    }

    if visible("ranking") && yy + 2 < end && !s.ranking.is_empty() {
        // THE PACE beside the points: what each one's points grew by a minute
        // over the last half hour, as the ranking showed them (fleet.rs `paces`).
        let pace = if s.ranking.iter().any(|r| r.rate.is_some()) { "每分钟按近半小时" } else { "每分钟测量中" };
        let left = " 本周贡献榜 ";
        let mut total = match s.ranking_total {
            Some(t) => format!("共 {t} 人 · {pace}"),
            None => pace.to_string(),
        };
        // Too narrow for the note: the count alone.
        if c.tw(left) + c.tw(&total) + 2 > w {
            total = s.ranking_total.map(|t| format!("共 {t} 人")).unwrap_or_default();
        }
        header(c, x, yy, w, left, &total, HDR);
        yy += 1;
        for r in &s.ranking {
            if yy >= end {
                break;
            }
            let base = if r.you { on(24) } else { PLAIN };
            if r.you {
                c.fill(x, yy, w, 1, base);
            }
            let rank_st = match r.rank {
                1 => Sty { fg: Some(220), bold: true, ..base },
                2 => Sty { fg: Some(252), bold: true, ..base },
                3 => Sty { fg: Some(173), bold: true, ..base },
                _ => Sty { fg: Some(244), ..base },
            };
            c.text(x + 1, yy, 4, &format!("{:>3}", r.rank), rank_st);
            let p = group(r.points);
            let pw = c.tw(&p);
            c.text(x + w - pw - 1, yy, pw, &p, Sty { fg: Some(255), ..base });
            // The pace, where the name keeps room enough.
            let rate = r.rate.map(|v| format!("{}/分", per_min(v))).unwrap_or_default();
            let rw = c.tw(&rate);
            let rate_at = (x + w).saturating_sub(pw + 1 + rw + 2);
            let (name_w, rw) = if rw > 0 && rate_at >= x + 6 + 8 { (rate_at - (x + 6) - 1, rw) } else { (w.saturating_sub(pw + 8), 0) };
            if rw > 0 {
                c.text(rate_at, yy, rw, &rate, Sty { fg: Some(if r.rate.is_some_and(|v| v > 0.0) { 114 } else { 244 }), ..base });
            }
            let name = if r.you { format!("{}（你）", r.name) } else { r.name.clone() };
            c.text(x + 6, yy, name_w, &name, base);
            yy += 1;
        }
    }
}

fn compact(c: &mut Canvas, x: usize, y: usize, w: usize, h: usize, f: &Frame, v: &mut View) {
    let s = f.s;
    let settings = f.ctl.desired_settings();
    let visible = |key| settings.sidebar_visible(key);
    let end = y + h;
    let mut yy = y;
    let (rate, _, _) = rates(s, Instant::now());
    let mut a = String::new();
    if visible("session") { a += &format!(" 本次 {C_OK}{}{C_END} 分 · {} 任务",pts(points(s.session_work)),group(s.session_tasks)); }
    if visible("speed") { a += &format!(" · {C_OK}{rate:.1}{C_END} 分/活跃分钟"); }
    if visible("pending") && s.kept_rows > 0 {
        a += &format!(" · 待交 {} 分", pts(points(s.kept_work)));
    }
    if let Some(t) = s.today_points.filter(|_| visible("today")) {
        a += &format!(" · 今日 {} 分", group(t));
    }
    if let (Some(eff), Some(share), true) = (s.effective_cores, s.lane_yield, visible("resources") && visible("effective_cores")) {
        let col = if share < 0.6 { C_BAD } else if share < 0.85 { C_GO } else { "" };
        a += &format!(" · 实得 {col}{eff:.1}{C_END} 核");
    }
    if visible("account") { match (&s.account, s.account_points) {
        (Some(_), Some(p)) => a += &format!(" · 账号 {} 分", group(p)),
        _ => {
            if let Some(p) = s.device_points {
                a += &format!(" · 本机 {} 分", group(p));
            }
        }
    } }
    if visible("contribution") {
        c.ansi(x, yy, w, &a, PLAIN); yy += 1;
        if visible("speed") && yy < end {
            c.text(x+1,yy,w.saturating_sub(2),&format!("CPU {:.1}/分 {} · GPU {:.1}/分 {}",s.efficiency[1].rates(Instant::now()).0,s.efficiency[1].state(),s.efficiency[2].rates(Instant::now()).0,s.efficiency[2].state()),DIM);yy+=1;
        }
    }
    if visible("resources") && yy < end && w >= 40 {
        // CPU, memory and the lanes' use, a third of the width each.
        let third = w / 3;
        let mem = f.m.mem.map(|(u, a, _)| u as f64 / a.max(1) as f64);
        for (i, (l, v)) in [("CPU", f.m.cpu), ("内存", mem), ("利用率", s.utilisation)].into_iter().enumerate() {
            if !visible(["cpu","memory","utilisation"][i]) { continue; }
            let x0 = x + i * third;
            let t = v.map_or(" --".into(), |v| format!("{:>3.0}%", v * 100.0));
            let lw = c.text(x0 + 1, yy, 7, l, DIM) + 2;
            let bw = third.saturating_sub(lw + 7);
            if bw >= 2 {
                bar(c, x0 + lw, yy, bw, v.unwrap_or(0.0), if i == 2 { 31 } else { load_color(v.unwrap_or(0.0)) });
            }
            c.text(x0 + lw + bw + 1, yy, 5, &t, VAL);
        }
        yy += 1;
    }
    let budget = end.saturating_sub(yy + 3);
    if visible("resources") && visible("gpu") && s.gpu.enabled && yy<end {
        c.text(x+1,yy,w.saturating_sub(2),&format!("GPU {} · 本程序 {}",gpu_activity(&s.gpu,f.up),gpu_percent(s.gpu.sample.utilization)),fg(75));yy+=1;
    }
    let budget=budget.min(end.saturating_sub(yy+3));
    for t in s.tasks.iter().take(budget) {
        task_row(c, x, yy, w, t, f.multi);
        yy += 1;
    }
    if s.tasks.is_empty() && yy < end {
        c.text(x + 2, yy, w - 2, "等待任务……", DIM);
        yy += 1;
    }
    if yy + 1 < end {
        let hint = if v.scroll > 0 { format!("往前翻了 {} 行", v.scroll) } else { String::new() };
        header(c, x, yy, w, " 日志 ", &hint, HDR);
        logs(c, x, yy + 1, w, end - yy - 1, f, Pick::All, &mut v.scroll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(c: &Canvas, y: usize) -> String {
        c.cells[y * c.w..(y + 1) * c.w].iter().filter(|x| x.ch != CONT).map(|x| x.ch).collect()
    }
    #[test]
    fn gpu_resources_and_indicator_show_without_fake_progress() {
        let ctl=Ctl::new(&crate::config::Config::fresh(16),16);let lines=VecDeque::new();
        let mut s=Status::default();s.state="计算中".into();
        s.gpu=crate::gpu::View{enabled:true,state:"计算".into(),device:"NVIDIA GeForce RTX 5080".into(),rows:16,runs:16000,seconds:2,finished:32,fallback:0,reason:String::new(),
            sample:crate::gpu_meter::Sample{utilization:Some(0.87),dedicated_bytes:Some(4<<30),shared_bytes:Some(100<<20)}};
        s.tasks.push(TaskLine{label:"fulmin_prime".into(),total:1000,extra:"NVIDIA GPU".into(),..Default::default()});
        for (w,h) in [(150,44),(110,30),(70,24)] {
            let f=Frame{s:&s,lines:&lines,ctl:&ctl,m:SysView::default(),up:Duration::from_secs(10),multi:false};
            let mut c=Canvas::new(w-1,h,false);draw(&mut c,&f,&mut View::default());
            let rendered=(0..h).map(|y|text_of(&c,y)).collect::<Vec<_>>();
            let text=rendered.join("\n");assert!(text.contains("GPU"));assert!(text.contains("87%"));assert!(text.contains("16行"));
            assert!(!text.contains("0%  0s GPU"));
            assert!(!rendered[0].contains("核心") && !rendered[0].contains("并发") && !rendered[0].contains("在线"));
            assert!(rendered[1].contains("引擎"));
            for row in rendered {assert!(row.chars().map(|ch|c.cw(ch)).sum::<usize>()<w);}
        }
        ctl.settings.lock().unwrap().desired.hidden_sidebar=vec!["gpu".into(),"ranking".into()];
        let f=Frame{s:&s,lines:&lines,ctl:&ctl,m:SysView::default(),up:Duration::ZERO,multi:false};
        let mut c=Canvas::new(54,42,false);side(&mut c,0,0,54,42,&f);
        let text=(0..c.h).map(|y|text_of(&c,y)).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("GPU状态") && !text.contains("程序GPU") && text.contains("显存"));
        s.gpu.sample=Default::default();s.gpu.state="故障回退 CPU".into();s.gpu.rows=0;
        assert_eq!(gpu_percent(s.gpu.sample.utilization),"--");assert!(gpu_activity(&s.gpu,Duration::ZERO).contains("故障回退"));
    }

    #[test]
    fn settings_menu_navigation_validation_and_escape() {
        let cfg=crate::config::Config::fresh(2);let ctl=Ctl::new(&cfg,crate::sys::cores());let ui=Ui::new();let mut v=View::default();
        ui.key(KeyCode::Char('s'),KeyModifiers::NONE,&ctl,&mut v,24);assert!(v.settings_open);
        v.settings_row=9;ui.key(KeyCode::Enter,KeyModifiers::NONE,&ctl,&mut v,24);
        ui.key(KeyCode::Backspace,KeyModifiers::NONE,&ctl,&mut v,24);ui.key(KeyCode::Backspace,KeyModifiers::NONE,&ctl,&mut v,24);
        ui.key(KeyCode::Char('0'),KeyModifiers::NONE,&ctl,&mut v,24);ui.key(KeyCode::Enter,KeyModifiers::NONE,&ctl,&mut v,24);
        assert_eq!(ctl.desired_settings().native_wait_hours,0);
        v.settings_row=14;ui.key(KeyCode::Enter,KeyModifiers::NONE,&ctl,&mut v,24);
        assert!(v.sidebar_open);v.sidebar_row=10;
        ui.key(KeyCode::Char(' '),KeyModifiers::NONE,&ctl,&mut v,24);
        assert!(!ctl.desired_settings().sidebar_visible("gpu"));
        let mut saved=cfg.clone();ctl.desired_settings().write(&mut saved);
        let restored:crate::config::Config=toml::from_str(&toml::to_string(&saved).unwrap()).unwrap();
        assert_eq!(restored.hidden_sidebar,vec!["gpu"]);
        assert!(!ctl.desired_settings().topology_changed(&crate::settings::Settings::from_config(&cfg)));
        ui.key(KeyCode::Esc,KeyModifiers::NONE,&ctl,&mut v,24);assert!(!v.sidebar_open && v.settings_open);
        v.settings_row=0;v.edit=Some("0".into());ui.key(KeyCode::Enter,KeyModifiers::NONE,&ctl,&mut v,24);
        assert!(v.edit.is_some());assert_eq!(ctl.desired_settings().idle_cores,2);
        ui.key(KeyCode::Esc,KeyModifiers::NONE,&ctl,&mut v,24);assert!(v.settings_open);
        ui.key(KeyCode::Esc,KeyModifiers::NONE,&ctl,&mut v,24);assert!(!v.settings_open);assert!(!ctl.quit.load(Ordering::Relaxed));
    }

    #[test]
    fn settings_screen_fits_narrow_and_wide_consoles() {
        let ctl=Ctl::new(&crate::config::Config::fresh(2),16);let status=Status::default();let lines=VecDeque::new();
        let f=Frame{s:&status,lines:&lines,ctl:&ctl,m:SysView::default(),up:Duration::ZERO,multi:false};
        for (w,h,wide,selected) in [(39,12,true,20),(109,32,false,14)] {
            let mut v=View{settings_open:true,settings_row:14,sidebar_open:true,sidebar_row:selected,..Default::default()};
            let mut c=Canvas::new(w,h,wide);draw(&mut c,&f,&mut v);
            for y in 0..h {assert_eq!(text_of(&c,y).chars().map(|ch|c.cw(ch)).sum::<usize>(),w);}
            assert!((0..h).map(|y|text_of(&c,y)).collect::<Vec<_>>().join("\n").contains("侧栏显示"));
            if w==109 {if let Ok(path)=std::env::var("SIMFLEET_SETTINGS_PREVIEW") {std::fs::write(path,(0..h).map(|y|text_of(&c,y)).collect::<Vec<_>>().join("\n")).unwrap();}}
        }
    }

    #[test]
    fn rows_never_pass_the_width() {
        for wide in [false, true] {
            let mut c = Canvas::new(10, 1, wide);
            c.text(0, 0, 10, "──·●中文abcdef", PLAIN);
            let n: usize = text_of(&c, 0).chars().map(|ch| c.cw(ch)).sum();
            assert_eq!(n, 10, "wide={wide}");
        }
    }

    #[test]
    fn a_cut_wide_character_becomes_a_space() {
        let mut c = Canvas::new(6, 1, false);
        c.text(0, 0, 6, "中文字", PLAIN);
        c.text(1, 0, 1, "x", PLAIN);
        assert_eq!(text_of(&c, 0), " x文字");
    }

    #[test]
    fn log_colours_are_read() {
        let mut c = Canvas::new(12, 1, false);
        c.ansi(0, 0, 12, "\x1b[32mok\x1b[0m done", PLAIN);
        assert_eq!(c.cells[0].st.fg, Some(114));
        assert_eq!(c.cells[3].st, PLAIN);
        assert_eq!(text_of(&c, 0), "ok done     ");
    }

    /// The dashboard at a few sizes, as text: `cargo test preview -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn preview() {
        let mut config=crate::config::Config::fresh(10);config.active_cores=10;
        let ctl = Ctl::new(&config,16);
        let s = Status {
            state: "计算中".into(),
            lanes_now: 10,
            engine: "b48cf15757cb".into(),
            backend: "原生 AVX2".into(),
            session_tasks: 2056,
            session_work: 23_468_600_000_000,
            confirmed: 58,
            waiting: 2,
            device_points: Some(0),
            today_points: Some(21_388),
            computing_now: Some(37),
            owed: Some(23_320),
            per_hour: Some((1212, 77)),
            ranking: (1..=12)
                .map(|i| Ranked {
                    rank: i,
                    name: if i == 3 { "季·术宅".into() } else { format!("player{i}") },
                    points: 700_000 / i,
                    rate: (i != 4).then(|| 700.0 / (i * i) as f64),
                    you: i == 5,
                })
                .collect(),
            ranking_total: Some(79),
            utilisation: Some(0.88),
            tasks: vec![
                TaskLine { group: 0, label: "torid · standard_multi_target · cycle".into(), done: 330, total: 1000, secs: 12, lanes: 4, ..Default::default() },
                TaskLine { group: 1, riven: true, label: "soma_prime · demolisher [ab12cd]".into(), done: 41084, extra: "3,210 个构筑".into(), secs: 95, lanes: 3, ..Default::default() },
                TaskLine { group: 2, label: "ocucor · standard_multi_target · base".into(), done: 0, total: 1000, secs: 0, lanes: 0, ..Default::default() },
            ],
            groups: 3,
            max_groups: 8,
            held_secs: Some(34.0),
            group_views: (0..3).map(|i| GroupView { id: format!("a7i8qm{i:02}"), state: "计算中".into(), open: i < 3, tasks: 700 + i as u64, work: 7_800_000_000_000 }).collect(),
            lane_map: vec![Some(0), Some(0), Some(0), Some(0), Some(1), Some(1), Some(1), None, Some(2), None],
            lane_kind: vec![Some('P'), Some('P'), Some('P'), Some('P'), Some('P'), Some('P'), Some('E'), None, Some('E'), None],
            core_pref: Some("自动分配"),
            lane_yield: Some(0.52),
            effective_cores: Some(4.2),
            processors: 4,
            idle_cores: 10,
            active_cores: 2,
            ..Default::default()
        };
        let mut lines = VecDeque::new();
        for i in 0..40 {
            let group = if i % 5 == 4 { None } else { Some(i % 3) };
            lines.push_back(LogLine { group, text: format!("\x1b[2m10:17:{i:02}\x1b[0m \x1b[32m[完成]\x1b[0m 榜单  torid · heavy_gunner · base  = 0.6031 kpm · 1.44 分 · 4.4s") });
        }
        for (w, h, wide, groups) in [(150, 44, false, 3), (110, 30, true, 3), (110, 30, false, 1), (70, 24, false, 3)] {
            let mut s = s.clone();
            s.group_views.truncate(groups);
            s.tasks.retain(|t| t.group < groups);
            let f = Frame { s: &s, lines: &lines, ctl: &ctl, m: SysView { cpu: Some(0.63), me: Some(0.5), mem: Some((22 << 30, 63 << 30, 210 << 20)) }, up: Duration::from_secs(4567), multi: groups > 1 };
            let mut c = Canvas::new(w - 1, h, wide);
            draw(&mut c, &f, &mut View::default());
            println!("==== {w}x{h} wide={wide} groups={groups}");
            for y in 0..h {
                let t = text_of(&c, y);
                let n: usize = t.chars().map(|ch| c.cw(ch)).sum();
                assert!(n <= w - 1);
                println!("|{}|", t);
            }
        }
    }

    #[test]
    fn device_speeds_freeze_without_work_and_resume_their_own_clocks() {
        let mut s = Status::default();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        s.activity(at(0),true,true);
        s.record_computed(at(60),60_000_000_000,false);
        s.record_computed(at(60),30_000_000_000,true);
        assert_eq!(rates(&s,at(60)),(90.0,90.0,30.0));
        s.activity(at(60),true,false);
        s.record_computed(at(120),60_000_000_000,false);
        s.activity(at(120),false,false);
        assert_eq!(rates(&s,at(3600)),(75.0,75.0,30.0));
        assert_eq!(s.efficiency[1].rates(at(3600)).0,60.0);
        s.activity(at(3600),false,true);
        s.record_computed(at(3660),30_000_000_000,true);
        s.activity(at(3660),false,false);
        assert_eq!(rates(&s,at(7200)),(60.0,60.0,30.0));
        assert_eq!((s.computed_work,s.computed_gpu_work),(180_000_000_000,60_000_000_000));
        // Expire in active time, never by the intervening idle hour.
        s.activity(at(7200),false,true);
        s.activity(at(7801),false,false);
        assert_eq!(s.efficiency[2].rates(at(7801)).0,0.0);
        assert!(s.efficiency[2].recent.is_empty());
    }

    #[test]
    fn points_print_with_a_tenth() {
        assert_eq!(pts(23468.66), "23,468.6");
        assert_eq!(pts(0.05), "0.0");
    }
}
