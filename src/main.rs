// SPDX-License-Identifier: AGPL-3.0-or-later
//! simfleet — a small volunteer compute client for WFSim (https://wfsim.app).
//!
//! The browser fleet's work (board orders, riven gains) done by a console
//! program instead of an open tab: the same engine the site serves, the same
//! protocol, a budget you set, and a log you can read. Unofficial; based on
//! WFSim, whose engine it downloads and runs.
//!
//! Copyright (C) 2026 superexboom. Parts ported from WFSim's web client are
//! Copyright (C) magenie33 and WFSim contributors. AGPL-3.0-or-later; see
//! NOTICE.md and LICENSE.

/// Each thread its own heap (Cargo.toml).
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod account;
mod affinity;
mod config;
mod engine;
mod fleet;
mod jsnum;
mod net;
mod sys;
mod ui;
mod work;

use anyhow::{bail, Context, Result};
use config::{Config, Paths};
use engine::{Backend, Exec, Pool};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use ui::{Ctl, Ui, C_END, C_INFO, C_OK};

const HELP: &str = "\
simfleet — WFSim 志愿计算客户端（非官方，基于 WFSim 引擎）

用法:
  simfleet                 开始计算（第一次运行会先问几个问题）
  simfleet --cores N       本次空闲时使用 N 个核心
  simfleet setup           重新设置预算
  simfleet login           用邮箱和密码登录 wfsim.app，贡献记到账号名下
  simfleet login --cookie <wfsim_session>
                           用 Google/Discord/GitHub 登录的账号：从浏览器复制会话 cookie
  simfleet logout          退出登录（之后的贡献记在本机）
  simfleet status          看一眼本机和账号的贡献
  simfleet bench           本机测速（不连任务服务器派活）
  simfleet cpu             看 CPU 识别结果：是否 Intel 大小核、计算线程会放在哪种核心上
  simfleet engine-id <dir> 计算一份 WFSim 源码的引擎号（原生引擎构建脚本用）

运行时按键: P 暂停/继续 · + / - 空闲时的核心数 · Shift 加 + / - 使用电脑时的核心数（0 = 使用时暂停）
          [ / ] 并发上限 · L 全屏日志 · PgUp/PgDn 翻页 · Q 退出（会先把手头任务交还）

源码与许可: https://github.com/superexboom/simfleet （AGPL-3.0-or-later）
基于 WFSim（https://github.com/magenie33/wfsim，AGPL-3.0-or-later），非官方项目。
";

