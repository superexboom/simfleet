// SPDX-License-Identifier: AGPL-3.0-or-later
//! THE WATCH: a run of `simfleet` is two processes — this one, which only
//! waits, and the client it starts. A client that dies (a crash in an engine
//! library, a panic) is started again a few seconds later; one that ends
//! itself (Q, a closed window, Ctrl+C, an error it explained, "End task") is
//! not, and the watch ends with it.
//!
//! EACH WATCH HAS ITS OWN CLIENT: it waits on its own child's handle, and on
//! nothing else, so simfleets started by hand side by side — in one folder or
//! several — never start or stop each other's. The client is this program again,
//! with the same arguments and folder, marked by `SIMFLEET_WATCHED`; it writes
//! what killed it (where it can tell) to the watch's own file, and the watch
//! keeps a line of each in `simfleet-crash.log` beside the config.

use crate::config::Paths;
use crate::ui::{C_BAD, C_END, C_INFO};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Set in the client's environment: it is watched, and is not to watch.
const CLIENT: &str = "SIMFLEET_WATCHED";
/// Where the client writes what killed it.
const CRASH_FILE: &str = "SIMFLEET_CRASH_FILE";
/// How many times it was started again, and why the last one ended.
const RESTARTS: &str = "SIMFLEET_RESTARTS";
const LAST_EXIT: &str = "SIMFLEET_LAST_EXIT";
/// A completed maintenance drain, distinct from a crash or the user's exit.
pub const MAINTENANCE_EXIT: i32 = 75;
/// A client that dies this soon after it started, `GIVE_UP` times in a row,
/// is left dead: it would only die again.
const QUICK: Duration = Duration::from_secs(60);
const GIVE_UP: u32 = 5;
/// Seconds before a start again, by how many quick deaths came before.
const WAITS: [u64; 4] = [5, 15, 30, 60];

/// The client's process id while it runs, 0 between (signals, console events).
static CHILD: AtomicU32 = AtomicU32::new(0);
/// A user stop also cancels a maintenance restart racing with shutdown.
static CANCEL: AtomicBool = AtomicBool::new(false);

/// This process is a watched client.
pub fn is_client() -> bool {
    std::env::var_os(CLIENT).is_some()
}

/// Called after tasks, journals, configuration and the UI have shut down.
/// Without a watch, replace this process too; crash supervision remains off.
pub fn replace_process() -> anyhow::Result<()> {
    if is_client() {
        std::process::exit(MAINTENANCE_EXIT);
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cmd.exec().into())
    }
    #[cfg(not(unix))]
    {
        cmd.spawn()?;
        std::process::exit(0);
    }
}

/// The last start's reason, for the client's log: (times started again, why).
pub fn restarted() -> Option<(u32, String)> {
    let n = std::env::var(RESTARTS).ok()?.parse().ok()?;
    Some((n, std::env::var(LAST_EXIT).unwrap_or_default()))
}

