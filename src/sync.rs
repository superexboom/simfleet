// SPDX-License-Identifier: AGPL-3.0-or-later
//! PROTOCOL 8 (WFSim docs/CLIENTS.md §"The cycle, protocol 8", worker/sync.js,
//! the page's 69-board-work.js `boardSync`): one door, `/api/board/sync`, for
//! what protocol 7 asked of five. A sync hands in the results its id keeps,
//! names every row the id still holds — renewing them; a row it leaves out goes
//! back — reports the rows its engine refused, asks where earlier results
//! stand and what the id has earned, and asks for as much work as its pace
//! says, or for none. The reply says when the id may next be handed work
//! (`next_ms`), and the server holds it to that.
//!
//! What a sync is made of and read into is here; when each id syncs is the
//! dispatcher's (fleet.rs `syncs`).
//!
//! - RESULTS ARE KEPT ON DISK until a reply lists them taken or dropped
//!   (`Journal`, as the page keeps them in IndexedDB), so a lost reply, a crash
//!   or a restart costs no result.
//! - AN ID HOLDING NOTHING READS THE WORK SIGNAL (`SIGNAL_PATH`) instead of
//!   asking, its age read by the server's clock.
//! - THE DIALS are the server's, read from every reply; the values here are
//!   only its stated defaults, used until the first reply.

use crate::net::Net;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PROTOCOL: u32 = 8;
/// THE WORK SIGNAL, at the edge: an idle id reads it instead of asking.
pub const SIGNAL_PATH: &str = "/work.json";
/// …not believed once older than this by the server's clock (`SIGNAL_STALE_MS`).
const SIGNAL_STALE_MS: u64 = 90_000;
/// WITHOUT A SIGNAL an id holding nothing syncs this often, jittered (`SYNC_FALLBACK_MS`).
pub const FALLBACK_MS: u64 = 120_000;
/// A failed sync waits a random part of this, doubled for each failure in a
/// row up to the most (`SYNC_BACKOFF_MS`, `SYNC_BACKOFF_MAX_MS`).
const BACKOFF_MS: u64 = 5_000;
const BACKOFF_MAX_MS: u64 = 600_000;
/// A held row is renewed by a sync once its lease is this close to its end
/// (`RENEW_BEFORE_MS`).
pub const RENEW_BEFORE: Duration = Duration::from_secs(120);
/// A result kept longer than this is let go, and a taken one whose fact is
/// still unsettled after `PENDING_KEEP_MS` (`RESULT_KEEP_MS`, `PENDING_KEEP_MS`).
const RESULT_KEEP_MS: u64 = 24 * 3_600_000;
const PENDING_KEEP_MS: u64 = 3 * 24 * 3_600_000;
const PENDING_MAX: usize = 2_000;
/// WHAT ONE SYNC CARRIES AT MOST (worker/sync.js `SYNC_RESULTS_MAX`,
/// `STATES_MAX`, `REFUSED_MAX`), its body within `SYNC_BODY` (262 144
/// characters) with room left for the rows held.
pub const RESULTS_MAX: usize = 400;
const RESULTS_BYTES: usize = 200_000;
const STATES_MAX: usize = 100;
pub const REFUSED_MAX: usize = 50;

/// THE DIALS IN FORCE (worker/dials.js `SYNC_CLIENT_DIALS`), told by every
/// reply and by work.json; a client never hard-codes one.
#[derive(Clone, Debug)]
pub struct Dials {
    pub pack8_ms: u64,
    pub pack8_max: u64,
    pub sync_lease_ms: u64,
    pub pack_lease_ms: u64,
    pub sync_low_ms: u64,
    pub sync_floor_ms: u64,
    pub sync_idle_ms: u64,
    pub flush_ms: u64,
    pub packs_held: u64,
    pub row_ceiling_ms: u64,
    pub poll_ms: u64,
    pub jitter_ms: u64,
    pub urgent_jitter_ms: u64,
    pub report_ms: u64,
}

impl Default for Dials {
    /// The server's stated defaults (docs/CLIENTS.md §"Values").
    fn default() -> Dials {
        Dials {
            pack8_ms: 600_000,
            pack8_max: 200,
            sync_lease_ms: 1_500_000,
            pack_lease_ms: 300_000,
            sync_low_ms: 120_000,
            sync_floor_ms: 60_000,
            sync_idle_ms: 20_000,
            flush_ms: 600_000,
            packs_held: 2,
            row_ceiling_ms: 7_200_000,
            poll_ms: 10_000,
            jitter_ms: 5_000,
            urgent_jitter_ms: 2_000,
            report_ms: 3_600_000,
        }
    }
}

