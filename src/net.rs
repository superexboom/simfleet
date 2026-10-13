// SPDX-License-Identifier: AGPL-3.0-or-later
//! The site's endpoints, over one keep-alive agent. Every answer is read the
//! way a browser holds it (`jsnum::parse`), since what the server hands out is
//! fed to the engine.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const USER_AGENT: &str = concat!("simfleet/", env!("CARGO_PKG_VERSION"), " (volunteer compute client for wfsim.app)");

/// THE RELAY (WFSim docs/DISTRIBUTION.md §"The relay", 69-board-work.js
/// `chooseRoute`): a server in Hong Kong that carries `/api/board/` alone to
/// wfsim.app, for readers whose road to the edge is poor. Both roads are timed
/// every `ROUTE_EVERY` (`/api/board/ping`, `ROUTE_TRIES` each, the median) and
/// the quicker is taken; a call the relay drops goes direct at once, and the
/// relay is left alone until the next timing. No cookie crosses it.
const SITE: &str = "https://wfsim.app";
const RELAY: &str = "https://cn.wfsim.app";
const ROUTE_EVERY: Duration = Duration::from_secs(600);
const ROUTE_TRIES: usize = 3;

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
    route: Arc<Mutex<Route>>,
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
        }
    }

    /// Whether the board's work goes by the relay now.
    pub fn relayed(&self) -> bool {
        self.route.lock().map(|r| r.relay).unwrap_or(false)
    }

    /// The road for `path`: the relay's base, or `None` to go direct. A timing
    /// that is due starts on a thread of its own; this call goes by the road
    /// chosen last.
    fn road(&self, path: &str) -> Option<&'static str> {
        if self.base != SITE || !path.starts_with("/api/board/") {
            return None;
        }
        let mut r = self.route.lock().ok()?;
        if !r.timing && r.at.is_none_or(|t| t.elapsed() >= ROUTE_EVERY) {
            r.timing = true;
            let (net, route) = (self.clone(), self.route.clone());
            std::thread::spawn(move || {
                let (direct, relay) = (net.time_road(SITE), net.time_road(RELAY));
                if let Ok(mut r) = route.lock() {
                    r.relay = relay < direct;
                    r.at = Some(Instant::now());
                    r.timing = false;
                }
            });
        }
        r.relay.then_some(RELAY)
    }

    /// The median of `ROUTE_TRIES` pings by one road, in seconds; infinite when one fails.
    fn time_road(&self, base: &str) -> f64 {
        let mut ms = Vec::new();
        for _ in 0..ROUTE_TRIES {
            let t = Instant::now();
            let url = format!("{base}/api/board/ping");
            let ok = self.agent.get(&url).config().timeout_global(Some(Duration::from_secs(5))).build().call().ok().and_then(|mut r| {
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

    /// POST JSON; `Err` only when the network failed or the answer was not JSON.
    /// The board's work may go by the relay (`road`), and goes direct at once
    /// when the relay drops it.
    pub fn post(&self, path: &str, body: &Value) -> Result<Reply> {
        let text = body.to_string();
        if let Some(relay) = self.road(path) {
            let url = format!("{relay}{path}");
            let got = self
                .call(|a| a.post(&url).header("Content-Type", "application/json").send(text.as_str()))
                .and_then(|mut r| Self::reply(&mut r))
                .ok()
                .filter(|r| (200..300).contains(&r.status));
            if let Some(r) = got {
                return Ok(r);
            }
            if let Ok(mut r) = self.route.lock() {
                r.relay = false;
                r.at = Some(Instant::now());
            }
        }
        let mut resp = self.call(|a| {
            let mut req = a.post(&self.url(path)).header("Content-Type", "application/json");
            // The session is the site's: never sent to another host.
            if let (Some(s), false) = (&self.session, path.starts_with("http")) {
                req = req.header("Cookie", &format!("wfsim_session={s}"));
            }
            req.send(text.as_str())
        })?;
        Self::reply(&mut resp)
    }

    pub fn get(&self, path: &str) -> Result<Reply> {
        let mut resp = self.call(|a| {
            let mut req = a.get(&self.url(path)).header("Cache-Control", "no-store");
            // The session is the site's: never sent to another host.
            if let (Some(s), false) = (&self.session, path.starts_with("http")) {
                req = req.header("Cookie", &format!("wfsim_session={s}"));
            }
            req.call()
        })?;
        Self::reply(&mut resp)
    }

    fn reply(resp: &mut ureq::http::Response<ureq::Body>) -> Result<Reply> {
        let status = resp.status().as_u16();
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
    pub fn download(&self, path: &str, mut progress: impl FnMut(u64, u64)) -> Result<Vec<u8>> {
        let mut resp = self.call(|a| a.get(&self.url(path)).config().timeout_global(Some(Duration::from_secs(600))).build().call())?;
        if resp.status().as_u16() == 404 {
            bail!(NotFound);
        }
        if resp.status().as_u16() != 200 {
            bail!("GET {path}: HTTP {}", resp.status().as_u16());
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
