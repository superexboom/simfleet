// SPDX-License-Identifier: AGPL-3.0-or-later
//! Driver-only CUDA loading. Never feed PTX to the driver's unbounded JIT.
use anyhow::{bail, ensure, Context, Result};
use libloading::Library;
use std::ffi::{c_void, CString};
use std::path::Path;

type Handle = *mut c_void;
type Launch = unsafe extern "system" fn(
    Handle,
    u32,
    u32,
    u32,
    u32,
    u32,
    u32,
    u32,
    Handle,
    *mut Handle,
    Handle,
) -> i32;

pub struct Cuda {
    lib: Library,
    context: Handle,
    module: Handle,
    buffers: Vec<(u64, usize)>,
    pub name: String,
    copy_up: unsafe extern "system" fn(u64, *const c_void, usize) -> i32,
    copy_down: unsafe extern "system" fn(*mut c_void, u64, usize) -> i32,
    alloc: unsafe extern "system" fn(*mut u64, usize) -> i32,
    free: unsafe extern "system" fn(u64) -> i32,
    launch: Launch,
    sync: unsafe extern "system" fn() -> i32,
    function: unsafe extern "system" fn(*mut Handle, Handle, *const i8) -> i32,
    zero: unsafe extern "system" fn(u64, u8, usize) -> i32,
}
fn check(n: i32) -> Result<()> {
    ensure!(n == 0, "CUDA driver error {n}");
    Ok(())
}
#[derive(Debug)]
pub struct DriverError {
    pub code: i32,
    pub operation: &'static str,
    pub detail: String,
}
impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CUDA driver error {}{} at {}{}",
            self.code,
            if self.code == 2 {
                " (CUDA_ERROR_OUT_OF_MEMORY)"
            } else {
                ""
            },
            self.operation,
            self.detail
        )
    }
}
impl std::error::Error for DriverError {}
fn check_at(code: i32, operation: &'static str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(DriverError {
            code,
            operation,
            detail: String::new(),
        }
        .into())
    }
}
pub fn architecture() -> Result<String> {
    unsafe {
        let system = std::env::var_os("SystemRoot").context("SystemRoot")?;
        let lib = Library::new(Path::new(&system).join("System32/nvcuda.dll"))?;
        let init: unsafe extern "system" fn(u32) -> i32 = *lib.get(b"cuInit\0")?;
        let device: unsafe extern "system" fn(*mut i32, i32) -> i32 = *lib.get(b"cuDeviceGet\0")?;
        let attr: unsafe extern "system" fn(*mut i32, i32, i32) -> i32 =
            *lib.get(b"cuDeviceGetAttribute\0")?;
        check(init(0))?;
        let (mut dev, mut major, mut minor) = (0, 0, 0);
        check(device(&mut dev, 0))?;
        check(attr(&mut major, 75, dev))?;
        check(attr(&mut minor, 76, dev))?;
        Ok(format!("compute_{major}{minor}"))
    }
}
impl Cuda {
    pub fn memory_info(&self) -> Result<(usize, usize)> {
        unsafe {
            let f: unsafe extern "system" fn(*mut usize, *mut usize) -> i32 =
                *self.lib.get(b"cuMemGetInfo_v2\0")?;
            let (mut free, mut total) = (0, 0);
            check(f(&mut free, &mut total))?;
            Ok((free, total))
        }
    }
    pub fn driver_version(&self) -> Result<i32> {
        unsafe {
            let f = *self
                .lib
                .get::<unsafe extern "system" fn(*mut i32) -> i32>(b"cuDriverGetVersion\0")?;
            let mut version = 0;
            check(f(&mut version))?;
            Ok(version)
        }
    }
    pub fn open(image: &Path, architecture: &str) -> Result<Self> {
        let bytes = std::fs::read(image)?;
        ensure!(
            bytes.starts_with(b"\x7fELF"),
            "GPU package must contain a precompiled cubin"
        );
        unsafe {
            // Load only the installed system driver, never a DLL beside a package.
            let system = std::env::var_os("SystemRoot").context("SystemRoot")?;
            let lib = Library::new(Path::new(&system).join("System32/nvcuda.dll"))?;
            let init: unsafe extern "system" fn(u32) -> i32 = *lib.get(b"cuInit\0")?;
            let device: unsafe extern "system" fn(*mut i32, i32) -> i32 =
                *lib.get(b"cuDeviceGet\0")?;
            let attr: unsafe extern "system" fn(*mut i32, i32, i32) -> i32 =
                *lib.get(b"cuDeviceGetAttribute\0")?;
            let name: unsafe extern "system" fn(*mut i8, i32, i32) -> i32 =
                *lib.get(b"cuDeviceGetName\0")?;
            let create: unsafe extern "system" fn(*mut Handle, u32, i32) -> i32 =
                *lib.get(b"cuCtxCreate_v2\0")?;
            let mut gpu = Self {
                context: std::ptr::null_mut(),
                module: std::ptr::null_mut(),
                buffers: Vec::new(),
                name: String::new(),
                copy_up: *lib.get(b"cuMemcpyHtoD_v2\0")?,
                copy_down: *lib.get(b"cuMemcpyDtoH_v2\0")?,
                alloc: *lib.get(b"cuMemAlloc_v2\0")?,
                free: *lib.get(b"cuMemFree_v2\0")?,
                launch: *lib.get(b"cuLaunchKernel\0")?,
                sync: *lib.get(b"cuCtxSynchronize\0")?,
                function: *lib.get(b"cuModuleGetFunction\0")?,
                zero: *lib.get(b"cuMemsetD8_v2\0")?,
                lib,
            };
            check(init(0))?;
            let mut dev = 0;
            check(device(&mut dev, 0))?;
            let (mut major, mut minor) = (0, 0);
            check(attr(&mut major, 75, dev))?;
            check(attr(&mut minor, 76, dev))?;
            ensure!(
                architecture == format!("compute_{major}{minor}"),
                "GPU architecture is sm_{major}{minor}; package is {architecture}"
            );
            let mut label = [0i8; 256];
            check(name(label.as_mut_ptr(), 256, dev))?;
            gpu.name = std::ffi::CStr::from_ptr(label.as_ptr())
                .to_string_lossy()
                .into_owned();
            // CU_CTX_SCHED_BLOCKING_SYNC: the CPU thread sleeps during GPU work.
            check_at(create(&mut gpu.context, 4, dev), "cuCtxCreate_v2")?;
            let limit: unsafe extern "system" fn(i32, usize) -> i32 =
                *gpu.lib.get(b"cuCtxSetLimit\0")?;
            check_at(limit(0, 16384), "cuCtxSetLimit(stack)")?;
            let load: unsafe extern "system" fn(*mut Handle, *const c_void) -> i32 =
                *gpu.lib.get(b"cuModuleLoadData\0")?;
            check_at(
                load(&mut gpu.module, bytes.as_ptr().cast()),
                "cuModuleLoadData",
            )?;
            Ok(gpu)
        }
    }
    /// A fixed number of slots retain their bounded maximum, never one allocation per row.
    pub fn buffer(&mut self, slot: usize, bytes: usize) -> Result<u64> {
        ensure!(
            slot < 32 && bytes > 0 && bytes <= 4usize << 30,
            "GPU allocation outside budget"
        );
        self.buffers
            .resize(self.buffers.len().max(slot + 1), (0, 0));
        if self.buffers[slot].1 < bytes {
            let (old, _) = std::mem::replace(&mut self.buffers[slot], (0, 0));
            if old != 0 {
                unsafe {
                    check_at((self.free)(old), "cuMemFree_v2")?;
                }
            }
            let mut ptr = 0;
            unsafe {
                let code = (self.alloc)(&mut ptr, bytes);
                if code != 0 {
                    let memory = self.memory_info().ok();
                    let retained = self.buffers.iter().map(|(_, n)| *n).sum::<usize>();
                    return Err(DriverError{code,operation:"cuMemAlloc_v2",detail:format!("; slot={slot}, requested={bytes}, free={}, total={}, retained={retained}",memory.map_or(0,|m|m.0),memory.map_or(0,|m|m.1))}.into());
                }
            }
            self.buffers[slot] = (ptr, bytes);
        }
        Ok(self.buffers[slot].0)
    }
    pub fn upload<T>(&self, ptr: u64, data: &[T]) -> Result<()> {
        unsafe {
            check_at(
                (self.copy_up)(ptr, data.as_ptr().cast(), std::mem::size_of_val(data)),
                "cuMemcpyHtoD_v2",
            )
        }
    }
    pub fn download<T>(&self, data: &mut [T], ptr: u64) -> Result<()> {
        unsafe {
            check_at(
                (self.copy_down)(data.as_mut_ptr().cast(), ptr, std::mem::size_of_val(data)),
                "cuMemcpyDtoH_v2",
            )
        }
    }
    pub fn clear(&self, ptr: u64, bytes: usize) -> Result<()> {
        unsafe { check_at((self.zero)(ptr, 0, bytes), "cuMemsetD8_v2") }
    }
    pub fn run(&self, name: &str, count: u32, block: u32, args: &mut [Handle]) -> Result<()> {
        let name = CString::new(name)?;
        let mut fun = std::ptr::null_mut();
        unsafe {
            check_at(
                (self.function)(&mut fun, self.module, name.as_ptr()),
                "cuModuleGetFunction",
            )?;
            check_at(
                (self.launch)(
                    fun,
                    count.div_ceil(block),
                    1,
                    1,
                    block,
                    1,
                    1,
                    0,
                    std::ptr::null_mut(),
                    args.as_mut_ptr(),
                    std::ptr::null_mut(),
                ),
                "cuLaunchKernel",
            )?;
            check_at((self.sync)(), "cuCtxSynchronize")
        }
    }
}
impl Drop for Cuda {
    fn drop(&mut self) {
        unsafe {
            for (ptr, _) in self.buffers.drain(..) {
                if ptr != 0 {
                    (self.free)(ptr);
                }
            }
            if !self.module.is_null() {
                if let Ok(f) = self
                    .lib
                    .get::<unsafe extern "system" fn(Handle) -> i32>(b"cuModuleUnload\0")
                {
                    f(self.module);
                }
            }
            if !self.context.is_null() {
                if let Ok(f) = self
                    .lib
                    .get::<unsafe extern "system" fn(Handle) -> i32>(b"cuCtxDestroy_v2\0")
                {
                    f(self.context);
                }
            }
        }
    }
}

