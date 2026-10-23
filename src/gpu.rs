// SPDX-License-Identifier: AGPL-3.0-or-later
//! Optional, engine-bound NVIDIA worker. Leases and result ownership stay in Fleet.
#[cfg(all(windows, target_arch = "x86_64"))]
mod driver;
#[cfg(all(windows, target_arch = "x86_64"))]
mod native_probe;
mod policy;
#[cfg(all(windows, target_arch = "x86_64"))]
mod worker;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub const MAX_ROWS: usize = 16;
pub const MAX_RUNS: usize = 16_000;
fn architecture_suffix(architecture: &str) -> Option<&str> {
    architecture.strip_prefix("compute_").filter(|suffix| {
        (2..=3).contains(&suffix.len()) && suffix.bytes().all(|b| b.is_ascii_digit())
    })
}
fn device_run_limit(available_bytes: u64, private_kib: u32) -> usize {
    // Reserve snapshots, maps, retained buffers and desktop/display allocations.
    let available = available_bytes.saturating_sub(1536 << 20);
    ((available / ((u64::from(private_kib) + 128) * 1024)) as usize / 1000) * 1000
}
/// Reserve driver/module/snapshots and CPU growth before sizing mutable arenas.
/// The Windows Job remains the actual hard limit if measurements change.
pub fn memory_run_limit(total_mib: u64, cpu_bytes: u64, private_kib: u32) -> usize {
    let remaining =
        (total_mib << 20).saturating_sub(cpu_bytes.max(512 << 20).saturating_add(3072 << 20));
    let affordable = (remaining / (u64::from(private_kib) + 64) / 1024) as usize;
    (affordable.min(MAX_RUNS * 192 / private_kib as usize) / 1000) * 1000
}
/// Temporary failures back off without permanently disabling a long run.
/// Only a minute and eight successful batches reset an accumulated streak.
#[derive(Default)]
pub struct Recovery {
    failures: u32,
    healthy_since: Option<Instant>,
    healthy_batches: u32,
}
impl Recovery {
    pub fn failure(&mut self, probe_timeout: bool) -> Duration {
        // An unknown workload exceeding its exploration budget says nothing
        // about the health of the GPU. Do not penalize other learned contexts.
        if probe_timeout {
            return Duration::from_secs(15);
        }
        self.failures = self.failures.saturating_add(1);
        self.healthy_since = None;
        self.healthy_batches = 0;
        Duration::from_secs(match self.failures {
            1 => 15,
            2 => 60,
            3 => 300,
            _ => 600,
        })
    }
    pub fn success(&mut self, now: Instant) {
        let start = *self.healthy_since.get_or_insert(now);
        self.healthy_batches = self.healthy_batches.saturating_add(1);
        if self.healthy_batches >= 8 && now.duration_since(start) >= Duration::from_secs(60) {
            self.failures = 0;
        }
    }
}
#[derive(Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub engine: String,
    pub revision: u32,
    pub architecture: String,
    pub gas_address: usize,
    pub wire_format: u32,
    pub host: String,
    pub files: BTreeMap<String, String>,
}
impl Manifest {
    pub fn load(dir: &Path, engine: &str) -> Result<Self> {
        let m: Self = serde_json::from_slice(&std::fs::read(dir.join("engine.gpu.json"))?)?;
        ensure!(
            m.schema == 1 && m.revision == 1 && m.wire_format == 1,
            "unsupported GPU package ABI"
        );
        ensure!(
            m.engine == engine && m.host == "windows-x64-v3",
            "GPU engine or host mismatch"
        );
        ensure!(
            architecture_suffix(&m.architecture).is_some(),
            "invalid GPU architecture"
        );
        for (file, max) in [
            ("engine.cubin", 128 << 20),
            ("bridge.wasm", 32 << 20),
            ("wire.dll", 32 << 20),
        ] {
            let p = dir.join(file);
            ensure!(
                std::fs::metadata(&p)?.len() <= max,
                "oversized GPU file {file}"
            );
            let hash = format!("{:x}", Sha256::digest(std::fs::read(p)?));
            ensure!(
                m.files.get(file) == Some(&hash),
                "GPU hash mismatch: {file}"
            );
        }
        Ok(m)
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Input {
    pub id: u64,
    pub request: Value,
    pub ruler: String,
    #[serde(default = "default_private_kib")]
    pub private_kib: u32,
    #[serde(default)]
    pub probe: bool,
}
fn default_private_kib() -> u32 {
    192
}
#[derive(Serialize, Deserialize)]
pub struct Output {
    pub id: u64,
    pub score: Value,
    pub compute_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<Probe>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Probe {
    pub samples: u32,
    pub cpu_ms: f64,
    pub gpu_ms: f64,
    pub max_pages: u32,
}
pub fn sample_indices(runs: u32) -> Vec<u32> {
    let n = runs.min(8);
    (0..n)
        .map(|i| {
            if n <= 1 {
                0
            } else {
                ((runs - 1) as u64 * i as u64 / (n - 1) as u64) as u32
            }
        })
        .collect()
}
pub fn eligible(request: &Value, ruler: &str) -> bool {
    !ruler.is_empty()
        && ruler.len() <= 128
        && request.is_object()
        && request["weapon"]
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 128)
        && request["duration"]
            .as_f64()
            .is_some_and(|s| s.is_finite() && s > 0.0 && s <= 600.0)
        && request["runs"]
            .as_u64()
            .is_some_and(|n| (1..=1000).contains(&n))
}
pub fn validate_inputs(inputs: &[Input]) -> Result<()> {
    ensure!(
        !inputs.is_empty() && inputs.len() <= MAX_ROWS,
        "GPU batch must contain 1..={MAX_ROWS} rows"
    );
    let mut ids = std::collections::HashSet::new();
    let mut runs = 0;
    ensure!(
        !inputs.iter().any(|i| i.probe) || inputs.len() == 1,
        "probe must be isolated"
    );
    for input in inputs {
        ensure!(ids.insert(input.id), "duplicate GPU row id");
        ensure!(
            eligible(&input.request, &input.ruler),
            "row exceeds GPU input bounds"
        );
        runs += if input.probe {
            input.request["runs"].as_u64().unwrap_or(0).min(8) as usize
        } else {
            input.request["runs"].as_u64().unwrap_or(0) as usize
        };
        ensure!(
            [192, 384, 768, 1536].contains(&input.private_kib),
            "invalid GPU private memory budget"
        );
    }
    let kib = inputs.iter().map(|i| i.private_kib).max().unwrap_or(192) as usize;
    ensure!(
        runs <= MAX_RUNS && runs * kib <= MAX_RUNS * 192,
        "GPU run budget exceeded"
    );
    Ok(())
}
pub fn validate_score(v: &Value) -> Result<()> {
    ensure!(
        v["ok"] != false
            && v["score"]
                .as_f64()
                .is_some_and(|n| n.is_finite() && n >= 0.0)
            && v["metric"].as_str().is_some_and(|s| !s.is_empty())
            && v["work"]
                .as_u64()
                .is_some_and(|n| n <= 9_007_199_254_740_991),
        "invalid GPU score"
    );
    Ok(())
}
fn validate_outputs(value: &Value, pending: &[u64]) -> Result<Vec<Output>> {
    ensure!(!pending.is_empty(), "unexpected GPU response");
    let rows: Vec<Output> = serde_json::from_value(value["rows"].clone())?;
    ensure!(
        rows.len() == pending.len() && rows.iter().zip(pending).all(|(r, id)| r.id == *id),
        "GPU returned different task ids"
    );
    for row in &rows {
        if let Some(probe) = &row.probe {
            ensure!(
                row.fallback.is_none()
                    && row.score.is_null()
                    && probe.samples > 0
                    && probe.samples <= 8
                    && probe.cpu_ms.is_finite()
                    && probe.cpu_ms > 0.0
                    && probe.gpu_ms.is_finite()
                    && probe.gpu_ms >= 0.0
                    && probe.max_pages <= 384,
                "invalid GPU probe response"
            );
        } else if let Some(code) = row.fallback {
            ensure!(
                [2, 120].contains(&code) && row.score.is_null(),
                "invalid GPU fallback response"
            );
        } else {
            validate_score(&row.score)?;
        }
    }
    Ok(rows)
}
pub fn directory(engines: &Path, engine: &str) -> PathBuf {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        static ARCH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        let arch = ARCH.get_or_init(|| driver::architecture().ok());
        return directory_for(engines, engine, arch.as_deref().unwrap_or("unsupported"));
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    directory_for(engines, engine, "unsupported")
}
fn directory_for(engines: &Path, engine: &str, architecture: &str) -> PathBuf {
    let suffix = architecture_suffix(architecture).unwrap_or("unsupported");
    engines
        .join("gpu")
        .join(format!("{engine}-sfg1-sm{suffix}"))
}
pub fn worker_main(dir: &Path, engine: &str, native: Option<&Path>) -> Result<()> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        worker::run(dir, engine, native)
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        let _ = (dir, engine, native);
        anyhow::bail!("GPU preview currently requires Windows x64");
    }
}

