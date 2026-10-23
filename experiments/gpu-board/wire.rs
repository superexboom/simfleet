// SPDX-License-Identifier: AGPL-3.0-or-later
//! Decode GPU shards and perform the same ordered native fold and board score.
use serde_json::{json, Value};
use wfsim_engine::fight::Shard;
#[path = "../../native-engine/src/exact.rs"]
mod exact;
#[path = "../../src/json_numbers.rs"]
mod json_numbers;

unsafe fn output(result: impl FnOnce() -> anyhow::Result<Value> + std::panic::UnwindSafe, len: *mut usize) -> *mut u8 {
    let value = match std::panic::catch_unwind(result) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({"ok":false,"error":e.to_string()}),
        Err(_) => json!({"ok":false,"error":"native wire decoder panicked"}),
    };
    let bytes = serde_json::to_vec(&value).unwrap().into_boxed_slice();
    *len = bytes.len();
    Box::into_raw(bytes) as *mut u8
}

/// # Safety
/// data/size and out_len are live readable/writable buffers, respectively.
#[no_mangle]
pub unsafe extern "C" fn sf_wire_decode(data: *const u8, size: usize, out_len: *mut usize) -> *mut u8 {
    output(|| {
        anyhow::ensure!(size <= 65536, "oversized shard");
        let shard: Shard = postcard::from_bytes(std::slice::from_raw_parts(data, size))?;
        Ok(serde_json::to_value(exact::carry(shard)?)?)
    }, out_len)
}

/// # Safety
/// request/request_len is JSON. data has data_len bytes, sizes has count u32s;
/// each shard starts at i*stride. out_len points to writable storage.
#[no_mangle]
pub unsafe extern "C" fn sf_wire_score(request: *const u8, request_len: usize, data: *const u8,
    data_len: usize, sizes: *const u32, count: u32, stride: u32, out_len: *mut usize) -> *mut u8 {
    output(|| {
        anyhow::ensure!(count > 0 && count <= 1_000_000 && stride <= 65536, "invalid packet shape");
        anyhow::ensure!(u64::from(count) * u64::from(stride) <= data_len as u64, "truncated packet");
        let v: Value = serde_json::from_slice(std::slice::from_raw_parts(request, request_len))?;
        anyhow::ensure!(v["request"]["runs"].as_u64() == Some(u64::from(count)), "incomplete row");
        let bytes = std::slice::from_raw_parts(data, data_len);
        let sizes = std::slice::from_raw_parts(sizes, count as usize);
        let mut acc = Shard::default();
        for (i, &size) in sizes.iter().enumerate() {
            anyhow::ensure!(size <= stride, "oversized shard");
            let start = i * stride as usize;
            let piece: Shard = postcard::from_bytes(&bytes[start..start + size as usize])?;
            acc.merge(&exact::carry(piece)?);
            if i % 64 == 63 { exact::carry_acc(&mut acc)?; }
        }
        exact::carry_acc(&mut acc)?;
        Ok(wfsim_webapi::board_rows::board_score_json(&json!({"request":v["request"],"ruler":v["ruler"],"acc":acc})))
    }, out_len)
}

/// # Safety
/// ptr/len is exactly one unfreed output allocated by this library.
#[no_mangle]
pub unsafe extern "C" fn sf_wire_free(ptr: *mut u8, len: usize) {
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
}