/// RUN THE CLIENT UNDER WATCH until it ends itself; the code to exit with.
/// `None` when no client could be started at all (the caller runs unwatched).
pub fn watch(paths: &Paths) -> Option<i32> {
    let exe = std::env::current_exe().ok()?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // Beside the config, not in `cache/`, which a client empties of all but
    // the current release (work.rs `build_code`); this watch's own.
    let crash = Scratch(paths.dir.join(format!(".simfleet-crash-{}", std::process::id())));
    let crash_file = &crash.0;
    let log = paths.dir.join("simfleet-crash.log");
    let console = console::save();
    on_signals();
    let (mut restarts, mut quick, mut last) = (0u32, 0u32, String::new());
    loop {
        let _ = std::fs::remove_file(crash_file);
        let mut cmd = Command::new(&exe);
        cmd.args(&args).env(CLIENT, "1").env(CRASH_FILE, crash_file);
        if restarts > 0 {
            cmd.env(RESTARTS, restarts.to_string()).env(LAST_EXIT, &last);
        }
        let started = Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) if restarts == 0 => return None,
            Err(e) => {
                eprintln!("{C_BAD}simfleet 没能重新启动：{e}{C_END}");
                return Some(1);
            }
        };
        CHILD.store(child.id(), Ordering::SeqCst);
        let status = child.wait();
        CHILD.store(0, Ordering::SeqCst);
        let status = match status {
            Ok(s) => s,
            Err(_) => return Some(1),
        };
        if status.code() == Some(MAINTENANCE_EXIT) {
            console::restore(&console);
            if CANCEL.load(Ordering::SeqCst) {
                return Some(0);
            }
            // The old address space is gone before the new one is started.
            // Maintenance never consumes the crash retry allowance.
            quick = 0;
            restarts = 0;
            last.clear();
            eprintln!("{C_INFO}[维护] 旧进程已退出，已回收内存并重新读取配置，继续计算。{C_END}");
            continue;
        }
        let Some(why) = died(&status) else { return Some(status.code().unwrap_or(1)) };
        console::restore(&console);
        let what = std::fs::read_to_string(crash_file).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let _ = std::fs::remove_file(crash_file);
        let up = started.elapsed();
        note(&log, &why, what.as_deref(), up);
        quick = if up < QUICK { quick + 1 } else { 0 };
        let first = what.as_deref().and_then(|w| w.lines().next()).unwrap_or("");
        last = if first.is_empty() { why.clone() } else { format!("{why}，{first}") };
        if quick >= GIVE_UP {
            eprintln!("\n{C_BAD}simfleet 连续 {quick} 次在启动后一分钟内意外退出（{last}），不再自动重启。{C_END}");
            eprintln!("记录在 {}；可以把它发给开发者。", log.display());
            if args.is_empty() {
                pause();
            }
            return Some(1);
        }
        let wait = WAITS[(quick as usize).min(WAITS.len() - 1)];
        restarts += 1;
        eprintln!("\n{C_BAD}simfleet 意外退出：{last}{C_END}");
        eprintln!("{C_INFO}{wait} 秒后自动重启（第 {restarts} 次），按 Ctrl+C 取消。{C_END}记录在 {}", log.display());
        let until = Instant::now() + Duration::from_secs(wait);
        while Instant::now() < until {
            if CANCEL.load(Ordering::SeqCst) {
                eprintln!("已取消自动重启。");
                return Some(1);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// A file of the watch's, gone with it.
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Why the client died, or `None` when it ended itself.
fn died(s: &ExitStatus) -> Option<String> {
    #[cfg(windows)]
    {
        // 0 and 1: its own end (1 is an error it printed, or "End task");
        // 0xC000013A: the console's Ctrl+C or close.
        let c = s.code()? as u32;
        if matches!(c, 0 | 1 | 0xC000_013A) {
            return None;
        }
        let name = match c {
            101 => "程序内部错误（panic）",
            0xC000_0005 => "访问违规",
            0xC000_00FD => "栈溢出",
            0xC000_0409 => "程序中止（fast fail）",
            0xC000_0374 => "堆损坏",
            0xC000_001D => "非法指令",
            0xC000_0017 | 0xC000_009A => "内存不足",
            0x8000_0003 => "断点",
            _ => "",
        };
        Some(if name.is_empty() { format!("退出码 {c:#010X}") } else { format!("{c:#010X}（{name}）") })
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            let name = match sig {
                libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT => return None,
                libc::SIGSEGV => "SIGSEGV，段错误",
                libc::SIGBUS => "SIGBUS",
                libc::SIGILL => "SIGILL，非法指令",
                libc::SIGFPE => "SIGFPE",
                libc::SIGABRT => "SIGABRT，程序中止",
                libc::SIGKILL => "SIGKILL，多半是内存不足被系统结束",
                _ => "",
            };
            return Some(if name.is_empty() { format!("信号 {sig}") } else { format!("信号 {sig}（{name}）") });
        }
        match s.code()? {
            0 | 1 => None,
            101 => Some("程序内部错误（panic，退出码 101）".into()),
            c => Some(format!("退出码 {c}")),
        }
    }
}

/// One death, a line (and its stack, when the client could write it) in the log.
fn note(log: &Path, why: &str, what: Option<&str>, up: Duration) {
    if std::fs::metadata(log).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(log, log.with_extension("log.old"));
    }
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) else { return };
    let _ = writeln!(
        f,
        "{} simfleet {} (watch {}) 运行 {:.0} 秒后意外退出：{why}",
        crate::sys::iso(crate::sys::now_ms()),
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        up.as_secs_f64()
    );
    for l in what.into_iter().flat_map(str::lines) {
        let _ = writeln!(f, "    {l}");
    }
}

