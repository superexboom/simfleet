// SPDX-License-Identifier: AGPL-3.0-or-later
//! THE ENGINE, run outside the browser.
//!
//! The default engine is THE wasm module wfsim.app serves (`/pkg/wfsim_wasm_bg.<digest>.wasm`),
//! compiled once by wasmtime and cached: the same bytes every browser runs, so
//! its answers are a browser's answers, and a new release is a new download
//! rather than a new simfleet. Its seven `wasm-bindgen` imports are answered
//! here; the callbacks (progress) are not needed and answer nothing.
//!
//! A NATIVE build of the same source (`engines/<engine id>.dll`, see
//! native-engine/) is faster, and is used for board orders only once it has
//! reproduced the wasm's bits on a canary row.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wasmtime::{Caller, Config, Engine, ExternType, Func, Linker, Memory, Module, Ref, Store, Table, TypedFunc, Val, ValType};

fn we(e: wasmtime::Error) -> anyhow::Error {
    anyhow!("{e:?}")
}

pub struct Host {
    table: Option<Table>,
}

pub struct WasmCode {
    engine: Engine,
    module: Module,
    linker: Linker<Host>,
}

fn config() -> Config {
    let mut c = Config::new();
    c.wasm_reference_types(true);
    c.cranelift_opt_level(wasmtime::OptLevel::Speed);
    c
}

fn zero(t: &ValType) -> Val {
    match t {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0),
        ValType::F64 => Val::F64(0),
        _ => Val::ExternRef(None),
    }
}

impl WasmCode {
    /// Compile `bytes`, or read the compiled copy cached at `cached`.
    pub fn load(bytes: &[u8], cached: &Path) -> Result<WasmCode> {
        let engine = Engine::new(&config()).map_err(we)?;
        let from_cache = if cached.exists() {
            // SAFETY: the file is one this program wrote with `serialize` for
            // this engine configuration; wasmtime refuses a mismatched one.
            unsafe { Module::deserialize_file(&engine, cached) }.ok()
        } else {
            None
        };
        let module = match from_cache {
            Some(m) => m,
            None => {
                let m = Module::from_binary(&engine, bytes).map_err(we)?;
                if let Ok(art) = m.serialize() {
                    let tmp = cached.with_extension("tmp");
                    if std::fs::write(&tmp, art).is_ok() {
                        let _ = std::fs::rename(&tmp, cached);
                    }
                }
                m
            }
        };
        let mut linker: Linker<Host> = Linker::new(&engine);
        for imp in module.imports() {
            let (m, name) = (imp.module().to_string(), imp.name().to_string());
            let ExternType::Func(ty) = imp.ty() else { bail!("engine imports a non-function: {name}") };
            if name.starts_with("__wbg___wbindgen_throw") {
                linker
                    .func_wrap(&m, &name, |mut c: Caller<'_, Host>, p: u32, n: u32| -> wasmtime::Result<()> {
                        let msg = match c.get_export("memory").and_then(|e| e.into_memory()) {
                            Some(mem) => {
                                let d = mem.data(&c);
                                String::from_utf8_lossy(d.get(p as usize..(p as usize + n as usize)).unwrap_or(b"?")).to_string()
                            }
                            None => "?".into(),
                        };
                        Err(wasmtime::Error::msg(format!("engine threw: {msg}")))
                    })
                    .map_err(we)?;
            } else if name.starts_with("__wbg_now") {
                linker.func_wrap(&m, &name, || -> f64 { crate::sys::now_ms() as f64 }).map_err(we)?;
            } else if name == "__wbindgen_init_externref_table" {
                // The page's glue grows the table by four (undefined, null,
                // true, false); here they are all the null reference.
                linker
                    .func_wrap(&m, &name, |mut c: Caller<'_, Host>| -> wasmtime::Result<()> {
                        if let Some(t) = c.data().table {
                            t.grow(&mut c, 4, Ref::Extern(None))?;
                        }
                        Ok(())
                    })
                    .map_err(we)?;
            } else if name.starts_with("__wbg_call") || name.starts_with("__wbindgen_cast") {
                // A JS callback (progress) or a value made for one: nobody listens.
                let results: Vec<ValType> = ty.results().collect();
                linker
                    .func_new(&m, &name, ty.clone(), move |_c, _p, r| {
                        for (x, t) in r.iter_mut().zip(&results) {
                            *x = zero(t);
                        }
                        Ok(())
                    })
                    .map_err(we)?;
            } else {
                // A release that asks the host for something new fails loudly
                // instead of answering wrong.
                let n2 = name.clone();
                linker
                    .func_new(&m, &name, ty.clone(), move |_c, _p, _r| {
                        Err(wasmtime::Error::msg(format!("simfleet does not provide engine import {n2}; update simfleet")))
                    })
                    .map_err(we)?;
            }
        }
        Ok(WasmCode { engine, module, linker })
    }
}

