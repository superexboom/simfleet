// SPDX-License-Identifier: AGPL-3.0-or-later
//! WHAT macOS SAYS, for sys.rs and affinity.rs — compiled for macOS only, so
//! nothing here is in a Windows or Linux build.
//!
//! - the person's last input (CoreGraphics), whether the power is a battery
//!   (IOKit), a hold on idle sleep while the lanes compute (IOKit);
//! - the processor's load and the memory in use (Mach), the clock they tick at;
//! - Apple silicon's two kinds of core (`hw.perflevel0` the performance cores,
//!   `hw.perflevel1` the efficiency cores) and the QoS class a thread asks for.
//!   macOS offers no affinity: a thread's QoS class is what steers it. A
//!   `background` thread runs on the efficiency cores alone; `utility` and
//!   above take the performance cores first and the efficiency cores too.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::Mutex;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
const UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event: u32) -> f64;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSGetProvidingPowerSourceType(blob: CFTypeRef) -> CFStringRef;
    fn IOPMAssertionCreateWithName(kind: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

extern "C" {
    fn mach_host_self() -> u32;
    static mach_task_self_: u32;
    fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
}

/// `kCGEventSourceStateCombinedSessionState`, `kCGAnyInputEventType`.
const COMBINED_SESSION: i32 = 0;
const ANY_INPUT: u32 = !0;

/// Milliseconds since the last keyboard or mouse input in this login session.
pub fn idle_ms() -> u64 {
    // SAFETY: a plain query; no window server (a remote shell) answers a huge
    // number, which reads as away.
    let s = unsafe { CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION, ANY_INPUT) };
    if s.is_finite() && s >= 0.0 { (s * 1000.0).min(u64::MAX as f64) as u64 } else { u64::MAX }
}

fn cf_string(s: &str) -> CFStringRef {
    let c = CString::new(s).unwrap_or_default();
    // SAFETY: a NUL-terminated UTF-8 string; the caller releases the answer.
    unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) }
}

/// The power comes from a battery (`kIOPMBatteryPowerKey`): a Mac without one
/// says "AC Power".
pub fn on_battery() -> bool {
    // SAFETY: the blob is ours to release; the type string belongs to it.
    unsafe {
        let blob = IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return false;
        }
        let kind = IOPSGetProvidingPowerSourceType(blob);
        let mut buf = [0 as c_char; 64];
        let battery = !kind.is_null()
            && CFStringGetCString(kind, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0
            && CStr::from_ptr(buf.as_ptr()).to_bytes() == b"Battery Power";
        CFRelease(blob);
        battery
    }
}

/// THE HOLD ON IDLE SLEEP while the lanes compute (`PreventUserIdleSystemSleep`:
/// the display may still sleep, the Mac does not), let go when they stop. A
/// Mac left alone otherwise sleeps after its energy settings' minutes and the
/// lanes with it. Ends with the process in any case.
pub fn keep_awake(on: bool) {
    static HELD: Mutex<Option<u32>> = Mutex::new(None);
    let Ok(mut held) = HELD.lock() else { return };
    match (on, *held) {
        (true, None) => {
            let (kind, name) = (cf_string("PreventUserIdleSystemSleep"), cf_string("simfleet is computing for WFSim"));
            let mut id = 0u32;
            // SAFETY: two strings made above and released after; 255 is
            // `kIOPMAssertionLevelOn`.
            let ok = unsafe {
                let r = IOPMAssertionCreateWithName(kind, 255, name, &mut id);
                CFRelease(kind);
                CFRelease(name);
                r == 0
            };
            if ok {
                *held = Some(id);
            }
        }
        (false, Some(id)) => {
            // SAFETY: an assertion this process made.
            unsafe { IOPMAssertionRelease(id) };
            *held = None;
        }
        _ => {}
    }
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let c = CString::new(name).ok()?;
    let mut v = [0u8; 8];
    let mut n = v.len();
    // SAFETY: a buffer of `n` bytes for a numeric value of up to 8.
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), v.as_mut_ptr().cast(), &mut n, std::ptr::null_mut(), 0) };
    match (r, n) {
        (0, 4) => Some(u32::from_ne_bytes(v[..4].try_into().ok()?) as u64),
        (0, 8) => Some(u64::from_ne_bytes(v)),
        _ => None,
    }
}

