// SPDX-License-Identifier: AGPL-3.0-or-later
use super::driver::{memory_job, Cuda};
use super::{Input, Manifest, Output, MAX_ROWS, MAX_RUNS};
use anyhow::{anyhow, ensure, Context, Result};
use libloading::Library;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    io::{BufRead, Write},
    path::Path,
    time::Instant,
};
use wasmtime::{
    Engine, Instance, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
};

fn we(e: wasmtime::Error) -> anyhow::Error {
    anyhow!("{e:?}")
}
type Score = unsafe extern "C" fn(
    *const u8,
    usize,
    *const u8,
    usize,
    *const u32,
    u32,
    u32,
    *mut usize,
) -> *mut u8;
type Decode = unsafe extern "C" fn(*const u8, usize, *mut usize) -> *mut u8;
type Free = unsafe extern "C" fn(*mut u8, usize);
struct Wire {
    _lib: Library,
    score: Score,
    decode: Decode,
    free: Free,
}
impl Wire {
    fn open(dir: &Path) -> Result<Self> {
        unsafe {
            let lib = Library::new(dir.join("wire.dll"))?;
            Ok(Self {
                score: *lib.get(b"sf_wire_score\0")?,
                decode: *lib.get(b"sf_wire_decode\0")?,
                free: *lib.get(b"sf_wire_free\0")?,
                _lib: lib,
            })
        }
    }
    unsafe fn take(&self, p: *mut u8, n: usize) -> Result<Value> {
        ensure!(!p.is_null() && n > 0 && n <= 1 << 20, "invalid wire result");
        let v = crate::jsnum::parse(
            std::str::from_utf8(std::slice::from_raw_parts(p, n)).unwrap_or(""),
        );
        (self.free)(p, n);
        let v = v?;
        ensure!(v["ok"] != false, "GPU wire: {}", v["error"]);
        Ok(v)
    }
    fn decode(&self, data: &[u8]) -> Result<Value> {
        unsafe {
            let mut n = 0;
            let p = (self.decode)(data.as_ptr(), data.len(), &mut n);
            self.take(p, n)
        }
    }
    fn score(&self, input: &Input, data: &[u8], sizes: &[u32]) -> Result<Value> {
        let request = json!({"request":input.request,"ruler":input.ruler}).to_string();
        unsafe {
            let mut n = 0;
            let p = (self.score)(
                request.as_ptr(),
                request.len(),
                data.as_ptr(),
                data.len(),
                sizes.as_ptr(),
                sizes.len() as u32,
                8192,
                &mut n,
            );
            self.take(p, n)
        }
    }
}