fn pause() {
    eprint!("按回车关闭…");
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
}

// ---- the watch's own console and signals -------------------------------------

/// CTRL+C AND THE CLOSE BUTTON reach the client as well, which answers them
/// (it gives its work back first): the watch stays to see how it ended.
#[cfg(windows)]
fn on_signals() {
    use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};
    unsafe extern "system" fn handler(kind: u32) -> i32 {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            CANCEL.store(true, Ordering::SeqCst);
            return 1;
        }
        // A close, a sign-out, a shutdown: Windows ends this process when the
        // handler returns, so wait while the client gives its work back.
        CANCEL.store(true, Ordering::SeqCst);
        for _ in 0..50 {
            if CHILD.load(Ordering::SeqCst) == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        0
    }
    // SAFETY: registers a handler that only touches atomics and sleeps.
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

/// Ctrl+C reaches the whole foreground group, the client too; `kill` and a
/// closed terminal are handed on to the client.
#[cfg(unix)]
fn on_signals() {
    extern "C" fn handler(sig: libc::c_int) {
        let child = CHILD.load(Ordering::SeqCst);
        if sig != libc::SIGINT && child != 0 {
            // SAFETY: kill is async-signal-safe.
            unsafe {
                libc::kill(child as libc::pid_t, sig);
            }
        }
        CANCEL.store(true, Ordering::SeqCst);
    }
    // SAFETY: the handler only uses atomics and kill.
    unsafe {
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(s, handler as *const () as libc::sighandler_t);
        }
    }
}
#[cfg(not(any(windows, unix)))]
fn on_signals() {}

/// The console as the watch had it, put back after a client died on the
/// dashboard's screen (raw keys, the alternate screen, no cursor).
mod console {
    use std::io::Write;