pub enum Event {
    Ready(String),
    Progress,
    Finished(Vec<Output>),
    Failed(String),
}
pub struct Manager {
    child: Child,
    input: ChildStdin,
    rx: mpsc::Receiver<Result<Value>>,
    ready: bool,
    failed: bool,
    pending: Vec<u64>,
    since: Instant,
    meter: Option<crate::gpu_meter::Monitor>,
    device: String,
    phase: String,
    pending_runs: u64,
    finished: u64,
    private_kib: u32,
    pending_keys: Vec<[u8; 32]>,
    excluded: std::collections::VecDeque<([u8; 32], u64)>,
    pub retryable: bool,
    pub probe_timeout: bool,
    device_available_bytes: u64,
    fallback_count: u64,
    last_error: String,
    report_dir: PathBuf,
    pending_inputs: Vec<Input>,
    engine: String,
    policy: Option<policy::Policy>,
    module_binding: String,
    probing: bool,
    probe_supported: bool,
}
#[derive(Clone, Default)]
pub struct View {
    pub enabled: bool,
    pub state: String,
    pub device: String,
    pub rows: usize,
    pub runs: u64,
    pub seconds: u64,
    pub finished: u64,
    pub fallback: u64,
    pub reason: String,
    pub sample: crate::gpu_meter::Sample,
}
impl Manager {
    pub fn new(dir: &Path, engine: &str) -> Result<Self> {
        Self::spawn(&std::env::current_exe()?, dir, engine)
    }
    pub fn spawn(exe: &Path, dir: &Path, engine: &str) -> Result<Self> {
        let mut cmd = Command::new(exe);
        cmd.arg("gpu-worker")
            .arg(dir)
            .arg(engine)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let native = exe
            .parent()
            .context("client folder")?
            .join("engines")
            .join(crate::work::native_lib_name(engine, "-v3"));
        if native.is_file() {
            cmd.arg("--native").arg(&native);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let mut child = cmd.spawn()?;
        let input = child.stdin.take().context("GPU stdin")?;
        let output = child.stdout.take().context("GPU stdout")?;
        let (tx, rx) = mpsc::sync_channel(2);
        std::thread::Builder::new()
            .name("gpu-output".into())
            .spawn(move || {
                let mut reader = std::io::BufReader::new(output);
                loop {
                    let mut line = String::new();
                    let value = (|| -> Result<Value> {
                        let n = std::io::Read::take(&mut reader, 4 << 20).read_line(&mut line)?;
                        ensure!(n > 0, "GPU worker closed output");
                        if n >= 4 << 20 || !line.ends_with('\n') {
                            return Ok(serde_json::json!({"error":"invalid GPU output frame"}));
                        }
                        Ok(serde_json::from_str(&line).unwrap_or_else(
                            |_| serde_json::json!({"error":"invalid GPU output JSON"}),
                        ))
                    })();
                    let ended = value.is_err();
                    if tx.send(value).is_err() || ended {
                        break;
                    }
                }
            })?;
        let meter = Some(crate::gpu_meter::Monitor::new(child.id()));
        Ok(Self {
            child,
            input,
            rx,
            ready: false,
            failed: false,
            pending: Vec::new(),
            since: Instant::now(),
            meter,
            device: String::new(),
            phase: "初始化".into(),
            pending_runs: 0,
            finished: 0,
            private_kib: 192,
            pending_keys: Vec::new(),
            excluded: std::collections::VecDeque::new(),
            retryable: false,
            probe_timeout: false,
            device_available_bytes: u64::MAX,
            fallback_count: 0,
            last_error: String::new(),
            report_dir: exe
                .parent()
                .context("GPU executable parent")?
                .join("gpu-diagnostics"),
            pending_inputs: Vec::new(),
            engine: engine.into(),
            policy: None,
            module_binding: format!(
                "{:x}:{:x}",
                Sha256::digest(std::fs::read(dir.join("engine.gpu.json")).unwrap_or_default()),
                Sha256::digest(std::fs::read(native).unwrap_or_default())
            ),
            probing: false,
            probe_supported: false,
        })
    }
    pub fn available(&self) -> bool {
        self.ready && !self.failed && self.pending.is_empty()
    }
    pub fn disabled(&self) -> bool {
        self.failed
    }
    pub fn active(&self) -> bool {
        self.ready && !self.failed
    }
    pub fn view(&self) -> View {
        View {
            enabled: true,
            state: self.phase.clone(),
            device: self.device.clone(),
            rows: self.pending.len(),
            runs: self.pending_runs,
            seconds: if self.pending.is_empty() {
                0
            } else {
                self.since.elapsed().as_secs()
            },
            finished: self.finished,
            fallback: self.fallback_count,
            reason: self.last_error.clone(),
            sample: self
                .meter
                .as_ref()
                .map_or_else(crate::gpu_meter::Sample::default, |m| m.read()),
        }
    }
    fn key(input: &Input) -> [u8; 32] {
        Sha256::digest(
            serde_json::to_vec(&serde_json::json!([input.ruler, input.request]))
                .unwrap_or_default(),
        )
        .into()
    }
    pub fn accepts(&self, input: &Input) -> bool {
        !self
            .excluded
            .iter()
            .any(|(k, until)| *k == Self::key(input) && *until > crate::sys::now_ms())
            && self
                .policy
                .as_ref()
                .is_some_and(|p| p.allowed(input, crate::sys::now_ms()))
    }
    pub fn select(&self, candidates: &[Input], max_runs: usize) -> Vec<Input> {
        let now = crate::sys::now_ms();
        let Some(policy) = &self.policy else {
            return vec![];
        };
        let mut full = Vec::new();
        let mut count = 0;
        let mut budget = 192;
        let mut chosen = None;
        for row in candidates.iter().filter(|i| self.accepts(i)) {
            let family = policy::family(row);
            if chosen.as_ref().is_some_and(|key| *key != family) {
                continue;
            }
            chosen = Some(family);
            let next_budget = budget.max(policy.budget(row, now));
            let runs = row.request["runs"].as_u64().unwrap_or(0) as usize;
            if full.len() >= MAX_ROWS
                || count + runs
                    > max_runs
                        .min(MAX_RUNS * 192 / next_budget as usize)
                        .min(device_run_limit(self.device_available_bytes, next_budget))
            {
                break;
            }
            let mut row = row.clone();
            row.private_kib = next_budget;
            full.push(row);
            count += runs;
            budget = next_budget;
        }
        // Productive batches take priority over exploring another context.
        // Otherwise a stream of slow probes could keep known work waiting.
        if count
            >= 4000
                .min(max_runs)
                .min(MAX_RUNS * 192 / budget as usize)
                .min(device_run_limit(self.device_available_bytes, budget))
            && !full.is_empty()
        {
            return full;
        }
        if self.probe_supported && policy.can_probe(now) {
            if let Some(row) = candidates.iter().find(|i| {
                !self
                    .excluded
                    .iter()
                    .any(|(k, until)| *k == Self::key(i) && *until > now)
                    && policy.classify(i, now).is_none()
            }) {
                let mut row = row.clone();
                row.probe = true;
                row.private_kib = 1536;
                return vec![row];
            }
        }
        full
    }
    pub fn learning_summary(&self) -> (usize, usize) {
        self.policy
            .as_ref()
            .map_or((0, 0), |p| p.summary(crate::sys::now_ms()))
    }
    pub fn max_runs(&self) -> usize {
        self.max_runs_for(self.private_kib)
    }
    pub fn max_runs_for(&self, private_kib: u32) -> usize {
        (MAX_RUNS * 192 / private_kib as usize)
            .min(device_run_limit(self.device_available_bytes, private_kib))
    }
    pub fn budget(&self) -> u32 {
        self.private_kib
    }
    pub fn send_failed(&mut self, e: impl std::fmt::Display) {
        let _ = self.transient(e);
    }
    pub fn restart(&self, dir: &Path, engine: &str) -> Result<Self> {
        let mut fresh = Self::new(dir, engine)?;
        fresh.private_kib = self.private_kib;
        fresh.excluded = self.excluded.clone();
        fresh.finished = self.finished;
        fresh.fallback_count = self.fallback_count;
        fresh.last_error = self.last_error.clone();
        Ok(fresh)
    }
    pub fn submit(&mut self, inputs: &[Input]) -> Result<()> {
        ensure!(self.available(), "GPU worker is busy");
        let inputs: Vec<_> = inputs
            .iter()
            .cloned()
            .map(|mut i| {
                if !i.probe {
                    i.private_kib = i.private_kib.max(self.private_kib);
                }
                i
            })
            .collect();
        validate_inputs(&inputs)?;
        self.probing = inputs[0].probe;
        if self.probing {
            if let Some(p) = &mut self.policy {
                p.begin_probe(&inputs[0], crate::sys::now_ms());
            }
        }
        let text = serde_json::to_vec(&inputs)?;
        ensure!(text.len() < 4 << 20, "GPU input frame too large");
        self.input.write_all(&text)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        self.pending = inputs.iter().map(|v| v.id).collect();
        self.pending_keys = inputs.iter().map(Self::key).collect();
        self.pending_inputs = inputs.clone();
        self.pending_runs = inputs
            .iter()
            .map(|v| {
                if v.probe {
                    v.request["runs"].as_u64().unwrap_or(0).min(8)
                } else {
                    v.request["runs"].as_u64().unwrap_or(0)
                }
            })
            .sum();
        self.phase = if self.probing {
            "探测准备"
        } else {
            "准备"
        }
        .into();
        self.since = Instant::now();
        Ok(())
    }
    fn failure(&mut self, e: impl std::fmt::Display) -> Event {
        let error = e.to_string();
        self.last_error = error
            .chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect();
        let exit = self.child.try_wait().ok().flatten().map(|s| s.to_string());
        self.persist_failure(&error, exit.as_deref());
        self.failed = true;
        self.ready = false;
        self.pending.clear();
        self.pending_runs = 0;
        self.phase = if self.retryable {
            "临时故障，待恢复"
        } else {
            "错误停用，需检查"
        }
        .into();
        self.meter = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.pending_inputs.clear();
        Event::Failed(error)
    }
    fn persist_failure(&self, error: &str, exit: Option<&str>) {
        let report = serde_json::json!({"at_ms":crate::sys::now_ms(),"version":env!("CARGO_PKG_VERSION"),"engine":self.engine,
            "error":error.chars().take(4096).collect::<String>(),"retryable":self.retryable,"probe_timeout":self.probe_timeout,"phase":self.phase,"exit":exit,"private_kib":self.private_kib,
            "completed_rows":self.finished,"fallback_rows":self.fallback_count,"requests":self.pending_inputs});
        if let Err(e) = (|| -> Result<()> {
            std::fs::create_dir_all(&self.report_dir)?;
            let path = self.report_dir.join("last-failure.json");
            let previous = self.report_dir.join("previous-failure.json");
            let bytes = serde_json::to_vec_pretty(&report)?;
            ensure!(bytes.len() < 8 << 20, "GPU failure report too large");
            let tmp = self.report_dir.join("last-failure.tmp");
            std::fs::write(&tmp, bytes)?;
            if path.exists() {
                if previous.exists() {
                    std::fs::remove_file(&previous)?;
                }
                std::fs::rename(&path, &previous)?;
            }
            std::fs::rename(tmp, path)?;
            Ok(())
        })() {
            eprintln!("GPU failure evidence: {e}");
        }
    }
    fn transient(&mut self, e: impl std::fmt::Display) -> Event {
        self.retryable = true;
        if let Some(policy) = &mut self.policy {
            policy.failed_batch(&self.pending_inputs, &e.to_string(), crate::sys::now_ms());
        }
        for key in &self.pending_keys {
            if self.excluded.len() >= 512 {
                self.excluded.pop_front();
            }
            self.excluded
                .push_back((*key, crate::sys::now_ms() + 30 * 60 * 1000));
        }
        self.failure(e)
    }
    pub fn poll(&mut self) -> Option<Event> {
        if self.failed {
            return None;
        }
        match self.rx.try_recv() {
            Ok(Ok(value)) => {
                if let Some(error) = value["error"].as_str() {
                    let transient = error.starts_with("CUDA driver error")
                        || error.contains("another simfleet already owns GPU");
                    return Some(if transient {
                        self.transient(error)
                    } else {
                        self.failure(error)
                    });
                }
                if value["ready"] == true && !self.ready {
                    self.probe_supported = value["probe"] == true;
                    self.device_available_bytes = value["memory_free"].as_u64().unwrap_or(0);
                    self.ready = true;
                    self.phase = "待机".into();
                    self.device = value["device"].as_str().unwrap_or("NVIDIA").into();
                    let binding = format!(
                        "adaptive-v3:{}:{}:{}:{}:{}",
                        self.engine,
                        self.module_binding,
                        self.device,
                        value["driver"],
                        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default()
                    );
                    self.policy = Some(policy::Policy::open(
                        &self
                            .report_dir
                            .parent()
                            .unwrap_or(&self.report_dir)
                            .join("gpu-policy.json"),
                        binding,
                    ));
                    self.since = Instant::now();
                    return Some(Event::Ready(
                        value["device"].as_str().unwrap_or("NVIDIA").into(),
                    ));
                }
                if let Some(phase) = value["phase"].as_str() {
                    let translated = match phase {
                        "prepare" => "准备",
                        "compute" => "计算",
                        "merge" => "合并",
                        _ => return Some(self.failure("invalid GPU phase")),
                    };
                    if !self.ready || self.pending.first().copied() != value["id"].as_u64() {
                        return Some(self.failure("GPU progress belongs to a different batch"));
                    }
                    self.phase = if self.probing {
                        format!("探测{translated}")
                    } else {
                        translated.into()
                    };
                    return Some(Event::Progress);
                }
                let parsed = (|| -> Result<Vec<Output>> {
                    ensure!(
                        self.ready && !self.pending.is_empty(),
                        "unexpected GPU response"
                    );
                    validate_outputs(&value, &self.pending)
                })();
                match parsed {
                    Ok(rows) => {
                        if rows
                            .iter()
                            .zip(&self.pending_inputs)
                            .any(|(r, i)| r.probe.is_some() != i.probe && r.fallback.is_none())
                        {
                            return Some(self.failure("GPU probe/full response mismatch"));
                        }
                        if rows.iter().zip(&self.pending_inputs).any(|(r, i)| {
                            r.probe.as_ref().is_some_and(|p| {
                                Some(p.samples as u64)
                                    != i.request["runs"].as_u64().map(|n| n.min(8))
                            })
                        }) {
                            return Some(self.failure("GPU probe sample count mismatch"));
                        }
                        let now = crate::sys::now_ms();
                        if let Some(policy) = &mut self.policy {
                            let mut failed_families = std::collections::BTreeSet::new();
                            for (row, input) in rows.iter().zip(&self.pending_inputs) {
                                if let Some(probe) = &row.probe {
                                    policy.observed(input, probe, now);
                                } else if let Some(code) = row.fallback {
                                    if failed_families.insert(policy::family(input)) {
                                        policy.failed(input, &format!("row status {code}"), now);
                                    }
                                } else {
                                    policy.completed(input, row.compute_ms, now);
                                }
                            }
                        }
                        for (row, key) in rows.iter().zip(&self.pending_keys) {
                            if row.probe.is_some() {
                                continue;
                            }
                            if row.fallback.is_some() {
                                self.fallback_count += 1;
                                if self.excluded.len() >= 512 {
                                    self.excluded.pop_front();
                                }
                                self.excluded.push_back((*key, now + 30 * 60 * 1000));
                            } else {
                                self.finished += 1;
                            }
                        }
                        // Per-family memory learning must not shrink all other work.
                        self.phase = "待机".into();
                        self.pending_runs = 0;
                        self.pending.clear();
                        self.pending_keys.clear();
                        self.pending_inputs.clear();
                        self.since = Instant::now();
                        Some(Event::Finished(rows))
                    }
                    Err(e) => Some(self.failure(e)),
                }
            }
            Ok(Err(e)) => Some(self.transient(e)),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(self.transient("GPU worker disconnected"))
            }
            Err(mpsc::TryRecvError::Empty) => {
                let limit = if self.ready {
                    Duration::from_secs(if self.probing { 8 } else { 30 })
                } else {
                    Duration::from_secs(180)
                };
                if (!self.ready || !self.pending.is_empty()) && self.since.elapsed() > limit {
                    self.probe_timeout = self.ready && self.probing && !self.pending.is_empty();
                    Some(self.transient(if self.probe_timeout {
                        "GPU probe timed out; context deferred to CPU"
                    } else {
                        "GPU timed out; rows return to CPU"
                    }))
                } else {
                    None
                }
            }
        }
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temporary_failures_keep_retrying_and_stable_success_resets_backoff() {
        let mut recovery = Recovery::default();
        for seconds in [15, 60, 300, 600, 600] {
            assert_eq!(recovery.failure(false), Duration::from_secs(seconds));
        }
        let now = Instant::now();
        recovery.success(now);
        recovery.success(now + Duration::from_secs(61));
        assert_eq!(recovery.failure(false), Duration::from_secs(600));
        for n in 0..8 {
            recovery.success(now + Duration::from_secs(n * 10));
        }
        assert_eq!(recovery.failure(false), Duration::from_secs(15));
    }
    #[test]
    fn slow_unknown_contexts_do_not_extend_or_reset_fault_backoff() {
        let mut recovery = Recovery::default();
        assert_eq!(recovery.failure(false), Duration::from_secs(15));
        for _ in 0..20 {
            assert_eq!(recovery.failure(true), Duration::from_secs(15));
        }
        assert_eq!(recovery.failure(false), Duration::from_secs(60));
    }
    #[test]
    fn total_budget_sizes_batches_before_the_job_limit_is_hit() {
        assert_eq!(memory_run_limit(8192, 256 << 20, 192), 16000);
        assert_eq!(memory_run_limit(8192, 256 << 20, 1536), 2000);
        assert_eq!(memory_run_limit(4096, 256 << 20, 192), 2000);
        assert_eq!(memory_run_limit(4096, 256 << 20, 1536), 0);
        assert_eq!(memory_run_limit(8192, 8 << 30, 192), 0);
    }
    #[test]
    fn gpu_packages_match_the_device_and_small_vram_limits_batches() {
        let root = Path::new("engines");
        assert_eq!(
            directory_for(root, "abc", "compute_75"),
            root.join("gpu/abc-sfg1-sm75")
        );
        assert_eq!(
            directory_for(root, "abc", "compute_120"),
            root.join("gpu/abc-sfg1-sm120")
        );
        assert!(architecture_suffix("compute_../../bad").is_none());
        assert!(architecture_suffix("sm75").is_none());
        assert_eq!(device_run_limit(5 << 30, 384), 7000);
        assert_eq!(device_run_limit(2 << 30, 384), 1000);
        assert_eq!(device_run_limit(1 << 30, 384), 0);
    }
    #[test]
    fn mismatched_or_partial_batches_are_never_accepted() {
        let score = serde_json::json!({"score":1.25,"metric":"kpm","work":100});
        let result = serde_json::json!({"rows":[{"id":1,"score":score,"compute_ms":1},{"id":2,"score":score,"compute_ms":1}]});
        assert!(validate_outputs(&result, &[1, 2]).is_ok());
        for pending in [&[][..], &[1][..], &[2, 1][..], &[1, 3][..], &[1, 2, 3][..]] {
            assert!(validate_outputs(&result, pending).is_err());
        }
        let mut duplicate = result.clone();
        duplicate["rows"][1]["id"] = serde_json::json!(1);
        assert!(validate_outputs(&duplicate, &[1, 2]).is_err());
        let recovered = serde_json::json!({"rows":[{"id":1,"score":null,"compute_ms":3,"fallback":120},{"id":2,"score":score,"compute_ms":1}]});
        let rows = validate_outputs(&recovered, &[1, 2]).unwrap();
        assert_eq!(rows[0].fallback, Some(120));
        assert!(rows[1].fallback.is_none());
        let mut forged = recovered.clone();
        forged["rows"][0]["score"] = score.clone();
        assert!(validate_outputs(&forged, &[1, 2]).is_err());
        let mut forged = recovered;
        forged["rows"][0]["fallback"] = serde_json::json!(105);
        assert!(validate_outputs(&forged, &[1, 2]).is_err());
    }
    fn row(id: u64) -> Input {
        Input {
            id,
            private_kib: 192,
            probe: false,
            ruler: "heavy_gunner".into(),
            request: serde_json::json!({"weapon":"fulmin_prime","mode":"semi_auto","enemy":"corrupted_heavy_gunner","duration":180,"runs":1000}),
        }
    }
    #[test]
    fn selection_and_bounds() {
        assert!(validate_inputs(&[row(1)]).is_ok());
        assert!(validate_inputs(&[]).is_err());
        assert!(validate_inputs(&[row(1), row(1)]).is_err());
        assert!(validate_inputs(&(0..17).map(row).collect::<Vec<_>>()).is_err());
        let wide: Vec<_> = (0..4)
            .map(|n| {
                let mut r = row(n);
                r.private_kib = 768;
                r
            })
            .collect();
        assert!(validate_inputs(&wide).is_ok());
        let mut over = wide;
        let mut extra = row(5);
        extra.private_kib = 768;
        over.push(extra);
        assert!(validate_inputs(&over).is_err());
        let large: Vec<_> = (0..2)
            .map(|n| {
                let mut r = row(n);
                r.private_kib = 1536;
                r
            })
            .collect();
        assert!(validate_inputs(&large).is_ok());
        let mut over = large;
        let mut r = row(9);
        r.private_kib = 1536;
        over.push(r);
        assert!(validate_inputs(&over).is_err());
        let mut r = row(1);
        r.request["weapon"] = serde_json::json!("torid");
        assert!(validate_inputs(&[r]).is_ok());
        let mut r = row(1);
        r.request["runs"] = serde_json::json!(u64::MAX);
        assert!(validate_inputs(&[r]).is_err());
        for (weapon, mode) in [("zenith", "base"), ("torid", "cycle")] {
            let mut r = row(1);
            r.ruler = "demolisher".into();
            r.request["weapon"] = serde_json::json!(weapon);
            r.request["mode"] = serde_json::json!(mode);
            r.request["enemy"] = serde_json::json!("demolisher_devourer");
            assert!(validate_inputs(&[r.clone()]).is_ok());
            r.ruler = "standard_multi_target".into();
            assert!(validate_inputs(&[r]).is_ok());
        }
        let mut future = row(1);
        future.request["weapon"] = serde_json::json!("future_weapon");
        future.ruler = "future_ruler__future_variant".into();
        assert!(validate_inputs(&[future.clone()]).is_ok());
        future.request["duration"] = serde_json::json!(1e99);
        assert!(validate_inputs(&[future]).is_err());
    }
    #[test]
    fn probes_keep_original_run_indices_and_never_return_scores() {
        assert_eq!(sample_indices(1), vec![0]);
        assert_eq!(sample_indices(3), vec![0, 1, 2]);
        let indices = sample_indices(1000);
        assert_eq!(indices.len(), 8);
        assert_eq!(indices[0], 0);
        assert_eq!(indices[7], 999);
        assert!(indices.windows(2).all(|p| p[0] < p[1]));
        let value = serde_json::json!({"rows":[{"id":1,"score":null,"compute_ms":100,"probe":{"samples":8,"cpu_ms":1.0,"gpu_ms":50.0,"max_pages":42}}]});
        assert!(validate_outputs(&value, &[1]).is_ok());
        let mut forged = value;
        forged["rows"][0]["score"] = serde_json::json!({"score":1});
        assert!(validate_outputs(&forged, &[1]).is_err());
    }
    #[test]
    fn corrupt_scores_fail_closed() {
        assert!(
            validate_score(&serde_json::json!({"ok":false,"score":1,"metric":"kpm","work":1}))
                .is_err()
        );
        assert!(validate_score(&serde_json::json!({"score":-1,"metric":"kpm","work":1})).is_err());
        assert!(validate_score(
            &serde_json::json!({"score":1,"metric":"kpm","work":9007199254740992u64})
        )
        .is_err());
    }
}
