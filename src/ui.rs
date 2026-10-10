// SPDX-License-Identifier: AGPL-3.0-or-later
//! THE CONSOLE: tasks scroll past above, and the bottom three lines stay put —
//! what this machine is doing, what it has given, and the keys.
//!
//! The scrolling part is a terminal scroll region (DECSTBM), so the log is
//! ordinary text the terminal keeps in its scrollback; the status lines are
//! repainted in place four times a second. Without a terminal (output piped to
//! a file) it is a plain log with a status line every minute.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// What the keys change, read by the work loop.
pub struct Ctl {
    pub paused: AtomicBool,
    pub quit: AtomicBool,
    pub idle_cores: AtomicUsize,
    pub active_cores: AtomicUsize,
    pub max_cores: usize,
}

#[derive(Default, Clone)]
pub struct Status {
    /// "计算中", "等待任务", "已暂停：…"
    pub state: String,
    pub lanes_now: usize,
    pub idle: bool,
    pub engine: String,
    pub backend: String,
    pub session_tasks: u64,
    pub session_work: u64,
    pub confirmed: u64,
    pub waiting: u64,
    pub device_points: Option<u64>,
    pub today_points: Option<u64>,
    pub account: Option<String>,
    pub account_points: Option<u64>,
    pub contributor_rank: Option<u64>,
    pub computing_now: Option<u64>,
    pub task: String,
    pub done: u64,
    pub total: u64,
    /// For a task with no known length: what it has done so far.
    pub extra: String,
    pub note: String,
}

struct Inner {
    lines: Vec<String>,
    status: Status,
}

pub struct Ui {
    inner: Mutex<Inner>,
    tty: bool,
}

pub const C_DIM: &str = "\x1b[2m";
pub const C_OK: &str = "\x1b[32m";
pub const C_GO: &str = "\x1b[33m";
pub const C_BAD: &str = "\x1b[31m";
pub const C_INFO: &str = "\x1b[36m";
pub const C_END: &str = "\x1b[0m";

const STATUS_ROWS: u16 = 3;

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

/// Cut to `w` columns, counting a CJK character as two; escape codes count none.
fn fit(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            out.push(c);
            if c.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            esc = true;
            out.push(c);
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if used + cw > w {
            break;
        }
        used += cw;
        out.push(c);
    }
    out.push_str(C_END);
    out
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