#[cfg(windows)]
fn enable_vt() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    unsafe {
        SetConsoleOutputCP(65001);
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
#[cfg(not(windows))]
fn enable_vt() {}

/// The running fleet's controls, for the console's own Ctrl+C / close button.
static CTL: std::sync::OnceLock<Arc<Ctl>> = std::sync::OnceLock::new();
/// Set once the leases are handed back, so a closing window waits for it.
static SHUT: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
fn on_console_close() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn handler(_kind: u32) -> i32 {
        if let Some(c) = CTL.get() {
            c.quit.store(true, Ordering::Relaxed);
            // Windows ends the process a few seconds after a close; give the
            // leases back first.
            for _ in 0..40 {
                if SHUT.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        1
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}
/// Unix: Ctrl+C outside the dashboard, `kill`, a closed terminal — each asks
/// the loop to stop, which gives the leases back first.
#[cfg(unix)]
fn on_console_close() {
    extern "C" fn handler(_sig: libc::c_int) {
        // An atomic store: safe in a signal handler.
        if let Some(c) = CTL.get() {
            c.quit.store(true, Ordering::Relaxed);
        }
    }
    // SAFETY: the handler only stores an atomic.
    unsafe {
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(s, handler as *const () as libc::sighandler_t);
        }
    }
}
#[cfg(not(any(windows, unix)))]
fn on_console_close() {}

fn ask(q: &str) -> String {
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

fn yes(q: &str, default: bool) -> bool {
    loop {
        let a = ask(q).to_lowercase();
        match a.as_str() {
            "" => return default,
            "y" | "yes" | "是" | "好" => return true,
            "n" | "no" | "否" | "不" => return false,
            _ => println!("  请输入 y 或 n"),
        }
    }
}

fn default_idle(cores: usize) -> usize {
    (cores * 30).div_ceil(100).max(1)
}

/// FIRST RUN: the statement, the budget, and an optional sign-in. Nothing is
/// computed without the yes, and the yes is what every ask for work carries.
/// THE STATEMENT, number `config::CONSENT_V`: WFSim's own (73-compute.js
/// `computeAskHtml`, statement 3) said of this program instead of a page — what
/// it computes, how a result counts, what it takes and costs the machine, and
/// that it is voluntary and unpaid.
fn statement(idle_cores: usize, cores: usize) -> String {
    format!(
        "开启后，这台电脑会和其他志愿者一起，为所有玩家运行 WFSim 的模拟和优化任务（排行榜分数、紫卡增益）。\n\
         只有两位志愿者分别算出完全相同的答案，结果才算数，你的电脑就是其中之一。\n\n\
         只在 simfleet 运行时计算。离开电脑时用的核心数默认约 30%，现在设为本机 {cores} 个里的 {idle_cores} 个；\n\
         你在用电脑时留 1 个核心，进程以最低优先级运行；这些都可以在下一步或 simfleet.toml 里改。\n\
         用电池时暂停（可在 simfleet.toml 里改）。运行时电脑耗电更多、温度更高；按 P 暂停，按 Q 退出。\n\n\
         这是自愿贡献：算出的一切对所有玩家免费，WFSim 不靠它赚钱，你也没有报酬。\n\
         贡献记在本机；登录后可以记到你的账号名下，是否在贡献者排行榜上显示名字由你决定。\n"
    )
}

/// `full`: the budget questions too. A yes given to an older statement is
/// asked again alone, keeping the budget and the sign-in.
fn wizard(paths: &Paths, old: Option<Config>, full: bool) -> Result<Option<Config>> {
    let cores = sys::cores();
    let renewing = old.as_ref().is_some_and(|c| !c.consent_at.is_empty() && !c.consented());
    let mut c = old.unwrap_or_else(|| Config::fresh(default_idle(cores)));
    println!("\n{C_OK}simfleet{C_END} — WFSim 志愿计算客户端（非官方，基于 WFSim 引擎）\n");
    if renewing {
        println!("{C_INFO}同意书有更新（第 {} 版），请再确认一次；你的预算和登录都不变。{C_END}\n", config::CONSENT_V);
    }
    print!("{}", statement(c.idle_cores, cores));
    if !yes("\n加入计算吗？[Y/n] ", true) {
        println!("好的，没有开启。随时运行 simfleet 再来。");
        return Ok(None);
    }
    c.consent_at = sys::iso(sys::now_ms());
    c.consent_v = config::CONSENT_V;
    if renewing && !full {
        paths.save(&c)?;
        return Ok(Some(c));
    }
    let a = ask(&format!(
        "\n离开电脑时用几个核心？本机 {cores} 个逻辑核心，回车 = {}（30%）。可填数字或百分比（如 8 或 50%）：",
        c.idle_cores
    ));
    if !a.is_empty() {
        let n = match a.strip_suffix('%') {
            Some(p) => p.trim().parse::<f64>().map(|p| ((cores as f64 * p / 100.0).ceil()) as usize).ok(),
            None => a.parse::<usize>().ok(),
        };
        c.idle_cores = n.unwrap_or(c.idle_cores).clamp(1, cores);
    }
    let a = ask("使用电脑时：回车 = 留 1 个核心慢慢算，0 = 暂停：");
    if a == "0" {
        c.active_cores = 0;
    } else if let Ok(n) = a.parse::<usize>() {
        c.active_cores = n.clamp(1, cores);
    }
    let topo = affinity::Topology::read();
    if topo.hybrid().is_ok() {
        let a = ask(&format!(
            "\n检测到 Intel 大小核（P 核 {} 个、{} 线程，E 核 {} 个）。计算线程怎么放？\n  回车 = 自动（先 P 核，再 E 核，最后 P 核超线程，最快），p = 优先 P 核，e = 优先 E 核（更省电、更不打扰），windows = 交给系统：",
            topo.cores(true),
            topo.count(true),
            topo.cores(false)
        ));
        c.core_type = affinity::Prefer::parse(&a).key().into();
    }
    println!("\n预算：离开时 {} 核，使用时 {}。可随时编辑 {}", c.idle_cores,
        if c.active_cores == 0 { "暂停".to_string() } else { format!("{} 核", c.active_cores) }, paths.config.display());
    if c.account.session.is_empty() && yes("\n现在登录 wfsim.app 账号，把贡献记到你的名下吗？（之后也能用 simfleet login）[y/N] ", false) {
        let _ = login_email_interactive(&mut c);
    }
    paths.save(&c)?;
    Ok(Some(c))
}

fn login_email_interactive(c: &mut Config) -> Result<()> {
    println!("用邮箱和密码登录（没有账号？在 https://wfsim.app/signup 注册，或用第三方登录后见 simfleet help）");
    let email = ask("邮箱：");
    let password = rpassword::prompt_password("密码（不显示）：").unwrap_or_default();
    let net = net::Net::new(&c.server, None);
    match account::login_email(&net, &email, &password) {
        Ok(session) => adopt_session(c, session),
        Err(e) => {
            println!("登录失败：{e:#}");
            Err(e)
        }
    }
}

/// Keep a session, name it, and claim this computer for it.
fn adopt_session(c: &mut Config, session: String) -> Result<()> {
    let net = net::Net::new(&c.server, Some(session.clone()));
    let Some(acct) = account::whoami(&net)? else { bail!("这个会话无效或已过期") };
    account::claim(&net, &c.verifier)?;
    c.account.session = session;
    c.account.name = account::display_name(&acct);
    println!("{C_OK}已登录 {}{C_END}，本机的贡献（包括登录前的）会记到这个账号。", c.account.name);
    Ok(())
}

fn run(paths: Paths, mut cfg: Config, cores_override: Option<usize>) -> Result<()> {
    sys::set_priority(&cfg.priority);
    let max = sys::cores();
    let ctl = Arc::new(Ctl {
        paused: AtomicBool::new(false),
        quit: AtomicBool::new(false),
        idle_cores: AtomicUsize::new(cores_override.unwrap_or(cfg.idle_cores).clamp(1, max)),
        active_cores: AtomicUsize::new(cfg.active_cores.min(max)),
        max_cores: max,
        max_slots: AtomicUsize::new(cfg.max_slots),
    });
    let _ = CTL.set(ctl.clone());
    on_console_close();
    let ui = Ui::new();
    let net = net::Net::new(&cfg.server, Some(cfg.account.session.clone()));
    ui.status(|s| {
        s.state = "启动中".into();
        s.account = (!cfg.account.name.is_empty()).then(|| cfg.account.name.clone());
    });
    ui.log(format!(
        "{C_OK}simfleet {}{C_END} (AGPL-3.0, 基于 WFSim; 源码 github.com/superexboom/simfleet) · {} · 本机 id {}… · 配置 {}",
        env!("CARGO_PKG_VERSION"),
        cfg.server,
        &cfg.verifier[..6],
        paths.config.display()
    ));
    if let Some(l) = affinity::init(affinity::Prefer::parse(&cfg.core_type)) {
        ui.log(format!("{C_INFO}[CPU]{C_END} {l}"));
    }
    let painter = ui.clone().run(ctl.clone());
    let started = Instant::now();
    let result = (|| -> Result<fleet::Fleet> {
        let rel = loop {
            match work::fetch_release(&net) {
                Ok(r) => break r,
                Err(e) => {
                    ui.status(|s| s.state = format!("连不上 {}：{e}", cfg.server));
                    for _ in 0..40 {
                        if ctl.quit.load(Ordering::Relaxed) {
                            bail!("quit");
                        }
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                }
            }
        };
        ui.log(format!("{C_INFO}[引擎]{C_END} 网站引擎 {} · wasm {}", rel.engine, rel.wasm));
        let code = work::load_code(&net, &paths, &cfg, &ui, &rel)?;
        Ok(fleet::Fleet::new(net.clone(), cfg.clone(), paths.clone(), ctl.clone(), ui.clone(), rel, code))
    })();
    let mut summary = None;
    match result {
        Ok(mut fleet) => {
            fleet.run();
            if let Some(u) = fleet.utilisation_summary() {
                ui.log(u);
            }
            if fleet.session_dropped {
                cfg.account = Default::default();
            }
            // The ids groups were opened with, so the next run keeps them.
            cfg.extra_verifiers = fleet.cfg.extra_verifiers.clone();
            summary = Some(ui.snapshot());
        }
        Err(e) if e.to_string() == "quit" => {}
        Err(e) => ui.log(format!("{}[出错]{C_END} {e:#}", ui::C_BAD)),
    }
    SHUT.store(true, Ordering::Relaxed);
    ctl.quit.store(true, Ordering::Relaxed);
    let _ = painter.join();
    ui.flush_plain();
    cfg.idle_cores = ctl.idle_cores.load(Ordering::Relaxed);
    cfg.active_cores = ctl.active_cores.load(Ordering::Relaxed);
    cfg.max_slots = ctl.max_slots.load(Ordering::Relaxed);
    paths.save(&cfg)?;
    if let Some(s) = summary {
        println!(
            "本次运行 {:.0} 分钟，提交 {} 个任务，约 {:.2} 分（另一台电脑确认后计入）。再见！",
            started.elapsed().as_secs_f64() / 60.0,
            s.session_tasks,
            ui::points(s.session_work)
        );
        if let Some(u) = s.utilisation_total {
            println!("本次核心利用率 {:.0}%（引擎实际计算时间 ÷ 预算允许的核心时间）。", u * 100.0);
        }
    }
    Ok(())
}

fn status(cfg: &Config) -> Result<()> {
    let net = net::Net::new(&cfg.server, Some(cfg.account.session.clone()));
    for (i, v) in std::iter::once(&cfg.verifier).chain(&cfg.extra_verifiers).enumerate() {
        let p = net.post("/api/board/points", &serde_json::json!({ "verifier": v, "since": sys::local_midnight_utc_hour() }))?;
        println!("第 {} 路 · 本机 id {}…", i + 1, &v[..6]);
        println!("  未归属账号的点数 {} · 近 30 天 {} · 近 7 天 {} · 已绑定账号: {}", p.body["points"], p.body["recent"], p.body["week"], p.body["claimed"]);
        if let Some(t) = p.body.get("today") {
            println!("  今天 {} 个任务 · {} 分", t["tasks"], t["points"]);
        }
    }
    if net.session.is_some() {
        match account::devices(&net) {
            Ok(d) => println!(
                "账号 {}：累计 {} 分 · 近 30 天 {} · 近 7 天 {} · 贡献者段位 {} · {} 台设备",
                cfg.account.name, d["points"], d["recent"], d["week"], d["contributor_rank"]["rank"],
                d["devices"].as_array().map_or(0, |a| a.len())
            ),
            Err(e) if format!("{e}").contains("HTTP 401") => println!("账号会话无效或已过期，运行 simfleet login 重新登录"),
            Err(e) => println!("读取账号信息失败：{e:#}"),
        }
    }
    Ok(())
}

/// THE ENGINES, timed on one real board row, without asking the server for work.
fn bench(paths: &Paths, cfg: &Config, lanes_arg: Option<usize>) -> Result<()> {
    let net = net::Net::new(&cfg.server, None);
    let ui = Ui::new();
    let rel = work::fetch_release(&net)?;
    println!("网站引擎 {} · wasm {}", rel.engine, rel.wasm);
    let code = work::load_code(&net, paths, cfg, &Ui::new(), &rel)?;
    drop(ui);
    let o = work::sample_order();
    let mut backends = vec![Backend::Wasm];
    if code.native.is_some() {
        backends.push(Backend::Native);
    }
    let all = lanes_arg.unwrap_or_else(sys::cores);
    for b in backends {
        for lanes in [1, all] {
            let mut pool = Pool::new(code.clone());
            let mut exec = Exec::new(code.clone());
            let order = exec.api(b, "/api/board/order", o.to_string())?;
            let order = jsnum::parse(&order)?;
            let req = order["request"].clone();
            pool.ensure(lanes);
            let t = Instant::now();
            let s = work::measure_row(&mut pool, &mut exec, b, &req, "demolisher", &|_| lanes, &|| true, &mut |_, _| {})?
                .context("row did not finish")?;
            let secs = t.elapsed().as_secs_f64();
            println!(
                "  {:?} × {lanes:>2} 核: {:.2}s ({} 次模拟, {:.1} 次/秒)  分数 {} {}  work {}",
                b,
                secs,
                req["runs"],
                req["runs"].as_f64().unwrap_or(0.0) / secs,
                s.score,
                s.metric,
                s.work
            );
        }
    }
    Ok(())
}

/// THE GATE A NATIVE ENGINE PASSES BEFORE IT IS PUBLISHED: real board rows
/// (one per ruler and mode, from a few weapons' published boards) measured on
/// the wasm and on the native build, which must agree on every row to the bit.
///
/// Offline (the release workflow, which cannot reach wfsim.app): `--engine <id>
/// --wasm <file> --boards <dir>` take the engine id, the reference wasm built
/// from the same commit, and a WFSim checkout's `site/board/`.
fn check_native(paths: &Paths, engine: Option<String>, wasm: Option<String>, boards: Option<String>, rows: Option<String>, lanes: Option<usize>, weapons: usize) -> Result<()> {
    let mut cfg = Config::fresh(sys::cores());
    cfg.native_engine = true;
    cfg.native_engine_url = String::new();
    // The build under test, never WFSim's own library in its place.
    cfg.official_engine = false;
    let net = net::Net::new(&cfg.server, None);
    let ui = Ui::new();
    let code = match (engine, wasm) {
        (Some(engine), Some(file)) => {
            use sha2::{Digest, Sha256};
            let bytes = std::fs::read(&file).with_context(|| file.clone())?;
            let digest: String = Sha256::digest(&bytes).iter().take(6).map(|x| format!("{x:02x}")).collect();
            let rel = work::Release { engine, wasm: digest };
            println!("引擎 {} · 参照 wasm {} ({})", rel.engine, rel.wasm, file);
            work::build_code(&net, paths, &cfg, &ui, &rel, &bytes)?
        }
        (None, None) => {
            let rel = work::fetch_release(&net)?;
            work::load_code(&net, paths, &cfg, &ui, &rel)?
        }
        _ => bail!("--engine 和 --wasm 要一起给"),
    };
    let rel_engine = code.native.as_ref().map(|n| n.engine_id.clone()).unwrap_or_default();
    if code.native.is_none() {
        bail!("{} 下没有可用的原生引擎（或金丝雀不一致）", paths.engines.display());
    }
    let lanes = lanes.unwrap_or_else(sys::cores);
    let (mut same, mut skipped) = (0, 0);
    // THE ROWS: the board's (one per ruler and mode of the first `weapons` of
    // four), then the contract's vectors (docs/CLIENTS.md §"Conformance" 7,
    // tests/gas/rows.json).
    let mut orders: Vec<(String, String, String)> = Vec::new();
    for weapon in ["braton_prime", "dual_toxocyst", "torid", "burston_prime"].into_iter().take(weapons.clamp(1, 4)) {
        let rows = match &boards {
            Some(dir) => jsnum::parse(&std::fs::read_to_string(std::path::Path::new(dir).join(format!("{weapon}.json")))?)?,
            None => net.get(&format!("/board/{weapon}.json"))?.body,
        };
        let mut seen = std::collections::HashSet::new();
        for row in rows.as_array().into_iter().flatten() {
            let (ruler, mode) = (row["benchmark"].as_str().unwrap_or(""), row["mode"].as_str().unwrap_or(""));
            if !seen.insert(format!("{ruler}/{mode}")) {
                continue;
            }
            let mut rec = serde_json::json!({ "weapon": weapon, "mods": row["mods"], "evolutions": row["evolutions"], "arcanes": row["arcanes"] });
            for k in ["valence", "exilus"] {
                if row[k].as_str().is_some_and(|v| !v.is_empty()) {
                    rec[k] = row[k].clone();
                }
            }
            if let Some(r) = row["riven"].as_object() {
                let mut pos: Vec<String> = r["bonuses"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
                pos.sort();
                rec["riven_pos"] = serde_json::json!(pos);
                if r["malus"].as_str().is_some_and(|m| !m.is_empty()) {
                    rec["riven_neg"] = r["malus"].clone();
                }
                rec["riven_rolls"] = r["rolls"].clone();
            }
            let order = serde_json::json!({ "record": rec, "ruler": ruler, "mode": mode }).to_string();
            orders.push((format!("{weapon} · {ruler} · {mode}"), order, ruler.to_string()));
        }
    }
    if let Some(file) = &rows {
        let list = jsnum::parse(&std::fs::read_to_string(file).with_context(|| file.clone())?)?;
        for r in list.as_array().into_iter().flatten() {
            let (ruler, mode) = (r["ruler"].as_str().unwrap_or(""), r["mode"].as_str().unwrap_or(""));
            let label = format!("{} · {ruler} · {mode}（gas 向量）", r["record"]["weapon"].as_str().unwrap_or("?"));
            let order = serde_json::json!({ "record": r["record"], "ruler": ruler, "mode": mode }).to_string();
            orders.push((label, order, ruler.to_string()));
        }
    }
    for (label, order, ruler) in &orders {
        let mut answers = Vec::new();
        for b in [Backend::Wasm, Backend::Native] {
            let mut exec = Exec::new(code.clone());
            let mut pool = Pool::new(code.clone());
            let o = jsnum::parse(&exec.api(b, "/api/board/order", order.clone())?)?;
            if o["ok"] != serde_json::json!(true) {
                answers.push(None);
                continue;
            }
            let s = work::measure_row(&mut pool, &mut exec, b, &o["request"], ruler, &|_| lanes, &|| true, &mut |_, _| {})?;
            // THE ANSWER'S BITS: score, work, metric, and the gas the work was priced from.
            answers.push(s.map(|s| (s.score.to_bits(), s.work, s.metric, s.counts["gas"].as_u64())));
        }
        match (&answers[0], &answers[1]) {
            (None, None) => skipped += 1,
            (a, b) if a == b => {
                same += 1;
                println!("  一致  {label}");
            }
            (a, b) => bail!("不一致：{label}: wasm {a:?} / native {b:?}"),
        }
    }
    if same < 6 {
        bail!("只比对了 {same} 行（跳过 {skipped}），不足以放行");
    }
    println!("原生引擎 {rel_engine} 与 wasm 在 {same} 行上逐位一致（分数、work、gas；跳过 {skipped} 行）");
    // THE OPTIMIZER, where the build has one: reported, not refused — simfleet
    // holds it to the site's wasm again before any riven gain runs on it.
    let riven = match code.native.as_ref().filter(|n| n.has_optimize()).map(|n| n.wait_riven()) {
        Some(2) => {
            println!("紫卡搜索：原生与 wasm 逐位一致");
            "same"
        }
        Some(3) => {
            let why = code.native.as_ref().map(|n| n.riven_why()).unwrap_or_default();
            println!("警告：紫卡搜索与 wasm 不一致（{why}）；运行时同样会校验，这个版本只用于榜单");
            "differs"
        }
        _ => {
            println!("紫卡搜索：没有校验");
            "unchecked"
        }
    };
    // …AND ONE LINE FOR SCRIPTS (tools/publish-native-engine.ps1), in ASCII.
    println!("CHECK-NATIVE rows={same} skipped={skipped} riven={riven}");
    Ok(())
}

/// WFSim scripts/build_site_app.py `engine_id`, for a source checkout.
fn engine_id(dir: &std::path::Path) -> Result<Vec<String>> {
    use sha2::{Digest, Sha256};
    // THE CHECKOUT'S OWN LISTS (`ENGINE_SOURCES`, `ENGINE_SOURCES_NOT`), so a
    // commit that changes what the id covers is hashed as that commit does;
    // these (as of WFSim 2fb8138) when its script cannot be read.
    let script = std::fs::read_to_string(dir.join("scripts").join("build_site_app.py")).unwrap_or_default();
    let list = |name: &str, default: &[&str]| -> Vec<String> {
        let found = script.lines().position(|l| l.starts_with(name) && l[name.len()..].trim_start().starts_with('=')).and_then(|i| {
            let tail: String = script.lines().skip(i).collect::<Vec<_>>().join("
");
            let open = tail.find('(')?;
            let close = open + tail[open..].find(')')?;
            Some(tail[open + 1..close].split('"').skip(1).step_by(2).map(String::from).collect::<Vec<_>>())
        });
        found.filter(|v| !v.is_empty()).unwrap_or_else(|| default.iter().map(|s| s.to_string()).collect())
    };
    let sources = list("ENGINE_SOURCES", &["engine/src", "webapi/src", "wasm/src", "tools/gas-macro", "data"]);
    let not = list(
        "ENGINE_SOURCES_NOT",
        &["data/i18n/", "data/board_state.yaml", "data/market.yaml", "data/assets.yaml", "data/notes.yaml", "data/README.md"],
    );
    fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
    }
    let mut files = Vec::new();
    for s in &sources {
        walk(&dir.join(s), &mut files);
    }
    let rel: Vec<String> = files
        .iter()
        .map(|p| p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/"))
        .collect();
    let mut ids = Vec::new();
    for fold_case in [true, false] {
        // Python sorts paths by their PARTS (case-insensitively on Windows).
        let key = |r: &String| -> Vec<String> { r.split('/').map(|x| if fold_case { x.to_lowercase() } else { x.to_string() }).collect() };
        let mut list = rel.clone();
        list.sort_by_key(key);
        list.push("Cargo.lock".into());
        let mut h = Sha256::new();
        for r in list {
            if not.iter().any(|n| r.starts_with(n.as_str())) {
                continue;
            }
            let bytes = std::fs::read(dir.join(&r)).with_context(|| r.clone())?;
            let mut norm = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                    continue;
                }
                norm.push(bytes[i]);
                i += 1;
            }
            h.update(r.as_bytes());
            h.update([0]);
            h.update(&norm);
            h.update([0]);
        }
        let id: String = h.finalize().iter().map(|x| format!("{x:02x}")).collect::<String>()[..12].to_string();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn main() {
    enable_vt();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::locate();
    let code = (|| -> Result<()> {
        let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        // A bare flag (`--cores 8`) means run; `--help`/`--version` are commands.
        let cmd = match args.first().map(String::as_str) {
            Some(a @ ("--help" | "-h" | "--version" | "-V")) => a,
            Some(a) if !a.starts_with('-') => a,
            _ => "run",
        };
        match cmd {
            "help" | "-h" | "--help" => print!("{HELP}"),
            "version" | "-V" | "--version" => println!("simfleet {}", env!("CARGO_PKG_VERSION")),
            "engine-id" => {
                let dir = args.get(1).context("用法: simfleet engine-id <WFSim 源码目录>")?;
                for id in engine_id(std::path::Path::new(dir))? {
                    println!("{id}");
                }
            }
            "setup" => {
                wizard(&paths, paths.load()?, true)?;
            }
            "login" => {
                let mut c = match paths.load()? {
                    Some(c) => c,
                    None => match wizard(&paths, None, true)? {
                        Some(c) => c,
                        None => return Ok(()),
                    },
                };
                if let Some(cookie) = flag("--cookie") {
                    adopt_session(&mut c, cookie.trim().trim_start_matches("wfsim_session=").to_string())?;
                } else {
                    login_email_interactive(&mut c)?;
                }
                paths.save(&c)?;
            }
            "logout" => {
                if let Some(mut c) = paths.load()? {
                    account::logout(&net::Net::new(&c.server, Some(c.account.session.clone())));
                    c.account = Default::default();
                    paths.save(&c)?;
                    println!("已退出登录。本机之后的贡献记在本机 id 上（已记到账号的不变）。");
                }
            }
            "status" => match paths.load()? {
                Some(c) => status(&c)?,
                None => println!("还没有设置，先运行 simfleet"),
            },
            "replay" => {
                // Developer check: compute a saved task offline and print the answer.
                let file = args.get(1).context("用法: simfleet replay <task.json> [--lanes N]")?;
                let w: serde_json::Value = jsnum::parse(&std::fs::read_to_string(file)?)?;
                let lanes = flag("--lanes").and_then(|x| x.parse().ok()).unwrap_or_else(sys::cores);
                let mut cfg = Config::fresh(lanes);
                cfg.native_engine = flag("--wasm").is_none() && !args.iter().any(|a| a == "--wasm");
                let net = net::Net::new(&cfg.server, None);
                let ui = Ui::new();
                let rel = work::fetch_release(&net)?;
                let code = work::load_code(&net, &paths, &cfg, &ui, &rel)?;
                // A replay of a riven gain runs where the fleet's would: natively once the canary says so.
                if let Some(n) = &code.native {
                    eprintln!("riven canary: {}", ["not asked", "checking", "same answer", "differs"][n.wait_riven() as usize]);
                }
                let ctl = Arc::new(Ctl {
                    paused: AtomicBool::new(false),
                    quit: AtomicBool::new(false),
                    idle_cores: AtomicUsize::new(lanes),
                    active_cores: AtomicUsize::new(lanes),
                    max_cores: lanes,
                    max_slots: AtomicUsize::new(0),
                });
                let mut f = fleet::Fleet::new(net, cfg, paths.clone(), ctl, ui, rel, code);
                f.replay(&w)?;
            }
            "cpu" => {
                let c = paths.load()?.unwrap_or_else(|| Config::fresh(sys::cores()));
                print!("{}", affinity::report(affinity::Prefer::parse(&c.core_type), c.idle_cores.max(c.active_cores)));
            }
            "check-native" => check_native(&paths, flag("--engine"), flag("--wasm"), flag("--boards"), flag("--rows"), flag("--lanes").and_then(|x| x.parse().ok()), flag("--weapons").and_then(|x| x.parse().ok()).unwrap_or(4))?,
            "bench" => {
                let c = paths.load()?.unwrap_or_else(|| Config::fresh(1));
                bench(&paths, &c, flag("--lanes").and_then(|x| x.parse().ok()))?;
            }
            "run" => {
                let cfg = match paths.load()? {
                    Some(c) if c.consented() => c,
                    other => match wizard(&paths, other, false)? {
                        Some(c) => c,
                        None => return Ok(()),
                    },
                };
                run(paths, cfg, flag("--cores").and_then(|x| x.parse().ok()))?;
            }
            other => {
                print!("不认识的命令 {other}\n\n{HELP}");
            }
        }
        Ok(())
    })();
    if let Err(e) = code {
        eprintln!("simfleet: {e:#}");
        if std::env::args().len() <= 1 {
            ask("按回车关闭…");
        }
        std::process::exit(1);
    }
}
