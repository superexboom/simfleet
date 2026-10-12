// SPDX-License-Identifier: AGPL-3.0-or-later
//! WFSim's endpoint dispatch (`wfsim_webapi::route_text`, what the wasm build's
//! `api()` calls) and its optimizer (`optimize()`) behind four C functions, so simfleet can run the engine
//! natively. The engine id is stamped in at build time by the build script.

use std::ffi::c_char;

/// Each thread its own heap (Cargo.toml): no lane waits on another's allocation.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const ENGINE_ID: &str = concat!(env!("SFE_ENGINE_ID"), "\0");

#[no_mangle]
pub extern "C" fn sfe_engine_id() -> *const c_char {
    ENGINE_ID.as_ptr() as *const c_char
}

/// `api(path, body)` as the wasm build has it; the answer is owned by the
/// caller until it hands it to `sfe_free`. Null if the engine panicked.
///
/// # Safety
/// `path`/`body` must point at `*_len` readable bytes; `out_len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn sfe_api(path: *const u8, path_len: usize, body: *const u8, body_len: usize, out_len: *mut usize) -> *mut u8 {
    let path = String::from_utf8_lossy(std::slice::from_raw_parts(path, path_len)).to_string();
    let body = String::from_utf8_lossy(std::slice::from_raw_parts(body, body_len)).to_string();
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
/// and work). WFSim's code is used as committed.
///
/// # Safety
/// As `sfe_api`.
#[no_mangle]
pub unsafe extern "C" fn sfe_optimize(body: *const u8, body_len: usize, out_len: *mut usize) -> *mut u8 {
    let body = String::from_utf8_lossy(std::slice::from_raw_parts(body, body_len)).to_string();
    let r = std::panic::catch_unwind(move || {
        let mut v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("threads".into(), serde_json::json!(1));
        }
        let plan = match wfsim_webapi::parse_optimize(&v) {
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