impl Ui {
    pub fn new() -> Arc<Ui> {
        Arc::new(Ui { inner: Mutex::new(Inner { lines: Vec::new(), status: Status::default() }), tty: std::io::stdout().is_terminal() })
    }
    pub fn log(&self, line: impl Into<String>) {
        let line = format!("{}{} {}{}", C_DIM, crate::sys::clock(), C_END, line.into());
        if self.tty {
            self.inner.lock().unwrap().lines.push(line);
        } else {
            println!("{}", strip(&line));
        }
    }
    pub fn status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.inner.lock().unwrap().status);
    }
    /// Lines logged after the painter stopped, printed plainly.
    pub fn flush_plain(&self) {
        for l in std::mem::take(&mut self.inner.lock().unwrap().lines) {
            println!("{}", strip(&l));
        }
    }
    pub fn snapshot(&self) -> Status {
        self.inner.lock().unwrap().status.clone()
    }

    fn status_lines(s: &Status, ctl: &Ctl) -> [String; 2] {
        // MOST IMPORTANT FIRST: a narrow window cuts from the right.
        let mut a = format!(
            " {}●{} {} · {}/{} 核{} · 本次 {} 任务 {:.1} 分",
            if s.state.starts_with("计算") { C_OK } else { C_GO },
            C_END,
            s.state,
            s.lanes_now,
            ctl.max_cores,
            if s.idle { "(空闲)" } else { "(使用中)" },
            s.session_tasks,
            points(s.session_work)
        );
        if s.confirmed + s.waiting > 0 {
            a += &format!("(已确认 {}/待确认 {})", s.confirmed, s.waiting);
        }
        if let Some(t) = s.today_points {
            a += &format!(" · 今日 {} 分", group(t));
        }
        match (&s.account, s.account_points) {
            (Some(name), Some(p)) => {
                a += &format!(" · {} 累计 {} 分", name, group(p));
                if let Some(r) = s.contributor_rank {
                    a += &format!(" 段位 {r}");
                }
            }
            _ => {
                if let Some(p) = s.device_points {
                    a += &format!(" · 本机累计 {} 分(未登录)", group(p));
                }
            }
        }
        if let Some(c) = s.computing_now {
            a += &format!(" · 全站在线 {c}");
        }
        a += &format!(" · {} {}", s.backend, s.engine.get(..8).unwrap_or(&s.engine));
        let mut b = String::from(" ");
        if !s.task.is_empty() && s.total == 0 {
            b += &format!("{}  已打 {} 场 · 试了 {}   ", s.task, group(s.done), if s.extra.is_empty() { "0 个构筑" } else { &s.extra });
        } else if !s.task.is_empty() {
            let bar = 20usize;
            let frac = if s.total > 0 { (s.done as f64 / s.total as f64).clamp(0.0, 1.0) } else { 0.0 };
            let fill = (frac * bar as f64).round() as usize;
            b += &format!("{}  [{}{}] {:>3.0}%", s.task, "#".repeat(fill), "-".repeat(bar - fill), frac * 100.0);
            if s.total > 0 {
                b += &format!(" ({}/{})", s.done, s.total);
            }
            b += "   ";
        }
        if !s.note.is_empty() {
            b += &format!("{}{}{}   ", C_INFO, s.note, C_END);
        }
        b += &format!("{}[P]暂停/继续 [+/-]空闲核心 [Q]退出{}", C_DIM, C_END);
        [a, b]
    }

    /// The render-and-keys thread. Returns when `ctl.quit` is set and the screen is restored.
    pub fn run(self: Arc<Ui>, ctl: Arc<Ctl>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            if !self.tty {
                // Piped: a line on stdin is a key — `p` pauses, `q` quits.
                let (c2, u2) = (ctl.clone(), self.clone());
                std::thread::spawn(move || {
                    let mut line = String::new();
                    while std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                        match line.trim() {
                            "q" | "Q" => c2.quit.store(true, Ordering::Relaxed),
                            "p" | "P" => {
                                let p = !c2.paused.load(Ordering::Relaxed);
                                c2.paused.store(p, Ordering::Relaxed);
                                u2.log(if p { "[暂停]" } else { "[继续]" });
                            }
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
                        let [a, b] = Self::status_lines(&s, &ctl);
                        println!("{}", strip(&format!("{} |{}", a, b)));
                    }
                }
                return;
            }
            let _ = crossterm::terminal::enable_raw_mode();
            let mut out = std::io::stdout();
            let mut size = (0u16, 0u16);
            let mut log_row = 1u16;
            loop {
                let (w, h) = crossterm::terminal::size().unwrap_or((100, 30));
                if (w, h) != size {
                    size = (w, h);
                    let region = h.saturating_sub(STATUS_ROWS).max(2);
                    log_row = region;
                    let _ = write!(out, "\x1b[r\x1b[{h};1H\r\n\x1b[1;{region}r\x1b[{region};1H");
                }
                let (lines, status) = {
                    let mut g = self.inner.lock().unwrap();
                    (std::mem::take(&mut g.lines), g.status.clone())
                };
                let mut buf = String::new();
                for l in lines {
                    // A long line wraps inside the scroll region; the status rows are below it.
                    buf += &format!("\x1b[{log_row};1H\r\n{l}{C_END}");
                }
                let [a, b] = Self::status_lines(&status, &ctl);
                let cols = w.saturating_sub(1) as usize;
                let sep = "─".repeat(cols / "─".width().max(1));
                buf += &format!("\x1b7\x1b[{};1H\x1b[2K{}{}{}", h - 2, C_DIM, sep, C_END);
                buf += &format!("\x1b[{};1H\x1b[2K{}", h - 1, fit(&a, cols));
                buf += &format!("\x1b[{};1H\x1b[2K{}\x1b8", h, fit(&b, cols));
                let _ = out.write_all(buf.as_bytes());
                let _ = out.flush();
                if ctl.quit.load(Ordering::Relaxed) {
                    break;
                }
                while event::poll(Duration::from_millis(0)).unwrap_or(false) {
                    if let Ok(Event::Key(k)) = event::read() {
                        if k.kind != KeyEventKind::Press {
                            continue;
                        }
                        match k.code {
                            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => ctl.quit.store(true, Ordering::Relaxed),
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => ctl.quit.store(true, Ordering::Relaxed),
                            KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Char(' ') => {
                                let p = !ctl.paused.load(Ordering::Relaxed);
                                ctl.paused.store(p, Ordering::Relaxed);
                                self.log(if p { format!("{C_GO}[暂停]{C_END} 手头的任务交还给服务器，按 P 继续") } else { format!("{C_OK}[继续]{C_END}") });
                            }
                            KeyCode::Char('+') | KeyCode::Char('=') => {
                                let n = (ctl.idle_cores.load(Ordering::Relaxed) + 1).min(ctl.max_cores);
                                ctl.idle_cores.store(n, Ordering::Relaxed);
                                self.log(format!("空闲时使用 {n} 个核心"));
                            }
                            KeyCode::Char('-') | KeyCode::Char('_') => {
                                let n = ctl.idle_cores.load(Ordering::Relaxed).saturating_sub(1).max(1);
                                ctl.idle_cores.store(n, Ordering::Relaxed);
                                self.log(format!("空闲时使用 {n} 个核心"));
                            }
                            _ => {}
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            let _ = write!(out, "\x1b[r\x1b[{};1H\r\n", size.1);
            let _ = out.flush();
            let _ = crossterm::terminal::disable_raw_mode();
        })
    }
}
