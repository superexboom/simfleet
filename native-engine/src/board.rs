// SPDX-License-Identifier: AGPL-3.0-or-later
//! An opaque row, owned by the caller until every job using it has ended.
//! Immutable preparation is initialized on a compute lane. Shards stay in the
//! DLL and merge in run order; only a progress integer and final score cross
//! the ABI. No Rust layout or allocator ownership crosses the library boundary.
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use wfsim_engine::fight::Shard;
use wfsim_webapi::simfleet::PreparedBoard;

struct Row {
    request: Value,
    ruler: String,
    runs: u32,
    prepared: OnceLock<Result<PreparedBoard, String>>,
    state: Mutex<State>,
}
struct State {
    claimed: Vec<bool>,
    ready: BTreeMap<u32, Vec<Shard>>,
    acc: Option<Shard>,
    next: u32,
    failed: bool,
}

#[no_mangle]
pub extern "C" fn sfe_board_abi() -> u32 {
    1
}

/// # Safety
/// `body` contains `len` readable bytes. Returned context belongs to this DLL.
#[no_mangle]
pub unsafe extern "C" fn sfe_board_new(body: *const u8, len: usize) -> *mut c_void {
    std::panic::catch_unwind(|| {
        let v: Value = serde_json::from_slice(std::slice::from_raw_parts(body, len)).ok()?;
        let request = v.get("request")?.clone();
        let runs = u32::try_from(request.get("runs")?.as_u64()?).ok()?;
        if runs == 0 || runs > 1_000_000 {
            return None;
        }
        let row = Row {
            ruler: v.get("ruler")?.as_str()?.into(),
            request,
            runs,
            prepared: OnceLock::new(),
            state: Mutex::new(State { claimed: vec![false; runs as usize], ready: BTreeMap::new(), acc: None, next: 0, failed: false }),
        };
        Some(Box::into_raw(Box::new(row)).cast())
    })
    .ok()
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

impl Row {
    fn run(&self, from: u32, count: u32) -> anyhow::Result<u32> {
        let end = from.checked_add(count).filter(|&n| count > 0 && n <= self.runs).ok_or_else(|| anyhow::anyhow!("range"))?;
        {
            let mut s = self.state.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
            anyhow::ensure!(!s.failed && !s.claimed[from as usize..end as usize].iter().any(|&v| v), "duplicate or failed");
            s.claimed[from as usize..end as usize].fill(true);
        }
        let prepared = self.prepared.get_or_init(|| PreparedBoard::new(&self.request).map_err(|e| e.to_string()));
        let p = prepared.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
        let pieces = (from..end).map(|i| crate::exact::carry(p.run(i))).collect::<anyhow::Result<Vec<_>>>()?;
        let mut s = self.state.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        s.ready.insert(from, pieces);
        loop {
            let next = s.next;
            let Some(pieces) = s.ready.remove(&next) else { break };
            for piece in pieces {
                s.acc.get_or_insert_with(Shard::default).merge(&piece);
                s.next += 1;
            }
        }
        if let Some(acc) = &mut s.acc {
            crate::exact::carry_acc(acc)?;
        }
        Ok(s.next)
    }
}

/// # Safety
/// Context comes from `sfe_board_new`, remains alive during every call. Calls
/// may overlap on disjoint ranges. A failure invalidates the whole row.
#[no_mangle]
pub unsafe extern "C" fn sfe_board_runs(ctx: *mut c_void, from: u32, count: u32) -> i64 {
    let row = &*ctx.cast::<Row>();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| row.run(from, count))) {
        Ok(Ok(n)) => i64::from(n),
        _ => {
            if let Ok(mut s) = row.state.lock() {
                s.failed = true;
            }
            -1
        }
    }
}

/// # Safety
/// Same live context, with every submitted call completed. Free output with
/// `sfe_free`, context separately with `sfe_board_drop`.
#[no_mangle]
pub unsafe extern "C" fn sfe_board_score(ctx: *mut c_void, len: *mut usize) -> *mut u8 {
    let row = &*ctx.cast::<Row>();
    super::hand_out(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let s = row.state.lock().expect("row state");
            if s.failed || s.next != row.runs {
                return json!({"ok":false,"error":"incomplete native row"}).to_string();
            }
            wfsim_webapi::board_rows::board_score_json(&json!({"request":row.request,"ruler":row.ruler,"acc":s.acc})).to_string()
        })),
        len,
    )
}

/// # Safety
/// Owns `ctx`; no call may use it afterward or concurrently with this call.
#[no_mangle]
pub unsafe extern "C" fn sfe_board_drop(ctx: *mut c_void) {
    if !ctx.is_null() {
        drop(Box::from_raw(ctx.cast::<Row>()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Value {
        json!({"weapon":"braton_prime","mods":["serration","split_chamber","point_strike","vital_sense"],
            "enemy":"corrupted_heavy_gunner","level":100,"steel_path":true,"duration":0.5,"runs":35,"seed":12345})
    }
    fn reference(req: &Value) -> Value {
        let pieces = wfsim_webapi::board_rows::board_runs_json(&json!({"request":req,"from":0,"count":35}));
        let fold = wfsim_webapi::board_rows::board_fold_json(&json!({"request":req,"acc":null,"pieces":pieces["shards"]}));
        wfsim_webapi::board_rows::board_score_json(&json!({"request":req,"ruler":"demolisher","acc":fold["acc"]}))
    }
    #[test]
    fn native_context_parallel_reverse_pieces_match_the_wire() {
        let req = request();
        let want = reference(&req);
        assert_eq!(want["ok"], json!(true));
        for width in [1u32, 7, 35] {
            let body = json!({"request":req,"ruler":"demolisher"}).to_string();
            let ptr = unsafe { sfe_board_new(body.as_ptr(), body.len()) };
            assert!(!ptr.is_null());
            let address = ptr as usize;
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for from in (0..35).step_by(width as usize).rev() {
                    handles.push(scope.spawn(move || unsafe { sfe_board_runs(address as *mut c_void, from, width.min(35 - from)) }));
                }
                for h in handles {
                    assert!(h.join().unwrap() >= 0);
                }
            });
            unsafe {
                let mut len = 0;
                let out = sfe_board_score(ptr, &mut len);
                assert!(!out.is_null());
                let have: Value = serde_json::from_slice(std::slice::from_raw_parts(out, len)).unwrap();
                super::super::sfe_free(out, len);
                sfe_board_drop(ptr);
                assert_eq!(have, want);
            }
        }
    }
    #[test]
    fn duplicate_or_incomplete_work_never_scores() {
        let body = json!({"request":request(),"ruler":"demolisher"}).to_string();
        unsafe {
            let ptr = sfe_board_new(body.as_ptr(), body.len());
            assert!(!ptr.is_null());
            assert_eq!(sfe_board_runs(ptr, 0, 7), 7);
            let mut len = 0;
            let out = sfe_board_score(ptr, &mut len);
            let result: Value = serde_json::from_slice(std::slice::from_raw_parts(out, len)).unwrap();
            super::super::sfe_free(out, len);
            assert_eq!(result["ok"], json!(false));
            assert_eq!(sfe_board_runs(ptr, 0, 7), -1);
            assert_eq!(sfe_board_runs(ptr, 7, 28), -1);
            sfe_board_drop(ptr);
        }
    }
}