impl Dials {
    /// Every dial `v` names as a whole number; the rest stay as they were.
    pub fn take(&mut self, v: &Value) {
        let fields: [(&str, &mut u64); 14] = [
            ("pack8_ms", &mut self.pack8_ms),
            ("pack8_max", &mut self.pack8_max),
            ("sync_lease_ms", &mut self.sync_lease_ms),
            ("pack_lease_ms", &mut self.pack_lease_ms),
            ("sync_low_ms", &mut self.sync_low_ms),
            ("sync_floor_ms", &mut self.sync_floor_ms),
            ("sync_idle_ms", &mut self.sync_idle_ms),
            ("flush_ms", &mut self.flush_ms),
            ("packs_held", &mut self.packs_held),
            ("row_ceiling_ms", &mut self.row_ceiling_ms),
            ("poll_ms", &mut self.poll_ms),
            ("jitter_ms", &mut self.jitter_ms),
            ("urgent_jitter_ms", &mut self.urgent_jitter_ms),
            ("report_ms", &mut self.report_ms),
        ];
        for (k, f) in fields {
            if let Some(x) = v[k].as_u64() {
                *f = x;
            }
        }
        // A look at the signal every instant would spin, and an id holds a pack at least.
        self.poll_ms = self.poll_ms.max(1_000);
        self.packs_held = self.packs_held.max(1);
    }
}

/// A RESULT COMPUTED AND NOT TAKEN YET: what a sync sends of it (`result`,
/// its row's lease inside), the engine that computed it, and when.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Kept {
    pub question: String,
    pub engine: String,
    pub at: u64,
    pub work: u64,
    pub result: Value,
}

/// A RESULT THE SERVER TOOK whose fact is not settled yet, asked after in `states`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pending {
    pub question: String,
    pub work: u64,
    pub at: u64,
}

/// WHAT AN ID KEEPS, one file an id under `kept/` beside the config.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Journal {
    pub verifier: String,
    #[serde(default)]
    pub results: Vec<Kept>,
    #[serde(default)]
    pub pending: Vec<Pending>,
    /// The id's pace as it last was (fleet.rs `Group::pace_ms`): a restart's
    /// first sync says it, and is handed a pack its size — not `pack_first`
    /// rows and "come back in eight minutes".
    #[serde(default)]
    pub pace_ms: f64,
}

impl Kept {
    fn valid(&self) -> bool {
        crate::work::valid_board_score(&self.result) && self.result["work"].as_u64() == Some(self.work)
    }
}

impl Journal {
    fn path(dir: &Path, verifier: &str) -> PathBuf {
        dir.join("kept").join(format!("{}.json", verifier.get(..12).unwrap_or(verifier)))
    }

    /// The id's journal as it was left (empty when there is none), with the
    /// results kept too long let go.
    pub fn load(dir: &Path, verifier: &str) -> Journal {
        let read = std::fs::read(Self::path(dir, verifier)).ok().and_then(|b| serde_json::from_slice::<Journal>(&b).ok());
        let mut j = read.filter(|j| j.verifier == verifier).unwrap_or_else(|| Journal { verifier: verifier.into(), ..Default::default() });
        j.prune(crate::sys::now_ms());
        j
    }

