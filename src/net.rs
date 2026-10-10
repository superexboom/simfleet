// SPDX-License-Identifier: AGPL-3.0-or-later
//! The site's endpoints, over one keep-alive agent. Every answer is read the
//! way a browser holds it (`jsnum::parse`), since what the server hands out is
//! fed to the engine.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

pub const USER_AGENT: &str = concat!("simfleet/", env!("CARGO_PKG_VERSION"), " (volunteer compute client for wfsim.app)");

#[derive(Clone)]
pub struct Net {
    /// Certificates checked by the operating system, as a browser checks them.
    agent: ureq::Agent,
    /// …and by the bundled Mozilla roots, for a system whose verifier fails.
    fallback: ureq::Agent,
    pub base: String,
    pub session: Option<String>,
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
        }
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
    pub fn post(&self, path: &str, body: &Value) -> Result<Reply> {
        let text = body.to_string();
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
