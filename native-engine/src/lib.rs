// SPDX-License-Identifier: AGPL-3.0-or-later
//! WFSim's endpoint dispatch (`wfsim_webapi::route_text`, what the wasm build's
//! `api()` calls) and its optimizer (`optimize()`) behind four C functions, so simfleet can run the engine
//! natively. The engine id is stamped in at build time by the build script.

use std::ffi::c_char;

#[cfg(feature = "cached-optimize")]
thread_local! {
    // At most one request shape per lane. No task data is shared between
    // leaders. Thread retirement releases this and the upstream search state.
    static PLAN: std::cell::RefCell<Option<(serde_json::Value, wfsim_webapi::OptimizePlan)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "cached-optimize")]
fn optimize_plan(mut v: serde_json::Value) -> Result<wfsim_webapi::OptimizePlan, serde_json::Value> {
    if v.get("quick_fleet").is_none() {
        return wfsim_webapi::parse_optimize(&v);
    }
    let fleet = v.as_object_mut().and_then(|o| o.remove("quick_fleet")).unwrap_or_default();
    let shape = v;
    PLAN.with(|cache| {
        let mut cache = cache.borrow_mut();
        if fleet.get("fresh").and_then(serde_json::Value::as_bool) == Some(true) || cache.as_ref().is_none_or(|(key, _)| *key != shape) {
            let p = wfsim_webapi::parse_optimize(&shape)?;
            *cache = Some((shape, p));
        }
        Ok(cache.as_ref().expect("prepared").1.for_fleet(fleet))
    })
}
#[cfg(not(feature = "cached-optimize"))]
fn optimize_plan(v: serde_json::Value) -> Result<wfsim_webapi::OptimizePlan, serde_json::Value> {
    wfsim_webapi::parse_optimize(&v)
}

#[cfg(feature = "split-finals")]
mod finals {
    use serde_json::{json, Value};
    use std::cell::{Cell, RefCell};
    use wfsim_webapi::{FinalRound, FinalScore};

    thread_local! {
        static GENERATION: Cell<u32> = const { Cell::new(0) };
        static ROUND: RefCell<Option<(u32, FinalRound)>> = const { RefCell::new(None) };
    }

    pub fn clear() {
        ROUND.with(|r| r.borrow_mut().take());
    }

    pub fn dispatch(round: FinalRound) -> Value {
        let id = GENERATION.with(|g| {
            let next = g.get().wrapping_add(1);
            g.set(next);
            next
        });
        let runs = round.replays().next().expect("multiple finalists")["runs"]
            .as_u64()
            .expect("replay runs");
        let requests: Vec<String> = round.replays().map(Value::to_string).collect();
        ROUND.with(|r| r.replace(Some((id, round))));
        json!({ "ok": true, "verify": requests, "generation": id, "runs": runs })
    }

    pub fn verify(generation: u32, index: u32, replay: &str) -> String {
        let replay: Value = match serde_json::from_str(replay) {
            Ok(v) => v,
            Err(_) => return String::new(),
        };
        let score = FinalScore::simulate(&replay, &mut |_, _| {});
        serde_json::to_string(&(generation, index, score)).expect("finalist receipt")
    }

    fn collect(generation: u32, count: usize, receipts: &str) -> Option<Vec<FinalScore>> {
        let raw: Vec<String> = serde_json::from_str(receipts).ok()?;
        if raw.len() != count {
            return None;
        }
        let mut slots: Vec<_> = (0..count).map(|_| None).collect();
        for receipt in raw {
            let (id, index, score): (u32, usize, FinalScore) = serde_json::from_str(&receipt).ok()?;
            if id != generation || slots.get_mut(index)?.replace(score).is_some() {
                return None;
            }
        }
        slots.into_iter().collect()
    }

    pub fn finish(generation: u32, receipts: &str) -> String {
        ROUND
            .with(|r| {
                let mut r = r.borrow_mut();
                if r.as_ref().is_none_or(|(id, _)| *id != generation) {
                    return wfsim_webapi::err_json("stale finalist round");
                }
                let (_, round) = r.take().expect("checked round");
                let count = round.replays().len();
                match collect(generation, count, receipts) {
                    Some(scores) => round.measured(scores),
                    None => wfsim_webapi::err_json("invalid or incomplete finalist receipts"),
                }
            })
            .to_string()
    }
}

#[cfg(feature = "fast-board")]
mod board;
#[cfg(feature = "fast-board")]
mod exact;
#[cfg(feature = "fast-board")]
#[path = "../../src/json_numbers.rs"]
mod json_numbers;

/// Each thread its own heap (Cargo.toml): no lane waits on another's allocation.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const ENGINE_ID: &str = concat!(env!("SFE_ENGINE_ID"), "\0");

#[no_mangle]
pub extern "C" fn sfe_engine_id() -> *const c_char {
    ENGINE_ID.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn sfe_capabilities() -> u32 {
    u32::from(cfg!(feature = "fast-board"))
        | (u32::from(option_env!("SFE_INLINE_OPTIMIZER") == Some("1")) << 1)
        | (u32::from(cfg!(feature = "split-finals")) << 2)
}

/// `api(path, body)` as the wasm build has it; the answer is owned by the
/// caller until it hands it to `sfe_free`. Null if the engine panicked.
///
/// # Safety
/// `path`/`body` must point at `*_len` readable bytes; `out_len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn sfe_api(path: *const u8, path_len: usize, body: *const u8, body_len: usize, out_len: *mut usize) -> *mut u8 {
    let path = String::from_utf8_lossy(std::slice::from_raw_parts(path, path_len));
    let body = String::from_utf8_lossy(std::slice::from_raw_parts(body, body_len));
    // The wasm module's `api` is this very call (wasm/src: `route_text`).
    let r = std::panic::catch_unwind(move || wfsim_webapi::route_text(&path, &body));
    hand_out(r, out_len)
}

