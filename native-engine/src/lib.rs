// SPDX-License-Identifier: AGPL-3.0-or-later
//! WFSim's endpoint dispatch (`wfsim_webapi::route`, what the wasm build's
//! `api()` calls) behind three C functions, so simfleet can run the engine
//! natively. The engine id is stamped in at build time by the build script.

use std::ffi::c_char;

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
    let r = std::panic::catch_unwind(move || {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        wfsim_webapi::route(&path, &v)
            .unwrap_or_else(|| wfsim_webapi::err_json(format!("unknown endpoint: {path}")))
            .to_string()
    });
    match r {
        Ok(s) => {
            let mut b = s.into_bytes().into_boxed_slice();
            *out_len = b.len();
            let p = b.as_mut_ptr();
            std::mem::forget(b);
            p
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// `p`/`len` must be exactly what `sfe_api` returned.
#[no_mangle]
pub unsafe extern "C" fn sfe_free(p: *mut u8, len: usize) {
    if !p.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, len)));
    }
}