    /// Preserve invalid answers outside the submission journal. Never clamp
    /// work: that would invent a result. A corrupt row must not block its peers.
    pub fn quarantine_invalid(&mut self, dir: &Path) -> std::io::Result<usize> {
        let bad: Vec<&Kept> = self.results.iter().filter(|k| !k.valid()).collect();
        let count = bad.len();
        if count == 0 { return Ok(0); }
        let folder = dir.join("result-diagnostics");
        std::fs::create_dir_all(&folder)?;
        let path = folder.join(Self::path(dir, &self.verifier).file_name().unwrap());
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec(&serde_json::json!({"at":crate::sys::now_ms(),"invalid_results":bad}))?)?;
        std::fs::rename(temp, path)?;
        self.results.retain(Kept::valid);
        Ok(count)
    }

    /// Written whole, through a file beside it; an empty journal leaves no file.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let p = Self::path(dir, &self.verifier);
        if self.results.is_empty() && self.pending.is_empty() && self.pace_ms <= 0.0 {
            return match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, &p)
    }

    /// What has been kept too long goes (`RESULT_KEEP_MS`, `PENDING_KEEP_MS`,
    /// the oldest pending past `PENDING_MAX`); the results let go.
    pub fn prune(&mut self, now: u64) -> usize {
        let before = self.results.len();
        self.results.retain(|k| now.saturating_sub(k.at) <= RESULT_KEEP_MS);
        self.pending.retain(|p| now.saturating_sub(p.at) <= PENDING_KEEP_MS);
        if self.pending.len() > PENDING_MAX {
            let over = self.pending.len() - PENDING_MAX;
            self.pending.drain(..over);
        }
        before - self.results.len()
    }

    pub fn keep(&mut self, k: Kept) {
        self.results.retain(|x| x.question != k.question);
        self.results.push(k);
    }

    /// The results a sync of `engine` carries: that engine's, oldest first,
    /// as many as one body holds.
    pub fn to_send(&self, engine: &str) -> Vec<&Kept> {
        let mut mine: Vec<&Kept> = self.results.iter().filter(|k| k.engine == engine && k.valid()).collect();
        mine.sort_by_key(|k| k.at);
        let (mut out, mut bytes) = (Vec::new(), 0);
        for k in mine {
            bytes += k.result.to_string().len() + 1;
            if out.len() >= RESULTS_MAX || (bytes > RESULTS_BYTES && !out.is_empty()) {
                break;
            }
            out.push(k);
        }
        out
    }

    /// When the oldest result of `engine` was computed.
    pub fn oldest(&self, engine: &str) -> Option<u64> {
        self.results.iter().filter(|k| k.engine == engine).map(|k| k.at).min()
    }

    /// An engine other than `engine` that results were kept of.
    pub fn other_engine(&self, engine: &str) -> Option<String> {
        self.results.iter().find(|k| k.engine != engine).map(|k| k.engine.clone())
    }

    /// WHAT A REPLY SAID of the results `sent`: a TAKEN one moves to pending,
    /// a DROPPED one goes (another computer took its row since, or its engine
    /// is not served). Returns the rows and work taken, and the rows dropped.
    pub fn settle(&mut self, sent: &[String], r: &Value, now: u64) -> (u64, u64, u64) {
        let sent: HashSet<&str> = sent.iter().map(String::as_str).collect();
        let named = |key: &str, field: Option<&str>| -> HashSet<String> {
            r[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| match field {
                    Some(f) => x[f].as_str(),
                    None => x.as_str(),
                })
                .filter(|q| sent.contains(q))
                .map(String::from)
                .collect()
        };
        let (taken, dropped) = (named("taken", None), named("dropped", Some("question")));
        let (mut rows, mut work) = (0, 0);
        for k in self.results.iter().filter(|k| taken.contains(&k.question)) {
            rows += 1;
            work += k.work;
            self.pending.retain(|p| p.question != k.question);
            self.pending.push(Pending { question: k.question.clone(), work: k.work, at: now });
        }
        let gone = self.results.iter().filter(|k| dropped.contains(&k.question) && !taken.contains(&k.question)).count() as u64;
        self.results.retain(|k| !taken.contains(&k.question) && !dropped.contains(&k.question));
        (rows, work, gone)
    }

    /// WHERE TAKEN RESULTS STAND (`states`): one confirmed, or gone, is let
    /// go. Returns how many were confirmed.
    pub fn states(&mut self, r: &Value) -> u64 {
        let mut settled = HashSet::new();
        let mut confirmed = 0;
        for s in r["states"].as_array().into_iter().flatten() {
            let (Some(q), Some(st)) = (s["question"].as_str(), s["state"].as_str()) else { continue };
            if st == "confirmed" || st == "gone" {
                confirmed += u64::from(st == "confirmed" && self.pending.iter().any(|p| p.question == q));
                settled.insert(q.to_string());
            }
        }
        self.pending.retain(|p| !settled.contains(&p.question));
        confirmed
    }

    /// The questions a sync asks after: the oldest taken ones.
    pub fn ask_states(&self) -> Vec<String> {
        let mut p: Vec<&Pending> = self.pending.iter().collect();
        p.sort_by_key(|x| x.at);
        p.into_iter().take(STATES_MAX).map(|x| x.question.clone()).collect()
    }

    pub fn kept_work(&self) -> u64 {
        self.results.iter().filter(|k| k.valid()).map(|k| k.work).sum()
    }
}

