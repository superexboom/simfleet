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

mod account;
mod config;
mod engine;
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
  simfleet engine-id <dir> 计算一份 WFSim 源码的引擎号（原生引擎构建脚本用）

运行时按键: P 暂停/继续 · + / - 调整空闲核心数 · Q 退出（会先把手头任务交还）

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
#[cfg(not(windows))]
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
fn wizard(paths: &Paths, old: Option<Config>) -> Result<Option<Config>> {
    let cores = sys::cores();
    let mut c = old.unwrap_or_else(|| Config::fresh(default_idle(cores)));
    println!("\n{C_OK}simfleet{C_END} — WFSim 志愿计算客户端（非官方，基于 WFSim 引擎）\n");
    println!("这台电脑会为 wfsim.app 计算排行榜分数和紫卡增益，结果免费给所有人用：");
    println!("  · 只在 simfleet 运行时计算，进程以最低优先级运行，不和你抢 CPU；");
    println!("  · 你在用电脑时只占 1 个核心（可设为暂停），离开一分钟后最多占你设定的核心数；");
    println!("  · 用电池时暂停；运行中按 P 暂停、按 Q 退出；");
    println!("  · 计算结果不出售，你也不会得到报酬；贡献记在本机，登录后记到你的账号名下。");
    println!("  · 每份结果都要另一台不同主人、不同网络的电脑算出同样的数才算数，服务器还会抽查。\n");
    if !yes("同意并开启计算吗？[Y/n] ", true) {
        println!("好的，没有开启。随时运行 simfleet 再来。");
        return Ok(None);
    }
    if c.consent_at.is_empty() {
        c.consent_at = sys::iso(sys::now_ms());
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
    let painter = ui.clone().run(ctl.clone());
    let started = Instant::now();
    let result = (|| -> Result<work::Fleet> {
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
        Ok(work::Fleet::new(net.clone(), cfg.clone(), paths_clone(&paths), ctl.clone(), ui.clone(), rel, code))
    })();
    let mut summary = None;
    match result {
        Ok(mut fleet) => {
            fleet.run();
            if fleet.session_dropped {
                cfg.account = Default::default();
            }
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
    paths.save(&cfg)?;
    if let Some(s) = summary {
        println!(
            "本次运行 {:.0} 分钟，提交 {} 个任务，约 {:.2} 分（另一台电脑确认后计入）。再见！",
            started.elapsed().as_secs_f64() / 60.0,
            s.session_tasks,
            ui::points(s.session_work)
        );
    }
    Ok(())
}

fn paths_clone(p: &Paths) -> Paths {
    Paths { dir: p.dir.clone(), config: p.config.clone(), cache: p.cache.clone(), engines: p.engines.clone() }
}

fn status(cfg: &Config) -> Result<()> {
    let net = net::Net::new(&cfg.server, Some(cfg.account.session.clone()));
    let p = net.post("/api/board/points", &serde_json::json!({ "verifier": cfg.verifier, "since": sys::local_midnight_utc_hour() }))?;
    println!("本机 id {}…", &cfg.verifier[..6]);
    println!("  未归属账号的点数 {} · 近 30 天 {} · 近 7 天 {} · 已绑定账号: {}", p.body["points"], p.body["recent"], p.body["week"], p.body["claimed"]);
    if let Some(t) = p.body.get("today") {
        println!("  今天 {} 个任务 · {} 分", t["tasks"], t["points"]);
    }
    if net.session.is_some() {
        match account::devices(&net) {
            Some(d) => println!(
                "账号 {}：累计 {} 分 · 近 30 天 {} · 近 7 天 {} · 贡献者段位 {} · {} 台设备",
                cfg.account.name, d["points"], d["recent"], d["week"], d["contributor_rank"]["rank"],
                d["devices"].as_array().map_or(0, |a| a.len())
            ),
            None => println!("账号会话无效或已过期，运行 simfleet login 重新登录"),
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
fn check_native(paths: &Paths) -> Result<()> {
    let mut cfg = Config::fresh(sys::cores());
    cfg.native_engine = true;
    let net = net::Net::new(&cfg.server, None);
    let ui = Ui::new();
    let rel = work::fetch_release(&net)?;
    let code = work::load_code(&net, paths, &cfg, &ui, &rel)?;
    if code.native.is_none() {
        bail!("{} 下没有可用的原生引擎 {}（或金丝雀不一致）", paths.engines.display(), rel.engine);
    }
    let lanes = sys::cores();
    let (mut same, mut skipped) = (0, 0);
    for weapon in ["braton_prime", "dual_toxocyst", "torid", "burston_prime"] {
        let rows = net.get(&format!("/board/{weapon}.json"))?.body;
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
                answers.push(s.map(|s| (s.score.to_bits(), s.work, s.metric)));
            }
            match (&answers[0], &answers[1]) {
                (None, None) => skipped += 1,
                (a, b) if a == b => {
                    same += 1;
                    println!("  一致  {weapon} · {ruler} · {mode}");
                }
                (a, b) => bail!("不一致：{weapon} · {ruler} · {mode}: wasm {a:?} / native {b:?}"),
            }
        }
    }
    if same < 6 {
        bail!("只比对了 {same} 行（跳过 {skipped}），不足以放行");
    }
    println!("原生引擎 {} 与 wasm 在 {same} 行榜单上逐位一致（跳过 {skipped} 行）", rel.engine);
    Ok(())
}

/// WFSim scripts/build_site_app.py `engine_id`, for a source checkout.
fn engine_id(dir: &std::path::Path) -> Result<Vec<String>> {
    use sha2::{Digest, Sha256};
    const SOURCES: [&str; 4] = ["engine/src", "webapi/src", "wasm/src", "data"];
    const NOT: [&str; 6] = ["data/i18n/", "data/board_state.yaml", "data/market.yaml", "data/assets.yaml", "data/notes.yaml", "data/README.md"];
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
    for s in SOURCES {
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
            if NOT.iter().any(|n| r.starts_with(n)) {
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
                wizard(&paths, paths.load()?)?;
            }
            "login" => {
                let mut c = match paths.load()? {
                    Some(c) => c,
                    None => match wizard(&paths, None)? {
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
                let ctl = Arc::new(Ctl {
                    paused: AtomicBool::new(false),
                    quit: AtomicBool::new(false),
                    idle_cores: AtomicUsize::new(lanes),
                    active_cores: AtomicUsize::new(lanes),
                    max_cores: lanes,
                });
                let mut f = work::Fleet::new(net, cfg, paths_clone(&paths), ctl, ui, rel, code);
                f.replay(&w)?;
            }
            "check-native" => check_native(&paths)?,
            "bench" => {
                let c = paths.load()?.unwrap_or_else(|| Config::fresh(1));
                bench(&paths, &c, flag("--lanes").and_then(|x| x.parse().ok()))?;
            }
            "run" => {
                let cfg = match paths.load()? {
                    Some(c) if c.consented() => c,
                    other => match wizard(&paths, other)? {
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
