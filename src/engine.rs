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
//!
//! THE DATA PACK (WFSim docs/CLIENTS.md "The official data pack"): the engine
//! is code, and the game data it computes from is a file published beside it,
//! named by release.json's `data`. Every wasm instance has it installed
//! (`install_data_bytes`) before its first call, and WFSim's own library
//! (abi 2, `wfsim_install_data`) once when it is loaded. From 2026-10-25 01:00
//! UTC a served engine carries no data of its own and answers nothing without
//! it; until then an installed pack replaces what it carries, to the same bits.
//! simfleet's own builds are plain builds of the commit, which embed the data.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
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
    /// The release's data pack, installed into every instance; None for a
    /// release that names none (or a reference module that embeds its data).
    pack: Option<Arc<Vec<u8>>>,
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
        Ok(WasmCode { engine, module, linker, pack: None })
    }

    /// The data pack every instance of this module is given first.
    pub fn with_pack(mut self, pack: Option<Arc<Vec<u8>>>) -> WasmCode {
        self.pack = pack;
        self
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
        // The bytes door first: a string door copies a pack in at up to three
        // bytes a character, and linear memory never shrinks (WFSim docs/WASM.md).
        let install = ["install_data_bytes", "install_data"]
            .iter()
            .find_map(|n| inst.get_typed_func::<(u32, u32), (u32, u32)>(&mut store, n).ok());
        let mut w = WasmInst {
            malloc: inst.get_typed_func(&mut store, "__wbindgen_malloc").map_err(we)?,
            free: inst.get_typed_func(&mut store, "__wbindgen_free").map_err(we)?,
            api: inst.get_typed_func(&mut store, "api").map_err(we)?,
            optimize: inst.get_func(&mut store, "optimize"),
            mem,
            store,
        };
        // THE DATA PACK, before the first call: once per instance.
        if let Some(pack) = &code.pack {
            let f = install.context("这个引擎没有安装数据包的入口（install_data），请更新 simfleet")?;
            let p = w.put(pack.as_slice())?;
            let (r, n) = f.call(&mut w.store, (p, pack.len() as u32)).map_err(we)?;
            let why = w.take(r, n)?;
            if !why.is_empty() {
                bail!("数据包安装失败：{why}");
            }
        }
        Ok(w)
    }

    fn put(&mut self, s: impl AsRef<[u8]>) -> Result<u32> {
        let s = s.as_ref();
        let p = self.malloc.call(&mut self.store, (s.len() as u32, 1)).map_err(we)?;
        self.mem.data_mut(&mut self.store)[p as usize..p as usize + s.len()].copy_from_slice(s);
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
/// `wfsim_install_data(body, body_len, &len) -> ptr` (abi 2), the answer "" or why.
type InstallFn = unsafe extern "C" fn(*const u8, usize, *mut usize) -> *mut u8;
type FreeFn = unsafe extern "C" fn(*mut u8, usize);
type IdFn = unsafe extern "C" fn() -> *const std::ffi::c_char;

type LibJob = Box<dyn FnOnce() + Send>;

/// THE LIBRARY THREAD: every native library is loaded, and let go, on this
/// one thread, which lives as long as simfleet does.
///
/// mimalloc — in simfleet's builds and in WFSim's own library — takes the
/// thread that loads a library for that library's main thread. On its v3 core
/// (WFSim's library has it; simfleet's builds before 0.6.6 had it), when that
/// thread ends the main heap goes with it (`_mi_theap_free` sets its `tld` to
/// NULL), and the next thread the system builds on the same TEB (on Linux, the
/// same thread pointer) is taken for the main thread and reads through that
/// NULL: 0xC0000005, no message. Loaded on the short-lived thread that fetched
/// a new engine, a library crashed a running client a few hundred threads
/// later (the optimizer starts one a batch, and each engine change a new set
/// of lanes).
fn lib_thread() -> &'static mpsc::Sender<LibJob> {
    static TX: OnceLock<mpsc::Sender<LibJob>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<LibJob>();
        std::thread::Builder::new()
            .name("libraries".into())
            .spawn(move || {
                // Never ends: the sender is held for good above.
                for job in rx {
                    job();
                }
            })
            .expect("spawn the library thread");
        tx
    })
}

/// `f` on the library thread, waited for.
fn on_lib_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let (back, got) = mpsc::sync_channel(1);
    let job: LibJob = Box::new(move || {
        let _ = back.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
    });
    lib_thread().send(job).expect("the library thread lives as long as simfleet");
    match got.recv().expect("the library thread lives as long as simfleet") {
        Ok(r) => r,
        Err(p) => std::panic::resume_unwind(p),
    }
}