/// `optimize(body)` as the wasm build has it — a riven gain's search, a
/// `quick_fleet` lead or score step among them — called on one thread for a
/// search's whole life: a lead's state between steps is the thread's own, as
/// a Web Worker's is its instance's. The answer is the wasm's string, owned
/// as `sfe_api`'s is.
///
/// ONE WORKER (`threads: 1`), as a request may ask: the optimizer evaluates a
/// batch as wide as four a worker, at least 16, and the wasm build's is 16 —
/// so with one worker the search is the browser fleet's, step for step (the
/// seeds never depended on the workers). The field rides into a result row's
/// replay request, and nowhere into a riven gain's answer (its build, score
/// and work). Local patches preserve the candidate order and run stream.
///
/// # Safety
/// As `sfe_api`.
#[no_mangle]
pub unsafe extern "C" fn sfe_optimize(body: *const u8, body_len: usize, out_len: *mut usize) -> *mut u8 {
    let body = String::from_utf8_lossy(std::slice::from_raw_parts(body, body_len));
    let r = std::panic::catch_unwind(move || {
        let mut v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("threads".into(), serde_json::json!(1));
        }
        #[cfg(feature = "split-finals")]
        let (lead, fresh, split_finals) = (
            v["quick_fleet"]["lead"] == true,
            v["quick_fleet"]["fresh"] == true,
            v["quick_fleet"]["split_finals"] == true,
        );
        #[allow(unused_mut)]
        let mut plan = match optimize_plan(v) {
            Ok(p) => p,
            Err(e) => return e.to_string(),
        };
        #[cfg(feature = "split-finals")]
        if lead {
            if fresh {
                finals::clear();
            }
            if split_finals {
                plan.dispatch_finals = Some(finals::dispatch);
            }
        }
        // THE WASM BUILD'S CALL (wasm/src/lib.rs `optimize_inner`), its
        // observers here doing nothing: what they see is display only.
        let state = wfsim_optimizer::FunnelState::default();
        wfsim_webapi::run_optimize_resumable(
            plan,
            &state,
            |_, _| {},
            Some(&|| {}),
            None,
            Some(&|_: usize, _: usize, _: &[wfsim_webapi::JobIdentity], _: &serde_json::Value| {}),
            Some(&|_: &serde_json::Value| {}),
        )
        .to_string()
    });
    hand_out(r, out_len)
}

#[cfg(feature = "split-finals")]
#[no_mangle]
pub unsafe extern "C" fn sfe_verify_final(
    generation: u32,
    index: u32,
    replay: *const u8,
    replay_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let replay = String::from_utf8_lossy(std::slice::from_raw_parts(replay, replay_len));
    let r = std::panic::catch_unwind(move || finals::verify(generation, index, &replay));
    hand_out(r, out_len)
}

#[cfg(feature = "split-finals")]
#[no_mangle]
pub unsafe extern "C" fn sfe_finish_finals(
    generation: u32,
    receipts: *const u8,
    receipts_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let receipts = String::from_utf8_lossy(std::slice::from_raw_parts(receipts, receipts_len));
    let r = std::panic::catch_unwind(move || finals::finish(generation, &receipts));
    hand_out(r, out_len)
}

fn hand_out(r: std::thread::Result<String>, out_len: *mut usize) -> *mut u8 {
    match r {
        Ok(s) => {
            let mut b = s.into_bytes().into_boxed_slice();
            // SAFETY: the caller's out-parameter.
            unsafe { *out_len = b.len() };
            let p = b.as_mut_ptr();
            std::mem::forget(b);
            p
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// `p`/`len` must be exactly what `sfe_api` or `sfe_optimize` returned.
#[no_mangle]
pub unsafe extern "C" fn sfe_free(p: *mut u8, len: usize) {
    if !p.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, len)));
    }
}

#[cfg(all(test, feature="cached-optimize"))]
mod tests {
    use super::*;
    #[test]
    fn frozen_plan_is_replaced_for_new_shapes_and_bad_requests() {
        let run=|weapon:&str| {
            let req=serde_json::json!({"weapon":weapon,"enemy":"thrax_centurion","level":100,"duration":0.2,"runs":2,"strategy":"quick","candidate_runs":2,
                "quick_fleet":{"lead":true,"fresh":true,"scores":[]},"threads":1});
            let plan=optimize_plan(req.clone()).expect("plan");
            let fresh=wfsim_webapi::parse_optimize(&req).expect("reference");
            let evaluate=|p| {
                let state=wfsim_optimizer::FunnelState::default();
                wfsim_webapi::run_optimize_resumable(p,&state,|_,_|{},None,None,None,None)
            };
            assert_eq!(evaluate(plan),evaluate(fresh));
        };
        run("braton_prime");run("torid");run("braton_prime");
        assert!(optimize_plan(serde_json::json!({"weapon":"missing","quick_fleet":{"score":[]}})).is_err());
        run("braton_prime");
        PLAN.with(|p|p.borrow_mut().take());
    }
}