/// A SYNC'S BODY (69-board-work.js `boardSyncBody`): `held` is every row the
/// id holds unanswered, by lease — a row left out is given back.
#[allow(clippy::too_many_arguments)]
pub fn body(
    verifier: &str,
    engine: &str,
    consent: Value,
    lanes: usize,
    pace_ms: Option<u64>,
    want_ms: u64,
    held: &[(String, Vec<String>)],
    results: &[&Kept],
    refused: &[Value],
    states: Vec<String>,
    since: &str,
) -> Value {
    let mut b = json!({
        "verifier": verifier,
        "engine": engine,
        "protocol": PROTOCOL,
        "consent": consent,
        "lanes": lanes.clamp(1, 256),
        "want_ms": want_ms,
        "held": held.iter().map(|(lease, rows)| json!({ "lease": lease, "rows": rows })).collect::<Vec<_>>(),
        "results": results.iter().map(|k| k.result.clone()).collect::<Vec<_>>(),
        "states": states,
        "since": since,
    });
    if let Some(p) = pace_ms {
        b["pace_ms"] = json!(p);
    }
    if !refused.is_empty() {
        b["refused"] = json!(refused.iter().take(REFUSED_MAX).collect::<Vec<_>>());
    }
    b
}

/// The rows a reply says are no longer the id's (`held: false`).
pub fn lost(r: &Value) -> Vec<String> {
    r["held"].as_array().into_iter().flatten().filter(|h| h["held"] == json!(false)).filter_map(|h| h["question"].as_str().map(String::from)).collect()
}

/// A SYNC'S OUTCOME as the page reads it.
pub enum Out {
    Reply(Value),
    /// `unrecognised_client`, and the server's message.
    Refused(String),
    Failed(String),
}

/// One sync, by the quicker road (net.rs `road`).
pub fn send(net: &Net, body: &Value) -> Out {
    match net.post("/api/board/sync", body) {
        Ok(r) if (200..300).contains(&r.status) && r.body["ok"] == json!(true) => Out::Reply(r.body),
        Ok(r) if r.body["error"] == json!("unrecognised_client") => {
            Out::Refused(r.body["message"].as_str().unwrap_or("服务器不再给这个客户端派发任务").to_string())
        }
        Ok(r) => Out::Failed(format!("HTTP {}{}", r.status, r.body["error"].as_str().map(|e| format!(" {e}")).unwrap_or_default())),
        Err(e) => Out::Failed(format!("{e:#}")),
    }
}

/// THE WORK SIGNAL, as far as it may be believed.
#[derive(Clone, Debug)]
pub struct Signal {
    pub open: u64,
    pub urgent: u64,
    pub engine: String,
}

/// `v` read at `server_now` (ms by the server's clock): `None` when it is not
/// v1, older than `SIGNAL_STALE_MS`, or says `mode: "ask"` — then an idle id
/// syncs directly (`FALLBACK_MS`). Its poll and jitter dials are taken.
pub fn signal(v: &Value, server_now: u64, dials: &mut Dials) -> Option<Signal> {
    if v["v"] != json!(1) || v["mode"] == json!("ask") {
        return None;
    }
    let at = v["at"].as_str().and_then(crate::sys::parse_iso)?;
    if server_now.saturating_sub(at) > SIGNAL_STALE_MS {
        return None;
    }
    let mut part = json!({});
    for k in ["poll_ms", "jitter_ms", "urgent_jitter_ms"] {
        part[k] = v[k].clone();
    }
    dials.take(&part);
    Some(Signal { open: v["open"].as_u64().unwrap_or(0), urgent: v["urgent"].as_u64().unwrap_or(0), engine: v["engine"].as_str().unwrap_or("").to_string() })
}

/// The wait after `failures` failed syncs in a row: a random part of 5 s, 10 s, 20 s, … 10 min.
pub fn backoff(failures: u32) -> Duration {
    let cap = (BACKOFF_MS << failures.saturating_sub(1).min(16)).min(BACKOFF_MAX_MS);
    Duration::from_millis((crate::sys::random_unit() * cap as f64) as u64)
}