/// A library let go of on the library thread (`lib_thread`), not waited for.
struct LibHandle(Option<libloading::Library>);
impl LibHandle {
    /// SAFETY: as `libloading::Library::new` — a library whose initialisers
    /// are its own allocator's and the Rust runtime's.
    unsafe fn open(path: &Path) -> Result<LibHandle> {
        let path = path.to_path_buf();
        // SAFETY: as the caller's.
        on_lib_thread(move || unsafe { libloading::Library::new(&path) }.map(|l| LibHandle(Some(l))).map_err(Into::into))
    }
    fn lib(&self) -> &libloading::Library {
        self.0.as_ref().expect("held until drop")
    }
}
impl Drop for LibHandle {
    fn drop(&mut self) {
        if let Some(l) = self.0.take() {
            let _ = lib_thread().send(Box::new(move || drop(l)));
        }
    }
}

/// A native engine library: simfleet's build (native-engine/: `sfe_api`,
/// `sfe_free`, `sfe_engine_id`, `sfe_optimize`) or WFSim's own (abi 1:
/// `wfsim_api` and `wfsim_free`, the same two doors, and no optimizer; abi 2
/// adds `wfsim_install_data`, the data pack's door).
/// Loaded and let go on the library thread (`lib_thread`), called from any.
pub struct NativeLib {
    _lib: LibHandle,
    api: ApiFn,
    optimize: Option<OptimizeFn>,
    free: FreeFn,
    pub engine_id: String,
    /// The CPU level it was built for: "" or "-v3" (`work::native_variants`).
    pub variant: &'static str,
    /// WFSim's official library (work.rs `fetch_official`), not simfleet's build.
    pub official: bool,
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
            let lib = LibHandle::open(path)?;
            let api: ApiFn = *lib.lib().get::<ApiFn>(b"sfe_api\0")?;
            let free: FreeFn = *lib.lib().get::<FreeFn>(b"sfe_free\0")?;
            let id: IdFn = *lib.lib().get::<IdFn>(b"sfe_engine_id\0")?;
            let optimize = lib.lib().get::<OptimizeFn>(b"sfe_optimize\0").ok().map(|f| *f);
            let engine_id = std::ffi::CStr::from_ptr(id()).to_string_lossy().to_string();
            Ok(NativeLib {
                _lib: lib,
                api,
                optimize,
                free,
                engine_id,
                variant: "",
                official: false,
                riven: Default::default(),
                riven_why: Default::default(),
            })
        }
    }

    /// WFSim's official library of `engine` (docs/CLIENTS.md, abi 1 or 2), its
    /// manifest's signature and the file's digest checked before (work.rs);
    /// at abi 2 the release's data `pack` is installed in it here.
    pub fn open_official(path: &Path, engine: &str, pack: Option<&[u8]>) -> Result<NativeLib> {
        // SAFETY: a library whose signed manifest names this engine and abi 1
        // or 2: plain C functions over byte buffers, as `sfe_api` and `sfe_free`.
        unsafe {
            let lib = LibHandle::open(path)?;
            let api: ApiFn = *lib.lib().get::<ApiFn>(b"wfsim_api\0")?;
            let free: FreeFn = *lib.lib().get::<FreeFn>(b"wfsim_free\0")?;
            if let Ok(install) = lib.lib().get::<InstallFn>(b"wfsim_install_data\0") {
                let pack = pack.context("WFSim 官方原生引擎要数据包，但 release.json 没有给出")?;
                let mut n = 0usize;
                let p = install(pack.as_ptr(), pack.len(), &mut n);
                if p.is_null() {
                    bail!("数据包安装失败");
                }
                let why = String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).to_string();
                free(p, n);
                // THE SAME FILE LOADED AGAIN is the same module, its data in:
                // installed the first time, or read since. The canary that
                // follows holds it to the wasm either way.
                if !why.is_empty() && !why.contains("is already installed") && !why.contains("was read before it was installed") {
                    bail!("数据包安装失败：{why}");
                }
            }
            Ok(NativeLib {
                _lib: lib,
                api,
                optimize: None,
                free,
                engine_id: engine.to_string(),
                variant: "",
                official: true,
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
    /// A board row's runs (`/api/board/runs`, so many of them): the answer is
    /// its shards, read on the lane as the page reads them (work.rs `shards_text`).
    Runs(String, u32),
    /// A board row's fold (`/api/board/fold`): the answer is its `acc`, read
    /// on the lane as the page reads it (work.rs `acc_text`).
    Fold(String),
}

/// A wasm instance its thread has not computed on for this long, while the
/// thread computes natively, is let go.
const WASM_IDLE: Duration = Duration::from_secs(30);

/// One thread's engine: instantiated on first use and thrown away after a trap,
/// since a module that trapped mid-call holds no state worth trusting.
pub struct Exec {
    code: Arc<Code>,
    wasm: Option<WasmInst>,
    /// When `wasm` last computed.
    wasm_at: Instant,
}

impl Exec {
    pub fn new(code: Arc<Code>) -> Exec {
        Exec { code, wasm: None, wasm_at: Instant::now() }
    }
    pub fn run(&mut self, backend: Backend, call: &Call) -> Result<String> {
        match call {
            Call::Api(path, body) => self.door(backend, Some(path), body),
            Call::Optimize(body) => self.door(backend, None, body),
            Call::Runs(body, count) => crate::work::shards_text(&self.door(backend, Some("/api/board/runs"), body)?, *count),
            Call::Fold(body) => crate::work::acc_text(&self.door(backend, Some("/api/board/fold"), body)?),
        }
    }
    /// `api(path, body)`, or `optimize(body)` with no path.
    fn door(&mut self, backend: Backend, path: Option<&str>, body: &str) -> Result<String> {
        if let (Backend::Native, Some(n)) = (backend, &self.code.native) {
            if path.is_some() || n.can_optimize() {
                // AN INSTANCE THIS THREAD NO LONGER COMPUTES ON goes: its memory
                // never shrinks while it lives (some 50 MB once it has worked),
                // and a lane that took up a native build would keep it for good.
                // Never a riven gain's search: its leader computes nothing but
                // that search, on the backend it began on.
                if self.wasm.is_some() && self.wasm_at.elapsed() >= WASM_IDLE {
                    self.wasm = None;
                }
                return match path {
                    Some(path) => n.api(path, body),
                    None => n.optimize(body),
                };
            }
        }
        if self.wasm.is_none() {
            self.wasm = Some(WasmInst::new(&self.code.wasm)?);
        }
        let w = self.wasm.as_mut().unwrap();
        let r = match path {
            Some(path) => w.api(path, body),
            None => w.optimize(body),
        };
        self.wasm_at = Instant::now();
        if r.is_err() {
            self.wasm = None;
        }
        r
    }
    /// THE SAME SERVED WASM, NOW WITH A NATIVE BUILD BESIDE IT (`Pool::adopt`):
    /// the instance stays, and a search living in it carries on.
    pub fn adopt(&mut self, code: Arc<Code>) {
        self.code = code;
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
/// in order. A riven gain's search lives on its leader's lane (engine.rs
/// `Exec`, fleet.rs `Riven`).
///
/// A LANE'S THREAD IS STARTED WHEN IT IS GIVEN WORK AND LET GO BY THE
/// DISPATCHER (`retire`): what lives on a thread goes with it — its wasm
/// instance, a riven gain's search (natively the thread's own state, some
/// hundreds of MB), what its allocator keeps for it — and nothing else lets
/// that memory go.
pub struct Pool {
    code: Arc<Code>,
    /// What each lane runs its next job with (`adopt`).
    latest: Arc<std::sync::Mutex<Arc<Code>>>,
    /// Each lane's thread; `None` while it has none.
    lanes: Vec<Option<mpsc::Sender<Job>>>,
    pending: Vec<usize>,
    /// When each lane was last given work or gave an answer.
    active: Vec<Instant>,
    done_tx: mpsc::Sender<Done>,
    done_rx: mpsc::Receiver<Done>,
    tag: u64,
    /// Engine time the lanes have spent, all of it (the utilisation meter).
    pub busy_ms: f64,
}

impl Pool {
    pub fn new(code: Arc<Code>) -> Pool {
        let (done_tx, done_rx) = mpsc::channel();
        let latest = Arc::new(std::sync::Mutex::new(code.clone()));
        Pool { code, latest, lanes: Vec::new(), pending: Vec::new(), active: Vec::new(), done_tx, done_rx, tag: 0, busy_ms: 0.0 }
    }
    pub fn code(&self) -> &Arc<Code> {
        &self.code
    }
    /// A native build of the same engine, found while the lanes work: each lane
    /// takes it up at its next job, keeping its instance and what lives there.
    pub fn adopt(&mut self, code: Arc<Code>) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = code.clone();
        self.code = code;
    }
    pub fn ensure(&mut self, n: usize) {
        while self.lanes.len() < n {
            self.lanes.push(None);
            self.pending.push(0);
            self.active.push(Instant::now());
        }
    }
    /// `lane`'s thread let go: it ends once the jobs it was given are done,
    /// and the lane's next job starts a new one.
    pub fn retire(&mut self, lane: usize) {
        if let Some(l) = self.lanes.get_mut(lane) {
            *l = None;
        }
    }
    /// EVERY LANE IDLE FOR `after` let go (`retire`), but those `keep` names —
    /// a riven gain's leader waits on the rest of its round with its search
    /// on its thread.
    pub fn retire_idle(&mut self, after: Duration, keep: &dyn Fn(usize) -> bool) {
        for lane in 0..self.lanes.len() {
            if self.lanes[lane].is_some() && self.pending[lane] == 0 && self.active[lane].elapsed() >= after && !keep(lane) {
                self.lanes[lane] = None;
            }
        }
    }
    fn spawn(&self, lane: usize) -> mpsc::Sender<Job> {
        let (tx, rx) = mpsc::channel::<Job>();
        let done = self.done_tx.clone();
        let code = self.code.clone();
        let latest = self.latest.clone();
        std::thread::Builder::new()
            .name(format!("lane-{lane}"))
            .stack_size(8 << 20)
            .spawn(move || {
                crate::affinity::apply_lane(lane);
                let mut exec = Exec::new(code);
                // Until the pool lets the lane go (`retire`).
                for job in rx {
                    if let Ok(c) = latest.lock() {
                        if !Arc::ptr_eq(&c, &exec.code) {
                            exec.adopt(c.clone());
                        }
                    }
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
        tx
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
        if self.lanes[lane].is_none() {
            self.lanes[lane] = Some(self.spawn(lane));
        }
        self.pending[lane] += 1;
        self.active[lane] = Instant::now();
        if let Some(tx) = &self.lanes[lane] {
            let _ = tx.send(job);
        }
    }
    pub fn recv(&mut self, wait: Duration) -> Option<Done> {
        let d = self.done_rx.recv_timeout(wait).ok()?;
        self.busy_ms += d.ms;
        if let Some(p) = self.pending.get_mut(d.lane) {
            *p = p.saturating_sub(1);
        }
        if let Some(a) = self.active.get_mut(d.lane) {
            *a = Instant::now();
        }
        Some(d)
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    /// Where the system built this thread: its TEB (Windows) or thread pointer
    /// (Linux), which mimalloc takes for its id (`_mi_prim_thread_id`).
    fn thread_block() -> usize {
        let t: usize;
        // SAFETY: reads the thread's own control block pointer.
        #[cfg(windows)]
        unsafe {
            std::arch::asm!("mov {}, gs:[0x30]", out(reg) t)
        };
        #[cfg(unix)]
        unsafe {
            std::arch::asm!("mov {}, fs:[0]", out(reg) t)
        };
        t
    }

    /// A LIBRARY OPENED FROM A THREAD THAT ENDS (a background engine fetch),
    /// then called from a thread built where that one was: WFSim's own library
    /// crashed here before the library thread (`lib_thread`).
    /// `SIMFLEET_TEST_LIB=<engines/…dll> [SIMFLEET_TEST_PACK=<pack.data.….json>] cargo test --release lib_outlives_its_opener -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn lib_outlives_its_opener() {
        let path = std::path::PathBuf::from(std::env::var("SIMFLEET_TEST_LIB").expect("SIMFLEET_TEST_LIB"));
        let pack = std::env::var("SIMFLEET_TEST_PACK").ok().map(|p| std::fs::read(p).expect("the pack"));
        let (opener, lib) = std::thread::spawn(move || {
            let lib = NativeLib::open(&path).or_else(|_| NativeLib::open_official(&path, "test", pack.as_deref())).expect("open the library");
            lib.api("/api/board/nothing", "{}").expect("an answer");
            (thread_block(), lib)
        })
        .join()
        .expect("the opener");
        let lib = Arc::new(lib);
        for i in 0..100_000 {
            let l = lib.clone();
            let reused = std::thread::spawn(move || {
                let reused = thread_block() == opener;
                if reused {
                    l.api("/api/board/nothing", "{}").expect("an answer");
                }
                reused
            })
            .join()
            .expect("a thread built where the opener was");
            if reused {
                eprintln!("thread {i} was built where the opener was, and the library answered it");
                return;
            }
        }
        eprintln!("no thread was built where the opener was");
    }
}