/// `kern.clockrate`'s `hz`: what the processor's load ticks count in.
fn hz() -> u64 {
    let c = CString::new("kern.clockrate").unwrap_or_default();
    let mut v = [0i32; 5];
    let mut n = std::mem::size_of_val(&v);
    // SAFETY: `struct clockinfo` is five ints, `hz` the first.
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), v.as_mut_ptr().cast(), &mut n, std::ptr::null_mut(), 0) };
    if r == 0 && v[0] > 0 { v[0] as u64 } else { 100 }
}

/// All cores' time so far (busy and idle) and the idle part, in µs
/// (`HOST_CPU_LOAD_INFO`: user, system, idle, nice ticks).
pub fn cpu_load_us() -> Option<(u64, u64)> {
    let mut t = [0u32; 4];
    let mut count = 4u32;
    // SAFETY: four natural_t and their count.
    if unsafe { host_statistics(mach_host_self(), 3, t.as_mut_ptr().cast(), &mut count) } != 0 {
        return None;
    }
    let us = 1_000_000 / hz().max(1);
    let all: u64 = t.iter().map(|x| *x as u64).sum();
    Some((all * us, t[2] as u64 * us))
}

/// `struct vm_statistics64` (mach/vm_statistics.h).
#[repr(C)]
#[derive(Default)]
struct VmStats {
    free: u32,
    active: u32,
    inactive: u32,
    wire: u32,
    zero_fill: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable: u32,
    speculative: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor: u32,
    throttled: u32,
    external: u32,
    internal: u32,
    uncompressed_in_compressor: u64,
}

/// `struct mach_task_basic_info` (mach/task_info.h).
#[repr(C, packed(4))]
#[derive(Default)]
struct TaskBasic {
    virtual_size: u64,
    resident_size: u64,
    resident_size_max: u64,
    user_time: [i32; 2],
    system_time: [i32; 2],
    policy: i32,
    suspend_count: i32,
}

/// Memory in use and in all, and this process's resident size, in bytes. In
/// use is what Activity Monitor calls Memory Used: app memory (anonymous, less
/// what may be purged), wired and compressed; the file cache is not.
pub fn memory() -> Option<(u64, u64, u64)> {
    let total = sysctl_u64("hw.memsize")?;
    let page = sysctl_u64("hw.pagesize").unwrap_or(16384);
    let mut vm = VmStats::default();
    let mut count = (std::mem::size_of::<VmStats>() / 4) as u32;
    // SAFETY: the struct and its size in integers (`HOST_VM_INFO64_COUNT`).
    if unsafe { host_statistics64(mach_host_self(), 4, (&mut vm as *mut VmStats).cast(), &mut count) } != 0 {
        return None;
    }
    let used = (vm.internal.saturating_sub(vm.purgeable) as u64 + vm.wire as u64 + vm.compressor as u64) * page;
    let mut me = TaskBasic::default();
    let mut count = (std::mem::size_of::<TaskBasic>() / 4) as u32;
    // SAFETY: as above, `MACH_TASK_BASIC_INFO` (20) on this task.
    let rss = if unsafe { task_info(mach_task_self_, 20, (&mut me as *mut TaskBasic).cast(), &mut count) } == 0 {
        me.resident_size
    } else {
        0
    };
    Some((used.min(total), total, rss))
}

/// Apple silicon's cores: (performance, efficiency) logical processors, if
/// the system tells two levels apart.
pub fn perf_levels() -> Option<(usize, usize)> {
    if sysctl_u64("hw.nperflevels")? < 2 {
        return None;
    }
    Some((sysctl_u64("hw.perflevel0.logicalcpu")? as usize, sysctl_u64("hw.perflevel1.logicalcpu")? as usize))
}

/// The chip, as the system names it ("Apple M2 Pro").
pub fn chip() -> String {
    let c = CString::new("machdep.cpu.brand_string").unwrap_or_default();
    let mut buf = [0u8; 128];
    let mut n = buf.len();
    // SAFETY: a buffer of `n` bytes for a string.
    if unsafe { libc::sysctlbyname(c.as_ptr(), buf.as_mut_ptr().cast(), &mut n, std::ptr::null_mut(), 0) } != 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..n]).trim_end_matches('\0').trim().to_string()
}

/// QoS classes (sys/qos.h).
pub const QOS_USER_INITIATED: u32 = 0x19;
pub const QOS_UTILITY: u32 = 0x11;
pub const QOS_BACKGROUND: u32 = 0x09;

/// This thread's QoS class; the threads it starts take it too.
pub fn set_thread_qos(class: u32) -> bool {
    // SAFETY: a plain call on the calling thread.
    unsafe { pthread_set_qos_class_self_np(class, 0) == 0 }
}
