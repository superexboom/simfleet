// SPDX-License-Identifier: AGPL-3.0-or-later
//! The site's endpoints, over one keep-alive agent. Every answer is read the
//! way a browser holds it (`jsnum::parse`), since what the server hands out is
//! fed to the engine.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::io::Read;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const USER_AGENT: &str = concat!("simfleet/", env!("CARGO_PKG_VERSION"), " (volunteer compute client for wfsim.app)");

/// THE RELAY (WFSim docs/DISTRIBUTION.md §"The relay", 69-board-work.js
/// `chooseRoute`): a server in Hong Kong that carries the board's work
/// (`RELAY_PATHS`) to wfsim.app, for readers whose road to the edge is poor.
/// Both roads are timed every `ROUTE_EVERY` (`/api/board/ping`, `ROUTE_TRIES`
/// each, the median) and the quicker is taken; a call either road drops goes
/// by the other at once, which is then kept until the next timing, and a call
/// both drop has the roads timed again. No cookie crosses it.
const SITE: &str = "https://wfsim.app";
const CDN: &str = "https://cdn.wfsim.app";
const RELAY: &str = "https://cn.wfsim.app";
const ROUTE_EVERY: Duration = Duration::from_secs(600);
const ROUTE_TRIES: usize = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Matches HK nginx `proxy_read_timeout 60s` with headroom so a slow server
/// `board_sync` under heavy queue load finishes cleanly instead of aborting
/// at 30s (`timeout: global`) after the server already committed the lease.
const BOARD_POST_TIMEOUT: Duration = Duration::from_secs(65);
/// WHAT THE RELAY CARRIES (69-board-work.js `RELAY_PATHS`, plus cookie-free
/// `/api/appraise/*` routed by `deploy/hk-website.nginx`).
const RELAY_PATHS: [&str; 8] = [
    "/api/board/sync",
    "/api/board/work",
    "/api/board/answers",
    "/api/board/renew",
    "/api/board/release",
    "/api/board/mine",
    "/api/board/points",
    "/api/board/ping",
];

fn relay_carries(path: &str) -> bool {
    RELAY_PATHS.contains(&path) || path.starts_with("/api/appraise/")
}

#[derive(Default)]
struct Route {
    relay: bool,
    at: Option<Instant>,
    timing: bool,
}

#[derive(Clone)]
pub struct Net {
    /// Certificates checked by the operating system, as a browser checks them.
    agent: ureq::Agent,
    /// …and by the bundled Mozilla roots, for a system whose verifier fails.
    fallback: ureq::Agent,
    pub base: String,
    pub session: Option<String>,
    route: Arc<(Mutex<Route>, Condvar)>,
    /// THE SERVER'S CLOCK, as the offset of its replies' `Date` from this
    /// machine's (ms; `i64::MIN` until one is read): a clock minutes off would
    /// read a fresh work signal as stale (69-board-work.js `learnServerClock`).
    skew: Arc<AtomicI64>,
}

fn tls_failure(e: &ureq::Error) -> bool {
    let t = e.to_string().to_lowercase();
    ["certificate", "tls", "handshake", "unknownissuer", "verif"].iter().any(|k| t.contains(k))
}

/// What a reader can do about a connection that failed on both verifiers.
fn explain(first: &ureq::Error, second: &ureq::Error) -> String {
    let both = format!("{first} / {second}");
    if tls_failure(first) && tls_failure(second) {
        format!("证书校验失败（{second}）。常见原因：加速器或代理改写了证书但其根证书没有装进 Windows，或系统时间不对")
    } else {
        both
    }
}

/// A download that does not exist (yet).
#[derive(Debug)]
pub struct NotFound;
impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not found")
    }
}
impl std::error::Error for NotFound {}

pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub set_cookies: Vec<String>,
}

