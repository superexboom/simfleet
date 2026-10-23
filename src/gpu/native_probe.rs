// SPDX-License-Identifier: AGPL-3.0-or-later
//! Time the prepared CPU ABI without changing requests or scoring.
use anyhow::{ensure, Result};
use std::{ffi::c_void, path::Path, time::Instant};
type New = unsafe extern "C" fn(*const u8, usize) -> *mut c_void;
type Run = unsafe extern "C" fn(*mut c_void, u32, u32) -> i64;
type DropRow = unsafe extern "C" fn(*mut c_void);
pub struct Native {
    _lib: libloading::Library,
    new: New,
    run: Run,
    drop_row: DropRow,
}
impl Native {
    pub fn open(path: &Path, engine: &str) -> Result<Self> {
        unsafe {
            let lib = libloading::Library::new(path)?;
            let id = *lib.get::<unsafe extern "C" fn() -> *const i8>(b"sfe_engine_id\0")?;
            ensure!(
                std::ffi::CStr::from_ptr(id()).to_str()? == engine,
                "GPU CPU probe engine mismatch"
            );
            let abi = *lib.get::<unsafe extern "C" fn() -> u32>(b"sfe_board_abi\0")?;
            ensure!(abi() == 1, "CPU probe ABI mismatch");
            Ok(Self {
                new: *lib.get(b"sfe_board_new\0")?,
                run: *lib.get(b"sfe_board_runs\0")?,
                drop_row: *lib.get(b"sfe_board_drop\0")?,
                _lib: lib,
            })
        }
    }
    pub fn measure(&self, input: &super::Input, count: u32) -> Result<f64> {
        let body =
            serde_json::to_vec(&serde_json::json!({"request":input.request,"ruler":input.ruler}))?;
        unsafe {
            let ptr = (self.new)(body.as_ptr(), body.len());
            ensure!(!ptr.is_null(), "CPU probe preparation failed");
            let began = Instant::now();
            let result = (self.run)(ptr, 0, count);
            let elapsed = began.elapsed().as_secs_f64() * 1000.0;
            (self.drop_row)(ptr);
            ensure!(result >= 0, "CPU probe execution failed");
            Ok(elapsed.max(0.000001))
        }
    }
}