struct Bridge {
    store: Store<StoreLimits>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    prepare: TypedFunc<(u32, u32), u32>,
    run: TypedFunc<(u32, u32), u64>,
    drop_row: TypedFunc<u32, ()>,
    stack: u32,
}
impl Bridge {
    fn new(engine: &Engine, module: &Module, gas_address: usize) -> Result<Self> {
        let mut store = Store::new(
            engine,
            StoreLimitsBuilder::new().memory_size(64 << 20).build(),
        );
        store.limiter(|s| s);
        let inst = Instance::new(&mut store, module, &[]).map_err(we)?;
        let memory = inst
            .get_memory(&mut store, "memory")
            .context("GPU bridge memory")?;
        let format = inst
            .get_typed_func::<(), u32>(&mut store, "sf_wire_format")
            .map_err(we)?
            .call(&mut store, ())
            .map_err(we)?;
        ensure!(format == 1, "unsupported GPU wire format");
        let stack = inst
            .get_global(&mut store, "__stack_pointer")
            .context("GPU stack")?
            .get(&mut store)
            .i32()
            .context("GPU stack type")? as u32;
        let probe = inst
            .get_typed_func::<u64, u64>(&mut store, "sf_gas_probe")
            .map_err(we)?;
        let before = probe.call(&mut store, 0).map_err(we)?;
        let after = probe.call(&mut store, 1000003).map_err(we)?;
        let mut raw = [0; 8];
        memory.read(&store, gas_address, &mut raw)?;
        ensure!(
            u64::from_le_bytes(raw) == after && after == before.wrapping_add(1000003),
            "GPU gas address mismatch"
        );
        ensure!(
            probe
                .call(&mut store, 0u64.wrapping_sub(1000003))
                .map_err(we)?
                == before,
            "GPU gas reset mismatch"
        );
        Ok(Self {
            alloc: inst.get_typed_func(&mut store, "sf_alloc").map_err(we)?,
            free: inst.get_typed_func(&mut store, "sf_free").map_err(we)?,
            prepare: inst.get_typed_func(&mut store, "sf_prepare").map_err(we)?,
            run: inst.get_typed_func(&mut store, "sf_run").map_err(we)?,
            drop_row: inst.get_typed_func(&mut store, "sf_drop").map_err(we)?,
            store,
            memory,
            stack,
        })
    }
    fn snapshot(
        &mut self,
        request: &Value,
        indices: &[u32],
    ) -> Result<(Vec<u8>, u32, Vec<Vec<u8>>)> {
        let data = serde_json::to_vec(request)?;
        ensure!(data.len() <= 256 << 10, "GPU request too large");
        let p = self
            .alloc
            .call(&mut self.store, data.len() as u32)
            .map_err(we)?;
        self.memory.write(&mut self.store, p as usize, &data)?;
        let handle = self
            .prepare
            .call(&mut self.store, (p, data.len() as u32))
            .map_err(we)?;
        self.free
            .call(&mut self.store, (p, data.len() as u32))
            .map_err(we)?;
        // One original WASM shard per row audits device arithmetic and the snapshot.
        let mut references = Vec::new();
        for index in indices {
            let pair = self
                .run
                .call(&mut self.store, (handle, *index))
                .map_err(we)?;
            let (p, n) = (pair as u32, (pair >> 32) as u32);
            ensure!(n > 0 && n <= 8192, "GPU canary output too large");
            let mut reference = vec![0; n as usize];
            self.memory.read(&self.store, p as usize, &mut reference)?;
            self.free.call(&mut self.store, (p, n)).map_err(we)?;
            references.push(reference);
        }
        let image = self.memory.data(&self.store).to_vec();
        self.drop_row.call(&mut self.store, handle).map_err(we)?;
        Ok((image, handle, references))
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Job {
    memory: u64,
    bytes: u64,
    handle: u32,
    stack: u32,
    runs: u32,
    reserved: u32,
}
fn parameter<T>(v: &mut T) -> *mut c_void {
    (v as *mut T).cast()
}
fn bridge_module(engine: &Engine, bytes: &[u8], dir: &Path) -> Result<(Module, bool)> {
    let key = format!("{:x}", Sha256::digest(bytes));
    let cached = dir.join(format!("bridge-{}.wasmtime49.cwasm", &key[..16]));
    // Own serialized artifact, keyed by the verified WASM. Wasmtime checks
    // target/config compatibility, as it does for the reference engine cache.
    if cached.is_file() {
        if let Ok(module) = unsafe { Module::deserialize_file(engine, &cached) } {
            return Ok((module, true));
        }
    }
    let module = Module::from_binary(engine, bytes).map_err(we)?;
    if let Ok(image) = module.serialize() {
        let tmp = cached.with_extension("tmp");
        if std::fs::write(&tmp, image).is_ok() { let _ = std::fs::rename(tmp, &cached); }
    }
    Ok((module, false))
}

struct Runtime {
    gpu: Cuda,
    bridge: Bridge,
    wire: Wire,
    native: Option<super::native_probe::Native>,
    block_threads: u32,
    layout: super::MemoryLayout,
}
impl Runtime {
    fn batch(
        &mut self,
        inputs: &[Input],
        progress: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<(Vec<Output>, super::BatchTiming)> {
        self.layout.inputs(inputs)?;
        let began = Instant::now();
        progress("prepare")?;
        let mut jobs = Vec::new();
        let mut owners = Vec::new();
        let mut indices = Vec::new();
        let mut references = Vec::new();
        let mut maximum = 0;
        let probing = inputs[0].probe;
        let mut cpu_ms = 0.0;
        let mut lengths_by_job = Vec::new();
        for (i, input) in inputs.iter().enumerate() {
            let runs = input.request["runs"].as_u64().context("GPU runs")? as u32;
            let checks = if probing {
                super::sample_indices(runs)
            } else {
                vec![0]
            };
            let (image, handle, reference) = self.bridge.snapshot(&input.request, &checks)?;
            if probing {
                let native = self
                    .native
                    .as_ref()
                    .context("GPU probing requires matching native CPU library")?;
                // Lazy native tables and one-off scheduling stalls are not throughput.
                let first = native.measure(input, checks.len() as u32)?;
                cpu_ms = self
                    .native
                    .as_ref()
                    .context("GPU probing requires matching native CPU library")?
                    .measure(input, checks.len() as u32)?
                    .min(first);
            }
            maximum = maximum.max(image.len());
            let memory = self.gpu.buffer(i, image.len())?;
            self.gpu.upload(memory, &image)?;
            let runs = input.request["runs"].as_u64().context("GPU runs")? as u32;
            jobs.push(Job {
                memory,
                bytes: image.len() as u64,
                handle,
                stack: self.bridge.stack,
                runs,
                reserved: 0,
            });
            let count = if probing { checks.len() } else { runs as usize };
            lengths_by_job.push(count);
            owners.extend(std::iter::repeat_n(i as u32, count));
            if probing {
                indices.extend(checks);
            } else {
                indices.extend(0..runs);
            }
            references.push(reference);
        }
        let count = indices.len();
        ensure!(count <= MAX_RUNS, "GPU batch too large");
        let rounded = count.div_ceil(32) * 32;
        let capacity = maximum + 8 * 1024 * 1024;
        let virtual_pages = capacity / self.layout.map_region_bytes;
        let map_bytes = rounded * virtual_pages * self.layout.index_bytes;
        // Snapshot and workspace slots never overlap, including 32-row batches.
        let workspace = MAX_ROWS;
        let mut djobs = self
            .gpu
            .buffer(workspace, std::mem::size_of_val(jobs.as_slice()))?;
        self.gpu.upload(djobs, &jobs)?;
        let mut downers = self.gpu.buffer(workspace+1, count * 4)?;
        self.gpu.upload(downers, &owners)?;
        let mut dindices = self.gpu.buffer(workspace+2, count * 4)?;
        self.gpu.upload(dindices, &indices)?;
        let private_kib = inputs.iter().map(|i| i.private_kib).max().unwrap_or(192);
        let mut private = self.gpu.buffer(workspace+3, rounded * private_kib as usize * 1024)?;
        let mut map = self.gpu.buffer(workspace+4, map_bytes)?;
        self.gpu.clear(map, map_bytes)?;
        let mut outputs = self.gpu.buffer(workspace+5, count * 8192)?;
        let mut sizes = self.gpu.buffer(workspace+6, count * 4)?;
        let mut pages = self.gpu.buffer(workspace+7, count * 4)?;
        let mut statuses = self.gpu.buffer(workspace+8, count * 4)?;
        self.gpu
            .run("sf_setup", 1, 32, &mut [parameter(&mut statuses)])?;
        let mut cap = capacity as u64;
        let mut private_pages = (private_kib as usize * 1024 / self.layout.page_bytes) as u32;
        let mut vpages = virtual_pages as u32;
        let mut n = count as u32;
        let mut stride = 8192u32;
        progress("compute")?;
        let prepare_ms = began.elapsed().as_millis() as u64;
        let gpu_began = Instant::now();
        self.gpu.run(
            "sf_board_packed",
            count as u32,
            if probing { 1 } else { self.block_threads },
            &mut [
                parameter(&mut djobs),
                parameter(&mut downers),
                parameter(&mut dindices),
                parameter(&mut private),
                parameter(&mut map),
                parameter(&mut cap),
                parameter(&mut private_pages),
                parameter(&mut vpages),
                parameter(&mut n),
                parameter(&mut outputs),
                parameter(&mut stride),
                parameter(&mut sizes),
                parameter(&mut pages),
                parameter(&mut statuses),
            ],
        )?;
        progress("merge")?;
        let gpu_ms = gpu_began.elapsed().as_secs_f64() * 1000.0;
        let download_began = Instant::now();
        let mut status = vec![0u32; count];
        self.gpu.download(&mut status, statuses)?;
        ensure!(
            status.iter().all(|s| matches!(*s, 0 | 2 | 120)),
            "GPU run failed, status {:?}",
            status.iter().find(|s| **s != 0)
        );
        let mut lengths = vec![0u32; count];
        self.gpu.download(&mut lengths, sizes)?;
        ensure!(
            lengths
                .iter()
                .zip(&status)
                .all(|(n, s)| *s != 0 || (*n > 0 && *n <= 8192)),
            "invalid GPU output lengths"
        );
        let mut data = vec![0u8; count * 8192];
        self.gpu.download(&mut data, outputs)?;
        let mut used = vec![0u32; count];
        self.gpu.download(&mut used, pages)?;
        let download_ms = download_began.elapsed().as_millis() as u64;
        let merge_began = Instant::now();
        let mut offset = 0;
        let mut results = Vec::new();
        for (i, input) in inputs.iter().enumerate() {
            let n = lengths_by_job[i];
            if let Some(code) = status[offset..offset + n].iter().copied().find(|s| *s != 0) {
                results.push(Output {
                    id: input.id,
                    score: Value::Null,
                    compute_ms: 0,
                    fallback: Some(code),
                    probe: None,
                    max_private_kib: None,
                });
                offset += n;
                continue;
            }
            for (j, reference) in references[i].iter().enumerate() {
                let at = offset + if probing { j } else { 0 };
                let sample = &data[at * 8192..at * 8192 + lengths[at] as usize];
                ensure!(
                    self.wire.decode(sample)? == self.wire.decode(reference)?,
                    "GPU row {} differs from WASM",
                    input.id
                );
            }
            if probing {
                results.push(Output {
                    id: input.id,
                    score: Value::Null,
                    compute_ms: began.elapsed().as_millis() as u64,
                    fallback: None,
                    probe: Some(super::Probe {
                        samples: n as u32,
                        cpu_ms,
                        gpu_ms,
                        max_pages: used.iter().copied().max().unwrap_or(0),
                    }),
                    max_private_kib: None,
                });
                offset += n;
                continue;
            }
            let score = self.wire.score(
                input,
                &data[offset * 8192..(offset + n) * 8192],
                &lengths[offset..offset + n],
            )?;
            super::validate_score(&score)?;
            results.push(Output {
                id: input.id,
                score,
                compute_ms: 0,
                fallback: None,
                probe: None,
                max_private_kib: Some(used[offset..offset+n].iter().copied().max().unwrap_or(0)
                    * self.layout.page_bytes as u32 / 1024),
            });
            offset += n;
        }
        let each = (began.elapsed().as_millis() as u64).div_ceil(inputs.len() as u64);
        for out in &mut results {
            out.compute_ms = each;
        }
        Ok((results, super::BatchTiming {
            prepare_ms, gpu_ms: gpu_ms as u64, download_ms,
            merge_ms: merge_began.elapsed().as_millis() as u64,
        }))
    }
}

pub fn run(dir: &Path, engine_id: &str, native_path: Option<&Path>, memory_mib: u64) -> Result<()> {
    let (_job, memory_limit_bytes) = memory_job(memory_mib)?;
    let _device = super::driver::device_lock()?;
    let manifest = Manifest::load(dir, engine_id)?;
    ensure!(crate::work::cpu_v3(), "GPU wire requires x86-64-v3/AVX2");
    let mut config = wasmtime::Config::new();
    config.cranelift_opt_level(wasmtime::OptLevel::Speed);
    let engine = Engine::new(&config).map_err(we)?;
    let bytes = std::fs::read(dir.join("bridge.wasm"))?;
    let (module, _) = bridge_module(&engine, &bytes, dir)?;
    ensure!(module.imports().len() == 0, "unexpected GPU bridge imports");
    let bridge = Bridge::new(&engine, &module, manifest.gas_address)?;
    let wire = Wire::open(dir)?;
    let gpu = Cuda::open(&dir.join("engine.cubin"), &manifest.architecture)?;
    let native = native_path
        .map(|path| super::native_probe::Native::open(path, engine_id))
        .transpose()?;
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "{}",
        json!({"ready":true,"device":gpu.name,"architecture":manifest.architecture,"memory_free":gpu.memory_info()?.0,"memory_limit_bytes":memory_limit_bytes,"base_commit_bytes":crate::sys::process_memory(),"engine":engine_id,"revision":manifest.revision,"driver":gpu.driver_version()?,"probe":native.is_some(),"cow_page_bytes":manifest.cow_page_bytes,"page_index_bytes":manifest.page_index_bytes,"page_map_region_bytes":manifest.layout()?.map_region_bytes})
    )?;
    stdout.flush()?;
    let mut runtime = Runtime {
        gpu,
        bridge,
        wire,
        native,
        block_threads: manifest.block_threads,
        layout: manifest.layout()?,
    };
    let mut reader = std::io::stdin().lock();
    let mut line = String::new();
    let mut batches = 0usize;
    loop {
        line.clear();
        if std::io::Read::take(&mut reader, 4 << 20).read_line(&mut line)? == 0 {
            break;
        }
        ensure!(
            line.ends_with('\n') && line.len() < 4 << 20,
            "GPU input frame too large"
        );
        let inputs: Vec<Input> = serde_json::from_str(&line)?;
        let id = inputs.first().map(|i| i.id);
        let (out, timing) = runtime.batch(&inputs, &mut |phase| {
            writeln!(stdout, "{}", json!({"phase":phase,"id":id}))?;
            stdout.flush()?;
            Ok(())
        })?;
        writeln!(stdout, "{}", json!({"rows":out,"timing":timing}))?;
        stdout.flush()?;
        // The linear heap cannot shrink. Periodic recreation releases its high-water mark.
        batches += 1;
        if batches % 64 == 0 {
            runtime.bridge = Bridge::new(&engine, &module, manifest.gas_address)?;
        }
        ensure!(inputs.len() <= MAX_ROWS, "GPU row limit");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bridge_cache_reuses_compatible_code_and_recovers_a_broken_file() {
        let dir=std::env::temp_dir().join(format!("simfleet-bridge-{}",crate::sys::random_hex(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let engine=Engine::default();
        let wasm=b"\0asm\x01\0\0\0";
        assert!(!bridge_module(&engine,wasm,&dir).unwrap().1);
        assert!(bridge_module(&engine,wasm,&dir).unwrap().1);
        let path=std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        std::fs::write(path,b"incomplete").unwrap();
        assert!(!bridge_module(&engine,wasm,&dir).unwrap().1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