/// `ms`, give or take a quarter (69-board-work.js `jittered`).
pub fn jittered(ms: u64) -> Duration {
    Duration::from_millis((ms as f64 * (0.75 + crate::sys::random_unit() * 0.5)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(q: &str, engine: &str, at: u64) -> Kept {
        Kept { question: q.into(), engine: engine.into(), at, work: 1_000, result: json!({ "lease": "l", "question": q, "score": 1.5, "metric":"kpm", "work":1000 }) }
    }

    #[test]
    fn a_result_stays_until_a_reply_names_it() {
        let mut j = Journal { verifier: "v".into(), ..Default::default() };
        j.keep(kept("a", "e1", 1));
        j.keep(kept("b", "e1", 2));
        j.keep(kept("c", "e1", 3));
        j.keep(kept("d", "e0", 0));
        let sent: Vec<String> = j.to_send("e1").iter().map(|k| k.question.clone()).collect();
        assert_eq!(sent, ["a", "b", "c"]);
        // Taken, dropped, and one the reply did not name (a reply cut short): that one stays.
        let r = json!({ "taken": ["a", "x"], "dropped": [{ "question": "b", "why": "not_held" }] });
        assert_eq!(j.settle(&sent, &r, 10), (1, 1_000, 1));
        let left: Vec<&str> = j.results.iter().map(|k| k.question.as_str()).collect();
        assert_eq!(left, ["c", "d"]);
        assert_eq!(j.ask_states(), ["a"]);
        assert_eq!(j.other_engine("e1").as_deref(), Some("e0"));
        // A state: confirmed is counted and let go; waiting stays.
        assert_eq!(j.states(&json!({ "states": [{ "question": "a", "state": "waiting" }] })), 0);
        assert_eq!(j.pending.len(), 1);
        assert_eq!(j.states(&json!({ "states": [{ "question": "a", "state": "confirmed" }] })), 1);
        assert!(j.pending.is_empty());
    }

    #[test]
    fn a_journal_reads_back() {
        let dir = std::env::temp_dir().join(format!("simfleet-test-{}", crate::sys::random_hex(4)));
        let v = crate::sys::random_hex(12);
        let mut j = Journal::load(&dir, &v);
        assert!(j.results.is_empty());
        j.keep(Kept { at: crate::sys::now_ms(), ..kept("q", "e", 0) });
        j.save(&dir).unwrap();
        let back = Journal::load(&dir, &v);
        assert_eq!(back.results.len(), 1);
        assert_eq!(back.results[0].result, j.results[0].result);
        let mut bad = kept("bad", "e", crate::sys::now_ms());
        bad.work = 230358922269743420;
        bad.result["work"] = json!(bad.work);
        bad.result["counts"] = json!({"gas":4607178437394868700u64});
        j.keep(bad.clone());j.save(&dir).unwrap();
        let mut back = Journal::load(&dir,&v);
        assert_eq!(back.to_send("e").len(),1);
        assert_eq!(back.kept_work(),1000);
        assert_eq!(back.quarantine_invalid(&dir).unwrap(),1);
        assert_eq!(back.results.len(),1);
        let evidence:Value=serde_json::from_slice(&std::fs::read(dir.join("result-diagnostics").join(Journal::path(&dir,&v).file_name().unwrap())).unwrap()).unwrap();
        assert_eq!(evidence["invalid_results"][0]["result"],bad.result);
        back.save(&dir).unwrap();
        assert_eq!(Journal::load(&dir,&v).results.len(),1);
        // Another id whose file has the same name: not its journal.
        let w = format!("{}{}", &v[..12], if &v[12..] == "000000000000" { "111111111111" } else { "000000000000" });
        assert!(Journal::load(&dir, &w).results.is_empty());
        j.results.clear();
        j.pace_ms = 2500.0;
        j.save(&dir).unwrap();
        assert_eq!(Journal::load(&dir, &v).pace_ms, 2500.0);
        j.pace_ms = 0.0;
        j.save(&dir).unwrap();
        assert!(!Journal::path(&dir, &v).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_signal_is_aged_by_the_server_s_clock() {
        let mut d = Dials::default();
        let s = json!({ "v": 1, "at": "2026-10-15T02:17:38.831Z", "open": 4, "urgent": 0, "engine": "e", "poll_ms": 15000, "mode": "signal" });
        let at = crate::sys::parse_iso("2026-10-15T02:17:38.831Z").unwrap();
        assert!(signal(&s, at + 60_000, &mut d).is_some_and(|x| x.open == 4));
        assert_eq!(d.poll_ms, 15_000);
        assert!(signal(&s, at + 91_000, &mut d).is_none());
        let mut ask = s.clone();
        ask["mode"] = json!("ask");
        assert!(signal(&ask, at, &mut d).is_none());
    }

    #[test]
    fn dials_are_the_server_s() {
        let mut d = Dials::default();
        d.take(&json!({ "pack8_ms": 300000, "sync_low_ms": 60000, "poll_ms": 0, "unknown": 5 }));
        assert_eq!((d.pack8_ms, d.sync_low_ms, d.poll_ms, d.flush_ms), (300_000, 60_000, 1_000, 600_000));
    }
}