/// Enforced before loading CUDA or WASM, including allocator and driver commit.
pub fn memory_job(mib: u64) -> Result<(impl Drop, usize)> {
    use windows_sys::Win32::{
        Foundation::CloseHandle, System::JobObjects::*, System::Threading::GetCurrentProcess,
    };
    struct Job(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    ensure!(
        (4096..=65536).contains(&mib),
        "GPU memory limit must be 4096..=65536 MiB"
    );
    unsafe {
        let job = Job(CreateJobObjectW(std::ptr::null(), std::ptr::null()));
        ensure!(!job.0.is_null(), "GPU memory job creation failed");
        let mut limit: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limit.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        // The parent Job enforces the aggregate ceiling; this child must not
        // silently keep an unrelated fixed 8-GiB cap when users raise it.
        limit.ProcessMemoryLimit = ((mib - 64) as usize) << 20;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limit as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limit) as u32,
        ) == 0
            || AssignProcessToJobObject(job.0, GetCurrentProcess()) == 0
        {
            bail!("GPU memory job: {}", std::io::Error::last_os_error());
        }
        let mut actual: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        ensure!(
            QueryInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&mut actual as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&actual) as u32,
                std::ptr::null_mut()
            ) != 0,
            "GPU memory limit query failed"
        );
        Ok((job, actual.ProcessMemoryLimit))
    }
}

pub fn device_lock() -> Result<impl Drop> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0},
        System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    };
    struct Lock(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Lock {
        fn drop(&mut self) {
            unsafe {
                ReleaseMutex(self.0);
                CloseHandle(self.0);
            }
        }
    }
    let name: Vec<u16> = "Local\\simfleet-gpu-device-0"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        ensure!(!handle.is_null(), "GPU device mutex creation failed");
        if !matches!(
            WaitForSingleObject(handle, 0),
            WAIT_OBJECT_0 | WAIT_ABANDONED
        ) {
            CloseHandle(handle);
            bail!("another simfleet already owns GPU 0");
        }
        Ok(Lock(handle))
    }
}
