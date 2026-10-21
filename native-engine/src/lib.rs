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
    u32::from(cfg!(feature = "fast-board")) | (u32::from(option_env!("SFE_INLINE_OPTIMIZER") == Some("1")) << 1)
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
        let plan = match optimize_plan(v) {
            Ok(p) => p,
            Err(e) => return e.to_string(),
        };
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
