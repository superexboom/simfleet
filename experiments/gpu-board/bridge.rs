// SPDX-License-Identifier: AGPL-3.0-or-later
//! A small WASM boundary around the original prepared board simulator.
//! Preparation runs on the host. Each GPU instance starts from that snapshot.
use serde_json::Value;
use wfsim_webapi::simfleet::PreparedBoard;

#[no_mangle]
pub extern "C" fn sf_alloc(len: u32) -> u32 {
    Box::into_raw(vec![0u8; len as usize].into_boxed_slice()) as *mut u8 as u32
}

/// # Safety
/// ptr/len denotes the buffer allocated by sf_alloc or returned by sf_run.
#[no_mangle]
pub unsafe extern "C" fn sf_free(ptr: u32, len: u32) {
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr as *mut u8, len as usize)));
}

/// # Safety
/// ptr/len contains a live JSON request in this WASM instance.
#[no_mangle]
pub unsafe extern "C" fn sf_prepare(ptr: u32, len: u32) -> u32 {
    let value: Value = serde_json::from_slice(std::slice::from_raw_parts(ptr as *const u8, len as usize)).unwrap();
    Box::into_raw(Box::new(PreparedBoard::new(&value).unwrap())) as u32
}

/// # Safety
/// handle is a live sf_prepare handle in this instance. Return packs len:ptr.
#[no_mangle]
pub unsafe extern "C" fn sf_run(handle: u32, index: u32) -> u64 {
    let piece = (&*(handle as *const PreparedBoard)).run(index);
    #[cfg(not(feature = "binary"))]
    let data = serde_json::to_vec(&piece).unwrap().into_boxed_slice();
    #[cfg(feature = "binary")]
    let data = postcard::to_allocvec(&piece).unwrap().into_boxed_slice();
    let len = data.len() as u64;
    let ptr = Box::into_raw(data) as *mut u8 as u32;
    (len << 32) | u64::from(ptr)
}

#[no_mangle]
pub extern "C" fn sf_wire_format() -> u32 { u32::from(cfg!(feature = "binary")) }

#[no_mangle]
pub extern "C" fn sf_gas_probe(delta: u64) -> u64 {
    wfsim_engine::fight::gas::charge(delta);
    wfsim_engine::fight::gas::gas_now()
}

/// # Safety
/// handle is uniquely owned and no sf_run call is still using it.
#[no_mangle]
pub unsafe extern "C" fn sf_drop(handle: u32) {
    drop(Box::from_raw(handle as *mut PreparedBoard));
}