impl Net {
    pub fn new(base: &str, session: Option<String>) -> Net {
        let make = |roots: ureq::tls::RootCerts| -> ureq::Agent {
            ureq::Agent::config_builder()
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_global(Some(Duration::from_secs(30)))
                .http_status_as_error(false)
                .user_agent(USER_AGENT)
                .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build())
                .build()
                .into()
        };
        Net {
            agent: make(ureq::tls::RootCerts::PlatformVerifier),
            fallback: make(ureq::tls::RootCerts::WebPki),
            base: base.trim_end_matches('/').to_string(),
            session: session.filter(|s| !s.is_empty()),
            route: Arc::default(),
            skew: Arc::new(AtomicI64::new(i64::MIN)),
        }
    }

    /// Now by the server's clock (ms since the epoch), or by this machine's
    /// before a reply from the site said its time.
    pub fn server_now_ms(&self) -> u64 {
        let now = crate::sys::now_ms();
        match self.skew.load(Ordering::Relaxed) {
            i64::MIN => now,
            s => (now as i64).saturating_add(s).max(0) as u64,
        }
    }

    /// Whether the board's work goes by the relay now.
    pub fn relayed(&self) -> bool {
        self.route.0.lock().map(|r| r.relay).unwrap_or(false)
    }

    /// Choose between direct and HK relay on `SITE`: waits on initial/recovery
    /// parallel timing (`at == None`, as 69-board-work.js `chooseRoute` awaits
    /// `routing`), and refreshes asynchronously in the background once `at` ages
    /// past `ROUTE_EVERY`.
    fn choose_relay(&self) -> Option<bool> {
        if self.base != SITE {
            return None;
        }
        let (lock, cvar) = &*self.route;
        let mut r = lock.lock().ok()?;
        if r.at.is_some_and(|t| t.elapsed() < ROUTE_EVERY) {
            return Some(r.relay);
        }
        if r.at.is_some() {
            if !r.timing {
                r.timing = true;
                let net = self.clone();
                std::thread::spawn(move || net.refresh_route());
            }
            return Some(r.relay);
        }
        if !r.timing {
            r.timing = true;
            drop(r);
            self.refresh_route();
            return Some(self.relayed());
        }
        while r.timing {
            r = cvar.wait(r).ok()?;
        }
        Some(r.relay)
    }

    fn refresh_route(&self) {
        let (direct, relay) = std::thread::scope(|s| {
            let d = s.spawn(|| self.time_road(SITE));
            let r = s.spawn(|| self.time_road(RELAY));
            (d.join().unwrap_or(f64::INFINITY), r.join().unwrap_or(f64::INFINITY))
        });
        let (lock, cvar) = &*self.route;
        if let Ok(mut r) = lock.lock() {
            if direct.is_finite() || relay.is_finite() {
                r.relay = relay < direct;
                r.at = Some(Instant::now());
            } else {
                r.at = None;
            }
            r.timing = false;
            cvar.notify_all();
        }
    }

    /// CDN and the Hong Kong entrance both serve `/work.json` and `/stats/*`.
    fn public_hosts(&self) -> (&str, Option<&str>) {
        match self.base.as_str() {
            SITE if self.choose_relay().unwrap_or(false) => (RELAY, Some(CDN)),
            SITE => (CDN, Some(RELAY)),
            RELAY => (RELAY, Some(CDN)),
            other => (other, None),
        }
    }

    pub fn public_file(&self, path: &str) -> String {
        format!("{}{path}", self.public_hosts().0)
    }

    pub fn work_signal(&self) -> Option<Value> {
        let (primary, alternate) = self.public_hosts();
        std::iter::once(primary).chain(alternate).find_map(|host| {
            let url = format!("{host}{}", crate::sync::SIGNAL_PATH);
            let reply = self.get_no_cookie(&url, Duration::from_secs(5)).ok()?;
            (reply.status == 200 && crate::sync::current_signal(&reply.body, self.server_now_ms()))
                .then_some(reply.body)
        })
    }

    /// Public queue/demand snapshot across CDN and the HK/direct API endpoints.
    /// `cn.wfsim.app` serves `/api/board/demand` (JSON), not `/demand.json`.
    pub fn demand_snapshot(&self, default_ttl: Duration) -> Option<(Value, u64, Duration)> {
        let urls: Vec<String> = match self.base.as_str() {
            SITE if self.choose_relay().unwrap_or(false) => vec![
                format!("{RELAY}/api/board/demand"),
                format!("{CDN}/demand.json"),
                format!("{SITE}/api/board/demand"),
            ],
            SITE => vec![
                format!("{CDN}/demand.json"),
                format!("{SITE}/api/board/demand"),
                format!("{RELAY}/api/board/demand"),
            ],
            RELAY => vec![format!("{RELAY}/api/board/demand"), format!("{CDN}/demand.json")],
            other => vec![format!("{other}/demand.json"), format!("{other}/api/board/demand")],
        };
        urls.into_iter().find_map(|url| {
            let reply = self.get_no_cookie(&url, Duration::from_secs(8)).ok()?;
            if reply.status != 200 {
                return None;
            }
            let (total, ttl) = crate::sync::queue_count(&reply.body, self.server_now_ms())?;
            Some((reply.body, total, ttl.unwrap_or(default_ttl)))
        })
    }

    /// The road for `path`: `Some(true)` by the relay, `Some(false)` direct,
    /// `None` when the relay does not carry it.
    fn road(&self, path: &str) -> Option<bool> {
        if !relay_carries(path) {
            return None;
        }
        self.choose_relay()
    }

    /// The median of `ROUTE_TRIES` pings by one road, in seconds; infinite when one fails.
    fn time_road(&self, base: &str) -> f64 {
        let mut ms = Vec::new();
        for _ in 0..ROUTE_TRIES {
            let t = Instant::now();
            let url = format!("{base}/api/board/ping");
            let ok = self
                .call(|a| a.get(&url).config().timeout_connect(Some(Duration::from_secs(4))).timeout_global(Some(Duration::from_secs(4))).build().call())
                .ok()
                .and_then(|mut r| {
                    (r.status().as_u16() == 200).then_some(())?;
                    let text = r.body_mut().read_to_string().ok()?;
                    serde_json::from_str::<Value>(&text).ok()
                });
            if ok.is_none() {
                return f64::INFINITY;
            }
            ms.push(t.elapsed().as_secs_f64());
        }
        ms.sort_by(|a, b| a.total_cmp(b));
        ms[ms.len() / 2]
    }

    /// One request, on the system verifier and then — only if the connection
    /// itself failed — once more on the bundled roots.
    fn call(&self, f: impl Fn(&ureq::Agent) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<ureq::http::Response<ureq::Body>> {
        match f(&self.agent) {
            Ok(r) => Ok(r),
            // ONLY A HANDSHAKE THAT FAILED is tried again: nothing was sent, so
            // nothing (a result, a lease) can arrive twice.
            Err(first) if tls_failure(&first) => f(&self.fallback).map_err(|second| anyhow::anyhow!("{}", explain(&first, &second))),
            Err(e) => Err(e.into()),
        }
    }

    /// A path on the site, or an absolute URL as given.
    fn url(&self, path: &str) -> String {
        if path.starts_with("https://") || path.starts_with("http://") {
            path.to_string()
        } else {
            format!("{}{}", self.base, path)
        }
    }

    fn road_answered(r: &Result<Reply>) -> bool {
        matches!(r, Ok(rep) if (200..300).contains(&rep.status) || rep.status == 400 || rep.body["error"] == "unrecognised_client")
    }

    /// POST JSON; `Err` only when the network failed or the answer was not JSON.
    /// The board's work goes by the road chosen (`road`), and by the other at
    /// once when that one drops it (no answer, or 5xx, as the page's
    /// `postBoardWork` reads it).
    pub fn post(&self, path: &str, body: &Value) -> Result<Reply> {
        let text = body.to_string();
        let Some(relay) = self.road(path) else { return self.post_by(false, path, &text) };
        let first = self.post_by(relay, path, &text);
        if Self::road_answered(&first) {
            return first;
        }
        let again = self.post_by(!relay, path, &text);
        let ok = Self::road_answered(&again);
        if let Ok(mut r) = self.route.0.lock() {
            if ok {
                r.relay = !relay;
                r.at = Some(Instant::now());
            } else {
                // Both dropped it: keep the last known road, and re-time on the next call.
                r.at = None;
            }
        }
        if ok {
            return again;
        }
        match (first, again) {
            (Ok(r), _) => Ok(r),
            (_, Ok(r)) => Ok(r),
            (Err(e1), Err(e2)) => {
                let (a, b) = if relay { ("中继", "直连") } else { ("直连", "中继") };
                Err(anyhow!("{a}: {e1:#}；{b}: {e2:#}"))
            }
        }
    }

    /// One POST by the relay or direct; the session goes only direct.
    fn post_by(&self, relay: bool, path: &str, text: &str) -> Result<Reply> {
        let timeout = if relay || relay_carries(path) { BOARD_POST_TIMEOUT } else { Duration::from_secs(30) };
        let mut resp = self.call(|a| {
            if relay {
                return a
                    .post(&format!("{RELAY}{path}"))
                    .header("Content-Type", "application/json")
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
                    .send(text);
            }
            let mut req = a.post(&self.url(path)).header("Content-Type", "application/json");
            // The session is the site's: never sent to another host.
            if let (Some(s), false) = (&self.session, path.starts_with("http")) {
                req = req.header("Cookie", &format!("wfsim_session={s}"));
            }
            req.config().timeout_global(Some(timeout)).build().send(text)
        })?;
        self.reply(&mut resp, !path.starts_with("http") || relay)
    }

    pub fn get(&self, path: &str) -> Result<Reply> {
        self.get_timeout(path, Duration::from_secs(30))
    }

    /// Cookie-free GET for public site JSON (`/release.json`, `/board/*`),
    /// trying the chosen road first and falling back to the mirror on `SITE`.
    pub fn get_site(&self, path: &str) -> Result<Reply> {
        let Some(relay) = (!path.starts_with("http")).then(|| self.choose_relay()).flatten() else {
            return self.get_no_cookie(path, Duration::from_secs(20));
        };
        let (primary, alternate) = if relay { (RELAY, SITE) } else { (SITE, RELAY) };
        let first = self.get_no_cookie(&format!("{primary}{path}"), Duration::from_secs(20));
        if matches!(&first, Ok(r) if r.status == 200) {
            return first;
        }
        let second = self.get_no_cookie(&format!("{alternate}{path}"), Duration::from_secs(20));
        if matches!(&second, Ok(r) if r.status == 200) {
            return second;
        }
        first.or(second)
    }

    fn get_no_cookie(&self, path: &str, timeout: Duration) -> Result<Reply> {
        let url = self.url(path);
        let site = !path.starts_with("http")
            || url.starts_with(SITE)
            || url.starts_with(RELAY)
            || url.starts_with(CDN);
        let mut resp = self.call(|a| {
            a.get(&url)
                .header("Cache-Control", "no-store")
                .config()
                .timeout_global(Some(timeout))
                .build()
                .call()
        })?;
        self.reply(&mut resp, site)
    }

    pub fn get_timeout(&self, path: &str, timeout: Duration) -> Result<Reply> {
        let mut resp = self.call(|a| {
            let mut req = a.get(&self.url(path)).header("Cache-Control", "no-store");
            // The session is the site's: never sent to another host.
            if let (Some(s), false) = (&self.session, path.starts_with("http")) {
                req = req.header("Cookie", &format!("wfsim_session={s}"));
            }
            req.config().timeout_global(Some(timeout)).build().call()
        })?;
        self.reply(&mut resp, !path.starts_with("http"))
    }

    /// A reply read whole; `site`: from wfsim.app or its relay, whose `Date`
    /// is the server's clock.
    fn reply(&self, resp: &mut ureq::http::Response<ureq::Body>, site: bool) -> Result<Reply> {
        let status = resp.status().as_u16();
        let date = resp.headers().get("date").and_then(|v| crate::sys::parse_http_date(v.to_str().ok()?));
        if let (true, Some(d)) = (site, date) {
            self.skew.store(d as i64 - crate::sys::now_ms() as i64, Ordering::Relaxed);
        }
        let set_cookies = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok().map(String::from))
            .collect();
        let text = resp.body_mut().with_config().limit(16 << 20).read_to_string()?;
        let body = crate::jsnum::parse(&text).map_err(|e| anyhow!("HTTP {status}: not JSON ({e})"))?;
        Ok(Reply { status, body, set_cookies })
    }

    /// POST and take the body of an `ok` answer; `None` for anything else, the
    /// way the page's `postBoardWork` reads a failure.
    pub fn post_ok(&self, path: &str, body: &Value) -> Option<Value> {
        match self.post(path, body) {
            Ok(r) if (200..300).contains(&r.status) => Some(r.body),
            _ => None,
        }
    }

    /// A file, read whole, with `progress(got, total)` as it arrives.
    /// Relative paths on `SITE` (`/pkg/…`) fall back across the HK mirror.
    pub fn download(&self, path: &str, mut progress: impl FnMut(u64, u64)) -> Result<Vec<u8>> {
        if !path.starts_with("http") {
            if let Some(relay) = self.choose_relay() {
                let (primary, alternate) = if relay { (RELAY, SITE) } else { (SITE, RELAY) };
                let first = self.download_url(&format!("{primary}{path}"), &mut progress);
                if first.is_ok() || first.as_ref().is_err_and(|e| e.is::<NotFound>()) {
                    return first;
                }
                progress(0, 0);
                return self.download_url(&format!("{alternate}{path}"), &mut progress);
            }
        }
        let url = self.url(path);
        self.download_url(&url, &mut progress)
    }

    fn download_url(&self, url: &str, progress: &mut impl FnMut(u64, u64)) -> Result<Vec<u8>> {
        let mut resp = self.call(|a| a.get(url).config().timeout_global(Some(Duration::from_secs(600))).build().call())?;
        if resp.status().as_u16() == 404 {
            bail!(NotFound);
        }
        if resp.status().as_u16() != 200 {
            bail!("GET {url}: HTTP {}", resp.status().as_u16());
        }
        let total = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
            .unwrap_or(0);
        let mut out = Vec::with_capacity(total as usize);
        let mut reader = resp.body_mut().with_config().limit(256 << 20).reader();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            progress(out.len() as u64, total);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn road_answered_preserves_client_errors_and_retries_gateway_failures() {
        let reply = |status: u16, body: Value| -> Result<Reply> {
            Ok(Reply { status, body, set_cookies: Vec::new() })
        };
        assert!(Net::road_answered(&reply(200, json!({ "ok": true }))));
        assert!(Net::road_answered(&reply(400, json!({ "ok": false, "error": "bad request" }))));
        assert!(Net::road_answered(&reply(403, json!({ "ok": false, "error": "unrecognised_client" }))));
        assert!(!Net::road_answered(&reply(502, json!({ "ok": false }))));
        assert!(!Net::road_answered(&reply(503, json!({ "ok": false }))));
        assert!(!Net::road_answered(&Err(anyhow!("timeout: global"))));
        assert!(relay_carries("/api/board/sync"));
        assert!(relay_carries("/api/appraise/abc123/result"));
        assert!(!relay_carries("/api/account"));
    }
}