    #[cfg(windows)]
    pub struct Saved(u32, u32);
    #[cfg(windows)]
    pub fn save() -> Saved {
        use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        let (mut i, mut o) = (0, 0);
        // SAFETY: reads this process's console modes.
        unsafe {
            GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut i);
            GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut o);
        }
        Saved(i, o)
    }
    #[cfg(windows)]
    pub fn restore(s: &Saved) {
        use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        // SAFETY: sets this process's console modes back.
        unsafe {
            if s.0 != 0 {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), s.0);
            }
            if s.1 != 0 {
                SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), s.1);
            }
        }
        screen();
    }

    #[cfg(unix)]
    pub struct Saved(Option<libc::termios>);
    #[cfg(unix)]
    pub fn save() -> Saved {
        // SAFETY: reads the terminal's settings into a zeroed struct.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            Saved((libc::tcgetattr(0, &mut t) == 0).then_some(t))
        }
    }
    #[cfg(unix)]
    pub fn restore(s: &Saved) {
        if let Some(t) = &s.0 {
            // SAFETY: puts back settings read from the same terminal.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
        screen();
    }

    #[cfg(not(any(windows, unix)))]
    pub struct Saved;
    #[cfg(not(any(windows, unix)))]
    pub fn save() -> Saved {
        Saved
    }
    #[cfg(not(any(windows, unix)))]
    pub fn restore(_: &Saved) {
        screen();
    }

    /// The dashboard's screen undone (ui.rs `run`, on its way out).
    fn screen() {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b[0m\x1b[?25h\x1b[?7h\x1b[?1049l");
        let _ = out.flush();
    }
}

// ---- in the client: what killed it ---------------------------------------------

/// THE CLIENT'S LAST WORDS: which module the fault was in, and the stack's
/// modules and offsets, written to the watch's file before the process ends.
/// Nothing on the heap: the fault may be in the allocator itself.
#[cfg(windows)]
pub fn note_crashes() {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Diagnostics::Debug::{SetErrorMode, SetUnhandledExceptionFilter, EXCEPTION_POINTERS, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX};
    static FILE: OnceLock<std::fs::File> = OnceLock::new();
    let Some(path) = std::env::var_os(CRASH_FILE).map(PathBuf::from) else { return };
    let Ok(f) = std::fs::File::create(&path) else { return };
    let _ = FILE.set(f);

    /// A line's worth of text on the stack.
    struct Buf {
        b: [u8; 4096],
        n: usize,
    }
    impl std::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let take = s.len().min(self.b.len() - self.n);
            self.b[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
            self.n += take;
            Ok(())
        }
    }
    /// `name.dll+0x1234` for an address.
    fn place(out: &mut Buf, addr: usize) {
        use std::fmt::Write as _;
        use windows_sys::Win32::System::LibraryLoader::{
            GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };
        let mut module = std::ptr::null_mut();
        // SAFETY: looks up the module holding an address, without a reference.
        let found = unsafe {
            GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, addr as *const u16, &mut module)
        } != 0;
        if !found {
            let _ = write!(out, "{addr:#x}（不在任何模块里）");
            return;
        }
        let mut name = [0u16; 512];
        // SAFETY: fills a buffer of the size given.
        let n = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) } as usize;
        let name = &name[..n.min(name.len())];
        let file = name.iter().rposition(|&c| c == b'\\' as u16 || c == b'/' as u16).map_or(name, |i| &name[i + 1..]);
        for c in char::decode_utf16(file.iter().copied()) {
            let _ = out.write_char(c.unwrap_or('?'));
        }
        let _ = write!(out, "+{:#x}", addr - module as usize);
    }
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        use std::fmt::Write as _;
        use std::io::Write as _;
        use windows_sys::Win32::System::Diagnostics::Debug::{RtlCaptureStackBackTrace, EXCEPTION_CONTINUE_SEARCH};
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        let Some(file) = FILE.get() else { return EXCEPTION_CONTINUE_SEARCH };
        let mut out = Buf { b: [0; 4096], n: 0 };
        // SAFETY: the system's own record of the exception being dispatched.
        let rec = unsafe { (*info).ExceptionRecord.as_ref() };
        // The code is the exit code the watch reads; the place is what only
        // the client can tell.
        if let Some(r) = rec {
            let code = r.ExceptionCode as u32;
            let _ = write!(out, "位置 ");
            place(&mut out, r.ExceptionAddress as usize);
            if code == 0xC000_0005 && r.NumberParameters >= 2 {
                let how = match r.ExceptionInformation[0] {
                    0 => "读",
                    1 => "写",
                    8 => "执行",
                    _ => "访问",
                };
                let _ = write!(out, "（{how} {:#x}）", r.ExceptionInformation[1]);
            }
        }
        // SAFETY: reads this thread's id.
        let _ = write!(out, "，线程 {}\n调用栈：\n", unsafe { GetCurrentThreadId() });
        let mut frames = [std::ptr::null_mut(); 48];
        // SAFETY: fills the array given with this thread's return addresses
        // (through the exception dispatcher, to the faulting frame and its callers).
        let n = unsafe { RtlCaptureStackBackTrace(0, frames.len() as u32, frames.as_mut_ptr(), std::ptr::null_mut()) } as usize;
        for f in &frames[..n] {
            let _ = write!(out, "  ");
            place(&mut out, *f as usize);
            let _ = writeln!(out);
        }
        let _ = (&*file).write_all(&out.b[..out.n]);
        EXCEPTION_CONTINUE_SEARCH
    }
    // SAFETY: no error box to wait on (the watch is waiting); the filter
    // writes and lets the default handling end the process.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
        SetUnhandledExceptionFilter(Some(filter));
    }
}
/// Elsewhere the watch names the signal that ended the client.
#[cfg(not(windows))]
pub fn note_crashes() {}
