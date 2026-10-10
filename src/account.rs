// SPDX-License-Identifier: AGPL-3.0-or-later
//! Signing in, so this machine's work counts under a wfsim.app account
//! (WFSim docs/BOARD.md §"Contribution" rule 5: a signed-in page CLAIMS its
//! computer id). An account is optional: an unclaimed id keeps what it earns,
//! and its first claim takes all of it.

use crate::net::Net;
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// Email + password, the first-party way in. Returns the session cookie value.
pub fn login_email(net: &Net, email: &str, password: &str) -> Result<String> {
    let r = net.post("/api/auth/email/login", &json!({ "email": email, "password": password }))?;
    if r.status != 200 || r.body["ok"] != json!(true) {
        let why = r.body["error"].as_str().unwrap_or("").to_string();
        match why.as_str() {
            "wrong_credentials" => bail!("邮箱或密码不对"),
            "locked" => bail!("登录失败次数过多，15 分钟后再试"),
            "rate_limited" => bail!("请求太频繁，稍后再试"),
            _ => bail!("登录失败（HTTP {} {}）", r.status, why),
        }
    }
    for c in &r.set_cookies {
        if let Some(rest) = c.strip_prefix("wfsim_session=") {
            let v = rest.split(';').next().unwrap_or("").to_string();
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    bail!("服务器没有返回会话")
}

/// The signed-in account (`{id, username, display_name, …}`), or `None` when the
/// session is not (or no longer) valid.
pub fn whoami(net: &Net) -> Result<Option<Value>> {
    let r = net.get("/api/account")?;
    Ok(r.body.get("account").filter(|a| a.is_object()).cloned())
}

pub fn display_name(acct: &Value) -> String {
    let dn = acct["display_name"].as_str().unwrap_or("");
    let un = acct["username"].as_str().unwrap_or("");
    match (dn.is_empty(), un.is_empty()) {
        (false, false) => format!("{dn} (@{un})"),
        (false, true) => dn.to_string(),
        (true, false) => format!("@{un}"),
        _ => "已登录".into(),
    }
}

/// Join this machine's computer id to the signed-in account.
pub fn claim(net: &Net, verifier: &str) -> Result<()> {
    claim_as(net, verifier, 0)
}

/// …the `group`-th of this machine's ids, named so the account's device list
/// tells them apart ("simfleet · HOST #2").
pub fn claim_as(net: &Net, verifier: &str, group: usize) -> Result<()> {
    let n = if group > 0 { format!(" #{}", group + 1) } else { String::new() };
    let label: String = format!("simfleet · {}{n}", crate::sys::hostname()).chars().take(40).collect();
    let r = net.post("/api/account/devices/claim", &json!({ "verifier": verifier, "label": label }))?;
    if r.status != 200 || r.body["ok"] != json!(true) {
        bail!("绑定设备失败（HTTP {} {}）", r.status, r.body["error"].as_str().unwrap_or(""));
    }
    Ok(())
}

/// The account's own contribution page (`/api/account/devices`).
pub fn devices(net: &Net) -> Option<Value> {
    let r = net.get("/api/account/devices").ok()?;
    (r.status == 200 && r.body["ok"] == json!(true)).then_some(r.body)
}

pub fn logout(net: &Net) {
    let _ = net.post("/api/auth/logout", &json!({}));
}