pub struct WasmInst {
    store: Store<Host>,
    mem: Memory,
    malloc: TypedFunc<(u32, u32), u32>,
    free: TypedFunc<(u32, u32, u32), ()>,
    api: TypedFunc<(u32, u32, u32, u32), (u32, u32)>,
    optimize: Option<Func>,
}

impl WasmInst {
    pub fn new(code: &WasmCode) -> Result<WasmInst> {
        let mut store = Store::new(&code.engine, Host { table: None });
        let inst = code.linker.instantiate(&mut store, &code.module).map_err(we)?;
        store.data_mut().table = inst.get_table(&mut store, "__wbindgen_externrefs");
        if let Ok(start) = inst.get_typed_func::<(), ()>(&mut store, "__wbindgen_start") {
            start.call(&mut store, ()).map_err(we)?;
        }
        let mem = inst.get_memory(&mut store, "memory").context("engine exports no memory")?;
        Ok(WasmInst {
            malloc: inst.get_typed_func(&mut store, "__wbindgen_malloc").map_err(we)?,
            free: inst.get_typed_func(&mut store, "__wbindgen_free").map_err(we)?,
            api: inst.get_typed_func(&mut store, "api").map_err(we)?,
            optimize: inst.get_func(&mut store, "optimize"),
            mem,
            store,
        })
    }

    fn put(&mut self, s: &str) -> Result<u32> {
        let p = self.malloc.call(&mut self.store, (s.len() as u32, 1)).map_err(we)?;
        self.mem.data_mut(&mut self.store)[p as usize..p as usize + s.len()].copy_from_slice(s.as_bytes());
        Ok(p)
    }

    fn take(&mut self, p: u32, n: u32) -> Result<String> {
        let out = String::from_utf8(self.mem.data(&self.store)[p as usize..(p + n) as usize].to_vec())?;
        self.free.call(&mut self.store, (p, n, 1)).map_err(we)?;
        Ok(out)
    }

    pub fn api(&mut self, path: &str, body: &str) -> Result<String> {
        let pp = self.put(path)?;
        let bp = self.put(body)?;
        let (p, n) = self.api.call(&mut self.store, (pp, path.len() as u32, bp, body.len() as u32)).map_err(we)?;
        self.take(p, n)
    }

    pub fn optimize(&mut self, body: &str) -> Result<String> {
        let f = self.optimize.context("engine exports no optimize")?;
        let bp = self.put(body)?;
        let args = [
            Val::I32(bp as i32),
            Val::I32(body.len() as i32),
            Val::ExternRef(None),
            Val::ExternRef(None),
            Val::ExternRef(None),
        ];
        let mut out = [Val::I32(0), Val::I32(0)];
        f.call(&mut self.store, &args, &mut out).map_err(we)?;
        let (p, n) = (out[0].unwrap_i32() as u32, out[1].unwrap_i32() as u32);
        self.take(p, n)
    }
}

type ApiFn = unsafe extern "C" fn(*const u8, usize, *const u8, usize, *mut usize) -> *mut u8;
type OptimizeFn = unsafe extern "C" fn(*const u8, usize, *mut usize) -> *mut u8;
type FreeFn = unsafe extern "C" fn(*mut u8, usize);
type IdFn = unsafe extern "C" fn() -> *const std::ffi::c_char;

/// A native engine library (native-engine/): `sfe_api`, `sfe_free`,
/// `sfe_engine_id`, and in a build with the optimizer patch `sfe_optimize`.
pub struct NativeLib {
    _lib: libloading::Library,
    api: ApiFn,
    optimize: Option<OptimizeFn>,
    free: FreeFn,
    pub engine_id: String,
    /// The CPU level it was built for: "" or "-v3" (`work::native_variants`).
    pub variant: &'static str,
    /// ITS OPTIMIZER held to the wasm (work.rs `riven_canary`, on its own
    /// thread): 0 not asked, 1 checking, 2 the same answer, 3 not — and why.
    riven: std::sync::atomic::AtomicU8,
    riven_why: std::sync::Mutex<String>,
}

impl NativeLib {
    pub fn open(path: &Path) -> Result<NativeLib> {
        // SAFETY: a library built from native-engine/ for this platform; its
        // three entry points are plain C functions over byte buffers.
        unsafe {
            let lib = libloading::Library::new(path)?;
            let api: ApiFn = *lib.get::<ApiFn>(b"sfe_api\0")?;
            let free: FreeFn = *lib.get::<FreeFn>(b"sfe_free\0")?;
            let id: IdFn = *lib.get::<IdFn>(b"sfe_engine_id\0")?;
            let optimize = lib.get::<OptimizeFn>(b"sfe_optimize\0").ok().map(|f| *f);
            let engine_id = std::ffi::CStr::from_ptr(id()).to_string_lossy().to_string();
            Ok(NativeLib {
                _lib: lib,
                api,
                optimize,
                free,
                engine_id,
                variant: "",
                riven: Default::default(),
                riven_why: Default::default(),
            })
        }
    }

    pub fn api(&self, path: &str, body: &str) -> Result<String> {
        // SAFETY: the library's own entry point over byte buffers.
        unsafe {
            let mut n = 0usize;
            let p = (self.api)(path.as_ptr(), path.len(), body.as_ptr(), body.len(), &mut n);
            self.take(p, n, path)
        }
    }

    /// The build has the optimizer (`sfe_optimize`)…
    pub fn has_optimize(&self) -> bool {
        self.optimize.is_some()
    }
    /// …and it was held to the wasm: riven gains may run here.
    pub fn can_optimize(&self) -> bool {
        self.optimize.is_some() && self.riven_state() == 2
    }
    pub fn riven_state(&self) -> u8 {
        self.riven.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn riven_why(&self) -> String {
        self.riven_why.lock().map(|w| w.clone()).unwrap_or_default()
    }
    pub fn riven_checking(&self) {
        self.riven.store(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn riven_checked(&self, r: Result<()>) {
        if let Err(e) = &r {
            if let Ok(mut w) = self.riven_why.lock() {
                *w = format!("{e:#}");
            }
        }
        self.riven.store(if r.is_ok() { 2 } else { 3 }, std::sync::atomic::Ordering::Relaxed);
    }
    /// The canary's verdict, waited for (a replay, a check).
    pub fn wait_riven(&self) -> u8 {
        while self.riven_state() == 1 {
            std::thread::sleep(Duration::from_millis(100));
        }
        self.riven_state()
    }

    /// `optimize(body)`, as the wasm's, on the calling thread (a quick_fleet
    /// lead's state is that thread's, so a lead always runs on one lane).
    pub fn optimize(&self, body: &str) -> Result<String> {
        let Some(f) = self.optimize else { bail!("this native engine has no optimizer") };
        // SAFETY: as `api`.
        unsafe {
            let mut n = 0usize;
            let p = f(body.as_ptr(), body.len(), &mut n);
            self.take(p, n, "optimize")
        }
    }

    unsafe fn take(&self, p: *mut u8, n: usize, what: &str) -> Result<String> {
        if p.is_null() {
            bail!("native engine failed on {what}");
        }
        // SAFETY: what the library returned, freed by the library.
        unsafe {
            let out = String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).to_string();
            (self.free)(p, n);
            Ok(out)
        }
    }
}

/// Everything one engine release needs: its wasm, and a native build if one matched.
pub struct Code {
    pub wasm: WasmCode,
    pub native: Option<NativeLib>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Wasm,
    Native,
}

pub enum Call {
    Api(&'static str, String),
    Optimize(String),
}

/// One thread's engine: instantiated on first use and thrown away after a trap,
/// since a module that trapped mid-call holds no state worth trusting.
pub struct Exec {
    code: Arc<Code>,
    wasm: Option<WasmInst>,
}

impl Exec {
    pub fn new(code: Arc<Code>) -> Exec {
        Exec { code, wasm: None }
    }
    pub fn run(&mut self, backend: Backend, call: &Call) -> Result<String> {
        if let (Backend::Native, Some(n)) = (backend, &self.code.native) {
            match call {
                Call::Api(path, body) => return n.api(path, body),
                Call::Optimize(body) if n.can_optimize() => return n.optimize(body),
                Call::Optimize(_) => {}
            }
        }
        if self.wasm.is_none() {
            self.wasm = Some(WasmInst::new(&self.code.wasm)?);
        }
        let w = self.wasm.as_mut().unwrap();
        let r = match call {
            Call::Api(path, body) => w.api(path, body),
            Call::Optimize(body) => w.optimize(body),
        };
        if r.is_err() {
            self.wasm = None;
        }
        r
    }
    /// Let the wasm instance go (its memory never shrinks while it lives).
    pub fn release_wasm(&mut self) {
        self.wasm = None;
    }
    pub fn api(&mut self, backend: Backend, path: &'static str, body: String) -> Result<String> {
        self.run(backend, &Call::Api(path, body))
    }
}

pub struct Job {
    pub tag: u64,
    pub backend: Backend,
    pub call: Call,
}

pub struct Done {
    pub tag: u64,
    pub lane: usize,
    pub ms: f64,
    pub out: Result<String>,
}

/// THE LANES: one thread a core, each with its own engine instance, fed jobs
/// in order. Lane 0's instance is the one a riven gain's search lives in.
pub struct Pool {
    code: Arc<Code>,
    lanes: Vec<mpsc::Sender<Job>>,
    pending: Vec<usize>,
    done_tx: mpsc::Sender<Done>,
    done_rx: mpsc::Receiver<Done>,
    tag: u64,
    /// Engine time the lanes have spent, all of it (the utilisation meter).
    pub busy_ms: f64,
}

impl Pool {
    pub fn new(code: Arc<Code>) -> Pool {
        let (done_tx, done_rx) = mpsc::channel();
        Pool { code, lanes: Vec::new(), pending: Vec::new(), done_tx, done_rx, tag: 0, busy_ms: 0.0 }
    }
    pub fn code(&self) -> &Arc<Code> {
        &self.code
    }
    pub fn ensure(&mut self, n: usize) {
        while self.lanes.len() < n {
            let lane = self.lanes.len();
            let (tx, rx) = mpsc::channel::<Job>();
            let done = self.done_tx.clone();
            let code = self.code.clone();
            std::thread::Builder::new()
                .name(format!("lane-{lane}"))
                .stack_size(8 << 20)
                .spawn(move || {
                    crate::affinity::apply_lane(lane);
                    let mut exec = Exec::new(code);
                    loop {
                        // A lane idle for a minute gives its instance back, as the
                        // page lets a kept worker go (08-checkpoint-api.js `fleetKeep`).
                        // NOT LANE 0: a riven gain's search lives in its instance
                        // between steps, and a step can wait longer than that.
                        let job = match rx.recv_timeout(Duration::from_secs(60)) {
                            Ok(j) => j,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if lane != 0 {
                                    exec.release_wasm();
                                }
                                continue;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        };
                        let (t, cpu) = (Instant::now(), crate::sys::thread_cpu_us());
                        let out = exec.run(job.backend, &job.call);
                        let ms = t.elapsed().as_secs_f64() * 1000.0;
                        // WHAT THE LANE WAS GIVEN: CPU time against wall time — but
                        // not for a native search, which computes on a worker
                        // thread of the optimizer's own while this one waits.
                        let away = matches!((job.backend, &job.call), (Backend::Native, Call::Optimize(_)));
                        let (c, w) = if away { (0, 0) } else { (crate::sys::thread_cpu_us().saturating_sub(cpu), (ms * 1000.0) as u64) };
                        crate::affinity::note_job(lane, c, w);
                        if done.send(Done { tag: job.tag, lane, ms, out }).is_err() {
                            break;
                        }
                    }
                })
                .expect("spawn lane");
            self.lanes.push(tx);
            self.pending.push(0);
        }
    }
    pub fn next_tag(&mut self) -> u64 {
        self.tag += 1;
        self.tag
    }
    pub fn pending(&self, lane: usize) -> usize {
        self.pending.get(lane).copied().unwrap_or(0)
    }
    pub fn send(&mut self, lane: usize, job: Job) {
        self.ensure(lane + 1);
        self.pending[lane] += 1;
        let _ = self.lanes[lane].send(job);
    }
    pub fn recv(&mut self, wait: Duration) -> Option<Done> {
        let d = self.done_rx.recv_timeout(wait).ok()?;
        self.busy_ms += d.ms;
        if let Some(p) = self.pending.get_mut(d.lane) {
            *p = p.saturating_sub(1);
        }
        Some(d)
    }
}
